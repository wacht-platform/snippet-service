//! Starting the reviewer for a task that reported done and is waiting in review.

use crate::coordination::{Task, TaskState, TaskStatus, VerifyMode};
use crate::harness::{LaneSpawn, LoopInput};

use super::Daemon;

const DIFF_CHARS: usize = 40_000;

/// The agent that reviews a task: the one seated, otherwise the agent doing the
/// work, in a fresh read-only lane. Any agent can review.
fn pick_reviewer(d: &Daemon, task: &Task) -> Option<String> {
    let roster = d.store.list_task_agents(&task.id).unwrap_or_default();
    let live = || roster.iter().filter(|m| m.removed_at.is_none());
    live()
        .find(|m| m.role == "reviewer")
        .or_else(|| live().find(|m| m.status == "active"))
        .or_else(|| live().next())
        .map(|m| m.agent_id.clone())
}

fn git(workspace: &std::path::Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

/// The brief a reviewer lane starts from: the ledger, what the worker claims,
/// the diff since dispatch and what to check.
pub(super) fn review_brief(task: &Task, state: &TaskState, workspace: &std::path::Path) -> String {
    let base = if state.dispatch_rev.is_empty() { "HEAD" } else { state.dispatch_rev.as_str() };
    let stat = git(workspace, &["diff", "--stat", base]);
    let mut diff = git(workspace, &["diff", base]);
    if diff.chars().count() > DIFF_CHARS {
        diff = format!("{}\n… diff clipped; run `git diff {base} -- <path>` for the rest", diff.chars().take(DIFF_CHARS).collect::<String>());
    }
    let untracked = git(workspace, &["ls-files", "--others", "--exclude-standard"]);
    let report = state.pending_report.clone().unwrap_or_default();
    format!(
        "Review task \"{title}\" (task {id}) before it is accepted. You are read-only: do not change files. The worker says it is done; your job is to find out whether that is true.\n\n\
         {ledger}\n\n\
         worker's report:\n{summary}\n\n\
         evidence the worker gave:\n{evidence}\n\n\
         workspace: {workspace}\ndiff since dispatch ({base}):\n{stat}\nnew untracked files:\n{untracked}\n```diff\n{diff}\n```\n\n\
         Checklist:\n\
         1. For every acceptance criterion, check the evidence is real: re-run the command and compare, open the artifact, look at the screenshot with view_image.\n\
         2. Does the diff do what the brief asks and only that? Flag unrelated files, scope creep, leftovers and anything half-done.\n\
         3. Are there tests for new behaviour, and do they pass?\n\
         4. Anything unsafe, hard-coded or fragile; any claim in the report the diff does not support.\n\
         5. Content work: does it fit the audience, length, tone and definition of good in the ledger? Would its real reader accept it? For ideas and proposals, is each one specific and worth paying for?\n\n\
         Finish by calling submit_review (task_id {id}): verdict approve only if every criterion is met, otherwise changes with one finding per problem (file:line, what is wrong, what to do).",
        title = task.title,
        id = task.id,
        ledger = state.render(true),
        summary = report.summary,
        evidence = crate::review_tools::evidence_lines(state),
        workspace = workspace.display(),
    )
}

/// Start a reviewer for every task waiting on one, and hand tasks whose
/// reviewer went quiet to Mission Control.
pub(super) async fn drive_reviews(d: &Daemon) {
    let Ok(tasks) = d.store.list_tasks(None, Some(&TaskStatus::InReview)) else { return };
    let now = chrono::Utc::now();
    let stamp = now.to_rfc3339();
    for task in tasks {
        let Ok(Some(state)) = d.store.task_state(&task.id) else { continue };
        if state.review_escalated || crate::review_tools::route(&state) != VerifyMode::Reviewer {
            continue;
        }
        if state.review_started_at.is_empty() {
            start_review(d, &task, &state, &stamp).await;
            continue;
        }
        let elapsed_secs = chrono::DateTime::parse_from_rfc3339(&state.review_started_at)
            .map(|at| (now - at.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(crate::review_tools::REVIEW_TIMEOUT_SECS + 1);
        let lane_alive = elapsed_secs < 20
            || crate::session::state_path_for_id(&task.session_id)
                .and_then(|p| crate::session::read_session_meta(&p))
                .is_some_and(|meta| meta.lanes.iter().any(|l| l.status == crate::lanes::LaneStatus::Running));
        if elapsed_secs > crate::review_tools::REVIEW_TIMEOUT_SECS || !lane_alive {
            crate::review_tools::escalate(&d.store, &task.id, "the reviewer gave no verdict in time; check the work and submit_review yourself", &stamp);
        }
    }
}

async fn start_review(d: &Daemon, task: &Task, state: &TaskState, now: &str) {
    let workspace = crate::session::state_path_for_id(&task.session_id)
        .and_then(|p| crate::session::read_session_meta(&p))
        .map(|s| std::path::PathBuf::from(s.workspace));
    let Some(workspace) = workspace else {
        crate::review_tools::escalate(&d.store, &task.id, "no reviewer could start: the work session's workspace is gone", now);
        return;
    };
    let Some(reviewer) = pick_reviewer(d, task) else {
        crate::review_tools::escalate(&d.store, &task.id, "this task has no agent to review it; check the work and submit_review yourself", now);
        return;
    };
    let profile = d.store.agent_profile(&reviewer).ok().flatten();
    let spec = LaneSpawn {
        title: format!("Review: {}", task.title),
        brief: review_brief(task, state, &workspace),
        read_only: true,
        agent: Some(reviewer),
        profile,
    };
    let _ = d.store.modify_task_state(&task.id, now, |s| s.review_started_at = now.to_string());
    d.deliver(&task.session_id, LoopInput::SpawnLane(spec)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::PendingReport;

    #[test]
    fn the_review_brief_carries_ledger_report_diff_and_checklist() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success());
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        let base = crate::task_state_tools::head_rev(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let task = Task::filed_by_human("r1".into(), "Fix a".into(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        let mut state = TaskState::seeded(&task);
        state.dispatch_rev = base;
        state.pending_report = Some(PendingReport { summary: "changed a".into(), artifacts: vec![] });
        let brief = review_brief(&task, &state, dir.path());
        for needle in ["[task_ledger v1]", "worker's report:\nchanged a", "-one", "+two", "submit_review (task_id r1)", "Checklist"] {
            assert!(brief.contains(needle), "missing {needle}: {brief}");
        }
    }
}
