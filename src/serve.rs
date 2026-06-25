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

/// How the daemon is reached from outside the box.
pub enum Tunnel {
    /// Auto-launch a cloudflared quick tunnel (random public HTTPS URL, no account).
    Cloudflared,
    /// A named cloudflared tunnel by token (stable URL); the user supplies the URL.
    Named { token: String, url: String },
    /// Bring-your-own: just advertise this public URL (you run the tunnel).
    Url(String),
    /// Local only (no public URL).
    None,
}

/// Run the daemon's HTTP/WS server on `127.0.0.1:port`, bring up the tunnel, and
/// print a scannable QR + connection string. The token is the app-layer auth gate.
pub async fn run_serve(
    config: SnippetConfig,
    port: u16,
    token: String,
    tunnel: Tunnel,
) -> Result<(), String> {
    let token_for_print = token.clone();
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

    // Serve in the background so we can bring up the tunnel and print the QR.
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // Serve is remote-only: a tunnel is required (on-device, use the TUI). A tunnel
    // failure is fatal — never silently fall back to an unreachable localhost URL.
    // `--no-tunnel` (Tunnel::None) is an explicit local mode for testing only.
    let mut tunnel_child: Option<tokio::process::Child> = None;
    let resolved: Result<String, String> = async {
        match tunnel {
            Tunnel::Url(u) => Ok(u),
            Tunnel::None => Ok(format!("http://127.0.0.1:{port}")),
            Tunnel::Cloudflared => {
                let bin = ensure_cloudflared().await?;
                let (url, child) = start_cloudflared_quick(&bin, port).await?;
                tunnel_child = Some(child);
                Ok(url)
            }
            Tunnel::Named { token: t, url } => {
                let bin = ensure_cloudflared().await?;
                tunnel_child = Some(start_cloudflared_named(&bin, &t).await?);
                Ok(url)
            }
        }
    }
    .await;
    let public_url = match resolved {
        Ok(u) => u,
        Err(e) => {
            server.abort();
            return Err(format!("could not establish the tunnel: {e}"));
        }
    };

    print_connection(&public_url, &token_for_print);

    let result = server.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string());
    if let Some(mut child) = tunnel_child {
        let _ = child.start_kill();
    }
    result
}

/// Launch `cloudflared tunnel --url` and capture the printed `*.trycloudflare.com`
/// URL. The returned child must be kept alive for the tunnel's lifetime.
async fn start_cloudflared_quick(
    bin: &std::path::Path,
    port: u16,
) -> Result<(String, tokio::process::Child), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut child = tokio::process::Command::new(bin)
        .args(["tunnel", "--no-autoupdate", "--url", &format!("http://localhost:{port}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("launch cloudflared (is it installed? `brew install cloudflared`): {e}"))?;
    let stderr = child.stderr.take().ok_or("cloudflared: no stderr")?;
    let mut reader = BufReader::new(stderr).lines();
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(i) = line.find("https://") {
                let url: String = line[i..].chars().take_while(|c| !c.is_whitespace()).collect();
                if url.contains("trycloudflare.com") {
                    return Some(url);
                }
            }
        }
        None
    })
    .await;
    match found {
        Ok(Some(url)) => Ok((url, child)),
        _ => {
            let _ = child.start_kill();
            Err("timed out waiting for the cloudflared URL".to_string())
        }
    }
}

/// Run a pre-created named cloudflared tunnel by its token (stable URL).
async fn start_cloudflared_named(
    bin: &std::path::Path,
    token: &str,
) -> Result<tokio::process::Child, String> {
    tokio::process::Command::new(bin)
        .args(["tunnel", "--no-autoupdate", "run", "--token", token])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("launch cloudflared run: {e}"))
}

/// Locate a usable cloudflared: prefer one on PATH, else a cached copy under
/// `~/.snippet/bin`, else download the official static binary for this OS/arch.
async fn ensure_cloudflared() -> Result<PathBuf, String> {
    if std::process::Command::new("cloudflared")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(PathBuf::from("cloudflared"));
    }
    let bin = bin_dir().join("cloudflared");
    if bin.exists() {
        return Ok(bin);
    }
    download_cloudflared(&bin).await?;
    Ok(bin)
}

fn bin_dir() -> PathBuf {
    home_dir().join(".snippet").join("bin")
}

/// The cloudflared release asset for this platform (verified naming: Linux ships a
/// raw binary, macOS a `.tgz`).
fn cloudflared_asset() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("cloudflared-darwin-arm64.tgz"),
        ("macos", "x86_64") => Ok("cloudflared-darwin-amd64.tgz"),
        ("linux", "x86_64") => Ok("cloudflared-linux-amd64"),
        ("linux", "aarch64") => Ok("cloudflared-linux-arm64"),
        (os, arch) => Err(format!("no cloudflared build for {os}/{arch} — install it manually")),
    }
}

/// Fetch the official cloudflared static binary into `dest` (one-time, ~35 MB).
async fn download_cloudflared(dest: &std::path::Path) -> Result<(), String> {
    let asset = cloudflared_asset()?;
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    eprintln!("downloading cloudflared (one-time) for {}…", std::env::consts::OS);
    let bytes = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download cloudflared: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download cloudflared: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download cloudflared: {e}"))?;
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    if asset.ends_with(".tgz") {
        // macOS: extract the `cloudflared` binary from the tarball via system `tar`.
        let tgz = dir.join("cloudflared.tgz");
        std::fs::write(&tgz, &bytes).map_err(|e| e.to_string())?;
        let ok = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(&tgz)
            .arg("-C")
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&tgz);
        if !ok || !dest.exists() {
            return Err("failed to extract cloudflared from the .tgz".to_string());
        }
    } else {
        std::fs::write(dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Print the QR + connection string the mobile app scans/pastes: a JSON payload
/// `{url, token}` (the app derives wss/https from the URL).
fn print_connection(public_url: &str, token: &str) {
    let payload = serde_json::json!({ "url": public_url, "token": token }).to_string();
    println!("\n  Scan to connect (snippet mobile), or paste the string below:\n");
    if let Ok(code) = qrcode::QrCode::new(payload.as_bytes()) {
        let rendered = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{rendered}");
    }
    println!("  url    : {public_url}");
    println!("  token  : {token}");
    println!("  string : {payload}\n");
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
