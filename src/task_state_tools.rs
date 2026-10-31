//! Tools and helpers around the task ledger ([`crate::coordination::TaskState`]).

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::types::CoordinationEvent;
use crate::coordination::{StatePatch, Task, TaskState};
use crate::llm::NativeToolDefinition;
use crate::store::{Store, StoreError};
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

fn canonical_task_session(id: &str) -> String {
    crate::conversations::canonical_session_id(id).0
}

/// The ledger rendered for an envelope, with the path of the task's shared
/// memory when it has one. Empty if the task has no ledger yet and cannot be read.
pub fn ledger_block(store: &Store, task: &Task, full: bool) -> String {
    match store.task_state(&task.id) {
        Ok(Some(state)) => format!("{}{}{}", state.render(full), memory_line(task), goal_part(store, task, full)),
        _ => String::new(),
    }
}

fn goal_part(store: &Store, task: &Task, full: bool) -> String {
    let block = crate::coordination::goals::goal_block(store, task, full);
    if block.is_empty() { block } else { format!("\n{}", block.trim_end()) }
}

fn memory_line(task: &Task) -> String {
    crate::memory::MemoryRepo::task(&task.id)
        .filter(|r| r.exists())
        .map(|repo| format!("\ntask memory (decisions, read it with the shell): {}", repo.root.display()))
        .unwrap_or_default()
}

/// The ledger for a dispatch envelope. Stamps the workspace revision work starts
/// from (once), and leaves the brief out when the envelope already carries it as `scope`.
pub fn dispatch_ledger(store: &Store, task: &Task, workspace: &Path, scope: &str, now: &str) -> String {
    if let Some(rev) = head_rev(workspace) {
        let _ = store.modify_task_state(&task.id, now, |s| {
            if s.dispatch_rev.is_empty() {
                s.dispatch_rev = rev;
            }
        });
    }
    let Ok(Some(mut state)) = store.task_state(&task.id) else {
        return String::new();
    };
    if state.brief.trim() == scope.trim() {
        state.brief.clear();
    }
    format!("{}{}{}", state.render(true), memory_line(task), goal_part(store, task, true))
}

/// Gaps in a content brief worth fixing before the work starts.
pub fn content_warnings(
    title: &str,
    description: &str,
    audience: &str,
    definition_of_good: &str,
    criteria: &[crate::coordination::Criterion],
) -> Vec<String> {
    let text = format!("{title} {description}").to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    const CONTENT: &[&str] = &["deck", "slides", "presentation", "video", "blog", "article", "essay", "newsletter", "email", "readme", "copy", "script", "landing page", "documentation", "report", "mockup", "design"];
    const VISUAL: &[&str] = &["deck", "slides", "presentation", "video", "landing page", "mockup", "design", "ui ", "screen"];
    if !has(CONTENT) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let both = format!("{text} {} {}", audience.to_lowercase(), definition_of_good.to_lowercase());
    let said = |words: &[&str]| words.iter().any(|w| both.contains(w));
    if audience.trim().is_empty() && !said(&["audience", "readers", "for the team", "for customers", "for users"]) {
        out.push("content work with no audience: say who it is for".to_string());
    }
    if !said(&["words", "pages", "slides", "minutes", "seconds", "length", "long", "short", "brief"]) {
        out.push("content work with no length: say how long it should be".to_string());
    }
    if !said(&["tone", "voice", "formal", "casual", "friendly", "plain", "technical", "style"]) {
        out.push("content work with no tone: say how it should sound".to_string());
    }
    if has(VISUAL) && !criteria.iter().any(|c| c.method == crate::coordination::VerifyMethod::Visual) {
        out.push("visual work with no visual criterion: add an acceptance criterion with method visual (a screenshot or render someone looks at)".to_string());
    }
    out
}

/// Ledger fields for the task HTTP API. Additive: a client that predates the
/// ledger ignores them. Reads without creating a ledger.
pub fn api_fields(store: &Store, task: &Task) -> serde_json::Map<String, Value> {
    let state = store.peek_task_state(&task.id).ok().flatten();
    let mut out = serde_json::Map::new();
    let mode = state.as_ref().map(|s| s.effective_mode().as_str());
    out.insert("in_review".into(), json!(task.status == crate::coordination::TaskStatus::InReview));
    out.insert("verify_mode".into(), json!(mode));
    out.insert("acceptance".into(), json!(state.as_ref().map(|s| s.criteria.clone()).unwrap_or_default()));
    out.insert("revision".into(), json!(state.as_ref().map_or(0, |s| s.revision)));
    out.insert(
        "review".into(),
        json!(state.as_ref().map(|s| json!({
            "rounds": s.review_rounds,
            "findings": s.review_findings,
            "escalated": s.review_escalated,
            "waived": (!s.waiver.is_empty()).then(|| s.waiver.clone()),
            "evidence": s.evidence,
        }))),
    );
    out.insert("ledger".into(), json!(state));
    out
}

fn workspace_of(task: &Task) -> Option<std::path::PathBuf> {
    let path = crate::session::state_path_for_id(&task.session_id)?;
    crate::session::read_session_state(&path).map(|s| s.workspace.into())
}

fn git_lines(workspace: &Path, args: &[&str]) -> Vec<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

pub fn head_rev(workspace: &Path) -> Option<String> {
    git_lines(workspace, &["rev-parse", "HEAD"]).into_iter().next()
}

/// Files changed in the task's workspace since `base`, committed or not, plus untracked ones.
pub fn git_changed_files(workspace: &Path, base: &str) -> Vec<String> {
    let base = if base.is_empty() { "HEAD" } else { base };
    let mut files = git_lines(workspace, &["diff", "--name-only", base]);
    files.extend(git_lines(workspace, &["ls-files", "--others", "--exclude-standard"]));
    files.sort();
    files.dedup();
    files
}

/// Fold git's view of the task's workspace into the ledger. Never wakes anyone.
pub fn sync_changed_files(store: &Store, task: &Task, now: &str) {
    let (Some(workspace), Ok(Some(state))) = (workspace_of(task), store.task_state(&task.id)) else {
        return;
    };
    let files = git_changed_files(&workspace, &state.dispatch_rev);
    if files.is_empty() {
        return;
    }
    let patch = StatePatch { add_changed_files: files, ..StatePatch::default() };
    let _ = store.patch_task_state(&task.id, &patch, None, "git", now);
}

/// Record paths a session just wrote against the task it is working.
pub fn note_changed_files(ctx: &ToolContext, paths: &[String]) {
    let Some(session) = ctx.durable_session_id().or(ctx.lane_origin().map(|(s, _)| s)) else { return };
    let Ok(store) = ctx.store() else { return };
    let Some(task) = store
        .list_tasks(Some(session), Some(&crate::coordination::TaskStatus::InProgress))
        .ok()
        .and_then(|tasks| tasks.into_iter().find(|t| t.reporting_session.as_deref() == Some(session)))
    else {
        return;
    };
    let root = ctx.workspace_root();
    let rel: Vec<String> = paths
        .iter()
        .map(|p| Path::new(p).strip_prefix(root).map(|r| r.display().to_string()).unwrap_or_else(|_| p.clone()))
        .collect();
    let patch = StatePatch { add_changed_files: rel, ..StatePatch::default() };
    let _ = store.patch_task_state(&task.id, &patch, None, "change_files", &chrono::Utc::now().to_rfc3339());
}

fn remember_decisions(task_id: &str, author: &str, changes: &[String]) {
    let Some(repo) = crate::memory::MemoryRepo::task(task_id) else { return };
    let lines: Vec<&String> = changes.iter().filter(|c| c.starts_with("decision by ")).collect();
    if lines.is_empty() || repo.ensure().is_err() {
        return;
    }
    for line in lines {
        let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let entry: String = flat.chars().take(380).collect();
        let _ = repo.add_entry("decisions", &format!("{entry} [author: {author}]"));
    }
    let _ = repo.commit_all("Record decisions");
}

/// Tell the task room the ledger moved. The room wake carries only these lines.
pub fn post_state_event(store: &Store, ctx: &ToolContext, task: &Task, author: &str, state: &TaskState, changes: &[String]) {
    let body = format!("Task ledger v{} updated by {author}:\n- {}", state.version, changes.join("\n- "));
    let event = CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        thread_id: task.thread_id.clone(),
        partition_key: format!("thread:{}", task.thread_id),
        sequence: 0,
        event_type: "task.state_updated".into(),
        actor_kind: "agent".into(),
        actor_id: author.to_string(),
        payload_version: 1,
        payload: crate::coordination_tools::stamp_origin(ctx, json!({
            "body": body,
            "task_id": task.id,
            "version": state.version,
        })),
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

fn is_mission_control(ctx: &ToolContext) -> bool {
    ctx.agent_id() == Some(crate::mission_control::SESSION_ID)
}

fn author_of(ctx: &ToolContext) -> String {
    if is_mission_control(ctx) {
        return crate::mission_control::SESSION_ID.to_string();
    }
    crate::coordination_tools::speaks_as(ctx)
        .map(|(_, id)| id)
        .unwrap_or_else(|| crate::coordination::SNIPPET_AGENT_ID.to_string())
}

/// The task a call is about: the one named, or the task in progress in this
/// session. Mission Control may name any task; a worker only its own.
fn target_task(ctx: &ToolContext, store: &Store, named: Option<&str>) -> Result<Task, ToolError> {
    let session = ctx.durable_session_id().or(ctx.lane_origin().map(|(s, _)| s));
    let id = match named.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => session
            .and_then(|s| crate::coordination_tools::session_task(ctx, s))
            .map(|(id, _)| id)
            .ok_or_else(|| ToolError::msg("name a task_id: this session has no task in progress"))?,
    };
    let task = store
        .get_task(&id)
        .map_err(|e| ToolError::msg(format!("load task: {e}")))?
        .ok_or_else(|| ToolError::msg(format!("no task `{id}`")))?;
    if !is_mission_control(ctx) {
        let mine = session.is_some_and(|s| {
            canonical_task_session(s) == canonical_task_session(&task.session_id)
                || task.reporting_session.as_deref().is_some_and(|r| canonical_task_session(r) == canonical_task_session(s))
        });
        if !mine {
            return Err(ToolError::msg("that task is not yours; only the session doing it or Mission Control may use its ledger"));
        }
    }
    Ok(task)
}

#[derive(Deserialize)]
struct UpdateStateArgs {
    #[serde(default)]
    task_id: Option<String>,
    patch: Value,
    expected_version: u64,
}

pub struct UpdateTaskState;
#[async_trait]
impl Tool for UpdateTaskState {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "update_task_state".into(),
            description: "Change the task ledger the whole task shares. Send the version you last read; a stale one is refused. The others on the task are woken with only what changed.".into(),
            input_schema: json!({"type":"object","properties":{
                "task_id":{"type":"string","description":"Defaults to the task in progress in this session."},
                "expected_version":{"type":"integer"},
                "patch":{"type":"object","description":"Any of: brief, criteria [{criterion, method: command|artifact|visual|human}], deliverables [..], add_decisions [..], add_changed_files [..], open_questions [..] (replaces), current_step, audience, definition_of_good.","additionalProperties":false}
            },"required":["patch","expected_version"]}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: UpdateStateArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::msg(format!("update_task_state arguments: {e}")))?;
        let patch: StatePatch = serde_json::from_value(args.patch).map_err(|e| ToolError::msg(format!("patch: {e}")))?;
        let store = ctx.store().map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let task = target_task(ctx, &store, args.task_id.as_deref())?;
        if task.status.is_terminal() {
            return Err(ToolError::msg(format!("task `{}` is {}; its ledger is closed", task.id, task.status)));
        }
        let author = author_of(ctx);
        let now = chrono::Utc::now().to_rfc3339();
        let (state, changes) = match store.patch_task_state(&task.id, &patch, Some(args.expected_version), &author, &now) {
            Ok(done) => done,
            Err(error @ StoreError::StaleTaskState { .. }) => {
                let current = store.task_state(&task.id).ok().flatten().map(|s| s.render(true)).unwrap_or_default();
                return Err(ToolError::msg(format!("{error}\n{current}")));
            }
            Err(error) => return Err(ToolError::msg(format!("update ledger: {error}"))),
        };
        if changes.is_empty() {
            return Ok(ToolResult::success(json!({"version": state.version, "changed": [], "note": "nothing differed; version unchanged"})));
        }
        remember_decisions(&task.id, &author, &changes);
        post_state_event(&store, ctx, &task, &author, &state, &changes);
        Ok(ToolResult::success(json!({"version": state.version, "changed": changes})))
    }
}

#[derive(Deserialize)]
struct LedgerArgs {
    #[serde(default)]
    task_id: Option<String>,
}

pub struct TaskLedger;
#[async_trait]
impl Tool for TaskLedger {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "task_ledger".into(),
            description: "Read a task's ledger in full: brief, acceptance criteria, decisions, changed files (refreshed from git), open questions, current step. Gives the version update_task_state needs.".into(),
            input_schema: json!({"type":"object","properties":{"task_id":{"type":"string","description":"Defaults to the task in progress in this session."}}}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: LedgerArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let store = ctx.store().map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let task = target_task(ctx, &store, args.task_id.as_deref())?;
        sync_changed_files(&store, &task, &chrono::Utc::now().to_rfc3339());
        let state = store
            .task_state(&task.id)
            .map_err(|e| ToolError::msg(e.to_string()))?
            .ok_or_else(|| ToolError::msg("no ledger"))?;
        Ok(ToolResult::success(json!({"ledger": state, "text": ledger_block(&store, &task, true)})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &Path, session: &str) -> ToolContext {
        ToolContext::new(root)
            .unwrap()
            .with_durable_session_id(session)
            .with_store_path(root.join("snippet.db"))
    }

    fn seed(root: &Path, session: &str) -> (Store, Task) {
        let store = Store::open(root.join("snippet.db")).unwrap();
        let mut task = Task::filed_by_human("tl1".into(), "T".into(), "Brief text".into(), session.into(), 0, "2026-01-01T00:00:00Z".into());
        task.status = crate::coordination::TaskStatus::InProgress;
        task.reporting_session = Some(session.into());
        store.create_task(&task).unwrap();
        (store, task)
    }

    #[tokio::test]
    async fn a_worker_updates_its_own_ledger_and_a_stale_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), "sess-1");
        let (store, task) = seed(dir.path(), "sess-1");
        let ok = UpdateTaskState
            .execute(&ctx, json!({"expected_version": 1, "patch": {"current_step": "step one", "open_questions": ["which db?"]}}))
            .await
            .unwrap();
        assert_eq!(ok.value["data"]["version"], 2);
        let stale = UpdateTaskState
            .execute(&ctx, json!({"task_id": task.id, "expected_version": 1, "patch": {"audience": "x"}}))
            .await;
        assert!(stale.unwrap_err().to_string().contains("version 2"));
        let events = store.recent_events_for_thread(&task.thread_id, 5).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.state_updated" && e.payload["body"].as_str().unwrap().contains("step one")));
    }

    #[tokio::test]
    async fn another_session_cannot_touch_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "sess-1");
        let other = context(dir.path(), "sess-2");
        let result = UpdateTaskState
            .execute(&other, json!({"task_id": "tl1", "expected_version": 1, "patch": {"audience": "x"}}))
            .await;
        assert!(result.unwrap_err().to_string().contains("not yours"));
    }

    #[test]
    fn content_briefs_are_checked_for_audience_length_tone_and_a_visual_criterion() {
        let w = content_warnings("Write the launch blog post", "Announce the feature", "", "", &[]);
        assert_eq!(w.len(), 3, "{w:?}");
        let ok = content_warnings("Build the sales deck", "for customers, 10 slides, plain tone", "buyers", "crisp", &[crate::coordination::Criterion { criterion: "slides render".into(), method: crate::coordination::VerifyMethod::Visual }]);
        assert!(ok.is_empty(), "{ok:?}");
        assert!(content_warnings("Fix the parser bug", "off by one", "", "", &[]).is_empty());
        assert!(content_warnings("Build the sales deck", "for customers, 10 slides, plain tone", "", "", &[]).iter().any(|w| w.contains("visual criterion")));
    }

    #[test]
    fn changed_files_come_from_git_since_the_dispatch_revision() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success());
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        let base = head_rev(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "2").unwrap();
        git(&["commit", "-qam", "edit"]);
        std::fs::write(dir.path().join("new.txt"), "n").unwrap();
        assert_eq!(git_changed_files(dir.path(), &base), ["a.txt", "new.txt"]);
    }
}
