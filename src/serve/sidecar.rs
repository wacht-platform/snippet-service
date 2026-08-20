//! Local sidecar client — the TUI talks to a running `snippet serve` daemon
//! over its **local** HTTP/WS API so the daemon is the sole owner of every
//! `run_interactive` loop and every `state.json` write.
//!
//! Discovery:
//!   `~/.snippet/serve.json` → `{ api_url, token, … }`
//!   Always connect to `api_url` (e.g. `http://127.0.0.1:8787`), never the
//!   public tunnel URL. The tunnel is for remote clients only.
//!
//! Protocol (same as mobile):
//!   GET  /health
//!   POST /sessions              {folder, resume?, new_conversation?}
//!   WS   /attach?session=&token=  → state snapshot/delta + live stream frames
//!                                 ← LoopInput JSON
//!
//! Wire frames from the daemon:
//!   `{ "wire": "snapshot", …HarnessState }`
//!   `{ "wire": "delta", "new_events": […], "event_count": N, …scalars }`
//!   `{ "wire": "stream", "text": "…", "thinking": "…", "text_visible": bool }`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::config::workspaces_root;
use crate::harness::{HarnessEvent, HarnessState, LoopInput};
use crate::llm::{StreamBuffer, StreamHandle};

/// Parsed from `~/.snippet/serve.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonInfo {
    /// Public/tunnel URL (mobile / status display only).
    #[serde(default)]
    pub url: String,
    /// Auth token.
    pub token: String,
    /// Daemon PID (informational).
    #[serde(default)]
    pub pid: u32,
    /// Local API base, e.g. `http://127.0.0.1:8787`. Prefer this over `url`.
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

fn default_api_url() -> String {
    "http://127.0.0.1:8787".to_string()
}

fn serve_json_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".snippet/serve.json")
}

/// Read `~/.snippet/serve.json` if present and parseable.
pub fn read_daemon_info() -> Option<DaemonInfo> {
    let bytes = std::fs::read(serve_json_path()).ok()?;
    let info: DaemonInfo = serde_json::from_slice(&bytes).ok()?;
    if info.token.trim().is_empty() {
        return None;
    }
    Some(info)
}

/// True when the daemon answers `/health` on its local API URL.
pub async fn probe_daemon(info: &DaemonInfo) -> bool {
    let url = format!("{}/health", info.api_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(800))
        .build()
    else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Discover a live local daemon: read serve.json, probe `/health`.
pub async fn discover() -> Option<DaemonInfo> {
    let info = read_daemon_info()?;
    if probe_daemon(&info).await {
        Some(info)
    } else {
        None
    }
}

/// Discover a live daemon, or enable/start the OS service and wait until `/health` answers.
/// The TUI has no in-process agent path — this is the only way sessions run.
pub async fn discover_or_start(config_path: &Path) -> Result<DaemonInfo, String> {
    discover_or_start_with_progress(config_path, |_| {}).await
}

/// Like [`discover_or_start`], but reports short human phase labels so a TUI can
/// paint a connecting screen instead of hanging on a blank frame.
pub async fn discover_or_start_with_progress(
    config_path: &Path,
    mut on_phase: impl FnMut(&str),
) -> Result<DaemonInfo, String> {
    on_phase("Looking for local serve…");
    if let Some(info) = discover().await {
        on_phase("Connected");
        return Ok(info);
    }
    on_phase("Starting snippet serve…");
    // Blocking systemd/launchctl work off the async runtime.
    let config_path = config_path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::serve::ensure_service(&config_path))
        .await
        .map_err(|e| format!("ensure serve task: {e}"))?
        .map_err(|e| format!("enable/start snippet serve: {e}"))?;

    // Service starts asynchronously — poll until health is up.
    for i in 0..40 {
        on_phase(if i < 4 {
            "Waiting for serve health…"
        } else {
            "Still starting serve…"
        });
        if let Some(info) = discover().await {
            on_phase("Connected");
            return Ok(info);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err(
        "snippet serve did not become healthy after enable/start (check `snippet serve --status`)"
            .to_string(),
    )
}

/// Map a state file path to the daemon session id
/// (`path` relative to `workspaces_root()`).
pub fn state_path_to_session_id(state_path: &Path) -> String {
    let root = workspaces_root();
    state_path
        .strip_prefix(&root)
        .unwrap_or(state_path)
        .display()
        .to_string()
}

/// One TUI↔daemon session attachment.
pub struct SidecarAttach {
    /// Latest full `HarnessState` from snapshot/delta frames.
    pub state_rx: watch::Receiver<Option<HarnessState>>,
    /// Send `LoopInput` to the daemon (same shape mobile uses).
    pub input_tx: mpsc::UnboundedSender<LoopInput>,
    /// Raw JSON frames for the session PTY (`wire: term`).
    pub term_tx: mpsc::UnboundedSender<String>,
    pub term_rx: mpsc::UnboundedReceiver<serde_json::Value>,
    /// Live token stream — written by stream frames, read by the TUI renderer.
    pub stream: StreamHandle,
    /// True while the WS tasks are still running.
    connected: Arc<std::sync::atomic::AtomicBool>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SidecarAttach {
    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn send(&self, input: LoopInput) -> Result<(), String> {
        self.input_tx
            .send(input)
            .map_err(|_| "sidecar session disconnected".to_string())
    }

    pub fn send_term(&self, frame: serde_json::Value) -> Result<(), String> {
        self.term_tx
            .send(frame.to_string())
            .map_err(|_| "sidecar session disconnected".to_string())
    }
}

/// Attach to a live daemon session (starts/resumes it via `/attach` if needed).
pub async fn attach(info: &DaemonInfo, state_path: &Path) -> Result<SidecarAttach, String> {
    let session_id = state_path_to_session_id(state_path);
    let ws_url = ws_attach_url(info, &session_id)?;

    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("connect {ws_url}: {e}"))?;
    let (mut sink, mut source) = ws.split();

    let (state_tx, state_rx) = watch::channel::<Option<HarnessState>>(None);
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<LoopInput>();
    let (term_out_tx, mut term_out_rx) = mpsc::unbounded_channel::<String>();
    let (term_in_tx, term_in_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let stream: StreamHandle = Arc::new(Mutex::new(StreamBuffer::default()));
    let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut tasks = Vec::new();

    let connected_out = connected.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            tokio::select! {
                input = input_rx.recv() => {
                    let Some(input) = input else { break; };
                    let Ok(json) = serde_json::to_string(&input) else { continue; };
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                frame = term_out_rx.recv() => {
                    let Some(json) = frame else { break; };
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        connected_out.store(false, std::sync::atomic::Ordering::Relaxed);
    }));

    let stream_in = stream.clone();
    let connected_in = connected.clone();
    tasks.push(tokio::spawn(async move {
        let mut accumulated: Option<HarnessState> = None;
        while let Some(Ok(msg)) = source.next().await {
            let WsMessage::Text(text) = msg else {
                if matches!(msg, WsMessage::Close(_)) {
                    break;
                }
                continue;
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if v.get("wire").and_then(|w| w.as_str()) == Some("term") {
                    let _ = term_in_tx.send(v);
                    continue;
                }
            }
            apply_wire_frame(&text, &mut accumulated, &state_tx, &stream_in);
        }
        connected_in.store(false, std::sync::atomic::Ordering::Relaxed);
    }));

    Ok(SidecarAttach {
        state_rx,
        input_tx,
        term_tx: term_out_tx,
        term_rx: term_in_rx,
        stream,
        connected,
        _tasks: tasks,
    })
}

/// Ensure the daemon has a live session for this workspace folder.
/// Returns the daemon session id.
pub async fn open_session(
    info: &DaemonInfo,
    folder: &Path,
    resume: bool,
    new_conversation: bool,
) -> Result<String, String> {
    let url = format!("{}/sessions", info.api_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .query(&[("token", info.token.as_str())])
        .json(&serde_json::json!({
            "folder": folder.display().to_string(),
            "resume": resume,
            "new_conversation": new_conversation,
        }))
        .send()
        .await
        .map_err(|e| format!("open session: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("open session {status}: {body}"));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("open session body: {e}"))?;
    body.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("open session: missing id in {body}"))
}

/// POST /session/model — switch the model profile for one live conversation.
pub async fn set_session_model(
    info: &DaemonInfo,
    session_id: &str,
    profile: &str,
) -> Result<(), String> {
    let url = format!("{}/session/model", info.api_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .query(&[("token", info.token.as_str())])
        .json(&serde_json::json!({
            "session": session_id,
            "profile": profile,
        }))
        .send()
        .await
        .map_err(|e| format!("model switch: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("model switch {status}: {body}"));
    }
    Ok(())
}

/// POST /session/rewind — restore workspace files + truncate conversation history.
pub async fn rewind_session(
    info: &DaemonInfo,
    session_id: &str,
    checkpoint: &str,
) -> Result<(), String> {
    let url = format!("{}/session/rewind", info.api_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .query(&[("token", info.token.as_str())])
        .json(&serde_json::json!({
            "session": session_id,
            "checkpoint": checkpoint,
        }))
        .send()
        .await
        .map_err(|e| format!("rewind: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("rewind {status}: {body}"));
    }
    Ok(())
}

fn ws_attach_url(info: &DaemonInfo, session_id: &str) -> Result<String, String> {
    let base = info.api_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(format!("bad api_url: {base}"));
    };
    Ok(format!(
        "{ws_base}/attach?session={}&token={}",
        urlencoding::encode(session_id),
        urlencoding::encode(&info.token),
    ))
}

fn apply_wire_frame(
    text: &str,
    accumulated: &mut Option<HarnessState>,
    state_tx: &watch::Sender<Option<HarnessState>>,
    stream: &StreamHandle,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let wire = v.get("wire").and_then(|w| w.as_str()).unwrap_or("");
    match wire {
        "stream" => {
            let text = v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let thinking = v
                .get("thinking")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let visible = v
                .get("text_visible")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            if let Ok(mut buf) = stream.try_lock() {
                buf.text = text;
                buf.thinking = thinking;
                buf.text_visible = visible;
            }
        }
        "snapshot" => {
            // Full state; clear live stream (committed into events).
            StreamBuffer::clear(stream);
            if let Ok(state) = serde_json::from_value::<HarnessState>(v) {
                *accumulated = Some(state.clone());
                let _ = state_tx.send(Some(state));
            }
        }
        "delta" => {
            StreamBuffer::clear(stream);
            let new_events: Vec<HarnessEvent> = v
                .get("new_events")
                .cloned()
                .and_then(|e| serde_json::from_value(e).ok())
                .unwrap_or_default();
            // Merge scalars from the delta envelope onto the accumulated state.
            if let Some(acc) = accumulated.as_mut() {
                if let Ok(partial) = serde_json::from_value::<HarnessState>(v.clone()) {
                    // Keep event log; splice tail; take latest scalars from partial
                    // by re-serializing through a value merge is messy — copy fields
                    // the UI cares about from `partial` (which has empty events).
                    acc.status = partial.status;
                    acc.final_text = partial.final_text;
                    acc.approval_mode = partial.approval_mode;
                    acc.pending_question = partial.pending_question;
                    acc.total_tokens = partial.total_tokens;
                    acc.prompt_tokens = partial.prompt_tokens;
                    acc.completion_tokens = partial.completion_tokens;
                    acc.cache_read_tokens = partial.cache_read_tokens;
                    acc.last_prompt_tokens = partial.last_prompt_tokens;
                    acc.context_window = partial.context_window;
                    acc.rate_limit = partial.rate_limit;
                    // Only replace checkpoints when the delta explicitly includes them.
                    // Serde default would otherwise wipe a post-rewind list with [].
                    if v.get("checkpoints").is_some() {
                        acc.checkpoints = partial.checkpoints;
                    }
                    acc.goal = partial.goal;
                    acc.lanes = partial.lanes;
                    acc.watches = partial.watches;
                    acc.compacting = partial.compacting;
                    acc.turn_started_at = partial.turn_started_at;
                    acc.compacting_started_at = partial.compacting_started_at;
                    if let Some(t) = partial.title {
                        acc.title = Some(t);
                    }
                }
                acc.events.extend(new_events);
                let _ = state_tx.send(Some(acc.clone()));
            } else if let Ok(mut state) = serde_json::from_value::<HarnessState>(v) {
                // First frame was a delta (rare) — treat events as the full log.
                if state.events.is_empty() {
                    state.events = new_events;
                }
                *accumulated = Some(state.clone());
                let _ = state_tx.send(Some(state));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{HarnessEvent, HarnessStatus};

    #[test]
    fn wire_snapshot_without_messages_updates_state() {
        // Daemon strips `messages` from attach frames (clients render events).
        // Deserializing must still succeed so the TUI/mobile get live updates.
        let (state_tx, state_rx) = watch::channel::<Option<HarnessState>>(None);
        let stream: StreamHandle = Arc::new(Mutex::new(StreamBuffer::default()));
        let mut accumulated = None;
        let frame = serde_json::json!({
            "wire": "snapshot",
            "version": 1,
            "status": "running",
            "created_at": "t0",
            "updated_at": "t1",
            "events": [
                {"kind": "user_input", "text": "hello from user"}
            ],
            "iterations": 1
        });
        apply_wire_frame(&frame.to_string(), &mut accumulated, &state_tx, &stream);
        let state = state_rx
            .borrow()
            .clone()
            .expect("snapshot should publish state");
        assert_eq!(state.status, HarnessStatus::Running);
        assert_eq!(state.events.len(), 1);
        assert!(matches!(
            &state.events[0],
            HarnessEvent::UserInput { text } if text == "hello from user"
        ));
        assert!(state.messages.is_empty());
    }

    #[test]
    fn wire_delta_appends_events_without_messages_field() {
        let (state_tx, state_rx) = watch::channel::<Option<HarnessState>>(None);
        let stream: StreamHandle = Arc::new(Mutex::new(StreamBuffer::default()));
        let mut accumulated = None;
        let snap = serde_json::json!({
            "wire": "snapshot",
            "version": 1,
            "status": "idle",
            "created_at": "t0",
            "updated_at": "t0",
            "events": [],
            "iterations": 0
        });
        apply_wire_frame(&snap.to_string(), &mut accumulated, &state_tx, &stream);
        let delta = serde_json::json!({
            "wire": "delta",
            "version": 1,
            "status": "running",
            "created_at": "t0",
            "updated_at": "t1",
            "new_events": [
                {"kind": "user_input", "text": "next turn"},
                {"kind": "assistant_text", "text": "ok"}
            ],
            "event_count": 2,
            "iterations": 1
        });
        apply_wire_frame(&delta.to_string(), &mut accumulated, &state_tx, &stream);
        let state = state_rx
            .borrow()
            .clone()
            .expect("delta should publish state");
        assert_eq!(state.status, HarnessStatus::Running);
        assert_eq!(state.events.len(), 2);
        assert!(matches!(&state.events[0], HarnessEvent::UserInput { .. }));
        assert!(matches!(
            &state.events[1],
            HarnessEvent::AssistantText { .. }
        ));
    }

    #[test]
    fn session_id_strips_workspaces_root() {
        let root = workspaces_root();
        let path = root.join("proj/conversations/abc.json");
        assert_eq!(
            state_path_to_session_id(&path),
            "proj/conversations/abc.json"
        );
    }
}
