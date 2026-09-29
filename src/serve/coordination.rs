use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;

use crate::coordination::{Task, TaskFilter, TaskLink, TaskLinkKind, TaskStatus};
use crate::coordination::types::CoordinationEvent;
use crate::mission_control;
use crate::serve::task_summary;
use std::sync::OnceLock;
use tokio::sync::mpsc;

use super::{Auth, LoopInput, Shared, unauthorized, direct};

pub const COORDINATION_THREAD: &str = "system";

static COORDINATION_WAKE_TX: OnceLock<mpsc::UnboundedSender<CoordinationEvent>> = OnceLock::new();

pub fn queue_coordination_wake(event: CoordinationEvent) {
    if let Some(tx) = COORDINATION_WAKE_TX.get() {
        let _ = tx.send(event);
    }
}

pub async fn coordination_wake_loop(d: Shared) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = COORDINATION_WAKE_TX.set(tx);
    while let Some(event) = rx.recv().await {
        wake_coordination_thread_participants(&d, &event).await;
    }
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/coordination/tasks", get(list_tasks).post(create_task))
        .route("/coordination/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/coordination/tasks/{task_id}/status", post(set_task_status))
        .route("/coordination/tasks/{task_id}/links", get(task_links).post(link_tasks))
        .route("/coordination/tasks/{task_id}/links/{other_id}", delete(unlink_tasks))
        .route("/coordination/tasks/{task_id}/agents", get(task_agents).post(add_task_agent))
        .route("/coordination/tasks/{task_id}/agents/{agent_id}", delete(remove_task_agent))
        .route("/coordination/tasks/{task_id}/agents/{agent_id}/lease", post(transfer_task_lease))
        .route("/coordination/threads/{thread_id}/events", get(thread_events))
        .route("/coordination/threads/{thread_id}/messages", post(post_message))
}

#[derive(Deserialize)]
pub struct CoordinationTasksQuery {
    pub token: Option<String>,
    #[serde(default)]
    pub after_created: Option<String>,
    #[serde(default)]
    pub after_id: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
}

fn default_limit() -> u32 {
    50
}

pub fn parse_task_status(value: &str) -> Result<TaskStatus, String> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|_| format!("unknown task status: {value}"))
}

#[derive(Deserialize)]
pub struct CoordinationTaskReq {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub session_id: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub priority: i64,
}

#[derive(Deserialize)]
pub struct CoordinationTaskPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(Deserialize)]
pub struct CoordinationTaskStatusReq {
    pub status: String,
}

#[derive(Deserialize)]
pub struct CoordinationTaskLinkReq {
    pub to_task_id: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
pub struct CoordinationTaskAgentReq {
    pub agent_id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub work_session_id: Option<String>,
    #[serde(default)]
    pub scope: String,
    #[serde(default = "default_agent_status")]
    pub status: String,
}

fn default_agent_status() -> String {
    "pending".into()
}

#[derive(Deserialize)]
pub struct CoordinationTransferLeaseReq {
    pub to_agent_id: String,
}

#[derive(Deserialize)]
pub struct CoordinationEventsQuery {
    pub token: Option<String>,
    #[serde(default)]
    pub after_sequence: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize)]
pub struct CoordinationMessageReq {
    pub body: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

async fn list_tasks(
    State(d): State<Shared>,
    Query(q): Query<CoordinationTasksQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let after = match (q.after_created.as_deref(), q.after_id.as_deref()) {
        (Some(created), Some(id)) => Some((created, id)),
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "after_created and after_id must be provided together",
            )
                .into_response();
        }
        (None, None) => None,
    };
    let status = match q.status.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(raw) => match parse_task_status(raw) {
            Ok(status) => Some(status),
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        },
        None => None,
    };
    let filter = TaskFilter {
        status,
        agent_id: q.agent_id.as_deref().filter(|s| !s.trim().is_empty()),
    };
    let summary = q.view.as_deref() == Some("summary");
    match d.store.list_tasks_page(&filter, after, q.limit.clamp(1, 500)) {
        Ok(tasks) if summary => Json(
            tasks
                .iter()
                .map(|task| task_summary(serde_json::to_value(task).unwrap_or_default()))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(tasks) => Json(tasks).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list tasks: {error}"),
        )
            .into_response(),
    }
}

async fn create_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Json(req): Json<CoordinationTaskReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let title = req.title.trim();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title is required").into_response();
    }
    if crate::session::is_inbox_session_id(req.session_id.trim()) {
        return (
            StatusCode::BAD_REQUEST,
            "session_id names an agent's inbox; route to a work session instead",
        )
            .into_response();
    }
    let Some(target_path) = crate::session::state_path_for_id(req.session_id.trim()) else {
        return (StatusCode::BAD_REQUEST, "session_id must name a real session").into_response();
    };
    let Some(state) = crate::session::read_session_state(&target_path) else {
        return (StatusCode::BAD_REQUEST, "session_id must name a real session").into_response();
    };
    let session_id = crate::session::session_id_for_state_path(&target_path);
    if mission_control::get_session(&d.mission_control_root, &session_id).is_err() {
        if let Err(error) = mission_control::create_session(
            &d.mission_control_root,
            &session_id,
            state.title.as_deref().unwrap_or("Managed session"),
            std::path::Path::new(&state.workspace),
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("register managed session: {error}"),
            )
                .into_response();
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let id = req
        .id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut task = Task::filed_by_human(
        id,
        title.to_string(),
        req.description.trim().to_string(),
        session_id.clone(),
        req.priority,
        now,
    );
    if let Some(plan) = req.plan {
        task.plan = plan.trim().to_string();
    }
    match d.store.create_task(&task) {
        Ok(()) => (StatusCode::CREATED, Json(task)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create task: {error}"),
        )
            .into_response(),
    }
}

async fn get_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.get_task(&task_id) {
        Ok(Some(task)) => Json(task).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get task: {error}"),
        )
            .into_response(),
    }
}

async fn update_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
    Json(req): Json<CoordinationTaskPatch>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let now = chrono::Utc::now().to_rfc3339();
    let title = req.title.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let description = req.description.as_deref().map(str::trim);
    let plan = req.plan.as_deref().map(str::trim);
    match d.store.update_task_full(&task_id, title, description, plan, req.priority, &now) {
        Ok(true) => match d.store.get_task(&task_id) {
            Ok(Some(task)) => Json(task).into_response(),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "task vanished").into_response(),
        },
        Ok(false) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update task: {error}"),
        )
            .into_response(),
    }
}

async fn set_task_status(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
    Json(req): Json<CoordinationTaskStatusReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let status = match parse_task_status(req.status.trim()) {
        Ok(status) => status,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let now = chrono::Utc::now().to_rfc3339();
    match d.store.set_task_status(&task_id, &status, &now) {
        Ok(true) => match d.store.get_task(&task_id) {
            Ok(Some(task)) => Json(task).into_response(),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "task vanished").into_response(),
        },
        Ok(false) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set task status: {error}"),
        )
            .into_response(),
    }
}

async fn task_links(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.task_links(&task_id) {
        Ok(links) => {
            let blockers = d.store.blockers_of(&task_id).unwrap_or_default();
            Json(serde_json::json!({ "links": links, "blocked_by": blockers })).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task links: {error}"),
        )
            .into_response(),
    }
}

async fn link_tasks(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
    Json(req): Json<CoordinationTaskLinkReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let to_task_id = req.to_task_id.trim();
    if to_task_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "to_task_id is required").into_response();
    }
    if to_task_id == task_id {
        return (StatusCode::BAD_REQUEST, "a task cannot link to itself").into_response();
    }
    let kind = match req.kind.as_deref().unwrap_or("blocks") {
        "blocks" => TaskLinkKind::Blocks,
        "relates_to" => TaskLinkKind::RelatesTo,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown link kind: {other}"),
            )
                .into_response();
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    let link = TaskLink {
        from_task_id: task_id,
        to_task_id: to_task_id.to_string(),
        kind,
        created_at: now,
    };
    match d.store.link_tasks(&link) {
        Ok(()) => (StatusCode::CREATED, Json(link)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("link tasks: {error}"),
        )
            .into_response(),
    }
}

async fn unlink_tasks(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path((task_id, other_id)): Path<(String, String)>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let mut removed = false;
    for (from, to) in [
        (task_id.as_str(), other_id.as_str()),
        (other_id.as_str(), task_id.as_str()),
    ] {
        for kind in [TaskLinkKind::Blocks, TaskLinkKind::RelatesTo] {
            match d.store.unlink_tasks(from, to, &kind) {
                Ok(true) => removed = true,
                Ok(false) => {}
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("unlink tasks: {error}"),
                    )
                        .into_response();
                }
            }
        }
    }
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no such link").into_response()
    }
}

async fn task_agents(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.list_task_agents(&task_id) {
        Ok(agents) => Json(agents).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task agents: {error}"),
        )
            .into_response(),
    }
}

async fn add_task_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
    Json(req): Json<CoordinationTaskAgentReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let agent_id = req.agent_id.trim();
    if agent_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "agent_id is required").into_response();
    }
    let now = chrono::Utc::now().to_rfc3339();
    match d.store.add_task_agent_full(
        &task_id,
        agent_id,
        req.role.trim(),
        req.work_session_id.as_deref(),
        req.scope.trim(),
        req.status.trim(),
        &now,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("add task agent: {error}"),
        )
            .into_response(),
    }
}

async fn remove_task_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path((task_id, agent_id)): Path<(String, String)>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.remove_task_agent(&task_id, &agent_id, &chrono::Utc::now().to_rfc3339()) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "agent is not on this task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("remove task agent: {error}"),
        )
            .into_response(),
    }
}

async fn transfer_task_lease(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path((task_id, agent_id)): Path<(String, String)>,
    Json(req): Json<CoordinationTransferLeaseReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let to_agent = req.to_agent_id.trim();
    if to_agent.is_empty() {
        return (StatusCode::BAD_REQUEST, "to_agent_id is required").into_response();
    }
    match d.store.transfer_task_session_lease(&task_id, &agent_id, to_agent) {
        Ok(()) => {
            if let Ok(Some(task)) = d.store.get_task(&task_id) {
                if !task.session_id.trim().is_empty() {
                    let role = if to_agent == "snippet" { "standard" } else { "specialized" };
                    let agent_opt = if to_agent == "snippet" { None } else { Some(to_agent) };
                    let _ = d.store.set_session_role(&task.session_id, role, agent_opt);
                }
                let now = chrono::Utc::now().to_rfc3339();
                let event = CoordinationEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    partition_key: format!("thread:{}", task.thread_id),
                    thread_id: task.thread_id.clone(),
                    sequence: 0,
                    event_type: "task.lease_transferred".to_string(),
                    actor_kind: "system".to_string(),
                    actor_id: "coordination".to_string(),
                    payload_version: 1,
                    payload: serde_json::json!({
                        "body": format!("Session lease transferred from {agent_id} to {to_agent}"),
                        "task_id": task_id,
                        "from_agent_id": agent_id,
                        "to_agent_id": to_agent,
                    }),
                    causation_id: None,
                    correlation_id: Some(task_id.clone()),
                    idempotency_key: uuid::Uuid::new_v4().to_string(),
                    created_at: now,
                };
                if let Ok(saved) = d.store.append_event(&event) {
                    let _ = d.coordination_events.send(saved.clone());
                    crate::session::emit_device_event(serde_json::json!({
                        "kind": "coordination_event",
                        "event": saved.clone(),
                    }));
                    queue_coordination_wake(saved);
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("transfer task lease: {error}"),
        )
            .into_response(),
    }
}

async fn thread_events(
    State(d): State<Shared>,
    Query(q): Query<CoordinationEventsQuery>,
    Path(thread_id): Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.events_for_thread(&thread_id, q.after_sequence, q.limit.clamp(1, 500)) {
        Ok(events) => Json(events).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("thread events: {error}"),
        )
            .into_response(),
    }
}

pub(super) async fn post_message(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(thread_id): Path<String>,
    Json(req): Json<CoordinationMessageReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let body = req.body.trim();
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "body is required").into_response();
    }
    let now = chrono::Utc::now().to_rfc3339();
    let event = CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        partition_key: format!("thread:{thread_id}"),
        thread_id: thread_id.clone(),
        sequence: 0,
        event_type: "message.posted".to_string(),
        actor_kind: "human".to_string(),
        actor_id: "human".to_string(),
        payload_version: 1,
        payload: serde_json::json!({
            "body": body,
            "thread_id": thread_id,
        }),
        causation_id: None,
        correlation_id: None,
        idempotency_key: req
            .idempotency_key
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        created_at: now,
    };
    match d.store.append_event(&event) {
        Ok(saved) => {
            let _ = d.coordination_events.send(saved.clone());
            crate::session::emit_device_event(serde_json::json!({
                "kind": "coordination_event",
                "event": saved.clone(),
            }));
            queue_coordination_wake(saved.clone());
            Json(saved).into_response()
        }
        Err(error) => (
            StatusCode::CONFLICT,
            format!("post coordination message: {error}"),
        )
            .into_response(),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
pub fn board_message_envelope(
    event: &CoordinationEvent,
    history: &[CoordinationEvent],
) -> String {
    board_message_envelope_with_task(event, history, None)
}

pub fn board_message_envelope_with_task(
    event: &CoordinationEvent,
    history: &[CoordinationEvent],
    task: Option<&Task>,
) -> String {
    let body = event
        .payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let mut digest = String::new();
    if history.is_empty() {
        digest.push_str("history: (start of the room)\n");
    } else {
        digest.push_str(&format!(
            "history: last {} message(s), oldest first\n",
            history.len()
        ));
        for prior in history {
            let prior_body = one_line(
                prior
                    .payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            );
            digest.push_str(&format!(
                "  {} [{}] {}: {}\n",
                prior.sequence, prior.actor_kind, prior.actor_id, prior_body
            ));
        }
    }
    let mut task_meta = String::new();
    if let Some(t) = task {
        task_meta.push_str(&format!(
            "task_id: {}\ntask_title: {}\ntask_status: {}\n",
            t.id, t.title, t.status
        ));
        if !t.plan.trim().is_empty() {
            task_meta.push_str(&format!("task_plan:\n{}\n", t.plan.trim()));
        }
    }
    format!(
        "[coordination_board_message]\nthread_id: {}\nfrom_id: {}\nfrom_kind: {}\n{task_meta}\
         rules: board message on the shared task board, not an ordinary chat turn. \
         Decide whether it needs a response, coordination, or an action in your scope; \
         reply on this same thread with post_coordination_message if coordination is required. \
         Call inspect_task to inspect the complete task plan, roster, or dependencies. \
         If NO action, response, or coordination is needed from you right now, you may NO-OP \
         (do not reply or post unnecessarily). Only the recent history is included — call \
         read_coordination_thread to see more.\n{history}body: {body}\n\
         [/coordination_board_message]",
        event.thread_id,
        event.actor_id,
        event.actor_kind,
        task_meta = task_meta,
        history = digest,
        body = body,
    )
}

pub(super) fn should_wake_mission_control(actor_id: &str) -> bool {
    actor_id != crate::mission_control::SESSION_ID
}

/// Consecutive agent-to-agent messages after which a room stops waking anyone
/// until a human posts. Without it two agents can answer each other forever.
pub(super) const MAX_AGENT_CHAIN: usize = 8;

/// Events that are recorded on the room but are not worth a turn for anyone.
const QUIET_EVENTS: &[&str] = &["task.agent_assigned", "task.dispatched"];

/// Events Mission Control already receives as a task notification; waking it
/// for the room post as well would deliver the same news twice.
const NOTIFIED_EVENTS: &[&str] = &["task.reported", "task.message"];

/// One session a board event should wake. `inbox_agent` is set when the
/// session is that agent's inbox, which may need creating first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WakeTarget {
    pub session: String,
    pub inbox_agent: Option<String>,
}

/// Whether the room's tail is an agent-only exchange long enough to stop.
fn agent_chain_exhausted(history: &[CoordinationEvent], event: &CoordinationEvent) -> bool {
    let tail: Vec<&CoordinationEvent> = history
        .iter()
        .chain(std::iter::once(event))
        .rev()
        .take_while(|e| e.actor_kind != "human")
        .collect();
    let mut actors: Vec<&str> = tail.iter().map(|e| e.actor_id.as_str()).collect();
    actors.sort();
    actors.dedup();
    tail.len() >= MAX_AGENT_CHAIN && actors.len() >= 2
}

/// The sessions a board event wakes. Pure, so the routing rules are testable.
///
/// Never the session the event came from: a worker posting to its own task
/// room used to be handed its own message back as a new turn.
pub(super) fn wake_targets(
    event: &CoordinationEvent,
    history: &[CoordinationEvent],
    participants: &[(String, String)],
    roster: &[crate::coordination::TaskAgent],
    task_session: Option<&str>,
) -> Vec<WakeTarget> {
    let mc = crate::mission_control::SESSION_ID;
    if QUIET_EVENTS.contains(&event.event_type.as_str()) || agent_chain_exhausted(history, event) {
        return Vec::new();
    }
    let mut targets: Vec<WakeTarget> = Vec::new();
    if participants.is_empty() {
        if should_wake_mission_control(&event.actor_id) {
            targets.push(WakeTarget { session: mc.to_string(), inbox_agent: None });
        }
        return targets;
    }
    let lease_to = (event.event_type == "task.lease_transferred")
        .then(|| event.payload.get("to_agent_id").and_then(|v| v.as_str()))
        .flatten();
    for (actor_id, actor_kind) in participants {
        if *actor_id == event.actor_id || lease_to.is_some_and(|to| to != actor_id) {
            continue;
        }
        if actor_id == mc || actor_kind == "system" {
            if !NOTIFIED_EVENTS.contains(&event.event_type.as_str()) {
                targets.push(WakeTarget { session: mc.to_string(), inbox_agent: None });
            }
            continue;
        }
        if actor_kind != "agent" {
            continue;
        }
        let member = roster.iter().find(|m| m.agent_id == *actor_id);
        if member.is_some_and(|m| m.removed_at.is_some()) {
            continue;
        }
        let working = member
            .filter(|m| m.status == "active")
            .and_then(|m| m.work_session_id.clone().or_else(|| task_session.map(str::to_string)))
            .filter(|session| !session.is_empty());
        targets.push(match working {
            Some(session) => WakeTarget { session, inbox_agent: None },
            None => WakeTarget {
                session: crate::session::inbox_session_id(actor_id),
                inbox_agent: Some(actor_id.clone()),
            },
        });
    }
    let origin = event.payload.get("origin_session").and_then(|v| v.as_str());
    let from_session = (event.actor_kind == "session").then_some(event.actor_id.as_str());
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| {
        Some(t.session.as_str()) != origin
            && Some(t.session.as_str()) != from_session
            && seen.insert(t.session.clone())
    });
    targets
}

pub async fn wake_coordination_thread_participants(
    d: &Shared,
    saved: &CoordinationEvent,
) {
    let participants = d
        .store
        .list_thread_participants(&saved.thread_id)
        .unwrap_or_default();
    let history: Vec<_> = d
        .store
        .recent_events_for_thread(
            &saved.thread_id,
            crate::coordination_tools::BOARD_HISTORY_ON_WAKE + 1,
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.sequence < saved.sequence)
        .collect();
    let task = d.store.get_task_by_thread(&saved.thread_id).ok().flatten();
    let roster = task
        .as_ref()
        .and_then(|t| d.store.list_task_agents(&t.id).ok())
        .unwrap_or_default();
    let targets = wake_targets(
        saved,
        &history,
        &participants,
        &roster,
        task.as_ref().map(|t| t.session_id.as_str()),
    );
    if targets.is_empty() {
        return;
    }
    let envelope = board_message_envelope_with_task(saved, &history, task.as_ref());
    for target in targets {
        let daemon = d.clone();
        let envelope = envelope.clone();
        tokio::spawn(async move {
            if let Some(agent_id) = target.inbox_agent.as_deref() {
                if let Err(error) = direct::ensure_agent_inbox(&daemon, agent_id).await {
                    eprintln!("[coordination] no inbox for `{agent_id}`: {error}");
                    return;
                }
            }
            daemon
                .deliver(&target.session, LoopInput::UserMessage(envelope))
                .await;
        });
    }
}
