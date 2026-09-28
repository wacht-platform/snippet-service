use super::*;

impl CodingHarness {
    pub(super) async fn compact_history_if_needed(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
    ) -> Result<(), ToolError> {
        // Tier 0: strip old tool bodies (no model call) once usage is high.
        // Only then fall through to the expensive agentic table rewrite.
        if self.prune_old_tool_payloads(state) {
            let _ = self.persist_state(state).await;
        }
        self.compact_history_agentic(model, state, false).await
    }

    pub(super) fn prune_old_tool_payloads(&self, state: &mut HarnessState) -> bool {
        const MIN_SAVINGS_TOKENS: u64 = 256;

        let window = self.config.context_window_tokens.max(1);
        let start_at =
            window.saturating_mul(self.config.tool_prune_at_pct.clamp(1, 100) as u64) / 100;
        let prefix_budget =
            window.saturating_mul(self.config.tool_prune_prefix_pct.clamp(1, 100) as u64) / 100;
        if state.tool_payloads_pruned
            || start_at == 0
            || prefix_budget == 0
            || state.last_prompt_tokens < start_at
            || state.messages.is_empty()
        {
            return false;
        }

        let mut cumulative = 0u64;
        let mut cut = 0usize;
        for (i, msg) in state.messages.iter().enumerate() {
            let tokens = estimate_message_tokens(msg);
            if cumulative.saturating_add(tokens) > prefix_budget {
                break;
            }
            cumulative = cumulative.saturating_add(tokens);
            cut = i + 1;
        }
        if cut == 0 {
            return false;
        }

        let before = estimate_prompt_tokens(&state.messages);
        let mut messages = state.messages.clone();
        let mut pruned_n = 0u32;
        for message in &mut messages[..cut] {
            match message {
                HarnessMessage::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        if !tool_args_are_stub(&call.arguments) {
                            call.arguments = json!({"_":"pruned"});
                            pruned_n += 1;
                        }
                    }
                }
                HarnessMessage::ToolResult {
                    tool_name, content, ..
                } if !tool_result_is_stub(content) => {
                    *content = pruned_tool_result_stub(tool_name);
                    pruned_n += 1;
                }
                _ => {}
            }
        }

        let after = estimate_prompt_tokens(&messages);
        let saved = before.saturating_sub(after);
        if pruned_n == 0 || saved < MIN_SAVINGS_TOKENS {
            return false;
        }

        state.messages = messages;
        state.tool_payloads_pruned = true;
        // Pruning REPLACES tool bodies in place, so the transcript is the same
        // LENGTH with different content. A length check cannot see that, which is
        // exactly why the flag exists.
        state.history_rewritten = true;
        state.events.push(HarnessEvent::SystemDecision {
            step: "tool_payloads_pruned".to_string(),
            reasoning: "Pruned older tool data.".to_string(),
        });
        true
    }

    pub(super) async fn compact_history(
        &self,
        state: &mut HarnessState,
        force: bool,
    ) -> Result<(), ToolError> {
        const RECENT_DETAIL_KEEP: usize = 12;
        const MIN_COMPACTABLE_MESSAGES: usize = 18;
        const MAX_SECTION_ITEMS: usize = 18;
        const MAX_COMPACTION_PASSES: usize = 1;

        let window = self.config.context_window_tokens.max(1);
        let threshold =
            window.saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64) / 100;
        if !force && (threshold == 0 || state.last_prompt_tokens < threshold) {
            return Ok(());
        }

        let system_prompt = match state.messages.first() {
            Some(HarnessMessage::System { content }) => content.clone(),
            _ => return Ok(()),
        };
        state.compacting = true;

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

        let summarize_window = |messages: &[HarnessMessage],
                                original_request: &str|
         -> (String, String, usize) {
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
                        push_unique(&mut objective, format!("- USER {text}"), MAX_SECTION_ITEMS);
                    }
                    HarnessMessage::Assistant {
                        content,
                        tool_calls,
                    } => {
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
                    HarnessMessage::ToolResult {
                        tool_name, content, ..
                    } => {
                        let rendered = preview_json(content, 420);
                        push_unique(
                            &mut outcomes,
                            format!("- Tool result {tool_name}: {rendered}"),
                            MAX_SECTION_ITEMS,
                        );
                        if rendered
                            .to_ascii_lowercase()
                            .contains("\"status\":\"error\"")
                            || rendered
                                .to_ascii_lowercase()
                                .contains("\"status\": \"error\"")
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

            if objective.is_empty() && !original_request.trim().is_empty() {
                objective.push(format!(
                    "- Original request: {}",
                    preview_text(original_request, 320)
                ));
            }

            let now = Utc::now().to_rfc3339();
            let mut summary = format!(
                "<CONTEXT_SUMMARY>\n\
                 The following is a summary of the conversation history that has been truncated to fit within the context window:\n\n\
                 This summary was generated at {now}.\n\n"
            );

            let user_reqs = extract_user_requests("", messages, original_request);
            if !user_reqs.is_empty() {
                summary.push_str("# User Requests\n");
                summary.push_str(
                    "The following were the most recent user requests in chronological order:\n",
                );
                for (i, req) in user_reqs.iter().enumerate() {
                    summary.push_str(&format!("{}. {}\n", i + 1, req));
                }
                summary.push('\n');
            }

            summary.push_str("# Previous Session Summary:\n<summary>\n");
            let mut section_idx = 1;
            if !objective.is_empty() {
                summary.push_str(&format!(
                    "### {section_idx}. Task Overview\n{}\n\n",
                    objective.join("\n")
                ));
                section_idx += 1;
            }
            if !outcomes.is_empty() {
                summary.push_str(&format!(
                    "### {section_idx}. Progress\n{}\n\n",
                    outcomes.join("\n")
                ));
                section_idx += 1;
            }
            if !decisions.is_empty() || !errors_open.is_empty() {
                let mut tech = decisions.clone();
                tech.extend(errors_open.clone());
                summary.push_str(&format!(
                    "### {section_idx}. Key Findings & Technical Decisions\n{}\n\n",
                    tech.join("\n")
                ));
                section_idx += 1;
            }
            if !actions.is_empty() {
                summary.push_str(&format!(
                    "### {section_idx}. Next Steps\n{}\n\n",
                    actions.join("\n")
                ));
            }
            summary.push_str("</summary>\n</CONTEXT_SUMMARY>");

            // The compacted window targets ~6k tokens so it stays cheap to carry
            // forward. Approximate at ~3.5 chars/token and trim the tail if over.
            const COMPACTION_BUDGET_CHARS: usize = 21_000;
            if summary.chars().count() > COMPACTION_BUDGET_CHARS {
                summary = summary
                    .chars()
                    .take(COMPACTION_BUDGET_CHARS)
                    .collect::<String>()
                    + "\n…[compacted summary trimmed to fit the 6k-token budget]";
            }

            let recent_start = messages.len().saturating_sub(RECENT_DETAIL_KEEP);
            let recent_tail = &messages[recent_start..];
            let mut recent = String::from(
                "[Recent activity orientation — keep this in mind while continuing the thread]\n",
            );
            for message in recent_tail.iter().take(8) {
                let line = match message {
                    HarnessMessage::User { content } => {
                        format!("- user: {}", preview_text(content, 220))
                    }
                    HarnessMessage::Assistant {
                        content,
                        tool_calls,
                    } => {
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
                    HarnessMessage::ToolResult {
                        tool_name, content, ..
                    } => {
                        format!("- tool {tool_name}: {}", preview_json(content, 220))
                    }
                    HarnessMessage::Summary { kind, content } => {
                        format!("- {kind}: {}", preview_text(content, 220))
                    }
                    HarnessMessage::System { content } => {
                        format!("- system: {}", preview_text(content, 220))
                    }
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

            let original_request = state.initial_request().unwrap_or("");
            let (summary, recent, recent_len) = summarize_window(&older, original_request);
            preserved_recent_count = recent_len;

            let store = self.context.store().ok();
            let session_id = self.context.durable_session_id();
            let now = chrono::Utc::now().to_rfc3339();
            let final_summary = if let (Some(store), Some(session_id)) = (&store, session_id) {
                match crate::history_archive::archive_messages(store, session_id, &older, &now) {
                    Ok(summaries) => {
                        let micro = crate::history_archive::render_micro_pointers(
                            original_request,
                            &summaries,
                        );
                        format!("{summary}\n\n{micro}")
                    }
                    Err(_) => summary,
                }
            } else {
                summary
            };

            let mut next = vec![
                HarnessMessage::Summary {
                    kind: "compacted_window".to_string(),
                    content: final_summary,
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
        // Compaction replaces a span of history with a summary, so the stored
        // transcript must be rewritten rather than appended to.
        state.history_rewritten = true;
        state.compactions += 1;
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
        // Reset ONLY the current-context gauge — the cumulative session counters
        // (prompt/completion/total) reflect everything sent and are unaffected by
        // compaction. The gauge repopulates from the next response's usage.
        state.last_prompt_tokens = 0;
        state.tool_payloads_pruned = false;
        state.compacting = false;
        Ok(())
    }

    /// Agentic compaction: the model maintains ONE living "context table", updating
    /// its sections from the prior table + new activity, fit to a ~6k-token budget.
    /// Falls back to the heuristic compaction if the model is unavailable.
    pub(super) async fn compact_history_agentic(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        force: bool,
    ) -> Result<(), ToolError> {
        const RECENT_FOCUS: usize = 12;
        const MIN_COMPACTABLE_MESSAGES: usize = 14;

        let window = self.config.context_window_tokens.max(1);
        let threshold =
            window.saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64) / 100;
        if !force && (threshold == 0 || state.last_prompt_tokens < threshold) {
            return Ok(());
        }

        let Some(HarnessMessage::System {
            content: system_prompt,
        }) = state.messages.first().cloned()
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
        // `compacting` is what attached UIs poll — set it for auto *and* manual.
        state.compacting = true;
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compaction_pass".to_string(),
            reasoning: format!(
                "Compacting {} messages into the context table.",
                older.len()
            ),
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

        let original_request = state.initial_request().unwrap_or("");
        let window_text = render_window(&prior_table, &older, original_request, RECENT_FOCUS);
        let table = match self.run_agentic_summary(model, &window_text).await {
            Ok(table) => table,
            // Model unavailable / failed — fall back to the heuristic compaction.
            // Note: the heuristic path does NOT run the memory reflection pass.
            Err(e) => {
                model.swap_reasoning_effort(prev_effort);
                self.debug_log(&format!(
                    "agentic summary failed → heuristic compaction (memory reflection skipped): {e}"
                ));
                // compact_history clears `compacting` when it finishes.
                return self.compact_history(state, force).await;
            }
        };

        // Archive older messages to SQLite & FTS5 and generate IBM-style micro-pointers
        let store = self.context.store().ok();
        let session_id = self.context.durable_session_id();
        let now = chrono::Utc::now().to_rfc3339();
        let pending_users = older
            .iter()
            .rev()
            .take_while(|m| matches!(m, HarnessMessage::User { .. }))
            .count();
        let to_archive = &older[..older.len() - pending_users];
        let compacted_content = if let (Some(store), Some(session_id)) = (&store, session_id) {
            match crate::history_archive::archive_messages(store, session_id, to_archive, &now) {
                Ok(summaries) => {
                    let micro =
                        crate::history_archive::render_micro_pointers(original_request, &summaries);
                    format!("{table}\n\n{micro}")
                }
                Err(e) => {
                    self.debug_log(&format!("failed to archive messages: {e}"));
                    table
                }
            }
        } else {
            table
        };

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
            HarnessMessage::System {
                content: system_prompt,
            },
            HarnessMessage::Summary {
                kind: "compacted_window".to_string(),
                content: compacted_content,
            },
        ];
        state.messages.extend(trailing_user);
        // The whole conversation became the table, so this is the largest rewrite
        // there is — the stored rows must be replaced wholesale.
        state.history_rewritten = true;
        state.compactions += 1;
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compacted".to_string(),
            reasoning: format!(
                "Compacted the full conversation into the context table at {} / {} tokens ({}%), with extra detail on the most recent activity.",
                state.last_prompt_tokens, window, self.config.compact_at_pct
            ),
        });
        // Restore the session's reasoning effort for the next real model call.
        model.swap_reasoning_effort(prev_effort);
        state.last_prompt_tokens = 0;
        state.tool_payloads_pruned = false;
        state.compacting = false;
        Ok(())
    }
}
