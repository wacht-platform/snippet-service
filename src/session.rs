//! The single seam between a frontend and the agent: build the model + tools +
//! harness for a config and spawn the resident `run_interactive` loop. Drive it by
//! sending `LoopInput` on `input_tx`; observe it via the persisted `HarnessState`
//! (and, optionally, a live `StreamHandle`). Shared by the TUI and headless `serve`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::builtins::coding_tools;
use crate::config::{SnippetConfig, workspaces_root};
use crate::harness::{CodingHarness, HarnessConfig, HarnessState, LoopInput, deserialize_state};
use crate::lanes::ModelFactory;
use crate::llm::StreamHandle;
use crate::prompts::conversation_system_prompt;
use crate::tools::ToolContext;

pub struct SessionHandle {
    pub input_tx: mpsc::UnboundedSender<LoopInput>,
    pub join: tokio::task::JoinHandle<Result<HarnessState, String>>,
    pub state_path: PathBuf,
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
    let factory: ModelFactory = {
        let mc = model_config.clone();
        Arc::new(move || mc.build_model())
    };
    let sp = state_path.clone();

    let join = tokio::spawn(async move {
        let mut model = model_config.build_model();
        let context = ToolContext::new(workspace).map_err(|e| e.to_string())?;
        let harness = CodingHarness::new(
            HarnessConfig {
                system_prompt: conversation_system_prompt(),
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
                ..HarnessConfig::default()
            },
            coding_tools(
                exa_api_key,
                crate::memory::MemoryLimits {
                    enabled: memory_enabled,
                    writable: true,
                    index_budget_chars: memory_index_budget_chars,
                    entry_budget_chars: memory_entry_budget_chars,
                    max_entries: memory_max_entries,
                },
            ),
            context,
        );
        harness
            .run_interactive(&mut model, initial, rx, Some(factory), stream)
            .await
            .map_err(|e| e.to_string())
    });

    SessionHandle { input_tx, join, state_path }
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
                    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                    if !name.is_empty() {
                        read_session(&p, &root, &name, &mut out);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    out
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

/// The label for the session list: the user-set title override if present, else
/// the first request truncated.
fn effective_title(state: &HarnessState) -> String {
    state
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(120).collect())
        .unwrap_or_else(|| state.user_request.chars().take(120).collect())
}

fn meta_from_state(state: &HarnessState) -> SessionMeta {
    SessionMeta {
        folder: state.workspace.clone(),
        title: effective_title(state),
        status: format!("{:?}", state.status).to_lowercase(),
    }
}

/// Set (or clear, if empty) a saved session's title override and rewrite its
/// sidecar. For sessions that aren't currently live — the daemon routes live ones
/// through the loop so its in-memory state stays in sync.
pub fn set_session_title(state_path: &Path, title: &str) -> Result<(), String> {
    let bytes = std::fs::read(state_path).map_err(|e| e.to_string())?;
    let mut state = deserialize_state(&bytes)?;
    let t = title.trim();
    state.title = if t.is_empty() { None } else { Some(t.to_string()) };
    let out = crate::harness::serialize_state(&state)?;
    std::fs::write(state_path, out).map_err(|e| e.to_string())?;
    write_session_meta(state_path, &state);
    Ok(())
}

/// Write the metadata sidecar for a state file (best-effort). Called on every
/// persist so the sidecar tracks the latest title/folder/status.
pub fn write_session_meta(state_path: &Path, state: &HarnessState) {
    if let Ok(s) = serde_json::to_string(&meta_from_state(state)) {
        let _ = std::fs::write(meta_path(state_path), s);
    }
}

/// Remove a session's state file and its metadata sidecar.
pub fn remove_session_files(state_path: &Path) {
    let _ = std::fs::remove_file(state_path);
    let _ = std::fs::remove_file(meta_path(state_path));
}

fn read_session(path: &Path, root: &Path, conversation: &str, out: &mut Vec<SessionInfo>) {
    let last_active = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let id = path.strip_prefix(root).unwrap_or(path).display().to_string();

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
        status: format!("{:?}", state.status).to_lowercase(),
        last_active,
    });
}
