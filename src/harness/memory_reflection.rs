use super::*;

impl CodingHarness {
    pub(super) async fn run_agentic_summary(
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
                    if feedback.is_empty() {
                        String::new()
                    } else {
                        format!("\n\n{feedback}")
                    }
                )
            };
            let messages = vec![
                HarnessMessage::System {
                    content: SUMMARIZER_SYSTEM.to_string(),
                },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output
                .calls
                .first()
                .filter(|c| c.tool_name == "write_table")
            else {
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
    pub(super) async fn run_memory_reflection(
        &self,
        model: &mut dyn AgentModel,
        summary_table: &str,
    ) -> Result<(), ToolError> {
        // Tight cap — each turn is a full model round-trip and this runs after
        // every main-session compaction (was 8, then 5). Reflection should converge
        // in 1–2 writes; a higher cap just let the model re-save the same entry
        // over and over until it timed out on the cap.
        const MAX_TURNS: usize = 4;

        let store = crate::memory::MemoryStore::for_workspace(self.context.workspace_root());
        let global = crate::memory::MemoryStore::global();
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
            let patterns = global.read_patterns();
            let user = format!(
                "WORKSPACE: {ws}\n\nWHAT JUST HAPPENED (compacted session table):\n{summary_table}\n\nCURRENT MEMORY INDEX:\n{index}\n\nEXISTING ENTRIES: {entries}\n\nCURRENT REUSABLE PATTERNS (global):\n{patterns}\n\nLAST RESULT: {feedback}\n\nTurn {turn}/{MAX_TURNS} · {writes} write(s) so far. Make exactly one tool call. Capture what helps a FUTURE session: (a) workspace FACTS/pointers and how-to PLAYBOOK(s) for THIS project via memory_write; (b) any GENERALIZABLE PATTERN this session demonstrated — a reusable technique (situation → approach → why) that would help in ANY project — via memory_pattern (include the existing patterns plus the new/refined one). Write each distinct thing ONCE; NEVER re-save or 'polish' something you already wrote this pass. Once the durable value is captured (usually 1–2 writes) and the index points to workspace entries, call finalize. Only finalize with nothing written if the session was genuinely trivial.",
                index = if index.trim().is_empty() {
                    "(empty)".to_string()
                } else {
                    index
                },
                entries = if entries.is_empty() {
                    "(none)".to_string()
                } else {
                    entries.join(", ")
                },
                patterns = if patterns.trim().is_empty() {
                    "(none yet)".to_string()
                } else {
                    patterns
                },
                writes = writes,
            );
            let messages = vec![
                HarnessMessage::System {
                    content: MEMORY_REFLECTOR_SYSTEM.to_string(),
                },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output.calls.first() else {
                feedback = "no tool call received — call exactly one tool".to_string();
                continue;
            };
            let arg_str = |k: &str| {
                call.arguments
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            match call.tool_name.as_str() {
                "finalize" => {
                    self.debug_log(&format!(
                        "memory reflection: finalized after {writes} write(s)"
                    ));
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
                            self.debug_log(&format!(
                                "memory reflection: wrote entry `{id}` ({}b)",
                                content.len()
                            ));
                            format!(
                                "entry `{id}` saved. Do NOT re-write or 'polish' it. If the index needs it, call memory_index once — then finalize."
                            )
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
                "memory_pattern" => {
                    let content = arg_str("content");
                    feedback = match global.add_pattern(&content, crate::memory::patterns_budget())
                    {
                        Ok(true) => {
                            writes += 1;
                            self.debug_log("memory reflection: added global pattern");
                            "pattern added. If nothing else remains, finalize.".to_string()
                        }
                        Ok(false) => {
                            "identical pattern already stored — don't re-add it; finalize if done."
                                .to_string()
                        }
                        Err(e) => e,
                    };
                }
                other => feedback = format!("unknown tool `{other}`"),
            }
        }
        self.debug_log(&format!(
            "memory reflection: hit turn cap after {writes} write(s)"
        ));
        Ok(())
    }

    /// Append a line to `<state_dir>/debug.log` for tracing model/loop behaviour.
    /// No-op when no state path is configured.
    pub(super) fn lane_progress(&self, kind: &str, text: impl Into<String>) {
        let (Some(tx), Some(id)) = (&self.config.progress_tx, &self.config.progress_id) else {
            return;
        };
        let _ = tx.send(crate::lanes::LaneProgress {
            id: id.clone(),
            kind: kind.to_string(),
            text: text.into(),
        });
    }

    pub(super) fn debug_log(&self, line: &str) {
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

    pub(super) async fn persist_state(&self, state: &mut HarnessState) -> Result<(), ToolError> {
        stamp_activity_times(state);
        if self.config.state_path.is_none() {
            return Ok(());
        }
        state.updated_at = Utc::now().to_rfc3339();
        self.persist_to_store(state).await
    }

    /// List sort uses the store's `last_active` — bump only when the user
    /// actually sent a message (or a mid-run steer).
    pub(super) fn bump_activity(&self) {
        if let Some(path) = &self.config.state_path {
            crate::session::bump_session_activity(path);
        }
    }
}

/// The transcript line a recorded event contributes, if any.
///
/// A `Notice` is stored as BOTH an event and a message: the event is what the
/// UIs render, the message is what the model sees next turn. Returning `None`
/// means the event is UI-only and must not enter the model's context.
///
/// Public so a notice recorded into a DORMANT session (no running loop) produces
/// exactly the same transcript entry as one delivered to a live loop.
pub(crate) fn notice_text(event: &HarnessEvent) -> Option<String> {
    match event {
        HarnessEvent::AgentMessage {
            agent_id,
            body,
            outbound,
        } => Some(if *outbound {
            format!("[sent to {agent_id}]\n{body}")
        } else {
            format!("[reply from {agent_id}]\n{body}")
        }),
        HarnessEvent::TaskDispatched {
            task_id,
            title,
            session_id,
            by,
        } => Some(format!(
            "[dispatched by {by}] {title}\ntask {task_id} → session {session_id}\n             Informational: this work is already routed to a worker and will report back on its own. \
             Do not dispatch it again."
        )),
        _ => None,
    }
}

pub(super) fn queue_held(state: &mut HarnessState, item: QueuedInput) {
    let text = item.text.trim().to_string();
    if !text.is_empty() {
        state.queued_inputs.push(QueuedInput { id: item.id, text });
    }
}

pub(super) fn take_queued(state: &mut HarnessState, id: &str) -> Option<String> {
    let i = state.queued_inputs.iter().position(|item| item.id == id)?;
    Some(state.queued_inputs.remove(i).text)
}

/// Keep compacting/thinking clocks anchored to when the activity actually
/// started, not when a client widget mounted. Called on every persist so
/// attached UIs can tick from a durable RFC3339 stamp.
pub(super) fn stamp_activity_times(state: &mut HarnessState) {
    let now = Utc::now().to_rfc3339();
    if state.compacting {
        if state.compacting_started_at.is_none() {
            state.compacting_started_at = Some(now.clone());
        }
    } else {
        state.compacting_started_at = None;
    }
    if state.status == HarnessStatus::Running && !state.compacting {
        if state.turn_started_at.is_none() {
            state.turn_started_at = Some(now);
        }
    } else if state.status != HarnessStatus::Running {
        state.turn_started_at = None;
    }
}

/// Tiny marker left in place of pruned tool-call arguments.
pub(super) fn tool_args_are_stub(args: &Value) -> bool {
    match args {
        Value::Object(map) => {
            map.len() <= 1
                && map
                    .get("_")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == "pruned")
        }
        _ => false,
    }
}

/// True when a tool result was already replaced by our prune stub.
pub(super) fn tool_result_is_stub(content: &Value) -> bool {
    content
        .get("pruned")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || content
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s == "pruned")
}

/// Minimal tool-result body kept after prune — just enough for pairing + audit.
pub(super) fn pruned_tool_result_stub(tool_name: &str) -> Value {
    json!({
        "pruned": true,
        "tool": tool_name,
    })
}

/// Insert synthetic error results for assistant tool calls that never received
/// one. Every unanswered `tool_call_id` gets a stub result appended right after
/// the contiguous result run that follows its assistant message, preserving the
/// call/result pairing strict providers require.
pub(super) fn repair_unanswered_tool_calls(messages: &mut Vec<HarnessMessage>) {
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

/// Insert synthetic error results for tool call events that never received one
/// (e.g. interrupted mid-run or crashed). Every unanswered `ToolCall` gets a
/// stub `ToolResult` appended to preserve the pairing and ensure the transcript
/// record and failure reason remain visible in the UI.
pub(super) fn repair_unanswered_tool_events(events: &mut Vec<HarnessEvent>, from_index: usize) {
    let from_index = from_index.min(events.len());
    let mut pending_tools: Vec<String> = Vec::new();
    for event in &events[from_index..] {
        match event {
            HarnessEvent::ToolCall { tool_name, .. } => {
                pending_tools.push(tool_name.clone());
            }
            HarnessEvent::ToolResult { tool_name, .. } => {
                if let Some(pos) = pending_tools.iter().rposition(|n| n == tool_name) {
                    pending_tools.remove(pos);
                }
            }
            HarnessEvent::InvalidToolCall { tool_name, .. } => {
                if let Some(pos) = pending_tools.iter().rposition(|n| n == tool_name) {
                    pending_tools.remove(pos);
                }
            }
            _ => {}
        }
    }
    for tool_name in pending_tools {
        events.push(HarnessEvent::ToolResult {
            tool_name,
            result: json!({
                "schema_version": 1,
                "status": "error",
                "error": {
                    "code": "interrupted",
                    "message": "This tool call was interrupted before it produced a result (the process stopped mid-run). Re-run it if the work is still needed.",
                }
            }),
        });
    }
}

/// A real tool name is a short, clean identifier. Names with spaces, backticks,
/// dots, or other punctuation come from prose mis-parsed as tool markup.
pub(super) fn is_plausible_tool_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub(super) fn dbg_short(text: &str) -> String {
    let one_line = text.replace('\n', "\\n");
    one_line.chars().take(160).collect()
}

pub(super) fn tool_error(message: impl Into<String>) -> Value {
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
pub(super) fn build_live_context(
    state: &HarnessState,
    vars: &mut LoopVars,
    conversation_mode: bool,
    workspace: &std::path::Path,
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
    block.push_str(&format!("cwd = \"{}\"\n", compact_path(workspace)));

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
            let agent_str = l
                .agent
                .as_ref()
                .map(|a| format!(" [agent: {a}]"))
                .unwrap_or_default();
            block.push_str(&format!(
                "- \"{}\"{} — running ({})\n",
                clip(&l.title, 32),
                agent_str,
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
            let agent_str = l
                .agent
                .as_ref()
                .map(|a| format!(" [agent: {a}]"))
                .unwrap_or_default();
            block.push_str(&format!(
                "- \"{}\"{} — {} ({})\n",
                clip(&l.title, 32),
                agent_str,
                status,
                l.id
            ));
        }
        block.push_str(&format!(
            "orchestrate = \"{} lane(s) still working; end your turn to wait — reports wake you\"\n",
            running.len()
        ));
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

pub(super) fn backoff_delay(attempt: usize, base_ms: u64, max_ms: u64) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(7) as u32;
    let delay = base_ms
        .max(1)
        .saturating_mul(1u64 << shift)
        .min(max_ms.max(1));
    Duration::from_millis(delay)
}

/// Whether the agent has produced a user-visible reply (`AssistantText`) since the
/// most recent user message — i.e. it actually answered, not just took notes / ran
/// tools. Scans events newest-first, stopping at the last user input.
pub(super) fn replied_since_last_user(events: &[HarnessEvent]) -> bool {
    for e in events.iter().rev() {
        match e {
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. } => return false,
            HarnessEvent::AssistantText { .. } => return true,
            _ => {}
        }
    }
    false
}

/// Record an assistant turn. Near-duplicate *narration* (text accompanying
/// tool calls) is dropped before it enters `events` or `messages` so clients
/// and the on-disk session never see repeated working-aloud status. Turn-final
/// text (no tool calls) is the actual reply and always records — filtering it
/// made legitimate messages vanish from history and clients. Tool-call turns
/// still persist (empty content when redundant) so pairing stays valid.
pub(super) fn record_assistant_text(
    state: &mut HarnessState,
    text: String,
    tool_calls: Option<Vec<crate::llm::ToolCallRecord>>,
) {
    let narrating = tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    let redundant = narrating && assistant_text_is_redundant(&text, &state.events);
    let content = if redundant || text.trim().is_empty() {
        String::new()
    } else {
        text.clone()
    };
    let calls = tool_calls.unwrap_or_default();
    if !content.is_empty() || !calls.is_empty() {
        state.messages.push(HarnessMessage::Assistant {
            content,
            tool_calls: calls,
        });
    }
    if !redundant && !text.trim().is_empty() {
        state.events.push(HarnessEvent::AssistantText { text });
    }
}

pub(super) fn normalize_assistant_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

pub(super) fn assistant_text_is_redundant(text: &str, events: &[HarnessEvent]) -> bool {
    let next = normalize_assistant_text(text);
    if next.is_empty() {
        return false;
    }
    let recent: Vec<&str> = events
        .iter()
        .rev()
        .filter_map(|e| match e {
            HarnessEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .take(3)
        .collect();
    for prior in recent {
        let prev = normalize_assistant_text(prior);
        if prev.is_empty() {
            continue;
        }
        if next == prev {
            return true;
        }
        if next.chars().count() >= 24 && prev.contains(&next) {
            return true;
        }
        if prev.chars().count() >= 24 && next.contains(&prev) {
            return true;
        }
        if token_overlap(&next, &prev) >= 0.80 {
            return true;
        }
    }
    false
}

pub(super) fn token_overlap(a: &str, b: &str) -> f64 {
    let as_set: HashSet<&str> = a.split(' ').filter(|w| w.len() > 2).collect();
    let bs_set: HashSet<&str> = b.split(' ').filter(|w| w.len() > 2).collect();
    if as_set.is_empty() || bs_set.is_empty() {
        return 0.0;
    }
    let shared = as_set.intersection(&bs_set).count();
    let denom = as_set.len().min(bs_set.len());
    shared as f64 / denom as f64
}

/// On a fresh session in a git work tree, make sure snippet's `.snippet/` scratch
/// (bg-process registry, lane state) is gitignored so it never lands in the user's
/// history. Best-effort and idempotent: creates `.gitignore` if it's missing,
/// appends the entry if absent, no-ops if already covered. Skips folders that
/// aren't in a git repo — a `.gitignore` there would be pointless clutter.
pub(super) fn ensure_snippet_gitignored(workspace: &Path) {
    if !in_git_work_tree(workspace) {
        return;
    }
    let gitignore = workspace.join(".gitignore");
    let covered = |content: &str| {
        content.lines().any(|line| {
            matches!(
                line.trim(),
                ".snippet" | ".snippet/" | "/.snippet" | "/.snippet/"
            )
        })
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
pub(super) fn in_git_work_tree(dir: &Path) -> bool {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return true;
        }
        cur = d.parent();
    }
    false
}

pub(super) fn normalize_tool_aliases(calls: &mut [GeneratedToolCall]) {
    for call in calls {
        if call.tool_name == "execute_command" {
            call.tool_name = "bash".to_string();
        }
    }
}

