use std::path::{Path, PathBuf};
use crate::harness::HarnessState;
use crate::session::*;

/// Where a fork cuts the source conversation. Both ends are exclusive lengths
/// (`events[..event_end]`, `messages[..message_end]`).
#[derive(Debug, Clone, Copy)]
pub struct ForkPoint {
    pub event_end: usize,
    pub message_end: usize,
}

/// Resolve a fork cut from a checkpoint id and/or event index.
///
/// - **checkpoint**: same boundary as `/rewind` (state *before* that turn).
/// - **event_index**: keep through that event (inclusive), then snap back to a
///   provider-safe boundary (no orphan tool_call / tool_result pairs).
/// - both: checkpoint wins for the cut; event_index is ignored.
pub fn resolve_fork_point(
    state: &HarnessState,
    checkpoint: Option<&str>,
    event_index: Option<usize>,
) -> Result<ForkPoint, String> {
    if let Some(id) = checkpoint.map(str::trim).filter(|s| !s.is_empty()) {
        let cp = state
            .checkpoints
            .iter()
            .rev()
            .find(|c| c.id == id || c.id.starts_with(id))
            .ok_or_else(|| format!("no checkpoint matching `{id}`"))?;
        return Ok(ForkPoint {
            event_end: cp.event_index.min(state.events.len()),
            message_end: cp.message_index.min(state.messages.len()),
        });
    }
    let Some(idx) = event_index else {
        return Err("fork requires `checkpoint` or `event_index`".into());
    };
    if state.events.is_empty() {
        return Err("nothing to fork — session has no events".into());
    }
    if idx >= state.events.len() {
        return Err(format!(
            "event_index {idx} out of range (0..{})",
            state.events.len().saturating_sub(1)
        ));
    }
    // Keep through idx (inclusive), then walk back to a safe tool-pairing boundary.
    let mut event_end = idx + 1;
    event_end = snap_event_end_safe(&state.events, event_end);
    let message_end = message_end_for_events(state, event_end);
    Ok(ForkPoint {
        event_end,
        message_end,
    })
}

/// Walk exclusive `event_end` backward so we don't strand a tool_call without its
/// result (or a trailing tool_result without its call) — providers 400 on that.
fn snap_event_end_safe(events: &[crate::harness::HarnessEvent], mut end: usize) -> usize {
    use crate::harness::HarnessEvent;
    end = end.min(events.len());
    while end > 0 {
        match &events[end - 1] {
            HarnessEvent::ToolResult { .. } => {
                // Ensure a ToolCall exists earlier in the kept prefix for pairing
                // at the tail; if the tail is ToolResult after ToolCall we're fine.
                break;
            }
            HarnessEvent::ToolCall { .. } => {
                // Orphan call at end — drop it.
                end -= 1;
            }
            HarnessEvent::ApprovalRequest { .. } | HarnessEvent::InvalidToolCall { .. } => {
                end -= 1;
            }
            _ => break,
        }
    }
    end
}

/// Best-effort message length matching a kept event prefix.
/// Prefer a checkpoint on the same boundary; otherwise count user/assistant/tool
/// events and consume messages in order until those counts are met.
fn message_end_for_events(state: &HarnessState, event_end: usize) -> usize {
    use crate::harness::HarnessEvent;
    use crate::llm::HarnessMessage;

    if let Some(cp) = state
        .checkpoints
        .iter()
        .filter(|c| c.event_index == event_end)
        .last()
    {
        return cp.message_index.min(state.messages.len());
    }
    // Nearest checkpoint at or before the cut — start counts from there.
    let (mut base_event, mut base_msg) = state
        .checkpoints
        .iter()
        .filter(|c| c.event_index <= event_end)
        .max_by_key(|c| c.event_index)
        .map(|c| (c.event_index, c.message_index))
        .unwrap_or((0, 0));
    base_event = base_event.min(event_end);
    base_msg = base_msg.min(state.messages.len());

    let mut need_user = 0usize;
    let mut need_assistant = 0usize;
    let mut need_tool = 0usize;
    for ev in state
        .events
        .get(base_event..event_end)
        .into_iter()
        .flatten()
    {
        match ev {
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. } => need_user += 1,
            HarnessEvent::AssistantText { .. } => need_assistant += 1,
            HarnessEvent::ToolCall { .. } | HarnessEvent::ToolResult { .. } => need_tool += 1,
            _ => {}
        }
    }

    let mut i = base_msg;
    let mut got_user = 0usize;
    let mut got_assistant = 0usize;
    let mut got_tool = 0usize;
    while i < state.messages.len() {
        if got_user >= need_user && got_assistant >= need_assistant && got_tool >= need_tool {
            break;
        }
        match &state.messages[i] {
            HarnessMessage::User { .. } => {
                if got_user >= need_user {
                    break;
                }
                got_user += 1;
            }
            HarnessMessage::Assistant { .. } => {
                if got_assistant >= need_assistant && got_tool >= need_tool && got_user >= need_user
                {
                    // Extra assistant after targets met — stop before it.
                    break;
                }
                got_assistant += 1;
            }
            HarnessMessage::ToolResult { .. } => {
                got_tool += 1;
            }
            HarnessMessage::System { .. } | HarnessMessage::Summary { .. } => {}
        }
        i += 1;
    }
    i
}

/// Build a forked [`HarnessState`]: history truncated to `point`, idle, no live
/// lanes/watches/questions. Workspace path is unchanged (shared files on disk).
pub fn build_forked_state(source: &HarnessState, point: ForkPoint) -> HarnessState {
    use crate::harness::{ApprovalMode, HarnessStatus};

    let now = chrono::Utc::now().to_rfc3339();
    let event_end = point.event_end.min(source.events.len());
    let message_end = point.message_end.min(source.messages.len());

    let mut forked = source.clone();
    forked.events.truncate(event_end);
    forked.messages.truncate(message_end);
    forked
        .checkpoints
        .retain(|c| c.event_index <= event_end && c.message_index <= message_end);
    forked.lanes.clear();
    forked.watches.clear();
    forked.pending_question = None;
    forked.goal = None;
    forked.compacting = false;
    forked.compacting_started_at = None;
    forked.turn_started_at = None;
    forked.final_text = None;
    forked.status = HarnessStatus::Idle;
    forked.approval_mode = ApprovalMode::Auto;
    // Fresh usage accounting for the branch (history is what matters).
    forked.total_tokens = 0;
    forked.prompt_tokens = 0;
    forked.completion_tokens = 0;
    forked.cache_read_tokens = 0;
    forked.tool_payloads_pruned = false;
    forked.queued_inputs.clear();
    // Keep last_prompt_tokens / context_window as hints; model will refresh.
    forked.created_at = now.clone();
    forked.updated_at = now;
    forked.iterations = 0;

    let base_title = source
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("fork");
    let short: String = base_title.chars().take(60).collect();
    forked.title = Some(format!("fork · {short}"));
    forked
}

/// Result of writing a forked conversation next to the source session.
#[derive(Debug, Clone)]
pub struct ForkedConversation {
    /// Session id relative to the workspaces root (same form as `/sessions`).
    pub id: String,
    pub state_path: PathBuf,
    pub title: String,
    pub event_end: usize,
    pub message_end: usize,
}

/// Fork `source_state_path` at `point` into a new `conversations/<uuid>.json`.
/// Copies the model-profile sidecar when present. Does **not** start a live loop.
pub fn write_forked_conversation(
    source_state_path: &Path,
    source: &HarnessState,
    point: ForkPoint,
) -> Result<ForkedConversation, String> {
    let store =
        store_for_sessions().ok_or_else(|| "the session store is unavailable".to_string())?;
    write_forked_conversation_in(&store, source_state_path, source, point)
}

/// [`write_forked_conversation`] against an explicit store, so tests use an
/// in-memory one instead of writing into the real database.
pub fn write_forked_conversation_in(
    store: &crate::store::Store,
    source_state_path: &Path,
    source: &HarnessState,
    point: ForkPoint,
) -> Result<ForkedConversation, String> {
    let forked = build_forked_state(source, point);
    let title = forked.title.clone().unwrap_or_else(|| "fork".to_string());

    let parent = source_state_path
        .parent()
        .ok_or_else(|| "source state path has no parent".to_string())?;
    // Forks always land in `conversations/` beside the workspace state root.
    let conv_dir = if parent.file_name().and_then(|s| s.to_str()) == Some("conversations") {
        parent.to_path_buf()
    } else {
        parent.join("conversations")
    };

    let name = uuid::Uuid::new_v4().to_string();
    let dest = conv_dir.join(format!("{name}.json"));
    let root = workspaces_root();
    let id = dest
        .strip_prefix(&root)
        .unwrap_or(&dest)
        .display()
        .to_string();

    // The branch is a store row, like every other session — writing only a file
    // would make it unopenable, since reads are store-only.
    let extras = crate::conversations::SessionExtras {
        // Creating a branch is a user action: put it at the top of the list.
        last_active: Some(now_unix_secs()),
        // Carry the per-conversation model override onto the branch.
        profile: read_session_profile(source_state_path),
        ..Default::default()
    };
    store
        .import_session(
            &id,
            &crate::config::workspace_key(Path::new(&forked.workspace)),
            &forked,
            &extras,
        )
        .map_err(|e| format!("write fork: {e}"))?;

    Ok(ForkedConversation {
        id,
        state_path: dest,
        title,
        event_end: point.event_end.min(source.events.len()),
        message_end: point.message_end.min(source.messages.len()),
    })
}

