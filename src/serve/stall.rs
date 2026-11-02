//! Heartbeat stall check: flags in-progress work that has gone quiet, whatever
//! its session is doing.

use chrono::{DateTime, Utc};

use crate::coordination::{
    TaskStatus, progress_stamp, stall_marker_for_current_spell, stalled_minutes,
};
use crate::store::Store;

/// What the session behind a task is doing: its status and, if paused, the question.
pub(super) struct SessionProbe {
    pub status: String,
    pub question: Option<String>,
    pub last_said: Option<String>,
}

pub(super) fn probe_session(session_id: &str) -> Option<SessionProbe> {
    if session_id.trim().is_empty() {
        return None;
    }
    let state = crate::session::state_path_for_id(session_id).and_then(|p| crate::session::read_session_meta(&p))?;
    let question = (state.status == crate::harness::HarnessStatus::WaitingForInput)
        .then(|| state.pending_question.as_ref().map(super::autonomy::question_text))
        .flatten();
    let last_said = (state.status == crate::harness::HarnessStatus::Idle)
        .then(|| last_assistant_text(session_id))
        .flatten();
    Some(SessionProbe { status: crate::session::status_str(state.status), question, last_said })
}

fn last_assistant_text(session_id: &str) -> Option<String> {
    let path = crate::session::state_path_for_id(session_id)?;
    let (events, _, _) = crate::session::read_session_event_page(&path, None, 12)?;
    events.into_iter().rev().find_map(|e| match e {
        crate::harness::HarnessEvent::AssistantText { text } if !text.trim().is_empty() => {
            let text = text.trim();
            Some(match text.char_indices().nth(600) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text.to_string(),
            })
        }
        _ => None,
    })
}

/// Raise one `stalled` notice per quiet spell for every in-progress task with no
/// progress past its threshold. Returns the ids flagged.
pub(super) fn sweep(store: &Store, now: DateTime<Utc>, probe: impl Fn(&str) -> Option<SessionProbe>) -> Vec<String> {
    let mut flagged = Vec::new();
    for task in store.list_tasks(None, Some(&TaskStatus::InProgress)).unwrap_or_default() {
        if task.last_progress_at.is_none() {
            let _ = store.touch_task_progress(&task.id, &task.updated_at);
            continue;
        }
        let session = probe(&task.session_id);
        let question = session.as_ref().and_then(|s| s.question.clone());
        let idle = session.as_ref().is_some_and(|s| s.status == "idle");
        let Some(age) = stalled_minutes(&task, now, question.is_some() || idle) else { continue };
        if stall_marker_for_current_spell(&task).is_some() {
            continue;
        }
        let stamp = progress_stamp(&task).to_string();
        let last_said = session.as_ref().and_then(|s| s.last_said.clone());
        let state = session.map(|s| s.status).unwrap_or_else(|| "missing".into());
        let detail = match (question, last_said) {
            (Some(q), _) => format!("the worker is waiting on an answer:\n{q}\nanswer_worker (session_id {}) if you can settle it, otherwise ask the user.", task.session_id),
            (None, Some(said)) => format!("its session went idle without reporting; its last message was:\n{said}\nIf that asks for something, answer_worker (session_id {}) if you can settle it, otherwise ask the user; if it just stopped, retry_mission_task.", task.session_id),
            (None, None) => format!("its session is {state}; check it with inspect_session, then retry_mission_task, answer it or cancel."),
        };
        let message = format!("stalled {age} min: no progress since {stamp}; {detail}");
        if store.update_task_in(&task.id, &now.to_rfc3339(), |t| t.notify_once("stalled", &message)).is_ok() {
            flagged.push(task.id);
        }
    }
    flagged
}

/// Tell Mission Control once about a handover nobody has acknowledged in time.
pub(super) fn escalate_handoffs(store: &Store, now: DateTime<Utc>) -> usize {
    let mut escalated = 0;
    for h in store.overdue_handoffs(now).unwrap_or_default() {
        let message = format!(
            "handover {} from `{}` to `{}` was not acknowledged within {} min; the sender still holds the lease. Wake `{}` or take the work back.",
            h.id, h.from_agent, h.to_agent, crate::coordination::handoffs::ESCALATE_AFTER_MINUTES, h.to_agent
        );
        let stamp = now.to_rfc3339();
        if store.update_task_in(&h.task_id, &stamp, |t| t.notify_once("handover_unacked", &message)).is_ok() {
            escalated += 1;
        }
        let _ = store.mark_handoff_escalated(&h.id, &stamp);
    }
    escalated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::Task;

    fn running_task(store: &Store, id: &str, progress_minutes_ago: i64, expected: Option<u32>) {
        let t0 = (Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        let mut task = Task::filed_by_human(id.into(), id.into(), "d".into(), format!("s-{id}"), 0, t0.clone());
        task.expected_minutes = expected;
        store.create_task(&task).unwrap();
        store.claim_task_for_dispatch(id, &t0).unwrap();
        let at = (Utc::now() - chrono::Duration::minutes(progress_minutes_ago)).to_rfc3339();
        store.touch_task_progress(id, &at).unwrap();
    }

    fn idle(_: &str) -> Option<SessionProbe> {
        Some(SessionProbe { status: "idle".into(), question: None, last_said: None })
    }

    fn running(_: &str) -> Option<SessionProbe> {
        Some(SessionProbe { status: "running".into(), question: None, last_said: None })
    }

    #[test]
    fn an_idle_session_escalates_after_ten_minutes_with_its_last_message() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "soon", 5, Some(90));
        running_task(&store, "parked", 12, Some(90));
        let said = |_: &str| Some(SessionProbe { status: "idle".into(), question: None, last_said: Some("please log in".into()) });
        assert_eq!(sweep(&store, Utc::now(), said), vec!["parked".to_string()]);
        let task = store.get_task("parked").unwrap().unwrap();
        assert!(task.notifications.iter().any(|n| n.kind == "stalled" && n.message.contains("please log in")));
    }

    #[test]
    fn flags_after_max_of_floor_and_twice_expected_once_per_spell() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "fresh", 5, None);
        running_task(&store, "quiet", 25, None);
        running_task(&store, "long", 25, Some(30));
        assert_eq!(sweep(&store, Utc::now(), running), vec!["quiet".to_string()]);
        assert!(sweep(&store, Utc::now(), running).is_empty(), "once per spell");
        let task = store.get_task("quiet").unwrap().unwrap();
        assert!(task.notifications.iter().any(|n| n.kind == "stalled" && n.message.contains("no progress since")));
        assert_eq!(store.board_sub_status(&task).status_reason.as_deref(), Some("stalled"));
        assert!(store.board_sub_status(&task).stalled_minutes.is_some_and(|m| m >= 25));

        store.touch_task_progress("quiet", &Utc::now().to_rfc3339()).unwrap();
        let task = store.get_task("quiet").unwrap().unwrap();
        assert_eq!(store.board_sub_status(&task).status_reason.as_deref(), Some("working"), "progress retires the notice");
    }

    #[test]
    fn a_waiting_worker_escalates_early_with_its_question() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "ask", 12, None);
        let waiting = |_: &str| Some(SessionProbe { status: "waiting_for_input".into(), question: Some("- which db?".into()), last_said: None });
        assert_eq!(sweep(&store, Utc::now(), waiting), vec!["ask".to_string()]);
        let task = store.get_task("ask").unwrap().unwrap();
        assert!(task.notifications.iter().any(|n| n.message.contains("which db?")));
    }

    #[test]
    fn a_task_queued_behind_a_finished_one_is_not_flagged_by_the_stop() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "first", 1, None);
        running_task(&store, "second", 1, None);
        store.complete_task("first", TaskStatus::Done, crate::coordination::TaskResult { summary: "ok".into(), ..Default::default() }, &Utc::now().to_rfc3339()).unwrap();
        assert!(sweep(&store, Utc::now(), idle).is_empty());
        assert!(store.get_task("second").unwrap().unwrap().notifications.iter().all(|n| n.kind != "stalled"));
    }

    #[test]
    fn an_unacknowledged_handover_escalates_once_and_shows_in_the_board_view() {
        use crate::coordination::handoffs::Handover;
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "t", 1, None);
        for id in ["a", "b"] {
            store
                .create_agent(&crate::coordination::types::Agent {
                    id: id.into(), display_name: id.into(), handle: id.into(),
                    kind: crate::coordination::types::AgentKind::Worker,
                    status: crate::coordination::types::AgentStatus::Active,
                    role: crate::coordination::types::AgentRole::Implementer, capabilities: vec![],
                })
                .unwrap();
        }
        store.add_task_agent_full("t", "a", "implementer", None, "", "active", "2026-01-01T00:00:00Z").unwrap();
        store.add_task_agent_full("t", "b", "reviewer", None, "", "waiting", "2026-01-01T00:00:00Z").unwrap();
        let created = (Utc::now() - chrono::Duration::minutes(11)).to_rfc3339();
        store
            .create_handoff(&Handover {
                id: "h1".into(), task_id: "t".into(), from_agent: "a".into(), to_agent: "b".into(),
                done: "d".into(), next: "n".into(), risks: String::new(), files: vec![],
                created_at: created, acked_at: None, escalated_at: None,
            })
            .unwrap();
        let task = store.get_task("t").unwrap().unwrap();
        let sub = store.board_sub_status(&task);
        assert_eq!(sub.handover.as_ref().unwrap()["state"], "pending");
        assert_eq!(escalate_handoffs(&store, Utc::now()), 1);
        assert_eq!(escalate_handoffs(&store, Utc::now()), 0, "once");
        let task = store.get_task("t").unwrap().unwrap();
        assert!(task.notifications.iter().any(|n| n.kind == "handover_unacked"));
        store.current_lease("t").unwrap();
        assert_eq!(store.board_sub_status(&task).lease.unwrap().agent_id, "a");
    }

    #[test]
    fn legacy_rows_without_a_progress_stamp_are_backfilled_not_flagged() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "old", 60, None);
        store
            .with_connection(|c| Ok(c.execute("UPDATE tasks SET last_progress_at = NULL WHERE id = 'old'", [])?))
            .unwrap();
        assert!(sweep(&store, Utc::now(), idle).is_empty());
        assert!(store.get_task("old").unwrap().unwrap().last_progress_at.is_some());
    }
}
