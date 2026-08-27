//! The single seam between a frontend and the agent: build the model + tools +
//! harness for a config and spawn the resident `run_interactive` loop. Drive it by
//! sending `LoopInput` on `input_tx`; observe it via the persisted `HarnessState`
//! (and, optionally, a live `StreamHandle`). Shared by the TUI and headless `serve`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::builtins::coding_tools;
use crate::config::{SnippetConfig, workspaces_root};
use crate::harness::{CodingHarness, HarnessConfig, HarnessState, LoopInput, deserialize_state};
use crate::lanes::ModelFactory;
use crate::llm::StreamHandle;
use crate::prompts::{conversation_system_prompt, mission_control_system_prompt};
use crate::tools::{BrowserSummaryProvider, ToolContext, ToolRegistry};

pub struct SessionHandle {
    pub input_tx: mpsc::UnboundedSender<LoopInput>,
    pub join: tokio::task::JoinHandle<Result<HarnessState, String>>,
    pub state_path: PathBuf,
    /// Live token stream shared with attached UIs (TUI / mobile via WS).
    pub stream: Option<StreamHandle>,
}

/// Spawn a resident conversation session for `config`, persisting to `state_path`.
/// `stream` carries live text deltas to a UI sink; pass `None` for headless callers
/// that only read committed `HarnessState`.
pub fn start_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
) -> SessionHandle {
    start_session_with_role(config, state_path, initial, resume, stream, None, false)
}

pub fn start_mission_control_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
) -> SessionHandle {
    start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        true,
    )
}

pub fn start_session_with_browser_summary(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
) -> SessionHandle {
    start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        false,
    )
}

fn start_session_with_role(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
    mission_control: bool,
) -> SessionHandle {
    let (input_tx, rx) = mpsc::unbounded_channel();

    let workspace = config.workspace.clone();
    let model_config = config.model.clone();
    let exa_api_key = config.exa_api_key.clone();
    let manual_approval = config.manual_approval;
    let context_window_tokens = model_config.context_window;
    let compact_at_pct = model_config.compact_at_pct;
    let memory_enabled = config.memory_enabled;
    let memory_index_budget_chars = config.memory_index_budget_chars;
    let memory_entry_budget_chars = config.memory_entry_budget_chars;
    let memory_max_entries = config.memory_max_entries;
    let memory_reflect_on_compaction = config.memory_reflect_on_compaction;
    // Delegated lanes may run on a different model than the active session
    // (see `delegate_model_config`) — a cheaper model for parallel grunt work or
    // a stronger one for hard sub-tasks. Falls back to the active model.
    // Mission Control never gets a factory: it routes, it does not spawn lanes.
    let factory: Option<ModelFactory> = if mission_control {
        None
    } else {
        let mc = config.delegate_model_config();
        Some(Arc::new(move || mc.build_model()))
    };
    let sp = state_path.clone();
    let stream_out = stream.clone();

    let join = tokio::spawn(async move {
        let mut model = model_config.build_model();
        // Durable identity = state path relative to the workspaces root — the
        // same id the daemon uses for managed sessions and task envelopes.
        let durable_id = if mission_control {
            Some(crate::mission_control::SESSION_ID.to_string())
        } else {
            sp.strip_prefix(crate::config::workspaces_root())
                .ok()
                .map(|p| p.display().to_string())
        };
        let base_context = if mission_control {
            ToolContext::mission_control(workspace)
        } else {
            match browser_summary {
                Some(provider) => ToolContext::with_browser_summary(workspace, provider),
                None => ToolContext::new(workspace),
            }
        }
        .map_err(|e| e.to_string())?;
        let context = match durable_id {
            Some(id) => base_context.with_durable_session_id(id),
            None => base_context,
        };
        // MC is a router. Whitelist inspection + routing only — never the
        // coding toolkit or a lane factory. bash/read_image are for seeing
        // status and screenshots, not implementing.
        let tools = if mission_control {
            let mut tools = ToolRegistry::new();
            tools.insert(crate::builtins::BashTool);
            tools.insert(crate::builtins::ReadImageTool);
            crate::mission_tools::add_mission_control_tools(&mut tools);
            tools
        } else {
            let mut tools = coding_tools(
                exa_api_key.clone(),
                crate::memory::MemoryLimits {
                    enabled: memory_enabled,
                    writable: true,
                    index_budget_chars: memory_index_budget_chars,
                    entry_budget_chars: memory_entry_budget_chars,
                    max_entries: memory_max_entries,
                },
            );
            crate::mission_tools::add_worker_report_tool(&mut tools);
            tools
        };
        let harness = CodingHarness::new(
            HarnessConfig {
                system_prompt: if mission_control {
                    mission_control_system_prompt()
                } else {
                    conversation_system_prompt()
                },
                state_path: Some(sp),
                resume,
                exa_api_key: exa_api_key.clone(),
                context_window_tokens,
                compact_at_pct,
                manual_approval,
                memory_enabled,
                memory_index_budget_chars,
                memory_entry_budget_chars,
                memory_max_entries,
                memory_reflect_on_compaction,
                allow_lane_control: !mission_control,
                ..HarnessConfig::default()
            },
            tools,
            context,
        );
        harness
            .run_interactive(&mut model, initial, rx, factory, stream)
            .await
            .map_err(|e| e.to_string())
    });

    SessionHandle {
        input_tx,
        join,
        state_path,
        stream: stream_out,
    }
}

/// One session as seen on disk, for the serve daemon's device-wide list.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// Stable id = the state file's path relative to the workspaces root
    /// (e.g. `snipett-2a3f/state.json`). Used to resolve the session for /attach.
    pub id: String,
    /// Absolute workspace folder.
    pub folder: String,
    /// Conversation name (`default` for the active state, else the saved name).
    pub conversation: String,
    /// First user request (truncated), for a list label.
    pub title: String,
    pub status: String,
    /// Last-active time, unix seconds.
    pub last_active: i64,
}

/// Resolve a session id (relative path under the workspaces root) to its state
/// file, rejecting any path that escapes the root.
pub fn state_path_for_id(id: &str) -> Option<PathBuf> {
    if crate::mission_control::is_session_id(id) {
        let path = crate::mission_control::session_state_path();
        return path.exists().then_some(path);
    }
    let root = workspaces_root();
    let path = root.join(id);
    // Reject traversal: the resolved path must stay under the root.
    let canon_root = std::fs::canonicalize(&root).ok()?;
    let canon = std::fs::canonicalize(&path).ok()?;
    canon.starts_with(&canon_root).then_some(canon)
}

/// Sidecar file holding a session's per-conversation model override (the profile
/// name), kept next to its state file so it survives daemon restarts.
fn profile_sidecar(state_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.profile", state_path.display()))
}

/// Read a session's persisted model override, if one was set.
pub fn read_session_profile(state_path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(profile_sidecar(state_path)).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Persist a session's model override (or clear it when `profile` is empty).
pub fn write_session_profile(state_path: &std::path::Path, profile: &str) {
    let path = profile_sidecar(state_path);
    if profile.trim().is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(path, profile.trim());
}

/// Enumerate every session persisted on the device (across all workspaces).
pub fn list_device_sessions() -> Vec<SessionInfo> {
    let root = workspaces_root();
    let mut out = Vec::new();
    let Ok(workspaces) = std::fs::read_dir(&root) else {
        return out;
    };
    for ws in workspaces.flatten() {
        let dir = ws.path();
        if !dir.is_dir() {
            continue;
        }
        read_session(&dir.join("state.json"), &root, "default", &mut out);
        if let Ok(convs) = std::fs::read_dir(dir.join("conversations")) {
            for c in convs.flatten() {
                let p = c.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    let name = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() {
                        read_session(&p, &root, &name, &mut out);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    if let Some(mc) = mission_control_list_row() {
        out.insert(0, mc);
    }
    out
}

fn mission_control_list_row() -> Option<SessionInfo> {
    let path = crate::mission_control::session_state_path();
    if !path.exists() {
        return None;
    }
    let last_active = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (folder, title, status) = if let Some(meta) = std::fs::read(meta_path(&path))
        .ok()
        .and_then(|b| serde_json::from_slice::<SessionMeta>(&b).ok())
    {
        (meta.folder, meta.title, meta.status)
    } else {
        (
            crate::mission_control::workspace_path()
                .display()
                .to_string(),
            "Mission Control".to_string(),
            "idle".to_string(),
        )
    };
    Some(SessionInfo {
        id: crate::mission_control::SESSION_ID.to_string(),
        folder,
        conversation: "default".to_string(),
        title: if title.trim().is_empty() {
            "Mission Control".to_string()
        } else {
            title
        },
        status,
        last_active,
    })
}

/// Lightweight session metadata, written next to each state file so listing can
/// skip decompressing/parsing the full conversation (the scaling path).
#[derive(Serialize, Deserialize)]
struct SessionMeta {
    folder: String,
    title: String,
    status: String,
}

/// `<conv>.json` → `<conv>.meta.json`.
fn meta_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("meta.json")
}

/// The session-list label. New state stores this in `title`; the initial request
/// fallback is only for old states while they are being migrated.
fn effective_title(state: &HarnessState) -> String {
    state
        .title
        .as_deref()
        .or_else(|| state.initial_request())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(120).collect())
        .unwrap_or_default()
}

/// The status string exposed on the session list / events APIs. Uses the enum's
/// serde (snake_case) name.
pub fn status_str(status: crate::harness::HarnessStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn meta_from_state(state: &HarnessState) -> SessionMeta {
    SessionMeta {
        folder: state.workspace.clone(),
        title: effective_title(state),
        status: status_str(state.status),
    }
}

/// Set (or clear, if empty) a saved session's title override and rewrite its
/// sidecar. For sessions that aren't currently live — the daemon routes live ones
/// through the loop so its in-memory state stays in sync.
pub fn set_session_title(state_path: &Path, title: &str) -> Result<(), String> {
    let bytes = std::fs::read(state_path).map_err(|e| e.to_string())?;
    let mut state = deserialize_state(&bytes)?;
    let t = title.trim();
    state.title = if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    };
    let out = crate::harness::serialize_state(&state)?;
    // Temp + rename like `persist_state`: a crash mid-write must never leave a
    // truncated state file (an unreadable state is silently started-over on open).
    let tmp = state_path.with_extension("json.tmp");
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state_path).map_err(|e| e.to_string())?;
    write_session_meta(state_path, &state);
    Ok(())
}

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
    let forked = build_forked_state(source, point);
    let title = forked.title.clone().unwrap_or_else(|| "fork".to_string());

    let parent = source_state_path
        .parent()
        .ok_or_else(|| "source state path has no parent".to_string())?;
    // Source may be `state.json` or `conversations/<id>.json` — forks always land
    // in `conversations/` beside the workspace state root.
    let conv_dir = if parent.file_name().and_then(|s| s.to_str()) == Some("conversations") {
        parent.to_path_buf()
    } else {
        parent.join("conversations")
    };
    std::fs::create_dir_all(&conv_dir).map_err(|e| format!("create conversations dir: {e}"))?;

    let name = uuid::Uuid::new_v4().to_string();
    let dest = conv_dir.join(format!("{name}.json"));
    let bytes = crate::harness::serialize_state(&forked)?;
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write fork: {e}"))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename fork: {e}"))?;
    write_session_meta(&dest, &forked);

    // Carry the per-conversation model override onto the branch.
    if let Some(profile) = read_session_profile(source_state_path) {
        write_session_profile(&dest, &profile);
    }

    let root = workspaces_root();
    let id = dest
        .strip_prefix(&root)
        .unwrap_or(&dest)
        .display()
        .to_string();

    Ok(ForkedConversation {
        id,
        state_path: dest,
        title,
        event_end: point.event_end.min(source.events.len()),
        message_end: point.message_end.min(source.messages.len()),
    })
}

/// Write the metadata sidecar for a state file (best-effort). Called on every
/// persist so the sidecar tracks the latest title/folder/status.
pub fn write_session_meta(state_path: &Path, state: &HarnessState) {
    if let Ok(s) = serde_json::to_string(&meta_from_state(state)) {
        let _ = std::fs::write(meta_path(state_path), s);
    }
}

/// If `folder` is a git work tree, add a detached worktree under
/// `~/.snippet/worktrees/{repo}/{id}` and return that path (preserving a
/// subfolder relative to the repo root). Non-git folders, nested worktrees
/// already under that root, and any git failure fall back to `folder`.
pub fn prepare_new_session_workspace(folder: &Path) -> PathBuf {
    try_session_worktree(folder).unwrap_or_else(|| folder.to_path_buf())
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn sanitize_repo_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "repo".into()
    } else {
        s
    }
}

fn try_session_worktree(folder: &Path) -> Option<PathBuf> {
    let inside = git_stdout(folder, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let toplevel = PathBuf::from(git_stdout(folder, &["rev-parse", "--show-toplevel"])?);
    let root = crate::config::worktrees_root();
    if folder.starts_with(&root) || toplevel.starts_with(&root) {
        return None;
    }
    let repo = sanitize_repo_name(toplevel.file_name()?.to_str()?);
    let id = uuid::Uuid::new_v4().to_string();
    let dest = root.join(repo).join(&id[..8]);
    std::fs::create_dir_all(dest.parent()?).ok()?;
    if dest.exists() {
        return None;
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&toplevel)
        .args(["worktree", "add", "--detach"])
        .arg(&dest)
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dest);
        return None;
    }
    let workspace = match folder.strip_prefix(&toplevel) {
        Ok(rel) if !rel.as_os_str().is_empty() => dest.join(rel),
        _ => dest,
    };
    Some(workspace)
}

/// Persist a brand-new idle conversation in `folder` so Mission Control can
/// dispatch to it. `new_conversation=false` uses the folder's default
/// `state.json` (refuses if one already exists). `true` always writes a
/// fresh `conversations/<uuid>.json`. Git repos get an isolated worktree.
pub fn create_blank_session(
    folder: &Path,
    title: &str,
    new_conversation: bool,
) -> Result<SessionInfo, String> {
    let mut folder = folder
        .canonicalize()
        .map_err(|e| format!("folder is not a directory: {e}"))?;
    if !folder.is_dir() {
        return Err("folder is not a directory".into());
    }
    if new_conversation {
        folder = prepare_new_session_workspace(&folder);
        if let Ok(canonical) = folder.canonicalize() {
            folder = canonical;
        }
    }
    let base = crate::config::state_path_for_workspace(&folder);
    let dest = if new_conversation {
        let parent = base
            .parent()
            .ok_or_else(|| "workspace state path has no parent".to_string())?;
        let conv_dir = parent.join("conversations");
        std::fs::create_dir_all(&conv_dir).map_err(|e| format!("create conversations dir: {e}"))?;
        conv_dir.join(format!("{}.json", uuid::Uuid::new_v4()))
    } else {
        if base.exists() {
            return Err(
                "this folder already has a default session — pass new_conversation=true or route to the existing id".into(),
            );
        }
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create workspace dir: {e}"))?;
        }
        base
    };

    let label = title.trim();
    let state = HarnessState::blank(
        folder.display().to_string(),
        (!label.is_empty()).then(|| label.to_string()),
    );
    let bytes = crate::harness::serialize_state(&state)?;
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write session: {e}"))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename session: {e}"))?;
    write_session_meta(&dest, &state);

    let root = workspaces_root();
    let id = dest
        .strip_prefix(&root)
        .unwrap_or(&dest)
        .display()
        .to_string();
    Ok(SessionInfo {
        id,
        folder: folder.display().to_string(),
        conversation: if new_conversation {
            dest.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("new")
                .to_string()
        } else {
            "default".into()
        },
        title: effective_title(&state),
        status: status_str(state.status),
        last_active: 0,
    })
}

/// Remove a session's state file and its sidecars (metadata + model override —
/// a leftover `.profile` would silently re-apply the deleted session's model to
/// the next session opened on this state path).
pub fn remove_session_files(state_path: &Path) {
    let _ = std::fs::remove_file(state_path);
    let _ = std::fs::remove_file(meta_path(state_path));
    let _ = std::fs::remove_file(state_path.with_extension("nonces.json"));
    let _ = std::fs::remove_file(profile_sidecar(state_path));
}

fn read_session(path: &Path, root: &Path, conversation: &str, out: &mut Vec<SessionInfo>) {
    let last_active = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let id = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    if crate::mission_control::is_session_id(&id) {
        return;
    }
    let role = std::fs::read_to_string(PathBuf::from(format!("{}.role", path.display())))
        .ok()
        .map(|s| s.trim().to_string());
    if role.as_deref() == Some("mission_control") {
        return;
    }

    // Fast path: read the tiny sidecar, no decompression.
    if let Some(meta) = std::fs::read(meta_path(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<SessionMeta>(&b).ok())
    {
        out.push(SessionInfo {
            id,
            folder: meta.folder,
            conversation: conversation.to_string(),
            title: meta.title,
            status: meta.status,
            last_active,
        });
        return;
    }

    // Slow path (pre-sidecar sessions): decompress once, then backfill the sidecar.
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return;
    };
    write_session_meta(path, &state);
    out.push(SessionInfo {
        id,
        folder: state.workspace.clone(),
        conversation: conversation.to_string(),
        title: effective_title(&state),
        status: status_str(state.status),
        last_active,
    });
}

#[cfg(test)]
mod fork_tests {
    use super::*;
    use crate::harness::{HarnessEvent, HarnessState, HarnessStatus};
    use crate::llm::HarnessMessage;

    fn sample_state() -> HarnessState {
        // Build via JSON so private migration fields stay internal.
        let mut s: HarnessState = serde_json::from_value(serde_json::json!({
            "version": 1,
            "status": "idle",
            "created_at": "t0",
            "updated_at": "t0",
            "workspace": "/tmp/ws",
            "title": "original title",
            "messages": [],
            "events": [],
            "iterations": 3,
            "total_tokens": 100,
            "prompt_tokens": 80,
            "completion_tokens": 20,
            "last_prompt_tokens": 50,
            "context_window": 128000
        }))
        .expect("sample state");
        s.messages = vec![
            HarnessMessage::User {
                content: "hi".into(),
            },
            HarnessMessage::Assistant {
                content: "hello".into(),
                tool_calls: Vec::new(),
            },
            HarnessMessage::User {
                content: "again".into(),
            },
        ];
        s.events = vec![
            HarnessEvent::UserInput { text: "hi".into() },
            HarnessEvent::AssistantText {
                text: "hello".into(),
            },
            HarnessEvent::UserInput {
                text: "again".into(),
            },
        ];
        s.checkpoints = vec![crate::harness::CheckpointRecord {
            id: "abc12345deadbeef".into(),
            label: "hi".into(),
            created_at: "t0".into(),
            event_index: 0,
            message_index: 0,
        }];
        s
    }

    #[test]
    fn resolve_checkpoint_cut() {
        let s = sample_state();
        let p = resolve_fork_point(&s, Some("abc12345"), None).unwrap();
        assert_eq!(p.event_end, 0);
        assert_eq!(p.message_end, 0);
    }

    #[test]
    fn resolve_event_index_inclusive() {
        let s = sample_state();
        let p = resolve_fork_point(&s, None, Some(1)).unwrap();
        assert_eq!(p.event_end, 2); // keep through index 1
    }

    #[test]
    fn build_fork_truncates_and_idles() {
        let s = sample_state();
        let p = ForkPoint {
            event_end: 2,
            message_end: 2,
        };
        let f = build_forked_state(&s, p);
        assert_eq!(f.events.len(), 2);
        assert_eq!(f.messages.len(), 2);
        assert_eq!(f.status, HarnessStatus::Idle);
        assert!(f.lanes.is_empty());
        assert!(f.title.as_deref().unwrap_or("").starts_with("fork ·"));
        assert_eq!(f.total_tokens, 0);
        assert!(!f.tool_payloads_pruned);
    }

    #[test]
    fn snaps_orphan_tool_call_at_end() {
        let mut s = sample_state();
        s.events.push(HarnessEvent::ToolCall {
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        });
        s.messages.push(HarnessMessage::Assistant {
            content: String::new(),
            tool_calls: Vec::new(),
        });
        let last = s.events.len() - 1;
        let p = resolve_fork_point(&s, None, Some(last)).unwrap();
        // Exclusive end must not leave a trailing ToolCall.
        if p.event_end > 0 {
            assert!(!matches!(
                s.events[p.event_end - 1],
                HarnessEvent::ToolCall { .. }
            ));
        }
    }
}

#[cfg(test)]
mod create_blank_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn create_blank_session_writes_idle_state() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("snippet-mc-blank-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        let info = create_blank_session(&folder, "Odd request", true).unwrap();
        assert_eq!(info.title, "Odd request");
        assert_eq!(info.status, "idle");
        assert_eq!(info.folder, folder.canonicalize().unwrap().display().to_string());
        let path = state_path_for_id(&info.id).expect("created session is resolvable");
        assert!(path.exists());
        let state = deserialize_state(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(state.status, crate::harness::HarnessStatus::Idle);
        assert_eq!(state.title.as_deref(), Some("Odd request"));
        let _ = fs::remove_dir_all(&folder);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(parent.join(format!(
                "{}.meta.json",
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
            )));
        }
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    #[test]
    fn new_session_in_git_repo_uses_isolated_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!("snippet-wt-repo-{stamp}"));
        let trees = std::env::temp_dir().join(format!("snippet-wt-root-{stamp}"));
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q"]);
        git_ok(&repo, &["config", "user.email", "snippet@test"]);
        git_ok(&repo, &["config", "user.name", "snippet"]);
        fs::write(repo.join("README"), "hi\n").unwrap();
        git_ok(&repo, &["add", "README"]);
        git_ok(&repo, &["commit", "-qm", "init"]);
        let repo = repo.canonicalize().unwrap();
        let prev = std::env::var_os("SNIPPET_WORKTREES");
        // Test-only isolation of the worktree root; restored immediately after.
        unsafe { std::env::set_var("SNIPPET_WORKTREES", &trees) };
        let workspace = prepare_new_session_workspace(&repo);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("SNIPPET_WORKTREES", v),
                None => std::env::remove_var("SNIPPET_WORKTREES"),
            }
        }
        assert_ne!(workspace, repo);
        assert!(workspace.starts_with(&trees));
        assert!(workspace.join(".git").exists());
        assert!(workspace.join("README").exists());
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force"])
            .arg(&workspace)
            .status();
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&trees);
    }

    #[test]
    fn non_git_folder_stays_put() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("snippet-wt-plain-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        let got = prepare_new_session_workspace(&folder);
        assert_eq!(got, folder);
        let _ = fs::remove_dir_all(&folder);
    }
}
