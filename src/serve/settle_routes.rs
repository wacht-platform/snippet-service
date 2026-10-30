//! Settling sessions: the routes, the memory-saving settle itself, and the sweep that
//! settles cold sessions and deletes long-settled ones. Thresholds live in `crate::settle`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::settle::*;

static FAILED: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
static SETTLING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// A session whose settle failed is not retried for this long.
const RETRY_AFTER_SECS: i64 = 3600;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) enum SettleOutcome {
    Settled(serde_json::Value),
    Already(serde_json::Value),
    Refused(String),
    Failed(String),
    Gone,
}

struct Gathered {
    row: crate::conversations::SessionRow,
    facts: SettleFacts,
    state_path: PathBuf,
}

async fn gather(d: &Shared, id: &str) -> Option<Gathered> {
    let store = crate::session::store_for_sessions()?;
    let row = store.get_session_row(id).ok()??;
    let state_path = state_path_for_id(&row.id)?;
    let state = read_session_meta(&state_path)?;
    use crate::harness::{GoalStatus, HarnessStatus};
    let alive = d.sessions.lock().await.get(&row.id).is_some_and(|s| !s.join.is_finished());
    let busy_status = matches!(state.status, HarnessStatus::Running | HarnessStatus::WaitingForInput);
    let open_task = store
        .list_tasks(Some(&row.id), None)
        .unwrap_or_default()
        .iter()
        .any(|t| !t.status.is_terminal());
    let facts = SettleFacts {
        id: row.id.clone(),
        kind: row.kind.clone(),
        role: row.role.clone(),
        status: row.status.clone(),
        settled: row.settled_at.is_some(),
        live_busy: (alive && busy_status) || state.compacting || state.pending_question.is_some(),
        has_goal: state
            .goal
            .as_ref()
            .is_some_and(|g| matches!(g.status, GoalStatus::Active | GoalStatus::Paused)),
        has_open_task: open_task,
        has_lanes_or_watches: !state.watches.is_empty()
            || state.lanes.iter().any(|l| l.status == crate::lanes::LaneStatus::Running),
        has_queued_input: !state.queued_inputs.is_empty(),
    };
    Some(Gathered { row, facts, state_path })
}

fn session_json(id: &str) -> serde_json::Value {
    crate::session::all_device_sessions()
        .into_iter()
        .find(|s| s.id == id)
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Save the session's memory (awaited, bounded by `REFLECT_TIMEOUT_SECS`), then mark it settled.
/// A failed or timed-out reflection leaves the session awake.
pub(crate) async fn settle_session(d: &Shared, id: &str) -> SettleOutcome {
    let Some(g) = gather(d, id).await else {
        return SettleOutcome::Gone;
    };
    if g.facts.settled {
        return SettleOutcome::Already(session_json(&g.row.id));
    }
    if let Some(reason) = refusal(&g.facts) {
        return SettleOutcome::Refused(reason);
    }
    let sid = g.row.id.clone();
    {
        let mut busy = SETTLING.lock().unwrap();
        if busy.contains(&sid) {
            return SettleOutcome::Refused("not settled: a settle is already in progress".into());
        }
        busy.push(sid.clone());
    }
    let outcome = settle_locked(d, g).await;
    SETTLING.lock().unwrap().retain(|s| s != &sid);
    outcome
}

async fn settle_locked(d: &Shared, g: Gathered) -> SettleOutcome {
    let sid = g.row.id.clone();
    let last_active = g.row.last_active;
    let profile = d.resume_profile(&sid, &g.state_path);
    let folder = PathBuf::from(&g.row.workspace);
    if !folder.is_dir() {
        return SettleOutcome::Failed(format!("workspace {} is missing", g.row.workspace));
    }
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder);
        apply_profile(&mut w, &profile);
        w
    };
    // An idle resident loop would write to the same session while it is reflected.
    if let Some(old) = d.sessions.lock().await.remove(&sid) {
        old.join.abort();
    }
    let reflected = tokio::time::timeout(
        Duration::from_secs(REFLECT_TIMEOUT_SECS),
        crate::session::reflect_stored_session(&cfg, &g.state_path),
    )
    .await;
    match reflected {
        Err(_) => return SettleOutcome::Failed(format!("memory save timed out after {REFLECT_TIMEOUT_SECS}s")),
        Ok(Err(e)) => return SettleOutcome::Failed(format!("memory save failed: {e}")),
        Ok(Ok(_)) => {}
    }
    // Addressed while reflecting: it is awake again, so leave it.
    let Some(fresh) = gather(d, &sid).await else {
        return SettleOutcome::Gone;
    };
    if fresh.row.last_active != last_active {
        return SettleOutcome::Refused("not settled: it received a message while its memory was saved".into());
    }
    if let Some(reason) = refusal(&fresh.facts) {
        return SettleOutcome::Refused(reason);
    }
    let Some(store) = crate::session::store_for_sessions() else {
        return SettleOutcome::Failed("session store unavailable".into());
    };
    if let Err(e) = store.set_session_settled(&sid, Some(now())) {
        return SettleOutcome::Failed(format!("record settled: {e}"));
    }
    SettleOutcome::Settled(session_json(&sid))
}

/// Wake a session. Last-active moves to now so the next sweep does not settle it again.
pub(crate) fn unsettle_session(id: &str) -> Option<serde_json::Value> {
    let store = crate::session::store_for_sessions()?;
    let row = store.get_session_row(id).ok()??;
    if row.settled_at.is_some() {
        store.set_session_last_active(&row.id, now()).ok()?;
    }
    Some(session_json(&row.id))
}

#[derive(Deserialize)]
pub(crate) struct SettleReq {
    session: String,
}

fn outcome_response(outcome: SettleOutcome) -> Response {
    match outcome {
        SettleOutcome::Settled(s) => Json(serde_json::json!({"ok": true, "session": s})).into_response(),
        SettleOutcome::Already(s) => {
            Json(serde_json::json!({"ok": true, "already": true, "session": s})).into_response()
        }
        SettleOutcome::Refused(reason) => {
            (StatusCode::CONFLICT, Json(serde_json::json!({"ok": false, "error": reason}))).into_response()
        }
        SettleOutcome::Failed(reason) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": reason})),
        )
            .into_response(),
        SettleOutcome::Gone => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

// POST /session/settle {session}
pub(crate) async fn settle_route(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SettleReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    outcome_response(settle_session(&d, &req.session).await)
}

// POST /session/unsettle {session}
pub(crate) async fn unsettle_route(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SettleReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match unsettle_session(&req.session) {
        Some(s) => Json(serde_json::json!({"ok": true, "session": s})).into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

pub(crate) async fn settle_loop(d: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(SWEEP_EVERY_SECS)).await;
        sweep_once(&d).await;
    }
}

/// One sweep: delete sessions settled over `DELETE_AFTER_SECS` ago (keeping their memory), then
/// settle at most `MAX_SETTLES_PER_TICK` cold sessions, oldest first.
pub(crate) async fn sweep_once(d: &Shared) -> (usize, usize) {
    let Some(store) = crate::session::store_for_sessions() else {
        return (0, 0);
    };
    let now = now();
    let mut deleted = 0;
    for id in expired_settled(&store, now).into_iter().take(MAX_DELETES_PER_TICK) {
        if purge_session(d, &id, true).await {
            deleted += 1;
        }
    }

    let mut candidates: Vec<_> = crate::session::list_routable_sessions()
        .into_iter()
        .filter(|s| !s.settled && s.idle_secs >= COLD_AFTER_SECS && s.status != "running")
        .collect();
    candidates.sort_by(|a, b| b.idle_secs.cmp(&a.idle_secs));
    let mut spent = 0;
    for s in candidates {
        if spent >= MAX_SETTLES_PER_TICK {
            break;
        }
        let recently_failed = FAILED
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .get(&s.id)
            .is_some_and(|at| now - at < RETRY_AFTER_SECS);
        if recently_failed {
            continue;
        }
        match settle_session(d, &s.id).await {
            SettleOutcome::Settled(_) => spent += 1,
            SettleOutcome::Failed(e) => {
                eprintln!("settle {}: {e}", s.id);
                FAILED.lock().unwrap().get_or_insert_with(HashMap::new).insert(s.id.clone(), now);
                spent += 1;
            }
            _ => {}
        }
    }
    (spent, deleted)
}
