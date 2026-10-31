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
}

pub(super) fn probe_session(session_id: &str) -> Option<SessionProbe> {
    if session_id.trim().is_empty() {
        return None;
    }
    let state = crate::session::state_path_for_id(session_id).and_then(|p| crate::session::read_session_state(&p))?;
    let question = (state.status == crate::harness::HarnessStatus::WaitingForInput)
        .then(|| state.pending_question.as_ref().map(super::autonomy::question_text))
        .flatten();
    Some(SessionProbe { status: crate::session::status_str(state.status), question })
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
        let Some(age) = stalled_minutes(&task, now, question.is_some()) else { continue };
        if stall_marker_for_current_spell(&task).is_some() {
            continue;
        }
        let stamp = progress_stamp(&task).to_string();
        let state = session.map(|s| s.status).unwrap_or_else(|| "missing".into());
        let detail = match question {
            Some(q) => format!("the worker is waiting on an answer:\n{q}\nanswer_worker (session_id {}) if you can settle it, otherwise ask the user.", task.session_id),
            None => format!("its session is {state}; check it with inspect_session, then retry_mission_task, answer it or cancel."),
        };
        let message = format!("stalled {age} min: no progress since {stamp}; {detail}");
        if store.update_task_in(&task.id, &now.to_rfc3339(), |t| t.notify_once("stalled", &message)).is_ok() {
            flagged.push(task.id);
        }
    }
    flagged
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
        Some(SessionProbe { status: "idle".into(), question: None })
    }

    #[test]
    fn flags_after_max_of_floor_and_twice_expected_once_per_spell() {
        let store = Store::open_in_memory().unwrap();
        running_task(&store, "fresh", 5, None);
        running_task(&store, "quiet", 25, None);
        running_task(&store, "long", 25, Some(30));
        assert_eq!(sweep(&store, Utc::now(), idle), vec!["quiet".to_string()]);
        assert!(sweep(&store, Utc::now(), idle).is_empty(), "once per spell");
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
        let waiting = |_: &str| Some(SessionProbe { status: "waiting_for_input".into(), question: Some("- which db?".into()) });
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
