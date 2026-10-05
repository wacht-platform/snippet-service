use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde_json::Value;

use super::Shared;
use crate::coordination::TaskStatus;
use crate::harness::{HarnessStatus, LoopInput};
use crate::mission_control::SESSION_ID;
use crate::mission_autonomy;

const TICK: Duration = Duration::from_secs(5);
const STALL_SECS: i64 = 20 * 60;

pub async fn autonomy_loop(daemon: Shared) {
    loop {
        tokio::time::sleep(TICK).await;
        for queued in mission_autonomy::take_answers() {
            daemon.deliver(&queued.session_id, LoopInput::Answer(queued.answer)).await;
        }
        let state = mission_autonomy::load();
        if !state.settings.on {
            continue;
        }
        mission_autonomy::release_held_pings();
        wake_for_worker_questions(&daemon).await;
        if mission_control_busy() {
            continue;
        }
        let now = chrono::Utc::now().timestamp();
        let tasks = daemon.store.list_tasks(None, None).unwrap_or_default();
        let digest = board_digest(&tasks);
        let due = mission_autonomy::take_due_followups(now);
        let interval_due = mission_autonomy::next_round_at(&state).is_some_and(|at| now >= at);
        let changed = digest != state.last_digest;
        if due.is_empty() && !(interval_due && (changed || state.last_round_at == 0)) {
            if interval_due {
                let _ = mission_autonomy::update(|s| s.last_round_at = now);
            }
            continue;
        }
        let envelope = round_envelope(&tasks, &state, &due, now);
        let summary = round_summary(&tasks, &state, &due);
        let _ = mission_autonomy::update(|s| {
            s.last_round_at = now;
            s.last_digest = digest;
            s.last_round_summary = Some(summary);
        });
        deliver_to_mission_control(&daemon, envelope).await;
    }
}

async fn deliver_to_mission_control(daemon: &Shared, text: String) {
    if let Err(error) = super::mission_control::open_mission_control(daemon, None).await {
        eprintln!("[autonomy] could not open Mission Control: {error}");
        return;
    }
    daemon.deliver(SESSION_ID, LoopInput::UserMessage(text)).await;
}

fn mission_control_busy() -> bool {
    crate::session::read_session_state(&crate::mission_control::session_state_path())
        .is_some_and(|s| matches!(s.status, HarnessStatus::Running | HarnessStatus::WaitingForInput))
}

fn board_digest(tasks: &[crate::coordination::Task]) -> String {
    let mut hasher = DefaultHasher::new();
    let mut rows: Vec<(&str, String, &str)> = tasks
        .iter()
        .map(|t| (t.id.as_str(), t.status.to_string(), t.updated_at.as_str()))
        .collect();
    rows.sort();
    rows.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn secs_since(rfc3339: &str, now: i64) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| now - t.timestamp())
        .unwrap_or(0)
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn session_status(id: &str) -> Option<crate::harness::HarnessState> {
    if id.trim().is_empty() {
        return None;
    }
    crate::session::state_path_for_id(id).and_then(|p| crate::session::read_session_state(&p))
}

fn question_text(question: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(context) = question.get("context").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
        lines.push(context.trim().to_string());
    }
    for q in question.get("questions").and_then(Value::as_array).into_iter().flatten() {
        let text = q.get("text").and_then(Value::as_str).unwrap_or("").trim();
        let choices: Vec<&str> = q
            .pointer("/answer_kind/choices")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(|c| c.get("label").and_then(Value::as_str)).collect())
            .unwrap_or_default();
        if choices.is_empty() {
            lines.push(format!("- {text}"));
        } else {
            lines.push(format!("- {text} (choices: {})", choices.join(" / ")));
        }
    }
    lines.join("\n")
}

async fn wake_for_worker_questions(daemon: &Shared) {
    let tasks = daemon.store.list_tasks(None, Some(&TaskStatus::InProgress)).unwrap_or_default();
    for task in tasks {
        let Some(state) = session_status(&task.session_id) else { continue };
        if state.status != HarnessStatus::WaitingForInput {
            continue;
        }
        let Some(question) = state.pending_question.as_ref() else { continue };
        let mut hasher = DefaultHasher::new();
        question.to_string().hash(&mut hasher);
        let key = format!("{}:{:x}", task.session_id, hasher.finish());
        if !mission_autonomy::remember_question(&key) {
            continue;
        }
        let text = format!(
            "[worker_question]\ntask_id: {}\ntask: {}\nsession_id: {}\nquestion:\n{}\n[/worker_question]\nThe worker is paused on this question. Answer it yourself with answer_worker (session_id {}) when the brief, the task or the conversation already settles it; otherwise ping the user with the question and what you'd recommend.",
            task.id,
            task.title,
            task.session_id,
            question_text(question),
            task.session_id,
        );
        deliver_to_mission_control(daemon, text).await;
    }
}

fn round_envelope(
    tasks: &[crate::coordination::Task],
    state: &mission_autonomy::AutonomyState,
    due: &[mission_autonomy::Followup],
    now: i64,
) -> String {
    let since = state.last_round_at;
    let mut out = String::from("[autonomous_round]\n");
    out.push_str(&format!(
        "time: {}\nlast round: {}\n",
        chrono::Local::now().format("%a %d %b %H:%M"),
        if since == 0 { "never (autonomous mode was just switched on)".to_string() } else { ago(now - since) }
    ));
    if mission_autonomy::in_quiet_hours(&state.settings) {
        out.push_str("quiet hours: yes — non-urgent pings are held until morning\n");
    }

    let changed: Vec<String> = tasks
        .iter()
        .filter(|t| since == 0 || secs_since(&t.updated_at, now) < now - since)
        .take(20)
        .map(|t| format!("- {} · {} · {}", t.status, t.title, t.id))
        .collect();
    out.push_str("\n## Changed since last round\n");
    out.push_str(if changed.is_empty() { "nothing" } else { "" });
    out.push_str(&changed.join("\n"));

    let open: Vec<String> = tasks
        .iter()
        .filter(|t| !t.status.is_terminal())
        .map(|t| {
            let idle = secs_since(&t.updated_at, now);
            let session = session_status(&t.session_id);
            let note = match session.as_ref().map(|s| s.status) {
                Some(HarnessStatus::Running) => "worker running".to_string(),
                Some(HarnessStatus::WaitingForInput) => "worker waiting for input".to_string(),
                _ if t.status == TaskStatus::InProgress && idle > STALL_SECS => {
                    format!("no worker activity, last update {} — may be stalled", ago(idle))
                }
                _ => format!("updated {}", ago(idle)),
            };
            format!("- {} · {} · {} · {}", t.status, t.title, t.id, note)
        })
        .collect();
    out.push_str("\n\n## Open work\n");
    out.push_str(if open.is_empty() { "none" } else { "" });
    out.push_str(&open.join("\n"));

    if !due.is_empty() {
        out.push_str("\n\n## Follow-ups due now\n");
        for f in due {
            out.push_str(&format!("- {}\n", f.note));
        }
    }
    let upcoming: Vec<&mission_autonomy::Followup> = state
        .followups
        .iter()
        .filter(|f| !due.iter().any(|d| d.id == f.id))
        .collect();
    if !upcoming.is_empty() {
        out.push_str("\n\n## Follow-ups scheduled\n");
        for f in upcoming.into_iter().take(10) {
            out.push_str(&format!("- in {} min: {}\n", ((f.due_at - now) / 60).max(0), f.note));
        }
    }

    let brief = mission_autonomy::read_brief();
    out.push_str("\n\n## Your brief\n");
    out.push_str(if brief.trim().is_empty() {
        "(empty — start one with update_brief: the user's goals, priorities, preferences and open threads)"
    } else {
        brief.trim()
    });
    out.push_str("\n[/autonomous_round]");
    out
}

fn round_summary(
    tasks: &[crate::coordination::Task],
    state: &mission_autonomy::AutonomyState,
    due: &[mission_autonomy::Followup],
) -> String {
    let open = tasks.iter().filter(|t| !t.status.is_terminal()).count();
    let mut parts = vec![format!("{open} open task{}", if open == 1 { "" } else { "s" })];
    if !due.is_empty() {
        parts.push(format!("{} follow-up{} due", due.len(), if due.len() == 1 { "" } else { "s" }));
    }
    if state.last_round_at == 0 {
        parts.insert(0, "autonomy switched on".to_string());
    }
    parts.join(" · ")
}
