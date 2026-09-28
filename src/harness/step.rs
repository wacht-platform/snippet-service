use super::*;

/// What the model's output resolved to once inline markup was salvaged and
/// phantom calls dropped.
struct ParsedTurn {
    calls: Vec<GeneratedToolCall>,
    progress_text: Option<String>,
}

impl CodingHarness {
    /// One model call and the tools it asks for.
    #[allow(clippy::too_many_arguments)]
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

        if let Some(end) = unproductive_stop(state, vars) {
            return end;
        }

        // This turn counts against the current request's soft turn budget.
        vars.turns_this_request = vars.turns_this_request.saturating_add(1);

        // Keep the lane snapshot current so the live context shows what's still
        // running (the orchestrator must know what it's waiting on).
        state.lanes = lanes.records().to_vec();

        // `build_live_context` drains the pending signals into this request; if
        // the request FAILS they must survive to the retry — losing a
        // loop-breaking nudge exactly when the model is looping made things worse.
        let signals_backup = vars.pending_signals.clone();
        let request_messages = self.build_request(state, vars, model, conversation_mode);

        self.lane_progress(
            "model",
            format!(
                "waiting for model response (iteration {})",
                state.iterations
            ),
        );
        // Clear any leftover live-stream text before this turn streams into it;
        // the sink is present only for the interactive conversation.
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }

        let mut output = match self
            .call_model(model, state, &request_messages, &definitions, sink)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                vars.pending_signals = signals_backup;
                return error;
            }
        };

        // The request's workspace snapshot overlapped the model call; it must be
        // on disk before any tool can touch a file.
        self.finish_checkpoint(state, vars).await;

        // Capture this turn's reasoning so the next turn's live context can
        // surface "what you thought last time". Bounded to the last 2000 chars.
        if let Some(sink) = sink {
            let thought = StreamBuffer::snapshot_thinking(sink);
            let thought = thought.trim();
            vars.last_thought = (!thought.is_empty()).then(|| {
                let chars: Vec<char> = thought.chars().collect();
                let start = chars.len().saturating_sub(2000);
                chars[start..].iter().collect::<String>()
            });
        }

        let anchor_tokens = record_usage(state, &output, &request_messages, &definitions);
        let anchor_msg_len = state.messages.len();

        // A response cut off at the token cap is never a finished reply.
        let truncated = output.is_truncated();
        if truncated {
            vars.pending_signals.push(RuntimeSignal::ResponseTruncated);
        }

        let ParsedTurn {
            mut calls,
            progress_text,
        } = self.parse_output(state, &mut output, conversation_mode);

        if calls.is_empty() {
            return self
                .finish_text_turn(
                    state,
                    lanes,
                    vars,
                    sink,
                    progress_text,
                    truncated,
                    conversation_mode,
                )
                .await;
        }

        track_call_repeats(vars, &calls);
        self.record_assistant_turn(state, lanes, sink, &mut calls, progress_text.clone())
            .await;

        let stats = match self
            .run_tool_calls(
                state,
                lanes,
                watches,
                vars,
                conversation_mode,
                sink,
                approval_rx,
                calls,
                &definitions,
            )
            .await
        {
            Ok(stats) => stats,
            Err(end) => return end,
        };
        apply_turn_guards(vars, &stats, conversation_mode);

        // Every call this iteration was a successful delegation → the wait takes
        // effect NOW: end the turn so the agent goes idle and the lane reports
        // wake it, instead of burning iterations narrating that it's waiting.
        if conversation_mode && stats.only_delegations && stats.delegations_ok > 0 {
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

    /// Durable history plus a fresh live-context block. The block is sent but
    /// never persisted, so signals re-ground the model each turn instead of
    /// accumulating as stale nudges. It goes as System, not User: adapters wrap a
    /// mid-history System turn in a `[steering]` envelope, so the model reads it
    /// as runtime state rather than the user speaking.
    fn build_request(
        &self,
        state: &HarnessState,
        vars: &mut LoopVars,
        model: &dyn AgentModel,
        conversation_mode: bool,
    ) -> Vec<HarnessMessage> {
        let mut request_messages = state.messages.clone();
        self.inline_images(&mut request_messages, model.supports_images());
        request_messages.push(HarnessMessage::System {
            content: build_live_context(
                state,
                vars,
                conversation_mode,
                self.context.workspace_root(),
                &self.context.current_dir(),
                self.context.browser_summary(),
            ),
        });
        request_messages
    }

    /// Call the model. An unsupported reasoning tier is degraded one step and
    /// retried (pinned on the model for the rest of the session); any other
    /// failure becomes a `ModelError` with a concise message.
    async fn call_model(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        request_messages: &[HarnessMessage],
        definitions: &[NativeToolDefinition],
        sink: Option<&StreamHandle>,
    ) -> Result<crate::llm::ModelOutput, StepResult> {
        loop {
            let error = match model
                .generate(request_messages, definitions, false, sink.cloned())
                .await
            {
                Ok(output) => return Ok(output),
                Err(error) => error,
            };
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
                        reasoning: raw
                            .lines()
                            .next()
                            .unwrap_or(&raw)
                            .chars()
                            .take(200)
                            .collect(),
                    });
                    let _ = self.persist_state(state).await;
                    continue;
                }
                // No effort was set — not degradable (the swap restored None).
            }
            // Adapters embed the full HTTP body; show only a concise first line.
            let line = raw.lines().next().unwrap_or(&raw);
            let message = if line.chars().count() > 240 {
                format!("{}…", line.chars().take(240).collect::<String>())
            } else {
                line.to_string()
            };
            return Err(StepResult::ModelError {
                retryable: error.retryable(),
                message,
            });
        }
    }

    /// Salvage tool calls written as inline markup, clean the visible text, and
    /// drop calls whose names can't be real tools.
    fn parse_output(
        &self,
        state: &HarnessState,
        output: &mut crate::llm::ModelOutput,
        conversation_mode: bool,
    ) -> ParsedTurn {
        let native_call_names: Vec<String> =
            output.calls.iter().map(|c| c.tool_name.clone()).collect();
        let raw_content = output.content_text.clone();
        let mut calls = std::mem::take(&mut output.calls);
        let mut progress_text = None;

        if let Some(text) = output.content_text.as_deref() {
            if looks_like_inline_tool_submission(text) {
                let inline = extract_inline_tool_submissions(text);
                let residual = inline.residual_text.clone().unwrap_or_default();
                // Only adopt recovered calls when the markup dominated the message
                // (short residual prose); otherwise it's a real reply that happens
                // to mention markup.
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
        // A name that isn't a clean identifier can't be a real tool — it's noise
        // salvaged from quoted syntax, not an unknown tool to report back.
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

        ParsedTurn {
            calls,
            progress_text,
        }
    }

    /// A turn with no tool calls: the text is the final answer ("no tool calls =
    /// done"), unless it was truncated (take another turn) or the conversation
    /// agent hasn't actually replied yet (re-prompt, at most twice).
    #[allow(clippy::too_many_arguments)]
    async fn finish_text_turn(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
        vars: &mut LoopVars,
        sink: Option<&StreamHandle>,
        progress_text: Option<String>,
        truncated: bool,
        conversation_mode: bool,
    ) -> StepResult {
        if truncated {
            if let Some(text) = progress_text {
                record_assistant_text(state, text, None);
                // Committed into events — drop the live buffer so clients don't
                // keep showing the same fragment beside it.
                if let Some(sink) = sink {
                    StreamBuffer::clear(sink);
                }
                let _ = self.persist(state, lanes).await;
            }
            return StepResult::Continue;
        }
        if let Some(text) = progress_text.clone() {
            record_assistant_text(state, text, None);
        }
        // Always clear after a terminal plain-text step so sticky thinking can't
        // linger on idle clients between turns.
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }
        let _ = self.persist(state, lanes).await;
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
        StepResult::TurnEnded {
            kind: TurnEndKind::Complete,
            final_text: progress_text,
        }
    }

    /// Give every call a stable id and record the native assistant turn (visible
    /// text plus its tool calls) before any tool runs, so a crash mid-batch still
    /// leaves the model output on disk.
    async fn record_assistant_turn(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
        sink: Option<&StreamHandle>,
        calls: &mut [GeneratedToolCall],
        progress_text: Option<String>,
    ) {
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
        if let Some(text) = progress_text {
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
        let _ = self.persist(state, lanes).await;
    }
}

/// Fold the response's usage into the session and return the anchor for the
/// context gauge: provider-reported prompt tokens when present, the local
/// estimate only as a fallback for gateways that omit usage.
fn record_usage(
    state: &mut HarnessState,
    output: &crate::llm::ModelOutput,
    request_messages: &[HarnessMessage],
    definitions: &[NativeToolDefinition],
) -> u64 {
    let estimated_prompt =
        estimate_prompt_tokens(request_messages) + tools_token_overhead(definitions);
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
    state.last_prompt_tokens = anchor_tokens;
    // Assign unconditionally: the snapshot describes the most recent call, so a
    // model that reports nothing must clear it — otherwise a ChatGPT snapshot
    // outlived a switch to a provider that reports no rate limits.
    state.rate_limit = output.rate_limit.clone();
    anchor_tokens
}
