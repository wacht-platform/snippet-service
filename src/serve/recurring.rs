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

fn session_busy_for_recurring(id: &str) -> bool {
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
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let due = recurring::due_jobs(&daemon.recurring_root, now).unwrap_or_default();
        for job in due {
            if session_busy_for_recurring(&job.session_id) {
                let _ = recurring::mark_queued(&daemon.recurring_root, &job.id);
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
                    tokio::spawn(report_run_to_mission_control(
                        daemon.clone(),
                        job.title.clone(),
                        job.session_id.clone(),
                        seen,
                    ));
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

/// Wait for a scheduled run to finish and send its outcome to Mission Control,
/// so it learns how the run went from a report rather than by reading the
/// session. Only while automated wakes are on: otherwise nobody acts on it.
async fn report_run_to_mission_control(daemon: Shared, title: String, session_id: String, seen: usize) {
    let Some(sp) = state_path_for_id(&session_id) else { return };
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
        if !crate::mission_autonomy::is_on() {
            return;
        }
        let label = if title.trim().is_empty() { "untitled job" } else { title.trim() };
        let text = format!(
            "[recurring_run_report]\njob: {label}\nsession_id: {session_id}\nstatus: {status}\nsummary: {}\n[/recurring_run_report]\nWorkflow W6 · recurring run report: pass on what's worth the user's attention, note it in your brief if it changes anything, then stop. Don't read the session's files or logs; this report is the outcome.",
            summary.trim()
        );
        if super::mission_control::open_mission_control(&daemon, None).await.is_ok() {
            daemon
                .deliver(crate::mission_control::SESSION_ID, LoopInput::UserMessage(text))
                .await;
        }
        return;
    }
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
