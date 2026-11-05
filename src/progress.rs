//! Progress reports from a working session to its agent's inbox.
//!
//! The daemon only decides when one is due. The working session writes it,
//! because it already knows what it did and its cache is warm. The nudge rides
//! on a tool result rather than a reminder because CLI-backed sessions take
//! reminders only at the start of a message, and a result sits at the tail of
//! the transcript, so neither path disturbs the cached prefix.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::{BoardEntry, BoardEntryKind, BoardQuery, NewBoardEntry, SNIPPET_AGENT_ID, TaskStatus};
use crate::llm::NativeToolDefinition;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub const TOOL_NAME: &str = "report_progress";
const REPORT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MIN_TOOL_CALLS: u32 = 8;
const INBOX_DIGEST_ROWS: usize = 6;
const INBOX_DIGEST_SCAN: u32 = 30;
const DIGEST_SUMMARY_CHARS: usize = 240;

struct Clock {
    since: Instant,
    tool_calls: u32,
    nudged_at: Option<Instant>,
}

impl Clock {
    fn new(now: Instant) -> Self {
        Self { since: now, tool_calls: 0, nudged_at: None }
    }

    fn note_call(&mut self, now: Instant) -> bool {
        self.tool_calls += 1;
        let due = self.tool_calls >= MIN_TOOL_CALLS
            && now.duration_since(self.since) >= REPORT_INTERVAL
            && self.nudged_at.is_none_or(|at| now.duration_since(at) >= REPORT_INTERVAL);
        if due {
            self.nudged_at = Some(now);
        }
        due
    }
}

static CLOCKS: LazyLock<Mutex<HashMap<String, Clock>>> = LazyLock::new(Default::default);

fn reporting_session(ctx: &ToolContext) -> Option<&str> {
    if ctx.lane_origin().is_some() || ctx.is_mission_control() {
        return None;
    }
    ctx.durable_session_id().filter(|id| !crate::session::is_inbox_session_id(id))
}

/// Counts one finished tool call; returns the note to attach when a report is due.
pub fn note_tool_call(ctx: &ToolContext, tool: &str) -> Option<String> {
    let session = reporting_session(ctx)?;
    let now = Instant::now();
    let mut clocks = CLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if tool == TOOL_NAME {
        clocks.insert(session.to_string(), Clock::new(now));
        return None;
    }
    let clock = clocks.entry(session.to_string()).or_insert_with(|| Clock::new(now));
    if !clock.note_call(now) {
        return None;
    }
    Some(format!(
        "Progress report due: {} tool calls over {} minutes since your last one. Add {TOOL_NAME} to your next batch of tool calls rather than spending a turn on it.",
        clock.tool_calls,
        now.duration_since(clock.since).as_secs() / 60,
    ))
}

fn open_store(ctx: &ToolContext) -> Option<Store> {
    Store::open_cached(ctx.store_path().unwrap_or_else(crate::store::default_db_path)).ok()
}

/// The agent doing the work here and the task it counts towards. A plain
/// conversation has no bound agent and runs as `snippet`.
fn working_as(ctx: &ToolContext, session: &str) -> (String, Option<String>) {
    let task = open_store(ctx).and_then(|db| {
        let task = db
            .list_tasks(Some(session), Some(&TaskStatus::InProgress))
            .ok()?
            .into_iter()
            .find(|t| t.reporting_session.as_deref() == Some(session))?;
        let holder = db
            .list_task_agents(&task.id)
            .ok()?
            .into_iter()
            .find(|m| m.status == "active" && m.removed_at.is_none())
            .map(|m| m.agent_id);
        Some((holder, task.id))
    });
    let (holder, task_id) = task.map_or((None, None), |(holder, id)| (holder, Some(id)));
    let agent = holder
        .or_else(|| ctx.agent_id().map(str::to_string))
        .unwrap_or_else(|| SNIPPET_AGENT_ID.to_string());
    (agent, task_id)
}

#[derive(Deserialize)]
struct ReportArgs {
    summary: String,
}

pub struct ReportProgress;

#[async_trait]
impl Tool for ReportProgress {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: TOOL_NAME.into(),
            description: "Tell your own inbox what this session has done so far. It records the report and wakes no one, so call it alongside your next tool calls when a progress report falls due. A question or blocker doesn't belong here: use send_agent_message for another agent, post_task_coordination for the task's room, ask_user for the user.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "one to three sentences: what changed or what you found since your last report, and what comes next"}
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReportArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let summary = args.summary.trim();
        if summary.is_empty() {
            return Err(ToolError::msg("summary must not be empty"));
        }
        let Some(session) = reporting_session(ctx) else {
            return Err(ToolError::msg("only a working session reports progress; a lane reports to the session that started it"));
        };
        let (agent, task) = working_as(ctx, session);
        let workspace = ctx.workspace_root().display().to_string();
        let db = open_store(ctx).ok_or_else(|| ToolError::msg("open store failed"))?;
        let now = chrono::Utc::now().to_rfc3339();
        db.record_board_entry(
            &agent,
            BoardEntryKind::Progress,
            &NewBoardEntry {
                session_id: Some(session),
                workspace: Some(&workspace),
                summary,
                correlation_id: task.as_deref(),
                created_at: &now,
            },
        )
        .map_err(|e| ToolError::msg(format!("record progress: {e}")))?;
        Ok(ToolResult::success(json!({"recorded": true, "agent": agent})))
    }
}

/// What an inbox reads about its agent's sessions, in its own tail reminder.
pub fn inbox_digest(db: &Store, agent: &str) -> String {
    let rows = db.read_board(agent, &BoardQuery::default(), INBOX_DIGEST_SCAN).unwrap_or_default();
    let lines: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == BoardEntryKind::Progress.as_str() || row.kind == BoardEntryKind::Reported.as_str())
        .take(INBOX_DIGEST_ROWS)
        .map(digest_line)
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "What your sessions reported most recently (read_coordination_board for more):\n{}",
        lines.join("\n")
    )
}

fn digest_line(row: &BoardEntry) -> String {
    let summary = row.summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary = match summary.char_indices().nth(DIGEST_SUMMARY_CHARS) {
        Some((cut, _)) => format!("{}…", &summary[..cut]),
        None => summary,
    };
    format!(
        "- {} {} in {}: {summary}",
        row.created_at.get(..16).unwrap_or(&row.created_at),
        row.kind,
        row.session_id.as_deref().unwrap_or("no session"),
    )
}
