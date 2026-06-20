//! Live-context runtime signals — snippet's port of wacht's `RuntimeSignal`
//! (executor/core.rs).
//!
//! When the model does something off (text with no tool call, an empty turn, the
//! same call repeated, a tool that doesn't exist), the loop raises a typed signal.
//! Signals are *transient*: they are drained into the next turn's freshly-rendered
//! `[live_context]` block as a crisp imperative line, then discarded. They are
//! never written into the durable message history, so they re-ground the model
//! every turn without piling up as stale nudges — which is exactly why wacht steers
//! a flaky model into a clean tool call where snippet's old accumulating nudge let
//! it loop.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSignal {
    /// The previous turn was text with no tool call — it did not end the run.
    CompleteRequired,
    /// The previous turn produced nothing at all (no text, no call).
    EmptyResponse,
    /// The model's previous response was cut off at the token limit.
    ResponseTruncated,
    /// The same tool call was issued several turns running.
    ToolCallLoop { count: usize },
    /// The model called a tool that isn't in the available set.
    UnknownTool { name: String, available: String },
    /// Several tool-only steps with no word to the user.
    VisibilityLapse,
    /// Shell was used to do work a dedicated file tool does better. Carries the
    /// specific guidance for what was detected (redirect / sed -i / tee / cat).
    ShellDiscipline { message: String },
    /// The same shell-discipline nudge fired again — escalate to reflect-and-switch.
    ShellDisciplineEscalated { count: usize },
    /// Several note-only turns in a row with no real work.
    NoteLoop { count: usize },
    /// A very large batch of tool calls was issued in one turn.
    BatchBackpressure { batch_size: usize },
}

impl RuntimeSignal {
    pub fn key(&self) -> &'static str {
        match self {
            Self::CompleteRequired => "terminate_required",
            Self::EmptyResponse => "empty_response",
            Self::ResponseTruncated => "response_truncated",
            Self::ToolCallLoop { .. } => "tool_call_loop",
            Self::UnknownTool { .. } => "unknown_tool",
            Self::VisibilityLapse => "user_visibility",
            Self::ShellDiscipline { .. } => "shell_discipline",
            Self::ShellDisciplineEscalated { .. } => "shell_discipline",
            Self::NoteLoop { .. } => "note_loop",
            Self::BatchBackpressure { .. } => "batch_backpressure",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CompleteRequired =>
                "Internal — never mention this to the user. Your previous turn was text with no tool \
                 call, so the run did not end and your text was already delivered (do not repeat it). \
                 If that text was your complete answer, call `terminate_loop` now (summary only, no new \
                 message). If not, take the next concrete step with a real tool call. Do not narrate \
                 this mechanic or apologize for it — just act."
                    .to_string(),
            Self::EmptyResponse =>
                "previous turn was empty (no tool call, no text). Reply to the user, or take the \
                 next concrete action with a tool call."
                    .to_string(),
            Self::ToolCallLoop { count } => format!(
                "you have issued the same tool call {count} turns in a row; the result will not \
                 change. Change the inputs, use a different tool, or finish with `terminate_loop`."
            ),
            Self::UnknownTool { name, available } => format!(
                "`{name}` is not an available tool. Use one of these by exact name: [{available}]. \
                 If none fit, reply in plain text."
            ),
            Self::VisibilityLapse =>
                "no user-visible message in the last few steps; add one short progress line beside \
                 your next tool call unless it is a tiny read."
                    .to_string(),
            Self::ResponseTruncated =>
                "your previous response was cut off at the output-token limit. It was NOT treated as \
                 final. Continue with a concrete tool call, or keep the next reply shorter so it \
                 completes."
                    .to_string(),
            Self::ShellDiscipline { message } => message.clone(),
            Self::ShellDisciplineEscalated { count } => format!(
                "you have reached for the shell to do file work {count} times now despite the \
                 nudge. Stop and switch: use `read_file`/`write_file`/`edit_file`/`append_file` for \
                 file content; keep the shell for inspection (grep, pipes, find)."
            ),
            Self::NoteLoop { count } => format!(
                "you have written {count} notes in a row without doing any work. Notes do not make \
                 progress. Act now with a real tool call, or finish with `terminate_loop`."
            ),
            Self::BatchBackpressure { batch_size } => format!(
                "you issued {batch_size} tool calls in one turn. Large fan-outs are hard to verify \
                 and recover from — prefer a few focused calls, read the results, then continue."
            ),
        }
    }

    /// One-line `key = "message"` rendering for the live-context block.
    pub fn render(&self) -> String {
        let one_line = self.message().replace('\n', " ").replace('"', "'");
        let one_line = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("{} = \"{one_line}\"", self.key())
    }
}
