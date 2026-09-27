use super::*;

pub(crate) fn session_event_page(
    events: &[crate::harness::HarnessEvent],
    before: Option<usize>,
    limit: Option<usize>,
) -> (usize, usize, bool) {
    let end = before.unwrap_or(events.len()).min(events.len());
    let start = match limit {
        Some(size) => end.saturating_sub(size.clamp(1, 500)),
        None => turn_page_start(&events[..end]),
    };
    (start, end, start > 0)
}

fn turn_page_start(events: &[crate::harness::HarnessEvent]) -> usize {
    const MAX_EVENTS: usize = 600;
    const MIN_EVENTS: usize = 40;
    const TURNS: usize = 2;
    let len = events.len();
    let floor = len.saturating_sub(MAX_EVENTS);
    let mut seen = 0;
    let mut start = floor;
    for i in (floor..len).rev() {
        if matches!(events[i], crate::harness::HarnessEvent::UserInput { .. }) {
            seen += 1;
            if seen == TURNS {
                start = i;
                break;
            }
        }
    }
    start.min(len.saturating_sub(MIN_EVENTS))
}

const WIRE_STRING_LIMIT: usize = 500;
const WIRE_ARRAY_LIMIT: usize = 30;

pub(crate) fn clip_json(value: &mut serde_json::Value, string_limit: usize, array_limit: usize) -> bool {
    match value {
        serde_json::Value::String(text) if text.len() > string_limit => {
            let mut cut = string_limit;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            true
        }
        serde_json::Value::Array(items) => {
            let mut clipped = items.len() > array_limit;
            items.truncate(array_limit);
            for item in items.iter_mut() {
                clipped |= clip_json(item, string_limit, array_limit);
            }
            clipped
        }
        serde_json::Value::Object(map) => {
            let mut clipped = false;
            for item in map.values_mut() {
                clipped |= clip_json(item, string_limit, array_limit);
            }
            clipped
        }
        _ => false,
    }
}

pub(crate) fn wire_events(events: &[crate::harness::HarnessEvent], compact: bool) -> serde_json::Value {
    if !compact {
        return serde_json::to_value(events).unwrap_or_default();
    }
    serde_json::Value::Array(
        events
            .iter()
            .map(|event| {
                let mut v = serde_json::to_value(event).unwrap_or_default();
                if let crate::harness::HarnessEvent::ToolResult { .. } = event {
                    if let Some(o) = v.as_object_mut() {
                        if let Some(result) = o.get_mut("result") {
                            if clip_json(result, WIRE_STRING_LIMIT, WIRE_ARRAY_LIMIT) {
                                o.insert("result_clipped".into(), serde_json::json!(true));
                            }
                        }
                    }
                }
                v
            })
            .collect(),
    )
}

const TASK_SUMMARY_TEXT_LIMIT: usize = 280;

pub(crate) fn task_summary(mut task: serde_json::Value) -> serde_json::Value {
    let Some(o) = task.as_object_mut() else {
        return task;
    };
    let mut clipped = false;
    for key in ["description", "plan", "result", "handoff"] {
        if let Some(field) = o.get_mut(key) {
            clipped |= clip_json(field, TASK_SUMMARY_TEXT_LIMIT, 8);
        }
    }
    if let Some(serde_json::Value::Array(markers)) = o.get_mut("notifications") {
        let total = markers.len();
        markers.retain(|m| m.get("delivered").and_then(|d| d.as_bool()) != Some(true));
        for marker in markers.iter_mut() {
            clip_json(marker, TASK_SUMMARY_TEXT_LIMIT, 8);
        }
        clipped |= markers.len() != total;
        o.insert("notification_count".into(), serde_json::json!(total));
    }
    o.insert("summary".into(), serde_json::json!(clipped));
    task
}

#[derive(Deserialize)]
pub(crate) struct RewindReq {
    session: String,
    checkpoint: String,
}

// POST /session/rewind {session, checkpoint} — restore workspace files AND
// truncate conversation history to that checkpoint. Always updates the state
// file so clients (mobile/TUI) see the truncated transcript immediately; also
// notifies a live loop when present so in-memory state matches.
pub(crate) async fn rewind_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RewindReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let (sp, workspace) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let checkpoint_id = req.checkpoint.clone();

    // Load current state (store or file — authoritative for history indices).
    let Some(mut state) = read_session_state(&sp) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read session state",
        )
            .into_response();
    };

    let label = match state.apply_checkpoint_rewind(&checkpoint_id) {
        Ok(label) => label,
        Err(_) => return (StatusCode::NOT_FOUND, "checkpoint not found").into_response(),
    };

    // Persist truncated history first so any client re-read sees the cut.
    // `write_session_state` targets whichever store holds the session, so a
    // rewind does not silently write a file a DB session will never read.
    if let Err(error) = crate::session::write_session_state(&sp, &state) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write state: {error}"),
        )
            .into_response();
    }
    // Keep a live loop in sync (it may overwrite disk on its next persist otherwise).
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session) {
            if !s.join.is_finished() {
                let _ = s.input_tx.send(LoopInput::Rewind {
                    checkpoint: checkpoint_id.clone(),
                });
            }
        }
    }

    // Restore workspace files to the shadow commit.
    let checkpoint_for_restore = checkpoint_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::checkpoint::restore(&workspace, &checkpoint_for_restore)
    })
    .await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({
            "restored": checkpoint_id,
            "label": label,
            "event_end": state.events.len(),
            "message_end": state.messages.len(),
        }))
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ExecReq {
    session: String,
    command: String,
}

// POST /session/exec {session, command} — run a shell command in the session's
// workspace and return its output. Token-gated; runs as the daemon user.
pub(crate) async fn exec_in_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ExecReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let (_sp, dir) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if req.command.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty command").into_response();
    }
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&req.command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        // On timeout the output future is dropped — kill the child then, or it
        // keeps running detached forever with no handle to find or stop it.
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => {
            return Json(serde_json::json!({
                "exit_code": -1, "stdout": "", "stderr": "timed out after 60s", "truncated": false,
            }))
            .into_response();
        }
    };
    let (stdout, t1) = clip_output(&out.stdout, 20_000);
    let (stderr, t2) = clip_output(&out.stderr, 20_000);
    Json(serde_json::json!({
        "exit_code": out.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": t1 || t2,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct BgReq {
    session: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

// POST /bg {session} — snapshot of the session's background processes.
pub(crate) async fn bg_list(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    Json(serde_json::json!({ "processes": crate::bg::list(&dir) })).into_response()
}

// POST /bg/kill {session, id} — terminate one background process.
pub(crate) async fn bg_kill(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    match crate::bg::kill_by_id(&dir, id) {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})).into_response(),
    }
}

// POST /bg/log {session, id, tail?} — tail a background process's log.
pub(crate) async fn bg_log(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    let text = std::fs::read_to_string(crate::bg::log_path(&dir, id)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(req.tail.unwrap_or(400));
    Json(serde_json::json!({ "log": lines[start..].join("\n"), "truncated": start > 0 }))
        .into_response()
}

/// Change-detector for a session, from whichever store holds it: the store's
/// row timestamp plus its log lengths, else the state file's bytes.
///
/// The attach stream re-checks on a timer, so this must stay cheap and must NOT
/// load the transcript — not loading it is the whole point of having moved it
/// into the database.
pub(crate) fn session_state_fingerprint(store: &crate::store::Store, state_path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let id = session_id_for_state_path(state_path);
    if let Ok(Some(fingerprint)) = store.conversation_fingerprint(&id) {
        fingerprint.hash(&mut hasher);
        return Some(hasher.finish());
    }
    let bytes = std::fs::read(state_path).ok()?;
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

pub(crate) fn resolve_session_dir(session: &str) -> Result<PathBuf, Response> {
    if let Some(sp) = state_path_for_id(session) {
        if let Some(state) = read_session_state(&sp) {
            let dir = PathBuf::from(&state.workspace);
            if !state.workspace.is_empty() && dir.is_dir() {
                return Ok(dir);
            }
        }
    }
    // Not a session id → treat it as a folder path (no-session git).
    let dir = PathBuf::from(session);
    if dir.is_dir() {
        return Ok(dir);
    }
    Err((StatusCode::NOT_FOUND, "no such session or directory").into_response())
}

/// Lossy-decode bytes and clip to `max` chars, returning (text, was_truncated).
pub(crate) fn clip_output(b: &[u8], max: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > max {
        (s.chars().take(max).collect::<String>() + "\u{2026}", true)
    } else {
        (s.into_owned(), false)
    }
}

#[derive(Deserialize)]
pub(crate) struct DeleteReq {
    session: String,
}

// POST /session/delete {session} — stop the live loop (if any) and delete the
// session's conversation file.
pub(crate) async fn delete_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<DeleteReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    {
        let mut sessions = d.sessions.lock().await;
        if let Some(s) = sessions.remove(&req.session) {
            s.join.abort();
        }
    }
    crate::session::remove_session_files(&sp);
    Json(serde_json::json!({"deleted": true})).into_response()
}

#[derive(Deserialize)]
pub(crate) struct RenameReq {
    session: String,
    title: String,
}

// POST /session/rename {session, title} — set the session's title override. A live
// session goes through its loop so the in-memory state stays in sync; otherwise the
// state file is edited directly (without reviving the loop).
pub(crate) async fn rename_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RenameReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session) {
            if !s.join.is_finished() {
                let _ = s.input_tx.send(LoopInput::SetTitle(req.title.clone()));
                return Json(serde_json::json!({"renamed": true})).into_response();
            }
        }
    }
    match crate::session::set_session_title(&sp, &req.title) {
        Ok(()) => Json(serde_json::json!({"renamed": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ForkReq {
    session: String,
    /// Checkpoint id (or unique prefix) — same cut as `/session/rewind`.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Inclusive event index to keep through (snapped to a provider-safe boundary).
    #[serde(default)]
    event_index: Option<usize>,
}

// POST /session/fork {session, checkpoint?|event_index?} — branch a NEW conversation
// at the chosen history point. Source session is left untouched. Workspace files are
// shared (not snapshotted); only conversation history is forked.
pub(crate) async fn fork_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ForkReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };

    // Prefer the live session's own path when it is running, so the fork sees a
    // turn that has not flushed yet. The reader resolves whichever store holds
    // the session, so a database-backed session forks exactly like a file one.
    // This used to be a nested if/else cascade that repeated the same read four
    // times, which is how the file-only assumption got baked in four times over.
    let state = {
        let sessions = d.sessions.lock().await;
        let path = sessions
            .get(&req.session)
            .filter(|live| !live.join.is_finished())
            .map(|live| live.state_path.clone())
            .unwrap_or_else(|| sp.clone());
        drop(sessions);
        match read_session_state(&path) {
            Some(state) => state,
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session state unreadable",
                )
                    .into_response();
            }
        }
    };

    let point = match crate::session::resolve_fork_point(
        &state,
        req.checkpoint.as_deref(),
        req.event_index,
    ) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    match crate::session::write_forked_conversation(&sp, &state, point) {
        Ok(forked) => Json(serde_json::json!({
            "id": forked.id,
            "title": forked.title,
            "event_end": forked.event_end,
            "message_end": forked.message_end,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AttachQuery {
    pub(crate) token: Option<String>,
    pub(crate) session: String,
    #[serde(default)]
    pub(crate) compact: Option<String>,
}

