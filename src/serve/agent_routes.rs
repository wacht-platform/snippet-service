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
            let assignments = match d.store.list_agent_assigned_sessions() {
                Ok(rows) => rows,
                Err(error) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {error}"))
                        .into_response();
                }
            };
            let mut grouped: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for (agent_id, id, title, conversation, last_active) in assignments {
                grouped.entry(agent_id).or_default().push(serde_json::json!({
                    "id": id,
                    "title": title,
                    "conversation": conversation,
                    "last_active": last_active,
                }));
            }
            let agents = agents
                .into_iter()
                .map(|agent| {
                    let mut value = serde_json::to_value(&agent).unwrap_or_default();
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "assigned_sessions".into(),
                            serde_json::Value::Array(grouped.remove(&agent.id).unwrap_or_default()),
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
    if let Err(error) = home.ensure_layout(&default_identity) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create agent home: {error}"),
        )
            .into_response();
    }
    match d.store.create_agent(&agent) {
        Ok(()) => (StatusCode::CREATED, Json(agent)).into_response(),
        Err(error) => (StatusCode::CONFLICT, format!("create agent: {error}")).into_response(),
    }
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
pub(crate) struct NotificationReplayQuery {
    pub(crate) token: Option<String>,
    #[serde(default)]
    pub(crate) since: u64,
}

pub(crate) async fn notification_replay(
    State(d): State<Shared>,
    Query(q): Query<NotificationReplayQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    Json(serde_json::json!({
        "events": replay_notification_events(q.since),
    }))
    .into_response()
}

