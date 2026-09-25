use super::*;

impl CodingHarness {
    pub(super) fn dispatch_meta(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        tool_name: &str,
        arguments: &Value,
    ) -> (Value, MetaControl) {
        match tool_name {
            "cancel_delegated_task" => {
                let lane_id = arguments
                    .get("lane_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let reason = arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty());
                match (lane_id, reason) {
                    (Some(lane_id), Some(reason)) => match lanes.cancel(lane_id, reason) {
                        Ok(title) => {
                            state.events.push(HarnessEvent::LaneCancelled {
                                id: lane_id.to_string(),
                                title: title.clone(),
                                reason: reason.to_string(),
                            });
                            (
                                json!({"schema_version": 1, "status": "success", "data": {
                                    "cancelled": true,
                                    "lane_id": lane_id,
                                    "title": title,
                                    "note": "The delegated scope is now yours. Partial workspace changes were preserved; inspect and validate them before continuing."
                                }}),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    },
                    _ => (
                        tool_error(
                            "cancel_delegated_task requires non-empty `lane_id` and `reason`.",
                        ),
                        MetaControl::Continue,
                    ),
                }
            }
            "note" => {
                let entry = arguments
                    .get("entry")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(entry) = entry else {
                    return (
                        tool_error("note requires a non-empty `entry`."),
                        MetaControl::Continue,
                    );
                };
                state.events.push(HarnessEvent::Note {
                    entry: entry.to_string(),
                });
                (
                    json!({"schema_version": 1, "status": "success", "data": {"noted": true}}),
                    MetaControl::Continue,
                )
            }
            "set_session_title" => {
                let title = arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim);
                let Some(title) = title else {
                    return (
                        tool_error("set_session_title requires a `title` string."),
                        MetaControl::Continue,
                    );
                };
                state.title = (!title.is_empty()).then(|| title.to_string());
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "renamed": true,
                        "title": state.title,
                    }}),
                    MetaControl::Continue,
                )
            }
            "present_file" => {
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(path) = path else {
                    return (
                        tool_error("present_file requires a `path`."),
                        MetaControl::Continue,
                    );
                };
                let resolved = if std::path::Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    self.context.workspace_root().join(path)
                };
                // Only real files get presented — a hallucinated or not-yet-written
                // path fails loudly so the agent writes the file first.
                if !resolved.is_file() {
                    return (
                        tool_error(format!(
                            "present_file: `{path}` does not exist — write the file before presenting it."
                        )),
                        MetaControl::Continue,
                    );
                }
                let caption = arguments
                    .get("caption")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let shown = resolved.display().to_string();
                state.events.push(HarnessEvent::FilePresented {
                    path: shown.clone(),
                    caption,
                });
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "presented": shown,
                        "note": "shown to the user as an openable file card; continue your turn as usual",
                    }}),
                    MetaControl::Continue,
                )
            }
            "complete_goal" => {
                let summary = arguments
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match state.goal.as_mut() {
                    Some(goal) if goal.status == GoalStatus::Active => {
                        goal.status = GoalStatus::Complete;
                        let text = goal.text.clone();
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "goal_completed".to_string(),
                            reasoning: if summary.is_empty() {
                                text
                            } else {
                                summary.clone()
                            },
                        });
                        (
                            json!({"schema_version": 1, "status": "success", "data": {"completed": true}}),
                            MetaControl::EndTurn {
                                kind: TurnEndKind::Complete,
                                final_text: Some(if summary.is_empty() {
                                    "Goal complete.".to_string()
                                } else {
                                    summary
                                }),
                            },
                        )
                    }
                    _ => (
                        tool_error("complete_goal: there is no active goal to complete."),
                        MetaControl::Continue,
                    ),
                }
            }
            "ask_user" => match parse_ask_user(arguments) {
                Ok(rendered) => {
                    let prompt_text = first_question_text(&rendered)
                        .or_else(|| {
                            rendered
                                .get("context")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "Waiting for your input.".to_string());
                    state.events.push(HarnessEvent::UserQuestion {
                        questions: rendered.clone(),
                    });
                    state.pending_question = Some(rendered);
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"asked": true}}),
                        MetaControl::EndTurn {
                            kind: TurnEndKind::Ask,
                            final_text: Some(prompt_text),
                        },
                    )
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "delegate_task" => match parse_delegate_brief(arguments) {
                Ok(brief) => {
                    // Follow-up to an existing lane: resume it with the new brief,
                    // context intact.
                    if let Some(lane_id) = brief.lane_id.as_deref() {
                        return match lanes.follow_up(lane_id, &brief.description) {
                            Ok(title) => {
                                state.events.push(HarnessEvent::LaneSpawned {
                                    id: lane_id.to_string(),
                                    title: title.clone(),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "continued": true,
                                            "lane_id": lane_id,
                                            "title": title,
                                            "note": "Lane resumed with its prior context; its report will arrive as a [lane_report] message.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        };
                    }
                    match lanes.spawn(
                        &brief.title,
                        &brief.description,
                        brief.read_only,
                        brief.agent.clone(),
                        brief.profile.clone(),
                    ) {
                        Ok(id) => {
                            state.events.push(HarnessEvent::LaneSpawned {
                                id: id.clone(),
                                title: brief.title.clone(),
                            });
                            let mut data = json!({
                                "delegated": true,
                                "lane_id": id,
                                "title": brief.title,
                                "access": if brief.read_only { "read_only" } else { "full" },
                                "note": "Lane runs in the background; its report will arrive as a [lane_report] message. Follow up later by re-calling delegate_task with this lane_id.",
                            });
                            if let Some(ref agent) = brief.agent {
                                data["agent"] = json!(agent);
                            }
                            if let Some(ref profile) = brief.profile {
                                data["profile"] = json!(profile);
                            }
                            (
                                json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": data,
                                }),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    }
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "monitor" => {
                let action = arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("add");
                match action {
                    "add" => {
                        let Some(path) = arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            return (
                                tool_error("monitor add requires a `path`."),
                                MetaControl::Continue,
                            );
                        };
                        let label = arguments
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(path);
                        let filter = arguments
                            .get("filter")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        match watches.add(path, label, filter) {
                            Ok(record) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_added".to_string(),
                                    reasoning: format!(
                                        "watching \"{}\" ({})",
                                        record.label, record.path
                                    ),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "watching": true,
                                            "watch_id": record.id,
                                            "label": record.label,
                                            "path": record.path,
                                            "note": "Tailing from the current end of file. Appended text arrives as a [file_watch] message (debounced per burst); ending your turn is how you wait for it.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "remove" => {
                        let key = arguments
                            .get("watch_id")
                            .or_else(|| arguments.get("path"))
                            .or_else(|| arguments.get("label"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let Some(key) = key else {
                            return (
                                tool_error(
                                    "monitor remove needs a `watch_id`, `path`, or `label`.",
                                ),
                                MetaControl::Continue,
                            );
                        };
                        match watches.remove(key) {
                            Ok(label) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_removed".to_string(),
                                    reasoning: format!("stopped watching \"{label}\""),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": { "removed": true, "label": label }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "list" => (
                        json!({
                            "schema_version": 1,
                            "status": "success",
                            "data": {
                                "watches": watches.records().iter().map(|r| json!({
                                    "watch_id": r.id,
                                    "label": r.label,
                                    "path": r.path,
                                    "filter": r.filter,
                                })).collect::<Vec<_>>(),
                            }
                        }),
                        MetaControl::Continue,
                    ),
                    other => (
                        tool_error(format!(
                            "monitor action must be add | remove | list, got `{other}`."
                        )),
                        MetaControl::Continue,
                    ),
                }
            }
            other => (
                tool_error(format!("`{other}` is not a recognized meta tool.")),
                MetaControl::Continue,
            ),
        }
    }

    pub(super) fn definitions_for(
        &self,
        conversation_mode: bool,
        goal_active: bool,
    ) -> Vec<crate::llm::NativeToolDefinition> {
        let mut definitions = self.tools.definitions();
        if conversation_mode {
            // User-facing: meta tools (note/ask_user/delegate); no terminate tool —
            // a plain reply ends the turn. `complete_goal` is added only while a goal runs.
            definitions.extend(meta::conversation_meta_definitions_for(
                goal_active,
                self.config.allow_lane_control,
            ));
        } else {
            // Headless (lanes / one-shot run): an explicit terminate_loop carries a
            // structured summary back to the caller.
            definitions.push(meta::terminate_loop_tool());
        }
        definitions
    }

    pub(super) async fn recover(
        &self,
        state: &mut HarnessState,
        consecutive_errors: &mut usize,
    ) -> RecoveryAction {
        *consecutive_errors += 1;
        let max = self.config.max_consecutive_recovery;
        if *consecutive_errors > max {
            return RecoveryAction::GiveUp;
        }
        if max > 0 && *consecutive_errors == max {
            // Last chance: inject a runtime correction, then let the loop retry.
            let correction = RuntimeCorrectionKind::LlmRequestFailed;
            state.events.push(HarnessEvent::SystemDecision {
                step: correction.step().to_string(),
                reasoning: correction.reasoning().to_string(),
            });
            state.messages.push(HarnessMessage::System {
                content: correction.reasoning().to_string(),
            });
            return RecoveryAction::Retry;
        }
        sleep(backoff_delay(
            *consecutive_errors,
            self.config.recovery_base_ms,
            self.config.recovery_max_ms,
        ))
        .await;
        RecoveryAction::Retry
    }

    /// Bring a session loaded from either store into the current run.
    ///
    /// Both stores converge here so a resumed session behaves identically
    /// regardless of where it came from: the workspace and context window are
    /// refreshed, the system prefix is re-seeded (so new workspace memory lands),
    /// and any half-written tool batch is repaired so strict providers don't 400
    /// on the history forever after.
    pub(super) async fn resume_loaded_state(
        &self,
        mut state: HarnessState,
        seeded_system: String,
        initial_request: Option<String>,
    ) -> Result<HarnessState, ToolError> {
        // Migrate old metadata in memory; the next persist omits the legacy
        // `user_request` field and keeps the title as identity.
        normalize_state_title(&mut state);
        // Reflect the current run's folder (backfills pre-field states).
        state.workspace = self.context.workspace_root().display().to_string();
        state.context_window = self.config.context_window_tokens;
        // Refresh the system prefix so resumed sessions pick up the latest
        // workspace memory (guarded: no-op if messages[0] isn't System).
        if let Some(HarnessMessage::System { content }) = state.messages.first_mut() {
            *content = seeded_system;
        }
        // A crash mid tool-batch persists an assistant `tool_calls` message whose
        // later calls never got results; strict providers (Anthropic, DeepSeek)
        // 400 on that history forever after. Repair on load so a resumed session
        // is always well-formed.
        repair_unanswered_tool_calls(&mut state.messages);
        repair_unanswered_tool_events(&mut state.events, 0);
        if let Some(request) = initial_request
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
        {
            state.status = HarnessStatus::Running;
            state.final_text = None;
            state.pending_question = None;
            state.messages.push(HarnessMessage::User {
                content: request.clone(),
            });
            state.events.push(HarnessEvent::UserInput { text: request });
            self.bump_activity();
            self.persist_state(&mut state).await?;
        }
        Ok(state)
    }

    pub(super) async fn load_or_initialize_state(
        &self,
        initial_request: Option<String>,
    ) -> Result<HarnessState, ToolError> {
        // Build the per-workspace memory block once and fold it into the system
        // prefix, so it rides in the cached prompt and refreshes every session
        // (including resume). Within a session it stays fixed; mid-session writes
        // are visible to the agent only on the next start (cache-stable by design).
        let seeded_system = {
            let block = if self.config.memory_enabled {
                crate::memory::render_session_memory(
                    self.context.workspace_root(),
                    self.config.memory_index_budget_chars,
                )
            } else {
                None
            };
            match block {
                Some(b) => format!("{}\n\n{}", self.config.system_prompt, b),
                None => self.config.system_prompt.clone(),
            }
        };

        if self.config.resume
            && let Some(state) = self.load_from_store().await?
        {
            return self.resume_loaded_state(state, seeded_system, initial_request).await;
        }

        // Fresh session in this folder: keep snippet's `.snippet/` workspace scratch
        // (bg processes, lanes) out of the user's git history. Main agent only —
        // lanes share the workspace and would just race on the same file.
        if self.context.owner() == "main" {
            ensure_snippet_gitignored(self.context.workspace_root());
        }

        let now = Utc::now().to_rfc3339();
        let request = initial_request
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        let mut messages = vec![HarnessMessage::System {
            content: seeded_system,
        }];
        let mut events = Vec::new();
        let (status, title) = match request.as_ref() {
            Some(text) => {
                messages.push(HarnessMessage::User {
                    content: text.clone(),
                });
                events.push(HarnessEvent::UserInput { text: text.clone() });
                (HarnessStatus::Running, Some(text.clone()))
            }
            None => (HarnessStatus::Idle, None),
        };
        let mut state = HarnessState {
            version: 1,
            status,
            created_at: now.clone(),
            updated_at: now.clone(),
            workspace: self.context.workspace_root().display().to_string(),
            title,
            legacy_request: String::new(),
            goal: None,
            compacting: false,
            turn_started_at: if matches!(status, HarnessStatus::Running) {
                Some(now.clone())
            } else {
                None
            },
            compacting_started_at: None,
            watches: Vec::new(),
            messages,
            events,
            iterations: 0,
            final_text: None,
            lanes: Vec::new(),
            pending_question: None,
            approval_mode: if self.config.manual_approval {
                ApprovalMode::Manual
            } else {
                ApprovalMode::Auto
            },
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_prompt_tokens: 0,
            cache_read_tokens: 0,
            checkpoints: Vec::new(),
            rate_limit: None,
            context_window: self.config.context_window_tokens,
            tool_payloads_pruned: false,
            queued_inputs: Vec::new(),
            history_rewritten: false,
        };
        self.persist_state(&mut state).await?;
        if request.is_some() {
            self.bump_activity();
        }
        Ok(state)
    }

    pub(super) async fn persist(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
    ) -> Result<(), ToolError> {
        state.lanes = lanes.records().to_vec();
        self.persist_state(state).await
    }

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
        const MAX_COMPACTION_PASSES: usize = 4;

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

            let section = |title: &str, items: &[String]| -> String {
                if items.is_empty() {
                    format!("{title} = \"\"\n")
                } else {
                    format!("{title} = \"\"\"\n{}\n\"\"\"\n", items.join("\n"))
                }
            };

            // Order sections by importance (objective → decisions → open errors →
            // outcomes → actions) so the budget trim drops the least-critical detail.
            let mut summary = format!(
                "[compacted_window]\n{}{}{}{}{}",
                section("objective", &objective),
                section("decisions", &decisions),
                section("errors_open", &errors_open),
                section("outcomes", &outcomes),
                section("actions", &actions),
            );
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
            let session_id = self.context.durable_session_id().unwrap_or("default");
            let now = chrono::Utc::now().to_rfc3339();
            let final_summary = if let Some(store) = &store {
                match crate::history_archive::archive_messages(store, session_id, &older, 0, &now) {
                    Ok(summaries) => {
                        let micro = crate::history_archive::render_micro_pointers(original_request, &summaries);
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
        // Prune events to this compaction boundary. Tool rows are
        // reconstructible noise — they'd only bloat every persist.
        if let Some(i) = state.events.iter().rposition(
            |e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"),
        ) {
            state.events.drain(..i);
        }
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

        // Keep a copy of the fresh table to feed the memory reflection pass below.
        let table_for_memory = table.clone();

        // Archive older messages to SQLite & FTS5 and generate IBM-style micro-pointers
        let store = self.context.store().ok();
        let session_id = self.context.durable_session_id().unwrap_or("default");
        let now = chrono::Utc::now().to_rfc3339();
        let compacted_content = if let Some(store) = &store {
            match crate::history_archive::archive_messages(store, session_id, &older, 0, &now) {
                Ok(summaries) => {
                    let micro = crate::history_archive::render_micro_pointers(original_request, &summaries);
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
        state.events.push(HarnessEvent::SystemDecision {
            step: "history_compacted".to_string(),
            reasoning: format!(
                "Compacted the full conversation into the context table at {} / {} tokens ({}%), with extra detail on the most recent activity.",
                state.last_prompt_tokens, window, self.config.compact_at_pct
            ),
        });
        // Prune events to this compaction boundary. Tool rows are
        // reconstructible noise — they'd only bloat every persist.
        if let Some(i) = state.events.iter().rposition(
            |e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"),
        ) {
            state.events.drain(..i);
        }
        // Learning pass: distill durable facts/playbooks from the just-compacted
        // session into per-workspace memory. Main session only (lanes are read-only,
        // avoids concurrent index writers). Non-fatal — never abort compaction.
        // Skip a pure-conversation window (no tool calls / results): there's no
        // reusable procedure to learn, and skipping saves the reflection round-trips.
        let did_work = older.iter().any(|m| {
            matches!(m, HarnessMessage::ToolResult { .. })
                || matches!(m, HarnessMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty())
        });
        let reflect = self.config.memory_enabled
            && self.config.memory_reflect_on_compaction
            && self.context.owner() == "main";
        if reflect && did_work {
            if let Err(e) = self.run_memory_reflection(model, &table_for_memory).await {
                self.debug_log(&format!("memory reflection failed (non-fatal): {e}"));
            }
        } else if reflect {
            self.debug_log("memory reflection skipped: no tool work in the compacted window");
        }
        // Restore the session's reasoning effort for the next real model call.
        model.swap_reasoning_effort(prev_effort);
        state.last_prompt_tokens = 0;
        state.tool_payloads_pruned = false;
        state.compacting = false;
        Ok(())
    }
}

