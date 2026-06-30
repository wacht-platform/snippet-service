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
use axum::routing::{get, post, put};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::config::{ModelConfig, SnippetConfig, save_config, workspaces_root};
use crate::harness::{LoopInput, deserialize_state};
use crate::session::{
    list_device_sessions, read_session_profile, start_session, state_path_for_id,
    write_session_profile,
};

struct LiveSession {
    input_tx: UnboundedSender<LoopInput>,
    join: JoinHandle<Result<crate::harness::HarnessState, String>>,
    state_path: PathBuf,
    /// The profile this session's model was built from (per-conversation override,
    /// in-memory only — reverts to the global active profile on daemon restart).
    profile: Option<String>,
}

struct Daemon {
    config: std::sync::Mutex<SnippetConfig>,
    config_path: PathBuf,
    token: String,
    hostname: String,
    sessions: Mutex<HashMap<String, LiveSession>>,
    /// Serializes git WRITE operations daemon-wide so a user's git action can't
    /// race the agent's edits (or another git write) on the same index.
    git_write: Mutex<()>,
}

/// The machine's hostname, used as the app's default instance name.
fn machine_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "snippet".to_string())
}

type Shared = Arc<Daemon>;

/// Constant-time token check: hash both sides to a fixed 32-byte digest and compare
/// without short-circuiting, so neither token length nor content leaks via timing.
fn token_matches(provided: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(provided.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl Daemon {
    fn authed(&self, token: &Option<String>) -> bool {
        token.as_deref().is_some_and(|t| token_matches(t, &self.token))
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
        // Re-apply any persisted per-conversation model override; else the default.
        let profile = read_session_profile(&sp);
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            if let Some(name) = profile.as_ref() {
                if let Some(m) = w.setups.as_ref().and_then(|s| s.get(name)).cloned() {
                    w.model = m;
                    w.active_setup = Some(name.clone());
                }
            }
            w
        };
        let handle = start_session(&cfg, sp.clone(), None, true, None);
        let tx = handle.input_tx.clone();
        sessions.insert(
            id.to_string(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp.clone(),
                profile,
            },
        );
        Some((tx, sp))
    }

    /// Send a loop input to a session. If its loop has ended (interrupt / failure),
    /// a new message revives it as the next turn — so the app can resume like the
    /// TUI does. Non-message inputs to a dead loop are dropped.
    async fn deliver(&self, id: &str, input: LoopInput) {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(id) {
            if !s.join.is_finished() {
                let _ = s.input_tx.send(input);
                return;
            }
        }
        let text = match input {
            LoopInput::UserMessage(t) | LoopInput::Answer(t) => t,
            _ => return,
        };
        let (sp, profile) = match sessions.get(id) {
            Some(s) => (s.state_path.clone(), s.profile.clone()),
            None => match state_path_for_id(id) {
                Some(sp) => {
                    let p = read_session_profile(&sp);
                    (sp, p)
                }
                None => return,
            },
        };
        let Ok(bytes) = std::fs::read(&sp) else {
            return;
        };
        let Ok(state) = deserialize_state(&bytes) else {
            return;
        };
        let folder = PathBuf::from(&state.workspace);
        if state.workspace.is_empty() || !folder.is_dir() {
            return;
        }
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            if let Some(name) = profile.as_ref() {
                if let Some(m) = w.setups.as_ref().and_then(|s| s.get(name)).cloned() {
                    w.model = m;
                    w.active_setup = Some(name.clone());
                }
            }
            w
        };
        let handle = start_session(&cfg, sp.clone(), Some(text), true, None);
        sessions.insert(
            id.to_string(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp,
                profile,
            },
        );
    }
}

/// How the daemon is reached from outside the box.
pub enum Tunnel {
    /// Auto-launch a cloudflared quick tunnel (random public HTTPS URL, no account).
    /// The only tunnel serve manages itself.
    Cloudflared,
    /// Bring-your-own: just advertise this public URL (you run your own tunnel —
    /// e.g. a named cloudflared run as its own service — pointed at the local port).
    Url(String),
    /// Local only (no public URL).
    None,
}

/// Map the serve CLI's tunnel flags to a `Tunnel`. Shared by the daemonizing worker
/// and the supervised (service-manager) path. serve only ever runs the default quick
/// tunnel; a stable URL means binding locally and running your own tunnel.
pub fn resolve_tunnel(no_tunnel: bool, public_url: Option<String>) -> Tunnel {
    if no_tunnel {
        Tunnel::None
    } else if let Some(u) = public_url {
        Tunnel::Url(u)
    } else {
        Tunnel::Cloudflared
    }
}

/// Run the daemon's HTTP/WS server on `127.0.0.1:port`, bring up the tunnel, and
/// print a scannable QR + connection string. The token is the app-layer auth gate.
pub async fn run_serve(
    config: SnippetConfig,
    config_path: PathBuf,
    host: &str,
    port: u16,
    token: String,
    tunnel: Tunnel,
) -> Result<(), String> {
    let token_for_print = token.clone();
    let mut config = config;
    config.ensure_setups();
    let daemon: Shared = Arc::new(Daemon {
        config: std::sync::Mutex::new(config),
        config_path,
        token,
        hostname: machine_hostname(),
        sessions: Mutex::new(HashMap::new()),
        git_write: Mutex::new(()),
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/sessions/counts", get(session_counts))
        .route("/fs", get(browse_fs))
        .route("/fs/file", get(read_fs_file))
        .route("/fs/upload", post(upload_fs_file))
        .route("/fs/write", post(write_fs_file))
        .route("/fs/mkdir", post(make_fs_dir))
        .route("/fs/delete", post(delete_fs_path))
        .route("/fs/download", get(download_fs_file))
        .route("/attach", get(attach_ws))
        .route("/events", get(events_ws))
        .route("/config", get(get_config))
        .route("/config/profile", put(put_profile).delete(delete_profile))
        .route("/config/active", post(set_active))
        .route("/session/model", post(set_session_model))
        .route("/session/rewind", post(rewind_session))
        .route("/session/exec", post(exec_in_session))
        .route("/session/delete", post(delete_session))
        .route("/session/rename", post(rename_session))
        .route("/git/status", post(git_status))
        .route("/git/diff", post(git_diff))
        .route("/git/log", post(git_log))
        .route("/git/branches", post(git_branches))
        .route("/git/stage", post(git_stage))
        .route("/git/unstage", post(git_unstage))
        .route("/git/commit", post(git_commit))
        .route("/git/checkout", post(git_checkout))
        .route("/git/push", post(git_push))
        .route("/git/pull", post(git_pull))
        .route("/git/stash", post(git_stash))
        .with_state(daemon);
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("invalid bind address {host}:{port}: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

    // Serve in the background so we can bring up the tunnel and print the QR.
    let mut server = tokio::spawn(async move { axum::serve(listener, app).await });

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
    write_serve_state(&public_url, &token_for_print);

    // Run until the listener dies or we get SIGTERM/SIGINT (`serve --stop`); either
    // way tear down the tunnel so cloudflared doesn't linger, and clear our pidfile.
    let result = tokio::select! {
        joined = &mut server => match joined {
            Ok(inner) => inner.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        },
        _ = shutdown_signal() => Ok(()),
    };
    server.abort();
    if let Some(mut child) = tunnel_child {
        let _ = child.start_kill();
    }
    let _ = std::fs::remove_file(state_json_path());
    let _ = std::fs::remove_file(pid_path());
    result
}

/// Launch `cloudflared tunnel --url` and capture the printed `*.trycloudflare.com`
/// URL. cloudflared's output goes to a log FILE (not a parent pipe): if we piped it
/// and stopped reading, the pipe would fill and cloudflared would die on SIGPIPE,
/// killing the tunnel. The returned child must be kept alive for the tunnel to serve.
async fn start_cloudflared_quick(
    bin: &std::path::Path,
    port: u16,
) -> Result<(String, tokio::process::Child), String> {
    let _ = std::fs::create_dir_all(snippet_dir());
    let log = snippet_dir().join("cloudflared.log");
    let _ = std::fs::remove_file(&log);
    let out = std::fs::File::create(&log).map_err(|e| format!("cloudflared log: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(bin)
        .args(["tunnel", "--no-autoupdate", "--url", &format!("http://localhost:{port}")])
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("launch cloudflared: {e}"))?;

    // ~30s: poll the log file for the assigned URL while cloudflared keeps running.
    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&log) {
            if let Some(url) = extract_trycloudflare_url(&content) {
                return Ok((url, child));
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = child.start_kill();
    Err("timed out waiting for the cloudflared URL".to_string())
}

/// Pull the first `https://*.trycloudflare.com` URL out of cloudflared's log output.
fn extract_trycloudflare_url(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(i) = line.find("https://") {
            let url: String =
                line[i..].chars().take_while(|c| !c.is_whitespace()).collect();
            if url.contains("trycloudflare.com") {
                return Some(url.trim_end_matches(['.', ',']).to_string());
            }
        }
    }
    None
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

/// Fetch cloudflared in the foreground (with a progress bar) before detaching, so
/// the one-time download is visible. The detached child then finds it cached and
/// returns instantly. Runs on a current-thread runtime so no worker threads exist
/// across the subsequent fork.
pub fn ensure_cloudflared_foreground() -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(ensure_cloudflared())?;
    Ok(())
}

/// Redraw the single-line download progress bar in place (driven by chunk arrival).
fn draw_download_progress(spinner: char, done: u64, total: Option<u64>) {
    use std::io::Write;
    let mb = |b: u64| b as f64 / 1_000_000.0;
    let line = match total.filter(|t| *t > 0) {
        Some(t) => {
            let frac = (done as f64 / t as f64).min(1.0);
            let width = 24usize;
            let filled = (frac * width as f64).round() as usize;
            let bar: String = (0..width)
                .map(|i| if i < filled { '█' } else { '░' })
                .collect();
            format!(
                "\r  {spinner} cloudflared  {:>5.1} / {:>5.1} MB  [{bar}] {:>3.0}%",
                mb(done),
                mb(t),
                frac * 100.0
            )
        }
        None => format!("\r  {spinner} cloudflared  {:.1} MB", mb(done)),
    };
    print!("{line}");
    let _ = std::io::stdout().flush();
}

/// Fetch the official cloudflared static binary into `dest` (one-time, ~35 MB).
async fn download_cloudflared(dest: &std::path::Path) -> Result<(), String> {
    use futures_util::StreamExt;

    let asset = cloudflared_asset()?;
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    println!("  Fetching cloudflared (one-time) for {}…", std::env::consts::OS);

    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download cloudflared: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download cloudflared: {e}"))?;
    let total = resp.content_length();
    let mut stream = resp.bytes_stream();
    let mut bytes: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut tick = 0usize;
    draw_download_progress(FRAMES[0], 0, total);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download cloudflared: {e}"))?;
        bytes.extend_from_slice(&chunk);
        tick = tick.wrapping_add(1);
        draw_download_progress(FRAMES[tick % FRAMES.len()], bytes.len() as u64, total);
    }
    println!("\r\x1b[2K  ✓ cloudflared downloaded ({:.1} MB)", bytes.len() as f64 / 1_000_000.0);

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
    let connection = connection_string(public_url, token);
    println!("\n  Scan the QR in the snippet app, or paste this connection string:\n");
    if let Ok(code) = qrcode::QrCode::new(connection.as_bytes()) {
        let rendered = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{rendered}");
    }
    println!("  {connection}\n");
}

/// The single connection string the app pastes/scans: the public URL carrying the
/// auth token as a query param (e.g. https://host/?token=abc).
fn connection_string(public_url: &str, token: &str) -> String {
    let sep = if public_url.contains('?') { '&' } else { '?' };
    format!("{public_url}{sep}token={token}")
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[derive(Deserialize)]
struct Auth {
    token: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    /// Optional: only sessions whose workspace is exactly this folder.
    #[serde(default)]
    folder: Option<String>,
    /// Optional: cap to the N most-recent (the list is sorted last-active first).
    #[serde(default)]
    limit: Option<usize>,
}

// GET /sessions[?folder=] — device sessions (optionally scoped to one folder),
// each with a `running` flag. Metadata comes from per-session sidecars, so this
// no longer decompresses every conversation.
async fn list_sessions(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let mut sessions = list_device_sessions();
    if let Some(folder) = q.folder.as_deref().filter(|f| !f.is_empty()) {
        sessions.retain(|s| s.folder == folder);
    }
    if let Some(n) = q.limit {
        sessions.truncate(n);
    }
    let live = d.sessions.lock().await;
    let out: Vec<serde_json::Value> = sessions
        .into_iter()
        .map(|s| {
            let live_s = live.get(&s.id);
            let running = s.status == "running";
            // Live override if running, else the persisted sidecar (or none → default).
            let profile = live_s
                .and_then(|l| l.profile.clone())
                .or_else(|| state_path_for_id(&s.id).and_then(|p| read_session_profile(&p)));
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("running".into(), serde_json::json!(running));
                obj.insert("profile".into(), serde_json::json!(profile));
            }
            v
        })
        .collect();
    Json(out).into_response()
}

// GET /sessions/counts — {folder: count} across all sessions (cheap, from
// sidecars), for the app's per-folder session badges without downloading the list.
async fn session_counts(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for s in list_device_sessions() {
        if !s.folder.is_empty() {
            *counts.entry(s.folder).or_insert(0) += 1;
        }
    }
    Json(counts).into_response()
}

#[derive(Deserialize)]
struct OpenReq {
    folder: String,
    #[serde(default = "default_true")]
    resume: bool,
    /// Optional profile to build this session's model from (else the global active).
    #[serde(default)]
    profile: Option<String>,
    /// Start a brand-new conversation in the folder (a fresh `conversations/<uuid>.json`)
    /// instead of opening the folder's default session. Lets a folder hold many
    /// conversations, like the TUI — the existing ones are left untouched.
    #[serde(default)]
    new_conversation: bool,
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
    // Resolve the state path first (needs the workspace's base config).
    let base_state = {
        let c = d.config.lock().unwrap();
        c.for_workspace(folder.clone()).state_path
    };
    // Default → the folder's single `state.json`. New conversation → a fresh
    // `conversations/<uuid>.json` (started blank), so the folder's existing
    // session(s) are preserved and a new one appears in the list.
    let (sp, resume) = if req.new_conversation {
        let name = uuid::Uuid::new_v4().to_string();
        let path = base_state
            .parent()
            .map(|p| p.join("conversations").join(format!("{name}.json")))
            .unwrap_or_else(|| base_state.clone());
        (path, false)
    } else {
        (base_state.clone(), req.resume)
    };
    let id = sp.strip_prefix(workspaces_root()).unwrap_or(&sp).display().to_string();
    // Effective model: an explicit request wins; otherwise reuse any persisted
    // per-conversation override (when resuming); otherwise the global default.
    let profile = req.profile.clone().or_else(|| read_session_profile(&sp));
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder);
        if let Some(name) = profile.as_ref() {
            if let Some(m) = w.setups.as_ref().and_then(|s| s.get(name)).cloned() {
                w.model = m;
                w.active_setup = Some(name.clone());
            }
        }
        w
    };

    let mut sessions = d.sessions.lock().await;
    if !sessions.contains_key(&id) {
        let handle = start_session(&cfg, sp.clone(), None, resume, None);
        if let Some(name) = req.profile.as_ref() {
            write_session_profile(&sp, name); // persist an explicit override
        }
        sessions.insert(
            id.clone(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp.clone(),
                profile,
            },
        );
    }
    Json(serde_json::json!({ "id": id, "folder": req.folder })).into_response()
}

// ---- model configuration (mirrors the TUI's profiles, shared config.toml) ----

#[derive(Serialize)]
struct ProfileView {
    name: String,
    provider: String,
    base_url: String,
    model: String,
    has_key: bool,
    active: bool,
    context_window: u64,
}

#[derive(Serialize)]
struct ConfigView {
    profiles: Vec<ProfileView>,
    active: Option<String>,
    theme: Option<String>,
    manual_approval: bool,
    hostname: String,
}

// GET /config — profiles with keys redacted (has_key only), active profile, theme.
async fn get_config(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let c = d.config.lock().unwrap();
    let active = c.active_setup.clone();
    let mut profiles = Vec::new();
    if let Some(setups) = c.setups.as_ref() {
        for (name, m) in setups {
            profiles.push(ProfileView {
                name: name.clone(),
                provider: m.provider.clone(),
                base_url: m.base_url.clone(),
                model: m.model.clone(),
                has_key: !m.api_key.trim().is_empty(),
                active: active.as_deref() == Some(name.as_str()),
                context_window: m.context_window,
            });
        }
    }
    Json(ConfigView {
        profiles,
        active,
        theme: c.theme.clone(),
        manual_approval: c.manual_approval,
        hostname: d.hostname.clone(),
    })
    .into_response()
}

#[derive(Deserialize)]
struct ProfileReq {
    name: Option<String>,
    provider: String,
    #[serde(default)]
    base_url: Option<String>,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    supports_images: Option<bool>,
    /// Model context window in tokens (drives the usage gauge + compaction point).
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    set_active: bool,
}

// PUT /config/profile — add/update an API-key provider profile; persists to disk.
// An omitted/blank api_key keeps any existing key (so editing doesn't wipe it).
async fn put_profile(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ProfileReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.provider.trim().is_empty() || req.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "provider and model are required").into_response();
    }
    let result = {
        let mut c = d.config.lock().unwrap();
        let name = req
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| c.unique_profile_key(&req.provider));
        let existing = c.setups.as_ref().and_then(|m| m.get(&name));
        let existing_key = existing.map(|m| m.api_key.clone());
        let existing_ctx = existing.map(|m| m.context_window);
        let base_url = req
            .base_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| ModelConfig::default().base_url);
        let api_key = req
            .api_key
            .clone()
            .filter(|s| !s.is_empty())
            .or(existing_key)
            .unwrap_or_default();
        let mc = ModelConfig {
            provider: req.provider.clone(),
            base_url,
            model: req.model.clone(),
            api_key,
            reasoning_effort: req.reasoning_effort.clone().filter(|s| !s.is_empty()),
            supports_images: req.supports_images.unwrap_or(false),
            // Explicit value wins; else keep the profile's current one; else default.
            context_window: req
                .context_window
                .filter(|&n| n > 0)
                .or(existing_ctx)
                .unwrap_or_else(|| ModelConfig::default().context_window),
            ..ModelConfig::default()
        };
        c.upsert_profile(&name, mc);
        if req.set_active {
            c.activate(&name);
        }
        save_config(&c, &d.config_path).map(|_| name)
    };
    match result {
        Ok(name) => Json(serde_json::json!({ "name": name })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct ActiveReq {
    name: String,
}

// POST /config/active — set the global active profile (default for new sessions).
async fn set_active(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ActiveReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let result = {
        let mut c = d.config.lock().unwrap();
        if !c.activate(&req.name) {
            return (StatusCode::NOT_FOUND, "no such profile").into_response();
        }
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => Json(serde_json::json!({ "active": req.name })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct DeleteProfileQuery {
    token: Option<String>,
    name: String,
}

// DELETE /config/profile?name= — remove a profile (active falls back to first left).
async fn delete_profile(State(d): State<Shared>, Query(q): Query<DeleteProfileQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let result = {
        let mut c = d.config.lock().unwrap();
        c.remove_profile(&q.name);
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => Json(serde_json::json!({ "removed": q.name })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct SessionModelReq {
    session: String,
    profile: String,
}

// POST /session/model {session, profile} — switch one conversation's model until
// daemon restart: rebuild its loop on the chosen profile, resuming from disk.
async fn set_session_model(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<SessionModelReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let model_cfg = {
        let c = d.config.lock().unwrap();
        match c.setups.as_ref().and_then(|m| m.get(&req.profile)).cloned() {
            Some(m) => m,
            None => return (StatusCode::NOT_FOUND, "no such profile").into_response(),
        }
    };
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let Ok(bytes) = std::fs::read(&sp) else {
        return (StatusCode::NOT_FOUND, "session state unreadable").into_response();
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "bad session state").into_response();
    };
    let folder = PathBuf::from(&state.workspace);
    if state.workspace.is_empty() || !folder.is_dir() {
        return (StatusCode::BAD_REQUEST, "session workspace missing").into_response();
    }
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder);
        w.model = model_cfg;
        w.active_setup = Some(req.profile.clone());
        w
    };
    let mut sessions = d.sessions.lock().await;
    if let Some(old) = sessions.remove(&req.session) {
        old.join.abort();
    }
    let handle = start_session(&cfg, sp.clone(), None, true, None);
    write_session_profile(&sp, &req.profile); // persist so it survives restart
    sessions.insert(
        req.session.clone(),
        LiveSession {
            input_tx: handle.input_tx,
            join: handle.join,
            state_path: sp,
            profile: Some(req.profile.clone()),
        },
    );
    Json(serde_json::json!({ "session": req.session, "profile": req.profile })).into_response()
}

#[derive(Deserialize)]
struct RewindReq {
    session: String,
    checkpoint: String,
}

// POST /session/rewind {session, checkpoint} — restore the workspace files to a
// checkpoint (the conversation continues; only the work-tree reverts), mirroring
// the TUI's /rewind.
async fn rewind_session(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<RewindReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let Ok(bytes) = std::fs::read(&sp) else {
        return (StatusCode::NOT_FOUND, "session state unreadable").into_response();
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "bad session state").into_response();
    };
    if state.workspace.is_empty() {
        return (StatusCode::BAD_REQUEST, "session workspace missing").into_response();
    }
    let workspace = PathBuf::from(&state.workspace);
    let checkpoint = req.checkpoint.clone();
    let result =
        tokio::task::spawn_blocking(move || crate::checkpoint::restore(&workspace, &checkpoint)).await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({ "restored": req.checkpoint })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ExecReq {
    session: String,
    command: String,
}

// POST /session/exec {session, command} — run a shell command in the session's
// workspace and return its output. Token-gated; runs as the daemon user.
async fn exec_in_session(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ExecReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let Ok(bytes) = std::fs::read(&sp) else {
        return (StatusCode::NOT_FOUND, "session state unreadable").into_response();
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "bad session state").into_response();
    };
    let dir = PathBuf::from(&state.workspace);
    if state.workspace.is_empty() || !dir.is_dir() {
        return (StatusCode::BAD_REQUEST, "session workspace missing").into_response();
    }
    if req.command.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty command").into_response();
    }
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&req.command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => {
            return Json(serde_json::json!({
                "exit_code": -1, "stdout": "", "stderr": "timed out after 60s", "truncated": false,
            }))
            .into_response()
        }
    };
    fn clip(b: &[u8]) -> (String, bool) {
        const MAX: usize = 20_000;
        let s = String::from_utf8_lossy(b);
        if s.chars().count() > MAX {
            (s.chars().take(MAX).collect::<String>() + "\u{2026}", true)
        } else {
            (s.into_owned(), false)
        }
    }
    let (stdout, t1) = clip(&out.stdout);
    let (stderr, t2) = clip(&out.stderr);
    Json(serde_json::json!({
        "exit_code": out.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": t1 || t2,
    }))
    .into_response()
}

// ---- git operations (server-side, shells out to the system `git`) -----------
// Shelling the real `git` is the only approach with full feature parity (cred
// helpers, SSH config, every edge case). Args are passed as a VECTOR (never via
// `sh -c`), so user-supplied paths/messages/refs can't inject shell. Write ops
// take the daemon's git_write lock so a user action can't race the agent's edits.

/// Resolve a session id to its on-disk workspace folder, or an error response.
fn resolve_session_dir(session: &str) -> Result<PathBuf, Response> {
    let Some(sp) = state_path_for_id(session) else {
        return Err((StatusCode::NOT_FOUND, "no such session").into_response());
    };
    let Ok(bytes) = std::fs::read(&sp) else {
        return Err((StatusCode::NOT_FOUND, "session state unreadable").into_response());
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "bad session state").into_response());
    };
    let dir = PathBuf::from(&state.workspace);
    if state.workspace.is_empty() || !dir.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "session workspace missing").into_response());
    }
    Ok(dir)
}

fn clip_git(b: &[u8]) -> (String, bool) {
    const MAX: usize = 100_000;
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > MAX {
        (s.chars().take(MAX).collect::<String>() + "\u{2026}", true)
    } else {
        (s.into_owned(), false)
    }
}

/// Run `git -C <dir> <args...>` (no shell) with a timeout. Returns
/// (exit_code, stdout, stderr, truncated).
async fn run_git<I, S>(dir: &std::path::Path, args: I) -> Result<(i32, String, String, bool), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let fut = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(Duration::from_secs(120), fut).await {
        Ok(Ok(o)) => {
            let (so, t1) = clip_git(&o.stdout);
            let (se, t2) = clip_git(&o.stderr);
            Ok((o.status.code().unwrap_or(-1), so, se, t1 || t2))
        }
        Ok(Err(e)) => Err(format!("failed to run git (is it installed?): {e}")),
        Err(_) => Err("git timed out after 120s".to_string()),
    }
}

/// Standard JSON for a write op: ok flag + raw streams so the app can show errors.
fn git_result(code: i32, stdout: String, stderr: String, truncated: bool) -> Response {
    Json(serde_json::json!({
        "ok": code == 0,
        "exit_code": code,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": truncated,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct GitReq {
    session: String,
}

// POST /git/status {session} — branch, upstream, ahead/behind, and changed files.
async fn git_status(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // -uall lists every untracked file individually (not a collapsed `dir/`), so
    // the app can show what's inside a new folder.
    match run_git(&dir, ["status", "--porcelain=v1", "-b", "-z", "-uall"]).await {
        Ok((0, stdout, _, _)) => Json(parse_status(&stdout)).into_response(),
        Ok((_, _, stderr, _)) => Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn parse_branch_header(rest: &str) -> (String, Option<String>, i64, i64) {
    if let Some(b) = rest.strip_prefix("No commits yet on ") {
        return (b.trim().to_string(), None, 0, 0);
    }
    let (left, bracket) = match rest.split_once(" [") {
        Some((l, b)) => (l, Some(b.trim_end_matches(']'))),
        None => (rest, None),
    };
    let (branch, upstream) = match left.split_once("...") {
        Some((b, u)) => (b.to_string(), Some(u.to_string())),
        None => (left.to_string(), None),
    };
    let (mut ahead, mut behind) = (0i64, 0i64);
    if let Some(b) = bracket {
        for part in b.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
    (branch, upstream, ahead, behind)
}

/// Parse `git status --porcelain=v1 -b -z` into a structured snapshot.
fn parse_status(z: &str) -> serde_json::Value {
    let parts: Vec<&str> = z.split('\0').collect();
    let (mut branch, mut upstream, mut ahead, mut behind) = (String::new(), None, 0i64, 0i64);
    let mut files = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let tok = parts[i];
        if tok.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("## ") {
            let (b, u, a2, be) = parse_branch_header(rest);
            branch = b;
            upstream = u;
            ahead = a2;
            behind = be;
        } else if tok.len() >= 3 {
            let bytes = tok.as_bytes();
            let x = bytes[0] as char;
            let y = bytes[1] as char;
            let path = &tok[3..];
            // Rename/copy entries carry the original path in the NEXT NUL field.
            let mut orig: Option<String> = None;
            if x == 'R' || x == 'C' {
                if let Some(o) = parts.get(i + 1) {
                    orig = Some((*o).to_string());
                    i += 1;
                }
            }
            files.push(serde_json::json!({
                "path": path,
                "orig": orig,
                "x": x.to_string(),
                "y": y.to_string(),
                "staged": x != ' ' && x != '?',
                "unstaged": y != ' ' && y != '?',
                "untracked": x == '?',
            }));
        }
        i += 1;
    }
    serde_json::json!({
        "ok": true,
        "branch": branch,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
        "clean": files.is_empty(),
        "files": files,
    })
}

#[derive(Deserialize)]
struct GitDiffReq {
    session: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    staged: bool,
    /// Untracked (new) file: show its whole content as an add-diff via
    /// `git diff --no-index` (plain `git diff` shows nothing for untracked files).
    #[serde(default)]
    untracked: bool,
}

// POST /git/diff {session, file?, staged?, untracked?} — unified diff (clipped).
async fn git_diff(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitDiffReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let file = req.file.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let args: Vec<String> = if req.untracked {
        let Some(f) = file else {
            return (StatusCode::BAD_REQUEST, "untracked diff needs a file").into_response();
        };
        // /dev/null → file shows the entire file as additions.
        vec!["diff".into(), "--no-index".into(), "--".into(), "/dev/null".into(), f.to_string()]
    } else {
        let mut a = vec!["diff".into()];
        if req.staged {
            a.push("--staged".into());
        }
        if let Some(f) = file {
            a.push("--".into());
            a.push(f.to_string());
        }
        a
    };
    match run_git(&dir, &args).await {
        Ok((code, stdout, stderr, truncated)) => Json(serde_json::json!({
            // `git diff --no-index` exits 1 when files differ — that's success here.
            "ok": code == 0 || (req.untracked && code == 1),
            "patch": stdout,
            "stderr": stderr,
            "truncated": truncated,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitLogReq {
    session: String,
    #[serde(default)]
    limit: Option<u32>,
}

// POST /git/log {session, limit?} — recent commits as structured records.
async fn git_log(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitLogReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let n = req.limit.unwrap_or(50).clamp(1, 500);
    // Unit-separator (\x1f) between fields, record-separator (\x1e) between commits.
    let fmt = "--pretty=format:%H\x1f%h\x1f%an\x1f%ad\x1f%s\x1e";
    match run_git(&dir, ["log", &format!("-n{n}"), "--date=short", fmt]).await {
        Ok((0, stdout, _, _)) => {
            let commits: Vec<serde_json::Value> = stdout
                .split('\x1e')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .filter_map(|rec| {
                    let f: Vec<&str> = rec.split('\x1f').collect();
                    (f.len() == 5).then(|| serde_json::json!({
                        "hash": f[0], "short": f[1], "author": f[2], "date": f[3], "subject": f[4],
                    }))
                })
                .collect();
            Json(serde_json::json!({"ok": true, "commits": commits})).into_response()
        }
        Ok((_, _, stderr, _)) => Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/branches {session} — local branches + which is current.
async fn git_branches(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match run_git(&dir, ["branch", "--format=%(HEAD)\x1f%(refname:short)"]).await {
        Ok((0, stdout, _, _)) => {
            let mut current = String::new();
            let branches: Vec<String> = stdout
                .lines()
                .filter_map(|l| l.split_once('\x1f'))
                .map(|(head, name)| {
                    if head == "*" {
                        current = name.to_string();
                    }
                    name.to_string()
                })
                .collect();
            Json(serde_json::json!({"ok": true, "current": current, "branches": branches})).into_response()
        }
        Ok((_, _, stderr, _)) => Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitStageReq {
    session: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    all: bool,
}

// POST /git/stage {session, paths?, all?} — `git add`.
async fn git_stage(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitStageReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["add".into()];
    if req.all {
        args.push("-A".into());
    } else if !req.paths.is_empty() {
        args.push("--".into());
        args.extend(req.paths.iter().cloned());
    } else {
        return (StatusCode::BAD_REQUEST, "no paths (pass paths[] or all:true)").into_response();
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitUnstageReq {
    session: String,
    #[serde(default)]
    paths: Vec<String>,
}

// POST /git/unstage {session, paths?} — `git restore --staged` (all if none given).
async fn git_unstage(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitUnstageReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["restore".into(), "--staged".into(), "--".into()];
    if req.paths.is_empty() {
        args.push(".".into());
    } else {
        args.extend(req.paths.iter().cloned());
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitCommitReq {
    session: String,
    message: String,
    #[serde(default)]
    amend: bool,
}

// POST /git/commit {session, message, amend?} — commit the staged index.
async fn git_commit(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitCommitReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.message.trim().is_empty() && !req.amend {
        return (StatusCode::BAD_REQUEST, "empty commit message").into_response();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["commit".into(), "-m".into(), req.message.clone()];
    if req.amend {
        args.push("--amend".into());
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitCheckoutReq {
    session: String,
    target: String,
    #[serde(default)]
    create: bool,
}

// POST /git/checkout {session, target, create?} — switch (or create) a branch.
async fn git_checkout(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitCheckoutReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.target.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty target").into_response();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["checkout".into()];
    if req.create {
        args.push("-b".into());
    }
    args.push(req.target.clone());
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/push {session} — push the current branch (uses the box's git creds).
async fn git_push(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["push"]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/pull {session} — fast-forward-only pull (surfaces non-ff for the user).
async fn git_pull(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["pull", "--ff-only"]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct GitStashReq {
    session: String,
    #[serde(default)]
    op: Option<String>,
}

// POST /git/stash {session, op?} — op = push (default) | pop | list | drop.
async fn git_stash(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<GitStashReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let sub = match req.op.as_deref().unwrap_or("push") {
        "pop" => "pop",
        "list" => "list",
        "drop" => "drop",
        "push" | "save" | "" => "push",
        other => return (StatusCode::BAD_REQUEST, format!("unknown stash op `{other}`")).into_response(),
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["stash", sub]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Resolve the auth token: an explicit one wins; else reuse the persisted token so
/// restarts keep the same token (only the tunnel URL changes); else generate + save.
pub fn resolve_token(explicit: Option<String>) -> String {
    if let Some(t) = explicit.filter(|s| !s.trim().is_empty()) {
        return t;
    }
    let path = snippet_dir().join("serve.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 16 {
            return t;
        }
    }
    let t = uuid::Uuid::new_v4().simple().to_string();
    let _ = std::fs::create_dir_all(snippet_dir());
    if std::fs::write(&path, &t).is_ok() {
        crate::config::set_private(&path);
    }
    t
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

#[derive(Serialize)]
struct FileContent {
    path: String,
    content: String,
    size: u64,
    truncated: bool,
    binary: bool,
    /// Fingerprint of the full on-disk bytes — the editor sends it back on save
    /// for optimistic-concurrency conflict detection.
    hash: String,
}

/// Fast non-cryptographic fingerprint of file bytes (same scheme used elsewhere).
fn hash_bytes(b: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Deserialize)]
struct FsWriteReq {
    path: String,
    content: String,
    /// If set, the write is refused when the file on disk no longer matches it
    /// (someone — the agent or another editor — changed it since it was opened).
    #[serde(default)]
    prev_hash: Option<String>,
}

#[derive(Deserialize)]
struct FsMkdirReq {
    /// Absolute path of the new directory to create.
    path: String,
}

#[derive(Deserialize)]
struct FsDeleteReq {
    /// Absolute path of the file or directory to delete (dirs are removed recursively).
    path: String,
}

// POST /fs/write {path, content, prev_hash?} — atomic write (temp + rename) with
// optimistic-concurrency conflict detection. Token-gated; UTF-8 text only.
async fn write_fs_file(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsWriteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    if let Some(prev) = req.prev_hash.as_deref() {
        match std::fs::read(&path) {
            Ok(cur) if hash_bytes(&cur) != prev => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "ok": false,
                        "conflict": true,
                        "error": "file changed on disk since you opened it — reload before saving",
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"ok": false, "conflict": true, "error": "file no longer exists"})),
                )
                    .into_response();
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&parent) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create dir: {e}")).into_response();
    }
    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{fname}.snippet.tmp"));
    let bytes = req.content.into_bytes();
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("write temp: {e}")).into_response();
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")).into_response();
    }
    Json(serde_json::json!({"ok": true, "hash": hash_bytes(&bytes), "size": bytes.len()})).into_response()
}

// POST /fs/mkdir {path} — create a new directory (token-gated).
async fn make_fs_dir(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsMkdirReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "error": "a file or folder with that name already exists"})),
        )
            .into_response();
    }
    if let Err(e) = std::fs::create_dir_all(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create dir: {e}")).into_response();
    }
    Json(serde_json::json!({"ok": true, "path": path.to_string_lossy()})).into_response()
}

// POST /fs/delete {path} — remove a file or directory (recursive). Token-gated.
async fn delete_fs_path(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsDeleteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Json(serde_json::json!({"ok": true, "already_gone": true})).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let res = if meta.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match res {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")).into_response(),
    }
}

// GET /fs/download?path= — stream a file's raw bytes (any type) for the app to
// save to the device. Token-gated.
const MAX_FS_DOWNLOAD_BYTES: u64 = 50 * 1024 * 1024;
async fn download_fs_file(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if meta.is_dir() {
        return (StatusCode::BAD_REQUEST, "path is a directory").into_response();
    }
    if meta.len() > MAX_FS_DOWNLOAD_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "file too large to download").into_response();
    }
    match std::fs::read(&path) {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Cap a single file read for the mobile viewer.
const MAX_FS_FILE_BYTES: usize = 512 * 1024;

// GET /fs/file?path= — read one text file's contents (for the in-app file viewer).
async fn read_fs_file(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    };
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => {
            let size = m.len();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            };
            let truncated = bytes.len() > MAX_FS_FILE_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_FS_FILE_BYTES)];
            // Binary if it has a NUL or isn't valid UTF-8 up to the cut.
            let binary = slice.contains(&0) || std::str::from_utf8(slice).is_err();
            let content = if binary {
                String::new()
            } else {
                String::from_utf8_lossy(slice).to_string()
            };
            Json(FileContent { path: path.display().to_string(), content, size, truncated, binary, hash: hash_bytes(&bytes) }).into_response()
        }
        Ok(_) => (StatusCode::BAD_REQUEST, "not a file").into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct DeleteReq {
    session: String,
}

// POST /session/delete {session} — stop the live loop (if any) and delete the
// session's conversation file.
async fn delete_session(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<DeleteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    {
        let mut sessions = d.sessions.lock().await;
        if let Some(s) = sessions.remove(&req.session) {
            s.join.abort();
        }
    }
    crate::session::remove_session_files(&sp);
    Json(serde_json::json!({"deleted": true})).into_response()
}

#[derive(Deserialize)]
struct RenameReq {
    session: String,
    title: String,
}

// POST /session/rename {session, title} — set the session's title override. A live
// session goes through its loop so the in-memory state stays in sync; otherwise the
// state file is edited directly (without reviving the loop).
async fn rename_session(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<RenameReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session) {
            if !s.join.is_finished() {
                let _ = s.input_tx.send(LoopInput::SetTitle(req.title.clone()));
                return Json(serde_json::json!({"renamed": true})).into_response();
            }
        }
    }
    match crate::session::set_session_title(&sp, &req.title) {
        Ok(()) => Json(serde_json::json!({"renamed": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct UploadReq {
    data_base64: String,
    #[serde(default)]
    name: Option<String>,
    /// When set, save into this directory under the original [name] (instead of
    /// a temp dir with a generated name) — used by the file-explorer upload.
    #[serde(default)]
    dir: Option<String>,
}

// POST /fs/upload {data_base64, name?} — save an uploaded file (e.g. an image the
// user posts from the app) to a temp dir and return its absolute path, which the
// agent can then view with `read_image` (or read).
async fn upload_fs_file(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<UploadReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data_base64.trim()) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad base64: {e}")).into_response(),
    };
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty upload").into_response();
    }
    // Targeted upload into a directory under the original filename, or (default)
    // a temp dir with a generated name (for chat attachments).
    let path = if let Some(dir) = req.dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let name = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).file_name())
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return (StatusCode::BAD_REQUEST, "name required when uploading to a directory").into_response();
        };
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok": false, "error": "a file with that name already exists"})),
            )
                .into_response();
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        p
    } else {
        let ext = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).extension())
            .and_then(|e| e.to_str())
            .filter(|e| e.len() <= 5)
            .unwrap_or("png");
        let dir = std::env::temp_dir().join("snippet-uploads");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        dir.join(format!("{}.{ext}", uuid::Uuid::new_v4().simple()))
    };
    if let Err(e) = std::fs::write(&path, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(serde_json::json!({"path": path.display().to_string(), "size": bytes.len()})).into_response()
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
        Some((_, state_path)) => {
            let daemon = d.clone();
            let session = q.session.clone();
            ws.on_upgrade(move |socket| handle_ws(socket, daemon, session, state_path))
        }
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn handle_ws(socket: WebSocket, daemon: Shared, session: String, state_path: PathBuf) {
    let (mut sender, mut receiver) = socket.split();

    // Push the HarnessState whenever it changes on disk (mtime poll, like the TUI).
    let push = tokio::spawn(async move {
        let mut last_mtime = None;
        let mut last_count: Option<usize> = None;
        let mut last_head: u64 = 0;
        loop {
            if let Ok(meta) = tokio::fs::metadata(&state_path).await {
                if let Ok(mtime) = meta.modified() {
                    if Some(mtime) != last_mtime {
                        last_mtime = Some(mtime);
                        if let Ok(bytes) = tokio::fs::read(&state_path).await {
                            if let Ok(state) = deserialize_state(&bytes) {
                                if let Ok(mut v) = serde_json::to_value(&state) {
                                    // `messages` (raw LLM history) is unused by the app — never wire it.
                                    if let Some(o) = v.as_object_mut() {
                                        o.remove("messages");
                                    }
                                    let count = state.events.len();
                                    let head = events_head_fp(&state);
                                    // Full snapshot on connect / compaction (head changed) / shrink;
                                    // otherwise stream only the appended tail.
                                    let snapshot = match last_count {
                                        None => true,
                                        Some(lc) => head != last_head || count < lc,
                                    };
                                    if let Some(o) = v.as_object_mut() {
                                        if snapshot {
                                            o.insert("wire".into(), serde_json::json!("snapshot"));
                                        } else {
                                            let start = last_count.unwrap_or(0);
                                            let tail = serde_json::to_value(&state.events[start..])
                                                .unwrap_or_default();
                                            o.remove("events");
                                            o.insert("wire".into(), serde_json::json!("delta"));
                                            o.insert("new_events".into(), tail);
                                            o.insert("event_count".into(), serde_json::json!(count));
                                        }
                                    }
                                    last_count = Some(count);
                                    last_head = head;
                                    if let Ok(json) = serde_json::to_string(&v) {
                                        if sender.send(Message::Text(json.into())).await.is_err() {
                                            break;
                                        }
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
                    daemon.deliver(&session, input).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

/// Fingerprint of the event log's head — detects compaction (history replaced)
/// vs a plain append. Stable across appends; changes when events[0] changes.
fn events_head_fp(state: &crate::harness::HarnessState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(first) = state.events.first() {
        if let Ok(s) = serde_json::to_string(first) {
            s.hash(&mut h);
        }
    }
    h.finish()
}

// WS /events — device-wide notification firehose. Emits a compact event whenever a
// session leaves the running state (asked a question / needs approval / stopped /
// errored), so the app can notify even for sessions it isn't actively watching.
async fn events_ws(ws: WebSocketUpgrade, State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    ws.on_upgrade(handle_events_ws)
}

async fn handle_events_ws(socket: WebSocket) {
    use std::collections::{HashMap, HashSet};
    let (mut sender, mut receiver) = socket.split();
    let push = tokio::spawn(async move {
        let mut last: HashMap<String, String> = HashMap::new();
        let mut first = true;
        loop {
            let sessions = list_device_sessions();
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for s in &sessions {
                seen.insert(s.id.clone());
                let prev = last.insert(s.id.clone(), s.status.clone());
                if first {
                    continue;
                }
                let prevs = prev.as_deref().unwrap_or("");
                if prevs == s.status {
                    continue;
                }
                // Notify on a change into any attention state, regardless of the prior
                // state (a session can go idle->running->waiting within one poll).
                let kind = match s.status.as_str() {
                    "waiting_for_input" => "waiting", // asked a question / needs approval
                    "failed" => "error",
                    "completed" => "done",
                    "idle" if prevs == "running" => "idle", // a turn just finished
                    _ => continue, // running / interrupted / newly-seen idle
                };
                out.push(serde_json::json!({
                    "session": s.id,
                    "title": s.title,
                    "workspace": s.folder,
                    "kind": kind,
                    "status": s.status,
                }));
            }
            last.retain(|k, _| seen.contains(k));
            first = false;
            for e in out {
                if sender.send(Message::Text(e.to_string().into())).await.is_err() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    });
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Close(_) = msg {
            break;
        }
    }
    push.abort();
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

// --- background daemon lifecycle ---

fn snippet_dir() -> PathBuf {
    home_dir().join(".snippet")
}
fn pid_path() -> PathBuf {
    snippet_dir().join("serve.pid")
}
fn log_path() -> PathBuf {
    snippet_dir().join("serve.log")
}
fn state_json_path() -> PathBuf {
    snippet_dir().join("serve.json")
}

/// Persist the live connection (url + token) so the launching parent and
/// `serve --status` can reprint the QR. Written 0600 — it holds the auth token.
fn write_serve_state(public_url: &str, token: &str) {
    let _ = std::fs::create_dir_all(snippet_dir());
    let payload = serde_json::json!({
        "url": public_url,
        "token": token,
        "pid": std::process::id(),
    });
    let path = state_json_path();
    if std::fs::write(&path, payload.to_string()).is_ok() {
        crate::config::set_private(&path);
    }
}

/// Fully detach the current process into the background (double-fork + setsid via
/// the `daemonize` crate, output to the log). Run by the spawned worker before it
/// builds the runtime and serves.
pub fn daemonize_self() -> Result<(), String> {
    std::fs::create_dir_all(snippet_dir()).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_path()).map_err(|e| format!("open log: {e}"))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    daemonize::Daemonize::new()
        .pid_file(pid_path())
        .working_directory(home_dir())
        .stdout(daemonize::Stdio::from(log))
        .stderr(daemonize::Stdio::from(log2))
        .start()
        .map_err(|e| format!("daemonize: {e}"))
}

/// Foreground launcher (what `snippet serve` runs): spawn the detached worker, wait
/// for it to publish the connection, print the QR here, then exit. This is why the
/// QR shows on the terminal even though the server runs in the background.
pub fn launch_and_show(
    host: &str,
    port: u16,
    token: &str,
    no_tunnel: bool,
    public_url: Option<String>,
    config_path: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    if let Some(pid) = running_pid() {
        return Err(format!(
            "snippet serve is already running (pid {pid}). `snippet serve --stop` first."
        ));
    }
    std::fs::create_dir_all(snippet_dir()).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(state_json_path());

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config")
        .arg(config_path)
        .arg("serve")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .arg("--token")
        .arg(token);
    if no_tunnel {
        cmd.arg("--no-tunnel");
    }
    if let Some(u) = &public_url {
        cmd.arg("--public-url").arg(u);
    }
    // The worker re-enters `serve` with this marker set and runs the server; we (the
    // launcher) stay alive to print the QR. The worker redirects output to the log.
    cmd.env("__SNIPPET_SERVE_WORKER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    cmd.spawn().map_err(|e| format!("spawn worker: {e}"))?;

    println!("\n  snippet serve — bringing up the tunnel…");
    for _ in 0..120 {
        if let Some((url, tok)) = read_serve_state() {
            print_connection(&url, &tok);
            println!("  Running in the background.  stop: snippet serve --stop\n");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!(
        "the server didn't come up within 60s — check the log: {}",
        log_path().display()
    ))
}

/// Resolves on SIGTERM/SIGINT; pends forever if the handlers can't be installed
/// (so the server arm of the select still wins).
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut intr) = match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
        (Ok(t), Ok(i)) => (t, i),
        _ => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {},
        _ = intr.recv() => {},
    }
}

fn read_serve_state() -> Option<(String, String)> {
    let bytes = std::fs::read(state_json_path()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some((v["url"].as_str()?.to_string(), v["token"].as_str()?.to_string()))
}

/// The running daemon's pid, if the pidfile points at a live process.
fn running_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path()).ok()?.trim().parse().ok()?;
    // signal 0 = liveness probe
    let alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    alive.then_some(pid)
}

/// Stop the background daemon (kills its whole process group → tunnel too).
pub fn stop() -> Result<(), String> {
    let Some(pid) = running_pid() else {
        let _ = std::fs::remove_file(pid_path());
        return Err("snippet serve is not running".to_string());
    };
    // SIGTERM the daemon; its handler tears down the tunnel before exiting.
    let _ = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(state_json_path());
    println!("stopped snippet serve (pid {pid})");
    Ok(())
}

/// Print the current daemon status + reprint the QR/connection if it's running.
pub fn status() -> Result<(), String> {
    match (running_pid(), read_serve_state()) {
        (Some(pid), Some((url, token))) => {
            println!("snippet serve running (pid {pid})");
            print_connection(&url, &token);
            Ok(())
        }
        (Some(pid), None) => {
            println!("snippet serve running (pid {pid}) — connection not published yet");
            Ok(())
        }
        (None, _) => {
            println!("snippet serve is not running");
            Ok(())
        }
    }
}

// --- auto-start service (launchd / systemd) ---

/// Supervised mode doesn't daemonize, so record our pid here so `serve --status`
/// can find us (the service manager owns actual lifecycle/restart).
pub fn write_own_pidfile() {
    let _ = std::fs::create_dir_all(snippet_dir());
    let _ = std::fs::write(pid_path(), std::process::id().to_string());
}

/// Persisted serve runtime config (`~/.config/snippet/serve.toml`). The auto-start
/// service reads this on every boot rather than having the settings frozen into
/// its plist/unit — so re-running `--enable` (or hand-editing this one file)
/// changes how the daemon comes up without touching the service definition.
/// Absent/empty file → the daemon starts with CLI defaults.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ServeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(default)]
    pub no_tunnel: bool,
}

/// XDG-style config dir, shared by the CLI and the auto-start service so both
/// agree on one location regardless of working directory. `$XDG_CONFIG_HOME` wins
/// (Linux/systemd convention); otherwise `~/.config/snippet` (also sensible on
/// macOS). This holds user-editable config; secrets/runtime stay in ~/.snippet.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("snippet")
}

fn serve_settings_path() -> PathBuf {
    config_dir().join("serve.toml")
}

impl ServeSettings {
    /// Load from disk; an absent or unparseable file yields defaults.
    pub fn load() -> Self {
        std::fs::read_to_string(serve_settings_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist 0600.
    fn save(&self) -> Result<(), String> {
        let _ = std::fs::create_dir_all(config_dir());
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let path = serve_settings_path();
        std::fs::write(&path, body).map_err(|e| format!("write serve.toml: {e}"))?;
        crate::config::set_private(&path);
        Ok(())
    }
}

/// Persist an explicit auth token to `~/.snippet/serve.token` (0600) so the daemon
/// reuses it across restarts without it ever appearing on a command line.
pub fn persist_token(token: &str) {
    let _ = std::fs::create_dir_all(snippet_dir());
    let path = snippet_dir().join("serve.token");
    if std::fs::write(&path, token).is_ok() {
        crate::config::set_private(&path);
    }
}

/// The fixed command the service runs. Runtime settings come from serve.toml,
/// not from these args, so the service definition never has to change.
/// `--config` is a top-level arg, so it precedes the `serve` subcommand.
fn service_args(config_path: &std::path::Path) -> Vec<String> {
    vec![
        "--config".to_string(),
        config_path.display().to_string(),
        "serve".to_string(),
        "--supervised".to_string(),
    ]
}

/// Install snippet serve as an OS service that auto-starts on boot/login. The
/// serve flags are written to serve.toml (read at runtime); the service itself
/// just runs `snippet serve --supervised`. launchd on macOS, systemd `--user` on
/// Linux. Re-running updates serve.toml in place — there is only ever one service.
pub fn install_service(
    host: &str,
    port: u16,
    token: Option<&str>,
    no_tunnel: bool,
    public_url: Option<&str>,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Persist the settings the supervised daemon will read on boot.
    ServeSettings {
        host: Some(host.to_string()),
        port: Some(port),
        public_url: public_url.map(str::to_string),
        no_tunnel,
    }
    .save()?;
    if let Some(t) = token {
        persist_token(t);
    }
    let args = service_args(config_path);
    // Free the port: a manually-started daemon would block the service's bind.
    let _ = stop();

    #[cfg(target_os = "macos")]
    {
        install_launchd(&exe, &args)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd(&exe, &args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (&exe, &args);
        Err("auto-start is only supported on macOS and Linux".to_string())
    }
}

/// Remove the auto-start service installed by `install_service`.
pub fn uninstall_service() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let plist = launch_agent_path();
        if plist.exists() {
            let uid = current_uid();
            if let Some(uid) = &uid {
                let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{}", SERVICE_LABEL)]);
            }
            let _ = run("launchctl", &["unload", "-w", &plist.display().to_string()]);
            std::fs::remove_file(&plist).map_err(|e| format!("remove plist: {e}"))?;
            println!("✓ Removed launchd agent: {}", plist.display());
        } else {
            println!("no launchd agent installed");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let unit = systemd_unit_path();
        let _ = run("systemctl", &["--user", "disable", "--now", "snippet-serve.service"]);
        if unit.exists() {
            std::fs::remove_file(&unit).map_err(|e| format!("remove unit: {e}"))?;
            println!("✓ Removed systemd user unit: {}", unit.display());
        } else {
            println!("no systemd user unit installed");
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err("auto-start is only supported on macOS and Linux".to_string());
    }
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(state_json_path());
    Ok(())
}

/// Run a command to completion, returning whether it exited 0 (errors → false).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll for the published connection (the service starts asynchronously) and print
/// the QR + string once it's up.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_service_connection() {
    for _ in 0..30 {
        if let Some((url, tok)) = read_serve_state() {
            print_connection(&url, &tok);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("  (starting… run `snippet serve --status` for the connection string)");
}

#[cfg(target_os = "macos")]
const SERVICE_LABEL: &str = "com.snippet.serve";

#[cfg(target_os = "macos")]
fn launch_agent_path() -> PathBuf {
    home_dir().join("Library/LaunchAgents/com.snippet.serve.plist")
}

#[cfg(target_os = "macos")]
fn current_uid() -> Option<String> {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install_launchd(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    let plist = launch_agent_path();
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut prog = format!("    <string>{}</string>\n", xml_escape(&exe.display().to_string()));
    for a in args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let home = home_dir().display().to_string();
    // cloudflared lives in ~/.snippet/bin; include common brew/system paths too.
    let path_env = format!("{home}/.snippet/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    let log = log_path().display().to_string();
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>WorkingDirectory</key>
  <string>{home_x}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home_x}</string>
    <key>PATH</key>
    <string>{path_x}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log_x}</string>
  <key>StandardErrorPath</key>
  <string>{log_x}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        prog = prog,
        home_x = xml_escape(&home),
        path_x = xml_escape(&path_env),
        log_x = xml_escape(&log),
    );
    std::fs::write(&plist, content).map_err(|e| format!("write plist: {e}"))?;

    let plist_s = plist.display().to_string();
    let loaded = if let Some(uid) = current_uid() {
        // Modern domain-target API; bootout first so a re-enable is idempotent.
        let target = format!("gui/{uid}/{SERVICE_LABEL}");
        let _ = run("launchctl", &["bootout", &target]);
        let ok = run("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist_s]);
        if ok {
            let _ = run("launchctl", &["enable", &target]);
        }
        ok
    } else {
        false
    };
    // Fall back to the legacy load -w if bootstrap is unavailable.
    if !loaded {
        let _ = run("launchctl", &["unload", &plist_s]);
        if !run("launchctl", &["load", "-w", &plist_s]) {
            return Err("launchctl could not load the agent".to_string());
        }
    }

    println!("✓ Installed launchd agent: {}", plist.display());
    println!("  Auto-starts at login and restarts on crash.  Disable: snippet serve --disable");
    print_service_connection();
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> PathBuf {
    home_dir().join(".config/systemd/user/snippet-serve.service")
}

/// Quote an arg for a systemd `ExecStart` line (which is shell-like word-split).
#[cfg(target_os = "linux")]
fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'='));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(target_os = "linux")]
fn install_systemd(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    let unit = systemd_unit_path();
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut exec = sh_quote(&exe.display().to_string());
    for a in args {
        exec.push(' ');
        exec.push_str(&sh_quote(a));
    }
    let home = home_dir().display().to_string();
    let path_env = format!("{home}/.snippet/bin:/usr/local/bin:/usr/bin:/bin");
    let content = format!(
        r#"[Unit]
Description=snippet serve (remote-control daemon)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec}
WorkingDirectory={home}
Environment=HOME={home}
Environment=PATH={path_env}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#,
    );
    std::fs::write(&unit, content).map_err(|e| format!("write unit: {e}"))?;

    let _ = run("systemctl", &["--user", "daemon-reload"]);
    if !run("systemctl", &["--user", "enable", "--now", "snippet-serve.service"]) {
        return Err("systemctl --user enable failed (is a user systemd session available?)".to_string());
    }
    // Survive logout / start at boot without an active login session.
    if let Ok(user) = std::env::var("USER") {
        let _ = run("loginctl", &["enable-linger", &user]);
    }

    println!("✓ Installed systemd user unit: {}", unit.display());
    println!("  Enabled + started; lingering on so it survives logout.  Disable: snippet serve --disable");
    print_service_connection();
    Ok(())
}
