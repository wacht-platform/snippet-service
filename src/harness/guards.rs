use super::*;

/// Productivity of one tool-calling turn, fed to [`apply_turn_guards`].
pub(super) struct TurnStats {
    pub real_work: usize,
    pub failed: usize,
    pub had_plan: bool,
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
            had_plan: false,
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

/// Hard stop for a loop the nudge didn't break: the exact same batch issued
/// five turns running. The run ends and says why, instead of burning tokens.
pub(super) fn repeat_stop(state: &mut HarnessState, vars: &mut LoopVars) -> Option<StepResult> {
    const STOP_AT: usize = 4;
    if vars.repeated_tool_count < STOP_AT {
        return None;
    }
    vars.repeated_tool_count = 0;
    vars.last_tool_signature = None;
    state.events.push(HarnessEvent::SystemDecision {
        step: "stopped_repeating".to_string(),
        reasoning: "Stopped: the agent issued the same tool call five times in a row.".to_string(),
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

/// Commands that only look at files, as opposed to building or running
/// something that checks them.
const LOOK_COMMANDS: [&str; 20] = [
    "cat", "head", "tail", "nl", "less", "more", "bat", "sed", "rg", "grep", "ls", "find", "fd",
    "wc", "tree", "stat", "file", "echo", "pwd", "git",
];
const READ_COMMANDS: [&str; 8] = ["cat", "head", "tail", "nl", "less", "more", "bat", "sed"];

/// After a bash command ran: count files it read (re-reading an unchanged file
/// raises `RepeatedRead`), and treat anything that isn't a look as checking
/// the edits made so far.
pub(super) fn note_bash_command(vars: &mut LoopVars, command: &str, cwd: &std::path::Path) {
    let mut checked = false;
    for segment in command.split(['\n', ';', '|', '&']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .map(|w| w.trim_matches(|c| c == '\'' || c == '"'))
            .collect();
        let Some(program) = words.first().map(|w| w.rsplit('/').next().unwrap_or(w)) else {
            continue;
        };
        if program.is_empty() || program == "cd" {
            continue;
        }
        if !LOOK_COMMANDS.contains(&program) {
            checked = true;
            continue;
        }
        let is_read = READ_COMMANDS.contains(&program)
            && (program != "sed"
                || (words.contains(&"-n") && !words.iter().any(|w| w.starts_with("-i"))));
        if !is_read {
            continue;
        }
        for arg in words[1..].iter().filter(|w| !w.starts_with('-') && !w.is_empty()) {
            note_file_read(vars, arg, cwd);
        }
    }
    if checked {
        vars.edits_since_check = 0;
    }
}

fn note_file_read(vars: &mut LoopVars, arg: &str, cwd: &std::path::Path) {
    use std::hash::{Hash, Hasher};
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let expanded = match arg.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => std::path::PathBuf::from(arg),
    };
    let path = if expanded.is_absolute() { expanded } else { cwd.join(expanded) };
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let fingerprint = hasher.finish();
    let entry = vars.file_reads.entry(path.clone()).or_insert((fingerprint, 0));
    if entry.0 == fingerprint {
        entry.1 += 1;
    } else {
        *entry = (fingerprint, 1);
    }
    if entry.1 == REPEATED_READ_AT {
        let shown = path.strip_prefix(cwd).unwrap_or(&path).display().to_string();
        vars.pending_signals.push(RuntimeSignal::RepeatedRead {
            path: shown,
            count: REPEATED_READ_AT,
        });
    }
}

/// A successful `change_files` call; several with nothing run in between
/// raise `UnverifiedEdits`, and rewriting one file from scratch again and
/// again raises `Rewrite`.
pub(super) fn note_file_change(vars: &mut LoopVars, arguments: &Value) {
    let changes = arguments.get("changes").and_then(Value::as_array);
    for change in changes.into_iter().flatten() {
        let rewrite = change.get("action").and_then(Value::as_str) == Some("create")
            && change.get("overwrite").and_then(Value::as_bool) == Some(true);
        let Some(path) = change.get("path").and_then(Value::as_str).filter(|_| rewrite) else {
            continue;
        };
        let count = vars.rewrites.entry(path.to_string()).or_insert(0);
        *count += 1;
        if *count == REWRITE_AT {
            vars.pending_signals.push(RuntimeSignal::Rewrite {
                path: path.to_string(),
                count: REWRITE_AT,
            });
        }
    }
    vars.edits_since_check += 1;
    if vars.edits_since_check == UNVERIFIED_EDITS_AT {
        vars.pending_signals.push(RuntimeSignal::UnverifiedEdits {
            count: vars.edits_since_check,
        });
    }
}

/// A tool-call turn: several in a row without any text raise `SilentRun`.
pub(super) fn note_turn_text(vars: &mut LoopVars, said_something: bool) {
    if said_something {
        vars.silent_turns = 0;
        return;
    }
    vars.silent_turns += 1;
    if vars.silent_turns == SILENT_RUN_AT {
        vars.pending_signals.push(RuntimeSignal::SilentRun {
            turns: vars.silent_turns,
        });
    }
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
pub(super) fn apply_turn_guards(
    vars: &mut LoopVars,
    stats: &TurnStats,
    conversation_mode: bool,
    plan_open: bool,
) {
    // A turn with no shell nudge breaks the escalation streak.
    if !stats.shell_nudged {
        vars.shell_nudge_count = 0;
    }

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
    // updated the plan (or only hit unknown tools) is unproductive and is nudged
    // toward action, then wrapped up by the top-of-step backstop.
    if stats.real_work > 0 {
        vars.unproductive_turns = 0;
        vars.consecutive_plan_count = 0;
    } else {
        vars.unproductive_turns += 1;
        if stats.had_plan {
            vars.consecutive_plan_count += 1;
            if vars.consecutive_plan_count >= PLAN_LOOP_AT {
                vars.pending_signals.push(RuntimeSignal::PlanOnly {
                    count: vars.consecutive_plan_count,
                });
            }
        }
    }

    // A plan with unfinished steps that hasn't been touched in a while has
    // probably drifted from the work; one reminder to bring it up to date.
    if plan_open && !stats.had_plan {
        vars.turns_since_plan += 1;
        if vars.turns_since_plan == PLAN_STALE_AFTER {
            vars.pending_signals.push(RuntimeSignal::PlanStale {
                turns: vars.turns_since_plan,
            });
        }
    }
}
