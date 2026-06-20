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

use crate::builtins::coding_tools;
use crate::harness::{CodingHarness, HarnessConfig};
use crate::llm::AgentModel;
use crate::locks::LockRegistry;
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
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// Owns lane lifecycle for one conversation run. Lives in the interactive loop's
/// local scope (not in the immutable `CodingHarness`).
pub struct LaneManager {
    factory: Option<ModelFactory>,
    workspace_root: PathBuf,
    lane_root: PathBuf,
    result_tx: mpsc::UnboundedSender<LaneResult>,
    locks: Option<Arc<LockRegistry>>,
    records: Vec<LaneRecord>,
    counter: usize,
}

impl LaneManager {
    pub fn new(
        factory: Option<ModelFactory>,
        workspace_root: PathBuf,
        lane_root: PathBuf,
        result_tx: mpsc::UnboundedSender<LaneResult>,
        locks: Option<Arc<LockRegistry>>,
    ) -> Self {
        Self {
            factory,
            workspace_root,
            lane_root,
            result_tx,
            locks,
            records: Vec::new(),
            counter: 0,
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
        let locks = self.locks.clone();

        tokio::spawn(async move {
            let result = run_lane(factory, workspace_root, state_path, brief, lane_id.clone(), locks).await;
            let lane_result = match result {
                Ok(summary) => LaneResult {
                    id: lane_id,
                    title,
                    status: LaneStatus::Completed,
                    summary: Some(summary),
                    error: None,
                },
                Err(error) => LaneResult {
                    id: lane_id,
                    title,
                    status: LaneStatus::Failed,
                    summary: None,
                    error: Some(error),
                },
            };
            let _ = result_tx.send(lane_result);
        });

        Ok(id)
    }

    /// Fold a completed lane's terminal report into its record and release its locks.
    pub fn record_result(&mut self, result: &LaneResult) {
        if let Some(record) = self.records.iter_mut().find(|record| record.id == result.id) {
            record.status = result.status;
            record.summary = result.summary.clone();
            record.error = result.error.clone();
            record.finished_at = Some(Utc::now().to_rfc3339());
        }
        if let Some(locks) = &self.locks {
            locks.release_all(&result.id);
        }
    }
}

async fn run_lane(
    factory: ModelFactory,
    workspace_root: PathBuf,
    state_path: PathBuf,
    brief: String,
    owner: String,
    locks: Option<Arc<LockRegistry>>,
) -> Result<String, String> {
    let mut model = factory();
    let context = match locks {
        Some(registry) => ToolContext::with_locks(workspace_root, owner, registry),
        None => ToolContext::new(workspace_root),
    }
    .map_err(|error| error.to_string())?;
    let harness = CodingHarness::new(
        HarnessConfig {
            system_prompt: coding_system_prompt(),
            state_path: Some(state_path),
            resume: false,
            ..HarnessConfig::default()
        },
        coding_tools(),
        context,
    );
    let outcome = harness
        .run(&mut *model, brief)
        .await
        .map_err(|error| error.to_string())?;
    Ok(outcome
        .final_text
        .unwrap_or_else(|| "lane completed without a summary".to_string()))
}
