use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Json, Path as AxumPath, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::Router;
use serde::Deserialize;

use crate::harness::{GoalStatus, LoopInput};
use crate::recurring::{self, Schedule};
use crate::session::{read_session_meta, state_path_for_id, status_str};
use super::{mission_error, unauthorized, Auth, Shared};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/recurring", get(list_recurring).post(create_recurring))
        .route(
            "/recurring/{id}",
            put(update_recurring).delete(delete_recurring),
        )
}

pub(super) fn session_busy_for_recurring(id: &str) -> bool {
    let Some(sp) = state_path_for_id(id) else {
        return false;
    };
    let Some(state) = read_session_meta(&sp) else {
        return false;
    };
    recurring::session_is_busy(&status_str(state.status))
        || state
            .lanes
            .iter()
            .any(|l| matches!(l.status, crate::lanes::LaneStatus::Running))
        || state.goal.as_ref().is_some_and(|g| match g.status {
            GoalStatus::Active => true,
            GoalStatus::Paused => !g.text.starts_with("**Scheduled run"),
            _ => false,
        })
}

pub async fn tick_loop(daemon: Shared) {
    if let Ok(store) = recurring::store_at(&daemon.recurring_root) {
        let _ = store.interrupt_running_recurring_runs(chrono::Utc::now().timestamp());
    }
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let due = recurring::due_jobs(&daemon.recurring_root, now).unwrap_or_default();
        for job in due {
            if let Some(wake) = &job.wake
                && !wake_may_fire(&daemon, &job, wake, now)
            {
                continue;
            }
            if session_busy_for_recurring(&job.session_id) {
                let _ = recurring::mark_queued(&daemon.recurring_root, &job.id);
                continue;
            }
            if let Some(wake) = &job.wake
                && let Err(error) = open_owner(&daemon, wake).await
            {
                let _ = recurring::mark_error(&daemon.recurring_root, &job.id, &error);
                continue;
            }
            let workspace = state_path_for_id(&job.session_id)
                .and_then(|sp| read_session_meta(&sp))
                .map(|s| PathBuf::from(s.workspace))
                .filter(|p| p.is_dir());
            match job.render_goal(workspace.as_deref()) {
                Ok(text) => {
                    let seen = state_path_for_id(&job.session_id)
                        .and_then(|sp| crate::session::read_session_state(&sp))
                        .map(|s| s.events.len())
                        .unwrap_or(0);
                    daemon
                        .deliver(&job.session_id, LoopInput::SetGoal(text))
                        .await;
                    let _ = recurring::mark_fired(&daemon.recurring_root, &job.id, now);
                    let run_id = recurring::store_at(&daemon.recurring_root)
                        .and_then(|s| {
                            s.insert_recurring_run(
                                &job.id,
                                job.wake.as_ref().map(|w| w.owner.as_str()),
                                &job.session_id,
                                now as i64,
                            )
                            .map_err(|e| e.to_string())
                        })
                        .ok();
                    tokio::spawn(watch_run(daemon.clone(), job.clone(), run_id, seen));
                }
                Err(error) => {
                    let _ = recurring::mark_error(&daemon.recurring_root, &job.id, &error);
                }
            }
        }
        let wait = if recurring::has_queued(&daemon.recurring_root) {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(15)
        };
        tokio::time::sleep(wait).await;
    }
}

/// Quiet hours and the daily cap; a deferred wake moves its next run.
fn wake_may_fire(daemon: &Shared, job: &recurring::RecurringJob, wake: &crate::agent_wakes::WakePolicy, now: u64) -> bool {
    let store = recurring::store_at(&daemon.recurring_root).ok();
    let day_ago = now as i64 - 86_400;
    let runs = store
        .as_ref()
        .and_then(|s| s.count_recurring_runs_since(&job.id, day_ago).ok())
        .unwrap_or(0);
    let oldest = store
        .as_ref()
        .and_then(|s| s.list_recurring_runs(Some(&job.id), None, wake.max_runs_per_day).ok())
        .and_then(|r| r.into_iter().filter(|r| r.fired_at >= day_ago).map(|r| r.fired_at as u64).min());
    let offset = crate::agent_wakes::policy_offset(wake);
    if let crate::agent_wakes::Gate::Defer { until, reason } = crate::agent_wakes::gate(wake, offset, now, runs, oldest) {
        eprintln!("[wakes] {} deferred until {until}: {reason}", wake.owner);
        let _ = recurring::update_job(&daemon.recurring_root, &job.id, |j| {
            j.next_run_at = until;
            j.queued = false;
        });
        return false;
    }
    true
}

async fn open_owner(daemon: &Shared, wake: &crate::agent_wakes::WakePolicy) -> Result<(), String> {
    match wake.owner_kind {
        crate::agent_wakes::OwnerKind::Agent => super::direct::ensure_agent_inbox(daemon, &wake.owner).await,
        crate::agent_wakes::OwnerKind::MissionControl => {
            super::mission_control::open_mission_control(daemon, None).await.map(|_| ())
        }
        crate::agent_wakes::OwnerKind::Session => Ok(()),
    }
}

/// Wait for a scheduled run to finish, record it, and report it: by the wake
/// policy for a wake schedule, else to Mission Control while automated wakes are on.
async fn watch_run(daemon: Shared, job: recurring::RecurringJob, run_id: Option<i64>, seen: usize) {
    let finish = |status: &str, summary: Option<&str>, reported_to: Option<&str>| {
        if let (Some(id), Ok(store)) = (run_id, recurring::store_at(&daemon.recurring_root)) {
            let _ = store.finish_recurring_run(id, status, summary, reported_to, chrono::Utc::now().timestamp());
        }
    };
    let Some(sp) = state_path_for_id(&job.session_id) else {
        finish("unknown", None, None);
        return;
    };
    for _ in 0..(4 * 60 * 3) {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Some(state) = crate::session::read_session_state(&sp) else { continue };
        let outcome = state.events.iter().skip(seen).find_map(|event| match event {
            crate::harness::HarnessEvent::SystemDecision { step, reasoning }
                if matches!(step.as_str(), "goal_completed" | "goal_cancelled" | "goal_paused") =>
            {
                Some((step.trim_start_matches("goal_").to_string(), reasoning.clone()))
            }
            _ => None,
        });
        let Some((status, summary)) = outcome else { continue };
        let summary = summary.trim().to_string();
        let previous = job.last_result.clone();
        let _ = recurring::update_job(&daemon.recurring_root, &job.id, |j| {
            j.last_result = Some(if status == "completed" { summary.clone() } else { format!("{status}: {summary}") });
        });
        let reported = match &job.wake {
            Some(wake) => report_wake(&daemon, &job, wake, &status, &summary, previous.as_deref()).await,
            None => report_run_to_mission_control(&daemon, &job.title, &job.session_id, &status, &summary)
                .await
                .to_string(),
        };
        finish(&status, Some(&summary), Some(&reported));
        return;
    }
    finish("timed_out", None, None);
}

async fn report_wake(
    daemon: &Shared,
    job: &recurring::RecurringJob,
    wake: &crate::agent_wakes::WakePolicy,
    status: &str,
    summary: &str,
    previous: Option<&str>,
) -> String {
    use crate::agent_wakes::ReportTo;
    let Some(to) = crate::agent_wakes::route(&wake.report, status, summary, previous) else {
        return if wake.report.to == ReportTo::None { "none".into() } else { "suppressed: no change".into() };
    };
    let label = if job.title.trim().is_empty() { wake.owner.as_str() } else { job.title.trim() };
    match to {
        ReportTo::None => "none".into(),
        ReportTo::User => match crate::mission_autonomy::ping(label, summary, "update") {
            Ok(sent) => format!("user: {sent}"),
            Err(error) => format!("user: failed: {error}"),
        },
        ReportTo::MissionControl => {
            let text = format!(
                "[agent_wake_report]\nowner: {}\nschedule: {}\nsession_id: {}\nstatus: {status}\nsummary: {summary}\n[/agent_wake_report]\nA scheduled wake finished. Pass on what's worth the user's attention or act on it; otherwise no reply.",
                wake.owner,
                job.schedule.display(),
                job.session_id,
            );
            if super::mission_control::open_mission_control(daemon, None).await.is_ok() {
                daemon.deliver(crate::mission_control::SESSION_ID, LoopInput::UserMessage(text)).await;
                "mission_control".into()
            } else {
                "mission_control: unavailable".into()
            }
        }
        ReportTo::TaskRoom => {
            let task_id = wake.report.task_id.clone().unwrap_or_default();
            match post_to_task_room(daemon, wake, &task_id, &format!("Scheduled wake ({status}): {summary}")) {
                Ok(()) => format!("task_room: {task_id}"),
                Err(error) => format!("task_room: failed: {error}"),
            }
        }
    }
}

fn post_to_task_room(daemon: &Shared, wake: &crate::agent_wakes::WakePolicy, task_id: &str, body: &str) -> Result<(), String> {
    let task = daemon
        .store
        .get_task(task_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("task `{task_id}` not found"))?;
    let actor_kind = match wake.owner_kind {
        crate::agent_wakes::OwnerKind::Agent => "agent",
        crate::agent_wakes::OwnerKind::Session => "session",
        crate::agent_wakes::OwnerKind::MissionControl => "system",
    };
    let event = crate::coordination::types::CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        thread_id: task.thread_id.clone(),
        partition_key: format!("thread:{}", task.thread_id),
        sequence: 0,
        event_type: "message.posted".into(),
        actor_kind: actor_kind.into(),
        actor_id: wake.owner.clone(),
        payload_version: 1,
        payload: serde_json::json!({"body": body, "task_id": task_id, "origin_session": crate::agent_wakes::session_for(&wake.owner, wake.owner_kind)}),
        causation_id: None,
        correlation_id: Some(task_id.to_string()),
        idempotency_key: uuid::Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let saved = daemon.store.append_event(&event).map_err(|e| e.to_string())?;
    crate::session::emit_device_event(serde_json::json!({"kind": "coordination_event", "event": saved.clone()}));
    super::queue_coordination_wake(saved);
    Ok(())
}

/// A plain recurring run's outcome for Mission Control. Completed runs are
/// noted for the next wake; others open a turn. Only while automated wakes are on.
async fn report_run_to_mission_control(daemon: &Shared, title: &str, session_id: &str, status: &str, summary: &str) -> &'static str {
    if !crate::mission_autonomy::is_on() {
        return "none: automated wakes off";
    }
    let label = if title.trim().is_empty() { "untitled job" } else { title.trim() };
    if status == "completed" {
        let line = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        let line: String = line.chars().take(240).collect();
        let note = format!("- {label} · {line}");
        let _ = crate::mission_autonomy::update(|s| {
            s.run_notes.push(note);
            let excess = s.run_notes.len().saturating_sub(20);
            s.run_notes.drain(..excess);
        });
        return "mission_control: next wake";
    }
    let text = format!(
        "[recurring_run_report]\njob: {label}\nsession_id: {session_id}\nstatus: {status}\nsummary: {}\n[/recurring_run_report]\nWorkflow W6 · recurring run report: pass on what's worth the user's attention, note it in your brief if it changes anything, then stop. Don't read the session's files or logs; this report is the outcome.",
        summary.trim()
    );
    if super::mission_control::open_mission_control(daemon, None).await.is_err() {
        return "mission_control: unavailable";
    }
    daemon
        .deliver(crate::mission_control::SESSION_ID, LoopInput::UserMessage(text))
        .await;
    "mission_control"
}

async fn list_recurring(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match recurring::list_jobs(&d.recurring_root) {
        Ok(jobs) => Json(jobs).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct RecurringCreateReq {
    #[serde(default)]
    title: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    schedule: String,
    #[serde(default)]
    delivery: Option<String>,
}

#[derive(Deserialize)]
struct RecurringUpdateReq {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn create_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RecurringCreateReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let schedule = match Schedule::parse(&req.schedule) {
        Ok(s) => s,
        Err(error) => return mission_error(error),
    };
    let delivery = match req.delivery.as_deref() {
        None | Some("") | Some("goal") => recurring::Delivery::Goal,
        Some("message") => recurring::Delivery::Message,
        Some(other) => {
            return mission_error(format!("delivery must be goal or message, got `{other}`"));
        }
    };
    match recurring::create_job_with(
        &d.recurring_root,
        &req.title,
        &req.session_id,
        &req.prompt,
        schedule,
        req.plan_path.as_deref(),
        delivery,
    ) {
        Ok(job) => {
            Json(job).into_response()
        }
        Err(error) => mission_error(error),
    }
}

async fn update_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<RecurringUpdateReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let schedule = match req.schedule.as_deref() {
        Some(raw) if !raw.trim().is_empty() => match Schedule::parse(raw) {
            Ok(s) => Some(s),
            Err(error) => return mission_error(error),
        },
        _ => None,
    };
    match recurring::update_job(&d.recurring_root, &id, |job| {
        if let Some(title) = req.title.as_deref() {
            let title = title.trim();
            if !title.is_empty() {
                job.title = title.to_string();
            }
        }
        if let Some(session_id) = req.session_id.as_deref() {
            let session_id = session_id.trim();
            if !session_id.is_empty() {
                job.session_id = session_id.to_string();
            }
        }
        if let Some(prompt) = req.prompt.as_deref() {
            job.prompt = prompt.trim().to_string();
        }
        if let Some(plan_path) = req.plan_path.as_deref() {
            let trimmed = plan_path.trim();
            job.plan_path = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Some(schedule) = schedule.clone() {
            job.schedule = schedule;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            job.next_run_at = job.schedule.next_after(now);
            job.queued = false;
        }
        if let Some(enabled) = req.enabled {
            job.enabled = enabled;
            if !enabled {
                job.queued = false;
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if job.next_run_at < now {
                    job.next_run_at = job.schedule.next_after(now);
                }
            }
        }
    }) {
        Ok(job) => {
            Json(job).into_response()
        }
        Err(error) => mission_error(error),
    }
}

async fn delete_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match recurring::delete_job(&d.recurring_root, &id) {
        Ok(()) => {
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(error) => mission_error(error),
    }
}
