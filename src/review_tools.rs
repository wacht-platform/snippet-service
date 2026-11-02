//! The verification gate: evidence a worker owes when it reports done, the
//! hand-off to a reviewer, and the verdict that completes or reopens the task.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::{Evidence, PendingReport, Task, TaskResult, TaskStatus, VerifyMode};
use crate::llm::NativeToolDefinition;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

/// Reviews that may send work back before every later report goes to Mission Control.
pub const REVIEW_CHANGE_ROUNDS: u32 = 1;

/// A reviewer silent for this long is replaced by Mission Control.
pub const REVIEW_TIMEOUT_SECS: i64 = 45 * 60;

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) }
}

/// Who verifies the next report on this task.
pub fn route(state: &crate::coordination::TaskState) -> VerifyMode {
    match state.effective_mode() {
        VerifyMode::Reviewer if state.review_rounds >= REVIEW_CHANGE_ROUNDS => VerifyMode::MissionControl,
        mode => mode,
    }
}

pub fn evidence_lines(state: &crate::coordination::TaskState) -> String {
    state
        .evidence
        .iter()
        .map(|e| {
            let path = e.path.as_deref().map(|p| format!(" [{p}]")).unwrap_or_default();
            format!("  - {}: {}{path}", clip(&e.criterion, 80), clip(&e.evidence, 400))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Check a done report against the ledger. `Ok(None)`: complete it now.
/// `Ok(Some(mode))`: hold it for that verifier.
pub fn gate_done(
    store: &Store,
    task: &Task,
    evidence: &[Evidence],
    workspace: &Path,
    now: &str,
) -> Result<Option<VerifyMode>, ToolError> {
    let state = store
        .task_state(&task.id)
        .map_err(|e| ToolError::msg(format!("read ledger: {e}")))?
        .ok_or_else(|| ToolError::msg("task has no ledger"))?;
    if state.waiver.is_empty() {
        let missing = state.uncovered(evidence, workspace);
        if !missing.is_empty() {
            return Err(ToolError::msg(format!(
                "report refused: evidence does not cover every acceptance criterion. Uncovered:\n{}\nSend evidence[] with one entry per criterion ({{criterion: its number or text, evidence: the command and its result or what you saw, path: the artifact or screenshot for artifact and visual criteria}}).",
                missing.join("\n")
            )));
        }
    }
    store
        .modify_task_state(&task.id, now, |s| s.evidence = evidence.to_vec())
        .map_err(|e| ToolError::msg(format!("record evidence: {e}")))?;
    crate::task_state_tools::sync_changed_files(store, task, now);
    let mode = route(&state);
    Ok((mode != VerifyMode::SelfCheck).then_some(mode))
}

/// Hold a finished report for its verifier: the task moves to `in_review`.
pub fn submit_for_review(
    ctx: &ToolContext,
    store: &Store,
    task: &Task,
    mode: VerifyMode,
    summary: &str,
    artifacts: &[String],
    now: &str,
) -> Result<Task, ToolError> {
    let state = store
        .modify_task_state(&task.id, now, |s| {
            s.pending_report = Some(PendingReport { summary: summary.to_string(), artifacts: artifacts.to_vec() });
            s.review_started_at.clear();
            s.review_escalated = false;
            s.review_findings.clear();
        })
        .map_err(|e| ToolError::msg(format!("record report: {e}")))?;
    let by_mc = matches!(mode, VerifyMode::MissionControl | VerifyMode::User);
    let updated = store
        .update_task_in(&task.id, now, |t| {
            t.status = TaskStatus::InReview;
            if by_mc {
                let ask = if mode == VerifyMode::User { "show it to the user and ask whether to accept" } else { "check the evidence against the criteria" };
                t.notify_once(
                    "in_review",
                    &format!("{summary}\nevidence:\n{}\nVerification is yours ({}): {ask}, then submit_review.", evidence_lines(&state), mode.as_str()),
                );
            }
        })
        .map_err(|e| ToolError::msg(format!("update task: {e}")))?;
    post_room(ctx, store, &updated, "task.in_review", &format!("Task submitted for review ({}): {}", mode.as_str(), clip(summary, 300)));
    Ok(updated)
}

fn post_room(ctx: &ToolContext, store: &Store, task: &Task, event_type: &str, body: &str) {
    let author = ctx.agent_id().unwrap_or("reviewer").to_string();
    let event = crate::coordination::types::CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        thread_id: task.thread_id.clone(),
        partition_key: format!("thread:{}", task.thread_id),
        sequence: 0,
        event_type: event_type.into(),
        actor_kind: "agent".into(),
        actor_id: author,
        payload_version: 1,
        payload: crate::coordination_tools::stamp_origin(ctx, json!({"body": body, "task_id": task.id})),
        causation_id: None,
        correlation_id: Some(task.id.clone()),
        idempotency_key: uuid::Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Ok(saved) = store.append_event(&event) {
        crate::session::emit_device_event(json!({"kind": "coordination_event", "event": saved.clone()}));
        crate::serve::queue_coordination_wake(store, saved);
    }
}

/// Hand a review the reviewer cannot give to Mission Control: if a pending
/// report is already on record (criteria and artifact paths verified by
/// `gate_done`), complete the task so it never wedges in `in_review`.
pub fn escalate(store: &Store, task_id: &str, reason: &str, now: &str) {
    let pending = store
        .task_state(task_id)
        .ok()
        .flatten()
        .and_then(|s| s.pending_report);
    let _ = store.modify_task_state(task_id, now, |s| {
        s.review_escalated = true;
        s.pending_report = None;
    });
    if let Some(report) = pending
        && store
            .complete_task(
                task_id,
                TaskStatus::Done,
                TaskResult {
                    summary: report.summary,
                    artifacts: report.artifacts.into_iter().map(Into::into).collect(),
                    authoritative: true,
                },
                now,
            )
            .is_ok()
    {
        return;
    }
    let _ = store.update_task_in(task_id, now, |t| {
        if t.status == TaskStatus::InReview {
            t.notify_once("review_escalated", reason);
        }
    });
}

/// Complete a task from the report it held for review.
pub fn approve_pending(
    ctx: &ToolContext,
    store: &Store,
    task: &Task,
    state: &crate::coordination::TaskState,
    by: &str,
    decision: &str,
    now: String,
) -> Result<Task, ToolError> {
    let pending = state.pending_report.clone().ok_or_else(|| ToolError::msg("no report is waiting for review"))?;
    let done = store
        .complete_task(
            &task.id,
            TaskStatus::Done,
            TaskResult { summary: pending.summary.clone(), artifacts: pending.artifacts.iter().map(Into::into).collect(), authoritative: true },
            &now,
        )
        .map_err(|e| ToolError::msg(format!("complete task: {e}")))?;
    let _ = store.modify_task_state(&task.id, &now, |s| {
        s.pending_report = None;
        s.review_findings.clear();
    });
    let patch = crate::coordination::StatePatch { add_decisions: vec![decision.to_string()], ..Default::default() };
    let _ = store.patch_task_state(&task.id, &patch, None, by, &now);
    crate::mission_tools::announce_report(ctx, store, &done, TaskStatus::Done, &pending.summary, now);
    Ok(done)
}

pub fn revision_envelope(task: &Task, state: &crate::coordination::TaskState, feedback: &str, by: &str) -> String {
    format!(
        "[mission_control_task]\ntask_id: {id}\ntitle: {title}\nrevision: {n}\nrequested_by: agent {by} (your finished work reopened, same session)\nprevious result:\n{prev}\nfeedback:\n{feedback}\n{ledger}\n\nChange what the feedback names, in this session, then report again with report_mission_task (task_id {id}) and evidence for each acceptance criterion.\n[/mission_control_task]",
        id = task.id,
        title = task.title,
        n = state.revision,
        prev = state.previous_result.as_ref().map(|p| p.summary.as_str()).unwrap_or(""),
        ledger = state.render(false),
    )
}

/// Reopen a done task in its own session with the reviewer's or Mission
/// Control's feedback, and the previous result in the envelope.
pub fn revise_task(store: &Store, task_id: &str, feedback: &str, by: &str, now: &str) -> Result<Task, ToolError> {
    let feedback = feedback.trim();
    if feedback.is_empty() {
        return Err(ToolError::msg("revise needs feedback: say what to change"));
    }
    let before = store.get_task(task_id).map_err(|e| ToolError::msg(e.to_string()))?.ok_or_else(|| ToolError::msg(format!("no task `{task_id}`")))?;
    let reopened = store.reopen_for_revision(task_id, now).map_err(|e| ToolError::msg(e.to_string()))?;
    let previous = before.result.as_ref().map(|r| PendingReport { summary: r.summary.clone(), artifacts: r.artifacts.iter().map(|p| p.display().to_string()).collect() });
    let state = store
        .modify_task_state(task_id, now, |s| {
            s.revision += 1;
            s.previous_result = previous.clone();
            s.review_rounds = 0;
            s.review_findings.clear();
            s.review_started_at.clear();
            s.review_escalated = false;
            s.pending_report = None;
            s.evidence.clear();
        })
        .map_err(|e| ToolError::msg(e.to_string()))?;
    let patch = crate::coordination::StatePatch { add_decisions: vec![format!("revision {} requested: {}", state.revision, clip(feedback, 300))], ..Default::default() };
    let _ = store.patch_task_state(task_id, &patch, None, by, now);
    let state = store.task_state(task_id).ok().flatten().unwrap_or(state);
    let envelope = revision_envelope(&reopened, &state, feedback, by);
    store
        .send_task_notice(("agent", by), &reopened.session_id, &reopened.id, "task_brief", &envelope, now)
        .map_err(|e| ToolError::msg(format!("send revision: {e}")))?;
    Ok(reopened)
}

#[derive(Deserialize)]
struct SubmitReviewArgs {
    #[serde(default)]
    task_id: Option<String>,
    verdict: String,
    #[serde(default)]
    findings: Vec<String>,
}

pub struct SubmitReview;
#[async_trait]
impl Tool for SubmitReview {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "submit_review".into(),
            description: "Give the verdict on a task in review. approve completes it; changes sends it back to its worker with your findings.".into(),
            input_schema: json!({"type":"object","properties":{
                "task_id":{"type":"string","description":"Defaults to the task in review in the session you review for."},
                "verdict":{"type":"string","enum":["approve","changes"]},
                "findings":{"type":"array","items":{"type":"string"},"description":"For changes: each problem with file:line and what to do. For approve: optional notes."}
            },"required":["verdict"]}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SubmitReviewArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(format!("submit_review arguments: {e}")))?;
        let approve = match args.verdict.as_str() {
            "approve" => true,
            "changes" => false,
            other => return Err(ToolError::msg(format!("verdict must be approve or changes, got `{other}`"))),
        };
        let findings: Vec<String> = args.findings.iter().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
        if !approve && findings.is_empty() {
            return Err(ToolError::msg("changes needs findings: say what to fix"));
        }
        let store = ctx.store().map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let canonical = |id: &str| crate::conversations::canonical_session_id(id).0;
        let is_mc = ctx.agent_id() == Some(crate::mission_control::SESSION_ID)
            || ctx.durable_session_id().is_some_and(|s| canonical(s) == crate::mission_control::SESSION_ID);
        let task = match args.task_id.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(id) => store.get_task(id).map_err(|e| ToolError::msg(e.to_string()))?.ok_or_else(|| ToolError::msg(format!("no task `{id}`")))?,
            None => {
                let origin = ctx.lane_origin().map(|(s, _)| canonical(s)).ok_or_else(|| ToolError::msg("name a task_id"))?;
                store
                    .list_tasks(None, Some(&TaskStatus::InReview))
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .into_iter()
                    .find(|t| canonical(&t.session_id) == origin)
                    .ok_or_else(|| ToolError::msg("no task is in review for this session"))?
            }
        };
        if task.status.is_terminal() {
            return Ok(ToolResult::success(json!({"task_id": task.id, "status": task.status.as_str()})));
        }
        let state = store.task_state(&task.id).map_err(|e| ToolError::msg(e.to_string()))?.unwrap_or_default();
        if task.status != TaskStatus::InReview {
            if is_mc && approve {
                let now = chrono::Utc::now().to_rfc3339();
                let note = if findings.is_empty() { String::new() } else { format!(" Notes: {}", findings.join("; ")) };
                let done = approve_pending(ctx, &store, &task, &state, crate::mission_control::SESSION_ID, &format!("approved by Mission Control.{note}"), now)?;
                return Ok(ToolResult::success(json!({"task_id": done.id, "status": "done"})));
            }
            return Err(ToolError::msg(format!("task `{}` is {}, not in review", task.id, task.status)));
        }
        let reviewer = if is_mc {
            crate::mission_control::SESSION_ID.to_string()
        } else {
            let lane_of_worker = ctx.lane_origin().is_some_and(|(s, _)| canonical(s) == canonical(&task.session_id));
            if !lane_of_worker || route(&state) != VerifyMode::Reviewer {
                return Err(ToolError::msg("only the reviewer of this task, or Mission Control when it holds the verification, may give a verdict"));
            }
            ctx.agent_id().unwrap_or("reviewer").to_string()
        };
        let now = chrono::Utc::now().to_rfc3339();
        if approve {
            let note = if findings.is_empty() { String::new() } else { format!(" Notes: {}", findings.join("; ")) };
            let done = approve_pending(ctx, &store, &task, &state, &reviewer, &format!("review approved.{note}"), now)?;
            return Ok(ToolResult::success(json!({"task_id": done.id, "status": "done"})));
        }
        let rounds = state.review_rounds + 1;
        let patch = crate::coordination::StatePatch { add_decisions: vec![format!("review asked for changes (round {rounds})")], ..Default::default() };
        let _ = store.patch_task_state(&task.id, &patch, None, &reviewer, &now);
        let state = store
            .modify_task_state(&task.id, &now, |s| {
                s.review_rounds = rounds;
                s.review_findings = findings.clone();
                s.pending_report = None;
                s.review_started_at.clear();
            })
            .map_err(|e| ToolError::msg(e.to_string()))?;
        let reopened = store
            .update_task_in(&task.id, &now, |t| {
                t.status = TaskStatus::InProgress;
                t.reporting_session = Some(t.session_id.clone());
            })
            .map_err(|e| ToolError::msg(e.to_string()))?;
        let envelope = format!(
            "[review_findings]\ntask_id: {}\ntitle: {}\nreviewer: {reviewer}\nround: {rounds}\nThe review sent this task back. Fix every finding, then report again with report_mission_task (task_id {}) and evidence for each acceptance criterion.\nfindings:\n{}\n{}\n[/review_findings]",
            task.id,
            task.title,
            task.id,
            findings.iter().map(|f| format!("  - {f}")).collect::<Vec<_>>().join("\n"),
            state.render(false),
        );
        store
            .send_task_notice(("agent", &reviewer), &task.session_id, &task.id, "task_brief", &envelope, &now)
            .map_err(|e| ToolError::msg(format!("send findings: {e}")))?;
        Ok(ToolResult::success(json!({"task_id": reopened.id, "status": "in_progress", "sent_back_to": reopened.session_id})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{Criterion, StatePatch, VerifyMethod};

    fn seed(root: &Path, session: &str) -> (Store, Task) {
        let store = Store::open(root.join("snippet.db")).unwrap();
        let mut task = Task::filed_by_human("rv1".into(), "T".into(), "Brief".into(), session.into(), 0, "2026-01-01T00:00:00Z".into());
        task.status = TaskStatus::InProgress;
        task.reporting_session = Some(session.into());
        store.create_task(&task).unwrap();
        let patch = StatePatch {
            criteria: Some(vec![
                Criterion { criterion: "cargo test passes".into(), method: VerifyMethod::Command },
                Criterion { criterion: "screenshot shows the page".into(), method: VerifyMethod::Visual },
            ]),
            ..Default::default()
        };
        store.patch_task_state("rv1", &patch, None, "mission-control", "2026-01-01T00:00:01Z").unwrap();
        (store, task)
    }

    fn ev(criterion: &str, evidence: &str, path: Option<&str>) -> Evidence {
        Evidence { criterion: criterion.into(), evidence: evidence.into(), path: path.map(Into::into) }
    }

    #[test]
    fn a_done_report_must_cover_every_criterion_and_names_the_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let (store, task) = seed(dir.path(), "s1");
        let err = gate_done(&store, &task, &[ev("1", "cargo test: 12 passed", None)], dir.path(), "2026-01-02T00:00:00Z").unwrap_err().to_string();
        assert!(err.contains("2. [visual] screenshot shows the page (no evidence)") && !err.contains("1. [command]"), "{err}");
        let err = gate_done(&store, &task, &[ev("cargo test passes", "ok", None), ev("2", "looks right", Some("shot.png"))], dir.path(), "2026-01-02T00:00:00Z").unwrap_err().to_string();
        assert!(err.contains("needs a path to an existing artifact"), "{err}");
        std::fs::write(dir.path().join("shot.png"), "x").unwrap();
        let mode = gate_done(&store, &task, &[ev("cargo test passes", "ok", None), ev("2", "looks right", Some("shot.png"))], dir.path(), "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(mode, Some(VerifyMode::Reviewer));
    }

    #[test]
    fn a_waiver_or_no_criteria_completes_without_review_and_one_round_escalates() {
        let dir = tempfile::tempdir().unwrap();
        let (store, task) = seed(dir.path(), "s1");
        store.modify_task_state("rv1", "t", |s| s.waiver = "user said skip".into()).unwrap();
        assert_eq!(gate_done(&store, &task, &[], dir.path(), "t").unwrap(), None);
        store.modify_task_state("rv1", "t", |s| { s.waiver.clear(); s.review_rounds = 1; }).unwrap();
        let state = store.task_state("rv1").unwrap().unwrap();
        assert_eq!(route(&state), VerifyMode::MissionControl);
        let bare = crate::coordination::TaskState::default();
        assert_eq!(bare.effective_mode(), VerifyMode::SelfCheck);
    }

    #[tokio::test]
    async fn reporting_done_holds_the_task_for_review_and_refuses_a_second_report() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = seed(dir.path(), "s1");
        let worker = ToolContext::new(dir.path()).unwrap().with_durable_session_id("s1").with_store_path(dir.path().join("snippet.db"));
        let report = |evidence: Value| json!({"task_id": "rv1", "status": "done", "summary": "all done", "evidence": evidence});
        let refused = crate::mission_tools::ReportMissionTask.execute(&worker, report(json!([]))).await.unwrap_err().to_string();
        assert!(refused.contains("1. [command] cargo test passes") && refused.contains("2. [visual]"), "{refused}");
        std::fs::write(dir.path().join("shot.png"), "x").unwrap();
        let held = crate::mission_tools::ReportMissionTask
            .execute(&worker, report(json!([{"criterion": "1", "evidence": "12 passed"}, {"criterion": "2", "evidence": "page renders", "path": "shot.png"}])))
            .await
            .unwrap();
        assert_eq!(held.value["data"]["in_review"], "reviewer");
        assert_eq!(store.get_task("rv1").unwrap().unwrap().status, TaskStatus::InReview);
        let again = crate::mission_tools::ReportMissionTask.execute(&worker, report(json!([]))).await.unwrap_err().to_string();
        assert!(again.contains("is in review"), "{again}");
        let state = store.task_state("rv1").unwrap().unwrap();
        assert_eq!((state.evidence.len(), state.pending_report.unwrap().summary.as_str()), (2, "all done"));
    }

    #[test]
    fn revising_a_done_task_reopens_it_in_the_same_session_with_the_previous_result() {
        let dir = tempfile::tempdir().unwrap();
        let (store, task) = seed(dir.path(), "s1");
        assert!(revise_task(&store, "rv1", "tighten it", "mission-control", "2026-01-02T00:00:00Z").unwrap_err().to_string().contains("only a done task"));
        store
            .complete_task("rv1", TaskStatus::Done, TaskResult { summary: "first cut".into(), artifacts: vec![], authoritative: true }, "2026-01-02T00:00:00Z")
            .unwrap();
        assert!(revise_task(&store, "rv1", " ", "mission-control", "t").is_err());
        let back = revise_task(&store, "rv1", "tighten the intro", "mission-control", "2026-01-03T00:00:00Z").unwrap();
        assert_eq!((&back.status, back.session_id.as_str(), back.reporting_session.as_deref()), (&TaskStatus::InProgress, "s1", Some("s1")));
        let state = store.task_state("rv1").unwrap().unwrap();
        assert_eq!((state.revision, state.previous_result.as_ref().unwrap().summary.as_str()), (1, "first cut"));
        assert!(state.decisions.iter().any(|d| d.text.contains("revision 1 requested: tighten the intro")));
        let envelope = revision_envelope(&back, &state, "tighten the intro", "mission-control");
        for needle in ["revision: 1", "previous result:\nfirst cut", "feedback:\ntighten the intro", "report_mission_task (task_id rv1)"] {
            assert!(envelope.contains(needle), "{needle}: {envelope}");
        }
        let _ = task;
        assert!(revise_task(&store, "rv1", "again", "mission-control", "t").is_err());
    }

    #[tokio::test]
    async fn filing_a_task_without_acceptance_is_refused_unless_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let mc = ToolContext::new(dir.path()).unwrap().with_agent_id(crate::mission_control::SESSION_ID).with_store_path(dir.path().join("snippet.db"));
        let err = crate::mission_tools::CreateMissionTask
            .execute(&mc, json!({"title": "t", "description": "d", "session": "nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("acceptance criteria are required"), "{err}");
        let err = crate::mission_tools::CreateMissionTask
            .execute(&mc, json!({"title": "t", "description": "d", "session": "nope", "read_only": true}))
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("acceptance"), "{err}");
    }

    #[tokio::test]
    async fn the_reviewer_lane_approves_or_sends_back_and_the_worker_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let (store, task) = seed(dir.path(), "s1");
        let worker = ToolContext::new(dir.path()).unwrap().with_durable_session_id("s1").with_store_path(dir.path().join("snippet.db"));
        let lane = ToolContext::new(dir.path()).unwrap().with_lane_origin("s1", "lane-1").with_store_path(dir.path().join("snippet.db"));
        submit_for_review(&worker, &store, &task, VerifyMode::Reviewer, "did it", &[], "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(store.get_task("rv1").unwrap().unwrap().status, TaskStatus::InReview);
        let refused = SubmitReview.execute(&worker, json!({"verdict": "approve"})).await.unwrap_err().to_string();
        assert!(refused.contains("no task is in review") || refused.contains("name a task_id") || refused.contains("only the reviewer"), "{refused}");
        let refused = SubmitReview.execute(&worker, json!({"task_id": "rv1", "verdict": "approve"})).await.unwrap_err().to_string();
        assert!(refused.contains("only the reviewer"), "{refused}");
        assert!(SubmitReview.execute(&lane, json!({"verdict": "changes"})).await.is_err());
        SubmitReview.execute(&lane, json!({"verdict": "changes", "findings": ["src/a.rs:3 unchecked unwrap"]})).await.unwrap();
        let back = store.get_task("rv1").unwrap().unwrap();
        assert_eq!(back.status, TaskStatus::InProgress);
        let state = store.task_state("rv1").unwrap().unwrap();
        assert_eq!((state.review_rounds, state.review_findings.len()), (1, 1));
        assert!(state.render(false).contains("review findings to fix (round 1)"));
        assert_eq!(route(&state), VerifyMode::MissionControl);
        submit_for_review(&worker, &store, &back, VerifyMode::MissionControl, "fixed", &[], "2026-01-03T00:00:00Z").unwrap();
        assert!(store.get_task("rv1").unwrap().unwrap().notifications.iter().any(|n| n.kind == "in_review"));
        let mc = ToolContext::new(dir.path()).unwrap().with_agent_id(crate::mission_control::SESSION_ID).with_store_path(dir.path().join("snippet.db"));
        SubmitReview.execute(&mc, json!({"task_id": "rv1", "verdict": "approve"})).await.unwrap();
        let done = store.get_task("rv1").unwrap().unwrap();
        assert_eq!((done.status, done.result.unwrap().summary.as_str()), (TaskStatus::Done, "fixed"));
    }
}
