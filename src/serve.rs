//! Headless control daemon. Runs alongside (never replacing) the TUI: it manages
//! sessions across the device and exposes them over HTTP + WebSocket so a remote
//! client (mobile app) can browse folders, open a session in any folder, list every
//! session on the box, and stream/drive one. Every endpoint is token-authed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::config::{SnippetConfig, workspaces_root};
use crate::harness::{LoopInput, deserialize_state};
use crate::session::{list_device_sessions, start_session, state_path_for_id};

struct LiveSession {
    input_tx: UnboundedSender<LoopInput>,
    join: JoinHandle<Result<crate::harness::HarnessState, String>>,
    state_path: PathBuf,
}

struct Daemon {
    config: SnippetConfig,
    token: String,
    sessions: Mutex<HashMap<String, LiveSession>>,
}

type Shared = Arc<Daemon>;

impl Daemon {
    fn authed(&self, token: &Option<String>) -> bool {
        token.as_deref() == Some(self.token.as_str())
    }

    /// Return a live session's input channel + state path, starting (resuming) it
    /// from disk if it isn't already running.
    async fn ensure_live(&self, id: &str) -> Option<(UnboundedSender<LoopInput>, PathBuf)> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(id) {
            return Some((s.input_tx.clone(), s.state_path.clone()));
        }
        let sp = state_path_for_id(id)?;
        let bytes = std::fs::read(&sp).ok()?;
        let state = deserialize_state(&bytes).ok()?;
        let folder = PathBuf::from(&state.workspace);
        if state.workspace.is_empty() || !folder.is_dir() {
            return None;
        }
        let cfg = self.config.for_workspace(folder);
        let handle = start_session(&cfg, sp.clone(), None, true, None);
        let tx = handle.input_tx.clone();
        sessions.insert(
            id.to_string(),
            LiveSession { input_tx: handle.input_tx, join: handle.join, state_path: sp.clone() },
        );
        Some((tx, sp))
    }
}

/// Run the daemon's HTTP/WS server on `127.0.0.1:port`. (A tunnel exposes it; the
/// token is the app-layer gate.)
pub async fn run_serve(config: SnippetConfig, port: u16, token: String) -> Result<(), String> {
    let daemon: Shared = Arc::new(Daemon {
        config,
        token,
        sessions: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/fs", get(browse_fs))
        .route("/attach", get(attach_ws))
        .with_state(daemon);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[derive(Deserialize)]
struct Auth {
    token: Option<String>,
}

// GET /sessions — every session on the device, with a `running` flag.
async fn list_sessions(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let sessions = list_device_sessions();
    let live = d.sessions.lock().await;
    let out: Vec<serde_json::Value> = sessions
        .into_iter()
        .map(|s| {
            let running = live.contains_key(&s.id);
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("running".into(), serde_json::json!(running));
            }
            v
        })
        .collect();
    Json(out).into_response()
}

#[derive(Deserialize)]
struct OpenReq {
    folder: String,
    #[serde(default = "default_true")]
    resume: bool,
}
fn default_true() -> bool {
    true
}

// POST /sessions {folder, resume?} — open a folder, start/resume its session.
async fn open_session(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<OpenReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let folder = PathBuf::from(&req.folder);
    if !folder.is_dir() {
        return (StatusCode::BAD_REQUEST, "not a directory").into_response();
    }
    let cfg = d.config.for_workspace(folder.clone());
    let sp = cfg.state_path.clone();
    let id = sp.strip_prefix(workspaces_root()).unwrap_or(&sp).display().to_string();

    let mut sessions = d.sessions.lock().await;
    if !sessions.contains_key(&id) {
        let handle = start_session(&cfg, sp.clone(), None, req.resume, None);
        sessions.insert(
            id.clone(),
            LiveSession { input_tx: handle.input_tx, join: handle.join, state_path: sp.clone() },
        );
    }
    Json(serde_json::json!({ "id": id, "folder": req.folder })).into_response()
}

#[derive(Serialize)]
struct FsEntry {
    name: String,
    path: String,
    is_dir: bool,
    git: bool,
}

#[derive(Serialize)]
struct FsListing {
    path: String,
    parent: Option<String>,
    entries: Vec<FsEntry>,
}

#[derive(Deserialize)]
struct FsQuery {
    token: Option<String>,
    path: Option<String>,
}

// GET /fs?path= — one directory level (lazy folder tree). Defaults to $HOME.
async fn browse_fs(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let path = q
        .path
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&path) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // hide dotfiles
            }
            let p = e.path();
            let is_dir = p.is_dir();
            entries.push(FsEntry {
                git: is_dir && p.join(".git").exists(),
                name,
                path: p.display().to_string(),
                is_dir,
            });
        }
    }
    // Directories first, then alphabetical.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Json(FsListing {
        parent: path.parent().map(|p| p.display().to_string()),
        path: path.display().to_string(),
        entries,
    })
    .into_response()
}

#[derive(Deserialize)]
struct AttachQuery {
    token: Option<String>,
    session: String,
}

// WS /attach?session= — stream this session's HarnessState + receive LoopInput.
async fn attach_ws(ws: WebSocketUpgrade, State(d): State<Shared>, Query(q): Query<AttachQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.ensure_live(&q.session).await {
        Some((tx, state_path)) => ws.on_upgrade(move |socket| handle_ws(socket, tx, state_path)),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn handle_ws(socket: WebSocket, tx: UnboundedSender<LoopInput>, state_path: PathBuf) {
    let (mut sender, mut receiver) = socket.split();

    // Push the HarnessState whenever it changes on disk (mtime poll, like the TUI).
    let push = tokio::spawn(async move {
        let mut last_mtime = None;
        loop {
            if let Ok(meta) = tokio::fs::metadata(&state_path).await {
                if let Ok(mtime) = meta.modified() {
                    if Some(mtime) != last_mtime {
                        last_mtime = Some(mtime);
                        if let Ok(bytes) = tokio::fs::read(&state_path).await {
                            if let Ok(state) = deserialize_state(&bytes) {
                                if let Ok(json) = serde_json::to_string(&state) {
                                    if sender.send(Message::Text(json.into())).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    // Inbound: JSON LoopInput → the session's channel.
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(input) = serde_json::from_str::<LoopInput>(t.as_str()) {
                    let _ = tx.send(input);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
