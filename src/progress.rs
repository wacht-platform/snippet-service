//! Progress reports from a working session to its agent's inbox.
//!
//! The daemon only decides when one is due. The working session writes it,
//! because it already knows what it did and its cache is warm. The nudge rides
//! on a tool result rather than a reminder because CLI-backed sessions take
//! reminders only at the start of a message, and a result sits at the tail of
//! the transcript, so neither path disturbs the cached prefix.

use std::collections::{HashMap, VecDeque};
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
const RECENT_LABELS: usize = 6;
const LABEL_CHARS: usize = 80;
const INBOX_DIGEST_ROWS: usize = 6;
const INBOX_DIGEST_SCAN: u32 = 30;
const DIGEST_SUMMARY_CHARS: usize = 240;
const LEDGER_LOG_ROWS: u32 = 12;
const LEDGER_LOG_LINES: usize = 6;
const AUTOMATIC_PREFIX: &str = "Automatic, the session wrote no report:";

struct Clock {
    since: Instant,
    tool_calls: u32,
    nudged_at: Option<Instant>,
    recent: VecDeque<String>,
}

impl Clock {
    fn new(now: Instant) -> Self {
        Self { since: now, tool_calls: 0, nudged_at: None, recent: VecDeque::new() }
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

/// What a call says it's doing, for a report the session didn't write itself.
pub fn call_label(arguments: &Value) -> String {
    ["label", "toolSummary"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .chars()
        .take(LABEL_CHARS)
        .collect()
}

static CLOCKS: LazyLock<Mutex<HashMap<String, Clock>>> = LazyLock::new(Default::default);

fn reporting_session(ctx: &ToolContext) -> Option<&str> {
    if ctx.lane_origin().is_some() || ctx.is_mission_control() {
        return None;
    }
    ctx.durable_session_id().filter(|id| !crate::session::is_inbox_session_id(id))
}

/// Counts one finished tool call; returns the note to attach when a report is due.
pub fn note_tool_call(ctx: &ToolContext, tool: &str, label: String) -> Option<String> {
    let session = reporting_session(ctx)?;
    let now = Instant::now();
    let mut clocks = CLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if tool == TOOL_NAME {
        clocks.insert(session.to_string(), Clock::new(now));
        return None;
    }
    let clock = clocks.entry(session.to_string()).or_insert_with(|| Clock::new(now));
    if clock.recent.len() == RECENT_LABELS {
        clock.recent.pop_front();
    }
    clock.recent.push_back(if label.is_empty() { tool.to_string() } else { label });
    let ignored = clock.nudged_at.is_some();
    if !clock.note_call(now) {
        return None;
    }
    // Some models pass over the note, and the inbox must still learn what happened.
    if ignored {
        let summary = format!(
            "{AUTOMATIC_PREFIX} {} tool calls over {} minutes. Recent: {}.",
            clock.tool_calls,
            now.duration_since(clock.since).as_secs() / 60,
            clock.recent.iter().cloned().collect::<Vec<_>>().join("; "),
        );
        clocks.insert(session.to_string(), Clock::new(now));
        drop(clocks);
        if let Err(error) = record_progress(ctx, session, &summary) {
            eprintln!("progress {session}: {error}");
        }
        return None;
    }
    Some(format!(
        "Progress report due: {} tool calls over {} minutes since your last one. Add {TOOL_NAME} to your next batch of tool calls with what you did and why, rather than spending a turn on it.",
        clock.tool_calls,
        now.duration_since(clock.since).as_secs() / 60,
    ))
}

fn open_store(ctx: &ToolContext) -> Option<Store> {
    Store::open_cached(ctx.store_path().unwrap_or_else(crate::store::default_db_path)).ok()
}

/// The agent doing the work here and the task it counts towards. A plain
/// conversation has no bound agent and runs as `snippet`.
pub(crate) fn working_as(ctx: &ToolContext, session: &str) -> (String, Option<String>) {
    if let Some(member) = open_store(ctx).and_then(|db| db.member_by_session(session).ok().flatten()) {
        return (member.agent_id, Some(member.task_id));
    }
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

fn record_progress(ctx: &ToolContext, session: &str, summary: &str) -> Result<String, String> {
    let (agent, task) = working_as(ctx, session);
    let workspace = ctx.workspace_root().display().to_string();
    let db = open_store(ctx).ok_or("open store failed")?;
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
    .map_err(|e| format!("record progress: {e}"))?;
    Ok(agent)
}

#[derive(Deserialize)]
struct ReportArgs {
    summary: String,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    learned: Option<String>,
}

fn report_text(args: &ReportArgs) -> String {
    let mut text = args.summary.trim().to_string();
    for (label, part) in [("Why", &args.why), ("Learned", &args.learned)] {
        if let Some(part) = part.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            text.push_str(&format!("\n{label}: {part}"));
        }
    }
    text
}

pub struct ReportProgress;

#[async_trait]
impl Tool for ReportProgress {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: TOOL_NAME.into(),
            description: "Log what you did and why to your agent board, against this task and folder. It wakes no one, so call it alongside your other tool calls after each meaningful result. Your desk, the next agent on the task and the user read it, and you can read it back with read_coordination_board. A question or blocker doesn't belong here: use send_agent_message for another agent, post_task_coordination for the task's room, ask_user for the user.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "one to three sentences: what you did or found since your last report"},
                    "why": {"type": "string", "description": "why you did it that way, or why it mattered"},
                    "learned": {"type": "string", "description": "what would help you or the next agent on similar work"}
                },
                "required": ["summary"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReportArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.summary.trim().is_empty() {
            return Err(ToolError::msg("summary must not be empty"));
        }
        let summary = report_text(&args);
        let Some(session) = reporting_session(ctx) else {
            return Err(ToolError::msg("only a working session reports progress; a lane reports to the session that started it"));
        };
        let agent = record_progress(ctx, session, &summary).map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({"recorded": true, "agent": agent})))
    }
}

/// The written log lines a ledger reader gets, oldest first. Empty when none.
pub fn ledger_log(db: &Store, task_id: &str) -> String {
    let rows = db.task_log(task_id, None, LEDGER_LOG_ROWS).unwrap_or_default();
    let mut lines: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == BoardEntryKind::Progress.as_str() && !row.summary.starts_with(AUTOMATIC_PREFIX))
        .take(LEDGER_LOG_LINES)
        .map(|row| {
            format!(
                "- {} {}: {}",
                row.created_at.get(..16).unwrap_or(&row.created_at),
                row.agent_id,
                clip_summary(&row.summary),
            )
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    lines.reverse();
    format!("\nprogress log so far (read_coordination_board with task_id for more):\n{}", lines.join("\n"))
}

fn clip_summary(summary: &str) -> String {
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    match summary.char_indices().nth(DIGEST_SUMMARY_CHARS) {
        Some((cut, _)) => format!("{}…", &summary[..cut]),
        None => summary,
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
    format!(
        "- {} {} in {}: {}",
        row.created_at.get(..16).unwrap_or(&row.created_at),
        row.kind,
        row.session_id.as_deref().unwrap_or("no session"),
        clip_summary(&row.summary),
    )
}
