use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::inline::{extract_inline_tool_submissions, looks_like_inline_tool_submission};
use crate::lanes::{LaneManager, LaneRecord, LaneResult, LaneStatus, ModelFactory};
use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, NativeToolDefinition, StreamBuffer, StreamHandle,
};
use crate::meta::{self, parse_ask_user, parse_delegate_brief};
use crate::prompts::coding_system_prompt;
use crate::shell_guard::{ShellVerdict, classify_shell_command};
use crate::signals::RuntimeSignal;
use crate::tools::{ToolContext, ToolError, ToolRegistry};
use crate::watches::{WatchEvent, WatchManager, WatchRecord};

/// Consecutive tool-call turns with no real work before the run is wrapped up.
const MAX_UNPRODUCTIVE_TURNS: usize = 10;

/// Consecutive plan-only turns before raising a `PlanOnly` nudge.
const PLAN_LOOP_AT: usize = 3;
/// Tool-call turns without a plan update, while steps are unfinished, before a
/// reminder to bring the plan up to date.
const PLAN_STALE_AFTER: u64 = 10;
/// Reads of the same unchanged file in one request before a reminder.
const REPEATED_READ_AT: usize = 3;
/// Consecutive tool-call turns without a word before asking for a progress note.
const SILENT_RUN_AT: usize = 5;
/// File-change calls with nothing run in between before asking for a check.
const UNVERIFIED_EDITS_AT: usize = 4;
/// Whole-file rewrites of the same path in one request before a reminder.
const REWRITE_AT: usize = 3;

/// A single-turn tool batch this large raises `BatchBackpressure`.
const LARGE_TOOL_BATCH: usize = 10;

/// Read-only tools whose exact-duplicate re-call within a request is wasteful
/// spinning (the result is already in history). A write to memory clears it.

/// Tools that change the workspace: manual approval gates them, and a
/// successful one invalidates the dedup set.
const MUTATING_TOOLS: [&str; 2] = ["change_files", "bash"];

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Runaway backstop for the one-shot / lane loop (the interactive loop is
    /// unbounded). High so deep, many-step work is never cut short — it only trips
    /// on a genuine runaway.
    pub runtime_backstop_iterations: usize,
    pub system_prompt: String,
    pub state_path: Option<PathBuf>,
    pub resume: bool,
    /// Consecutive model-call failures tolerated before giving up. `0` fails on
    /// the first error (used by one-shot tests).
    pub max_consecutive_recovery: usize,
    pub recovery_base_ms: u64,
    pub recovery_max_ms: u64,
    /// Exa API key, propagated to delegated lanes so their tool set matches the
    /// main agent's (web_search enabled only when set).
    pub exa_api_key: Option<String>,
    /// Configured model context window for this run; used by compaction gates.
    pub context_window_tokens: u64,
    /// Full history rewrite (agentic table) when prompt usage hits this % of window.
    pub compact_at_pct: u8,
    /// Cheap tool-body prune starts at this % of the window (no model call).
    pub tool_prune_at_pct: u8,
    /// When prune fires, stub tools only in the oldest prefix whose estimated
    /// size is this % of the context window (default first 40%).
    pub tool_prune_prefix_pct: u8,
    /// Start fresh runs in manual approval mode (bash + file edits wait for y/n).
    pub manual_approval: bool,
    /// Inject the `[memory]` block (rules, learnings, notes table of contents)
    /// into the system prefix each session.
    pub memory_enabled: bool,
    /// Run the reflection pass after a request that did real work (main session only).
    pub memory_reflect: bool,
    /// Optional live progress sink used by delegated lanes. Progress is operational
    /// status only (tool names/paths, never prompts) and is not user-addressable.
    pub progress_tx: Option<mpsc::UnboundedSender<crate::lanes::LaneProgress>>,
    pub progress_id: Option<String>,
    /// Mission Control coordinates durable sessions rather than spawning lanes.
    pub allow_lane_control: bool,
    pub lane_approval: Option<crate::lanes::LaneApprovalRoute>,
    pub hidden_meta: &'static [&'static str],
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            runtime_backstop_iterations: 1000,
            system_prompt: coding_system_prompt(),
            state_path: None,
            resume: false,
            max_consecutive_recovery: 8,
            recovery_base_ms: 1_000,
            recovery_max_ms: 30_000,
            exa_api_key: None,
            context_window_tokens: 128_000,
            compact_at_pct: 90,
            tool_prune_at_pct: 75,
            tool_prune_prefix_pct: 40,
            manual_approval: false,
            memory_enabled: true,
            memory_reflect: true,
            progress_tx: None,
            progress_id: None,
            allow_lane_control: true,
            lane_approval: None,
            hidden_meta: &[],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEvent {
    UserInput {
        text: String,
    },
    /// A mid-run user message injected while the agent was working (steering).
    Steer {
        text: String,
    },
    AssistantText {
        text: String,
    },
    /// The agent replaced its visible plan.
    PlanUpdated {
        steps: Vec<PlanStep>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    /// A direct message between this session and an agent.
    ///
    /// Recorded in BOTH directions so the transcript shows the exchange: what
    /// this session sent out, and what came back. Without the outbound record a
    /// reply would appear from nowhere with no indication of what was asked.
    AgentMessage {
        /// The agent on the other side.
        agent_id: String,
        body: String,
        /// True when this session sent it; false when the agent replied.
        outbound: bool,
    },
    /// Someone other than this session dispatched a task, recorded here so the
    /// coordinator's transcript shows what was sent out on its behalf.
    ///
    /// A NOTICE, never a wake: the work is already routed to a worker, so waking
    /// Mission Control would spend a turn on something that needs no decision.
    TaskDispatched {
        task_id: String,
        title: String,
        /// The session the work was routed to.
        session_id: String,
        /// Who dispatched it — `human` for a user filing work directly.
        by: String,
    },
    /// The agent presented a file to the user (an openable card in the UIs).
    FilePresented {
        path: String,
        caption: Option<String>,
    },
    /// A runtime-injected correction after recoverable failures.
    SystemDecision {
        step: String,
        reasoning: String,
    },
    ModelError {
        message: String,
    },
    /// The agent asked the user a question and the turn is paused.
    UserQuestion {
        questions: Value,
    },
    /// In manual mode, a mutating tool is awaiting approval. `index`/`total` track
    /// position within a batch of tool calls so the UI can show "action 2 of 3".
    ApprovalRequest {
        tool_name: String,
        summary: String,
        index: usize,
        total: usize,
    },
    /// A delegated lane was started.
    LaneSpawned {
        id: String,
        title: String,
    },
    LaneCancelled {
        id: String,
        title: String,
        reason: String,
    },
    /// A delegated lane reported back.
    LaneCompleted {
        id: String,
        title: String,
        status: LaneStatus,
        summary: Option<String>,
    },
    ToolCall {
        tool_name: String,
        arguments: Value,
    },
    ToolResult {
        tool_name: String,
        result: Value,
    },
    InvalidToolCall {
        tool_name: String,
        error: String,
    },
    /// An event kind this build no longer produces (e.g. the retired `note`),
    /// read from an older session and ignored.
    #[serde(other)]
    Retired,
}

/// One step of the agent's visible plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessOutcome {
    pub final_text: Option<String>,
    pub events: Vec<HarnessEvent>,
    pub iterations: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HarnessStatus {
    /// No active turn; awaiting the next user input or lane report.
    #[default]
    Idle,
    Running,
    /// Paused on an `ask_user` question; awaiting the user's answer.
    WaitingForInput,
    /// The user cancelled the run.
    Interrupted,
    /// One-shot run finished via `complete`.
    Completed,
    Failed,
}

/// Whether mutating tools (writes / shell) run freely or require per-call approval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Mutating tools run without prompting (current behavior).
    #[default]
    Auto,
    /// Each mutating tool call pauses for the user's approval.
    Manual,
}

/// A user's decision on a pending approval, delivered to the in-flight step.
#[derive(Debug, Clone, Copy)]
pub enum ApprovalDecision {
    Approve,
    /// Approve this call and switch to Auto for the rest of the run.
    ApproveAll,
    Deny,
}

/// An active `/goal`: the agent auto-continues toward it each idle turn (the loop
/// re-prompts it as the user) until it's complete, the user cancels, or a rate
/// limit is hit. It keeps its plans/artifacts/findings in `dir` under
/// `.snippet/goals/…`. Persisted with the session so it survives restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    /// What the user asked the agent to accomplish.
    pub text: String,
    /// The agent's goal workspace dir (it picks/creates one under
    /// `.snippet/goals/…`); empty until the agent has chosen it.
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub status: GoalStatus,
    /// Autonomous (loop-driven, non-user) turns taken toward the goal — drives the
    /// periodic self-evaluation checkpoint.
    #[serde(default)]
    pub autonomous_turns: usize,
    /// When Paused by a rate limit, the unix epoch seconds the window resets (0 if
    /// unknown) — shown to the user so they know when it can resume.
    #[serde(default)]
    pub resume_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The agent is actively driving toward the goal (auto-continues).
    #[default]
    Active,
    /// Rate-limited — the loop stopped auto-continuing until the window resets.
    Paused,
    /// The agent reported the goal 100% done.
    Complete,
    /// The user cancelled it.
    Cancelled,
}

/// Autonomous goal turns between forced self-evaluations — the runaway guard for
/// providers with no rate limit: the agent steps back, checks progress, and
/// decides for itself whether to keep going.
const GOAL_SELF_CHECK_EVERY: usize = 300;

/// Where the agent keeps its goal scratch space, phrased for a directive.
fn goal_dir_phrase(dir: &str) -> String {
    if dir.trim().is_empty() {
        "your goal workspace under `.snippet/goals/`".to_string()
    } else {
        format!("your goal workspace (`{dir}`)")
    }
}

fn goal_start_directive(text: &str) -> String {
    format!(
        "[goal] The user has given you a goal to carry through on your own: {text}\n\n\
You'll get a nudge after each turn to keep going, so work through it end to end without \
checking in on ordinary steps.\n\
1. Pick a goal folder under `.snippet/goals/` (a new one, or a relevant existing one). It \
survives compaction, so keep your plan, findings, decisions and artifacts there.\n\
2. Lay out the steps with `update_plan` and save the plan in that folder too.\n\
3. Start on the first step.\n\n\
When the goal is fully done, call `complete_goal` with a short summary. If you hit something \
only the user can resolve, say exactly what you need."
    )
}

fn goal_continue_directive(text: &str, dir: &str) -> String {
    format!(
        "[goal] Carry on with your goal: {text}\n\
Your plan and notes are in {where}. Pick up the next unfinished step and do it, keeping the \
plan current as you go; no recap needed unless something material changed. When it's fully \
done, call `complete_goal`.",
        where = goal_dir_phrase(dir)
    )
}

fn goal_selfcheck_directive(text: &str, dir: &str, n: usize) -> String {
    format!(
        "[goal] Time for a check-in: you've taken {n} turns on this goal: {text}\n\
Re-read your plan in {where} and be honest with yourself: is this making real progress, or \
going in circles? If it's done, call `complete_goal`. If you're blocked on something only the \
user can give, say exactly what. Otherwise adjust course if needed and carry on.",
        where = goal_dir_phrase(dir)
    )
}

fn goal_cancel_directive(text: &str, dir: &str) -> String {
    format!(
        "[goal] The user has CANCELLED this goal. STOP working toward it now. Give a brief summary of \
    what you accomplished and where you left off (your work is saved in {where}). Take no further \
    action on the goal: {text}",
        where = goal_dir_phrase(dir)
    )
}

/// Whether a model error is a rate/usage limit (429) — matches the humanized
/// "Rate limited …" message plus a couple of raw fallbacks.
fn is_rate_limit_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    message.starts_with("Rate limited") || m.contains("rate limit") || m.contains("429")
}

/// The soonest window reset (unix epoch secs) across a rate-limit snapshot, or None.
fn earliest_reset(snap: &crate::llm::RateLimitSnapshot) -> Option<i64> {
    [snap.primary.as_ref(), snap.secondary.as_ref()]
        .into_iter()
        .flatten()
        .map(|w| w.resets_at)
        .filter(|&t| t > 0)
        .min()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedInput {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HarnessState {
    pub version: u32,
    pub status: HarnessStatus,
    pub created_at: String,
    pub updated_at: String,
    /// Absolute workspace folder this session runs in (for the serve daemon's
    /// device-wide session list). Empty on states from before this field existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    /// The session title. For a new session this is seeded from its first request;
    /// renaming replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Legacy migration input only. Old state files stored the first request under
    /// this key; it is accepted when reading and never written back.
    #[serde(rename = "user_request", default, skip_serializing)]
    legacy_request: String,
    /// An active `/goal` the agent is autonomously working toward (None = normal
    /// interactive mode). See [`Goal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    /// True while a history-compaction pass is running (manual or auto). Surfaced
    /// so the UI can show a distinct "Compacting…" state instead of a generic
    /// "Running" — compaction can take a while and otherwise looks like a normal turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacting: bool,
    /// When the current running turn began (RFC3339). Cleared when not running
    /// so attached UIs can tick elapsed from the event, not from widget mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<String>,
    /// When the current compaction pass began (RFC3339). Cleared when idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting_started_at: Option<String>,
    /// Omitted on the attach wire (clients render from `events` + live stream).
    /// Default so snapshot/delta frames that strip LLM history still deserialize.
    #[serde(default)]
    pub messages: Vec<HarnessMessage>,
    /// Present on full snapshots; deltas send `new_events` instead and omit this.
    #[serde(default)]
    pub events: Vec<HarnessEvent>,
    pub iterations: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    /// Background delegated lanes (snapshot for display + resume).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<LaneRecord>,
    /// Active file watches (`monitor` meta-tool) — snapshot for display + resume.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<WatchRecord>,
    /// The currently pending `ask_user` question set, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_question: Option<Value>,
    /// Auto (run mutating tools freely) vs Manual (per-call approval).
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    /// Cumulative model token usage for this session (across all turns).
    #[serde(default)]
    pub total_tokens: u64,
    /// Cumulative prompt (input) tokens sent to the model this session.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Cumulative completion (output) tokens received this session.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Prompt tokens of the most recent request (current context fill).
    #[serde(default)]
    pub last_prompt_tokens: u64,
    /// Cumulative prompt tokens served from the provider's cache this session.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Working-tree checkpoints taken before each turn (newest last), for `/rewind`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointRecord>,
    /// Latest ChatGPT-subscription rate-limit usage, for the footer (None otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::llm::RateLimitSnapshot>,
    /// The model's context window in tokens, for the usage gauge (0 = unknown).
    #[serde(default)]
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tool_payloads_pruned: bool,
    /// Messages typed while a run is in progress — held until the turn ends
    /// (idle / interrupted), then submitted as the next turn. Shared across
    /// TUI and app clients so every attach sees the same queue. Always serialized
    /// (even empty) so a flush clears every attached client instead of leaving
    /// a stale hold list.
    #[serde(default)]
    pub queued_inputs: Vec<QueuedInput>,
    /// The agent's current plan, kept with `update_plan` and shown to the user
    /// as a checklist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<PlanStep>,
    /// Completed history compactions. Checkpoints record it so a rewind can
    /// tell whether their message position still exists.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compactions: u64,
    /// Set by the few writers that rewrite history in place — compaction,
    /// checkpoint rewind, interrupt rollback, tool-payload pruning. Those are the
    /// only cases where the append-only store cannot be appended to, so the flag
    /// is what lets a persist be an append the rest of the time. Never
    /// serialized: it describes the pending write, not the session.
    #[serde(skip)]
    pub history_rewritten: bool,
    /// Set only when existing events change or disappear (checkpoint rewind).
    /// Compaction and interrupt repair leave earlier events intact, so they
    /// must not force a full rewrite of the event log.
    #[serde(skip)]
    pub events_rewritten: bool,
}

impl HarnessState {
    /// Idle persisted conversation with no turns — used when Mission Control
    /// opens a new chat so it can dispatch into it.
    pub fn blank(workspace: impl Into<String>, title: Option<String>) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            version: 1,
            status: HarnessStatus::Idle,
            created_at: now.clone(),
            updated_at: now,
            workspace: workspace.into(),
            title,
            ..Self::default()
        }
    }

    /// Truncate events/messages/checkpoints to a checkpoint boundary.
    /// Returns the checkpoint label on success.
    pub fn apply_checkpoint_rewind(&mut self, checkpoint_id: &str) -> Result<String, String> {
        let cp = self
            .checkpoints
            .iter()
            .find(|c| c.id == checkpoint_id || c.id.starts_with(checkpoint_id))
            .cloned()
            .ok_or_else(|| format!("checkpoint not found: {checkpoint_id}"))?;
        let event_index = cp.event_index.min(self.events.len());
        self.events.truncate(event_index);
        if cp.compactions == self.compactions {
            let keep_system = usize::from(matches!(self.messages.first(), Some(HarnessMessage::System { .. })));
            let message_index = cp.message_index.min(self.messages.len()).max(keep_system);
            self.messages.truncate(message_index);
        } else {
            self.messages.push(HarnessMessage::System {
                content: format!(
                    "[rewind] The user rewound this session to before their message \"{}\". \
                     Everything after that point was undone, including file changes. Your \
                     context summary still describes some of that later work: treat it as \
                     not having happened.",
                    cp.label
                ),
            });
        }
        // A rewind moves history backwards, which an append-only store cannot
        // express — mark the pending write as a full replace.
        self.history_rewritten = true;
        self.events_rewritten = true;
        self.checkpoints.retain(|c| c.event_index <= event_index);
        self.final_text = None;
        self.pending_question = None;
        self.compacting = false;
        self.compacting_started_at = None;
        self.turn_started_at = None;
        self.tool_payloads_pruned = false;
        self.queued_inputs.clear();
        self.status = HarnessStatus::Idle;
        Ok(cp.label)
    }

    /// Return the original request for model-context features without making it
    /// part of the session metadata or public wire model.
    pub fn initial_request(&self) -> Option<&str> {
        self.messages
            .iter()
            .find_map(|message| match message {
                HarnessMessage::User { content } => Some(content.as_str()),
                _ => None,
            })
            .or_else(|| {
                (!self.legacy_request.trim().is_empty()).then_some(self.legacy_request.as_str())
            })
    }
}

fn normalize_state_title(state: &mut HarnessState) {
    let legacy_request = state.legacy_request.trim().to_string();
    // Very old states carried the first prompt only in `user_request`. Preserve
    // that conversational context before dropping the metadata field. Current
    // states already have the prompt in `messages`, so this is a no-op for them.
    if !legacy_request.is_empty()
        && !state
            .messages
            .iter()
            .any(|message| matches!(message, HarnessMessage::User { .. }))
    {
        state.messages.push(HarnessMessage::User {
            content: legacy_request.clone(),
        });
    }
    let title_is_empty = state
        .title
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty);
    if title_is_empty {
        if let Some(request) = state
            .initial_request()
            .filter(|text| !text.trim().is_empty())
        {
            state.title = Some(request.to_string());
        }
    }
    // The legacy field is an input-only migration bridge. Once its value has
    // seeded the title/context, drop it so every subsequent persist emits the
    // clean schema.
    state.legacy_request.clear();
}

/// A working-tree snapshot the user can rewind to (a commit in the shadow repo).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointRecord {
    /// Shadow-repo commit id.
    pub id: String,
    /// The user prompt this checkpoint was taken before (truncated).
    pub label: String,
    pub created_at: String,
    /// Event count when checkpoint was taken — for truncating history on rewind.
    #[serde(default)]
    pub event_index: usize,
    /// Message count when checkpoint was taken — for truncating conversation on rewind.
    #[serde(default)]
    pub message_index: usize,
    /// `HarnessState::compactions` when the checkpoint was taken.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compactions: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Inputs the interactive driver receives from its UI (or, headless, over the
/// wire — hence `Serialize`/`Deserialize`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum LoopInput {
    /// A new user message or a mid-run steer.
    UserMessage(String),
    /// An answer to a pending `ask_user` question.
    Answer(String),
    /// Request a manual history compaction pass.
    Compact,
    /// Approve / deny the currently pending mutating tool call (manual mode).
    Approve,
    /// Approve the pending call and switch to Auto for the rest of the run.
    ApproveAll,
    Deny,
    /// Switch between Auto and Manual (approval) mode.
    SetMode(ApprovalMode),
    /// Hold a message until the current run ends (idle / interrupted). Shown on
    /// every attached client. Does not steer the in-flight turn.
    Queue(QueuedInput),
    /// Drop one held message by its stable ID.
    Unqueue(String),
    /// Send one held message now as a mid-run steer and remove it by ID.
    SteerQueued(String),
    /// Drop every held message (`queued_inputs`) and anything buffered mid-step.
    DropQueued,
    /// Rename the session (user-set title override).
    SetTitle(String),
    /// Set (or replace) the autonomous `/goal` — the agent begins driving toward it.
    SetGoal(String),
    /// Resume a paused rate-limited goal without replacing its text.
    ResumeGoal,
    /// Cancel the active goal — the agent is told and winds down.
    CancelGoal,
    /// Cancel the run.
    Interrupt,
    /// Record an event WITHOUT starting a turn.
    ///
    /// For information addressed to the session that the session's agent must not
    /// be made to act on: a reply from another agent, or the record of a message
    /// this session sent out. `UserMessage` would wake the agent and spend a turn
    /// on a notice — this records it and leaves the loop parked.
    Notice(HarnessEvent),
    /// Rewind to a checkpoint — truncate events and checkpoints to that point.
    Rewind {
        checkpoint: String,
    },
}

// --- Runtime corrections ---

#[derive(Debug, Clone, Copy)]
enum RuntimeCorrectionKind {
    LlmRequestFailed,
}

impl RuntimeCorrectionKind {
    fn step(self) -> &'static str {
        match self {
            Self::LlmRequestFailed => "llm_request_failed",
        }
    }

    fn reasoning(self) -> &'static str {
        match self {
            Self::LlmRequestFailed => {
                "The previous model request failed repeatedly before a valid response was produced. \
                 Retry the same turn with the existing context; if it keeps failing, simplify the \
                 next step."
            }
        }
    }
}

enum RecoveryAction {
    Retry,
    GiveUp,
}

// --- Per-turn driver bookkeeping ---

#[derive(Default)]
struct LoopVars {
    /// Signals raised this turn, delivered in the next turn's reminder.
    pending_signals: Vec<RuntimeSignal>,
    /// Signature of the previous turn's tool calls, for loop detection.
    last_tool_signature: Option<String>,
    /// How many turns the same tool-call signature has repeated.
    repeated_tool_count: usize,
    /// Recent tool-call signatures (windowed), to catch a repeated call even when
    /// other calls are interleaved between the repeats.
    recent_tool_signatures: std::collections::VecDeque<String>,
    /// Consecutive shell-discipline nudges, for escalation.
    shell_nudge_count: usize,
    /// Consecutive plan-only turns (plan updates with no real work).
    consecutive_plan_count: usize,
    /// Tool-call turns since the plan was last updated.
    turns_since_plan: u64,
    /// Files read this request: content fingerprint and how many times it was
    /// read unchanged, to catch re-reading the same file.
    file_reads: std::collections::HashMap<std::path::PathBuf, (u64, usize)>,
    /// Consecutive tool-call turns without any text from the model.
    silent_turns: usize,
    /// Whole-file rewrites (`create` with `overwrite`) per path this request.
    rewrites: std::collections::HashMap<String, usize>,
    /// Successful file-change calls since the last command that wasn't a read.
    edits_since_check: usize,
    /// Consecutive tool-call turns that did no real work (plan-only / unknown tools).
    unproductive_turns: usize,
    /// Consecutive turns in which EVERY executed tool call failed — the approach
    /// isn't working; escalates to a re-think-or-ask-for-help nudge.
    consecutive_failed_turns: usize,
    /// What the model was last told about each section of volatile state, so a
    /// reminder carries only what changed.
    reminded: std::collections::HashMap<&'static str, String>,
    /// `HarnessState::compactions` when `reminded` was last valid; compaction
    /// drops the old reminders from history, so everything is told again.
    reminded_compactions: u64,
    /// Output fingerprint of each bash command run during this request, so an
    /// identical rerun returns a short notice instead of the same output again.
    bash_outputs: std::collections::HashMap<String, u64>,
    /// Empty completions (no reply at all) re-prompted
    /// this response cycle. Capped so we ask for an answer without looping forever.
    /// Reset on a new user message.
    empty_reply_reprompts: usize,
    /// Turns spent on the CURRENT request (a soft budget surfaced each turn so the
    /// agent converges instead of sprawling). Reset on a new user request.
    turns_this_request: u64,
    /// Consecutive failed edits on the same file.
    consecutive_failed_edits: usize,
    /// Path of the file whose edit recently failed.
    last_failed_edit_path: Option<String>,
    /// Workspace snapshot for the current request, still being taken off the
    /// runtime. Finished before any tool runs so a rewind restores the files
    /// exactly as they were when the request arrived.
    pending_checkpoint: Option<PendingCheckpoint>,
}

struct PendingCheckpoint {
    snapshot: tokio::task::JoinHandle<Result<String, String>>,
    label: String,
    created_at: String,
    event_index: usize,
    message_index: usize,
    compactions: u64,
}

/// What a single model step resolved to.
enum StepResult {
    Continue,
    TurnEnded {
        kind: TurnEndKind,
        final_text: Option<String>,
    },
    /// The model request failed. `retryable` is false for fatal errors
    /// (auth/permission/not-found/bad-request) so the loop gives up at once
    /// instead of re-running the whole step and flooding the transcript.
    ModelError {
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum TurnEndKind {
    Complete,
    Ask,
}

/// Outcome of dispatching one meta tool.
enum MetaControl {
    Continue,
    EndTurn {
        kind: TurnEndKind,
        final_text: Option<String>,
    },
}

pub struct CodingHarness {
    pub(super) config: HarnessConfig,
    pub(super) tools: ToolRegistry,
    pub(super) context: ToolContext,
    /// Ordinals already durable, so a persist can append the tail instead of
    /// rewriting the transcript.
    pub(super) written_messages: std::sync::atomic::AtomicUsize,
    pub(super) written_events: std::sync::atomic::AtomicUsize,
}



mod cli_agent;
mod compactor;
mod dispatch;
mod events;
mod guards;
mod live_context;
mod memory_reflection;
mod meta_tools;
mod persistence;
mod prompts;
mod runner;
pub mod state;
mod step;
mod transcript;

use guards::*;
use live_context::*;
use prompts::*;
pub use cli_agent::{CliAgentModel, claude_rate_limits, cli_agent_status};
pub use state::*;
use transcript::*;

pub(crate) use transcript::notice_text;

#[cfg(test)]
mod tests;

pub(crate) fn is_daemon_envelope(text: &str) -> bool {
    let first = text.lines().next().unwrap_or("").trim();
    first.len() > 2
        && first.starts_with('[')
        && first.ends_with(']')
        && first[1..first.len() - 1].chars().all(|c| c.is_ascii_lowercase() || c == '_')
}
