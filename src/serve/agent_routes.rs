use super::*;

pub(crate) fn default_agent_page_limit() -> u32 {
    200
}

#[derive(Deserialize)]
pub(crate) struct AgentsQuery {
    pub(crate) token: Option<String>,
    #[serde(default)]
    pub(crate) after_name: Option<String>,
    #[serde(default)]
    pub(crate) after_id: Option<String>,
    #[serde(default = "default_agent_page_limit")]
    pub(crate) limit: u32,
}


pub(crate) async fn list_agents(State(d): State<Shared>, Query(q): Query<AgentsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let after = match (q.after_name.as_deref(), q.after_id.as_deref()) {
        (Some(name), Some(id)) => Some((name, id)),
        // A lone cursor half is a caller error, not a silent full listing.
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "after_name and after_id must be provided together",
            )
                .into_response();
        }
        (None, None) => None,
    };
    match d.store.list_agents_page(after, q.limit.clamp(1, 500)) {
        Ok(agents) => {
            let since = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339();
            let counts = match d.store.agent_task_counts(&since) {
                Ok(rows) => rows,
                Err(error) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {error}"))
                        .into_response();
                }
            };
            let colors = d.store.agent_colors().unwrap_or_default();
            let shapes = d.store.agent_shapes().unwrap_or_default();
            let nicknames = d.store.agent_nicknames().unwrap_or_default();
            let profiles = d.store.agent_profiles().unwrap_or_default();
            let context_limits = d.store.agent_context_limits().unwrap_or_default();
            let mut grouped: HashMap<String, serde_json::Map<String, serde_json::Value>> =
                HashMap::new();
            for (agent_id, status, count) in counts {
                grouped.entry(agent_id).or_default().insert(status, count.into());
            }
            let agents = agents
                .into_iter()
                .map(|agent| {
                    let mut value = serde_json::to_value(&agent).unwrap_or_default();
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "task_counts".into(),
                            serde_json::Value::Object(grouped.remove(&agent.id).unwrap_or_default()),
                        );
                        object.insert("task_counts_since".into(), since.clone().into());
                        if let Some(color) = colors.get(&agent.id) {
                            object.insert("color".into(), color.clone().into());
                        }
                        if let Some(shape) = shapes.get(&agent.id) {
                            object.insert("shape".into(), shape.clone().into());
                        }
                        if let Some(nickname) = nicknames.get(&agent.id) {
                            object.insert("nickname".into(), nickname.clone().into());
                        }
                        if let Some(profile) = profiles.get(&agent.id) {
                            object.insert("profile".into(), profile.clone().into());
                        }
                        object.insert(
                            "max_context_tokens".into(),
                            context_limits.get(&agent.id).copied().into(),
                        );
                    }
                    value
                })
                .collect::<Vec<_>>();
            Json(agents).into_response()
        }
        Err(error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {error}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentBoardQuery {
    token: Option<String>,
    /// Only rows about this folder.
    #[serde(default)]
    workspace: Option<String>,
    /// Substring to find in the recorded summaries.
    #[serde(default)]
    contains: Option<String>,
    /// `dispatched`, `reported`, or `noted`.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default = "default_agent_page_limit")]
    limit: u32,
}

/// GET /agents/{agent_id}/board — one agent's coordination memory.
///
/// Read-only: the board is written by the agent as it works, and by the
/// dispatcher/report paths. This is what makes an agent's history inspectable
/// rather than invisible — what it sent, what came back, what it concluded.
pub(crate) async fn agent_board(
    State(d): State<Shared>,
    Query(q): Query<AgentBoardQuery>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let kind = match q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        None => None,
        Some(raw) => match crate::coordination::BoardEntryKind::parse(raw) {
            Some(kind) => Some(kind),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown kind `{raw}` — use dispatched, reported, or noted"),
                )
                    .into_response();
            }
        },
    };
    let query = crate::coordination::BoardQuery {
        workspace: q.workspace.as_deref().filter(|w| !w.trim().is_empty()),
        contains: q.contains.as_deref().filter(|c| !c.trim().is_empty()),
        kind,
    };
    match d
        .store
        .read_board(&agent_id, &query, q.limit.clamp(1, 200))
    {
        Ok(entries) => Json(serde_json::json!({
            "agent_id": agent_id,
            "entries": entries,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read board: {error}"),
        )
            .into_response(),
    }
}

/// Record one event in a session's transcript WITHOUT starting a turn.
///
/// The one place a notice is written, so a live loop and a dormant session
/// produce identical transcripts. Prefers the LIVE loop when the session is
/// running, so the entry lands in the loop's own state and cannot be clobbered
/// by a later full rewrite. A dormant session has no loop to hold it, so it goes
/// straight to the store — which is where a live loop reads from anyway.
///
/// `notice_text` decides whether the event also enters the model's context.
pub(crate) async fn record_notice(d: &Daemon, session_id: &str, event: HarnessEvent) {
    let canonical = crate::session::state_path_for_id(session_id)
        .map(|path| crate::session::session_id_for_state_path(&path));
    let session_id = canonical.as_deref().unwrap_or(session_id);
    // A FINISHED loop is a dead entry: `send` into its closed channel reports
    // success and the notice is gone. `deliver` guards on this; without the same
    // guard here, a message sent to a session whose loop has stopped — after an
    // interrupt, say — never reaches its transcript even though the daemon
    // accepted it. Filtering finished loops falls through to the store write
    // below, which is where a resumed loop reads from anyway.
    let live = {
        let sessions = d.sessions.lock().await;
        sessions
            .get(session_id)
            .filter(|s| !s.join.is_finished())
            .map(|s| s.input_tx.clone())
    };
    if let Some(tx) = live {
        // A notice, not a `UserMessage`: the loop records it and stays parked.
        let _ = tx.send(LoopInput::Notice(event));
        return;
    }
    // Dormant: no loop to deliver into. Write straight to the store, but only
    // when the session actually has a row — `session_events` has a foreign key to
    // `sessions`, so an insert for an unknown session would just fail.
    if !d.store.has_conversation(session_id).unwrap_or(false) {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let _ = d
        .store
        .append_conversation_events(session_id, std::slice::from_ref(&event), &now);
    if let Some(text) = crate::harness::notice_text(&event) {
        let _ = d.store.append_conversation_messages(
            session_id,
            &[crate::llm::HarnessMessage::User { content: text }],
            &now,
        );
    }
}

/// Record a direct message in a session's transcript WITHOUT starting a turn.
///
/// Both directions use this: what a session sent out, and what came back. The
/// event is for the UIs; the paired message is so the model sees the exchange
/// next turn.
///
/// Private to `serve`; its child module `direct` reaches it via `super::`.
pub(crate) async fn record_agent_message(
    d: &Shared,
    session_id: &str,
    agent_id: &str,
    body: &str,
    outbound: bool,
) {
    record_notice(
        d,
        session_id,
        HarnessEvent::AgentMessage {
            agent_id: agent_id.to_string(),
            body: body.to_string(),
            outbound,
        },
    )
    .await;
}

/// Note on Mission Control's transcript that work was dispatched on its behalf.
///
/// The user can file a task directly, which bypasses Mission Control entirely —
/// so without this the coordinator's record would show the task appearing from
/// nowhere. It is a NOTICE, never a wake: the work is already routed to a worker
/// and will report back on its own, so waking Mission Control would spend a turn
/// on something that needs no decision.
pub(crate) async fn record_dispatch_notice(d: &Daemon, task: &crate::coordination::Task) {
    // Mission Control dispatching through its own tool already has the tool call
    // and result in its transcript. Recording a notice too would tell it twice,
    // so this is for dispatches made AROUND it — the case where its record would
    // otherwise show work appearing from nowhere.
    if task.created_by_id == crate::mission_control::SESSION_ID {
        return;
    }
    let target = crate::session::state_path_for_id(&task.session_id)
        .map(|path| crate::session::session_id_for_state_path(&path))
        .unwrap_or_else(|| task.session_id.clone());
    let by = if task.created_by_kind == "human" {
        "You".to_string()
    } else {
        format!("{}:{}", task.created_by_kind, task.created_by_id)
    };
    record_notice(
        d,
        crate::mission_control::SESSION_ID,
        HarnessEvent::TaskDispatched {
            task_id: task.id.clone(),
            title: task.title.clone(),
            session_id: target,
            by,
        },
    )
    .await;
}

#[derive(Deserialize)]
pub(crate) struct AgentReq {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) handle: String,
    #[serde(default = "default_agent_kind")]
    pub(crate) kind: crate::coordination::types::AgentKind,
    #[serde(default = "default_agent_status")]
    pub(crate) status: crate::coordination::types::AgentStatus,
    #[serde(default = "default_agent_role")]
    pub(crate) role: crate::coordination::types::AgentRole,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) profile: Option<String>,
    #[serde(default)]
    pub(crate) nickname: Option<String>,
}
fn default_agent_kind() -> crate::coordination::types::AgentKind {
    crate::coordination::types::AgentKind::Worker
}
fn default_agent_status() -> crate::coordination::types::AgentStatus {
    crate::coordination::types::AgentStatus::Active
}
fn default_agent_role() -> crate::coordination::types::AgentRole {
    crate::coordination::types::AgentRole::Implementer
}

pub(crate) async fn create_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Json(req): Json<AgentReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    if req.id.trim().is_empty() || req.handle.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "id and handle are required").into_response();
    }
    let profile = req.profile.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string);
    if let Some(name) = profile.as_deref() {
        if let Err(error) = crate::mission_tools::check_profile(name).await {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    }
    let req_nickname = req.nickname.as_deref().map(|n| n.trim().to_string());
    let agent = crate::coordination::types::Agent {
        id: req.id,
        display_name: req.display_name,
        handle: req.handle,
        kind: req.kind,
        status: req.status,
        role: req.role,
        capabilities: req.capabilities,
    };
    let home = match crate::coordination::AgentHome::new(
        crate::coordination::agents_root(&d.mission_control_root),
        &agent.id,
    ) {
        Ok(home) => home,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let default_identity = format!(
        "# {}\n\nAgent handle: @{}\n\nThis identity is awaiting its first build and research pass.\n",
        agent.display_name, agent.handle
    );
    let fresh_home = !home.root().exists();
    if let Err(error) = home.ensure_layout(&default_identity) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create agent home: {error}"),
        )
            .into_response();
    }
    match d.store.create_agent(&agent) {
        Ok(()) => {
            if let Some(name) = profile.as_deref() {
                let _ = d.store.set_agent_profile(&agent.id, Some(name));
            }
            let nickname = req_nickname.filter(|n| !n.is_empty());
            if let Some(n) = nickname.as_deref() {
                let _ = d.store.set_agent_nickname(&agent.id, Some(n));
            }
            let mut body = serde_json::to_value(&agent).unwrap_or_default();
            if let (Some(object), Some(name)) = (body.as_object_mut(), profile) {
                object.insert("profile".into(), name.into());
            }
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(error) => {
            if fresh_home {
                let _ = std::fs::remove_dir_all(home.root());
            }
            let status = if is_unique_violation(&error) {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, format!("create agent: {error}")).into_response()
        }
    }
}

fn is_unique_violation(error: &crate::store::StoreError) -> bool {
    matches!(
        error,
        crate::store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                || e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

#[derive(Deserialize)]
pub(crate) struct AgentStatusReq {
    pub(crate) status: crate::coordination::types::AgentStatus,
}

/// POST /agents/{id}/status — pause, drain, disable or resume an agent. A
/// paused agent keeps its current lease but takes no new work or wake-ups.
pub(crate) async fn set_agent_status(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentStatusReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.set_agent_status(&agent_id, &req.status) {
        Ok(true) => match d.store.get_agent(&agent_id) {
            Ok(Some(agent)) => Json(agent).into_response(),
            _ => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        },
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent status: {error}"))
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentColorReq {
    pub(crate) color: Option<String>,
}

/// POST /agents/{id}/color — the agent's mascot colour as `#RRGGBB`, or null to
/// go back to the default.
pub(crate) async fn set_agent_color(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentColorReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let color = req.color.map(|c| c.trim().to_ascii_uppercase()).filter(|c| !c.is_empty());
    if let Some(c) = &color {
        let hex = c.strip_prefix('#').unwrap_or("");
        if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return (StatusCode::BAD_REQUEST, "color must be #RRGGBB").into_response();
        }
    }
    match d.store.set_agent_color(&agent_id, color.as_deref()) {
        Ok(true) => Json(serde_json::json!({ "id": agent_id, "color": color })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent color: {error}"))
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentShapeReq {
    pub(crate) shape: Option<String>,
}

pub(crate) const AGENT_SHAPES: [&str; 7] = ["mochi", "bunny", "cat", "bear", "ghost", "cloud", "onigiri"];

/// POST /agents/{id}/shape — the agent's mascot shape (one of `AGENT_SHAPES`), or null to
/// go back to the default.
pub(crate) async fn set_agent_shape(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentShapeReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let shape = req.shape.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
    if let Some(s) = &shape {
        if !AGENT_SHAPES.contains(&s.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                format!("shape must be one of: {}", AGENT_SHAPES.join(", ")),
            )
                .into_response();
        }
    }
    match d.store.set_agent_shape(&agent_id, shape.as_deref()) {
        Ok(true) => Json(serde_json::json!({ "id": agent_id, "shape": shape })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent shape: {error}"))
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentNicknameReq {
    pub(crate) nickname: Option<String>,
}

/// POST /agents/{id}/nickname — a short name shown instead of the display name, or null to clear it.
pub(crate) async fn set_agent_nickname(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentNicknameReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let nickname = req.nickname.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if nickname.as_deref().is_some_and(|n| n.chars().count() > 40) {
        return (StatusCode::BAD_REQUEST, "nickname is at most 40 characters").into_response();
    }
    match d.store.set_agent_nickname(&agent_id, nickname.as_deref()) {
        Ok(true) => Json(serde_json::json!({ "id": agent_id, "nickname": nickname })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent nickname: {error}"))
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentProfileReq {
    pub(crate) profile: Option<String>,
}

/// POST /agents/{id}/profile — the inference profile this agent works on in
/// every session it takes, or null to fall back to the session's own model.
pub(crate) async fn set_agent_profile(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentProfileReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let profile = req.profile.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if let Some(name) = profile.as_deref() {
        d.reload_config().await;
        if let Err(error) = crate::mission_tools::check_profile(name).await {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    }
    match d.store.set_agent_profile(&agent_id, profile.as_deref()) {
        Ok(true) => Json(serde_json::json!({ "id": agent_id, "profile": profile })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent profile: {error}"))
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct AgentContextReq {
    pub(crate) max_context_tokens: Option<u64>,
}

/// POST /agents/{id}/context — the agent's context ceiling before compaction,
/// clamped to the model's window at session start; null or 0 clears it.
pub(crate) async fn set_agent_context(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    Json(req): Json<AgentContextReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let tokens = match crate::agent_context::validate(req.max_context_tokens) {
        Ok(tokens) => tokens,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    match d.store.set_agent_context_limit(&agent_id, tokens) {
        Ok(true) => Json(serde_json::json!({ "id": agent_id, "max_context_tokens": tokens }))
            .into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("set agent context: {error}"))
            .into_response(),
    }
}

/// DELETE /agents/{id} — remove an agent and its home. Mission Control stays,
/// and an agent still on an unfinished task is refused with those tasks named.
pub(crate) async fn delete_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    use crate::coordination::AgentRemoval;
    match d.store.delete_agent(&agent_id) {
        Ok(AgentRemoval::Removed) => {
            if let Ok(home) = crate::coordination::AgentHome::new(
                crate::coordination::agents_root(&d.mission_control_root),
                &agent_id,
            ) {
                let _ = std::fs::remove_dir_all(home.root());
            }
            Json(serde_json::json!({ "deleted": true })).into_response()
        }
        Ok(AgentRemoval::NotFound) => (StatusCode::NOT_FOUND, "no such agent").into_response(),
        Ok(AgentRemoval::MissionControl) => {
            (StatusCode::BAD_REQUEST, "Mission Control can't be deleted").into_response()
        }
        Ok(AgentRemoval::OnOpenTasks(titles)) => {
            let shown: Vec<String> = titles.iter().take(3).cloned().collect();
            let more = titles.len().saturating_sub(shown.len());
            let mut msg = format!("Still on {}: {}", if titles.len() == 1 { "a task" } else { "tasks" }, shown.join(", "));
            if more > 0 {
                msg.push_str(&format!(" and {more} more"));
            }
            msg.push_str(". Finish or reassign them first.");
            (StatusCode::CONFLICT, msg).into_response()
        }
        Err(error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("delete agent: {error}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct NotificationReplayQuery {
    pub(crate) token: Option<String>,
    #[serde(default)]
    pub(crate) since: u64,
    pub(crate) since_created_at: Option<i64>,
    pub(crate) since_event_id: Option<u64>,
    #[serde(default = "notification_limit")]
    pub(crate) limit: usize,
}

fn notification_limit() -> usize { 100 }

pub(crate) async fn notification_replay(
    State(d): State<Shared>,
    Query(q): Query<NotificationReplayQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    if q.since > i64::MAX as u64 || q.limit == 0 || q.limit > 500 {
        return (StatusCode::BAD_REQUEST, "since must fit signed 64-bit; limit must be 1..500").into_response();
    }
    if q.since_event_id.is_some_and(|id| id > i64::MAX as u64)
        || (q.since_created_at.is_none() && q.since_event_id.is_some())
        || (q.since_created_at.is_some() && q.since != 0)
        || q.since_created_at.is_some_and(|ts| ts < 0) {
        return (StatusCode::BAD_REQUEST, "use since_created_at >= 0 with optional since_event_id (signed 64-bit); do not mix with since").into_response();
    }
    let result = if let Some(created_at) = q.since_created_at {
        crate::session::replay_notification_events_after_cursor(created_at, q.since_event_id.unwrap_or(0), q.limit)
    } else {
        replay_notification_events(q.since, q.limit)
    };
    match result {
        Ok(page) => Json(page).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

