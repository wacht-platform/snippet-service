use super::*;

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

pub(super) fn normalize_tool_aliases(calls: &mut [GeneratedToolCall]) {
    for call in calls {
        if call.tool_name == "execute_command" {
            call.tool_name = "bash".to_string();
        }
    }
}
