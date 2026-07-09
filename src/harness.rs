use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::inline::{extract_inline_tool_submissions, looks_like_inline_tool_submission};
use crate::lanes::{LaneManager, LaneRecord, LaneResult, LaneStatus, ModelFactory};
use crate::llm::{AgentModel, GeneratedToolCall, HarnessMessage, StreamBuffer, StreamHandle};
use crate::meta::{self, parse_ask_user, parse_delegate_brief};
use crate::prompts::coding_system_prompt;
use crate::signals::RuntimeSignal;
use crate::shell_guard::{ShellVerdict, classify_shell_command};
use crate::tools::{ToolContext, ToolError, ToolRegistry};
use crate::watches::{WatchEvent, WatchManager, WatchRecord};

/// How many tool-only steps may pass before the agent is nudged to emit a
/// user-visible progress line. Ported from wacht's `STEER_VISIBILITY_NUDGE_WINDOW`.
const VISIBILITY_NUDGE_WINDOW: usize = 4;

/// Consecutive tool-call turns with no real work before the run is wrapped up.
/// Ported from wacht's `MAX_UNPRODUCTIVE_TURNS`.
const MAX_UNPRODUCTIVE_TURNS: usize = 4;

/// Consecutive note-only turns before raising a `NoteLoop` nudge.
const NOTE_LOOP_AT: usize = 3;

/// A single-turn tool batch this large raises `BatchBackpressure`. Ported from
/// wacht's `LARGE_TOOL_BATCH`.
const LARGE_TOOL_BATCH: usize = 10;

/// The second consecutive shell-discipline nudge escalates to reflect-and-switch.
/// Ported from wacht's `SHELL_NUDGE_ESCALATE_AT`.
const SHELL_NUDGE_ESCALATE_AT: usize = 2;


/// Read-only discovery tools whose exact-duplicate re-call within a request is
/// wasteful spinning (the result is already in history). `read_file` is excluded
/// — re-reading after an edit is legitimate.
const DEDUP_TOOLS: [&str; 4] = ["list_files", "search_content", "search_files", "view_outline"];

/// Tools that change the workspace; running one invalidates the dedup set so
/// re-discovery afterward is allowed.
const MUTATING_TOOLS: [&str; 5] = [
    "write_file",
    "edit_file",
    "append_file",
    "replace_file_content",
    "bash",
];

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
    /// the first error (used by one-shot tests). Ported from
    /// `MAX_CONSECUTIVE_RECOVERY_ATTEMPTS`.
    pub max_consecutive_recovery: usize,
    pub recovery_base_ms: u64,
    pub recovery_max_ms: u64,
    /// Exa API key, propagated to delegated lanes so their tool set matches the
    /// main agent's (web_search enabled only when set).
    pub exa_api_key: Option<String>,
    /// Configured model context window for this run; used by compaction gates.
    pub context_window_tokens: u64,
    /// Start compaction when the largest observed prompt reaches this percentage
    /// of `context_window_tokens`.
    pub compact_at_pct: u8,
    /// Start fresh runs in manual approval mode (bash + file edits wait for y/n).
    pub manual_approval: bool,
    /// Per-workspace memory: inject the `[workspace_memory]` index into the system
    /// prefix each session and offer the memory tools.
    pub memory_enabled: bool,
    pub memory_index_budget_chars: usize,
    pub memory_entry_budget_chars: usize,
    pub memory_max_entries: usize,
    /// Run a bounded learning/reflection pass during compaction (main session only).
    pub memory_reflect_on_compaction: bool,
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
            manual_approval: false,
            memory_enabled: true,
            memory_index_budget_chars: 5_000,
            memory_entry_budget_chars: 12_000,
            memory_max_entries: 128,
            memory_reflect_on_compaction: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessEvent {
    UserInput { text: String },
    /// A mid-run user message injected while the agent was working (steering).
    Steer { text: String },
    AssistantText { text: String },
    /// A private note-to-self the agent recorded.
    Note { entry: String },
    /// A runtime-injected correction after recoverable failures.
    SystemDecision { step: String, reasoning: String },
    ModelError { message: String },
    /// The agent asked the user a question and the turn is paused.
    UserQuestion { questions: Value },
    /// In manual mode, a mutating tool is awaiting approval. `index`/`total` track
    /// position within a batch of tool calls so the UI can show "action 2 of 3".
    ApprovalRequest {
        tool_name: String,
        summary: String,
        index: usize,
        total: usize,
    },
    /// A delegated lane was started.
    LaneSpawned { id: String, title: String },
    /// A delegated lane reported back.
    LaneCompleted {
        id: String,
        title: String,
        status: LaneStatus,
        summary: Option<String>,
    },
    ToolCall { tool_name: String, arguments: Value },
    ToolResult { tool_name: String, result: Value },
    InvalidToolCall { tool_name: String, error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessOutcome {
    pub final_text: Option<String>,
    pub events: Vec<HarnessEvent>,
    pub iterations: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessStatus {
    /// No active turn; awaiting the next user input or lane report.
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
        "[goal] The user has set you an AUTONOMOUS GOAL to drive to completion on your own — \
you'll be re-prompted each turn to keep going.\n\n\
GOAL: {text}\n\n\
Do this now:\n\
1. Set up a GOAL WORKSPACE — create a new folder (or reuse a relevant existing one) under \
`.snippet/goals/`, and record its path in a `note`. This is durable scratch space that \
SURVIVES compaction: keep your plan, todos, findings, decisions, and any artifacts there.\n\
2. Write an initial PLAN + TODO list into that folder.\n\
3. Start executing toward 100% completion.\n\n\
Rules: keep going on your own; do NOT stop to ask for confirmation on ordinary steps. When the \
goal is genuinely 100% complete, call `complete_goal` with a short summary. If you hit a hard \
blocker you truly cannot resolve, say exactly what you need. Begin."
    )
}

fn goal_continue_directive(text: &str, dir: &str) -> String {
    format!(
        "[goal] Continue toward your goal: {text}\n\
Re-read your plan/todos/findings in {where}, pick the next unfinished step, and do it — update \
the todos as you go. Keep going; don't stop or recap unless something material changed. When it's \
100% complete, call `complete_goal`.",
        where = goal_dir_phrase(dir)
    )
}

fn goal_selfcheck_directive(text: &str, dir: &str, n: usize) -> String {
    format!(
        "[goal] SELF-CHECK — you've taken {n} autonomous turns on this goal. Step back and evaluate \
honestly:\n\
- Are you making REAL progress toward: {text}?\n\
- Is your approach working, or are you looping/stuck?\n\
- Re-read your plan/todos in {where}.\n\
Then decide: if it's actually complete, call `complete_goal`; if you're genuinely blocked, state \
exactly what you need; otherwise correct course if needed and CONTINUE. Proceed.",
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
pub struct HarnessState {
    pub version: u32,
    pub status: HarnessStatus,
    pub created_at: String,
    pub updated_at: String,
    /// Absolute workspace folder this session runs in (for the serve daemon's
    /// device-wide session list). Empty on states from before this field existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    pub user_request: String,
    /// User-set title override for the session list; when set it wins over the
    /// `user_request`-derived label. Renaming (TUI/app) sets this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// An active `/goal` the agent is autonomously working toward (None = normal
    /// interactive mode). See [`Goal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<Goal>,
    /// True while a history-compaction pass is running (manual or auto). Surfaced
    /// so the UI can show a distinct "Compacting…" state instead of a generic
    /// "Running" — compaction can take a while and otherwise looks like a normal turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacting: bool,
    pub messages: Vec<HarnessMessage>,
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
}

/// A working-tree snapshot the user can rewind to (a commit in the shadow repo).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointRecord {
    /// Shadow-repo commit id.
    pub id: String,
    /// The user prompt this checkpoint was taken before (truncated).
    pub label: String,
    pub created_at: String,
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
    /// Drop messages queued mid-run (in `pending_inputs`) before they're applied.
    DropQueued,
    /// Rename the session (user-set title override).
    SetTitle(String),
    /// Set (or replace) the autonomous `/goal` — the agent begins driving toward it.
    SetGoal(String),
    /// Cancel the active goal — the agent is told and winds down.
    CancelGoal,
    /// Cancel the run.
    Interrupt,
}

// --- Runtime corrections (ported from executor/runtime/step_control.rs) ---

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
    /// Tool-only steps since the agent last said something visible.
    steps_since_visible: usize,
    /// Signals raised this turn, drained into next turn's live context.
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
    /// Consecutive note-only turns (notes with no real work).
    consecutive_note_count: usize,
    /// Consecutive tool-call turns that did no real work (notes / unknown tools).
    unproductive_turns: usize,
    /// Consecutive turns in which EVERY executed tool call failed — the approach
    /// isn't working; escalates to a re-think-or-ask-for-help nudge.
    consecutive_failed_turns: usize,
    /// Signatures of read-only discovery calls already executed THIS request, to
    /// short-circuit exact-duplicate re-calls. Cleared on a new user request and
    /// whenever a mutation makes re-discovery legitimate again.
    executed_calls: std::collections::HashSet<String>,
    /// Whether the PREVIOUS turn repeated a tool call (consecutive or dedup-caught)
    /// — so the live context explains the re-prompt only when actually looping.
    /// Reset on a new user request.
    last_turn_had_repeat: bool,
    /// The model's reasoning from the previous turn, surfaced back in the live
    /// context (experimental). Reset on a new user request.
    last_thought: Option<String>,
    /// Empty completions (no reply — e.g. the agent only left a note) re-prompted
    /// this response cycle. Capped so we ask for an answer without looping forever.
    /// Reset on a new user message.
    empty_reply_reprompts: usize,
    /// Turns spent on the CURRENT request (a soft budget surfaced each turn so the
    /// agent converges instead of sprawling). Reset on a new user request.
    turns_this_request: u64,
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
    ModelError { message: String, retryable: bool },
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
    config: HarnessConfig,
    tools: ToolRegistry,
    context: ToolContext,
}

impl CodingHarness {
    pub fn new(config: HarnessConfig, tools: ToolRegistry, context: ToolContext) -> Self {
        Self {
            config,
            tools,
            context,
        }
    }

    /// One-shot run: drive the agent until it ends a turn (via `complete`), then
    /// return the outcome. Delegation is disabled (no model factory). Used by the
    /// library, by tests, and by each background lane.
    pub async fn run(
        &self,
        model: &mut dyn AgentModel,
        user_request: impl Into<String>,
    ) -> Result<HarnessOutcome, ToolError> {
        let mut state = self
            .load_or_initialize_state(Some(user_request.into()))
            .await?;
        self.compact_history_if_needed(model, &mut state).await?;
        if state.status == HarnessStatus::Completed {
            return Ok(HarnessOutcome {
                final_text: state.final_text,
                events: state.events,
                iterations: state.iterations,
            });
        }

        // Lanes are inert without a factory; this channel is never driven here.
        let (lane_tx, _lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let mut lanes = self.new_lane_manager(None, lane_tx, &state);
        // Watches are likewise inert on the one-shot path (nothing selects on the
        // channel) — present only so the meta-tool dispatch signature is uniform.
        let (watch_tx, _watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches =
            WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        let mut vars = LoopVars::default();
        let mut consecutive_errors = 0usize;

        let start = state.iterations + 1;
        // One-shot / lane runs are always Auto — force it so a resumed/forced Manual
        // state can't block forever on an approval channel that's never driven here.
        state.approval_mode = ApprovalMode::Auto;
        let (_approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalDecision>();
        for iteration in start..=self.config.runtime_backstop_iterations {
            state.iterations = iteration;
            self.persist(&mut state, &lanes).await?;

            match self
                .step(model, &mut state, &mut lanes, &mut watches, &mut vars, false, None, &mut approval_rx)
                .await
            {
                StepResult::Continue => {
                    consecutive_errors = 0;
                }
                StepResult::TurnEnded { final_text, .. } => {
                    state.status = HarnessStatus::Completed;
                    state.final_text = final_text.clone();
                    self.persist(&mut state, &lanes).await?;
                    return Ok(HarnessOutcome {
                        final_text,
                        events: state.events,
                        iterations: iteration,
                    });
                }
                StepResult::ModelError { message, retryable } => {
                    state.events.push(HarnessEvent::ModelError {
                        message: message.clone(),
                    });
                    // Fatal errors (auth/permission/not-found/bad-request) never
                    // succeed on retry — give up at once instead of recovering.
                    let action = if retryable {
                        self.recover(&mut state, &mut consecutive_errors).await
                    } else {
                        RecoveryAction::GiveUp
                    };
                    match action {
                        RecoveryAction::Retry => {
                            self.persist(&mut state, &lanes).await?;
                            continue;
                        }
                        RecoveryAction::GiveUp => {
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            return Err(ToolError::msg(message));
                        }
                    }
                }
            }
        }

        state.status = HarnessStatus::Failed;
        self.persist(&mut state, &lanes).await?;
        Err(ToolError::msg(format!(
            "harness reached runtime backstop after {} iterations",
            self.config.runtime_backstop_iterations
        )))
    }

    /// Resident conversation run: a long-lived loop that processes turns, accepts
    /// mid-run steering, delegates background lanes, and folds lane reports back in.
    /// Returns the final state when the input channel closes or the user interrupts.
    pub async fn run_interactive(
        &self,
        model: &mut dyn AgentModel,
        initial_request: Option<String>,
        mut input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
    ) -> Result<HarnessState, ToolError> {
        let (lane_tx, mut lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let mut state = self.load_or_initialize_state(initial_request).await?;
        self.compact_history_if_needed(model, &mut state).await?;
        // A reopened terminal state (completed / failed / interrupted) starts idle
        // so the loop blocks for the next message instead of exiting at the top.
        // Interrupted is the important one: a session is left in that state whenever
        // the user switches away or hits Esc, and without this reset, resuming it
        // would break out of the loop immediately — the agent would die on load and
        // the user's next message would start a fresh session over it.
        if matches!(
            state.status,
            HarnessStatus::Completed | HarnessStatus::Failed | HarnessStatus::Interrupted
        ) {
            state.status = HarnessStatus::Idle;
        }
        let mut lanes = self.new_lane_manager(factory, lane_tx, &state);
        // File watches (`monitor` meta-tool): re-arm any persisted from the state
        // so a daemon/TUI restart resumes tailing where it left off.
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches =
            WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        watches.restore(&state.watches);
        let mut vars = LoopVars::default();
        let mut consecutive_errors = 0usize;
        // Inputs that arrived while a step was running (the interrupt race consumes
        // input_rx, so non-interrupt messages are parked here until the next turn).
        let mut pending_inputs: Vec<LoopInput> = Vec::new();
        self.persist(&mut state, &lanes).await?;

        loop {
            // Apply any input buffered during a step. A message that arrived mid- or
            // post-turn wakes the loop so the next step addresses it.
            if !pending_inputs.is_empty() {
                let prior_status = state.status;
                let was_running = prior_status == HarnessStatus::Running;
                let mut had_user_msg = false;
                let mut wants_compact = false;
                for input in std::mem::take(&mut pending_inputs) {
                    match input {
                        // Buffered /compact must actually run — `apply_input`
                        // treated it as a no-op, silently swallowing the request.
                        LoopInput::Compact => wants_compact = true,
                        LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            had_user_msg = true;
                            if was_running {
                                // Mid-run steer: the step continues; fold it in.
                                state.messages.push(HarnessMessage::User {
                                    content: format!("[steer]\n{text}"),
                                });
                                state.events.push(HarnessEvent::Steer { text });
                            } else {
                                // The step ENDED while this was queued: it's the
                                // next real request (or the answer to the question
                                // that ended the turn), not a steer — take the full
                                // new-request path (checkpoint, budget/dedup reset,
                                // pending-question clearing).
                                self.accept_user_message(&mut state, &mut vars, text).await;
                                consecutive_errors = 0;
                            }
                        }
                        other => {
                            self.apply_input(&mut state, other);
                        }
                    }
                }
                if had_user_msg {
                    // A steer is still a user message — nudge an intent
                    // restatement next turn so the interruption is captured.
                    vars.pending_signals.push(RuntimeSignal::StateIntent);
                    vars.empty_reply_reprompts = 0;
                }
                if wants_compact {
                    self.run_manual_compaction(model, &mut state, &lanes).await?;
                }
                if had_user_msg || was_running {
                    state.status = HarnessStatus::Running;
                } else if wants_compact {
                    // Compaction ran while parked — return to the parked status
                    // rather than waking the model with nothing new.
                    state.status = prior_status;
                    self.persist(&mut state, &lanes).await?;
                }
            }

            if state.status == HarnessStatus::Running {
                if !model.is_configured() {
                    state.events.push(HarnessEvent::ModelError {
                        message:
                            "No API key configured for this model. Add one in the model settings (app: Models · TUI: /model) before sending."
                                .to_string(),
                    });
                    state.status = HarnessStatus::Idle;
                    self.persist(&mut state, &lanes).await?;
                    continue;
                }
                let (interrupted, wants_compact) = self.drain_pending(
                    &mut state,
                    &mut lanes,
                    &mut watches,
                    &mut input_rx,
                    &mut lane_rx,
                    &mut watch_rx,
                );
                if interrupted {
                    state.status = HarnessStatus::Interrupted;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "interrupted".to_string(),
                        reasoning: "User interrupted the run.".to_string(),
                    });
                    self.persist(&mut state, &lanes).await?;
                    break;
                }
                if wants_compact {
                    self.run_manual_compaction(model, &mut state, &lanes).await?;
                }

                state.iterations += 1;
                self.persist(&mut state, &lanes).await?;

                // Compact before the next model call when the last prompt exceeded the
                // budget — not just once at startup.
                self.compact_history_if_needed(model, &mut state).await?;

                // Race the step against the input channel so an interrupt cancels
                // the in-flight model call immediately — otherwise the loop only
                // notices the interrupt at the next iteration, after waiting out the
                // whole HTTP request and its retry backoff. Non-interrupt messages
                // that land mid-step are buffered and applied at the next loop top.
                // The marks drop a half-written turn on interrupt so a later resume
                // sees a clean boundary (never an unpaired assistant tool call).
                let msg_mark = state.messages.len();
                let evt_mark = state.events.len();
                // Bridge approvals from the input channel to the in-flight step: while
                // a mutating tool waits (manual mode), Approve/Deny arrive here and are
                // forwarded to the step over this channel; interrupt still cancels.
                let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalDecision>();
                let outcome = {
                    let step_fut = self.step(
                        model,
                        &mut state,
                        &mut lanes,
                        &mut watches,
                        &mut vars,
                        true,
                        sink.as_ref(),
                        &mut approval_rx,
                    );
                    tokio::pin!(step_fut);
                    loop {
                        tokio::select! {
                            result = &mut step_fut => break Some(result),
                            msg = input_rx.recv() => match msg {
                                Some(LoopInput::Interrupt) | None => break None,
                                Some(LoopInput::Approve) => {
                                    let _ = approval_tx.send(ApprovalDecision::Approve);
                                }
                                Some(LoopInput::ApproveAll) => {
                                    let _ = approval_tx.send(ApprovalDecision::ApproveAll);
                                }
                                Some(LoopInput::Deny) => {
                                    let _ = approval_tx.send(ApprovalDecision::Deny);
                                }
                                Some(LoopInput::DropQueued) => pending_inputs.clear(),
                                Some(other) => pending_inputs.push(other),
                            }
                        }
                    }
                };

                let Some(result) = outcome else {
                    // Interrupted mid-step: discard the partial turn and stop.
                    state.messages.truncate(msg_mark);
                    state.events.truncate(evt_mark);
                    state.status = HarnessStatus::Interrupted;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "interrupted".to_string(),
                        reasoning: "User interrupted the run.".to_string(),
                    });
                    self.persist(&mut state, &lanes).await?;
                    break;
                };

                match result {
                    StepResult::Continue => {
                        consecutive_errors = 0;
                    }
                    StepResult::TurnEnded { kind, final_text } => {
                        consecutive_errors = 0;
                        state.final_text = final_text;
                        state.status = match kind {
                            TurnEndKind::Ask => HarnessStatus::WaitingForInput,
                            TurnEndKind::Complete => HarnessStatus::Idle,
                        };
                        vars.steps_since_visible = 0;
                        self.persist(&mut state, &lanes).await?;
                    }
                    StepResult::ModelError { message, retryable } => {
                        state.events.push(HarnessEvent::ModelError {
                            message: message.clone(),
                        });
                        // Goal mode: a rate limit PAUSES the goal rather than failing
                        // the session — the drive stops until the window resets. Record
                        // when that is (from the last rate-limit snapshot) so the UI can
                        // show it. The user can re-issue /goal to resume.
                        if is_rate_limit_error(&message) {
                            if let Some(goal) =
                                state.goal.as_mut().filter(|g| g.status == GoalStatus::Active)
                            {
                                goal.status = GoalStatus::Paused;
                                goal.resume_at =
                                    state.rate_limit.as_ref().and_then(earliest_reset).unwrap_or(0);
                                let text = goal.text.clone();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "goal_paused".to_string(),
                                    reasoning: format!("rate limited — goal paused: {text}"),
                                });
                                state.status = HarnessStatus::Idle;
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            }
                        }
                        // Fatal errors never recover — fail at once, no backoff.
                        if !retryable {
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            break;
                        }
                        // Race the recovery backoff against the input channel so
                        // Esc cancels during the wait instead of after it.
                        let action = {
                            let recover_fut = self.recover(&mut state, &mut consecutive_errors);
                            tokio::pin!(recover_fut);
                            loop {
                                tokio::select! {
                                    a = &mut recover_fut => break Some(a),
                                    msg = input_rx.recv() => match msg {
                                        Some(LoopInput::Interrupt) | None => break None,
                                        Some(LoopInput::DropQueued) => pending_inputs.clear(),
                                        Some(other) => pending_inputs.push(other),
                                    }
                                }
                            }
                        };
                        match action {
                            None => {
                                state.status = HarnessStatus::Interrupted;
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "interrupted".to_string(),
                                    reasoning: "User interrupted the run.".to_string(),
                                });
                                self.persist(&mut state, &lanes).await?;
                                break;
                            }
                            Some(RecoveryAction::Retry) => {
                                self.persist(&mut state, &lanes).await?;
                            }
                            Some(RecoveryAction::GiveUp) => {
                                state.status = HarnessStatus::Failed;
                                self.persist(&mut state, &lanes).await?;
                                break;
                            }
                        }
                    }
                }
            } else if state.status == HarnessStatus::Interrupted {
                break;
            } else {
                // Idle or WaitingForInput. When a goal is Active and we're Idle (not
                // blocked on a question), DRIVE the loop forward instead of waiting —
                // but a queued real input or a lane report is handled first (biased).
                let goal_driving = state.status == HarnessStatus::Idle
                    && matches!(&state.goal, Some(g) if g.status == GoalStatus::Active);
                tokio::select! {
                    biased;
                    input = input_rx.recv() => match input {
                        Some(LoopInput::UserMessage(text)) | Some(LoopInput::Answer(text)) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            self.accept_user_message(&mut state, &mut vars, text).await;
                            consecutive_errors = 0;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::Compact) => {
                            self.run_manual_compaction(model, &mut state, &lanes).await?;
                            state.pending_question = None;
                            state.status = HarnessStatus::Idle;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetMode(mode)) => {
                            state.approval_mode = mode;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetTitle(title)) => {
                            let t = title.trim();
                            state.title = if t.is_empty() { None } else { Some(t.to_string()) };
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetGoal(text)) => {
                            self.begin_goal(&mut state, text);
                            consecutive_errors = 0;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::CancelGoal) => {
                            self.end_goal(&mut state);
                            self.persist(&mut state, &lanes).await?;
                        }
                        // No tool call is pending while idle — nothing to approve.
                        Some(LoopInput::Approve) | Some(LoopInput::ApproveAll) | Some(LoopInput::Deny) => {}
                        // Nothing queued while idle.
                        Some(LoopInput::DropQueued) => {}
                        Some(LoopInput::Interrupt) | None => {
                            state.status = HarnessStatus::Interrupted;
                            self.persist(&mut state, &lanes).await?;
                            break;
                        }
                    },
                    Some(result) = lane_rx.recv() => {
                        self.inject_lane_result(&mut state, &mut lanes, &result);
                        // A lane reporting in while idle is new information to act on.
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        self.persist(&mut state, &lanes).await?;
                    }
                    Some(event) = watch_rx.recv() => {
                        // A watched file grew (and matched its filter) while idle —
                        // wake the agent with the appended text.
                        self.inject_watch_event(&mut state, &mut watches, &event);
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        consecutive_errors = 0;
                        self.persist(&mut state, &lanes).await?;
                    }
                    _ = std::future::ready(()), if goal_driving => {
                        // Autonomous goal, nothing else queued: take the next goal turn.
                        self.drive_goal_turn(&mut state, &mut vars);
                        consecutive_errors = 0;
                        self.persist(&mut state, &lanes).await?;
                    }
                }
            }
        }

        Ok(state)
    }

    /// Accept a user message that STARTS a turn (or answers the question that
    /// ended one) — the shared path for the idle receive and for a message that
    /// was buffered while a step finished. Handles checkpointing, per-request
    /// bookkeeping resets, and the pending question, and marks the loop Running.
    /// Persisting is left to the caller.
    async fn accept_user_message(
        &self,
        state: &mut HarnessState,
        vars: &mut LoopVars,
        text: String,
    ) {
        let answering =
            state.status == HarnessStatus::WaitingForInput && state.pending_question.is_some();
        // Snapshot the workspace before acting on a NEW request, so the whole turn
        // (direct edits + any lane changes + bash) can be rewound. An answer
        // continues a turn already checkpointed.
        if !answering {
            // First real request seeds the session title (app sessions open
            // empty, so user_request starts blank).
            if state.user_request.trim().is_empty() {
                state.user_request = text.clone();
            }
            self.checkpoint(state, &text).await;
            // Fresh request: re-discovery is legitimate again, and prior-turn
            // loop/thought/failure state belongs to the past run.
            vars.executed_calls.clear();
            vars.last_turn_had_repeat = false;
            vars.last_thought = None;
            vars.turns_this_request = 0;
            vars.consecutive_failed_turns = 0;
        }
        state.pending_question = None;
        state.messages.push(HarnessMessage::User {
            content: if answering {
                format!("[answer]\n{text}")
            } else {
                text.clone()
            },
        });
        state.events.push(HarnessEvent::UserInput { text });
        // Every user message → restate intent next turn.
        vars.pending_signals.push(RuntimeSignal::StateIntent);
        state.status = HarnessStatus::Running;
        vars.steps_since_visible = 0;
        vars.empty_reply_reprompts = 0;
    }

    /// Run a user-requested compaction pass now, surfacing the compaction UI
    /// (Running + the pass event). Leaves `state.status` Running; callers decide
    /// what follows and persist it.
    async fn run_manual_compaction(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        lanes: &LaneManager,
    ) -> Result<(), ToolError> {
        let before_len = state.messages.len();
        state.status = HarnessStatus::Running;
        state.compacting = true;
        self.persist(state, lanes).await?;
        let result = self.compact_history_agentic(model, state, true).await;
        state.compacting = false;
        result?;
        if state.messages.len() >= before_len {
            state.events.push(HarnessEvent::SystemDecision {
                step: "history_compaction_skipped".to_string(),
                reasoning: "Manual compaction ran, but there was no additional older history left to shrink beyond the preserved recent tail.".to_string(),
            });
        }
        Ok(())
    }

    /// Stamp base64 image bytes onto `read_image` tool results so the model can
    /// actually SEE the image. Done per-turn on the cloned request only (never
    /// persisted), so state stays lean and images re-inline fresh each turn.
    /// Providers turn the inlined bytes into real image blocks.
    fn inline_images(&self, messages: &mut [HarnessMessage], supports_images: bool) {
        use base64::{Engine, engine::general_purpose::STANDARD};
        // Skip absurdly large images so a request can't balloon unboundedly.
        const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
        // Inlined images cost their image tokens again on EVERY subsequent turn,
        // so a long session that read a few screenshots pays for all of them
        // forever. Age out by TURN, not by count: everything the model read in
        // the last few assistant turns stays visible (a 10-screenshot batch is
        // still fully visible while it's being worked on); older results carry a
        // note instead — the model re-reads if it truly needs one back.
        const IMAGE_TURN_WINDOW: usize = 3;
        // Safety cap across the inlined set so one batch can't balloon a request.
        const MAX_TOTAL_IMAGE_BYTES: usize = 24 * 1024 * 1024;
        let mut assistant_turns = 0usize;
        let mut cutoff = 0usize;
        for (i, m) in messages.iter().enumerate().rev() {
            if matches!(m, HarnessMessage::Assistant { .. }) {
                assistant_turns += 1;
                if assistant_turns >= IMAGE_TURN_WINDOW {
                    cutoff = i;
                    break;
                }
            }
        }
        // Budget pass, newest first, so when a batch exceeds the total cap it's
        // the OLDEST images that drop to notes.
        let mut allowed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut total_inlined = 0usize;
        for (index, message) in messages.iter().enumerate().rev() {
            let HarnessMessage::ToolResult { tool_name, content, .. } = message else {
                continue;
            };
            if tool_name != "read_image" || index < cutoff {
                continue;
            }
            let Some(path) = content.pointer("/data/path").and_then(Value::as_str) else {
                continue;
            };
            let Ok(resolved) = self.context.resolve_workspace_path(path) else {
                continue;
            };
            let Ok(len) = std::fs::metadata(&resolved).map(|m| m.len() as usize) else {
                continue;
            };
            if len == 0 || len > MAX_IMAGE_BYTES || total_inlined + len > MAX_TOTAL_IMAGE_BYTES {
                continue;
            }
            total_inlined += len;
            allowed.insert(index);
        }
        for (index, message) in messages.iter_mut().enumerate() {
            let HarnessMessage::ToolResult { tool_name, content, .. } = message else {
                continue;
            };
            if tool_name != "read_image" {
                continue;
            }
            let Some(path) = content
                .pointer("/data/path")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            // Text-only model: never inline image bytes (it 400s and poisons every
            // later turn). Leave a note so the model knows an image was read.
            if !supports_images {
                if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                    data.insert(
                        "image_note".to_string(),
                        Value::String(format!(
                            "[image at {path} not shown — the current model is text-only]"
                        )),
                    );
                }
                continue;
            }
            // Aged out of the inline window (or over the total budget): note
            // instead of bytes.
            if !allowed.contains(&index) {
                if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                    data.insert(
                        "image_note".to_string(),
                        Value::String(format!(
                            "[image at {path} was shown in an earlier turn — call read_image again if you need to see it now]"
                        )),
                    );
                }
                continue;
            }
            let Ok(resolved) = self.context.resolve_workspace_path(&path) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&resolved) else {
                continue;
            };
            if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                continue;
            }
            let encoded = STANDARD.encode(&bytes);
            if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                data.insert("image_base64".to_string(), Value::String(encoded));
            }
        }
    }

    /// Snapshot the workspace before a turn so the user can `/rewind` to it.
    /// Best-effort — a failure (no git, etc.) is skipped, never blocking the turn.
    async fn checkpoint(&self, state: &mut HarnessState, prompt: &str) {
        let label: String = prompt.chars().take(80).collect();
        let workspace = self.context.workspace_root().to_path_buf();
        let snap_label = label.clone();
        // `git add -A` + commit-tree can take a while on a large workspace — run it
        // off the async runtime so it never stalls streaming or other lanes.
        let id = tokio::task::spawn_blocking(move || crate::checkpoint::snapshot(&workspace, &snap_label))
            .await
            .ok()
            .flatten();
        if let Some(id) = id {
            state.checkpoints.push(CheckpointRecord {
                id,
                label,
                created_at: chrono::Utc::now().to_rfc3339(),
            });
            // Cap retained records so a long session doesn't bloat persisted state.
            const MAX_CHECKPOINTS: usize = 8;
            let len = state.checkpoints.len();
            let dropped: Vec<String> = if len > MAX_CHECKPOINTS {
                state
                    .checkpoints
                    .drain(..len - MAX_CHECKPOINTS)
                    .map(|c| c.id)
                    .collect()
            } else {
                Vec::new()
            };
            // Drop ONLY this session's aged-out snapshots from the shadow repo and
            // gc so disk stays bounded (off the async runtime — gc can be slow).
            // The shadow is shared by all sessions in the workspace, so pruning by
            // "everything not in my keep list" destroyed sibling sessions' rewinds.
            let keep: Vec<String> = state.checkpoints.iter().map(|c| c.id.clone()).collect();
            let ws = self.context.workspace_root().to_path_buf();
            let _ =
                tokio::task::spawn_blocking(move || crate::checkpoint::prune(&ws, &keep, &dropped))
                    .await;
        }
    }

    fn new_lane_manager(
        &self,
        factory: Option<ModelFactory>,
        lane_tx: mpsc::UnboundedSender<LaneResult>,
        state: &HarnessState,
    ) -> LaneManager {
        let lane_root = self
            .config
            .state_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join("lanes"))
            .unwrap_or_else(|| self.context.workspace_root().join(".snippet/lanes"));
        LaneManager::new(
            factory,
            self.context.workspace_root().to_path_buf(),
            lane_root,
            lane_tx,
            self.config.exa_api_key.clone(),
        )
        .with_records(state.lanes.clone())
    }

    /// Non-blocking drain of steers + lane reports between iterations. Returns
    /// `(interrupted, wants_compact)` — a `/compact` seen here must be run by the
    /// caller (`apply_input` can't do it; it has no model access).
    fn drain_pending(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        input_rx: &mut mpsc::UnboundedReceiver<LoopInput>,
        lane_rx: &mut mpsc::UnboundedReceiver<LaneResult>,
        watch_rx: &mut mpsc::UnboundedReceiver<WatchEvent>,
    ) -> (bool, bool) {
        let mut interrupted = false;
        let mut wants_compact = false;
        while let Ok(input) = input_rx.try_recv() {
            if matches!(input, LoopInput::Compact) {
                wants_compact = true;
                continue;
            }
            interrupted |= self.apply_input(state, input);
        }
        while let Ok(result) = lane_rx.try_recv() {
            self.inject_lane_result(state, lanes, &result);
        }
        while let Ok(event) = watch_rx.try_recv() {
            self.inject_watch_event(state, watches, &event);
        }
        (interrupted, wants_compact)
    }

    /// Apply one queued input while a run is active: a message/answer becomes a
    /// `[steer]`, an interrupt returns `true`. Shared by the between-iteration
    /// drain and the buffered-input drain.
    fn apply_input(&self, state: &mut HarnessState, input: LoopInput) -> bool {
        match input {
            LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    state.messages.push(HarnessMessage::User {
                        content: format!("[steer]\n{text}"),
                    });
                    state.events.push(HarnessEvent::Steer { text });
                }
                false
            }
            LoopInput::Compact => {
                // Manual compaction is handled directly by the outer interactive loop;
                // it should not inject a steer or schedule another model turn.
                false
            }
            LoopInput::SetMode(mode) => {
                state.approval_mode = mode;
                false
            }
            // Cancelling queued input is handled where pending_inputs lives; by the
            // time it reaches apply_input (between turns) there's nothing to drop.
            LoopInput::DropQueued => false,
            LoopInput::SetTitle(title) => {
                let t = title.trim();
                state.title = if t.is_empty() { None } else { Some(t.to_string()) };
                false
            }
            // Approve/Deny are only meaningful while a tool call is awaiting approval
            // inside a step; arriving here (between turns) they're stray no-ops.
            LoopInput::Approve | LoopInput::ApproveAll | LoopInput::Deny => false,
            LoopInput::SetGoal(text) => {
                self.begin_goal(state, text);
                false
            }
            LoopInput::CancelGoal => {
                self.end_goal(state);
                false
            }
            LoopInput::Interrupt => true,
        }
    }

    /// Start (or replace) an autonomous goal and prompt the agent toward it now.
    /// Called from `/goal` (idle or mid-run). The loop then re-drives the agent
    /// each idle turn until the goal completes, the user cancels, or it's paused.
    fn begin_goal(&self, state: &mut HarnessState, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        state.goal = Some(Goal {
            text: text.clone(),
            dir: String::new(),
            status: GoalStatus::Active,
            autonomous_turns: 0,
            resume_at: 0,
        });
        state.messages.push(HarnessMessage::User {
            content: goal_start_directive(&text),
        });
        state.events.push(HarnessEvent::SystemDecision {
            step: "goal_set".to_string(),
            reasoning: text,
        });
        state.status = HarnessStatus::Running;
    }

    /// Cancel the active goal (user-initiated) — tell the agent to stop and wind down.
    fn end_goal(&self, state: &mut HarnessState) {
        let Some(goal) = state.goal.as_mut() else {
            return;
        };
        goal.status = GoalStatus::Cancelled;
        let (text, dir) = (goal.text.clone(), goal.dir.clone());
        state.messages.push(HarnessMessage::User {
            content: goal_cancel_directive(&text, &dir),
        });
        state.events.push(HarnessEvent::SystemDecision {
            step: "goal_cancelled".to_string(),
            reasoning: text,
        });
        state.status = HarnessStatus::Running;
    }

    /// Take one autonomous turn toward the active goal: bump the counter and push
    /// the continue/self-check directive as a fresh user turn. Called from the idle
    /// point when a goal is Active and nothing else is pending.
    fn drive_goal_turn(&self, state: &mut HarnessState, vars: &mut LoopVars) {
        let (text, dir, n) = match state.goal.as_mut() {
            Some(g) => {
                g.autonomous_turns += 1;
                (g.text.clone(), g.dir.clone(), g.autonomous_turns)
            }
            None => return,
        };
        let directive = if n % GOAL_SELF_CHECK_EVERY == 0 {
            goal_selfcheck_directive(&text, &dir, n)
        } else {
            goal_continue_directive(&text, &dir)
        };
        state.messages.push(HarnessMessage::User { content: directive });
        // A goal turn is a fresh turn: reset per-turn loop bookkeeping so discovery
        // and progress tracking start clean (no checkpoint — the goal start already
        // seeded one, and per-turn snapshots would flood the shadow git).
        *vars = LoopVars::default();
        state.status = HarnessStatus::Running;
    }

    /// Fold a file-watch wake into history: advance the persisted tail offset and
    /// inject a `[file_watch]` envelope carrying the appended text, mirroring how
    /// lane reports arrive. The follow-up id is demoted to an internal handle so
    /// the agent speaks of the watch by subject, never "watch-1".
    fn inject_watch_event(
        &self,
        state: &mut HarnessState,
        watches: &mut WatchManager,
        event: &WatchEvent,
    ) {
        watches.advance_offset(&event.id, event.new_offset);
        state.watches = watches.records().to_vec();
        let skipped_note = if event.skipped > 0 {
            format!("\n(…{} earlier bytes of this burst omitted — read the file for the full text)", event.skipped)
        } else {
            String::new()
        };
        state.messages.push(HarnessMessage::User {
            content: format!(
                "[file_watch]\nsubject = \"{}\"\npath = \"{}\"\nappended:{skipped_note}\n{}\n[orchestration] Act on this if it needs action; stay quiet and end the turn if it doesn't. Remove the watch (monitor action:\"remove\") once it has served its purpose.\n[follow_up_id = \"{}\"]  # internal handle for monitor remove ONLY — refer to this work by its SUBJECT to the user, never by this id\n[/file_watch]",
                event.label, event.path, event.appended.trim_end(), event.id
            ),
        });
        let preview: String = event.appended.trim().chars().take(120).collect();
        state.events.push(HarnessEvent::SystemDecision {
            step: "file_watch".to_string(),
            reasoning: format!("\"{}\" — {} grew: {preview}", event.label, event.path),
        });
    }

    fn inject_lane_result(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        result: &LaneResult,
    ) {
        lanes.record_result(result);
        let body = match result.status {
            // Prefer the full report (actions + findings + summary); fall back to
            // the concise summary so the parent agent sees what the lane actually did.
            LaneStatus::Completed => result
                .report
                .clone()
                .or_else(|| result.summary.clone())
                .unwrap_or_else(|| "completed".to_string()),
            // A failure is a decision point, not a dead end — spell the options
            // out so the orchestrator recovers instead of stalling or re-briefing
            // from scratch.
            LaneStatus::Failed => format!(
                "FAILED: {}\nRecover deliberately: follow up THIS lane (delegate_task with lane_id=\"{}\") \
                 with a narrower or corrected brief — it keeps everything it learned — or do the slice \
                 yourself if it's small. Don't silently drop this part of the work.",
                result.error.clone().unwrap_or_else(|| "unknown error".to_string()),
                result.id,
            ),
            LaneStatus::Running => "still running".to_string(),
        };
        // Orchestration cue: tell the agent whether it's still waiting on others
        // or holding the complete picture — the difference between a progress
        // note and the final synthesis.
        let outstanding = lanes.active_count();
        let cue = if outstanding > 0 {
            format!(
                "{outstanding} lane(s) still out — fold this in (or note progress) and keep waiting; don't finalize yet."
            )
        } else {
            "ALL delegated lanes have now reported. You hold the complete picture: synthesize the results \
             into the deliverable now (verify load-bearing findings against the cited file:line first). \
             Don't restart the investigation yourself; follow up a specific lane if something is missing."
                .to_string()
        };
        state.messages.push(HarnessMessage::User {
            content: format!(
                "[lane_report]\nsubject = \"{}\"\nstatus = {:?}\n{}\n[orchestration] {}\n[follow_up_id = \"{}\"]  # internal handle for a delegate_task follow-up ONLY — refer to this work by its SUBJECT to the user, never by this id\n[/lane_report]",
                result.title, result.status, body, cue, result.id
            ),
        });
        state.events.push(HarnessEvent::LaneCompleted {
            id: result.id.clone(),
            title: result.title.clone(),
            status: result.status,
            summary: result.summary.clone(),
        });
    }

    /// Run one model step: generate, parse tool calls, execute them, and report
    /// how the turn should proceed.
    async fn step(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        sink: Option<&StreamHandle>,
        approval_rx: &mut mpsc::UnboundedReceiver<ApprovalDecision>,
    ) -> StepResult {
        let goal_active = state
            .goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Active);
        let definitions = self.definitions_for(conversation_mode, goal_active);

        // Unproductive backstop: too many tool-call turns in a row that did no
        // real work (notes / unknown tools). Wrap the run up cleanly rather than
        // spinning. Ported from wacht's `MAX_UNPRODUCTIVE_TURNS` gate.
        if vars.unproductive_turns >= MAX_UNPRODUCTIVE_TURNS {
            vars.unproductive_turns = 0;
            return StepResult::TurnEnded {
                kind: TurnEndKind::Complete,
                final_text: None,
            };
        }

        // Visibility lapse: too many tool-only steps without a word to the user.
        if vars.steps_since_visible >= VISIBILITY_NUDGE_WINDOW {
            vars.pending_signals.push(RuntimeSignal::VisibilityLapse);
            vars.steps_since_visible = 0;
        }

        // This turn counts against the current request's soft turn budget.
        vars.turns_this_request = vars.turns_this_request.saturating_add(1);

        // Keep the lane snapshot current so the live context shows what's still
        // running (the orchestrator must know what it's waiting on).
        state.lanes = lanes.records().to_vec();

        // Rebuild the live-context block fresh every turn (freshest user input +
        // drained runtime signals) and append it after the durable history. It is
        // sent to the model but never persisted into `state.messages`, so signals
        // re-ground the model each turn instead of accumulating as stale nudges.
        // Snapshot the pending signals: `build_live_context` drains them into this
        // request, but if the request FAILS they must survive to the retry — losing
        // a loop-breaking nudge exactly when the model is looping made things worse.
        let signals_backup = vars.pending_signals.clone();
        let mut request_messages = state.messages.clone();
        self.inline_images(&mut request_messages, model.supports_images());
        // Delivered as System, not User: adapters wrap a mid-history System turn
        // in a `[runtime_signal]` envelope, so the model reads it as out-of-band
        // runtime state rather than the user speaking — which stopped it from (a)
        // replying with "you should do X" advice and (b) narrating its completion
        // status back to "the user". (On the wire this still lands in the user
        // role on providers with no mid-conversation system role — the framing in
        // build_live_context is the portable half of the fix.)
        request_messages.push(HarnessMessage::System {
            content: build_live_context(state, vars, conversation_mode, self.context.workspace_root()),
        });

        // Clear any leftover live-stream text before this turn streams into it; the
        // sink is present only for the interactive conversation (lanes/one-shot
        // pass None and stay buffered).
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }

        // "No tool calls = done": a plain-text turn ends the run, so we never force
        // a tool call — the model finishes simply by replying without one.
        let mut output = match model
            .generate(&request_messages, &definitions, false, sink.cloned())
            .await
        {
            Ok(output) => output,
            Err(error) => {
                // The failed request consumed the drained signals — restore them
                // so the retry carries the same steering.
                vars.pending_signals = signals_backup;
                // Adapters embed the full HTTP body; show only a concise first line.
                let raw = error.to_string();
                let line = raw.lines().next().unwrap_or(&raw);
                let message = if line.chars().count() > 240 {
                    format!("{}…", line.chars().take(240).collect::<String>())
                } else {
                    line.to_string()
                };
                return StepResult::ModelError {
                    retryable: error.retryable(),
                    message,
                };
            }
        };
        // Capture this turn's reasoning (from the sink) so the next turn's live
        // context can surface "what you thought last time". Bounded so it can't
        // bloat the request.
        if let Some(sink) = sink {
            let thought = StreamBuffer::snapshot_thinking(sink);
            let thought = thought.trim();
            vars.last_thought =
                (!thought.is_empty()).then(|| thought.chars().take(1500).collect::<String>());
        }
        if let Some(usage) = output.usage {
            state.total_tokens = state.total_tokens.saturating_add(usage.total_tokens);
            state.prompt_tokens = state.prompt_tokens.saturating_add(usage.prompt_tokens);
            state.completion_tokens =
                state.completion_tokens.saturating_add(usage.completion_tokens);
            state.cache_read_tokens =
                state.cache_read_tokens.saturating_add(usage.cache_read_tokens);
            state.last_prompt_tokens = usage.prompt_tokens;
        }
        if output.rate_limit.is_some() {
            state.rate_limit = output.rate_limit.clone();
        }

        // A response cut off at the token cap is never a finished reply.
        let truncated = output.is_truncated();
        if truncated {
            vars.pending_signals.push(RuntimeSignal::ResponseTruncated);
        }

        let native_call_names: Vec<String> =
            output.calls.iter().map(|c| c.tool_name.clone()).collect();
        let raw_content = output.content_text.clone();
        let mut calls = Vec::new();
        calls.append(&mut output.calls);
        let mut progress_text = None;

        if let Some(text) = output.content_text.as_deref() {
            if looks_like_inline_tool_submission(text) {
                let inline = extract_inline_tool_submissions(text);
                let residual = inline.residual_text.clone().unwrap_or_default();
                // Salvage gating (wacht): only adopt recovered calls when the
                // markup dominated the message (short residual prose). If the
                // residual is long, the text is a real reply that happens to
                // mention markup — keep it as prose, ignore the salvage.
                let residual_short = residual.trim().chars().count() <= 240;
                if residual_short
                    && inline.calls.iter().any(|c| is_plausible_tool_name(&c.tool_name))
                {
                    if !residual.trim().is_empty() {
                        progress_text = Some(residual);
                    }
                    calls.extend(inline.calls);
                } else if !text.trim().is_empty() {
                    progress_text = Some(text.trim().to_string());
                }
            } else if !text.trim().is_empty() {
                progress_text = Some(text.trim().to_string());
            }
        }

        // Strip leading time prefixes and drop reasoning dumps / hallucinated
        // tool-call renders from the user-visible text.
        progress_text = progress_text.and_then(|t| crate::sanitize::clean_user_text(&t));

        normalize_tool_aliases(&mut calls);
        // Drop phantom calls: a name that isn't a clean identifier (`...`, or prose
        // fragments like `bash ... ``` `)` salvaged from quoted syntax) can't be a real
        // tool, so it's noise rather than a genuine unknown-tool to report back.
        calls.retain(|call| is_plausible_tool_name(&call.tool_name));

        self.debug_log(&format!(
            "iter={} {} native=[{}] parsed=[{}] content={:?} progress={:?}",
            state.iterations,
            if conversation_mode { "conv" } else { "lane" },
            native_call_names.join(","),
            calls
                .iter()
                .map(|c| c.tool_name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            raw_content.as_deref().map(dbg_short),
            progress_text.as_deref().map(dbg_short),
        ));

        if calls.is_empty() {
            // Truncated text is a partial answer, not a finished reply — surface
            // the fragment as progress and take another turn instead of letting
            // `handle_terminal_text` move toward completion.
            if truncated {
                if let Some(text) = progress_text {
                    state.messages.push(HarnessMessage::Assistant {
                        content: text.clone(),
                        tool_calls: Vec::new(),
                    });
                    state.events.push(HarnessEvent::AssistantText { text });
                    vars.steps_since_visible = 0;
                }
                return StepResult::Continue;
            }
            // No tool calls: the turn is over and this text is the final answer
            // ("no tool calls = done"). Render it once, in order, and end the run.
            if let Some(text) = progress_text.clone() {
                state.messages.push(HarnessMessage::Assistant {
                    content: text.clone(),
                    tool_calls: Vec::new(),
                });
                state.events.push(HarnessEvent::AssistantText { text });
                vars.steps_since_visible = 0;
            }
            // Conversation agent: don't end with NO visible reply when the agent
            // hasn't actually answered since the last user message (e.g. it only
            // left a note and then returned an empty turn). Re-prompt for a real
            // reply a couple of times before giving up.
            let empty_reply = progress_text.as_deref().map(str::trim).unwrap_or("").is_empty();
            if conversation_mode
                && empty_reply
                && !replied_since_last_user(&state.events)
                && vars.empty_reply_reprompts < 2
            {
                vars.empty_reply_reprompts += 1;
                vars.pending_signals.push(RuntimeSignal::EmptyResponse);
                return StepResult::Continue;
            }
            return StepResult::TurnEnded {
                kind: TurnEndKind::Complete,
                final_text: progress_text,
            };
        }

        // Tool-call-loop detection: if the model repeats the exact same call(s),
        // steer it next turn instead of letting it spin.
        let signature = calls
            .iter()
            .map(|call| format!("{}:{}", call.tool_name, call.arguments))
            .collect::<Vec<_>>()
            .join("|");
        if vars.last_tool_signature.as_deref() == Some(signature.as_str()) {
            vars.repeated_tool_count += 1;
            if vars.repeated_tool_count >= 2 {
                vars.pending_signals.push(RuntimeSignal::ToolCallLoop {
                    count: vars.repeated_tool_count + 1,
                });
            }
        } else {
            vars.repeated_tool_count = 0;
        }
        vars.last_tool_signature = Some(signature.clone());

        // Windowed repeat detection: the same call appearing 3+ times within the
        // last 8 turns is a loop even if other calls are interleaved between them.
        vars.recent_tool_signatures.push_back(signature.clone());
        while vars.recent_tool_signatures.len() > 8 {
            vars.recent_tool_signatures.pop_front();
        }
        let windowed = vars.recent_tool_signatures.iter().filter(|s| **s == signature).count();
        if windowed >= 3 && vars.repeated_tool_count < 2 {
            vars.pending_signals.push(RuntimeSignal::ToolCallLoop { count: windowed });
        }

        // Assign every call a stable id and record the native assistant turn: the
        // visible progress text plus the tool calls it made. Each call is answered
        // below by a ToolResult with the matching id (valid tool_call/tool_result
        // exchange).
        for (idx, call) in calls.iter_mut().enumerate() {
            if call.id.is_none() {
                call.id = Some(format!("call_{}_{}", state.iterations, idx));
            }
        }
        let tool_calls: Vec<crate::llm::ToolCallRecord> = calls
            .iter()
            .map(|call| crate::llm::ToolCallRecord {
                id: call.id.clone().unwrap_or_default(),
                name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
                signature: call.signature.clone(),
                origin_model: call.origin_model.clone(),
            })
            .collect();
        state.messages.push(HarnessMessage::Assistant {
            content: progress_text.clone().unwrap_or_default(),
            tool_calls,
        });
        // Progress text on a tool turn is rendered immediately, in order — never
        // buffered or re-delivered (that was the old duplicate-answer source).
        if let Some(text) = progress_text {
            state.events.push(HarnessEvent::AssistantText { text });
            vars.steps_since_visible = 0;
        } else {
            vars.steps_since_visible += 1;
        }

        // Per-turn productivity tracking, drives note-loop / unproductive /
        // backpressure / shell-discipline signals after the batch runs.
        let mut real_work_count = 0usize;
        let mut failed_results = 0usize;
        let mut had_note = false;
        let mut shell_nudged_this_turn = false;
        let mut dedup_hits = 0usize;

        // Manual mode: count mutating calls up front so each approval prompt can show
        // "action N of M", and track which one we're on.
        let total_mutating = calls
            .iter()
            .filter(|c| MUTATING_TOOLS.contains(&c.tool_name.as_str()))
            .count();
        let mut approval_index = 0usize;

        for call in calls {
            let tool_name = call.tool_name.clone();
            let call_id = call.id.clone().unwrap_or_default();
            state.events.push(HarnessEvent::ToolCall {
                tool_name: tool_name.clone(),
                arguments: call.arguments.clone(),
            });

            // Headless explicit completion: a lane / one-shot run ends with a
            // structured `summary` (folded back into the caller). Not advertised to
            // the conversation agent, which finishes by replying with no tool calls.
            if tool_name == "terminate_loop" {
                let summary = call
                    .arguments
                    .get("summary")
                    .or_else(|| call.arguments.get("message"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let result = match summary {
                    Some(s) => {
                        json!({"schema_version": 1, "status": "success", "data": {"summary": s}})
                    }
                    None => tool_error(
                        "`terminate_loop` requires a non-empty `summary` of what you did and found.",
                    ),
                };
                state.events.push(HarnessEvent::ToolResult {
                    tool_name: tool_name.clone(),
                    result: result.clone(),
                });
                state.messages.push(HarnessMessage::ToolResult {
                    tool_call_id: call_id,
                    tool_name,
                    content: result,
                });
                if let Some(s) = summary {
                    // The conversation agent isn't offered terminate_loop, but a
                    // steered model calls it anyway. Its summary IS the answer —
                    // render it as one, or the turn ends silently and the user
                    // never sees the reply.
                    if conversation_mode && !replied_since_last_user(&state.events) {
                        state.messages.push(HarnessMessage::Assistant {
                            content: s.to_string(),
                            tool_calls: Vec::new(),
                        });
                        state.events.push(HarnessEvent::AssistantText { text: s.to_string() });
                    }
                    return StepResult::TurnEnded {
                        kind: TurnEndKind::Complete,
                        final_text: Some(s.to_string()),
                    };
                }
                // Missing summary: the error result nudges a retry next turn.
                continue;
            }

            let is_meta = conversation_mode && meta::is_meta_tool(&tool_name);

            if is_meta {
                if tool_name == "note" {
                    had_note = true;
                }
                let (result, control) =
                    self.dispatch_meta(state, lanes, watches, &tool_name, &call.arguments);
                state.events.push(HarnessEvent::ToolResult {
                    tool_name: tool_name.clone(),
                    result: result.clone(),
                });
                state.messages.push(HarnessMessage::ToolResult {
                    tool_call_id: call_id,
                    tool_name,
                    content: result,
                });
                // ask_user pauses the run here; nothing else ends a turn now.
                if let MetaControl::EndTurn { kind, final_text } = control {
                    return StepResult::TurnEnded { kind, final_text };
                }
                continue;
            }

            if !self.tools.contains(&tool_name) {
                let available = definitions
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                vars.pending_signals.push(RuntimeSignal::UnknownTool {
                    name: tool_name.clone(),
                    available: available.clone(),
                });
                let error = format!("Unknown tool `{tool_name}`. Available tools: {available}");
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {"code": "unknown_tool", "message": error},
                });
                state.events.push(HarnessEvent::InvalidToolCall {
                    tool_name: tool_name.clone(),
                    error,
                });
                state.messages.push(HarnessMessage::ToolResult {
                    tool_call_id: call_id,
                    tool_name,
                    content: result,
                });
                continue;
            }

            // Dedup: re-calling a read-only discovery tool with identical args this
            // request is the classic spinning loop — its result is already in
            // history. Short-circuit with a notice instead of re-running. (A
            // mutation below clears the set, so re-discovery after a change still
            // works; read_file/bash are excluded — re-reads after edits are legit.)
            let signature = format!("{}:{}", tool_name, call.arguments);
            if DEDUP_TOOLS.contains(&tool_name.as_str())
                && vars.executed_calls.contains(&signature)
            {
                // Already ran this exact discovery call; skip re-running it. But we
                // must STILL answer the call_id: an assistant tool_calls message with
                // any unanswered tool_call_id makes strict providers (DeepSeek) 400
                // ("insufficient tool messages"), and the broken turn poisons every
                // later request until compaction.
                dedup_hits += 1;
                let result = json!({
                    "schema_version": 1,
                    "status": "ok",
                    "data": {"skipped": "Identical discovery call already ran this turn — reuse the earlier result instead of repeating it."},
                });
                state.events.push(HarnessEvent::ToolResult {
                    tool_name: tool_name.clone(),
                    result: result.clone(),
                });
                state.messages.push(HarnessMessage::ToolResult {
                    tool_call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    content: result,
                });
                continue;
            }

            real_work_count += 1;

            // Shell discipline: nudge (never block) when `bash` does work a file
            // tool does better. A repeated nudge escalates to reflect-and-switch.
            if tool_name == "bash" {
                if let Some(command) = call.arguments.get("command").and_then(|v| v.as_str()) {
                    if let ShellVerdict::Nudge(message) = classify_shell_command(command) {
                        shell_nudged_this_turn = true;
                        vars.shell_nudge_count += 1;
                        if vars.shell_nudge_count >= SHELL_NUDGE_ESCALATE_AT {
                            vars.pending_signals.push(RuntimeSignal::ShellDisciplineEscalated {
                                count: vars.shell_nudge_count,
                            });
                        } else {
                            vars.pending_signals
                                .push(RuntimeSignal::ShellDiscipline { message });
                        }
                    }
                }
            }

            // Manual mode: pause for the user's approval before a mutating tool runs.
            // Approvals queue across a batch (index/total); Deny skips this one with a
            // denial result so the model adapts. Interrupt cancels the whole step.
            if state.approval_mode == ApprovalMode::Manual
                && MUTATING_TOOLS.contains(&tool_name.as_str())
            {
                approval_index += 1;
                // Discard stale decisions (double-taps, key repeat) queued before
                // this prompt existed — an unconsumed leftover would instantly
                // "approve" an action the user never saw.
                while approval_rx.try_recv().is_ok() {}
                state.events.push(HarnessEvent::ApprovalRequest {
                    tool_name: tool_name.clone(),
                    summary: approval_summary(&tool_name, &call.arguments),
                    index: approval_index,
                    total: total_mutating,
                });
                state.status = HarnessStatus::WaitingForInput;
                let _ = self.persist(state, lanes).await;
                let decision = approval_rx.recv().await;
                state.status = HarnessStatus::Running;
                if matches!(decision, Some(ApprovalDecision::ApproveAll)) {
                    state.approval_mode = ApprovalMode::Auto;
                }
                let approved = matches!(
                    decision,
                    Some(ApprovalDecision::Approve | ApprovalDecision::ApproveAll)
                );
                if !approved {
                    let result = json!({
                        "schema_version": 1,
                        "status": "error",
                        "error": {
                            "code": "user_denied",
                            "message": "The user denied this action. Do not retry it as-is — adjust your approach or ask what they'd prefer."
                        }
                    });
                    state.events.push(HarnessEvent::ToolResult {
                        tool_name: tool_name.clone(),
                        result: result.clone(),
                    });
                    state.messages.push(HarnessMessage::ToolResult {
                        tool_call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        content: result,
                    });
                    continue;
                }
            }

            // Surface the in-flight call before running it so a slow tool (bash,
            // web fetch) isn't a black box — the TUI shows this ToolCall with a
            // "running" indicator until its result lands. Best-effort; the
            // end-of-step persist in the run loop is authoritative.
            let _ = self.persist(state, lanes).await;

            let result = match self
                .tools
                .execute(&self.context, &tool_name, call.arguments)
                .await
            {
                Ok(result) => result.value,
                Err(error) => json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "tool_execution_error",
                        "message": error.to_string(),
                    }
                }),
            };
            if result.get("status").and_then(Value::as_str) == Some("error") {
                failed_results += 1;
            }
            // A mutation may have changed the workspace, so prior discovery results
            // are stale — re-discovery is legitimate again; clear the dedup set.
            // Otherwise remember this discovery call so an exact repeat is caught.
            if MUTATING_TOOLS.contains(&tool_name.as_str()) {
                vars.executed_calls.clear();
            } else if DEDUP_TOOLS.contains(&tool_name.as_str()) {
                vars.executed_calls.insert(signature);
            }
            state.events.push(HarnessEvent::ToolResult {
                tool_name: tool_name.clone(),
                result: result.clone(),
            });
            state.messages.push(HarnessMessage::ToolResult {
                tool_call_id: call_id,
                tool_name,
                content: result,
            });
        }

        // A turn with no shell nudge breaks the escalation streak.
        if !shell_nudged_this_turn {
            vars.shell_nudge_count = 0;
        }

        // Record whether THIS turn repeated a call (dedup-caught or the exact same
        // batch as last turn), so next turn's live context explains the re-prompt
        // only when actually looping.
        vars.last_turn_had_repeat = dedup_hits > 0 || vars.repeated_tool_count > 0;

        // Backpressure on very large single-turn fan-outs.
        if real_work_count >= LARGE_TOOL_BATCH {
            vars.pending_signals.push(RuntimeSignal::BatchBackpressure {
                batch_size: real_work_count,
            });
        }

        // Stuck detection: a turn where every executed call failed doesn't loop
        // forever on the same broken approach — after a couple of those, steer the
        // model to re-think creatively or ask for help (`ask_user` in conversation).
        if real_work_count > 0 && failed_results >= real_work_count {
            vars.consecutive_failed_turns += 1;
            if vars.consecutive_failed_turns >= 2 {
                vars.pending_signals.push(RuntimeSignal::StuckEscalation {
                    failed_turns: vars.consecutive_failed_turns,
                    can_ask_user: conversation_mode,
                });
            }
        } else if real_work_count > 0 {
            vars.consecutive_failed_turns = 0;
        }

        // Productivity accounting: real work resets the streaks; a turn that only
        // took notes (or only hit unknown tools) is unproductive and is nudged
        // toward action, then wrapped up by the top-of-step backstop.
        if real_work_count > 0 {
            vars.unproductive_turns = 0;
            vars.consecutive_note_count = 0;
        } else {
            vars.unproductive_turns += 1;
            if had_note {
                vars.consecutive_note_count += 1;
                if vars.consecutive_note_count >= NOTE_LOOP_AT {
                    vars.pending_signals.push(RuntimeSignal::NoteLoop {
                        count: vars.consecutive_note_count,
                    });
                }
            }
        }

        StepResult::Continue
    }

    /// Dispatch a conversation meta tool. Pushes any specialized events / pending
    /// state, returns the tool-result value and how the turn should proceed.
    fn dispatch_meta(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        tool_name: &str,
        arguments: &Value,
    ) -> (Value, MetaControl) {
        match tool_name {
            "note" => {
                let entry = arguments
                    .get("entry")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(entry) = entry else {
                    return (
                        tool_error("note requires a non-empty `entry`."),
                        MetaControl::Continue,
                    );
                };
                state.events.push(HarnessEvent::Note {
                    entry: entry.to_string(),
                });
                (
                    json!({"schema_version": 1, "status": "success", "data": {"noted": true}}),
                    MetaControl::Continue,
                )
            }
            "complete_goal" => {
                let summary = arguments
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match state.goal.as_mut() {
                    Some(goal) if goal.status == GoalStatus::Active => {
                        goal.status = GoalStatus::Complete;
                        let text = goal.text.clone();
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "goal_completed".to_string(),
                            reasoning: if summary.is_empty() { text } else { summary.clone() },
                        });
                        (
                            json!({"schema_version": 1, "status": "success", "data": {"completed": true}}),
                            MetaControl::EndTurn {
                                kind: TurnEndKind::Complete,
                                final_text: Some(if summary.is_empty() {
                                    "Goal complete.".to_string()
                                } else {
                                    summary
                                }),
                            },
                        )
                    }
                    _ => (
                        tool_error("complete_goal: there is no active goal to complete."),
                        MetaControl::Continue,
                    ),
                }
            }
            "ask_user" => match parse_ask_user(arguments) {
                Ok(rendered) => {
                    let prompt_text = first_question_text(&rendered)
                        .or_else(|| {
                            rendered
                                .get("context")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "Waiting for your input.".to_string());
                    state.events.push(HarnessEvent::UserQuestion {
                        questions: rendered.clone(),
                    });
                    state.pending_question = Some(rendered);
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"asked": true}}),
                        MetaControl::EndTurn {
                            kind: TurnEndKind::Ask,
                            final_text: Some(prompt_text),
                        },
                    )
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "delegate_task" => match parse_delegate_brief(arguments) {
                Ok(brief) => {
                    // Follow-up to an existing lane: resume it with the new brief,
                    // context intact.
                    if let Some(lane_id) = brief.lane_id.as_deref() {
                        return match lanes.follow_up(lane_id, &brief.description) {
                            Ok(title) => {
                                state.events.push(HarnessEvent::LaneSpawned {
                                    id: lane_id.to_string(),
                                    title: title.clone(),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "continued": true,
                                            "lane_id": lane_id,
                                            "title": title,
                                            "note": "Lane resumed with its prior context; its report will arrive as a [lane_report] message.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        };
                    }
                    match lanes.spawn(&brief.title, &brief.description, brief.read_only) {
                        Ok(id) => {
                            state.events.push(HarnessEvent::LaneSpawned {
                                id: id.clone(),
                                title: brief.title.clone(),
                            });
                            (
                                json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": {
                                        "delegated": true,
                                        "lane_id": id,
                                        "title": brief.title,
                                        "access": if brief.read_only { "read_only" } else { "full" },
                                        "note": "Lane runs in the background; its report will arrive as a [lane_report] message. Follow up later by re-calling delegate_task with this lane_id.",
                                    }
                                }),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    }
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "monitor" => {
                let action = arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("add");
                match action {
                    "add" => {
                        let Some(path) = arguments.get("path").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
                            return (tool_error("monitor add requires a `path`."), MetaControl::Continue);
                        };
                        let label = arguments
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(path);
                        let filter = arguments.get("filter").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
                        match watches.add(path, label, filter) {
                            Ok(record) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_added".to_string(),
                                    reasoning: format!("watching \"{}\" ({})", record.label, record.path),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "watching": true,
                                            "watch_id": record.id,
                                            "label": record.label,
                                            "path": record.path,
                                            "note": "Tailing from the current end of file. Appended text arrives as a [file_watch] message (debounced per burst); ending your turn is how you wait for it.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "remove" => {
                        let key = arguments
                            .get("watch_id")
                            .or_else(|| arguments.get("path"))
                            .or_else(|| arguments.get("label"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let Some(key) = key else {
                            return (tool_error("monitor remove needs a `watch_id`, `path`, or `label`."), MetaControl::Continue);
                        };
                        match watches.remove(key) {
                            Ok(label) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_removed".to_string(),
                                    reasoning: format!("stopped watching \"{label}\""),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": { "removed": true, "label": label }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "list" => (
                        json!({
                            "schema_version": 1,
                            "status": "success",
                            "data": {
                                "watches": watches.records().iter().map(|r| json!({
                                    "watch_id": r.id,
                                    "label": r.label,
                                    "path": r.path,
                                    "filter": r.filter,
                                })).collect::<Vec<_>>(),
                            }
                        }),
                        MetaControl::Continue,
                    ),
                    other => (
                        tool_error(format!("monitor action must be add | remove | list, got `{other}`.")),
                        MetaControl::Continue,
                    ),
                }
            }
            other => (
                tool_error(format!("`{other}` is not a recognized meta tool.")),
                MetaControl::Continue,
            ),
        }
    }

    fn definitions_for(
        &self,
        conversation_mode: bool,
        goal_active: bool,
    ) -> Vec<crate::llm::NativeToolDefinition> {
        let mut definitions = self.tools.definitions();
        if conversation_mode {
            // User-facing: meta tools (note/ask_user/delegate); no terminate tool —
            // a plain reply ends the turn. `complete_goal` is added only while a goal runs.
            definitions.extend(meta::conversation_meta_definitions(goal_active));
        } else {
            // Headless (lanes / one-shot run): an explicit terminate_loop carries a
            // structured summary back to the caller.
            definitions.push(meta::terminate_loop_tool());
        }
        definitions
    }

    async fn recover(
        &self,
        state: &mut HarnessState,
        consecutive_errors: &mut usize,
    ) -> RecoveryAction {
        *consecutive_errors += 1;
        let max = self.config.max_consecutive_recovery;
        if *consecutive_errors > max {
            return RecoveryAction::GiveUp;
        }
        if max > 0 && *consecutive_errors == max {
            // Last chance: inject a runtime correction, then let the loop retry.
            let correction = RuntimeCorrectionKind::LlmRequestFailed;
            state.events.push(HarnessEvent::SystemDecision {
                step: correction.step().to_string(),
                reasoning: correction.reasoning().to_string(),
            });
            state.messages.push(HarnessMessage::System {
                content: correction.reasoning().to_string(),
            });
            return RecoveryAction::Retry;
        }
        sleep(backoff_delay(
            *consecutive_errors,
            self.config.recovery_base_ms,
            self.config.recovery_max_ms,
        ))
        .await;
        RecoveryAction::Retry
    }

    async fn load_or_initialize_state(
        &self,
        user_request: Option<String>,
    ) -> Result<HarnessState, ToolError> {
        // Build the per-workspace memory block once and fold it into the system
        // prefix, so it rides in the cached prompt and refreshes every session
        // (including resume). Within a session it stays fixed; mid-session writes
        // are visible to the agent only on the next start (cache-stable by design).
        let seeded_system = {
            let block = if self.config.memory_enabled {
                crate::memory::render_session_memory(
                    self.context.workspace_root(),
                    self.config.memory_index_budget_chars,
                )
            } else {
                None
            };
            match block {
                Some(b) => format!("{}\n\n{}", self.config.system_prompt, b),
                None => self.config.system_prompt.clone(),
            }
        };

        if self.config.resume
            && let Some(path) = &self.config.state_path
            && tokio::fs::try_exists(path).await?
        {
            let bytes = tokio::fs::read(path).await?;
            // A state file saved by an older build may be unreadable. Don't fail
            // the run — fall through and start a fresh session, overwriting it.
            match deserialize_state(&bytes) {
                Ok(mut state) => {
                    // Reflect the current run's folder (backfills pre-field states).
                    state.workspace = self.context.workspace_root().display().to_string();
                    state.context_window = self.config.context_window_tokens;
                    // Refresh the system prefix so resumed sessions pick up the
                    // latest workspace memory (guarded: no-op if messages[0] isn't System).
                    if let Some(HarnessMessage::System { content }) = state.messages.first_mut() {
                        *content = seeded_system.clone();
                    }
                    // tokio tasks don't survive a process restart; surface lost lanes.
                    for lane in state.lanes.iter_mut() {
                        if lane.status == LaneStatus::Running {
                            lane.status = LaneStatus::Failed;
                            lane.error =
                                Some("lane lost on resume (process restarted)".to_string());
                        }
                    }
                    // A crash mid tool-batch persists an assistant `tool_calls`
                    // message whose later calls never got results; strict providers
                    // (Anthropic, DeepSeek) 400 on that history forever after.
                    // Repair on load so a resumed session is always well-formed.
                    repair_unanswered_tool_calls(&mut state.messages);
                    if let Some(request) =
                        user_request.map(|r| r.trim().to_string()).filter(|r| !r.is_empty())
                    {
                        state.status = HarnessStatus::Running;
                        state.final_text = None;
                        state.pending_question = None;
                        state.messages.push(HarnessMessage::User {
                            content: request.clone(),
                        });
                        state.events.push(HarnessEvent::UserInput { text: request });
                        self.persist_state(&state).await?;
                    }
                    return Ok(state);
                }
                Err(err) => {
                    self.debug_log(&format!("resume: ignoring unreadable state file: {err}"));
                }
            }
        }

        // Fresh session in this folder: keep snippet's `.snippet/` workspace scratch
        // (bg processes, lanes) out of the user's git history. Main agent only —
        // lanes share the workspace and would just race on the same file.
        if self.context.owner() == "main" {
            ensure_snippet_gitignored(self.context.workspace_root());
        }

        let now = Utc::now().to_rfc3339();
        let request = user_request.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
        let mut messages = vec![HarnessMessage::System {
            content: seeded_system,
        }];
        let mut events = Vec::new();
        let (status, user_request) = match request {
            Some(text) => {
                messages.push(HarnessMessage::User {
                    content: text.clone(),
                });
                events.push(HarnessEvent::UserInput { text: text.clone() });
                (HarnessStatus::Running, text)
            }
            None => (HarnessStatus::Idle, String::new()),
        };
        let state = HarnessState {
            version: 1,
            status,
            created_at: now.clone(),
            updated_at: now,
            workspace: self.context.workspace_root().display().to_string(),
            user_request,
            title: None,
            goal: None,
            compacting: false,
            watches: Vec::new(),
            messages,
            events,
            iterations: 0,
            final_text: None,
            lanes: Vec::new(),
            pending_question: None,
            approval_mode: if self.config.manual_approval {
                ApprovalMode::Manual
            } else {
                ApprovalMode::Auto
            },
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_prompt_tokens: 0,
            cache_read_tokens: 0,
            checkpoints: Vec::new(),
            rate_limit: None,
            context_window: self.config.context_window_tokens,
        };
        self.persist_state(&state).await?;
        Ok(state)
    }

    async fn persist(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
    ) -> Result<(), ToolError> {
        state.lanes = lanes.records().to_vec();
        self.persist_state(state).await
    }

    async fn compact_history_if_needed(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
    ) -> Result<(), ToolError> {
        self.compact_history_agentic(model, state, false).await
    }

    async fn compact_history(
        &self,
        state: &mut HarnessState,
        force: bool,
    ) -> Result<(), ToolError> {
        const RECENT_DETAIL_KEEP: usize = 12;
        const MIN_COMPACTABLE_MESSAGES: usize = 18;
        const MAX_SECTION_ITEMS: usize = 18;
        const MAX_COMPACTION_PASSES: usize = 4;

        let window = self.config.context_window_tokens.max(1);
        let threshold = window
            .saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64)
            / 100;
        if !force && (threshold == 0 || state.last_prompt_tokens < threshold) {
            return Ok(());
        }

        let system_prompt = match state.messages.first() {
            Some(HarnessMessage::System { content }) => content.clone(),
            _ => return Ok(()),
        };

        let last_summary_index = state
            .messages
            .iter()
            .rposition(|message| matches!(message, HarnessMessage::Summary { kind, .. } if kind == "compacted_window"));
        let window_start = last_summary_index.map(|idx| idx + 1).unwrap_or(1);
        if state.messages.len().saturating_sub(window_start) <= MIN_COMPACTABLE_MESSAGES {
            return Ok(());
        }

        let preview_text = |text: &str, limit: usize| -> String {
            let trimmed = text.trim();
            let snippet: String = trimmed.chars().take(limit).collect();
            if trimmed.chars().count() > limit {
                format!("{snippet}…")
            } else {
                snippet
            }
        };

        let preview_json = |value: &Value, limit: usize| -> String {
            let raw = serde_json::to_string(value).unwrap_or_default();
            preview_text(&raw, limit)
        };

        let summarize_window = |messages: &[HarnessMessage], user_request: &str| -> (String, String, usize) {
            let mut objective = Vec::new();
            let mut actions = Vec::new();
            let mut outcomes = Vec::new();
            let mut decisions = Vec::new();
            let mut errors_open = Vec::new();

            let push_unique = |items: &mut Vec<String>, value: String, max: usize| {
                let trimmed = value.trim();
                if trimmed.is_empty() || items.len() >= max {
                    return;
                }
                if !items.iter().any(|existing| existing == trimmed) {
                    items.push(trimmed.to_string());
                }
            };

            for message in messages {
                match message {
                    HarnessMessage::User { content } => {
                        let text = preview_text(content, 320);
                        push_unique(
                            &mut objective,
                            format!("- USER {text}"),
                            MAX_SECTION_ITEMS,
                        );
                    }
                    HarnessMessage::Assistant { content, tool_calls } => {
                        let content = content.trim();
                        if !content.is_empty() {
                            push_unique(
                                &mut actions,
                                format!("- Assistant reply: {}", preview_text(content, 320)),
                                MAX_SECTION_ITEMS,
                            );
                        }
                        for call in tool_calls {
                            push_unique(
                                &mut actions,
                                format!(
                                    "- Tool call: {} args={}",
                                    call.name,
                                    preview_json(&call.arguments, 220)
                                ),
                                MAX_SECTION_ITEMS,
                            );
                        }
                    }
                    HarnessMessage::ToolResult { tool_name, content, .. } => {
                        let rendered = preview_json(content, 420);
                        push_unique(
                            &mut outcomes,
                            format!("- Tool result {tool_name}: {rendered}"),
                            MAX_SECTION_ITEMS,
                        );
                        if rendered.to_ascii_lowercase().contains("\"status\":\"error\"")
                            || rendered.to_ascii_lowercase().contains("\"status\": \"error\"")
                            || rendered.to_ascii_lowercase().contains("\"error\"")
                        {
                            push_unique(
                                &mut errors_open,
                                format!("- {tool_name}: {rendered}"),
                                MAX_SECTION_ITEMS,
                            );
                        }
                    }
                    HarnessMessage::Summary { kind, content } => {
                        push_unique(
                            &mut outcomes,
                            format!("- Prior {kind}: {}", preview_text(content, 360)),
                            MAX_SECTION_ITEMS,
                        );
                    }
                    HarnessMessage::System { content } => {
                        let text = content.trim();
                        if text.is_empty() {
                            continue;
                        }
                        if text.starts_with("[Recent activity orientation") {
                            push_unique(
                                &mut decisions,
                                format!("- Orientation: {}", preview_text(text, 320)),
                                MAX_SECTION_ITEMS,
                            );
                        } else if text.starts_with("[Compressed prior thread history") {
                            push_unique(
                                &mut outcomes,
                                format!("- Prior archive: {}", preview_text(text, 320)),
                                MAX_SECTION_ITEMS,
                            );
                        } else {
                            push_unique(
                                &mut decisions,
                                format!("- System: {}", preview_text(text, 320)),
                                MAX_SECTION_ITEMS,
                            );
                        }
                    }
                }
            }

            if objective.is_empty() && !user_request.trim().is_empty() {
                objective.push(format!("- Original request: {}", preview_text(user_request, 320)));
            }

            let section = |title: &str, items: &[String]| -> String {
                if items.is_empty() {
                    format!("{title} = \"\"\n")
                } else {
                    format!("{title} = \"\"\"\n{}\n\"\"\"\n", items.join("\n"))
                }
            };

            // Order sections by importance (objective → decisions → open errors →
            // outcomes → actions) so the budget trim drops the least-critical detail.
            let mut summary = format!(
                "[compacted_window]\n{}{}{}{}{}",
                section("objective", &objective),
                section("decisions", &decisions),
                section("errors_open", &errors_open),
                section("outcomes", &outcomes),
                section("actions", &actions),
            );
            // The compacted window targets ~6k tokens so it stays cheap to carry
            // forward. Approximate at ~3.5 chars/token and trim the tail if over.
            const COMPACTION_BUDGET_CHARS: usize = 21_000;
            if summary.chars().count() > COMPACTION_BUDGET_CHARS {
                summary = summary.chars().take(COMPACTION_BUDGET_CHARS).collect::<String>()
                    + "\n…[compacted summary trimmed to fit the 6k-token budget]";
            }

            let recent_start = messages.len().saturating_sub(RECENT_DETAIL_KEEP);
            let recent_tail = &messages[recent_start..];
            let mut recent = String::from(
                "[Recent activity orientation — keep this in mind while continuing the thread]\n",
            );
            for message in recent_tail.iter().take(8) {
                let line = match message {
                    HarnessMessage::User { content } => format!("- user: {}", preview_text(content, 220)),
                    HarnessMessage::Assistant { content, tool_calls } => {
                        let content = content.trim();
                        if !content.is_empty() {
                            format!("- assistant: {}", preview_text(content, 220))
                        } else if let Some(call) = tool_calls.first() {
                            format!(
                                "- assistant tool_call {}: {}",
                                call.name,
                                preview_json(&call.arguments, 180)
                            )
                        } else {
                            continue;
                        }
                    }
                    HarnessMessage::ToolResult { tool_name, content, .. } => {
                        format!("- tool {tool_name}: {}", preview_json(content, 220))
                    }
                    HarnessMessage::Summary { kind, content } => {
                        format!("- {kind}: {}", preview_text(content, 220))
                    }
                    HarnessMessage::System { content } => format!("- system: {}", preview_text(content, 220)),
                };
                recent.push_str(&line);
                recent.push('\n');
            }

            (summary, recent, recent_tail.len())
        };

        let preserved_prefix: Vec<HarnessMessage> = state.messages[..window_start].to_vec();
        let mut working: Vec<HarnessMessage> = state.messages[window_start..].to_vec();
        let mut pass_count = 0usize;
        let mut preserved_recent_count = 0usize;
        let mut ran = false;

        while working.len() > MIN_COMPACTABLE_MESSAGES && pass_count < MAX_COMPACTION_PASSES {
            pass_count += 1;
            ran = true;
            state.events.push(HarnessEvent::SystemDecision {
                step: "history_compaction_pass".to_string(),
                reasoning: format!(
                    "Compaction pass {}: condensing {} new history messages since the last summary while preserving a recent verbatim tail.",
                    pass_count,
                    working.len()
                ),
            });
            // Don't split between a tool call and its results — that orphans the
            // function_call_output and providers reject it.
            let mut split_at = working.len().saturating_sub(RECENT_DETAIL_KEEP);
            while split_at > 0 && matches!(working[split_at], HarnessMessage::ToolResult { .. }) {
                split_at -= 1;
            }
            if split_at == 0 {
                break;
            }
            let older = working[..split_at].to_vec();
            let recent_tail = working[split_at..].to_vec();
            if older.is_empty() {
                break;
            }

            let (summary, recent, recent_len) = summarize_window(&older, &state.user_request);
            preserved_recent_count = recent_len;

            let mut next = vec![
                HarnessMessage::Summary {
                    kind: "compacted_window".to_string(),
                    content: summary,
                },
                HarnessMessage::Summary {
                    kind: "recent_activity".to_string(),
                    content: recent,
                },
            ];
            next.extend(recent_tail.iter().cloned());

            let tail_has_user = recent_tail
                .iter()
                .any(|m| matches!(m, HarnessMessage::User { .. }));
            if !tail_has_user {
                if let Some(last_user) = state.messages.iter().rev().find_map(|m| match m {
                    HarnessMessage::User { content } => Some(content.clone()),
                    _ => None,
                }) {
                    next.push(HarnessMessage::User { content: last_user });
                }
            }

            if next.len() >= working.len() {
                break;
            }
            working = next;
        }

        if !ran {
            return Ok(());
        }

        let mut messages = vec![HarnessMessage::System {
            content: system_prompt,
        }];
        messages.extend(preserved_prefix.into_iter().skip(1));
        messages.extend(working);
        state.messages = messages;
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compacted".to_string(),
            reasoning: format!(
                "Compacted history after prompt usage reached {} / {} tokens ({}%) in {} pass(es); preserved {} recent messages verbatim and compacted only the post-summary window.",
                state.last_prompt_tokens,
                window,
                self.config.compact_at_pct,
                pass_count,
                preserved_recent_count
            ),
        });
        // Prune events to this compaction boundary (see compact_history_agentic).
        if let Some(i) = state.events.iter().rposition(
            |e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"),
        ) {
            state.events.drain(..i);
        }
        // Reset ONLY the current-context gauge — the cumulative session counters
        // (prompt/completion/total) reflect everything sent and are unaffected by
        // compaction. The gauge repopulates from the next response's usage.
        state.last_prompt_tokens = 0;
        Ok(())
    }

    /// Agentic compaction: the model maintains ONE living "context table", updating
    /// its sections from the prior table + new activity, fit to a ~6k-token budget.
    /// Falls back to the heuristic compaction if the model is unavailable.
    async fn compact_history_agentic(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        force: bool,
    ) -> Result<(), ToolError> {
        const RECENT_FOCUS: usize = 12;
        const MIN_COMPACTABLE_MESSAGES: usize = 14;

        let window = self.config.context_window_tokens.max(1);
        let threshold = window
            .saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64)
            / 100;
        if !force && (threshold == 0 || state.last_prompt_tokens < threshold) {
            return Ok(());
        }

        let Some(HarnessMessage::System { content: system_prompt }) = state.messages.first().cloned()
        else {
            return Ok(());
        };

        let total = state.messages.len();
        if !force && total.saturating_sub(1) <= MIN_COMPACTABLE_MESSAGES {
            return Ok(());
        }

        // The prior living table, carried forward and updated (not chained).
        let prior_table = state
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                HarnessMessage::Summary { kind, content } if kind == "compacted_window" => {
                    Some(content.clone())
                }
                _ => None,
            })
            .unwrap_or_default();

        // Summarize the ENTIRE conversation (minus the system prompt and the prior
        // table) into one table — no verbatim tail kept. The summarizer is told to
        // capture the most recent messages in extra detail.
        let older: Vec<HarnessMessage> = state.messages[1..]
            .iter()
            .filter(|m| !matches!(m, HarnessMessage::Summary { kind, .. } if kind == "compacted_window"))
            .cloned()
            .collect();
        if older.is_empty() {
            return Ok(());
        }

        // Surface the compaction animation while the summarizer works.
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compaction_pass".to_string(),
            reasoning: format!("Compacting {} messages into the context table.", older.len()),
        });
        let _ = self.persist_state(state).await;

        // Run the summarizer/reflection tool-loops with reasoning turned OFF:
        // they're mechanical (fill a structured table, then write memory entries),
        // so chain-of-thought here just burns time. This matters most on the
        // ChatGPT/Codex backend — it rejects "minimal", so "low" still reasons on
        // every one of the ~9 calls (up to 4 summary + 5 reflection turns), which
        // is what turned compaction into minutes. "off" makes normalize_effort
        // drop the reasoning field entirely (Anthropic/Gemini likewise skip
        // thinking). Restore the session's effort before returning either way.
        let prev_effort = model.swap_reasoning_effort(Some("off".to_string()));

        let window_text = render_window(&prior_table, &older, &state.user_request, RECENT_FOCUS);
        let table = match self.run_agentic_summary(model, &window_text).await {
            Ok(table) => table,
            // Model unavailable / failed — fall back to the heuristic compaction.
            // Note: the heuristic path does NOT run the memory reflection pass.
            Err(e) => {
                model.swap_reasoning_effort(prev_effort);
                self.debug_log(&format!("agentic summary failed → heuristic compaction (memory reflection skipped): {e}"));
                return self.compact_history(state, force).await;
            }
        };

        // Keep a copy of the fresh table to feed the memory reflection pass below.
        let table_for_memory = table.clone();
        // A trailing user message hasn't been acted on yet (auto-compaction runs
        // between the push and the step) — keep it verbatim, or the request only
        // survives as well as the summarizer happened to capture it.
        let trailing_user: Vec<HarnessMessage> = state
            .messages
            .iter()
            .rev()
            .take_while(|m| matches!(m, HarnessMessage::User { .. }))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        // The whole conversation is now the table — no verbatim tail (except the
        // pending user message(s) above).
        state.messages = vec![
            HarnessMessage::System { content: system_prompt },
            HarnessMessage::Summary { kind: "compacted_window".to_string(), content: table },
        ];
        state.messages.extend(trailing_user);
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compacted".to_string(),
            reasoning: format!(
                "Compacted the full conversation into the context table at {} / {} tokens ({}%), with extra detail on the most recent activity.",
                state.last_prompt_tokens, window, self.config.compact_at_pct
            ),
        });
        // Drop events before this boundary — they're hidden from the UI and absent
        // from model history (which lives in `messages`), so they'd only bloat every
        // persist. This is what keeps a long session from growing unbounded.
        if let Some(i) = state.events.iter().rposition(
            |e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"),
        ) {
            state.events.drain(..i);
        }
        // Learning pass: distill durable facts/playbooks from the just-compacted
        // session into per-workspace memory. Main session only (lanes are read-only,
        // avoids concurrent index writers). Non-fatal — never abort compaction.
        // Skip a pure-conversation window (no tool calls / results): there's no
        // reusable procedure to learn, and skipping saves the reflection round-trips.
        let did_work = older.iter().any(|m| {
            matches!(m, HarnessMessage::ToolResult { .. })
                || matches!(m, HarnessMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty())
        });
        let reflect = self.config.memory_enabled
            && self.config.memory_reflect_on_compaction
            && self.context.owner() == "main";
        if reflect && did_work {
            if let Err(e) = self.run_memory_reflection(model, &table_for_memory).await {
                self.debug_log(&format!("memory reflection failed (non-fatal): {e}"));
            }
        } else if reflect {
            self.debug_log("memory reflection skipped: no tool work in the compacted window");
        }
        // Restore the session's reasoning effort for the next real model call.
        model.swap_reasoning_effort(prev_effort);
        state.last_prompt_tokens = 0;
        Ok(())
    }

    /// Drive the summarizer worker: it calls `write_section` to fill the living table
    /// and `finalize` to finish; finalize is rejected while a required section is
    /// empty or the table is over the ~6k-token budget, so it progressively
    /// compresses. Returns the assembled `[compacted_window]` table.
    async fn run_agentic_summary(
        &self,
        model: &mut dyn AgentModel,
        window_text: &str,
    ) -> Result<String, ToolError> {
        // The whole table is written in ONE call. Extra turns exist only to fill a
        // missing required section or compress to budget — and a pure over-budget
        // retry re-sends only the (small) table, never the full window again. This
        // is the token fix: the old per-section loop re-sent the entire window on
        // every one of up to 16 turns (~window × 16); now it's ~window × 1.
        const MAX_TURNS: usize = 4;
        const BUDGET_CHARS: usize = 21_000; // ~6k tokens at ~3.5 chars/token

        let tools = summarizer_tools();
        // The last full table we assembled (used for a cheap, window-free trim turn).
        let mut over_budget_table: Option<String> = None;
        let mut feedback = String::new();

        for _turn in 1..=MAX_TURNS {
            // Over-budget retry: hand back only the table to compress — the raw
            // window isn't needed to shrink an existing table, and re-sending it is
            // the whole cost we're avoiding. Every other turn sends the window.
            let user = if let Some(table) = &over_budget_table {
                format!(
                    "Compress this context table to fit the ~6k-token budget. Drop the lowest-value \
                     detail from the largest/oldest sections; keep every exact path, id, error string, \
                     decision, and the recent thread. Return the FULL table via write_table.\n\n\
                     CURRENT TABLE:\n{table}\n\n{feedback}"
                )
            } else {
                format!(
                    "CONVERSATION TO COMPACT — fold ALL of it into one table:\n{window_text}\n\n\
                     Call write_table ONCE with every section filled.{}",
                    if feedback.is_empty() { String::new() } else { format!("\n\n{feedback}") }
                )
            };
            let messages = vec![
                HarnessMessage::System { content: SUMMARIZER_SYSTEM.to_string() },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output.calls.first().filter(|c| c.tool_name == "write_table") else {
                feedback = "Call write_table once, filling every section.".to_string();
                continue;
            };

            // Pull each section out of the single call.
            let mut sections: BTreeMap<&'static str, String> = BTreeMap::new();
            for (name, ..) in SUMMARY_SECTIONS {
                if let Some(v) = call
                    .arguments
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    sections.insert(name, v.to_string());
                }
            }

            let missing = required_missing(&sections);
            if !missing.is_empty() {
                // Model left a required section empty — needs the window again.
                over_budget_table = None;
                feedback = format!(
                    "Required section(s) empty: {}. Fill them from the conversation.",
                    missing.join(", ")
                );
                continue;
            }

            let assembled = assemble_sections(&sections);
            if assembled.chars().count() > BUDGET_CHARS {
                let over = assembled.chars().count() - BUDGET_CHARS;
                over_budget_table = Some(assembled);
                feedback = format!("Currently ~{over} chars over budget.");
                continue;
            }
            return Ok(assembled);
        }

        // Turn cap hit: use the last over-budget table (hard-trimmed) if we have one,
        // else give up so the caller falls back to heuristic compaction.
        if let Some(table) = over_budget_table {
            return Ok(table.chars().take(BUDGET_CHARS).collect::<String>()
                + "\n…[trimmed to the 6k-token budget]");
        }
        Err(ToolError::msg("summarizer did not produce a valid table"))
    }

    /// Learning pass run after compaction: a bounded worker that curates the
    /// per-workspace memory (facts, pointers, how-to playbooks) from the freshly
    /// compacted session table. Mirrors `run_agentic_summary`'s tool-loop shape.
    /// Best-effort: errors are surfaced to the caller, which treats them as non-fatal.
    async fn run_memory_reflection(
        &self,
        model: &mut dyn AgentModel,
        summary_table: &str,
    ) -> Result<(), ToolError> {
        // Tight cap — each turn is a full model round-trip and this runs after
        // every main-session compaction (was 8, then 5). Reflection should converge
        // in 1–2 writes; a higher cap just let the model re-save the same entry
        // over and over until it timed out on the cap.
        const MAX_TURNS: usize = 3;

        let store = crate::memory::MemoryStore::for_workspace(self.context.workspace_root());
        let index_budget = self.config.memory_index_budget_chars;
        let entry_budget = self.config.memory_entry_budget_chars;
        let max_entries = self.config.memory_max_entries;
        let tools = memory_reflector_tools();
        let ws = self.context.workspace_root().display().to_string();
        let mut feedback = "(review the current index/entries, then extract the reusable procedure(s) and key facts)".to_string();
        let mut writes = 0usize;
        self.debug_log(&format!(
            "memory reflection: start (existing entries={}, index={}b)",
            store.list_entries().len(),
            store.read_index().len()
        ));

        for turn in 1..=MAX_TURNS {
            let index = store.read_index();
            let entries = store.list_entries();
            let user = format!(
                "WORKSPACE: {ws}\n\nWHAT JUST HAPPENED (compacted session table):\n{summary_table}\n\nCURRENT MEMORY INDEX:\n{index}\n\nEXISTING ENTRIES: {entries}\n\nLAST RESULT: {feedback}\n\nTurn {turn}/{MAX_TURNS} · {writes} write(s) so far. Make exactly one tool call. Capture what helps a FUTURE session here — the reusable PROCEDURE(s)/playbook(s) this session showed (steps that worked + gotchas) and any durable FACTS/pointers. Write each distinct thing ONCE with memory_write; NEVER re-save or 'polish' an entry you already wrote this pass. Once the durable value is captured (usually 1–2 writes) and the index points to it, call finalize. Only finalize with nothing written if the session was genuinely trivial.",
                index = if index.trim().is_empty() { "(empty)".to_string() } else { index },
                entries = if entries.is_empty() { "(none)".to_string() } else { entries.join(", ") },
                writes = writes,
            );
            let messages = vec![
                HarnessMessage::System { content: MEMORY_REFLECTOR_SYSTEM.to_string() },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output.calls.first() else {
                feedback = "no tool call received — call exactly one tool".to_string();
                continue;
            };
            let arg_str = |k: &str| call.arguments.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            match call.tool_name.as_str() {
                "finalize" => {
                    self.debug_log(&format!("memory reflection: finalized after {writes} write(s)"));
                    return Ok(());
                }
                "memory_read" => {
                    let id = arg_str("id");
                    feedback = match store.read_entry(&id) {
                        Ok(c) => format!("entry `{id}`:\n{c}"),
                        Err(e) => e,
                    };
                }
                "memory_write" => {
                    let id = arg_str("id");
                    let content = arg_str("content");
                    feedback = match store.write_entry(&id, &content, entry_budget, max_entries) {
                        Ok(()) => {
                            writes += 1;
                            self.debug_log(&format!("memory reflection: wrote entry `{id}` ({}b)", content.len()));
                            format!("entry `{id}` saved. Do NOT re-write or 'polish' it. If the index needs it, call memory_index once — then finalize.")
                        }
                        Err(e) => e,
                    };
                }
                "memory_index" => {
                    let content = arg_str("content");
                    feedback = match store.write_index(&content, index_budget) {
                        Ok(()) => {
                            writes += 1;
                            self.debug_log("memory reflection: updated index");
                            "index updated".to_string()
                        }
                        Err(e) => e,
                    };
                }
                "memory_delete" => {
                    let id = arg_str("id");
                    feedback = match store.delete_entry(&id) {
                        Ok(()) => format!("entry `{id}` deleted"),
                        Err(e) => e,
                    };
                }
                other => feedback = format!("unknown tool `{other}`"),
            }
        }
        self.debug_log(&format!("memory reflection: hit turn cap after {writes} write(s)"));
        Ok(())
    }

    /// Append a line to `<state_dir>/debug.log` for tracing model/loop behaviour.
    /// No-op when no state path is configured.
    fn debug_log(&self, line: &str) {
        let Some(path) = self.config.state_path.as_ref() else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        let log_path = dir.join("debug.log");
        let stamp = Utc::now().format("%H:%M:%S%.3f");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            use std::io::Write;
            let _ = writeln!(file, "{stamp} {line}");
        }
    }



    async fn persist_state(&self, state: &HarnessState) -> Result<(), ToolError> {
        let Some(path) = &self.config.state_path else {
            return Ok(());
        };
        let mut state = state.clone();
        state.updated_at = Utc::now().to_rfc3339();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temp_path = temp_state_path(path);
        let bytes = serialize_state(&state).map_err(ToolError::msg)?;
        tokio::fs::write(&temp_path, bytes).await?;
        tokio::fs::rename(&temp_path, path).await?;
        // Tiny metadata sidecar so `list_device_sessions` can skip decompressing
        // every conversation when enumerating (scales to thousands of sessions).
        crate::session::write_session_meta(path, &state);
        Ok(())
    }
}

/// Insert synthetic error results for assistant tool calls that never received
/// one. Every unanswered `tool_call_id` gets a stub result appended right after
/// the contiguous result run that follows its assistant message, preserving the
/// call/result pairing strict providers require.
fn repair_unanswered_tool_calls(messages: &mut Vec<HarnessMessage>) {
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| match m {
            HarnessMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let mut i = 0;
    while i < messages.len() {
        let missing: Vec<(String, String)> = match &messages[i] {
            HarnessMessage::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .filter(|c| !c.id.is_empty() && !answered.contains(&c.id))
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect(),
            _ => Vec::new(),
        };
        // Insertion point: after the results that did land for this turn.
        let mut j = i + 1;
        while j < messages.len() && matches!(messages[j], HarnessMessage::ToolResult { .. }) {
            j += 1;
        }
        for (id, name) in missing.into_iter().rev() {
            messages.insert(
                j,
                HarnessMessage::ToolResult {
                    tool_call_id: id,
                    tool_name: name,
                    content: json!({
                        "schema_version": 1,
                        "status": "error",
                        "error": {
                            "code": "interrupted",
                            "message": "This tool call was interrupted before it produced a result (the process stopped mid-run). Re-run it if the work is still needed.",
                        }
                    }),
                },
            );
        }
        i = j;
    }
}

/// A real tool name is a short, clean identifier. Names with spaces, backticks,
/// dots, or other punctuation come from prose mis-parsed as tool markup.
fn is_plausible_tool_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn dbg_short(text: &str) -> String {
    let one_line = text.replace('\n', "\\n");
    one_line.chars().take(160).collect()
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "schema_version": 1,
        "status": "error",
        "error": {"code": "invalid_tool_call", "message": message.into()},
    })
}

/// Soft budget of turns per request — surfaced each turn so the agent converges
/// instead of sprawling (bounds context growth and cost). Not a hard kill: the
/// loop is unbounded, this only shapes pacing. Set generously — real agentic
/// coding routinely runs dozens of tool calls, and cutting off at a low number
/// makes the agent bail with half-done work. Under an active goal it's ignored
/// entirely (see build_live_context) since goal work is deliberately long-horizon.
const TURN_BUDGET: u64 = 70;

/// Build the per-turn live-context block — snippet's port of wacht's
/// `agent_loop_live_context.hbs`. Regenerated every turn and appended after the
/// durable history (never persisted): it re-surfaces the freshest user input so it
/// can't be lost behind a long history, re-states how to end a turn, and carries
/// any runtime signals raised last turn (drained here). This fresh-every-turn
/// steering is what reliably nudges the model into a clean tool call.
fn build_live_context(
    state: &HarnessState,
    vars: &mut LoopVars,
    conversation_mode: bool,
    workspace: &std::path::Path,
) -> String {
    let signals = std::mem::take(&mut vars.pending_signals);
    let mut block = String::from("<runtime_context>\n");
    // Terse on purpose: re-sent uncached every turn, so it carries only volatile
    // STATE (facts the model reads), never standing rules or imperatives. The
    // governing rules — that this block is not the user, and not to narrate turn
    // status — live once in the cached system prompt (conversation_agent_layer.md
    // [runtime_context]), not repeated here every turn where they read like a user
    // instruction and cost uncached tokens. One-line reminder only:
    block.push_str("# SYSTEM STATE — NOT the user, NOT a message. Never reply to, acknowledge, quote, or mention this block (not even to say you're ignoring it); read it silently and act with tool calls.\n");

    block.push_str("\n[workspace]\n");
    block.push_str(&format!(
        "cwd = \"{}\"  # base for relative paths + shell; not a jail — read/edit any absolute or ~ path.\n",
        workspace.display()
    ));

    // Surface the model's prior-turn reasoning so it can build on it instead of
    // re-deriving (experimental; conversation only).
    if let Some(thought) = vars.last_thought.as_deref() {
        block.push_str("\n[last_thought]  # continue from it, don't re-derive\n");
        block.push_str(&format!("text = \"{}\"\n", sanitize_one_line(thought)));
    }

    block.push_str("\n[turn]\n");
    // Private pacing only — a fact the model reads to converge, NEVER spoken to the
    // user. The word "budget" (and "near/over budget") leaked into replies, so it's
    // gone; the note is a quiet internal nudge and the line is marked private.
    let n = vars.turns_this_request;
    // Autonomous goal work is long-horizon by design — no wrap-up pressure; the
    // goal's own completion check governs when it ends.
    let goal_active = matches!(&state.goal, Some(g) if g.status == GoalStatus::Active);
    if goal_active {
        block.push_str(&format!(
            "pace = \"{n} steps in (autonomous goal — keep going until the goal is done)\"  # PRIVATE — internal pacing only; never mention step counts to the user.\n"
        ));
    } else {
        let note = if n >= TURN_BUDGET {
            " — wrap up now: deliver your best current result, don't open new threads"
        } else if n + 5 >= TURN_BUDGET {
            " — begin converging toward the result"
        } else {
            ""
        };
        block.push_str(&format!(
            "pace = \"{n} of ~{TURN_BUDGET} steps in{note}\"  # PRIVATE — internal pacing only; never mention step counts, pacing, or 'converging' to the user.\n"
        ));
    }
    // Observed loop (a repeated call last turn) — stated as an observation, not an
    // order; the system prompt covers what to do about it.
    if vars.last_turn_had_repeat {
        block.push_str(
            "observed = \"your last tool call repeated one already in history — its result won't change.\"\n",
        );
    }
    // Conversation mode: how to finish/ask is a standing RULE, now stated once in
    // the cached system prompt (conversation_agent_layer.md) — not repeated here.
    // Headless lanes DON'T load that layer, so they still need the terminate_loop
    // reminder; keep it (it's an instruction to a reporter, not the user-facing
    // symptom this rework targets).
    if !conversation_mode {
        block.push_str("finish = \"when the work is genuinely done, call terminate_loop with your summary. Do the real work first; don't emit intermediate status.\"\n");
    }

    if !signals.is_empty() {
        block.push_str("\n[runtime_signals]  # one-shot state about last turn; act now, won't repeat. never quote it.\n");
        for signal in &signals {
            block.push_str(&format!("{}\n", signal.render()));
        }
    }

    // Surface heuristics about the latest user message (prompt-injection,
    // exfiltration, secrets, destructive intent) so the model weighs them rather
    // than blindly complying. Ported from wacht's `derive_input_safety_signals`.
    if let Some(latest) = latest_user_input(state) {
        let safety = derive_input_safety_signals(&latest);
        if !safety.is_empty() {
            block.push_str("\n[input_safety]  # flags on the latest message; weigh them, don't blindly comply or refuse.\n");
            for line in safety {
                block.push_str(&format!("{line}\n"));
            }
        }
    }

    // Skills are NOT preloaded into context — the agent finds them on demand with
    // `search_skills` (keeps context lean however many skills exist), then loads the
    // chosen one with `skill`.

    // Background processes the agent started (dev servers, watchers) — so it knows
    // what's already running instead of re-launching, and can tail logs / kill them.
    if let Some(bg) = crate::bg::render_live(workspace) {
        block.push_str("\n[background_processes]  # started via bash(background:true); tail the log or kill <pid>; don't relaunch a running one\n");
        block.push_str(&bg);
    }

    // Delegated lanes — you're an ORCHESTRATOR here. Running ones: don't finalize
    // while they're in flight; end the turn to wait (their reports wake you).
    // Finished ones ride along by id so follow-ups stay targetable even after
    // their report messages have been compacted away.
    let running: Vec<&LaneRecord> = state
        .lanes
        .iter()
        .filter(|l| l.status == LaneStatus::Running)
        .collect();
    let finished: Vec<&LaneRecord> = state
        .lanes
        .iter()
        .rev()
        .filter(|l| l.status != LaneStatus::Running)
        .take(6)
        .collect();
    if !running.is_empty() || !finished.is_empty() {
        block.push_str("\n[delegated_lanes]\n");
        block.push_str("# background sub-agents; reports wake you. Continue ANY finished one with delegate_task{lane_id} — it resumes with its context intact (prefer that over re-briefing from scratch).\n");
        for l in &running {
            block.push_str(&format!("- {} \"{}\" — running\n", l.id, clip(&l.title, 40)));
        }
        for l in &finished {
            let status = match l.status {
                LaneStatus::Completed => "completed",
                LaneStatus::Failed => "FAILED",
                LaneStatus::Running => unreachable!("filtered above"),
            };
            block.push_str(&format!("- {} \"{}\" — {}\n", l.id, clip(&l.title, 40), status));
        }
        if !running.is_empty() {
            block.push_str(&format!(
                "orchestrate = \"{} lane(s) still working. You're the orchestrator. Ending your turn IS how you wait — you go idle and each lane's report wakes you (no polling, no blocking). A short progress note to the user about what you kicked off is good. Just don't present your COMPLETE/final answer while lanes you need are still out — fold each report in as it lands, then deliver the synthesis (progressively, or all at once when the last is in). Spawn more lanes to keep your own context lean.\"\n",
                running.len()
            ));
        }
    }

    block.push_str("</runtime_context>\n");
    block
}

/// Flag the latest user message for prompt-injection / exfiltration / secret /
/// destructive phrasing (capped at 6). Ported from wacht's
/// `derive_input_safety_signals`.
fn derive_input_safety_signals(input: &str) -> Vec<String> {
    let input_lower = input.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut signals = Vec::new();

    let pattern_checks: [(&str, &str, &[&str]); 5] = [
        (
            "instruction_override",
            "attempt to override system rules detected",
            &[
                "ignore previous instructions",
                "disregard prior instructions",
                "forget all rules",
                "override system prompt",
            ],
        ),
        (
            "prompt_exfiltration",
            "attempt to reveal hidden prompts or internal policy detected",
            &[
                "show system prompt",
                "reveal your prompt",
                "print your instructions",
                "developer instructions",
            ],
        ),
        (
            "safety_bypass",
            "attempt to bypass safety constraints detected",
            &["disable safety", "jailbreak", "bypass policy", "no restrictions"],
        ),
        (
            "secret_exfiltration",
            "request may involve secrets, credentials, or token exfiltration",
            &["api key", "access token", "password", "private key", "secret"],
        ),
        (
            "destructive_operations",
            "potential destructive operation request detected",
            &["drop database", "delete all", "rm -rf", "truncate table", "wipe"],
        ),
    ];

    for (tag, message, phrases) in pattern_checks {
        if phrases.iter().any(|phrase| input_lower.contains(phrase)) && seen.insert(tag) {
            signals.push(format!("{tag} = \"{message}\""));
        }
        if signals.len() >= 6 {
            break;
        }
    }

    signals
}

fn sanitize_one_line(text: &str) -> String {
    let collapsed = text.replace('\n', " ").replace('"', "'");
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most recent user-originated text (a fresh request or a mid-run steer).
fn latest_user_input(state: &HarnessState) -> Option<String> {
    state.events.iter().rev().find_map(|event| match event {
        HarnessEvent::UserInput { text } | HarnessEvent::Steer { text } => Some(text.clone()),
        _ => None,
    })
}

fn first_question_text(rendered: &Value) -> Option<String> {
    rendered
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| question.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn backoff_delay(attempt: usize, base_ms: u64, max_ms: u64) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(7) as u32;
    let delay = base_ms.max(1).saturating_mul(1u64 << shift).min(max_ms.max(1));
    Duration::from_millis(delay)
}

/// Whether the agent has produced a user-visible reply (`AssistantText`) since the
/// most recent user message — i.e. it actually answered, not just took notes / ran
/// tools. Scans events newest-first, stopping at the last user input.
fn replied_since_last_user(events: &[HarnessEvent]) -> bool {
    for e in events.iter().rev() {
        match e {
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. } => return false,
            HarnessEvent::AssistantText { .. } => return true,
            _ => {}
        }
    }
    false
}

/// On a fresh session in a git work tree, make sure snippet's `.snippet/` scratch
/// (bg-process registry, lane state) is gitignored so it never lands in the user's
/// history. Best-effort and idempotent: creates `.gitignore` if it's missing,
/// appends the entry if absent, no-ops if already covered. Skips folders that
/// aren't in a git repo — a `.gitignore` there would be pointless clutter.
fn ensure_snippet_gitignored(workspace: &Path) {
    if !in_git_work_tree(workspace) {
        return;
    }
    let gitignore = workspace.join(".gitignore");
    let covered = |content: &str| {
        content
            .lines()
            .any(|line| matches!(line.trim(), ".snippet" | ".snippet/" | "/.snippet" | "/.snippet/"))
    };
    match std::fs::read_to_string(&gitignore) {
        Ok(content) => {
            if covered(&content) {
                return;
            }
            let mut updated = content;
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(".snippet/\n");
            let _ = std::fs::write(&gitignore, updated);
        }
        // No `.gitignore` yet (or unreadable) — create one with just the entry.
        Err(_) => {
            let _ = std::fs::write(&gitignore, ".snippet/\n");
        }
    }
}

/// Whether `dir` sits inside a git work tree — walk up for a `.git` marker (a dir
/// for a normal clone, a file for a worktree/submodule). No subprocess.
fn in_git_work_tree(dir: &Path) -> bool {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return true;
        }
        cur = d.parent();
    }
    false
}

fn temp_state_path(path: &Path) -> PathBuf {
    let mut temp = path.to_path_buf();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!("{value}.tmp"))
        .unwrap_or_else(|| "tmp".to_string());
    temp.set_extension(extension);
    temp
}

fn normalize_tool_aliases(calls: &mut [GeneratedToolCall]) {
    for call in calls {
        if call.tool_name == "execute_command" {
            call.tool_name = "bash".to_string();
        }
    }
}

pub fn serialize_state(state: &HarnessState) -> Result<Vec<u8>, String> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    // `to_vec_named` encodes structs as field-name → value maps. The positional
    // `to_vec` is NOT safe here: `HarnessState`'s `skip_serializing_if` fields
    // drop array elements when empty, which shifts every later field and breaks
    // the round-trip on read.
    let raw_bytes = rmp_serde::to_vec_named(state)
        .map_err(|e| format!("failed to serialize state to MessagePack: {e}"))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw_bytes)
        .map_err(|e| format!("failed to compress state with Gzip: {e}"))?;
    let compressed_bytes = encoder.finish()
        .map_err(|e| format!("failed to finalize Gzip compression: {e}"))?;
    Ok(compressed_bytes)
}

pub fn deserialize_state(bytes: &[u8]) -> Result<HarnessState, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    // Try parsing as compressed MessagePack first
    let mut decoder = GzDecoder::new(bytes);
    let mut decompressed_bytes = Vec::new();
    if decoder.read_to_end(&mut decompressed_bytes).is_ok() {
        if let Ok(state) = rmp_serde::from_slice::<HarnessState>(&decompressed_bytes) {
            return Ok(state);
        }
    }

    // Fallback: try parsing as legacy JSON
    if let Ok(state) = serde_json::from_slice::<HarnessState>(bytes) {
        return Ok(state);
    }

    Err("failed to deserialize state: not a valid compressed MessagePack or legacy JSON".to_string())
}

// --- Agentic compaction: the living "context table" ---

/// (name, description, required) — the sections the summarizer maintains.
const SUMMARY_SECTIONS: &[(&str, &str, bool)] = &[
    ("objective", "what the user ultimately wants, and for whom", true),
    (
        "state",
        "where things stand now: files changed, what works/doesn't, plus exact paths/IDs/values worth keeping verbatim",
        true,
    ),
    ("actions", "what was actually done, in order — the condensed trail", false),
    ("decisions", "key decisions and user corrections, verbatim where wording matters", false),
    ("open_issues", "exact error strings and genuinely unresolved/open work", false),
    ("next_steps", "what to do next", false),
];

const MEMORY_REFLECTOR_SYSTEM: &str = r#"# memory_reflector
[identity]
role = "worker that curates a coding agent's PERSISTENT, per-workspace memory"
input = "each turn: the workspace path, a compacted table of the session that just ran, the current memory index, the existing entry ids, your last tool result, and the turn counter"
purpose = "carry forward only what will help FUTURE sessions in THIS exact folder"

[what_to_keep]
durable = "stable facts (architecture, where things live, conventions), pointers to key files/resources, and how-to PLAYBOOKS for recurring tasks (the steps that worked + the gotchas)"
learning = "when this session revealed a better way or a pitfall, fold it into the relevant playbook so next time is faster"
skip = "ephemeral task state, one-off details, and anything already obvious from the code — that belongs in the session table, not here"

[how]
entries = "memory_write(id, content) stores a full note under a short kebab-case id; prefer UPDATING an existing entry over creating a near-duplicate (memory_read it first)"
index = "memory_index(content) REPLACES the always-loaded index — keep it lean: one short line per entry (label, one-line summary, id). It must fit its budget; oversize writes are rejected, so compress"
evidence = "exact paths, commands, and IDs verbatim; no speculation, no padding"

[finalize]
write_once = "write each entry ONCE. Do NOT re-save or 'polish' an entry you already wrote in this pass — it changes little and just burns turns. Aim for 1–2 writes total (an entry, then the index), then finalize."
bias_to_capture = "if the session did REAL work (edits, debugging, a build, a multi-step task), it almost always demonstrated a reusable procedure or surfaced a durable fact — write at least one entry before finalizing. Finalizing with nothing written is only correct when the session was genuinely trivial."
when = "finalize as soon as the index and entries reflect the durable procedures/facts from this session — usually within 1–2 writes"
how = "call finalize (one tool call per turn)""#;

fn memory_reflector_tools() -> Vec<crate::llm::NativeToolDefinition> {
    use crate::llm::NativeToolDefinition;
    let id_schema = json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "kebab-case entry id" } },
        "required": ["id"],
        "additionalProperties": false
    });
    vec![
        NativeToolDefinition {
            name: "memory_read".to_string(),
            description: "Read the full content of an existing entry by id.".to_string(),
            input_schema: id_schema.clone(),
        },
        NativeToolDefinition {
            name: "memory_write".to_string(),
            description: "Create or replace an entry (durable fact, pointer, or how-to playbook) under a short kebab-case id.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "kebab-case entry id" },
                    "content": { "type": "string" }
                },
                "required": ["id", "content"],
                "additionalProperties": false
            }),
        },
        NativeToolDefinition {
            name: "memory_index".to_string(),
            description: "Replace the always-loaded index — one short line per entry (label, summary, id). Must fit the budget.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "content": { "type": "string" } },
                "required": ["content"],
                "additionalProperties": false
            }),
        },
        NativeToolDefinition {
            name: "memory_delete".to_string(),
            description: "Delete an entry by id (also drop its line from the index).".to_string(),
            input_schema: id_schema,
        },
        NativeToolDefinition {
            name: "finalize".to_string(),
            description: "Finish — memory reflects all durable learnings from this session.".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
    ]
}

const SUMMARIZER_SYSTEM: &str = r#"# compaction_summarizer
[identity]
role = "you compress a coding agent's whole conversation into ONE dense context table that REPLACES the raw history"
stakes = "the raw messages are then discarded — this table is all that survives. Anything you leave out is lost forever; anything you pad is re-sent on every future turn and wastes tokens. Maximize signal per token."

[preserve]  # carry these forward — verbatim where the exact value/wording matters
goal = "the user's actual objective and any hard constraints or preferences they stated"
state = "the CURRENT state: which files were created/changed, what works, what's broken or unverified"
specifics = "every exact path, identifier, function/type/symbol name, command, config value, URL, version, and error string — copy these literally, never paraphrase them"
decisions = "key decisions and the user's corrections in their OWN words"
open = "genuinely unresolved problems and in-flight work"
next = "the immediate next step, so the agent resumes without re-deriving it"
recent_bias = "spend MORE detail on the most recent activity than on old activity — precise current state + what was mid-flight + the next action; that is what lets the agent continue seamlessly"

[drop]  # do NOT carry these — they are the bulk of the tokens and add nothing
noise = "resolved intermediate steps, superseded/abandoned attempts, verbose tool output (keep the CONCLUSION, not the dump), acknowledgements and chit-chat, restated instructions, and anything trivially re-readable from the code itself"

[method]
fold = "if a PRIOR TABLE is present, update it in place — keep what's still true, add what's new, delete what's stale or superseded; do not just append"
dense = "terse markdown bullets, not prose. Facts and values, not sentences. No preamble, no narration of this process, no filler adjectives."
budget = "the whole table must fit ~6k tokens. If told it is OVER BUDGET, compress the largest/oldest sections first (drop the lowest-value detail) while keeping every exact value and the recent thread — never re-expand."

[how]
one_call = "call write_table ONCE, filling every section from the entire conversation. That single call is the whole job. You are asked again only if you left a required section empty or the table is over budget — otherwise you are done in one shot.""#;

fn summarizer_tools() -> Vec<crate::llm::NativeToolDefinition> {
    // One tool that writes the ENTIRE table in a single call — a string field per
    // section. Filling it in one shot is the whole job (the loop only comes back
    // for a missing required section or a budget trim), which is what keeps the
    // big conversation window from being re-sent turn after turn.
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, desc, req) in SUMMARY_SECTIONS {
        properties.insert(
            name.to_string(),
            json!({ "type": "string", "description": desc }),
        );
        if *req {
            required.push(*name);
        }
    }
    vec![crate::llm::NativeToolDefinition {
        name: "write_table".to_string(),
        description: "Write the ENTIRE context table in one call — fill every section with dense \
            markdown bullets. This table replaces the raw history, so anything you omit is lost. \
            You are only asked again to fill a required section you left empty or to compress to fit \
            the budget."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }]
}

fn toml_block(body: &str) -> String {
    format!("\"\"\"\n{}\n\"\"\"", body.replace("\"\"\"", "'''"))
}


fn required_missing(sections: &BTreeMap<&'static str, String>) -> Vec<&'static str> {
    SUMMARY_SECTIONS
        .iter()
        .filter(|(name, _, req)| {
            *req && sections.get(name).map(|s| s.trim().is_empty()).unwrap_or(true)
        })
        .map(|(n, ..)| *n)
        .collect()
}

fn assemble_sections(sections: &BTreeMap<&'static str, String>) -> String {
    let body = SUMMARY_SECTIONS
        .iter()
        .filter_map(|(name, ..)| {
            sections
                .get(name)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|b| format!("{name} = {}", toml_block(b)))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("[compacted_window]\n{body}")
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        t.chars().take(n).collect::<String>() + "…"
    } else {
        t.to_string()
    }
}

/// Render the prior table + the new activity window as plain text for the summarizer.
/// A short one-line summary of a mutating tool call for the approval prompt:
/// the shell command for `bash`, otherwise the target path.
fn approval_summary(tool_name: &str, args: &Value) -> String {
    let raw = match tool_name {
        "bash" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        _ => args.get("path").and_then(Value::as_str).unwrap_or(""),
    };
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 120 {
        s.chars().take(120).collect::<String>() + "…"
    } else {
        s
    }
}

fn render_window(
    prior_table: &str,
    older: &[HarnessMessage],
    user_request: &str,
    recent_focus: usize,
) -> String {
    let mut out = String::new();
    if !prior_table.trim().is_empty() {
        out.push_str("PRIOR TABLE (update this in place):\n");
        out.push_str(prior_table.trim());
        out.push_str("\n\n");
    } else if !user_request.trim().is_empty() {
        out.push_str(&format!("ORIGINAL REQUEST: {}\n\n", clip(user_request, 600)));
    }
    out.push_str("CONVERSATION TO SUMMARIZE:\n");
    // Overall budget: this window is re-sent on EVERY summarizer turn (up to 16),
    // so an uncapped render of a huge history multiplies into serious input cost.
    // Render newest-first mentally: when over budget, elide the OLDEST lines —
    // the prior table already carries the old context.
    const WINDOW_BUDGET_CHARS: usize = 120_000; // ~35k tokens
    let mut rendered: Vec<String> = Vec::with_capacity(older.len());
    // Mark the most recent messages so the summarizer captures them in extra detail.
    let focus_start = older.len().saturating_sub(recent_focus);
    for (i, m) in older.iter().enumerate() {
        if i == focus_start && focus_start > 0 {
            rendered.push(
                "\n=== MOST RECENT ACTIVITY — capture this in extra detail (what just happened, current state, next steps) ===".to_string(),
            );
        }
        let line = match m {
            HarnessMessage::User { content } => format!("USER: {}", clip(content, 600)),
            HarnessMessage::Assistant { content, tool_calls } => {
                let mut s = String::new();
                if !content.trim().is_empty() {
                    s.push_str(&format!("ASSISTANT: {}", clip(content, 600)));
                }
                for c in tool_calls {
                    s.push_str(&format!("\nTOOL_CALL {}({})", c.name, clip(&c.arguments.to_string(), 300)));
                }
                s
            }
            HarnessMessage::ToolResult { tool_name, content, .. } => {
                format!("TOOL_RESULT {tool_name}: {}", clip(&content.to_string(), 600))
            }
            HarnessMessage::Summary { kind, content } => format!("[{kind}] {}", clip(content, 600)),
            HarnessMessage::System { content } => format!("SYSTEM: {}", clip(content, 300)),
        };
        if !line.trim().is_empty() {
            rendered.push(line);
        }
    }
    let total: usize = rendered.iter().map(|l| l.chars().count() + 1).sum();
    if total > WINDOW_BUDGET_CHARS {
        // Drop oldest lines until the window fits; note the elision once.
        let mut over = total - WINDOW_BUDGET_CHARS;
        let mut skip = 0usize;
        for line in &rendered {
            if over == 0 {
                break;
            }
            over = over.saturating_sub(line.chars().count() + 1);
            skip += 1;
        }
        out.push_str(&format!(
            "[…{skip} oldest message(s) elided to fit the window budget — rely on the PRIOR TABLE for that span]\n"
        ));
        for line in &rendered[skip..] {
            out.push_str(line);
            out.push('\n');
        }
    } else {
        for line in &rendered {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod gitignore_tests {
    use super::{ensure_snippet_gitignored, in_git_work_tree};

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    #[test]
    fn creates_gitignore_when_missing() {
        let repo = git_repo();
        ensure_snippet_gitignored(repo.path());
        let gi = std::fs::read_to_string(repo.path().join(".gitignore")).unwrap();
        assert_eq!(gi, ".snippet/\n");
    }

    #[test]
    fn appends_entry_and_preserves_prior_lines() {
        let repo = git_repo();
        let gi = repo.path().join(".gitignore");
        std::fs::write(&gi, "target/\nnode_modules/").unwrap(); // no trailing newline
        ensure_snippet_gitignored(repo.path());
        let out = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(out, "target/\nnode_modules/\n.snippet/\n");
    }

    #[test]
    fn idempotent_when_already_covered() {
        let repo = git_repo();
        let gi = repo.path().join(".gitignore");
        std::fs::write(&gi, "target/\n.snippet\n").unwrap(); // bare form counts
        ensure_snippet_gitignored(repo.path());
        let out = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(out, "target/\n.snippet\n"); // unchanged
    }

    #[test]
    fn skips_non_git_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!in_git_work_tree(dir.path()));
        ensure_snippet_gitignored(dir.path());
        assert!(!dir.path().join(".gitignore").exists());
    }

    #[test]
    fn detects_repo_from_subfolder() {
        let repo = git_repo();
        let sub = repo.path().join("crates/inner");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(in_git_work_tree(&sub));
    }
}
