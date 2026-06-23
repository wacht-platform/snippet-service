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

/// Owns lane lifecycle for one conversation run. Lives in the interactive loop's
/// local scope (not in the immutable `CodingHarness`).
pub struct LaneManager {
    factory: Option<ModelFactory>,
    workspace_root: PathBuf,
    lane_root: PathBuf,
    result_tx: mpsc::UnboundedSender<LaneResult>,
    records: Vec<LaneRecord>,
    counter: usize,
    exa_api_key: Option<String>,
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
        }
    }

    /// Restore prior records (e.g. on resume) so the display reflects history.
    pub fn with_records(mut self, records: Vec<LaneRecord>) -> Self {
        self.counter = records.len();
        self.records = records;
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
    pub fn spawn(&mut self, title: &str, brief: &str) -> Result<String, String> {
        let Some(factory) = self.factory.clone() else {
            return Err(
                "delegate_task is unavailable in this run (no model factory; interactive mode only)."
                    .to_string(),
            );
        };

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
        });

        let result_tx = self.result_tx.clone();
        let workspace_root = self.workspace_root.clone();
        let state_path = self.lane_root.join(format!("{id}.json"));
        let brief = brief.to_string();
        let title = title.to_string();
        let lane_id = id.clone();
        let exa_api_key = self.exa_api_key.clone();

        tokio::spawn(async move {
            let result =
                run_lane(factory, workspace_root, state_path, brief, lane_id.clone(), exa_api_key)
                    .await;
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

        Ok(id)
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

async fn run_lane(
    factory: ModelFactory,
    workspace_root: PathBuf,
    state_path: PathBuf,
    brief: String,
    owner: String,
    exa_api_key: Option<String>,
) -> Result<(String, String), String> {
    let mut model = factory();
    let context = ToolContext::with_owner(workspace_root, owner).map_err(|error| error.to_string())?;
    let harness = CodingHarness::new(
        HarnessConfig {
            system_prompt: coding_system_prompt(),
            state_path: Some(state_path),
            resume: false,
            exa_api_key: exa_api_key.clone(),
            ..HarnessConfig::default()
        },
        coding_tools(exa_api_key),
        context,
    );
    let outcome = harness
        .run(&mut *model, brief)
        .await
        .map_err(|error| error.to_string())?;
    let summary = outcome
        .final_text
        .clone()
        .unwrap_or_else(|| "lane completed without a summary".to_string());
    let report = summarize_lane_outcome(&outcome);
    Ok((summary, report))
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
