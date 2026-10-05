use super::*;

fn tool_activity_text(name: &str) -> String {
    match name {
        "bash" => "Running command".into(),
        "change_files" => "Editing files".into(),
        "view_image" => "Viewing image".into(),
        "web_search" => "Searching the web".into(),
        "web_read" => "Reading web page".into(),
        "search_skills" => "Finding skills".into(),
        "skill" => "Loading skill".into(),
        "manage_process" => "Managing background process".into(),
        "delegate_task" => "Delegating task".into(),
        "update_plan" => "Updating plan".into(),
        "terminate_loop" => "Finishing task".into(),
        _ => format!("Running {name}"),
    }
}

#[cfg(test)]
#[test]
fn tool_activity_uses_precise_labels_and_preserves_unknown_names() {
    for (name, expected) in [
        ("bash", "Running command"),
        ("change_files", "Editing files"),
        ("view_image", "Viewing image"),
        ("web_search", "Searching the web"),
        ("web_read", "Reading web page"),
        ("search_skills", "Finding skills"),
        ("skill", "Loading skill"),
        ("manage_process", "Managing background process"),
        ("delegate_task", "Delegating task"),
        ("update_plan", "Updating plan"),
        ("terminate_loop", "Finishing task"),
        ("custom_tool", "Running custom_tool"),
    ] {
        assert_eq!(tool_activity_text(name), expected);
    }
}

/// Read-only tools with no side effects on the workspace or the session, so a
/// run of them can execute concurrently. Results are still recorded in call
/// order, one ToolCall/ToolResult pair at a time, which is what the clients pair
/// on.
const PARALLEL_SAFE_TOOLS: [&str; 3] = ["view_image", "web_search", "web_read"];

fn scrub_vault(result: &mut Value) {
    let vault = crate::vault::Vault::load();
    if !vault.is_empty() {
        vault.scrub_value(result);
    }
}

/// Rerunning a command that prints exactly what it printed earlier in this
/// request tells the model nothing new, so it gets a short notice instead of the
/// same output again. Short outputs are left alone: they cost little and a
/// notice there would read oddly.
fn collapse_repeated_output(vars: &mut LoopVars, command: String, result: &mut Value) {
    use std::hash::{Hash, Hasher};
    const MIN_CHARS: usize = 300;
    let Some(data) = result.get_mut("data").and_then(Value::as_object_mut) else {
        return;
    };
    let log_path = data.get("saved_output_path").and_then(Value::as_str).unwrap_or("").to_string();
    let text = |key: &str| {
        let text = data.get(key).and_then(Value::as_str).unwrap_or("");
        if log_path.is_empty() { text.to_string() } else { text.replace(&log_path, "") }
    };
    let (stdout, stderr) = (text("stdout"), text("stderr"));
    if stdout.len() + stderr.len() < MIN_CHARS {
        return;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&stdout, &stderr, data.get("exit_code").map(Value::to_string)).hash(&mut hasher);
    let fingerprint = hasher.finish();
    if vars.bash_outputs.insert(command, fingerprint) == Some(fingerprint) {
        for key in ["stdout", "stderr", "hint", "truncated", "total_lines", "total_bytes"] {
            data.remove(key);
        }
        data.insert(
            "note".to_string(),
            Value::String(
                "Identical to the output of this exact command earlier in this task; nothing changed. Use that result instead of rerunning it."
                    .to_string(),
            ),
        );
    }
}

fn is_error_result(result: &Value) -> bool {
    result.get("status").and_then(Value::as_str) == Some("error")
}

fn execution_error(error: ToolError) -> Value {
    json!({
        "schema_version": 1,
        "status": "error",
        "error": {
            "code": "tool_execution_error",
            "message": error.to_string(),
        }
    })
}

/// Answer a call in both logs: the event the clients render and the message the
/// model sees. Every call must be answered — an unanswered tool_call_id makes
/// strict providers reject the whole history.
fn answer_call(state: &mut HarnessState, tool_name: &str, call_id: &str, result: Value) {
    state.events.push(HarnessEvent::ToolResult {
        tool_name: tool_name.to_string(),
        result: result.clone(),
    });
    state.messages.push(HarnessMessage::ToolResult {
        tool_call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: result,
    });
}

impl CodingHarness {
    /// Execute one turn's tool calls. `Err` ends the turn immediately
    /// (`terminate_loop`, `ask_user`); `Ok` carries the turn's productivity.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_tool_calls(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        sink: Option<&StreamHandle>,
        approval_rx: &mut mpsc::UnboundedReceiver<ApprovalDecision>,
        calls: Vec<GeneratedToolCall>,
        definitions: &[NativeToolDefinition],
    ) -> Result<TurnStats, StepResult> {
        let mut stats = TurnStats::default();
        // Manual mode: count mutating calls up front so each approval prompt can
        // show "action N of M".
        let total_mutating = calls
            .iter()
            .filter(|c| MUTATING_TOOLS.contains(&c.tool_name.as_str()))
            .count();
        let mut approval_index = 0usize;

        let mut i = 0;
        while i < calls.len() {
            let end = self.parallel_run_end(&calls, i, conversation_mode);
            if end > i + 1 {
                self.run_parallel_reads(state, lanes, &calls[i..end], &mut stats)
                    .await;
                i = end;
                continue;
            }
            let call = calls[i].clone();
            i += 1;
            if let Some(end_turn) = self
                .run_call(
                    state,
                    lanes,
                    watches,
                    vars,
                    conversation_mode,
                    sink,
                    approval_rx,
                    call,
                    definitions,
                    &mut stats,
                    total_mutating,
                    &mut approval_index,
                )
                .await
            {
                return Err(end_turn);
            }
        }
        Ok(stats)
    }

    fn is_parallel_safe(&self, call: &GeneratedToolCall, conversation_mode: bool) -> bool {
        PARALLEL_SAFE_TOOLS.contains(&call.tool_name.as_str())
            && !(conversation_mode && meta::is_meta_tool(&call.tool_name))
            && self.tools.contains(&call.tool_name)
    }

    /// End (exclusive) of the run of parallel-safe calls starting at `start`.
    fn parallel_run_end(
        &self,
        calls: &[GeneratedToolCall],
        start: usize,
        conversation_mode: bool,
    ) -> usize {
        let mut end = start;
        while end < calls.len() && self.is_parallel_safe(&calls[end], conversation_mode) {
            end += 1;
        }
        end
    }

    /// A run of read-only calls: they execute concurrently, and every pair is
    /// recorded in call order.
    async fn run_parallel_reads(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
        calls: &[GeneratedToolCall],
        stats: &mut TurnStats,
    ) {
        stats.only_delegations = false;
        for call in calls {
            self.lane_progress("tool_call", tool_activity_text(&call.tool_name));
        }
        let results = futures_util::future::join_all(calls.iter().map(|call| {
            self.tools
                .execute(&self.context, &call.tool_name, call.arguments.clone())
        }))
        .await;
        for (call, outcome) in calls.iter().zip(results) {
            let call_id = call.id.clone().unwrap_or_default();
            state.events.push(HarnessEvent::ToolCall {
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
            });
            stats.real_work += 1;
            let mut result = match outcome {
                Ok(result) => result.value,
                Err(error) => execution_error(error),
            };
            scrub_vault(&mut result);
            if is_error_result(&result) {
                stats.failed += 1;
            }
            answer_call(state, &call.tool_name, &call_id, result);
        }
        let _ = self.persist(state, lanes).await;
    }

    /// One call through the full policy: headless completion, meta tools,
    /// unknown tools, shell discipline, vault gating, manual
    /// approval, execution. `Some` ends the turn.
    #[allow(clippy::too_many_arguments)]
    async fn run_call(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        sink: Option<&StreamHandle>,
        approval_rx: &mut mpsc::UnboundedReceiver<ApprovalDecision>,
        call: GeneratedToolCall,
        definitions: &[NativeToolDefinition],
        stats: &mut TurnStats,
        total_mutating: usize,
        approval_index: &mut usize,
    ) -> Option<StepResult> {
        let tool_name = call.tool_name.clone();
        let call_id = call.id.clone().unwrap_or_default();
        if tool_name != "delegate_task" {
            stats.only_delegations = false;
        }
        state.events.push(HarnessEvent::ToolCall {
            tool_name: tool_name.clone(),
            arguments: call.arguments.clone(),
        });
        self.lane_progress("tool_call", tool_activity_text(&tool_name));

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
            answer_call(state, &tool_name, &call_id, result);
            if let Some(s) = summary {
                // A steered conversation model can call it anyway; its summary
                // IS the answer — render it as one, or the turn ends silently.
                if conversation_mode && !replied_since_last_user(&state.events) {
                    record_assistant_text(state, s.to_string(), None);
                    if let Some(sink) = sink {
                        StreamBuffer::clear(sink);
                    }
                }
                let _ = self.persist(state, lanes).await;
                return Some(StepResult::TurnEnded {
                    kind: TurnEndKind::Complete,
                    final_text: Some(s.to_string()),
                });
            }
            // Missing summary: the error result nudges a retry next turn.
            let _ = self.persist(state, lanes).await;
            return None;
        }

        let is_meta = conversation_mode
            && meta::is_meta_tool(&tool_name)
            && (self.config.allow_lane_control
                || !matches!(
                    tool_name.as_str(),
                    "delegate_task" | "cancel_delegated_task"
                ));
        if is_meta {
            let (result, control) =
                self.dispatch_meta(state, lanes, watches, &tool_name, &call.arguments);
            if tool_name == "update_plan" && !is_error_result(&result) {
                stats.had_plan = true;
                vars.turns_since_plan = 0;
                vars.reminded.insert("plan", render_plan(&state.plan));
            }
            if tool_name == "delegate_task" {
                if result.get("status").and_then(Value::as_str) == Some("success") {
                    stats.delegations_ok += 1;
                } else {
                    stats.only_delegations = false;
                }
            }
            answer_call(state, &tool_name, &call_id, result);
            let _ = self.persist(state, lanes).await;
            // ask_user pauses the run here; nothing else ends a turn now.
            if let MetaControl::EndTurn { kind, final_text } = control {
                return Some(StepResult::TurnEnded { kind, final_text });
            }
            return None;
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
            return None;
        }

        stats.real_work += 1;

        if tool_name == "bash" {
            if let Some(command) = call.arguments.get("command").and_then(Value::as_str) {
                if note_shell_command(vars, command) {
                    stats.shell_nudged = true;
                }
            }
        }

        // A bash command that references a vault secret ($NAME) ALWAYS requires
        // explicit user confirmation, regardless of approval mode or a prior
        // Approve-All. A delegated / headless run has no user to confirm, so it is
        // denied there — the step must run on the interactive thread.
        let custom = self
            .tools
            .custom_dir()
            .filter(|_| self.tools.is_custom(&tool_name))
            .and_then(|dir| crate::agent_tools::prepare(dir, &tool_name, &call.arguments))
            .and_then(Result::ok);
        let vault_secrets_used: Vec<String> = if tool_name == "bash" {
            call.arguments
                .get("command")
                .and_then(Value::as_str)
                .map(|c| crate::vault::Vault::load().referenced_names(c))
                .unwrap_or_default()
        } else if let Some(custom) = custom.as_ref() {
            custom.vault.clone()
        } else {
            Vec::new()
        };
        let first_run = custom.as_ref().is_some_and(|c| c.needs_first_approval);
        let mut lane_approved = false;
        if !conversation_mode
            && (first_run || !vault_secrets_used.is_empty())
            && let Some(route) = self.config.lane_approval.as_ref()
        {
            let summary = match custom.as_ref() {
                Some(custom) => crate::agent_tools::approval_summary(custom),
                None => format!(
                    "⚠ uses vault secret(s) [{}] — {}",
                    vault_secrets_used.join(", "),
                    approval_summary(&tool_name, &call.arguments)
                ),
            };
            if !route.ask(&tool_name, &summary).await {
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "user_denied",
                        "message": "The user denied this action. Do not retry it as-is — adjust your approach, or report what you needed."
                    }
                });
                answer_call(state, &tool_name, &call_id, result);
                let _ = self.persist(state, lanes).await;
                return None;
            }
            if let (Some(custom), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                && custom.needs_first_approval
            {
                crate::agent_tools::approve(dir, &custom.tool);
            }
            lane_approved = true;
        }
        if first_run && !conversation_mode && !lane_approved {
            let result = json!({
                "schema_version": 1,
                "status": "error",
                "error": {
                    "code": "custom_tool_needs_approval",
                    "message": format!("The custom tool `{tool_name}` is new and needs the user's approval before its first run, which isn't available in a delegated/headless run. Report that it needs approving, so it's run once on the main thread.")
                }
            });
            answer_call(state, &tool_name, &call_id, result);
            let _ = self.persist(state, lanes).await;
            return None;
        }
        if !vault_secrets_used.is_empty() && !conversation_mode && !lane_approved {
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
            answer_call(state, &tool_name, &call_id, result);
            let _ = self.persist(state, lanes).await;
            return None;
        }
        let force_vault_approval = !vault_secrets_used.is_empty() || first_run;

        // Manual mode (or any vault-secret call): pause for the user's decision.
        if !lane_approved
            && (force_vault_approval
                || (state.approval_mode == ApprovalMode::Manual
                    && (MUTATING_TOOLS.contains(&tool_name.as_str()) || custom.is_some())))
        {
            *approval_index += 1;
            // Discard stale decisions queued before this prompt existed — an
            // unconsumed leftover would instantly "approve" an unseen action.
            while approval_rx.try_recv().is_ok() {}
            let summary = match custom.as_ref() {
                Some(custom) => crate::agent_tools::approval_summary(custom),
                None if force_vault_approval => format!(
                    "⚠ uses vault secret(s) [{}] — {}",
                    vault_secrets_used.join(", "),
                    approval_summary(&tool_name, &call.arguments)
                ),
                None => approval_summary(&tool_name, &call.arguments),
            };
            state.events.push(HarnessEvent::ApprovalRequest {
                tool_name: tool_name.clone(),
                summary,
                index: *approval_index,
                total: total_mutating.max(*approval_index),
            });
            state.status = HarnessStatus::WaitingForInput;
            let _ = self.persist(state, lanes).await;
            let decision = approval_rx.recv().await;
            state.status = HarnessStatus::Running;
            // Approve-All flips to Auto for ordinary mutating calls — never off a
            // vault prompt, which must not authorize an unattended mode.
            if matches!(decision, Some(ApprovalDecision::ApproveAll)) && !force_vault_approval {
                state.approval_mode = ApprovalMode::Auto;
            }
            let approved = matches!(
                decision,
                Some(ApprovalDecision::Approve | ApprovalDecision::ApproveAll)
            );
            if approved
                && let (Some(custom), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                && custom.needs_first_approval
            {
                crate::agent_tools::approve(dir, &custom.tool);
            }
            if !approved {
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "user_denied",
                        "message": "The user denied this action. Do not retry it as-is — adjust your approach or ask what they'd prefer."
                    }
                });
                answer_call(state, &tool_name, &call_id, result);
                let _ = self.persist(state, lanes).await;
                return None;
            }
        }

        // Surface the in-flight call before running it so a slow tool (bash,
        // web fetch) isn't a black box.
        let _ = self.persist(state, lanes).await;

        let edit_path = (tool_name == "change_files")
            .then(|| {
                call.arguments
                    .pointer("/changes/0/path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let markdown_read = match call.arguments.get("command").and_then(Value::as_str) {
            Some(command) if tool_name == "bash" && self.config.memory_enabled => {
                crate::memory::markdown_paths_in_command(command, &self.context.current_dir())
            }
            _ => Vec::new(),
        };
        let bash_command = (tool_name == "bash")
            .then(|| call.arguments.get("command").and_then(Value::as_str).map(str::to_string))
            .flatten();
        let background = call.arguments.get("background").and_then(Value::as_bool) == Some(true);
        let change_args = (tool_name == "change_files").then(|| call.arguments.clone());
        let cwd_before = self.context.current_dir();
        let mut result = match self
            .tools
            .execute(&self.context, &tool_name, call.arguments)
            .await
        {
            Ok(result) => result.value,
            Err(error) => execution_error(error),
        };
        // Vault choke point: secret values become [vault:NAME] before a result
        // enters the conversation.
        scrub_vault(&mut result);
        let is_err = is_error_result(&result);
        if is_err {
            stats.failed += 1;
        }
        if tool_name == "change_files" {
            note_edit_result(vars, edit_path, is_err);
            if let Some(arguments) = change_args.as_ref().filter(|_| !is_err) {
                note_file_change(vars, arguments);
            }
        }
        if let Some(command) = bash_command
            && !is_err
            && !background
        {
            note_bash_command(vars, &command, &cwd_before);
            collapse_repeated_output(vars, command, &mut result);
        }
        if !is_err && !markdown_read.is_empty() {
            crate::memory::Memory::open(self.context.workspace_root()).record_reads(&markdown_read);
        }
        answer_call(state, &tool_name, &call_id, result);
        // Flush after every tool result so a mid-batch kill still keeps completed
        // calls on disk.
        let _ = self.persist(state, lanes).await;
        None
    }
}
