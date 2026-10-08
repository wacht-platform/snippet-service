use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Json, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use serde::Deserialize;

use crate::config::workspaces_root;
use crate::serve::task_summary;
use crate::coordination::{
    HandoffMode, NotificationMarker, Task, TaskResult, TaskStatus,
};
use crate::mission_control::{self, ManagedSession};
use crate::session::{
    read_session_profile, start_mission_control_session, start_session_with_browser_summary,
    write_session_sidecar, SessionRole, SessionSidecar,
};
use super::{
    apply_profile, live_from_handle, record_dispatch_notice, unauthorized, Auth, Daemon,
    LoopInput, Shared,
};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/agents/build", post(build_agent_from_prompt))
        .route("/mission-control/overview", get(overview))
        .route("/mission-control/settings", get(settings).put(update_settings))
        .route("/mission-control/autonomy", get(autonomy).post(update_autonomy))
        .route("/mission-control/open", post(open))
        .route("/mission-control/tasks", get(tasks).post(create_task))
        .route("/mission-control/tasks/{id}", get(task).put(update_task))
        .route("/mission-control/tasks/{id}/archive", post(archive_task))
        .route("/mission-control/sessions", get(sessions).post(create_session))
        .route("/mission-control/sessions/{id}", put(update_session))
        .route("/mission-control/sessions/{id}/archive", post(archive_session))
}

#[derive(Deserialize)]
struct MissionListQuery {
    token: Option<String>,
    archived: Option<bool>,
    #[serde(default)]
    view: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct MissionTaskReq {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    handoff_mode: Option<String>,
    #[serde(default)]
    owned_paths: Vec<String>,
    #[serde(default)]
    status: Option<String>,
}

pub fn validated_owned_paths(
    raw_paths: &[String],
    workspace: &Path,
) -> Result<Vec<PathBuf>, String> {
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut out = Vec::new();
    for raw in raw_paths {
        let path = PathBuf::from(raw);
        let mut prefix = path.clone();
        let mut suffix = Vec::new();
        let resolved = loop {
            match prefix.canonicalize() {
                Ok(canonical) => {
                    let mut resolved = canonical;
                    for part in suffix.iter().rev() {
                        resolved.push(part);
                    }
                    break resolved;
                }
                Err(_) => match prefix.parent() {
                    Some(parent) => {
                        if let Some(name) = prefix.file_name() {
                            suffix.push(name.to_os_string());
                        }
                        prefix = parent.to_path_buf();
                    }
                    None => break path.clone(),
                },
            }
        };
        if resolved.starts_with(&root) {
            out.push(resolved);
        } else {
            return Err(format!(
                "owned path '{raw}' is outside the target session's workspace"
            ));
        }
    }
    if out.is_empty() {
        out.push(root);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct MissionSessionReq {
    #[serde(default)]
    session_id: String,
    folder: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Deserialize)]
struct MissionSessionUpdate {
    #[serde(default)]
    title: Option<String>,
}

fn mission_task_view(task: &Task) -> serde_json::Value {
    serde_json::json!({
        "id": task.id,
        "title": task.title,
        "description": task.description,
        "status": task.status,
        "session_id": task.session_id,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "archived": task.status.is_terminal(),
        "owned_paths": task.owned_paths,
        "handoff": task.handoff,
        "handoff_mode": task.handoff_mode,
        "result": task.result,
        "notifications": task.notifications,
        "dispatch_failures": task.dispatch_failures,
    })
}

fn mission_session_view(session: &ManagedSession, task_count: usize) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "session_id": session.id,
        "folder": session.workspace,
        "title": session.label,
        "status": session.status,
        "created_at": session.created_at,
        "last_active_at": session.updated_at,
        "task_count": task_count,
        "archived": matches!(session.status, mission_control::SessionStatus::Archived),
        "metadata": session.tags,
    })
}

pub fn mission_error(error: String) -> Response {
    (StatusCode::BAD_REQUEST, error).into_response()
}

#[derive(Deserialize)]
struct AgentBuildReq {
    prompt: String,
}

async fn build_agent_from_prompt(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<AgentBuildReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let prompt = req.prompt.trim();
    if prompt.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            "prompt must describe the desired agent",
        )
            .into_response();
    }
    if !d.store.has_conversation(mission_control::SESSION_ID).unwrap_or(false) {
        return (StatusCode::CONFLICT, "open Mission Control before building an agent").into_response();
    }
    let session_id = crate::mission_control::SESSION_ID;
    let task_id = uuid::Uuid::new_v4().to_string();
    let title = "Build specialized agent";
    let description = format!(
        "Build a specialized agent from this user brief:\n\n{prompt}\n\nResearch the role first (web_search / web_read when available): the domain's standards, the checks an expert runs, common failure modes. Choose a short kebab-case id and a display name, then write the identity: who the agent is, its mandate, how it works step by step, what it checks, and how it reports. Create the agent with register_agent, then report this task with report_mission_task: the agent id, a two-line identity summary, and the sources you used."
    );
    let task = Task::dispatched_to(
        task_id,
        session_id.to_string(),
        title.to_string(),
        description,
        Vec::new(),
        HandoffMode::Resume,
        "mission-control",
        "mission-control",
        chrono::Utc::now().to_rfc3339(),
    );
    if let Err(error) = d.store.create_task(&task) {
        return mission_error(error.to_string());
    }
    match dispatch_mission_task(&d, &task.id).await {
        Ok(task) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "build_id": task.id,
                "status": format!("{:?}", task.status).to_lowercase(),
                "task_id": task.id,
            })),
        )
            .into_response(),
        Err(error) => mission_error(error),
    }
}

async fn overview(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    let Ok(tasks) = d.store.list_tasks(None, None) else {
        return mission_error("could not read Mission Control tasks".to_string());
    };
    let Ok(sessions) = mission_control::list_sessions(root, false) else {
        return mission_error("could not read Mission Control sessions".to_string());
    };
    let active_tasks = tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    let done_tasks = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Done)
        .count();
    let active_sessions = sessions
        .iter()
        .filter(|session| session.status == mission_control::SessionStatus::Active)
        .count();
    let recent_tasks = tasks
        .iter()
        .rev()
        .take(12)
        .map(mission_task_view)
        .map(task_summary)
        .collect::<Vec<_>>();
    let recent_sessions = sessions
        .iter()
        .rev()
        .take(12)
        .map(|session| {
            let count = tasks
                .iter()
                .filter(|task| task.session_id == session.id)
                .count();
            mission_session_view(session, count)
        })
        .collect::<Vec<_>>();
    let mc_session_id = Some(mission_control::SESSION_ID);
    Json(serde_json::json!({
        "active_tasks": active_tasks,
        "completed_tasks": done_tasks,
        "total_tasks": tasks.len(),
        "active_sessions": active_sessions,
        "total_sessions": sessions.len(),
        "recent_tasks": recent_tasks,
        "recent_sessions": recent_sessions,
        "mc_session_id": mc_session_id,
    }))
    .into_response()
}

async fn tasks(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let summary = q.view.as_deref() == Some("summary");
    match d.store.list_tasks(None, None) {
        Ok(tasks) => Json(
            tasks
                .iter()
                .filter(|task| {
                    q.archived
                        .is_none_or(|archived| archived == task.status.is_terminal())
                })
                .skip(q.offset.unwrap_or(0))
                .take(q.limit.unwrap_or(usize::MAX))
                .map(mission_task_view)
                .map(|view| if summary { task_summary(view) } else { view })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

async fn task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match d.store.get_task(&id) {
        Ok(Some(task)) => Json(mission_task_view(&task)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

pub(super) async fn dispatch_mission_task(d: &Daemon, task_id: &str) -> Result<Task, String> {
    let root = &d.mission_control_root;
    let now = chrono::Utc::now().to_rfc3339();
    // Checked before the claim so a task waiting on someone else's paths stays
    // queued without being claimed and released every tick.
    if let Some(offered) = await_claim(d, task_id, &now)? {
        return Ok(offered);
    }
    if let Some(waiting) = wait_for_path_owners(d, task_id, &now, false)? {
        return Ok(waiting);
    }
    if let Some(waiting) = wait_for_worker(d, task_id, &now)? {
        return Ok(waiting);
    }
    let task = d
        .store
        .claim_task_for_dispatch(task_id, &now)
        .map_err(|error| error.to_string())?;
    if task.status != TaskStatus::InProgress || task.reporting_session.is_none() {
        return Ok(task);
    }
    if task.session_id.trim().is_empty() {
        release_failed_claim(d, &task, "task has no target session")?;
        return Err("task has no target session".to_string());
    }
    for blocker_id in d
        .store
        .blockers_of(task_id)
        .map_err(|error| error.to_string())?
    {
        let other = match d.store.get_task(&blocker_id) {
            Ok(Some(other)) => other,
            Ok(None) => {
                let error = format!("dependency {blocker_id} no longer exists");
                release_failed_claim(d, &task, &error)?;
                return Err(error);
            }
            Err(error) => {
                let message = error.to_string();
                release_failed_claim(d, &task, &message)?;
                return Err(message);
            }
        };
        if other.status != TaskStatus::Done {
            let reason = if other.status.is_terminal() {
                format!(
                    "dependency {blocker_id} is {}; retry it, or cancel or unlink this task",
                    other.status
                )
            } else {
                format!("waiting on dependency {blocker_id}")
            };
            let blocked = d
                .store
                .update_task_in(task_id, &now, |task| {
                    task.status = TaskStatus::Blocked;
                    task.reporting_session = None;
                    task.notify_once("blocked", &reason);
                })
                .map_err(|error| error.to_string())?;
            return Ok(blocked);
        }
    }
    // Mission Control's own work (building an agent) runs in its home; it is
    // not a managed session, and listing it as one would show it to the human.
    let (session_id, workspace) = if mission_control::is_session_id(&task.session_id) {
        if !d.store.has_conversation(mission_control::SESSION_ID).unwrap_or(false) {
            let error = "Mission Control has not been opened yet".to_string();
            release_failed_claim(d, &task, &error)?;
            return Err(error);
        }
        (mission_control::SESSION_ID.to_string(), mission_control::workspace_path())
    } else {
        let managed = match mission_control::get_session(root, &task.session_id) {
            Ok(managed) => managed,
            Err(error) => {
                release_failed_claim(d, &task, &error)?;
                return Err(error);
            }
        };
        if managed.status != mission_control::SessionStatus::Active {
            let error = "target session is archived".to_string();
            release_failed_claim(d, &task, &error)?;
            return Err(error);
        }
        (managed.id, managed.workspace)
    };
    // A concurrent dispatch may have taken the paths between the check above
    // and the claim; hand the claim back rather than deliver into them.
    if let Some(waiting) = wait_for_path_owners(d, task_id, &now, true)? {
        return Ok(waiting);
    }
    let handoff = task
        .handoff
        .as_ref()
        .map(|handoff| handoff.description.as_str())
        .unwrap_or("");
    let mode_line = match task.handoff_mode {
        HandoffMode::Fresh => {
            "handoff_mode: fresh (self-contained briefing; the session has no prior context)\n"
        }
        HandoffMode::Resume => "",
    };
    let roster = d.store.list_task_agents(task_id).unwrap_or_default();
    let active_worker = roster
        .iter()
        .find(|m| m.status == "active" && m.removed_at.is_none())
        .map(|m| m.agent_id.as_str())
        .unwrap_or("snippet");

    let mut plan_line = String::new();
    if !task.plan.trim().is_empty() {
        plan_line = format!("plan:\n{}\n", task.plan.trim());
    }

    let roster_summary: Vec<String> = roster
        .iter()
        .filter(|m| m.removed_at.is_none())
        .map(|m| format!("{} ({}, {})", m.agent_id, m.role, m.status))
        .collect();
    let roster_line = if roster_summary.is_empty() {
        String::new()
    } else {
        format!("collaborators: [{}]\n", roster_summary.join(", "))
    };

    let owned_line = if task.owned_paths.is_empty() {
        String::new()
    } else {
        let paths: Vec<String> = task.owned_paths.iter().map(|p| p.display().to_string()).collect();
        format!("owned_paths: [{}]\n", paths.join(", "))
    };

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|text| !text.is_empty())
    };
    let revision_line = match (
        git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        git(&["log", "-1", "--format=%h %s"]),
    ) {
        (Some(branch), Some(head)) => format!("branch: {branch} at {head}\n"),
        _ => String::new(),
    };
    let identity = super::coordination::identity_overlay(d, active_worker, &session_id);
    let text = format!(
        "{identity}[mission_control_task]\ntask_id: {}\ntitle: {}\nrequested_by: {} {}\n{}active_agent: {}\n{plan_line}{roster_line}{owned_line}workspace: {}\n{revision_line}scope:\n{}\n\nBegin now. Before you stop, report with report_mission_task (task_id {}): what was done, files changed, how it was verified, anything left open.\n[/mission_control_task]",
        task.id,
        task.title,
        task.created_by_kind,
        task.created_by_id,
        mode_line,
        active_worker,
        workspace.display(),
        if handoff.is_empty() {
            task.description.as_str()
        } else {
            handoff
        },
        task.id,
    );
    if let Some(profile) = task.profile.as_deref() {
        if let Err(error) = d.run_session_on_profile(&session_id, profile, false).await {
            release_failed_claim(d, &task, &error)?;
            return Err(error);
        }
    }
    d.deliver(&session_id, LoopInput::UserMessage(text)).await;
    record_dispatch_notice(d, &task).await;
    let task = d
        .store
        .update_task_in(task_id, &now, |t| t.dispatch_failures = 0)
        .map_err(|error| error.to_string())?;
    Ok(task)
}

/// If other in-progress tasks own any of this task's paths, leave it queued
/// behind them and tell Mission Control once. `claimed` hands a claim back.
///
/// The wait is not written as a `blocks` link: that edge would outlive the
/// conflict, and a failed owner would then hold this task forever.
fn wait_for_path_owners(
    d: &Daemon,
    task_id: &str,
    now: &str,
    claimed: bool,
) -> Result<Option<Task>, String> {
    let Some(task) = d.store.get_task(task_id).map_err(|error| error.to_string())? else {
        return Err(format!("unknown task {task_id}"));
    };
    let expected = if claimed { TaskStatus::InProgress } else { TaskStatus::Todo };
    if task.status != expected {
        return Ok(None);
    }
    let conflicts = d
        .store
        .task_path_conflicts(task_id, &task.owned_paths)
        .map_err(|error| error.to_string())?;
    if conflicts.is_empty() {
        return Ok(None);
    }
    let mut owners: Vec<String> = conflicts.into_iter().map(|(owner, _)| owner).collect();
    owners.sort();
    owners.dedup();
    let reason = format!("waiting on workspace owner(s): {}", owners.join(", "));
    if !claimed && task.notifications.iter().any(|n| n.message == reason) {
        return Ok(Some(task));
    }
    let task = d
        .store
        .update_task_in(task_id, now, |task| {
            if claimed {
                task.status = TaskStatus::Todo;
                task.reporting_session = None;
            }
            task.notify_once("blocked", &reason);
        })
        .map_err(|error| error.to_string())?;
    Ok(Some(task))
}

/// If the agent holding the task's lease is paused, draining or disabled, leave
/// the task queued until it is active again, and tell Mission Control once.
/// A task starts only once its agent has claimed it. Until then the agent is on
/// the roster as `offered` and its inbox gets the offer, once.
fn await_claim(d: &Daemon, task_id: &str, now: &str) -> Result<Option<Task>, String> {
    let Some(task) = d.store.get_task(task_id).map_err(|error| error.to_string())? else {
        return Err(format!("unknown task {task_id}"));
    };
    if task.status != TaskStatus::Todo || mission_control::is_session_id(&task.session_id) {
        return Ok(None);
    }
    let roster = d.store.list_task_agents(task_id).map_err(|error| error.to_string())?;
    let current: Vec<_> = roster.iter().filter(|m| m.removed_at.is_none()).collect();
    if current.iter().any(|m| m.status == "active") {
        return Ok(None);
    }
    let agent = match current.iter().find(|m| m.status == "offered") {
        Some(member) => member.agent_id.clone(),
        None => {
            let agent = crate::session::state_path_for_id(&task.session_id)
                .and_then(|path| crate::session::read_session_sidecar(&path))
                .and_then(|sidecar| sidecar.agent_id)
                .unwrap_or_else(|| crate::coordination::SNIPPET_AGENT_ID.to_string());
            if d.store.get_agent(&agent).ok().flatten().is_none() {
                return Ok(None);
            }
            d.store
                .add_task_agent_full(task_id, &agent, "implementer", None, "", "offered", now)
                .map_err(|error| error.to_string())?;
            agent
        }
    };
    if agent == mission_control::SESSION_ID {
        return Ok(None);
    }
    let marker = crate::coordination::offer_marker(&agent);
    if let Some(offered_at) = task.notifications.iter().find(|n| n.target == marker).map(|n| n.message.clone()) {
        if let Some(error) = inbox_failure_since(&agent, &offered_at) {
            let note = format!(
                "The offer to {agent} is stuck: its inbox can't run ({error}). Start the work now by handing {agent} the lease (transfer_mission_task_lease), offer it to another agent, or wait until it recovers."
            );
            if !task.notifications.iter().any(|n| n.kind == "offer_stuck") {
                return d
                    .store
                    .update_task_in(task_id, now, |t| t.notify_once("offer_stuck", &note))
                    .map(Some)
                    .map_err(|error| error.to_string());
            }
        }
        return Ok(Some(task));
    }
    d.store
        .send_direct_message(
            ("agent", mission_control::SESSION_ID),
            ("agent", &agent),
            &task_offer_body(&task),
            &uuid::Uuid::new_v4().to_string(),
            now,
        )
        .map_err(|error| error.to_string())?;
    d.store
        .update_task_in(task_id, now, |t| {
            t.notifications.push(crate::coordination::NotificationMarker {
                target: marker.clone(),
                kind: "offered".into(),
                message: now.to_string(),
                delivered: true,
            })
        })
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The model error an agent's inbox ended on after `since`, if its last turn failed.
fn inbox_failure_since(agent: &str, since: &str) -> Option<String> {
    let path = crate::session::state_path_for_id(&crate::session::inbox_session_id(agent))?;
    let state = crate::session::read_session_state(&path)?;
    if state.updated_at.as_str() < since {
        return None;
    }
    match state.events.last()? {
        crate::harness::HarnessEvent::ModelError { message } => Some(message.clone()),
        _ => None,
    }
}

fn task_offer_body(task: &Task) -> String {
    let owned = if task.owned_paths.is_empty() {
        "none".to_string()
    } else {
        task.owned_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    };
    format!(
        "[task_offer]\ntask_id: {}\ntitle: {}\ntarget_session: {}\nhandoff: {:?}\nowned_paths: {owned}\nbriefing:\n{}\n[/task_offer]\nMission Control is offering you this task. It starts only when you claim it: review it, then claim it with claim_and_dispatch_task (task_id above), ask Mission Control first with send_agent_message if something is unclear, or decline it with decline_task and your reason.",
        task.id,
        task.title,
        task.session_id,
        task.handoff_mode,
        task.description.trim(),
    )
}

fn wait_for_worker(d: &Daemon, task_id: &str, now: &str) -> Result<Option<Task>, String> {
    let roster = d.store.list_task_agents(task_id).map_err(|error| error.to_string())?;
    let Some(worker) = roster
        .iter()
        .find(|m| m.status == "active" && m.removed_at.is_none())
    else {
        return Ok(None);
    };
    if d.store.agent_takes_work(&worker.agent_id).map_err(|error| error.to_string())? {
        return Ok(None);
    }
    let reason = format!(
        "agent {} is not taking work; activate it or move the lease",
        worker.agent_id
    );
    let Some(task) = d.store.get_task(task_id).map_err(|error| error.to_string())? else {
        return Err(format!("unknown task {task_id}"));
    };
    if task.status != TaskStatus::Todo {
        return Ok(None);
    }
    if task.notifications.iter().any(|n| n.message == reason) {
        return Ok(Some(task));
    }
    d.store
        .update_task_in(task_id, now, |task| task.notify_once("blocked", &reason))
        .map(Some)
        .map_err(|error| error.to_string())
}

fn release_failed_claim(d: &Daemon, task: &Task, error: &str) -> Result<(), String> {
    const MAX_DISPATCH_FAILURES: u32 = 5;
    let now = chrono::Utc::now().to_rfc3339();
    d.store
        .update_task_in(&task.id, &now, |t| {
            t.dispatch_failures += 1;
            t.reporting_session = None;
            if t.dispatch_failures >= MAX_DISPATCH_FAILURES {
                t.status = TaskStatus::Blocked;
                t.notifications.push(NotificationMarker {
                    target: "mission_control".to_string(),
                    kind: "blocked".to_string(),
                    message: format!("dispatch failed {} times: {error}", t.dispatch_failures),
                    delivered: false,
                });
            } else {
                t.status = TaskStatus::Todo;
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn create_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionTaskReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.title.trim().is_empty() || req.session_id.trim().is_empty() {
        return mission_error("title and session_id are required".to_string());
    }
    let root = &d.mission_control_root;
    let managed = match mission_control::get_session(root, &req.session_id) {
        Ok(session) => session,
        Err(error) => return mission_error(error),
    };
    let owned_paths = match validated_owned_paths(&req.owned_paths, &managed.workspace) {
        Ok(paths) => paths,
        Err(error) => return mission_error(error),
    };
    let handoff_mode = match req.handoff_mode.as_deref() {
        None => HandoffMode::Resume,
        Some(raw) => match HandoffMode::parse(raw) {
            Some(mode) => mode,
            None => {
                return mission_error(format!(
                    "handoff_mode must be 'resume' or 'fresh', got '{raw}'"
                ));
            }
        },
    };
    let id = uuid::Uuid::new_v4().to_string();
    let task = Task::dispatched_to(
        id,
        req.session_id.clone(),
        req.title.trim().to_string(),
        req.description.trim().to_string(),
        owned_paths,
        handoff_mode,
        "mission-control",
        "mission-control",
        chrono::Utc::now().to_rfc3339(),
    );
    if let Err(error) = d.store.create_task(&task) {
        return mission_error(error.to_string());
    }
    let task = if req.status.as_deref() == Some("pending") {
        task
    } else {
        // A failed first attempt is already counted and re-queued; the loop
        // retries it like any other, up to the failure ceiling.
        if let Err(error) = dispatch_mission_task(&d, &task.id).await {
            eprintln!("[mission-control] first dispatch of {} failed: {error}", task.id);
        }
        match d.store.get_task(&task.id) {
            Ok(Some(task)) => task,
            Ok(None) => return mission_error("task vanished".to_string()),
            Err(error) => return mission_error(error.to_string()),
        }
    };
    Json(mission_task_view(&task)).into_response()
}

async fn update_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<MissionTaskReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    let existing = match d.store.get_task(&id) {
        Ok(Some(task)) => task,
        Ok(None) => return mission_error("unknown task".to_string()),
        Err(error) => return mission_error(error.to_string()),
    };
    let effective_session_id = if req.session_id.trim().is_empty() {
        existing.session_id.clone()
    } else {
        req.session_id.trim().to_string()
    };
    let workspace_for_paths: PathBuf =
        match mission_control::get_session(root, &effective_session_id) {
            Ok(session) => session.workspace,
            Err(error) => return mission_error(error),
        };
    let validated_paths = if req.owned_paths.is_empty() {
        None
    } else {
        Some(
            match validated_owned_paths(&req.owned_paths, &workspace_for_paths) {
                Ok(paths) => paths,
                Err(error) => return mission_error(error),
            },
        )
    };
    let status = match req.status.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match super::coordination::parse_task_status(raw) {
            Ok(status) => Some(status),
            Err(error) => return mission_error(error),
        },
        None => None,
    };
    let handoff_mode = match req.handoff_mode.as_deref() {
        None => None,
        Some(mode_raw) => match HandoffMode::parse(mode_raw) {
            Some(mode) => Some(mode),
            None => {
                return mission_error(format!(
                    "handoff_mode must be 'resume' or 'fresh', got '{mode_raw}'"
                ));
            }
        },
    };
    let updated = d
        .store
        .update_task_in(&id, &chrono::Utc::now().to_rfc3339(), |task| {
            if !req.title.trim().is_empty() {
                task.title = req.title.trim().to_string();
            }
            if !req.description.trim().is_empty() {
                task.description = req.description.trim().to_string();
            }
            if !req.session_id.trim().is_empty() && req.session_id.trim() != existing.session_id {
                task.session_id = req.session_id.trim().to_string();
                task.reporting_session = None;
            }
            if let Some(paths) = &validated_paths {
                task.owned_paths = paths.clone();
            }
            if let Some(mode) = handoff_mode {
                task.handoff_mode = mode;
            }
        });
    let updated = match (updated, status) {
        (Ok(_), Some(status)) => {
            let note = format!("Marked {status} by Mission Control.");
            match d.store.move_task(&id, status, &note, &chrono::Utc::now().to_rfc3339()) {
                Ok(task) => Ok(task),
                Err(error) => return super::coordination::store_error(error),
            }
        }
        (updated, _) => updated,
    };
    match updated {
        Ok(task) if task.status == TaskStatus::Todo => match dispatch_mission_task(&d, &id).await {
            Ok(task) => Json(mission_task_view(&task)).into_response(),
            Err(error) => mission_error(error),
        },
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

async fn archive_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match d.store.complete_task(
        &id,
        TaskStatus::Cancelled,
        TaskResult {
            summary: "Archived by Mission Control.".to_string(),
            ..Default::default()
        },
        &chrono::Utc::now().to_rfc3339(),
    ) {
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

async fn sessions(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    let tasks = d.store.list_tasks(None, None).unwrap_or_default();
    match mission_control::list_sessions(root, false) {
        Ok(sessions) => Json(
            sessions
                .iter()
                .filter(|session| {
                    q.archived.is_none_or(|archived| {
                        archived
                            == matches!(session.status, mission_control::SessionStatus::Archived)
                    })
                })
                .map(|session| {
                    mission_session_view(
                        session,
                        tasks
                            .iter()
                            .filter(|task| task.session_id == session.id)
                            .count(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => mission_error(error),
    }
}

async fn create_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionSessionReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let folder = PathBuf::from(&req.folder);
    if !folder.is_dir() {
        return mission_error("folder is not a directory".to_string());
    }
    let session_id = if req.session_id.trim().is_empty() {
        let base_state = {
            d.config
                .lock()
                .unwrap()
                .for_workspace(folder.clone())
                .state_path
        };
        let path = base_state
            .parent()
            .map(|parent| {
                parent
                    .join("conversations")
                    .join(format!("{}.json", uuid::Uuid::new_v4()))
            })
            .unwrap_or(base_state);
        let id = path
            .strip_prefix(workspaces_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        let cfg = {
            let config = d.config.lock().unwrap();
            let mut workspace = config.for_workspace(folder.clone());
            apply_profile(&mut workspace, &req.profile);
            workspace
        };
        let handle = start_session_with_browser_summary(
            &cfg,
            path,
            None,
            false,
            Some(Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(d.browser.summary_provider()),
        );
        let mut live = d.sessions.lock().await;
        live.insert(id.clone(), live_from_handle(handle, req.profile.clone()));
        id
    } else {
        req.session_id.trim().to_string()
    };
    let label = if req.title.trim().is_empty() {
        "Managed session"
    } else {
        req.title.trim()
    };
    let root = &d.mission_control_root;
    let session = match mission_control::create_session(root, &session_id, label, &folder) {
        Ok(session) => session,
        Err(error) => return mission_error(error),
    };
    Json(mission_session_view(&session, 0)).into_response()
}

async fn update_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<MissionSessionUpdate>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::update_session(&d.mission_control_root, &id, |session| {
        if let Some(title) = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
        {
            session.label = title.to_string();
        }
    }) {
        Ok(session) => Json(mission_session_view(&session, 0)).into_response(),
        Err(error) => mission_error(error),
    }
}

async fn archive_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::archive_session(&d.mission_control_root, &id) {
        Ok(session) => Json(mission_session_view(&session, 0)).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct MissionOpenReq {
    #[serde(default)]
    profile: Option<String>,
}

async fn open(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionOpenReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match open_mission_control(&d, req.profile).await {
        Ok((id, home)) => Json(serde_json::json!({ "id": id, "folder": home })).into_response(),
        Err(error) => mission_error(error),
    }
}

pub(super) async fn open_mission_control(d: &Shared, requested_profile: Option<String>) -> Result<(String, std::path::PathBuf), String> {
    let home = mission_control::ensure_home()?;
    let state_path = mission_control::session_state_path();
    let id = mission_control::SESSION_ID.to_string();
    // The conversation lives in the store; its path is only a key.
    let resume = d.store.has_conversation(&id).unwrap_or(false);
    let profile = requested_profile.or_else(|| read_session_profile(&state_path));
    write_session_sidecar(
        &state_path,
        &SessionSidecar {
            role: SessionRole::MissionControl,
            agent_id: None,
        },
    );
    let cfg = {
        let config = d.config.lock().unwrap();
        let mut workspace = config.for_workspace(home.clone());
        apply_profile(&mut workspace, &profile);
        workspace
    };
    let mut sessions = d.sessions.lock().await;
    let alive = sessions.get(&id).is_some_and(|s| !s.join.is_finished());
    if !alive {
        let handle = start_mission_control_session(
            &cfg,
            state_path,
            None,
            resume,
            Some(Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(d.browser.summary_provider()),
        );
        sessions.insert(id.clone(), live_from_handle(handle, profile));
        if !resume {
            if let Some(live) = sessions.get(&id) {
                let _ = live
                    .input_tx
                    .send(LoopInput::SetTitle("Mission Control".into()));
            }
        }
    }
    drop(sessions);
    if let Err(error) = mission_control::set_mission_control_session(&d.mission_control_root, &id) {
        eprintln!("[mission-control] failed to persist active MC session: {error}");
    }
    Ok((id, home))
}

pub fn register_builtin_agents(d: &Shared) {
    let coordinator = crate::coordination::types::Agent {
        id: crate::mission_control::SESSION_ID.to_string(),
        display_name: "Mission Control".into(),
        handle: "mission-control".into(),
        kind: crate::coordination::types::AgentKind::MissionControl,
        status: crate::coordination::types::AgentStatus::Active,
        role: crate::coordination::types::AgentRole::Coordinator,
        capabilities: vec![
            "orchestration".into(),
            "agent-directory".into(),
            "task-dispatch".into(),
        ],
    };
    let default_worker = crate::coordination::types::Agent {
        id: crate::coordination::SNIPPET_AGENT_ID.to_string(),
        display_name: "Snippet".into(),
        handle: "snippet".into(),
        kind: crate::coordination::types::AgentKind::Worker,
        status: crate::coordination::types::AgentStatus::Active,
        role: crate::coordination::types::AgentRole::Implementer,
        capabilities: vec![
            "coding".into(),
            "bash".into(),
            "files".into(),
            "web-search".into(),
        ],
    };
    for agent in [&coordinator, &default_worker] {
        if agent.id == crate::coordination::SNIPPET_AGENT_ID {
            let home = match crate::coordination::AgentHome::new(
                crate::coordination::agents_root(&d.mission_control_root),
                &agent.id,
            ) {
                Ok(home) => home,
                Err(error) => {
                    eprintln!(
                        "[coordination] invalid built-in agent id `{}`: {error}",
                        agent.id
                    );
                    continue;
                }
            };
            let identity = "# Snippet\n\nYou are Snippet, the general coding agent. You own work \
                            end to end: read the relevant code before you change it, make the \
                            smallest change that achieves the goal, and verify it with the \
                            narrowest check that proves it.\n\nThis identity is the default. \
                            Mission Control dispatches work here unless a user names a more \
                            specialized agent.\n";
            if let Err(error) = home.ensure_layout(identity) {
                eprintln!(
                    "[coordination] could not create the home for agent `{}`: {error}",
                    agent.id
                );
                continue;
            }
        }
        if let Err(error) = d.store.upsert_agent(agent) {
            eprintln!(
                "[coordination] could not register agent `{}`: {error}",
                agent.id
            );
        }
    }
}

pub async fn dispatch_loop(daemon: Shared) {
    loop {
        let now = chrono::Utc::now().to_rfc3339();
        let _ = daemon.store.unblock_ready_tasks(&now);
        let tasks = daemon
            .store
            .list_tasks(None, Some(&TaskStatus::Todo))
            .unwrap_or_default();
        for task in tasks {
            if let Err(error) = dispatch_mission_task(&daemon, &task.id).await {
                tracing_log_dispatch_failure(&task.id, &error);
            }
        }
        deliver_mission_control_reports(&daemon).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn deliver_mission_control_reports(daemon: &Daemon) {
    if mission_control::awaiting_user() {
        return;
    }
    let Ok(tasks) = daemon.store.list_tasks(None, None) else {
        return;
    };
    for task in tasks {
        for marker in &task.notifications {
            if marker.delivered || marker.target != "mission_control" {
                continue;
            }
            let text = format!(
                "[mission_task_report]\ntask_id: {}\ntitle: {}\nstatus: {}\nsummary: {}\n[/mission_task_report]",
                task.id, task.title, marker.kind, marker.message
            );
            daemon
                .deliver(mission_control::SESSION_ID, LoopInput::UserMessage(text))
                .await;
            let _ = daemon
                .store
                .update_task_in(&task.id, &chrono::Utc::now().to_rfc3339(), |t| {
                    // Matched by content, not position: the list can change
                    // between this read and the write.
                    if let Some(n) = t.notifications.iter_mut().find(|n| !n.delivered && **n == *marker) {
                        n.delivered = true;
                    }
                });
        }
    }
}

fn tracing_log_dispatch_failure(task_id: &str, error: &str) {
    eprintln!("[mission-control] dispatch {task_id} failed: {error}");
}

fn autonomy_view() -> serde_json::Value {
    let state = crate::mission_autonomy::load();
    serde_json::json!({
        "on": state.settings.on,
        "round_minutes": state.settings.round_minutes,
        "quiet_start": state.settings.quiet_start,
        "quiet_end": state.settings.quiet_end,
        "utc_offset_minutes": state.settings.utc_offset_minutes,
        "in_quiet_hours": crate::mission_autonomy::in_quiet_hours(&state.settings),
        "last_round_at": (state.last_round_at > 0).then_some(state.last_round_at),
        "last_round_summary": state.last_round_summary,
        "next_round_at": crate::mission_autonomy::next_round_at(&state),
        "followups": state.followups,
        "held_pings": state.held_pings.len(),
        "pings_sent": state.pings_sent,
        "has_brief": !crate::mission_autonomy::read_brief().trim().is_empty(),
    })
}

#[derive(Deserialize)]
struct AutonomyReq {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    round_minutes: Option<u32>,
    #[serde(default)]
    quiet_start: Option<String>,
    #[serde(default)]
    quiet_end: Option<String>,
    #[serde(default)]
    utc_offset_minutes: Option<i32>,
}

async fn autonomy(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(autonomy_view()).into_response()
}

async fn update_autonomy(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<AutonomyReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    for clock in [&req.quiet_start, &req.quiet_end].into_iter().flatten() {
        if !clock.is_empty() && !crate::mission_autonomy::valid_clock(clock) {
            return (StatusCode::BAD_REQUEST, format!("quiet hours must be HH:MM, got `{clock}`")).into_response();
        }
    }
    let result = crate::mission_autonomy::update(|state| {
        if let Some(on) = req.on {
            if on && !state.settings.on {
                state.last_round_at = 0;
                state.last_digest.clear();
            }
            state.settings.on = on;
        }
        if let Some(minutes) = req.round_minutes {
            state.settings.round_minutes = minutes.clamp(crate::mission_autonomy::MIN_ROUND_MINUTES, 24 * 60);
        }
        if let Some(start) = req.quiet_start {
            state.settings.quiet_start = Some(start).filter(|s| !s.is_empty());
        }
        if let Some(offset) = req.utc_offset_minutes.filter(|m| m.abs() <= 14 * 60) {
            state.settings.utc_offset_minutes = Some(offset);
        }
        if let Some(end) = req.quiet_end {
            state.settings.quiet_end = Some(end).filter(|s| !s.is_empty());
        }
    });
    match result {
        Ok(()) => Json(autonomy_view()).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct MissionSettingsReq {
    notification_policy: String,
}

async fn settings(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(mission_control::load_settings(&d.mission_control_root)).into_response()
}

async fn update_settings(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionSettingsReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::set_notification_policy(
        &d.mission_control_root,
        &req.notification_policy,
    ) {
        Ok(settings) => Json(settings).into_response(),
        Err(error) => mission_error(error),
    }
}
