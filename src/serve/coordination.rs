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

use super::{Auth, LoopInput, Shared, unauthorized, direct};

pub const COORDINATION_THREAD: &str = "system";

static ROOM_WAKE_NUDGE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// A wake older than this when the loop reaches it (the daemon was down) is
/// dropped: the room has moved on, and a late turn would answer stale news.
const ROOM_WAKE_MAX_AGE_SECS: i64 = 30 * 60;

/// Persist a room wake and nudge the loop. The row, not the nudge, is the wake.
pub fn queue_coordination_wake(store: &crate::store::Store, event: CoordinationEvent) {
    if let Err(error) = store.queue_room_wake(&event.event_id, &event.thread_id, &event.created_at) {
        eprintln!("[coordination] room wake not queued for {}: {error}", event.event_id);
    }
    ROOM_WAKE_NUDGE.notify_one();
}

pub async fn coordination_wake_loop(d: Shared) {
    loop {
        drain_room_wakes(&d).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ROOM_WAKE_NUDGE.notified()).await;
    }
}

async fn drain_room_wakes(d: &Shared) {
    let now = chrono::Utc::now();
    let pending = match d.store.list_pending_room_wakes(50, &now.to_rfc3339()) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("[coordination] room wakes unreadable: {error}");
            return;
        }
    };
    for event in pending {
        let stale = chrono::DateTime::parse_from_rfc3339(&event.created_at)
            .is_ok_and(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() > ROOM_WAKE_MAX_AGE_SECS);
        if !stale {
            wake_coordination_thread_participants(d, &event).await;
        }
        let _ = d.store.mark_direct_delivered(
            &event.event_id,
            "room",
            &event.thread_id,
            &chrono::Utc::now().to_rfc3339(),
        );
    }
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/coordination/tasks", get(list_tasks).post(create_task))
        .route("/coordination/tasks/{task_id}", get(get_task).patch(update_task))
        .route("/coordination/tasks/{task_id}/status", post(set_task_status))
        .route("/coordination/tasks/{task_id}/links", get(task_links).post(link_tasks))
        .route("/coordination/tasks/{task_id}/links/{other_id}", delete(unlink_tasks))
        .route("/coordination/tasks/{task_id}/turns", get(task_turns))
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
    pub after_priority: Option<i64>,
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
    #[serde(default)]
    pub to: Vec<String>,
}

async fn list_tasks(
    State(d): State<Shared>,
    Query(q): Query<CoordinationTasksQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let after = match (q.after_priority, q.after_created.as_deref(), q.after_id.as_deref()) {
        (Some(priority), Some(created), Some(id)) => Some((priority, created, id)),
        (None, None, None) => None,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "after_priority, after_created and after_id must be provided together",
            )
                .into_response();
        }
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
                .map(|task| task_summary(task_with_sub_status(&d.store, task)))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(tasks) => Json(tasks.iter().map(|task| task_with_sub_status(&d.store, task)).collect::<Vec<_>>()).into_response(),
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
    let Some(state) = crate::session::read_session_meta(&target_path) else {
        return (StatusCode::BAD_REQUEST, "session_id must name a real session").into_response();
    };
    let session_id = crate::session::session_id_for_state_path(&target_path);
    if mission_control::get_session(&d.mission_control_root, &session_id).is_err()
        && let Err(error) = mission_control::create_session(
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

/// The task as stored, plus the derived board fields (`status_reason`,
/// `waiting_on`, `current_step`). Additive, so older clients ignore them.
pub(super) fn task_with_sub_status(store: &crate::store::Store, task: &Task) -> serde_json::Value {
    let mut value = serde_json::to_value(task).unwrap_or_default();
    if let (Some(map), Ok(serde_json::Value::Object(extra))) =
        (value.as_object_mut(), serde_json::to_value(store.board_sub_status(task)))
    {
        map.extend(extra);
        map.extend(crate::task_state_tools::api_fields(store, task));
    }
    value
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
        Ok(Some(task)) => Json(task_with_sub_status(&d.store, &task)).into_response(),
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
    let note = format!("Marked {status} on the board.");
    match d.store.move_task(&task_id, status, &note, &now) {
        Ok(task) => Json(task).into_response(),
        Err(error) => store_error(error),
    }
}

/// The response for a store error: refused transitions are conflicts.
pub(super) fn store_error(error: crate::store::StoreError) -> Response {
    use crate::store::StoreError;
    let code = match &error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => StatusCode::NOT_FOUND,
        StoreError::AlreadyTerminal { .. }
        | StoreError::NotRetryable { .. }
        | StoreError::InvalidTransition { .. }
        | StoreError::NotRevisable { .. }
        | StoreError::StaleTaskState { .. }
        | StoreError::WorkerBusy { .. }
        | StoreError::DependencyCycle { .. } => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, error.to_string()).into_response()
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
        Err(error) => store_error(error),
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

async fn task_turns(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Path(task_id): Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.list_turns(&task_id) {
        Ok(turns) => Json(turns).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task turns: {error}"),
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
    let now = chrono::Utc::now().to_rfc3339();
    let live_task = d.store.get_task(&task_id).ok().flatten().filter(|task| {
        task.status == TaskStatus::InProgress && crate::members::task_workspace(&d.store, task).is_some()
    });
    let mut body = format!("Session lease transferred from {agent_id} to {to_agent}");
    let moved = match &live_task {
        Some(task) => {
            let handover = crate::coordination::handoffs::Handover {
                id: uuid::Uuid::new_v4().to_string(),
                task_id: task_id.clone(),
                from_agent: agent_id.clone(),
                to_agent: to_agent.to_string(),
                done: "The user moved the turn.".to_string(),
                next: String::new(),
                risks: String::new(),
                files: Vec::new(),
                created_at: now.clone(),
                acked_at: None,
            };
            crate::members::pass_turn(&d.store, task, to_agent, &handover, &now).map(|passed| {
                body = crate::members::handover_body(&handover, &passed);
            })
        }
        None => d
            .store
            .transfer_task_session_lease(&task_id, &agent_id, to_agent)
            .map_err(|e| e.to_string()),
    };
    match moved {
        Ok(()) => {
            if let Ok(Some(task)) = d.store.get_task(&task_id) {
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
                        "body": body,
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
                    queue_coordination_wake(&d.store, saved);
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
    let mut payload = serde_json::json!({
        "body": body,
        "thread_id": thread_id,
    });
    let to: Vec<&str> = req.to.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    if !to.is_empty() {
        let Ok(Some(task)) = d.store.get_task_by_thread(&thread_id) else {
            return (StatusCode::BAD_REQUEST, "only a task's room can address its members").into_response();
        };
        let roster = d.store.list_task_agents(&task.id).unwrap_or_default();
        if let Some(stranger) = to.iter().find(|a| {
            **a != crate::mission_control::SESSION_ID
                && !roster.iter().any(|m| m.agent_id == **a && m.removed_at.is_none())
        }) {
            return (StatusCode::BAD_REQUEST, format!("{stranger} isn't on this task")).into_response();
        }
        if crate::members::task_workspace(&d.store, &task).is_some() {
            crate::members::seat_holder(&d.store, &task);
            for agent in to.iter().filter(|a| **a != crate::mission_control::SESSION_ID) {
                if let Err(error) = crate::members::seat(&d.store, &task, agent, &now) {
                    return (StatusCode::CONFLICT, error).into_response();
                }
            }
        }
        payload["to"] = serde_json::json!(to);
    }
    let event = CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        partition_key: format!("thread:{thread_id}"),
        thread_id: thread_id.clone(),
        sequence: 0,
        event_type: "message.posted".to_string(),
        actor_kind: "human".to_string(),
        actor_id: "human".to_string(),
        payload_version: 1,
        payload,
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
            queue_coordination_wake(&d.store, saved.clone());
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
    board_message_envelope_with_task(event, history, None, None)
}

pub fn board_message_envelope_with_task(
    event: &CoordinationEvent,
    history: &[CoordinationEvent],
    task: Option<&Task>,
    ledger: Option<&str>,
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
        // A ledger update's body already is the delta; the full ledger would bury it.
        if let Some(ledger) = ledger.filter(|l| !l.is_empty() && event.event_type != "task.state_updated") {
            task_meta.push_str(&format!("{ledger}\n"));
        }
    }
    let note = if event.event_type == "task.lease_transferred" {
        "the turn on this task has passed to you: it's yours to carry out here, and you may change files now."
    } else {
        "a teammate posted on the shared task board; this isn't a turn in your own \
         conversation. If it needs your input or changes what you're doing, reply (post_task_coordination on a task thread, \
         send_agent_message to the sender elsewhere) or act on it; if not, end your turn \
         without a reply."
    };
    format!(
        "[coordination_board_message]\nthread_id: {}\nfrom_id: {}\nfrom_kind: {}\n{tags}{task_meta}\
         note: {note} inspect_task shows the task's plan and roster; only recent history is \
         shown here, and read_coordination_thread has more.\n{history}body: {body}\n\
         [/coordination_board_message]",
        event.thread_id,
        event.actor_id,
        event.actor_kind,
        tags = crate::coordination::typed::envelope_lines(&event.payload),
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

/// Limits for a room of seated members, counted from the last human or Mission
/// Control post. They stand in for the chain stop above, which would end real
/// collaboration, and hitting one asks rather than going silent.
const ROOM_TALK_BUDGET: usize = 40;
const ROOM_TURN_BUDGET: usize = 20;
const ROOM_HOURS_BUDGET: i64 = 12;
const ROOM_TOKEN_BUDGET: u64 = 5_000_000;

/// Why a room of seated members should stop waking anyone, if it should.
pub(super) fn room_over_budget(events: &[CoordinationEvent], now: chrono::DateTime<chrono::Utc>, tokens: u64) -> Option<String> {
    let mc = crate::mission_control::SESSION_ID;
    let start = events
        .iter()
        .rposition(|e| e.actor_kind == "human" || (e.actor_id == mc && CHATTER_EVENTS.contains(&e.event_type.as_str())))
        .map_or(0, |at| at + 1);
    let run = &events[start..];
    let turns = run.iter().filter(|e| e.event_type == "task.lease_transferred").count();
    let last_pass = run.iter().rposition(|e| e.event_type == "task.lease_transferred").map_or(0, |at| at + 1);
    let talk = run[last_pass..].iter().filter(|e| CHATTER_EVENTS.contains(&e.event_type.as_str())).count();
    if talk >= ROOM_TALK_BUDGET {
        return Some(format!("{talk} messages since the turn last moved, so they may be going in circles"));
    }
    if turns >= ROOM_TURN_BUDGET {
        return Some(format!("the turn has passed {turns} times"));
    }
    if tokens >= ROOM_TOKEN_BUDGET {
        return Some(format!("its members have used about {tokens} fresh tokens"));
    }
    let hours = run
        .first()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(&e.created_at).ok())
        .map_or(0, |at| (now - at.with_timezone(&chrono::Utc)).num_hours());
    (hours >= ROOM_HOURS_BUDGET).then(|| format!("they have been at it for {hours} hours"))
}

/// Events that are recorded on the room but are not worth a turn for anyone.
const QUIET_EVENTS: &[&str] = &["task.agent_assigned", "task.dispatched"];

/// Events Mission Control already receives as a task notification; waking it
/// for the room post as well would deliver the same news twice.
const NOTIFIED_EVENTS: &[&str] = &["task.reported", "task.message", "task.state_updated", "task.in_review"];

/// One session a board event should wake. `inbox_agent` is set when the
/// session is that agent's inbox, which may need creating first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WakeTarget {
    pub session: String,
    pub inbox_agent: Option<String>,
}

const CHATTER_EVENTS: &[&str] = &["message.posted", "task.message"];

/// Whether the room's tail is an agent-only exchange long enough to stop.
/// `human_at` is the room's own last human post, so a human speaking in one
/// room never re-arms another.
fn agent_chain_exhausted(
    history: &[CoordinationEvent],
    event: &CoordinationEvent,
    human_at: Option<&str>,
) -> bool {
    use crate::coordination::typed::{Urgency, urgency_of};
    let chatter = |e: &&CoordinationEvent| CHATTER_EVENTS.contains(&e.event_type.as_str());
    let kind = event.payload.get("kind").and_then(|v| v.as_str());
    if !chatter(&event)
        || event.actor_id == crate::mission_control::SESSION_ID
        || urgency_of(&event.payload) == Urgency::Urgent
        || matches!(kind, Some("result" | "blocker" | "interrupt"))
    {
        return false;
    }
    let human = human_at.and_then(|h| chrono::DateTime::parse_from_rfc3339(h).ok());
    let after_human = |e: &&CoordinationEvent| {
        human.is_none_or(|h| chrono::DateTime::parse_from_rfc3339(&e.created_at).is_ok_and(|t| t > h))
    };
    let tail: Vec<&CoordinationEvent> = history
        .iter()
        .chain(std::iter::once(event))
        .rev()
        .take_while(|e| {
            e.actor_kind != "human"
                && e.actor_id != crate::mission_control::SESSION_ID
                && chatter(e)
                && after_human(e)
        })
        .collect();
    let mut actors: Vec<&str> = tail.iter().map(|e| e.actor_id.as_str()).collect();
    actors.sort();
    actors.dedup();
    tail.len() >= MAX_AGENT_CHAIN && actors.len() >= 2
}

/// The sessions a board event wakes. Pure, so the routing rules are testable.
/// `idle_agents` are the paused, draining and disabled ones, which are not woken.
///
/// Never the session the event came from: a worker posting to its own task
/// room used to be handed its own message back as a new turn.
pub(super) fn wake_targets(
    event: &CoordinationEvent,
    history: &[CoordinationEvent],
    participants: &[(String, String)],
    roster: &[crate::coordination::TaskAgent],
    task_session: Option<&str>,
    idle_agents: &[String],
    human_at: Option<&str>,
) -> Vec<WakeTarget> {
    let mc = crate::mission_control::SESSION_ID;
    let seats = roster.iter().any(|m| m.removed_at.is_none() && m.work_session_id.as_deref().is_some_and(|s| !s.is_empty()));
    if QUIET_EVENTS.contains(&event.event_type.as_str()) || (!seats && agent_chain_exhausted(history, event, human_at)) {
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
    let live = || roster.iter().filter(|m| m.removed_at.is_none());
    let seated = live().any(|m| m.work_session_id.as_deref().is_some_and(|s| !s.is_empty()));
    let addressed: Vec<&str> = event
        .payload
        .get("to")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .collect();
    let holder = live().find(|m| m.status == "active").map(|m| m.agent_id.as_str());
    let chatter = CHATTER_EVENTS.contains(&event.event_type.as_str());
    let blocker = event.payload.get("kind").and_then(|v| v.as_str()) == Some("blocker");
    for (actor_id, actor_kind) in participants {
        if *actor_id == event.actor_id || lease_to.is_some_and(|to| to != actor_id) {
            continue;
        }
        // Waking costs a model turn per member, so once a task has seats a post
        // reaches only who it names, or the holder when it names nobody.
        if seated && lease_to.is_none() {
            let is_mc = actor_id == mc || actor_kind == "system";
            let wanted = if !chatter {
                is_mc
            } else if addressed.is_empty() {
                holder == Some(actor_id.as_str()) || (is_mc && blocker)
            } else {
                addressed.contains(&actor_id.as_str()) || (is_mc && blocker)
            };
            if !wanted {
                continue;
            }
        }
        if actor_id == mc || actor_kind == "system" {
            if !NOTIFIED_EVENTS.contains(&event.event_type.as_str()) {
                targets.push(WakeTarget { session: mc.to_string(), inbox_agent: None });
            }
            continue;
        }
        if actor_kind != "agent" || idle_agents.contains(actor_id) {
            continue;
        }
        let member = roster.iter().find(|m| m.agent_id == *actor_id);
        if member.is_some_and(|m| m.removed_at.is_some()) {
            continue;
        }
        let working = member
            .and_then(|m| {
                m.work_session_id
                    .clone()
                    .filter(|session| !session.is_empty())
                    .or_else(|| (m.status == "active").then(|| task_session.map(str::to_string)).flatten())
            })
            .filter(|session| !session.is_empty());
        targets.push(match working {
            Some(session) => WakeTarget { session, inbox_agent: None },
            None => WakeTarget {
                session: crate::session::inbox_session_id(actor_id),
                inbox_agent: Some(actor_id.clone()),
            },
        });
    }
    if seated && chatter && blocker && event.actor_id != mc {
        targets.push(WakeTarget { session: mc.to_string(), inbox_agent: None });
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

/// The identity an agent brings into a session it does not own.
///
/// A session's system prompt is fixed when its loop starts, so an agent that
/// takes the lease on a running conversation brings its identity in the message
/// that hands it the work. Empty when the session already runs as that agent.
pub(super) fn identity_overlay(d: &super::Daemon, agent_id: &str, session_id: &str) -> String {
    let owner = crate::session::state_path_for_id(session_id)
        .and_then(|path| crate::session::read_session_sidecar(&path))
        .and_then(|sidecar| sidecar.agent_id);
    let native = owner
        .as_deref()
        .unwrap_or(crate::coordination::SNIPPET_AGENT_ID);
    if native == agent_id {
        return String::new();
    }
    let identity = crate::coordination::AgentHome::new(
        crate::coordination::agents_root(&d.mission_control_root),
        agent_id,
    )
    .and_then(|home| home.read_identity());
    match identity {
        Ok(identity) => format!(
            "[agent_identity]\nagent_id: {agent_id}\nYou are working this task as \
             {agent_id} until the lease moves. This identity takes precedence over \
             the one this session started with.\n\n{}\n[/agent_identity]\n",
            identity.trim()
        ),
        Err(error) => {
            eprintln!("[coordination] identity of `{agent_id}` unavailable: {error}");
            String::new()
        }
    }
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
    if let Some(task) = task.as_ref().filter(|_| roster.iter().any(|m| m.removed_at.is_none() && m.work_session_id.as_deref().is_some_and(|s| !s.is_empty()))) {
        let events = d.store.events_for_thread(&saved.thread_id, 0, 2000).unwrap_or_default();
        let sessions: Vec<String> = roster.iter().filter_map(|m| m.work_session_id.clone()).collect();
        let since = chrono::DateTime::parse_from_rfc3339(&task.created_at).map_or(0, |at| at.timestamp());
        let tokens = d.store.fresh_tokens_for_sessions(&sessions, since).unwrap_or(0);
        if let Some(reason) = room_over_budget(&events, chrono::Utc::now(), tokens) {
            let note = format!("The room stopped waking its members: {reason}. Check the room; a post from you lets them carry on, or take it to the user.");
            let _ = d.store.update_task_in(&task.id, &chrono::Utc::now().to_rfc3339(), |t| t.notify_once("room_limit", &note));
            return;
        }
    }
    let idle_agents: Vec<String> = d
        .store
        .list_agents()
        .unwrap_or_default()
        .into_iter()
        .filter(|agent| agent.status != crate::coordination::types::AgentStatus::Active)
        .map(|agent| agent.id)
        .collect();
    let human_at = d.store.last_human_event_at(&saved.thread_id).ok().flatten();
    let targets = wake_targets(
        saved,
        &history,
        &participants,
        &roster,
        task.as_ref().map(|t| t.session_id.as_str()),
        &idle_agents,
        human_at.as_deref(),
    );
    if targets.is_empty() {
        return;
    }
    let room = crate::members::room_line(&roster);
    let ledger = task.as_ref().map(|t| {
        let ledger = crate::task_state_tools::ledger_block(&d.store, t, false);
        if room.is_empty() { ledger } else { format!("{room}\n{ledger}") }
    });
    let envelope = board_message_envelope_with_task(saved, &history, task.as_ref(), ledger.as_deref());
    let incoming = (saved.event_type == "task.lease_transferred")
        .then(|| saved.payload.get("to_agent_id").and_then(|v| v.as_str()))
        .flatten();
    for target in targets {
        let daemon = d.clone();
        let payload = saved.payload.clone();
        let urgent = crate::coordination::typed::urgency_of(&payload) == crate::coordination::typed::Urgency::Urgent;
        let envelope = match incoming {
            Some(agent_id) if target.inbox_agent.is_none() => {
                format!("{}{envelope}", identity_overlay(d, agent_id, &target.session))
            }
            _ => envelope.clone(),
        };
        tokio::spawn(async move {
            if let Some(agent_id) = target.inbox_agent.as_deref()
                && let Err(error) = direct::ensure_agent_inbox(&daemon, agent_id).await {
                    eprintln!("[coordination] no inbox for `{agent_id}`: {error}");
                    return;
                }
            if urgent {
                direct::deliver_by_urgency(&daemon, &target.session, envelope, &payload).await;
            } else {
                daemon
                    .deliver(&target.session, LoopInput::UserMessage(envelope))
                    .await;
            }
        });
    }
}
