use std::time::Duration;

use axum::extract::{Json, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use crate::agent_wakes::Setter;
use crate::harness::LoopInput;
use crate::keep_warm::{self, Decision, KeepWarmSpec, Outcome};
use super::{mission_error, unauthorized, Auth, Shared};

const TICK: Duration = Duration::from_secs(60);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/keep-warm", get(list_entries))
        .route("/keep-warm/{*owner}", get(get_entry).put(put_entry).delete(delete_entry))
        .route("/agents/{agent_id}/keep-warm", get(get_entry).put(put_entry).delete(delete_entry))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub async fn tick_loop(daemon: Shared) {
    loop {
        tokio::time::sleep(TICK).await;
        let root = daemon.recurring_root.clone();
        let Ok(entries) = keep_warm::list(&root) else { continue };
        let ledger = crate::store::Store::open_cached(crate::store::default_db_path()).ok();
        for mut entry in entries {
            let now = now();
            let last_turn = ledger.as_ref().and_then(|s| s.last_turn_at(&entry.session_id).ok().flatten());
            let quiet = keep_warm::quiet_now(&root, &entry.owner, now);
            let busy = super::recurring::session_busy_for_recurring(&entry.session_id);
            let decision = keep_warm::decide(&entry, now, last_turn, quiet, busy);
            if decision != Decision::Warm {
                if keep_warm::note_decision(&mut entry, &decision) {
                    let _ = keep_warm::put(&root, &entry);
                }
                continue;
            }
            let outcome = ping(&daemon, &entry.session_id).await;
            match &outcome {
                Outcome::Warmed(usage) => eprintln!(
                    "[keep-warm] {} warmed: cache_read={} cache_write={}",
                    entry.owner, usage.cache_read_tokens, usage.cache_creation_tokens
                ),
                Outcome::Unsupported(reason) | Outcome::Skipped(reason) | Outcome::Failed(reason) => {
                    eprintln!("[keep-warm] {} not warmed: {reason}", entry.owner)
                }
            }
            keep_warm::record_outcome(&mut entry, &outcome, now);
            let _ = keep_warm::put(&root, &entry);
        }
    }
}

/// Ask the live session loop to re-read its cached prefix; only a session
/// already in memory has a prefix worth warming.
async fn ping(daemon: &Shared, session_id: &str) -> Outcome {
    let tx = {
        let sessions = daemon.sessions.lock().await;
        match sessions.get(session_id) {
            Some(s) if !s.join.is_finished() => s.input_tx.clone(),
            _ => return Outcome::Skipped("session is not open; nothing in memory to warm".into()),
        }
    };
    let id = format!("kw-{}", uuid::Uuid::new_v4().simple());
    let rx = keep_warm::register(&id);
    if tx.send(LoopInput::KeepWarm(id.clone())).is_err() {
        keep_warm::complete(&id, Outcome::Skipped(String::new()));
        return Outcome::Skipped("session closed".into());
    }
    match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
        Ok(Ok(outcome)) => outcome,
        _ => {
            keep_warm::complete(&id, Outcome::Skipped(String::new()));
            Outcome::Failed("no answer from the session".into())
        }
    }
}

fn not_found(owner: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"owner": owner, "error": "no keep-warm"}))).into_response()
}

async fn list_entries(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let spend = crate::store::Store::open_cached(crate::store::default_db_path())
        .ok()
        .and_then(|s| s.keep_warm_totals(now() - 7 * 86_400).ok())
        .unwrap_or_default();
    match keep_warm::list(&d.recurring_root) {
        Ok(entries) => Json(json!({
            "entries": entries.iter().map(keep_warm::view).collect::<Vec<_>>(),
            "limits": keep_warm::limits(),
            "spend_7d": spend,
        }))
        .into_response(),
        Err(error) => mission_error(error),
    }
}

async fn get_entry(State(d): State<Shared>, AxumPath(owner): AxumPath<String>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match keep_warm::get(&d.recurring_root, &owner) {
        Ok(Some(entry)) => Json(keep_warm::view(&entry)).into_response(),
        Ok(None) => not_found(&owner),
        Err(error) => mission_error(error),
    }
}

async fn put_entry(
    State(d): State<Shared>,
    AxumPath(owner): AxumPath<String>,
    Query(a): Query<Auth>,
    Json(spec): Json<KeepWarmSpec>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let kind = match crate::agent_wakes::resolve_owner(&d.store, &owner) {
        Ok(kind) => kind,
        Err(error) => return (StatusCode::NOT_FOUND, error).into_response(),
    };
    match keep_warm::set(&d.recurring_root, &owner, kind, spec, Setter::User, now()) {
        Ok(entry) => Json(keep_warm::view(&entry)).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

async fn delete_entry(State(d): State<Shared>, AxumPath(owner): AxumPath<String>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match keep_warm::clear(&d.recurring_root, &owner, Setter::User) {
        Ok(true) => Json(json!({"owner": owner, "deleted": true})).into_response(),
        Ok(false) => not_found(&owner),
        Err(error) => mission_error(error),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn routes_do_not_clash_with_wake_routes() {
        let _ = super::router().merge(super::super::agent_wakes::router());
    }
}
