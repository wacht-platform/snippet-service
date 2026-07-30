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

use crate::harness::{CodingHarness, HarnessConfig};
use crate::lane_log::LaneLog;
use crate::llm::AgentModel;
use crate::prompts::coding_system_prompt;
use crate::tools::ToolContext;
use crate::tools::coding_tools;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneActivity {
    pub at: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LaneProgress {
    pub id: String,
    pub kind: String,
    pub text: String,
}

/// Persisted, render-friendly snapshot of a lane (kept in `HarnessState`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneRecord {
    pub id: String,
    pub title: String,
    pub status: LaneStatus,
    /// The original handoff/brief given to this lane, before reporting instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The full verified report returned by the lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Latest safe operational activity, never a user-addressable prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_at: Option<String>,
    /// Small durable tail for the read-only live lane viewer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_log: Vec<LaneActivity>,
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

/// Finished lanes older than this are dropped from the session snapshot and disk.
const LANE_FINISHED_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Cap on retained finished lanes (Running never counts). Oldest finished drop first.
const MAX_FINISHED_LANES: usize = 32;

/// Owns lane lifecycle for one conversation run. Lives in the interactive loop's
/// local scope (not in the immutable `CodingHarness`). Aborts any still-running
/// lanes when dropped (the run was interrupted / ended).
pub struct LaneManager {
    factory: Option<ModelFactory>,
    workspace_root: PathBuf,
    lane_root: PathBuf,
    result_tx: mpsc::UnboundedSender<LaneResult>,
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
    records: Vec<LaneRecord>,
    counter: usize,
    exa_api_key: Option<String>,
    handles: Vec<tokio::task::JoinHandle<()>>,
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
        progress_tx: mpsc::UnboundedSender<LaneProgress>,
        exa_api_key: Option<String>,
    ) -> Self {
        Self {
            factory,
            workspace_root,
            lane_root,
            result_tx,
            progress_tx,
            records: Vec::new(),
            counter: 0,
            exa_api_key,
            handles: Vec::new(),
        }
    }

    /// Restore prior records (e.g. on resume) so the display reflects history.
    /// Runs housekeeping so aged-out finished lanes (and orphan disk files) do not
    /// accumulate across resumes.
    pub fn with_records(mut self, records: Vec<LaneRecord>) -> Self {
        // Counter must keep rising past historical ids even after prune, so new
        // spawns never collide with on-disk `lane-N.json` from dropped records.
        self.counter = records
            .iter()
            .filter_map(|r| r.id.strip_prefix("lane-")?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        self.records = records;
        let _ = self.housekeep();
        self
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
            handoff: Some(brief.to_string()),
            summary: None,
            report: None,
            error: None,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
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
        record.handoff = Some(brief.to_string());
        record.summary = None;
        record.report = None;
        record.error = None;
        let (title, read_only) = (record.title.clone(), record.read_only);
        self.launch(lane_id, &title, brief, true, read_only);
        Ok(title)
    }

    /// Shared spawn: run the lane on a tokio task and report back over the channel.
    fn launch(&mut self, id: &str, title: &str, brief: &str, resume: bool, read_only: bool) {
        let factory = self.factory.clone().expect("checked by callers");
        let result_tx = self.result_tx.clone();
        let progress_tx = self.progress_tx.clone();
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
                    progress_tx,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                let error = format!(
                    "lane timed out after {} minutes and was aborted — its partial work (if any) \
                     is in the workspace; re-delegate a narrower brief if the task is still needed",
                    LANE_TIMEOUT.as_secs() / 60
                );
                if let Ok(mut log) = crate::lane_log::LaneLog::open(&lane_id) {
                    let _ = log.write_end(&lane_id, "failed", None, Some(&error), None);
                }
                Err(error)
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

    pub fn record_progress(&mut self, progress: &LaneProgress) {
        let Some(record) = self.records.iter_mut().find(|r| r.id == progress.id) else {
            return;
        };
        let activity = LaneActivity {
            at: Utc::now().to_rfc3339(),
            kind: progress.kind.clone(),
            text: progress.text.chars().take(240).collect(),
        };
        record.activity = Some(activity.text.clone());
        record.activity_kind = Some(activity.kind.clone());
        record.activity_at = Some(activity.at.clone());
        record.activity_log.push(activity);
        const MAX_ACTIVITY: usize = 24;
        if record.activity_log.len() > MAX_ACTIVITY {
            let drop_count = record.activity_log.len() - MAX_ACTIVITY;
            record.activity_log.drain(..drop_count);
        }
    }

    /// Relaunch lanes that were running when the parent process stopped.
    pub fn resume_interrupted(&mut self) {
        let ids: Vec<(String, String, bool)> = self
            .records
            .iter()
            .filter(|r| r.status == LaneStatus::Running)
            .map(|r| (r.id.clone(), r.title.clone(), r.read_only))
            .collect();
        for (id, title, read_only) in ids {
            self.record_progress(&LaneProgress {
                id: id.clone(),
                kind: "restart".to_string(),
                text: "resuming from the last saved checkpoint".to_string(),
            });
            let handoff = self
                .records
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.handoff.clone())
                .unwrap_or_else(|| {
                    "Continue the delegated task from the saved lane state.".to_string()
                });
            self.launch(&id, &title, &handoff, true, read_only);
        }
    }

    /// Fold a completed lane's terminal report into its record.
    pub fn record_result(&mut self, result: &LaneResult) {
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == result.id)
        {
            record.status = result.status;
            record.summary = result.summary.clone();
            record.report = result.report.clone();
            record.error = result.error.clone();
            record.finished_at = Some(Utc::now().to_rfc3339());
            let text = match result.status {
                LaneStatus::Completed => "completed",
                LaneStatus::Failed => "failed",
                LaneStatus::Running => "running",
            };
            let activity = LaneActivity {
                at: Utc::now().to_rfc3339(),
                kind: "lifecycle".to_string(),
                text: text.to_string(),
            };
            record.activity = Some(activity.text.clone());
            record.activity_kind = Some(activity.kind.clone());
            record.activity_at = Some(activity.at.clone());
            record.activity_log.push(activity);
            const MAX_ACTIVITY: usize = 24;
            if record.activity_log.len() > MAX_ACTIVITY {
                let drop_count = record.activity_log.len() - MAX_ACTIVITY;
                record.activity_log.drain(..drop_count);
            }
        }
        // Best-effort: drop temp diagnostic JSONL once the lane is terminal.
        if result.status != LaneStatus::Running {
            let _ = LaneLog::cleanup_lane(&result.id);
            // Bound finished-lane growth (TTL + count) and sweep orphan disk state.
            let _ = self.housekeep();
        }
    }

    /// Drop finished lanes past TTL / over the finished-count cap, delete their
    /// on-disk harness state, and sweep orphan `lane-*.json` files under
    /// `lane_root` that are no longer referenced. Never touches Running lanes.
    /// Returns how many finished records were removed from the in-memory list.
    pub fn housekeep(&mut self) -> usize {
        let now = Utc::now();
        let ttl = chrono::Duration::from_std(LANE_FINISHED_TTL)
            .unwrap_or_else(|_| chrono::Duration::days(7));

        let mut finished_idx: Vec<(usize, chrono::DateTime<Utc>)> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            if rec.status == LaneStatus::Running {
                continue;
            }
            let when = rec
                .finished_at
                .as_deref()
                .and_then(parse_rfc3339)
                .or_else(|| parse_rfc3339(&rec.started_at))
                .unwrap_or(now);
            finished_idx.push((i, when));
        }

        // Age-out first.
        let mut drop: std::collections::HashSet<usize> = finished_idx
            .iter()
            .filter(|(_, when)| now.signed_duration_since(*when) > ttl)
            .map(|(i, _)| *i)
            .collect();

        // Then enforce max finished count (oldest first among survivors).
        let mut survivors: Vec<(usize, chrono::DateTime<Utc>)> = finished_idx
            .into_iter()
            .filter(|(i, _)| !drop.contains(i))
            .collect();
        if survivors.len() > MAX_FINISHED_LANES {
            survivors.sort_by_key(|(_, when)| *when); // oldest first
            let excess = survivors.len() - MAX_FINISHED_LANES;
            for (i, _) in survivors.into_iter().take(excess) {
                drop.insert(i);
            }
        }

        if drop.is_empty() {
            // Still sweep orphans (e.g. files left after a crash / older builds).
            self.sweep_orphan_lane_files();
            return 0;
        }

        let removed_ids: Vec<String> = drop
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.id.clone()))
            .collect();
        let mut idxs: Vec<usize> = drop.into_iter().collect();
        idxs.sort_unstable();
        for i in idxs.into_iter().rev() {
            self.records.remove(i);
        }
        for id in &removed_ids {
            self.delete_lane_files(id);
            let _ = LaneLog::cleanup_lane(id);
        }
        self.sweep_orphan_lane_files();
        removed_ids.len()
    }

    fn delete_lane_files(&self, id: &str) {
        if !is_safe_lane_file_id(id) {
            return;
        }
        let base = self.lane_root.join(id);
        let _ = std::fs::remove_file(base.with_extension("json"));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.meta.json", base.display())));
        // Some writers use `lane-N.meta.json` beside `lane-N.json`.
        let _ = std::fs::remove_file(self.lane_root.join(format!("{id}.meta.json")));
        let _ = std::fs::remove_file(self.lane_root.join(format!("{id}.json")));
    }

    /// Remove `lane-*.json` / `lane-*.meta.json` under lane_root that are not
    /// referenced by any current record (including Running).
    fn sweep_orphan_lane_files(&self) {
        let Ok(entries) = std::fs::read_dir(&self.lane_root) else {
            return;
        };
        let keep: std::collections::HashSet<&str> =
            self.records.iter().map(|r| r.id.as_str()).collect();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // lane-12.json / lane-12.meta.json
            let id = if let Some(rest) = name.strip_suffix(".meta.json") {
                rest
            } else if let Some(rest) = name.strip_suffix(".json") {
                rest
            } else {
                continue;
            };
            if !id.starts_with("lane-") || !is_safe_lane_file_id(id) {
                continue;
            }
            if keep.contains(id) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn is_safe_lane_file_id(id: &str) -> bool {
    // lane-<digits> only — never allow path separators or `..`.
    let Some(rest) = id.strip_prefix("lane-") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
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
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
) -> Result<(String, String), String> {
    let mut model = factory();
    let mut log = LaneLog::open(&owner).ok();
    if let Some(log) = log.as_mut() {
        let _ = log.write_start(&owner, &brief, read_only);
    }
    let workspace_for_grounding = workspace_root.clone();
    let context = match ToolContext::with_owner(workspace_root, &owner) {
        Ok(context) => context,
        Err(error) => {
            let message = error.to_string();
            if let Some(log) = log.as_mut() {
                let _ = log.write_end(&owner, "failed", None, Some(&message), None);
            }
            return Err(message);
        }
    };
    let mut tools = coding_tools(
        exa_api_key.clone(),
        crate::memory::MemoryLimits::read_only(),
    );
    if read_only {
        // Investigation lane: strip the file-mutation tools so a fan-out of
        // readers can't collide with the main agent's (or each other's) edits.
        // The shell remains for inspection — the brief tells the lane its role.
        for tool in ["write_file", "edit_file", "append_file"] {
            tools.remove(tool);
        }
    }
    let harness = CodingHarness::new(
        HarnessConfig {
            system_prompt: coding_system_prompt(),
            state_path: Some(state_path),
            resume,
            exa_api_key,
            progress_tx: Some(progress_tx),
            progress_id: Some(owner.clone()),
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
    let outcome = match harness.run(&mut *model, brief).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = error.to_string();
            if let Some(log) = log.as_mut() {
                let _ = log.write_end(&owner, "failed", None, Some(&message), None);
            }
            return Err(message);
        }
    };
    if let Some(log) = log.as_mut() {
        for event in &outcome.events {
            let _ = log.write_event(event);
        }
    }
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
    if let Some(log) = log.as_mut() {
        let _ = log.write_end(
            &owner,
            "completed",
            Some(outcome.iterations),
            None,
            outcome.final_text.as_deref(),
        );
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
        let line: usize = cap
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
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
            HarnessEvent::ToolCall {
                tool_name,
                arguments,
            } => {
                // Track files the lane actually operated on — the concrete results.
                if matches!(
                    tool_name.as_str(),
                    "write_file" | "edit_file" | "append_file"
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
        out.push_str(&format!(
            "\n\nActions taken ({} tool calls):",
            actions.len()
        ));
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
        | "view_outline" | "list_files" => arg("path"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record() -> LaneRecord {
        LaneRecord {
            id: "lane-1".to_string(),
            title: "audit".to_string(),
            status: LaneStatus::Running,
            handoff: Some("inspect the service".to_string()),
            summary: None,
            report: None,
            error: None,
            started_at: "2025-01-01T00:00:00Z".to_string(),
            finished_at: None,
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only: true,
        }
    }

    #[test]
    fn activity_log_is_bounded_and_keeps_latest_entries() {
        let (result_tx, _result_rx) = mpsc::unbounded_channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        let mut manager = LaneManager::new(
            None,
            PathBuf::from("."),
            PathBuf::from("."),
            result_tx,
            progress_tx,
            None,
        )
        .with_records(vec![test_record()]);

        for i in 0..30 {
            manager.record_progress(&LaneProgress {
                id: "lane-1".to_string(),
                kind: "tool_call".to_string(),
                text: format!("running tool {i}"),
            });
        }

        let record = &manager.records()[0];
        assert_eq!(record.activity_log.len(), 24);
        assert_eq!(record.activity_log.first().unwrap().text, "running tool 6");
        assert_eq!(record.activity.as_deref(), Some("running tool 29"));
    }

    #[test]
    fn lane_record_accepts_state_written_before_activity_fields() {
        let mut value = serde_json::to_value(test_record()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("activity");
        object.remove("activity_kind");
        object.remove("activity_at");
        object.remove("activity_log");
        object.remove("read_only");
        let restored: LaneRecord = serde_json::from_value(value).unwrap();
        assert!(restored.activity.is_none());
        assert!(restored.activity_log.is_empty());
        assert!(!restored.read_only);
    }

    fn finished_record(id: &str, finished_at: &str) -> LaneRecord {
        let mut r = test_record();
        r.id = id.to_string();
        r.status = LaneStatus::Completed;
        r.finished_at = Some(finished_at.to_string());
        r.started_at = finished_at.to_string();
        r
    }

    fn empty_manager(lane_root: PathBuf) -> LaneManager {
        let (result_tx, _result_rx) = mpsc::unbounded_channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        LaneManager::new(
            None,
            PathBuf::from("."),
            lane_root,
            result_tx,
            progress_tx,
            None,
        )
    }

    #[test]
    fn housekeep_drops_finished_past_ttl_keeps_running() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-ttl-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = "2015-01-01T00:00:00Z";
        let recent = Utc::now().to_rfc3339();
        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-1", old),
            {
                let mut run = test_record();
                run.id = "lane-2".into();
                run.status = LaneStatus::Running;
                run.finished_at = None;
                run
            },
            finished_record("lane-3", &recent),
        ]);
        // with_records already housekeeps — old finished should be gone.
        let ids: Vec<_> = mgr.records().iter().map(|r| r.id.as_str()).collect();
        assert!(
            !ids.contains(&"lane-1"),
            "ttl-expired finished must drop: {ids:?}"
        );
        assert!(ids.contains(&"lane-2"), "running must stay: {ids:?}");
        assert!(
            ids.contains(&"lane-3"),
            "recent finished must stay: {ids:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn housekeep_enforces_max_finished_count() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-cap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Build MAX_FINISHED_LANES + 5 finished, all "now" so TTL doesn't bite.
        let now = Utc::now();
        let mut recs = Vec::new();
        for i in 1..=(MAX_FINISHED_LANES + 5) {
            // Stagger finished_at so oldest are lane-1..lane-5
            let when = (now - chrono::Duration::seconds(i as i64)).to_rfc3339();
            recs.push(finished_record(&format!("lane-{i}"), &when));
        }
        // One running must survive regardless of count.
        let mut run = test_record();
        run.id = format!("lane-{}", MAX_FINISHED_LANES + 100);
        run.status = LaneStatus::Running;
        recs.push(run);

        let mgr = empty_manager(dir.clone()).with_records(recs);
        let finished: Vec<_> = mgr
            .records()
            .iter()
            .filter(|r| r.status != LaneStatus::Running)
            .collect();
        assert_eq!(finished.len(), MAX_FINISHED_LANES);
        assert!(
            mgr.records()
                .iter()
                .any(|r| r.status == LaneStatus::Running),
            "running lane must be retained"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn housekeep_deletes_disk_files_and_orphans() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Referenced recent finished + orphan file with no record.
        let recent = Utc::now().to_rfc3339();
        std::fs::write(dir.join("lane-1.json"), b"{}").unwrap();
        std::fs::write(dir.join("lane-1.meta.json"), b"{}").unwrap();
        std::fs::write(dir.join("lane-99.json"), b"orphan").unwrap();
        std::fs::write(dir.join("lane-99.meta.json"), b"orphan").unwrap();
        // Ancient finished with files — should drop record + files.
        std::fs::write(dir.join("lane-2.json"), b"old").unwrap();
        std::fs::write(dir.join("lane-2.meta.json"), b"old").unwrap();

        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-1", &recent),
            finished_record("lane-2", "2015-06-01T00:00:00Z"),
        ]);

        let ids: Vec<_> = mgr.records().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["lane-1".to_string()]);
        assert!(dir.join("lane-1.json").exists());
        assert!(
            !dir.join("lane-2.json").exists(),
            "ttl drop must delete files"
        );
        assert!(!dir.join("lane-99.json").exists(), "orphan must be swept");
        assert!(!dir.join("lane-99.meta.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_records_preserves_counter_past_pruned_ids() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-ctr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-40", "2015-01-01T00:00:00Z"), // pruned by TTL
            finished_record("lane-41", &Utc::now().to_rfc3339()),
        ]);
        // Next spawn id should be lane-42, not lane-1.
        assert_eq!(mgr.counter, 41);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
