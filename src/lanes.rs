//! Background sub-agent lanes — snippet's infra-free analog of wacht's task
//! delegation.
//!
//! wacht delegates by creating a board item + assignment + task subscription and
//! letting a separate DB-persisted executor thread (own sandbox, own S3 mounts)
//! pick it up over NATS. snippet has none of that substrate, so a "lane" here is
//! just a child [`CodingHarness`] run on a `tokio` task: it shares the parent
//! workspace (so produced files are visible to the conversation agent), runs the
//! plain coding-agent prompt to `complete`, and reports a [`LaneResult`] back over
//! a channel. Multiple lanes run in parallel.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::tools::coding_tools;
use crate::harness::{CodingHarness, HarnessConfig};
use crate::llm::AgentModel;
use crate::prompts::coding_system_prompt;
use crate::tools::ToolContext;

/// Builds a fresh model instance for a child lane run. The TUI supplies one that
/// constructs an `OpenAiCompatibleModel` from config; one-shot library callers
/// leave it `None`, which disables delegation.
pub type ModelFactory = Arc<dyn Fn() -> Box<dyn AgentModel> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStatus {
    Running,
    Completed,
    Failed,
}

/// Persisted, render-friendly snapshot of a lane (kept in `HarnessState`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneRecord {
    pub id: String,
    pub title: String,
    pub status: LaneStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Investigation lane: file-mutation tools removed. Sticky across follow-ups.
    #[serde(default)]
    pub read_only: bool,
}

/// Terminal report delivered back to the parent loop when a lane finishes.
#[derive(Debug, Clone)]
pub struct LaneResult {
    pub id: String,
    pub title: String,
    pub status: LaneStatus,
    /// Concise final summary (the lane's terminate_loop text) — shown in the TUI.
    pub summary: Option<String>,
    /// Full report for the parent agent: action log + findings + summary.
    pub report: Option<String>,
    pub error: Option<String>,
}

/// Max lanes running at once — a runaway/cost guard so "spawn several" can't
/// balloon into dozens of concurrent coding-agent runs.
const MAX_ACTIVE_LANES: usize = 8;

/// Wall-clock cap per lane. Without one, a hung lane (stalled provider, endless
/// tool loop under the iteration backstop) never reports, and the orchestrator —
/// told that ending its turn is how it waits — waits forever.
const LANE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Owns lane lifecycle for one conversation run. Lives in the interactive loop's
/// local scope (not in the immutable `CodingHarness`). Aborts any still-running
/// lanes when dropped (the run was interrupted / ended).
pub struct LaneManager {
    factory: Option<ModelFactory>,
    workspace_root: PathBuf,
    lane_root: PathBuf,
    result_tx: mpsc::UnboundedSender<LaneResult>,
    records: Vec<LaneRecord>,
    counter: usize,
    exa_api_key: Option<String>,
    handles: Vec<tokio::task::JoinHandle<()>>,
    /// (id, title) of lanes ghosted by the last restore — see `drain_ghosts`.
    ghosts: Vec<(String, String)>,
}

impl Drop for LaneManager {
    fn drop(&mut self) {
        // Interrupt/teardown: don't leave detached lanes burning tokens.
        for h in &self.handles {
            h.abort();
        }
    }
}

impl LaneManager {
    pub fn new(
        factory: Option<ModelFactory>,
        workspace_root: PathBuf,
        lane_root: PathBuf,
        result_tx: mpsc::UnboundedSender<LaneResult>,
        exa_api_key: Option<String>,
    ) -> Self {
        Self {
            factory,
            workspace_root,
            lane_root,
            result_tx,
            records: Vec::new(),
            counter: 0,
            exa_api_key,
            handles: Vec::new(),
            ghosts: Vec::new(),
        }
    }

    /// Restore prior records (e.g. on resume) so the display reflects history.
    /// Lanes do NOT survive a process restart, so any record still marked Running
    /// is a ghost — fail it so the orchestrator doesn't wait on it forever. The
    /// ghosted (id, title) pairs are kept for `drain_ghosts`, so the interactive
    /// loop can WAKE the orchestrator with a synthetic failure report (it was told
    /// ending its turn is how it waits — a report that never comes would otherwise
    /// leave it waiting until the user happens to speak).
    pub fn with_records(mut self, mut records: Vec<LaneRecord>) -> Self {
        for record in records.iter_mut() {
            if record.status == LaneStatus::Running {
                record.status = LaneStatus::Failed;
                record.error = Some("lane did not survive a restart".to_string());
                record.finished_at = Some(Utc::now().to_rfc3339());
                self.ghosts.push((record.id.clone(), record.title.clone()));
            }
        }
        self.counter = records.len();
        self.records = records;
        self
    }

    /// Lanes ghosted by the last `with_records` restore — one-shot handoff to the
    /// caller so each can be surfaced to the orchestrator as a failure report.
    pub fn drain_ghosts(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.ghosts)
    }

    pub fn enabled(&self) -> bool {
        self.factory.is_some()
    }

    pub fn records(&self) -> &[LaneRecord] {
        &self.records
    }

    pub fn active_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.status == LaneStatus::Running)
            .count()
    }

    /// Spawn a lane. Returns the new lane id, or an error string (fed back to the
    /// model as a tool error) when delegation is unavailable.
    pub fn spawn(&mut self, title: &str, brief: &str, read_only: bool) -> Result<String, String> {
        if self.factory.is_none() {
            return Err(
                "delegate_task is unavailable in this run (no model factory; interactive mode only)."
                    .to_string(),
            );
        };
        if self.active_count() >= MAX_ACTIVE_LANES {
            return Err(format!(
                "{MAX_ACTIVE_LANES} lanes are already running — wait for some to report before delegating more."
            ));
        }

        self.counter += 1;
        let id = format!("lane-{}", self.counter);
        self.records.push(LaneRecord {
            id: id.clone(),
            title: title.to_string(),
            status: LaneStatus::Running,
            summary: None,
            error: None,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            read_only,
        });
        self.launch(&id, title, brief, false, read_only);
        Ok(id)
    }

    /// Continue a FINISHED lane with a follow-up brief: its harness state is
    /// resumed from disk, so the lane keeps everything it learned (the analog of
    /// messaging an existing agent instead of spawning a fresh one). Returns the
    /// lane's title.
    pub fn follow_up(&mut self, lane_id: &str, brief: &str) -> Result<String, String> {
        if self.factory.is_none() {
            return Err(
                "delegate_task is unavailable in this run (no model factory; interactive mode only)."
                    .to_string(),
            );
        }
        if self.active_count() >= MAX_ACTIVE_LANES {
            return Err(format!(
                "{MAX_ACTIVE_LANES} lanes are already running — wait for some to report before delegating more."
            ));
        }
        let Some(record) = self.records.iter_mut().find(|r| r.id == lane_id) else {
            let known: Vec<String> = self
                .records
                .iter()
                .map(|r| format!("\"{}\" ({})", r.title, r.id))
                .collect();
            return Err(format!(
                "no follow_up_id `{lane_id}` in this conversation. Known: [{}]. Omit lane_id to start a new one.",
                known.join(", ")
            ));
        };
        if record.status == LaneStatus::Running {
            return Err(format!(
                "lane `{lane_id}` is still running — its report will arrive as a [lane_report]; follow up after that."
            ));
        }
        // A lane lost to a restart has no live task but its state file survives —
        // following up is exactly how to revive it.
        record.status = LaneStatus::Running;
        record.finished_at = None;
        record.error = None;
        let (title, read_only) = (record.title.clone(), record.read_only);
        self.launch(lane_id, &title, brief, true, read_only);
        Ok(title)
    }

    /// Shared spawn: run the lane on a tokio task and report back over the channel.
    fn launch(&mut self, id: &str, title: &str, brief: &str, resume: bool, read_only: bool) {
        let factory = self.factory.clone().expect("checked by callers");
        let result_tx = self.result_tx.clone();
        let workspace_root = self.workspace_root.clone();
        let state_path = self.lane_root.join(format!("{id}.json"));
        let brief = brief.to_string();
        let title = title.to_string();
        let lane_id = id.to_string();
        let exa_api_key = self.exa_api_key.clone();

        let handle = tokio::spawn(async move {
            let result = tokio::time::timeout(
                LANE_TIMEOUT,
                run_lane(
                    factory,
                    workspace_root,
                    state_path,
                    brief,
                    lane_id.clone(),
                    exa_api_key,
                    resume,
                    read_only,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "lane timed out after {} minutes and was aborted — its partial work (if any) \
                     is in the workspace; re-delegate a narrower brief if the task is still needed",
                    LANE_TIMEOUT.as_secs() / 60
                ))
            });
            let lane_result = match result {
                Ok((summary, report)) => LaneResult {
                    id: lane_id,
                    title,
                    status: LaneStatus::Completed,
                    summary: Some(summary),
                    report: Some(report),
                    error: None,
                },
                Err(error) => LaneResult {
                    id: lane_id,
                    title,
                    status: LaneStatus::Failed,
                    summary: None,
                    report: None,
                    error: Some(error),
                },
            };
            let _ = result_tx.send(lane_result);
        });
        self.handles.push(handle);
    }

    /// Fold a completed lane's terminal report into its record.
    pub fn record_result(&mut self, result: &LaneResult) {
        if let Some(record) = self.records.iter_mut().find(|record| record.id == result.id) {
            record.status = result.status;
            record.summary = result.summary.clone();
            record.error = result.error.clone();
            record.finished_at = Some(Utc::now().to_rfc3339());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_lane(
    factory: ModelFactory,
    workspace_root: PathBuf,
    state_path: PathBuf,
    brief: String,
    owner: String,
    exa_api_key: Option<String>,
    resume: bool,
    read_only: bool,
) -> Result<(String, String), String> {
    let mut model = factory();
    let workspace_for_grounding = workspace_root.clone();
    let context = ToolContext::with_owner(workspace_root, owner).map_err(|error| error.to_string())?;
    let mut tools = coding_tools(exa_api_key.clone(), crate::memory::MemoryLimits::read_only());
    if read_only {
        // Investigation lane: strip the file-mutation tools so a fan-out of
        // readers can't collide with the main agent's (or each other's) edits.
        // The shell remains for inspection — the brief tells the lane its role.
        for tool in ["write_file", "edit_file", "append_file", "replace_file_content"] {
            tools.remove(tool);
        }
    }
    let harness = CodingHarness::new(
        HarnessConfig {
            system_prompt: coding_system_prompt(),
            state_path: Some(state_path),
            resume,
            exa_api_key,
            ..HarnessConfig::default()
        },
        tools,
        context,
    );
    // Lanes report to an orchestrator: make findings navigable with exact locations.
    let role = if read_only {
        "You are a READ-ONLY investigation lane: your file-editing tools are removed; do not attempt \
         to mutate the workspace (including via shell) — investigate and report. "
    } else {
        ""
    };
    let brief = format!(
        "{brief}\n\n[lane_reporting]\n{role}You are a delegated lane reporting back to an orchestrator agent. \
         In your final terminate_loop summary, cite EXACT file:line references (e.g. `src/foo.rs:42`) \
         for every location, symbol, definition, or finding you identify — report WHERE things are, not \
         just that they exist, so the orchestrator can navigate straight to them without re-searching."
    );
    let outcome = harness
        .run(&mut *model, brief)
        .await
        .map_err(|error| error.to_string())?;
    let summary = outcome
        .final_text
        .clone()
        .unwrap_or_else(|| "lane completed without a summary".to_string());
    let mut report = summarize_lane_outcome(&outcome);
    // Ground the report: the file:line citations the prompt demands are only
    // useful if they're real. Verify each against the workspace and flag the ones
    // that don't resolve, so the orchestrator knows which locations to trust.
    if let Some(check) = verify_grounding(&workspace_for_grounding, &report) {
        report.push_str("\n\n");
        report.push_str(&check);
    }
    Ok((summary, report))
}

/// Verify every `path:line` reference in `text` against the workspace. Returns a
/// `[reference_check]` block when the text contains any such references: a
/// one-line all-clear, or the list of references that don't resolve (missing
/// file / line beyond EOF) so the orchestrator treats them as unverified.
fn verify_grounding(workspace: &std::path::Path, text: &str) -> Option<String> {
    let re = regex::Regex::new(r"([A-Za-z0-9_~./+\-]+\.[A-Za-z0-9_]+):(\d{1,7})\b").ok()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut verified = 0usize;
    let mut invalid: Vec<String> = Vec::new();
    for cap in re.captures_iter(text) {
        let path_str = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let line: usize = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        // Require a real-looking path (letters, not a decimal like `3.5:1`).
        if line == 0 || !path_str.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if !seen.insert(format!("{path_str}:{line}")) {
            continue;
        }
        let resolved = if std::path::Path::new(path_str).is_absolute() {
            PathBuf::from(path_str)
        } else {
            workspace.join(path_str)
        };
        match std::fs::read_to_string(&resolved) {
            Ok(content) => {
                let lines = content.lines().count();
                if line <= lines.max(1) {
                    verified += 1;
                } else {
                    invalid.push(format!("- {path_str}:{line} (file has {lines} lines)"));
                }
            }
            // Missing OR unreadable-as-text (binary): only flag when the file
            // isn't there at all — a binary file's line refs just aren't checkable.
            Err(_) => {
                if resolved.exists() {
                    verified += 1;
                } else {
                    invalid.push(format!("- {path_str}:{line} (file not found)"));
                }
            }
        }
    }
    if verified == 0 && invalid.is_empty() {
        return None;
    }
    if invalid.is_empty() {
        return Some(format!(
            "[reference_check]\nall {verified} file:line reference(s) verified against the workspace."
        ));
    }
    const CAP: usize = 20;
    let mut out = format!(
        "[reference_check]\n{verified} file:line reference(s) verified; {} did NOT resolve — treat \
         these as unverified and re-check before relying on them:",
        invalid.len()
    );
    for item in invalid.iter().take(CAP) {
        out.push('\n');
        out.push_str(item);
    }
    if invalid.len() > CAP {
        out.push_str(&format!("\n… and {} more", invalid.len() - CAP));
    }
    Some(out)
}

/// Build the parent-facing report for a finished lane: its final summary, the
/// full log of tool calls it made, and the findings/notes it recorded — so the
/// parent agent sees everything the lane did, not just a one-line summary.
fn summarize_lane_outcome(outcome: &crate::harness::HarnessOutcome) -> String {
    use crate::harness::HarnessEvent;

    let mut actions: Vec<String> = Vec::new();
    let mut findings: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    for event in &outcome.events {
        match event {
            HarnessEvent::ToolCall { tool_name, arguments } => {
                // Track files the lane actually operated on — the concrete results.
                if matches!(
                    tool_name.as_str(),
                    "write_file" | "edit_file" | "append_file" | "replace_file_content"
                ) {
                    if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                        if !changed.iter().any(|p| p == path) {
                            changed.push(path.to_string());
                        }
                    }
                }
                actions.push(action_label(tool_name, arguments));
            }
            // The lane's deliberate self-notes are findings; mid-run progress
            // chatter (AssistantText) is low-signal and redundant with the
            // summary, so it's left out to keep the report token-dense.
            HarnessEvent::Note { entry } => {
                findings.push(truncate_text(entry, 240));
            }
            _ => {}
        }
    }

    let summary = outcome
        .final_text
        .clone()
        .unwrap_or_else(|| "lane completed without a summary".to_string());

    let mut out = format!("Summary:\n{summary}");

    if !changed.is_empty() {
        out.push_str(&format!("\n\nFiles changed/created ({}):", changed.len()));
        for path in &changed {
            out.push_str(&format!("\n- {path}"));
        }
    }

    if !actions.is_empty() {
        const CAP: usize = 80;
        out.push_str(&format!("\n\nActions taken ({} tool calls):", actions.len()));
        for (i, action) in actions.iter().take(CAP).enumerate() {
            out.push_str(&format!("\n{}. {action}", i + 1));
        }
        if actions.len() > CAP {
            out.push_str(&format!("\n… and {} more", actions.len() - CAP));
        }
    }

    if !findings.is_empty() {
        const FCAP: usize = 40;
        out.push_str("\n\nNotes:");
        for finding in findings.iter().take(FCAP) {
            out.push_str(&format!("\n- {finding}"));
        }
        if findings.len() > FCAP {
            out.push_str(&format!("\n… and {} more", findings.len() - FCAP));
        }
    }

    out
}

/// One-line label for a tool call in a lane's action log (tool + key argument).
fn action_label(tool_name: &str, args: &serde_json::Value) -> String {
    let arg = |key: &str| args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match tool_name {
        "bash" => arg("command"),
        "read_file" | "read_image" | "write_file" | "append_file" | "edit_file"
        | "replace_file_content" | "view_outline" | "list_files" => arg("path"),
        "search_content" | "search_files" | "web_search" => arg("query"),
        "web_read" => arg("url"),
        "delegate_task" => arg("title"),
        _ => "",
    };
    let detail = truncate_text(detail, 120);
    if detail.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}: {detail}")
    }
}

fn truncate_text(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}
