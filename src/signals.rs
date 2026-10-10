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
    /// The same unchanged file was read several times this request.
    RepeatedRead { path: String, count: usize },
    /// The same file was rewritten from scratch several times this request.
    Rewrite { path: String, count: usize },
    /// Several tool-call turns in a row without a word to the user.
    SilentRun { turns: usize },
    /// Several file-change batches with nothing run to check them.
    UnverifiedEdits { count: usize },
    /// Several plan-only turns in a row with no real work.
    PlanOnly { count: usize },
    /// The plan has unfinished steps but hasn't been updated in a while.
    PlanStale { turns: u64 },
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
            Self::RepeatedRead { path, count } => format!(
                "You have read `{path}` {count} times this task and it hasn't changed; its \
                 content is already in your context above. Work from what you read."
            ),
            Self::Rewrite { path, count } => format!(
                "You have rewritten `{path}` from scratch {count} times. Rewriting a whole \
                 script and rerunning it blind is not converging. Keep the system you are \
                 driving running, take one small step at a time with a short command or \
                 script, and look at the result before the next step."
            ),
            Self::SilentRun { turns } => format!(
                "You have made {turns} rounds of tool calls without saying anything. Before the \
                 next call, write one or two sentences: what you have found so far and what you \
                 are doing next."
            ),
            Self::UnverifiedEdits { count } => format!(
                "You have changed files {count} times without running anything to check them. \
                 Build or run the narrowest test now, before changing more."
            ),
            Self::PlanOnly { count } => format!(
                "You have updated the plan {count} times in a row without doing any work. Act \
                 now with a real tool call, or finish and deliver your conclusion."
            ),
            Self::PlanStale { turns } => format!(
                "Your plan has unfinished steps and hasn't been updated in {turns} steps. Mark \
                 what's done and adjust it to the work as it stands, or drop steps that no \
                 longer apply."
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
