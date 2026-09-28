use super::*;

/// Productivity of one tool-calling turn, fed to [`apply_turn_guards`].
pub(super) struct TurnStats {
    pub real_work: usize,
    pub failed: usize,
    pub had_note: bool,
    pub shell_nudged: bool,
    /// Every call this turn was `delegate_task` (and none failed).
    pub only_delegations: bool,
    pub delegations_ok: usize,
}

impl Default for TurnStats {
    fn default() -> Self {
        Self {
            real_work: 0,
            failed: 0,
            had_note: false,
            shell_nudged: false,
            only_delegations: true,
            delegations_ok: 0,
        }
    }
}

/// Unproductive backstop: too many tool-call turns in a row that did no real
/// work (notes / unknown tools). Wrap the run up cleanly rather than spinning,
/// and say why so the turn doesn't end silently.
pub(super) fn unproductive_stop(
    state: &mut HarnessState,
    vars: &mut LoopVars,
) -> Option<StepResult> {
    if vars.unproductive_turns < MAX_UNPRODUCTIVE_TURNS {
        return None;
    }
    vars.unproductive_turns = 0;
    state.events.push(HarnessEvent::SystemDecision {
        step: "stopped_unproductive".to_string(),
        reasoning: format!(
            "Stopped after {MAX_UNPRODUCTIVE_TURNS} turns in a row that did no real work."
        ),
    });
    Some(StepResult::TurnEnded {
        kind: TurnEndKind::Complete,
        final_text: None,
    })
}

/// Tool-call-loop detection: the exact same batch repeated turn over turn, or
/// the same call appearing 3+ times within the last 8 turns even with other
/// calls interleaved, steers the model next turn instead of letting it spin.
pub(super) fn track_call_repeats(vars: &mut LoopVars, calls: &[GeneratedToolCall]) {
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

    vars.recent_tool_signatures.push_back(signature.clone());
    while vars.recent_tool_signatures.len() > 8 {
        vars.recent_tool_signatures.pop_front();
    }
    let windowed = vars
        .recent_tool_signatures
        .iter()
        .filter(|s| **s == signature)
        .count();
    if windowed >= 3 && vars.repeated_tool_count < 2 {
        vars.pending_signals
            .push(RuntimeSignal::ToolCallLoop { count: windowed });
    }
}

/// Shell discipline: nudge (never block) when `bash` does work a file tool does
/// better. A repeated nudge escalates to reflect-and-switch. Returns whether
/// this command was nudged.
pub(super) fn note_shell_command(vars: &mut LoopVars, command: &str) -> bool {
    let ShellVerdict::Nudge(message) = classify_shell_command(command) else {
        return false;
    };
    vars.shell_nudge_count += 1;
    if vars.shell_nudge_count >= SHELL_NUDGE_ESCALATE_AT {
        vars.pending_signals
            .push(RuntimeSignal::ShellDisciplineEscalated {
                count: vars.shell_nudge_count,
            });
    } else {
        vars.pending_signals
            .push(RuntimeSignal::ShellDiscipline { message });
    }
    true
}

/// Consecutive failed edits on the same file raise a `StuckEdit` nudge.
pub(super) fn note_edit_result(vars: &mut LoopVars, path: Option<String>, is_err: bool) {
    if !is_err {
        vars.consecutive_failed_edits = 0;
        vars.last_failed_edit_path = None;
        return;
    }
    let Some(path) = path else {
        return;
    };
    if vars.last_failed_edit_path.as_deref() == Some(&path) {
        vars.consecutive_failed_edits += 1;
    } else {
        vars.last_failed_edit_path = Some(path.clone());
        vars.consecutive_failed_edits = 1;
    }
    if vars.consecutive_failed_edits >= 2 {
        vars.pending_signals.push(RuntimeSignal::StuckEdit {
            path,
            count: vars.consecutive_failed_edits,
        });
    }
}

/// Post-turn signals from how the batch went.
pub(super) fn apply_turn_guards(vars: &mut LoopVars, stats: &TurnStats, conversation_mode: bool) {
    // A turn with no shell nudge breaks the escalation streak.
    if !stats.shell_nudged {
        vars.shell_nudge_count = 0;
    }

    // Record whether THIS turn repeated the exact same batch as last turn, so
    // next turn's live context explains the re-prompt only when actually looping.
    vars.last_turn_had_repeat = vars.repeated_tool_count > 0;

    // Backpressure on very large single-turn fan-outs.
    if stats.real_work >= LARGE_TOOL_BATCH {
        vars.pending_signals.push(RuntimeSignal::BatchBackpressure {
            batch_size: stats.real_work,
        });
    }

    // Stuck detection: after a couple of turns where every executed call
    // failed, steer the model to re-think or ask for help.
    if stats.real_work > 0 && stats.failed >= stats.real_work {
        vars.consecutive_failed_turns += 1;
        if vars.consecutive_failed_turns >= 2 {
            vars.pending_signals.push(RuntimeSignal::StuckEscalation {
                failed_turns: vars.consecutive_failed_turns,
                can_ask_user: conversation_mode,
            });
        }
    } else if stats.real_work > 0 {
        vars.consecutive_failed_turns = 0;
    }

    // Productivity accounting: real work resets the streaks; a turn that only
    // took notes (or only hit unknown tools) is unproductive and is nudged
    // toward action, then wrapped up by the top-of-step backstop.
    if stats.real_work > 0 {
        vars.unproductive_turns = 0;
        vars.consecutive_note_count = 0;
    } else {
        vars.unproductive_turns += 1;
        if stats.had_note {
            vars.consecutive_note_count += 1;
            if vars.consecutive_note_count >= NOTE_LOOP_AT {
                vars.pending_signals.push(RuntimeSignal::NoteLoop {
                    count: vars.consecutive_note_count,
                });
            }
        }
    }
}
