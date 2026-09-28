//! Runtime signals: one-shot notes the loop raises when the model does
//! something off (an empty turn, the same call repeated, a tool that doesn't
//! exist). Each is delivered once, in the next step's `<system-reminder>`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSignal {
    /// The previous turn produced nothing at all (no text, no call).
    EmptyResponse,
    /// The model's previous response was cut off at the token limit.
    ResponseTruncated,
    /// The same tool call was issued several turns running.
    ToolCallLoop { count: usize },
    /// The model called a tool that isn't in the available set.
    UnknownTool { name: String, available: String },
    /// Shell was used to do work a dedicated file tool does better. Carries the
    /// specific guidance for what was detected (redirect / sed -i / tee / cat).
    ShellDiscipline { message: String },
    /// The same shell-discipline nudge fired again — escalate to reflect-and-switch.
    ShellDisciplineEscalated { count: usize },
    /// Several note-only turns in a row with no real work.
    NoteLoop { count: usize },
    /// A very large batch of tool calls was issued in one turn.
    BatchBackpressure { batch_size: usize },
    /// Several consecutive turns of failing tool calls (or near the unproductive
    /// backstop): step back and re-think creatively, or ask for help.
    StuckEscalation {
        failed_turns: usize,
        /// Whether `ask_user` is available (conversation mode) — headless lanes
        /// report the blocker instead.
        can_ask_user: bool,
    },
    /// Edits to a file have failed repeatedly (e.g. identical strings or not found).
    StuckEdit { path: String, count: usize },
}

impl RuntimeSignal {
    pub fn message(&self) -> String {
        match self {
            Self::EmptyResponse =>
                "Your previous turn was empty (no text, no tool call). Reply to the user, or take \
                 the next concrete step with a tool call."
                    .to_string(),
            Self::ToolCallLoop { count } => format!(
                "You have issued the same tool call {count} times; its result will not change. \
                 Use the result you already have, change the inputs, or finish and deliver your \
                 conclusion."
            ),
            Self::UnknownTool { name, available } => format!(
                "`{name}` is not an available tool. Use one of these by exact name: [{available}]. \
                 If none fit, reply in plain text."
            ),
            Self::ResponseTruncated =>
                "Your previous response was cut off at the output-token limit and was not treated \
                 as final. Continue with a tool call, or keep the next reply shorter."
                    .to_string(),
            Self::ShellDiscipline { message } => message.clone(),
            Self::ShellDisciplineEscalated { count } => format!(
                "You have used the shell to change files {count} times despite the earlier \
                 note. Stop and switch: change files only with `change_files`; keep the shell for \
                 reading, searching and running things."
            ),
            Self::NoteLoop { count } => format!(
                "You have written {count} notes in a row without doing any work. Notes do not make \
                 progress. Act now with a real tool call, or finish the turn and deliver your \
                 conclusion."
            ),
            Self::BatchBackpressure { batch_size } => format!(
                "You issued {batch_size} tool calls in one turn. Large fan-outs are hard to verify \
                 and recover from — prefer a few focused calls, read the results, then continue."
            ),
            Self::StuckEscalation { failed_turns, can_ask_user } => {
                let escape = if *can_ask_user {
                    "if you genuinely cannot proceed (missing access, credentials, information, or \
                     permission), call `ask_user` and ask for help instead of spinning"
                } else {
                    "if you genuinely cannot proceed, stop and report exactly what is blocking you \
                     and what you'd need to continue"
                };
                format!(
                    "Your last {failed_turns} turns of tool calls all failed — the current approach \
                     is not working. STOP repeating it. Step back: question your assumptions, list \
                     what you know, and try a genuinely different angle (different tool, different \
                     starting point, simplify the step, or investigate the failure itself first); \
                     {escape}."
                )
            }
            Self::StuckEdit { path, count } => format!(
                "Your changes to `{path}` have failed {count} times in a row. Stop guessing: look at the current text with `rg -n` or `sed -n` in bash, then copy a small, exact, unique `find` snippet from that output (without the line numbers). If the file already has what you want, move on."
            ),
        }
    }
}
