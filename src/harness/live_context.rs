use super::*;

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
pub(super) fn build_live_context(
    state: &HarnessState,
    vars: &mut LoopVars,
    conversation_mode: bool,
    workspace: &std::path::Path,
    cwd: &std::path::Path,
    browser_summary: Option<String>,
    memory_writes: &[String],
) -> String {
    let signals = std::mem::take(&mut vars.pending_signals);
    let mut block = String::new();
    // Terse on purpose: re-sent uncached every turn, so it carries only volatile
    // STATE (facts the model reads), never standing rules or imperatives. The
    // governing rules — that this block is not the user, and not to narrate turn
    // status — live once in the cached system prompt (conversation_agent_layer.md
    // [steering]), not repeated here every turn where they read like a user
    // instruction and cost uncached tokens. One-line reminder only:
    block.push_str("# INTERNAL STATE — not user content; read silently and act.\n");

    block.push_str("\n[workspace]\n");
    block.push_str(&format!("cwd = \"{}\"\n", compact_path(cwd)));

    block.push_str("\n[session]\n");
    let title = state
        .title
        .as_deref()
        .map(sanitize_one_line)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    block.push_str(&format!("title = \"{title}\"\n"));

    if let Some(browser_summary) = browser_summary.as_deref()
        && crate::session::browser_summary_is_connected(browser_summary)
    {
        block.push('\n');
        block.push_str(browser_summary);
    }

    // Vault secrets: names only. Values are injected into bash child processes
    // and scrubbed from every tool result — the model never sees them.
    let vault_names = crate::vault::Vault::load().names();
    if !vault_names.is_empty() {
        block.push_str("\n[vault]\n");
        block.push_str(&format!("secrets = \"{}\"\n", vault_names.join(", ")));
    }

    // Surface the model's prior-turn reasoning so it can build on it instead of
    // re-deriving (experimental; conversation only).
    if let Some(thought) = vars.last_thought.as_deref() {
        block.push_str("\n[last_thought]  # continuity; don't re-derive\n");
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
            "pace = \"{n} steps in (autonomous goal — keep going until the goal is done)\"\n"
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
            "pace = \"{n} of ~{TURN_BUDGET} steps in{note}\"\n"
        ));
    }
    // Observed loop (a repeated call last turn) — stated as an observation, not an
    // order; the system prompt covers what to do about it.
    if vars.last_turn_had_repeat {
        block.push_str("observed = \"last tool call repeated one already in history; its result won't change.\"\n");
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
        block.push_str("\n[steering_signals]  # one-shot; act now, never quote\n");
        for signal in &signals {
            block.push_str(&format!("{}\n", signal.render()));
        }
    }

    // Surface heuristics about the latest user message (prompt-injection,
    // exfiltration, secrets, destructive intent) so the model weighs them rather
    // than blindly complying.
    if let Some(latest) = latest_user_input(state) {
        let safety = derive_input_safety_signals(&latest);
        if !safety.is_empty() {
            block.push_str("\n[input_safety]\n");
            for line in safety {
                block.push_str(&format!("{line}\n"));
            }
        }
    }

    // Skills: count-only (not the catalog) — search on demand keeps context lean.
    let skill_n = crate::skills::discover().len();
    if skill_n > 0 {
        block.push_str("\n[skills_available]\n");
        block.push_str(&format!("count = {skill_n}\n"));
    }

    // Mid-session memory writes (system index is cache-fixed until resume).
    if !memory_writes.is_empty() {
        let ids: Vec<&str> = memory_writes.iter().map(String::as_str).collect();
        block.push_str("\n[memory_updated]\n");
        block.push_str(&format!("ids = \"{}\"\n", ids.join(", ")));
    }

    // Background processes the agent started (dev servers, watchers) — so it knows
    // what's already running instead of re-launching, and can tail logs / kill them.
    if let Some(bg) = crate::bg::render_live(workspace) {
        block.push_str("\n[background_processes]\n");
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
    // Shown ONLY while at least one lane is still out: mid-fan-out, the finished
    // list gives the orchestrator the "2 of 5 in" picture and follow-up handles.
    // Once everything has reported, the section disappears — the reports are
    // already folded into history (with follow_up_ids), and re-listing completed
    // lanes every turn forever was pure re-stimulus that models kept narrating
    // ("the 5 lanes are folded in…") long after the work was done.
    if !running.is_empty() {
        block.push_str("\n[delegated_lanes]\n");
        block.push_str(&format!("running = {}\n", running.len()));
        for l in &running {
            let mut tags = String::new();
            if let Some(ref a) = l.agent {
                tags.push_str(&format!(" [agent: {a}]"));
            }
            if let Some(ref p) = l.profile {
                tags.push_str(&format!(" [profile: {p}]"));
            }
            block.push_str(&format!(
                "- \"{}\"{} — running ({})\n",
                clip(&l.title, 32),
                tags,
                l.id
            ));
        }
        for l in &finished {
            let status = match l.status {
                LaneStatus::Completed => "completed",
                LaneStatus::Failed => "FAILED",
                LaneStatus::Cancelled => "cancelled",
                LaneStatus::Running => unreachable!("filtered above"),
            };
            let mut tags = String::new();
            if let Some(ref a) = l.agent {
                tags.push_str(&format!(" [agent: {a}]"));
            }
            if let Some(ref p) = l.profile {
                tags.push_str(&format!(" [profile: {p}]"));
            }
            block.push_str(&format!(
                "- \"{}\"{} — {} ({})\n",
                clip(&l.title, 32),
                tags,
                status,
                l.id
            ));
        }
        block.push_str(&format!(
            "orchestrate = \"{} lane(s) still working; end your turn to wait — reports wake you\"\n",
            running.len()
        ));
    }

    if !state.watches.is_empty() {
        block.push_str("\n[active_watches]\n");
        for w in &state.watches {
            let filter = w.filter.as_deref().unwrap_or("none");
            block.push_str(&format!(
                "- \"{}\" on {} (filter: \"{}\", id: {})\n",
                clip(&w.label, 32),
                compact_path(std::path::Path::new(&w.path)),
                filter,
                w.id
            ));
        }
        block.push_str("wait = \"watch is active; end your turn to wait — [file_watch] wakes you. Clean up with monitor action:\\\"remove\\\" once done.\"\n");
    }

    block
}

/// Flag the latest user message for prompt-injection / exfiltration / secret /
/// destructive phrasing (capped at 6).
pub(super) fn derive_input_safety_signals(input: &str) -> Vec<String> {
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
            &[
                "disable safety",
                "jailbreak",
                "bypass policy",
                "no restrictions",
            ],
        ),
        (
            "secret_exfiltration",
            "request may involve secrets, credentials, or token exfiltration",
            &[
                "api key",
                "access token",
                "password",
                "private key",
                "secret",
            ],
        ),
        (
            "destructive_operations",
            "potential destructive operation request detected",
            &[
                "drop database",
                "delete all",
                "rm -rf",
                "truncate table",
                "wipe",
            ],
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

pub(super) fn sanitize_one_line(text: &str) -> String {
    let collapsed = text.replace('\n', " ").replace('"', "'");
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most recent user-originated text (a fresh request or a mid-run steer).
pub(super) fn latest_user_input(state: &HarnessState) -> Option<String> {
    state.events.iter().rev().find_map(|event| match event {
        HarnessEvent::UserInput { text } | HarnessEvent::Steer { text } => Some(text.clone()),
        _ => None,
    })
}

pub(super) fn first_question_text(rendered: &Value) -> Option<String> {
    rendered
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| question.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
