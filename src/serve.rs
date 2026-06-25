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
use crate::session::{list_device_sessions, start_session, state_path_for_id};

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
        let cfg = {
            let c = self.config.lock().unwrap();
            c.for_workspace(folder)
        };
        let handle = start_session(&cfg, sp.clone(), None, true, None);
        let tx = handle.input_tx.clone();
        sessions.insert(
            id.to_string(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp.clone(),
                profile: None,
            },
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
    config_path: PathBuf,
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
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/fs", get(browse_fs))
        .route("/attach", get(attach_ws))
        .route("/config", get(get_config))
        .route("/config/profile", put(put_profile).delete(delete_profile))
        .route("/config/active", post(set_active))
        .route("/session/model", post(set_session_model))
        .route("/session/rewind", post(rewind_session))
        .with_state(daemon);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
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
            let live_s = live.get(&s.id);
            let running = live_s.is_some();
            let profile = live_s.and_then(|l| l.profile.clone());
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

#[derive(Deserialize)]
struct OpenReq {
    folder: String,
    #[serde(default = "default_true")]
    resume: bool,
    /// Optional profile to build this session's model from (else the global active).
    #[serde(default)]
    profile: Option<String>,
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
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder.clone());
        if let Some(name) = req.profile.as_ref() {
            if let Some(m) = w.setups.as_ref().and_then(|s| s.get(name)).cloned() {
                w.model = m;
                w.active_setup = Some(name.clone());
            }
        }
        w
    };
    let sp = cfg.state_path.clone();
    let id = sp.strip_prefix(workspaces_root()).unwrap_or(&sp).display().to_string();

    let mut sessions = d.sessions.lock().await;
    if !sessions.contains_key(&id) {
        let handle = start_session(&cfg, sp.clone(), None, req.resume, None);
        sessions.insert(
            id.clone(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp.clone(),
                profile: req.profile.clone(),
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
        let existing_key = c.setups.as_ref().and_then(|m| m.get(&name)).map(|m| m.api_key.clone());
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
    port: u16,
    token: &str,
    no_tunnel: bool,
    public_url: Option<String>,
    tunnel_token: Option<String>,
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
    if let Some(t) = &tunnel_token {
        cmd.arg("--tunnel-token").arg(t);
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
