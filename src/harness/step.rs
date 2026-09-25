use super::*;

impl CodingHarness {
    pub(super) async fn step(
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
        // spinning.
        if vars.unproductive_turns >= MAX_UNPRODUCTIVE_TURNS {
            vars.unproductive_turns = 0;
            return StepResult::TurnEnded {
                kind: TurnEndKind::Complete,
                final_text: None,
            };
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
        // in a `[steering]` envelope, so the model reads it as out-of-band
        // runtime state rather than the user speaking — which stopped it from (a)
        // replying with "you should do X" advice and (b) narrating its completion
        // status back to "the user". (On the wire this still lands in the user
        // role on providers with no mid-conversation system role — the framing in
        // build_live_context is the portable half of the fix.)
        request_messages.push(HarnessMessage::System {
            content: build_live_context(
                state,
                vars,
                conversation_mode,
                self.context.workspace_root(),
                self.context.browser_summary(),
                &self.context.memory_writes_snapshot(),
            ),
        });

        // Clear any leftover live-stream text before this turn streams into it; the
        // sink is present only for the interactive conversation (lanes/one-shot
        // pass None and stay buffered).
        self.lane_progress(
            "model",
            format!(
                "waiting for model response (iteration {})",
                state.iterations
            ),
        );
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }

        // "No tool calls = done": a plain-text turn ends the run, so we never force
        // a tool call — the model finishes simply by replying without one.
        let mut output = loop {
            match model
                .generate(&request_messages, &definitions, false, sink.cloned())
                .await
            {
                Ok(output) => break output,
                Err(error) => {
                    // Unsupported reasoning tier: no provider (except Anthropic)
                    // lets us discover supported effort levels up front, so the
                    // reliable path is to degrade at runtime — step the model's
                    // effort down one tier and retry. The swap pins the lower tier
                    // on the model instance for the rest of the session, so the
                    // cost is one failed request per tier, once.
                    let raw = error.to_string();
                    if crate::llm::is_effort_rejection(&raw) {
                        if let Some(current) = model.swap_reasoning_effort(None) {
                            let next = crate::llm::degrade_effort(&current);
                            model.swap_reasoning_effort(next.map(str::to_string));
                            state.events.push(HarnessEvent::SystemDecision {
                                step: format!(
                                    "reasoning effort `{current}` rejected by the model — retrying at `{}`",
                                    next.unwrap_or("provider default")
                                ),
                                reasoning: raw.lines().next().unwrap_or(&raw).chars().take(200).collect(),
                            });
                            let _ = self.persist_state(state).await;
                            continue;
                        }
                        // No effort was set — not degradable; fall through to the
                        // normal error path (swap above already restored None).
                    }
                    // The failed request consumed the drained signals — restore
                    // them so the retry carries the same steering.
                    vars.pending_signals = signals_backup;
                    // Adapters embed the full HTTP body; show only a concise first line.
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
            }
        };
        // Capture this turn's reasoning (from the sink) so the next turn's live
        // context can surface "what you thought last time". Bounded to the LAST
        // 2000 chars so it can't bloat the request and keeps the freshest tail.
        if let Some(sink) = sink {
            let thought = StreamBuffer::snapshot_thinking(sink);
            let thought = thought.trim();
            vars.last_thought = (!thought.is_empty()).then(|| {
                let chars: Vec<char> = thought.chars().collect();
                let start = chars.len().saturating_sub(2000);
                chars[start..].iter().collect::<String>()
            });
        }
        // Prefer provider-reported prompt tokens when present. The manual estimate is
        // only a fallback for gateways that omit/zero usage — never a floor over
        // real API numbers.
        let estimated_prompt =
            estimate_prompt_tokens(&request_messages) + tools_token_overhead(&definitions);
        let anchor_tokens = if let Some(usage) = output.usage {
            state.total_tokens = state.total_tokens.saturating_add(usage.total_tokens);
            state.prompt_tokens = state.prompt_tokens.saturating_add(usage.prompt_tokens);
            state.completion_tokens = state
                .completion_tokens
                .saturating_add(usage.completion_tokens);
            state.cache_read_tokens = state
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            if usage.prompt_tokens > 0 {
                usage.prompt_tokens
            } else {
                estimated_prompt
            }
        } else {
            estimated_prompt
        };
        let anchor_msg_len = state.messages.len();
        state.last_prompt_tokens = anchor_tokens;
        // Assign UNCONDITIONALLY: the snapshot describes the most recent call, so
        // a model that reports nothing must CLEAR it.
        //
        // Overwriting only on `Some` let a ChatGPT snapshot outlive its session
        // forever. Switching the profile to a provider that reports no
        // rate-limit headers (opencode, anthropic, gemini) left the old figure in
        // place, where it was rendered as that provider's current limit — the
        // "stuck on awaiting update" report.
        state.rate_limit = output.rate_limit.clone();

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
                // Salvage gating: only adopt recovered calls when the
                // markup dominated the message (short residual prose). If the
                // residual is long, the text is a real reply that happens to
                // mention markup — keep it as prose, ignore the salvage.
                let residual_short = residual.trim().chars().count() <= 240;
                if residual_short
                    && inline
                        .calls
                        .iter()
                        .any(|c| is_plausible_tool_name(&c.tool_name))
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
                    record_assistant_text(state, text, None);
                    // Committed into events — drop the live buffer so clients don't
                    // keep showing the same fragment (and thinking) beside it.
                    if let Some(sink) = sink {
                        StreamBuffer::clear(sink);
                    }
                    // Durably land model output before the next step/restart.
                    let _ = self.persist(state, lanes).await;
                }
                return StepResult::Continue;
            }
            // No tool calls: the turn is over and this text is the final answer
            // ("no tool calls = done"). Render it once, in order, and end the run.
            if let Some(text) = progress_text.clone() {
                record_assistant_text(state, text, None);
            }
            // Always clear after a terminal plain-text step — even with no prose —
            // so sticky thinking can't linger on idle clients between turns.
            if let Some(sink) = sink {
                StreamBuffer::clear(sink);
            }
            // Flush model output before ending / re-prompting so a restart can't
            // drop the just-committed reply.
            let _ = self.persist(state, lanes).await;
            // Conversation agent: don't end with NO visible reply when the agent
            // hasn't actually answered since the last user message (e.g. it only
            // left a note and then returned an empty turn). Re-prompt for a real
            // reply a couple of times before giving up.
            let empty_reply = progress_text
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty();
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
        let windowed = vars
            .recent_tool_signatures
            .iter()
            .filter(|s| **s == signature)
            .count();
        if windowed >= 3 && vars.repeated_tool_count < 2 {
            vars.pending_signals
                .push(RuntimeSignal::ToolCallLoop { count: windowed });
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
        if let Some(text) = progress_text.clone() {
            record_assistant_text(state, text, Some(tool_calls));
        } else {
            state.messages.push(HarnessMessage::Assistant {
                content: String::new(),
                tool_calls,
            });
        }
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }
        // Persist the assistant turn (text and/or tool_calls) before executing
        // tools so a crash mid-batch still leaves the model output on disk.
        let _ = self.persist(state, lanes).await;

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

        // Delegation-only iterations end the turn on the spot (see below): the
        // brief is handed off, the tool result says ending the turn is how to
        // wait, so there's nothing productive left this turn — letting the model
        // keep generating just yields narration until it stumbles into an empty
        // reply. A failed delegation clears the flag so the model can react.
        let mut only_delegations = true;
        let mut delegations_ok = 0usize;
        for call in calls {
            let tool_name = call.tool_name.clone();
            let call_id = call.id.clone().unwrap_or_default();
            if tool_name != "delegate_task" {
                only_delegations = false;
            }
            state.events.push(HarnessEvent::ToolCall {
                tool_name: tool_name.clone(),
                arguments: call.arguments.clone(),
            });
            self.lane_progress("tool_call", format!("running {tool_name}"));

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
                        record_assistant_text(state, s.to_string(), None);
                        if let Some(sink) = sink {
                            StreamBuffer::clear(sink);
                        }
                    }
                    let _ = self.persist(state, lanes).await;
                    return StepResult::TurnEnded {
                        kind: TurnEndKind::Complete,
                        final_text: Some(s.to_string()),
                    };
                }
                // Missing summary: the error result nudges a retry next turn.
                let _ = self.persist(state, lanes).await;
                continue;
            }

            let is_meta = conversation_mode
                && meta::is_meta_tool(&tool_name)
                && (self.config.allow_lane_control
                    || !matches!(
                        tool_name.as_str(),
                        "delegate_task" | "cancel_delegated_task"
                    ));

            if is_meta {
                if tool_name == "note" {
                    had_note = true;
                }
                let (result, control) =
                    self.dispatch_meta(state, lanes, watches, &tool_name, &call.arguments);
                if tool_name == "delegate_task" {
                    if result.get("status").and_then(Value::as_str) == Some("success") {
                        delegations_ok += 1;
                    } else {
                        only_delegations = false;
                    }
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
                // Flush each meta tool (note/delegate/ask_user/…) as it lands.
                let _ = self.persist(state, lanes).await;
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
                let _ = self.persist(state, lanes).await;
                continue;
            }

            // Dedup: re-calling a read-only discovery tool with identical args this
            // request is the classic spinning loop — its result is already in
            // history. Short-circuit with a notice instead of re-running. (A
            // mutation below clears the set, so re-discovery after a change still
            // works; read_file/bash are excluded — re-reads after edits are legit.)
            let signature = format!("{}:{}", tool_name, call.arguments);
            if DEDUP_TOOLS.contains(&tool_name.as_str()) && vars.executed_calls.contains(&signature)
            {
                // Already ran this exact discovery call; skip re-running it. But we
                // must STILL answer the call_id: an assistant tool_calls message with
                // any unanswered tool_call_id makes strict providers (DeepSeek) 400
                // ("insufficient tool messages"), and the broken turn poisons every
                // later request until compaction.
                dedup_hits += 1;
                let skipped = if tool_name == "memory_read" {
                    "Identical memory_read already ran this turn — reuse that result. Don't recall the same id again."
                } else {
                    "Identical discovery call already ran this turn — reuse the earlier result instead of repeating it."
                };
                let result = json!({
                    "schema_version": 1,
                    "status": "ok",
                    "data": {"skipped": skipped},
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
                let _ = self.persist(state, lanes).await;
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
                            vars.pending_signals
                                .push(RuntimeSignal::ShellDisciplineEscalated {
                                    count: vars.shell_nudge_count,
                                });
                        } else {
                            vars.pending_signals
                                .push(RuntimeSignal::ShellDiscipline { message });
                        }
                    }
                }
            }

            // A bash command that references a vault secret ($NAME) ALWAYS requires
            // explicit user confirmation — a secret is about to be injected into a
            // process, so it's gated regardless of approval_mode / manual_approval /
            // a prior Approve-All. In a delegated lane / headless run there's no user
            // to confirm, so we deny it (the secret-using step must be done on the
            // interactive thread where the user can see and approve it).
            let vault_secrets_used: Vec<String> = if tool_name == "bash" {
                call.arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .map(|c| crate::vault::Vault::load().referenced_names(c))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if !vault_secrets_used.is_empty() && !conversation_mode {
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "vault_needs_confirmation",
                        "message": format!(
                            "Using vault secret(s) [{}] requires user confirmation, which isn't available in a delegated/headless run. Don't run this here — report that this step needs the secret, so it's done on the main thread where the user can approve it.",
                            vault_secrets_used.join(", ")
                        )
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
                let _ = self.persist(state, lanes).await;
                continue;
            }
            let force_vault_approval = !vault_secrets_used.is_empty();

            // Manual mode: pause for the user's approval before a mutating tool runs.
            // Approvals queue across a batch (index/total); Deny skips this one with a
            // denial result so the model adapts. Interrupt cancels the whole step.
            // A vault-secret call is ALSO gated here even in Auto mode.
            if force_vault_approval
                || (state.approval_mode == ApprovalMode::Manual
                    && MUTATING_TOOLS.contains(&tool_name.as_str()))
            {
                approval_index += 1;
                // Discard stale decisions (double-taps, key repeat) queued before
                // this prompt existed — an unconsumed leftover would instantly
                // "approve" an action the user never saw. A prior Approve-All never
                // reaches here for a vault call: the drain clears it and force_vault
                // re-prompts, so Approve-All can't silently authorize a secret.
                while approval_rx.try_recv().is_ok() {}
                let summary = if force_vault_approval {
                    format!(
                        "⚠ uses vault secret(s) [{}] — {}",
                        vault_secrets_used.join(", "),
                        approval_summary(&tool_name, &call.arguments)
                    )
                } else {
                    approval_summary(&tool_name, &call.arguments)
                };
                state.events.push(HarnessEvent::ApprovalRequest {
                    tool_name: tool_name.clone(),
                    summary,
                    index: approval_index,
                    total: total_mutating.max(approval_index),
                });
                state.status = HarnessStatus::WaitingForInput;
                let _ = self.persist(state, lanes).await;
                let decision = approval_rx.recv().await;
                state.status = HarnessStatus::Running;
                // Approve-All flips to Auto for ordinary mutating calls — but NOT off
                // a vault prompt: a secret must never silently authorize the session
                // into an unattended mode, and vault calls re-prompt anyway.
                if matches!(decision, Some(ApprovalDecision::ApproveAll)) && !force_vault_approval {
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
                    let _ = self.persist(state, lanes).await;
                    continue;
                }
            }

            // Surface the in-flight call before running it so a slow tool (bash,
            // web fetch) isn't a black box — the TUI shows this ToolCall with a
            // "running" indicator until its result lands.
            let _ = self.persist(state, lanes).await;

            let mut result = match self
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
            // Vault choke point: every tool result is scrubbed before it enters the
            // conversation — secret values become [vault:NAME], so even `echo $KEY`
            // (or reading a file that contains one) never puts a value in context.
            {
                let vault = crate::vault::Vault::load();
                if !vault.is_empty() {
                    vault.scrub_value(&mut result);
                }
            }
            if result.get("status").and_then(Value::as_str) == Some("error") {
                failed_results += 1;
            }
            // A mutation may have changed the workspace, so prior discovery results
            // are stale — re-discovery is legitimate again; clear the dedup set.
            // Otherwise remember this discovery call so an exact repeat is caught.
            if MUTATING_TOOLS.contains(&tool_name.as_str()) {
                // File/shell mutations stale workspace discovery, not memory.
                vars.executed_calls
                    .retain(|s| s.starts_with("memory_read:"));
            } else if matches!(
                tool_name.as_str(),
                "memory_write" | "memory_delete" | "memory_index"
            ) {
                vars.executed_calls
                    .retain(|s| !s.starts_with("memory_read:"));
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
            // Flush after every tool result so a mid-batch kill still keeps
            // completed calls on disk.
            let _ = self.persist(state, lanes).await;
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

        // Every call this iteration was a successful delegation → the wait takes
        // effect NOW: end the turn so the agent goes idle and the lane reports
        // wake it, instead of burning model iterations narrating that it's
        // waiting. Any text beside the calls is the progress line.
        if conversation_mode && only_delegations && delegations_ok > 0 {
            return StepResult::TurnEnded {
                kind: TurnEndKind::Complete,
                final_text: progress_text,
            };
        }

        let tail = anchor_msg_len.min(state.messages.len());
        let trailing = estimate_prompt_tokens(&state.messages[tail..]);
        state.last_prompt_tokens = anchor_tokens.saturating_add(trailing);

        StepResult::Continue
    }
}

