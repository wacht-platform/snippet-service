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
use axum::extract::{DefaultBodyLimit, Query, State};
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

mod fs;
mod git;
mod lifecycle;
mod tunnel;

pub use self::lifecycle::*;
pub use self::tunnel::ensure_cloudflared_foreground;

use self::fs::*;
use self::git::*;
use self::tunnel::{ensure_cloudflared, start_cloudflared_quick};

struct LiveSession {
    input_tx: UnboundedSender<LoopInput>,
    join: JoinHandle<Result<crate::harness::HarnessState, String>>,
    state_path: PathBuf,
    /// The profile this session's model was built from (per-conversation override,
    /// in-memory only — reverts to the global active profile on daemon restart).
    profile: Option<String>,
}

/// Apply a named profile's model to a workspace config (no-op if the name isn't a
/// known setup). Shared by the session open / resume / attach paths.
fn apply_profile(cfg: &mut SnippetConfig, profile: &Option<String>) {
    if let Some(name) = profile.as_ref() {
        if let Some(m) = cfg.setups.as_ref().and_then(|s| s.get(name)).cloned() {
            cfg.model = m;
            cfg.active_setup = Some(name.clone());
        }
    }
}

/// Resolve a session id to its (state_path, workspace_dir), reading and validating
/// the persisted state. Returns the error Response to send on any failure.
fn load_session_workspace(session: &str) -> Result<(PathBuf, PathBuf), Response> {
    let Some(sp) = state_path_for_id(session) else {
        return Err((StatusCode::NOT_FOUND, "no such session").into_response());
    };
    let Ok(bytes) = std::fs::read(&sp) else {
        return Err((StatusCode::NOT_FOUND, "session state unreadable").into_response());
    };
    let Ok(state) = deserialize_state(&bytes) else {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "bad session state").into_response());
    };
    let folder = PathBuf::from(&state.workspace);
    if state.workspace.is_empty() || !folder.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "session workspace missing").into_response());
    }
    Ok((sp, folder))
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

    /// Re-read the on-disk config so provider profiles added or removed out-of-band
    /// (from the TUI, or a hand-edit) are reflected here. `config.toml` is the
    /// single source of truth: the TUI and this daemon are independent writers, so
    /// we reload before every config read and before every read-modify-write —
    /// otherwise our stale in-memory copy would hide the TUI's newly-added profiles
    /// and clobber the ones it deleted. Keeps the last good config if a read/parse
    /// transiently fails (never wipes profiles on a bad read).
    async fn reload_config(&self) {
        if let Ok(fresh) = SnippetConfig::load(&self.config_path).await {
            *self.config.lock().unwrap() = fresh;
        }
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
        self.reload_config().await; // pick up profiles added from the TUI
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            apply_profile(&mut w, &profile);
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

    /// The provider actually driving a session: its per-chat profile's provider
    /// when overridden, else the global active model's. Used to scope
    /// provider-specific extras (e.g. the ChatGPT usage overlay) on the wire.
    async fn session_provider(&self, id: &str) -> String {
        let profile = self
            .sessions
            .lock()
            .await
            .get(id)
            .and_then(|s| s.profile.clone());
        let c = self.config.lock().unwrap();
        if let Some(name) = profile {
            if let Some(m) = c.setups.as_ref().and_then(|s| s.get(&name)) {
                return m.provider.clone();
            }
        }
        c.model.provider.clone()
    }

    /// Rebuild a live session's model from the CURRENT config (call after a config
    /// reload). Idle sessions are restarted in place — resume=true reloads their
    /// persisted state, so nothing is lost and the app's socket keeps streaming.
    /// A busy session is left alone (returns Busy) so a running turn isn't cut off.
    async fn rebuild_session_model(&self, id: &str) -> RebuildOutcome {
        let mut sessions = self.sessions.lock().await;
        let Some(existing) = sessions.get(id) else {
            return RebuildOutcome::Gone;
        };
        let sp = existing.state_path.clone();
        let profile = existing.profile.clone();

        // Don't restart mid-turn: only Idle / terminal states are safe.
        let Ok(bytes) = std::fs::read(&sp) else {
            return RebuildOutcome::Gone;
        };
        let Ok(state) = deserialize_state(&bytes) else {
            return RebuildOutcome::Gone;
        };
        use crate::harness::HarnessStatus::*;
        if matches!(state.status, Running | WaitingForInput) {
            return RebuildOutcome::Busy;
        }
        let folder = PathBuf::from(&state.workspace);
        if state.workspace.is_empty() || !folder.is_dir() {
            return RebuildOutcome::Gone;
        }
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            apply_profile(&mut w, &profile);
            w
        };
        // Abort the old loop and respawn with the fresh model config (state resumes).
        if let Some(old) = sessions.remove(id) {
            old.join.abort();
        }
        let handle = start_session(&cfg, sp.clone(), None, true, None);
        sessions.insert(
            id.to_string(),
            LiveSession {
                input_tx: handle.input_tx,
                join: handle.join,
                state_path: sp,
                profile,
            },
        );
        RebuildOutcome::Rebuilt
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
        // The resident loop isn't alive — e.g. after a daemon restart, before this
        // session has been activated this run. Revive it, then deliver the input.
        // A text message starts the loop WITH that message as the first turn; any
        // control input (compact / goal / mode / title) revives the parked loop and
        // is FORWARDED to it. Previously everything but text was dropped here, so a
        // phone-triggered compaction (or /goal, mode/title change) on a not-yet-live
        // session silently did nothing.
        let initial = match &input {
            LoopInput::UserMessage(t) | LoopInput::Answer(t) => Some(t.clone()),
            _ => None,
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
            apply_profile(&mut w, &profile);
            w
        };
        let forward = initial.is_none();
        let handle = start_session(&cfg, sp.clone(), initial, true, None);
        // Control inputs weren't consumed as the first turn — hand them to the
        // freshly-parked loop so it acts on them (idle-arm compaction, goal, etc.).
        if forward {
            let _ = handle.input_tx.send(input);
        }
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
    supervised: bool,
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

    // Background self-update: periodically check for a newer release, replace the
    // binary in place, wait for every session to be between turns (so nothing
    // in-flight is lost), then restart via the service manager to run the new
    // code. Supervised only — a bare daemonized process has no supervisor to
    // bring it back, so there we update the binary and leave applying it to the
    // next manual restart.
    if !crate::update::disabled() {
        let d = daemon.clone();
        tokio::spawn(async move { self_update_loop(d, supervised).await });
    }
    // Watch config.toml: when it changes (a profile edited in the app/TUI, an
    // added model, image support toggled, …) reload it and rebuild the model of
    // every live session so the change takes effect WITHOUT a manual model switch.
    {
        let d = daemon.clone();
        tokio::spawn(async move { config_watch_loop(d).await });
    }
    // The upload endpoint carries the file base64-encoded inside a JSON body, which
    // inflates it by ~4/3. Size the request-body limit so a ~50 MB file still fits
    // once encoded (≈67 MB) plus headroom for the JSON envelope. Every other route
    // keeps axum's small default body limit.
    const MAX_UPLOAD_FILE_BYTES: usize = 50 * 1024 * 1024;
    const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_FILE_BYTES / 3 * 4 + 64 * 1024;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/sessions/counts", get(session_counts))
        .route("/fs", get(browse_fs))
        .route("/fs/file", get(read_fs_file))
        .route(
            "/fs/upload",
            post(upload_fs_file).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/fs/write", post(write_fs_file))
        .route("/fs/mkdir", post(make_fs_dir))
        .route("/fs/delete", post(delete_fs_path))
        .route("/fs/download", get(download_fs_file))
        .route("/attach", get(attach_ws))
        .route("/events", get(events_ws))
        .route("/config", get(get_config))
        .route("/config/profile", put(put_profile).delete(delete_profile))
        .route("/config/active", post(set_active))
        .route("/config/delegate", post(set_delegate))
        .route("/provider/models", post(provider_models))
        .route("/vault", get(vault_list).put(vault_set).delete(vault_delete))
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

    // This stdout is a LOG (the daemonized worker's serve.log / the journal in
    // supervised mode), never a user terminal — the launcher and `--status` print
    // the real QR from serve.json. Keep the token out of it.
    println!("serve up at {public_url} (token elided — `snippet serve --status` shows the connection)");
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

/// Watch the config file; on change, reload it and rebuild every live session's
/// model so edits (image support, model swap, new profile) apply immediately.
/// A running turn is never interrupted — a busy session stays queued and is
/// rebuilt the moment it goes idle (its model is only used at the next turn
/// anyway, so nothing is lost by waiting).
async fn config_watch_loop(daemon: Shared) {
    use std::collections::HashSet;
    use std::time::Duration;

    let path = daemon.config_path.clone();
    let mut last_mtime = tokio::fs::metadata(&path).await.ok().and_then(|m| m.modified().ok());
    // Sessions whose model must be rebuilt but are currently busy — retried until idle.
    let mut pending: HashSet<String> = HashSet::new();

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Detect a config change (mtime).
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if let Ok(mtime) = meta.modified() {
                if Some(mtime) != last_mtime {
                    last_mtime = Some(mtime);
                    daemon.reload_config().await;
                    // Every live session may now have a different model — queue them all.
                    let ids: Vec<String> = daemon.sessions.lock().await.keys().cloned().collect();
                    eprintln!("config.toml changed — reloaded; rebuilding {} live session model(s)", ids.len());
                    pending.extend(ids);
                }
            }
        }

        if pending.is_empty() {
            continue;
        }
        // Apply to sessions that are safe to restart right now (not mid-turn).
        let mut done = Vec::new();
        for id in pending.iter() {
            match daemon.rebuild_session_model(id).await {
                RebuildOutcome::Rebuilt | RebuildOutcome::Gone => done.push(id.clone()),
                RebuildOutcome::Busy => {} // retry next tick
            }
        }
        for id in done {
            pending.remove(&id);
        }
    }
}

/// Result of an attempt to rebuild a live session's model from the current config.
enum RebuildOutcome {
    Rebuilt,
    Busy, // mid-turn — try again once idle
    Gone, // session no longer live; nothing to do
}

/// Periodic self-update loop for the daemon. On a new release: replace the
/// binary, wait for sessions to be idle, then hand off to the service manager.
async fn self_update_loop(daemon: Shared, supervised: bool) {
    use std::time::Duration;
    const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
    let client = reqwest::Client::new();
    // The version already staged on disk THIS run. Without a supervisor the
    // running process keeps its old CARGO_PKG_VERSION, so `is_newer` would stay
    // true and we'd re-download the same release every cycle — this guards it.
    let mut staged: Option<String> = None;
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if crate::update::disabled() {
            continue;
        }
        let Some(latest) = crate::update::latest_version(&client).await else {
            continue;
        };
        if !crate::update::is_newer(&latest) || staged.as_deref() == Some(latest.as_str()) {
            continue;
        }
        if crate::update::download_and_replace(&client, &latest).await.is_err() {
            continue;
        }
        staged = Some(latest);
        // Binary replaced on disk. Supervised: wait for a clean moment, then let
        // the service manager restart us onto it. Unsupervised: it's staged and
        // takes effect on the next manual restart (we can't safely self-restart
        // with nothing to bring us back).
        if supervised {
            wait_for_idle(&daemon).await;
            trigger_restart();
            return;
        }
    }
}

/// Whether any live session is mid-turn (persisted status `Running`).
async fn any_session_busy(daemon: &Shared) -> bool {
    let sessions = daemon.sessions.lock().await;
    for s in sessions.values() {
        if let Ok(bytes) = std::fs::read(&s.state_path) {
            if let Ok(state) = deserialize_state(&bytes) {
                if state.status == crate::harness::HarnessStatus::Running {
                    return true;
                }
            }
        }
    }
    false
}

/// Block until no session is mid-turn, capped at ~5 minutes so a perpetually
/// busy session can't defer the update forever (a restart never loses persisted
/// state — at worst it interrupts one in-flight turn, which resumes cleanly).
async fn wait_for_idle(daemon: &Shared) {
    for _ in 0..60 {
        if !any_session_busy(daemon).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Ask the OS service manager to restart this daemon (systemd --user on Linux,
/// launchd on macOS) so it comes back on the freshly-installed binary.
fn trigger_restart() {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "snippet-serve.service"])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(uid) = current_uid() {
            let _ = std::process::Command::new("launchctl")
                .args(["kickstart", "-k", &format!("gui/{uid}/{SERVICE_LABEL}")])
                .spawn();
        }
    }
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
    // Effective model: a persisted per-conversation override is AUTHORITATIVE on
    // resume — the app re-sends a profile on plain navigation (foregrounding,
    // reopening the chat), and honoring it silently reverted the model the user
    // set for this chat via /session/model. An explicit profile only seeds a
    // conversation that has no override yet (e.g. new_conversation).
    let persisted = read_session_profile(&sp);
    let profile = persisted.clone().or_else(|| req.profile.clone());
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder);
        apply_profile(&mut w, &profile);
        w
    };

    let mut sessions = d.sessions.lock().await;
    if !sessions.contains_key(&id) {
        let handle = start_session(&cfg, sp.clone(), None, resume, None);
        if persisted.is_none() {
            if let Some(name) = req.profile.as_ref() {
                write_session_profile(&sp, name); // seed the initial override
            }
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
    reasoning_effort: Option<String>,
    stream: bool,
    /// Returned so profile editors can round-trip it — without it, an app edit
    /// can only guess and silently resets the flag.
    supports_images: bool,
}

#[derive(Serialize)]
struct ConfigView {
    profiles: Vec<ProfileView>,
    active: Option<String>,
    /// Profile that delegated lanes run on; null → they use the active model.
    delegate: Option<String>,
    theme: Option<String>,
    manual_approval: bool,
    hostname: String,
}

// GET /config — profiles with keys redacted (has_key only), active profile, theme.
async fn get_config(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await; // reflect profiles the TUI added/removed
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
                reasoning_effort: m.reasoning_effort.clone(),
                stream: m.stream,
                supports_images: m.supports_images,
            });
        }
    }
    Json(ConfigView {
        profiles,
        active,
        delegate: c.delegate_setup.clone(),
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
    /// Force the streaming wire protocol (needed by stream-only models, e.g. NIM MiniMax).
    #[serde(default)]
    stream: Option<bool>,
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
    // Reject providers `SnippetConfig::load` won't accept — persisting one works
    // in-memory but bricks the next daemon/TUI startup on the config re-parse.
    if !crate::config::provider_supported(&req.provider) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported provider `{}`; expected one of {}",
                req.provider,
                crate::config::SUPPORTED_PROVIDERS.join(", ")
            ),
        )
            .into_response();
    }
    d.reload_config().await; // modify the current on-disk config, not a stale copy
    let result = {
        let mut c = d.config.lock().unwrap();
        let name = req
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| c.unique_profile_key(&req.provider));
        // Start from the existing profile so an edit only changes what the
        // request states — rebuilding from defaults silently wiped hand-tuned
        // fields (user_agent, temperature, retries, cache_prompt, …).
        let mut mc = c
            .setups
            .as_ref()
            .and_then(|m| m.get(&name))
            .cloned()
            .unwrap_or_default();
        mc.provider = req.provider.clone();
        mc.model = req.model.clone();
        if let Some(url) = req.base_url.clone().filter(|s| !s.trim().is_empty()) {
            mc.base_url = url;
        } else if mc.base_url.trim().is_empty() {
            mc.base_url = ModelConfig::default().base_url;
        }
        // An omitted/blank api_key keeps the existing one (editing doesn't wipe it).
        if let Some(key) = req.api_key.clone().filter(|s| !s.is_empty()) {
            mc.api_key = key;
        }
        // For the optional fields: an explicit value wins; omitted keeps current.
        if let Some(effort) = req.reasoning_effort.clone() {
            mc.reasoning_effort = Some(effort).filter(|s| !s.is_empty());
        }
        if let Some(images) = req.supports_images {
            mc.supports_images = images;
        }
        if let Some(ctx) = req.context_window.filter(|&n| n > 0) {
            mc.context_window = ctx;
        }
        if let Some(stream) = req.stream {
            mc.stream = stream;
        }
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
    d.reload_config().await; // don't clobber TUI-side profile edits
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
struct VaultSetReq {
    name: String,
    value: String,
}

// GET /vault — secret NAMES only; values never leave the daemon.
async fn vault_list(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "names": crate::vault::Vault::load().names() })).into_response()
}

// PUT /vault — store a secret (from the app's vault screen; TLS/tunnel carries it).
async fn vault_set(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<VaultSetReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let mut vault = crate::vault::Vault::load();
    match vault.set(&req.name, &req.value) {
        Ok(()) => Json(serde_json::json!({ "stored": req.name })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct VaultNameQ {
    name: String,
    token: Option<String>,
}

// DELETE /vault?name= — remove a secret.
async fn vault_delete(State(d): State<Shared>, Query(q): Query<VaultNameQ>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let mut vault = crate::vault::Vault::load();
    match vault.remove(&q.name) {
        Ok(true) => Json(serde_json::json!({ "removed": q.name })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such secret").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct ProviderModelsReq {
    /// Existing profile to list models for; its stored key/base URL are used.
    #[serde(default)]
    name: Option<String>,
    /// Ad-hoc lookup for a profile being created in an editor (not yet saved).
    /// `api_key` falls back to the named profile's stored key when empty.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

// POST /provider/models — query the provider's own models API (key stays
// server-side) and return a normalized catalog: real model IDs plus whatever
// capabilities the provider reports (effort tiers on Anthropic, reasoning
// support on OpenRouter, context windows where available).
async fn provider_models(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ProviderModelsReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let mut cfg = {
        let c = d.config.lock().unwrap();
        let stored = req
            .name
            .as_deref()
            .and_then(|n| c.setups.as_ref().and_then(|m| m.get(n)).cloned());
        match stored {
            Some(m) => m,
            None if req.provider.is_some() => crate::config::ModelConfig {
                provider: req.provider.clone().unwrap_or_default(),
                ..Default::default()
            },
            None => return (StatusCode::NOT_FOUND, "no such profile").into_response(),
        }
    };
    // Editor-supplied overrides win over the stored profile's values.
    if let Some(p) = req.provider {
        cfg.provider = p;
    }
    if let Some(b) = req.base_url {
        if !b.trim().is_empty() {
            cfg.base_url = b;
        }
    }
    if let Some(k) = req.api_key {
        if !k.trim().is_empty() {
            cfg.api_key = k;
        }
    }
    match crate::catalog::fetch_models(&cfg).await {
        Ok(models) => Json(serde_json::json!({ "models": models })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

#[derive(Deserialize)]
struct DelegateReq {
    /// Profile for delegated lanes. Empty/null clears it (delegation → active model).
    #[serde(default)]
    name: Option<String>,
}

// POST /config/delegate — set (or clear) the profile that delegated lanes run on.
async fn set_delegate(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<DelegateReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await; // don't clobber TUI-side profile edits
    let name = req.name.filter(|n| !n.trim().is_empty());
    let result = {
        let mut c = d.config.lock().unwrap();
        if let Some(n) = name.as_deref() {
            if !c.setups.as_ref().is_some_and(|m| m.contains_key(n)) {
                return (StatusCode::NOT_FOUND, "no such profile").into_response();
            }
        }
        c.delegate_setup = name.clone();
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => Json(serde_json::json!({ "delegate": name })).into_response(),
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
    d.reload_config().await; // start from current disk state so we don't resurrect TUI-deleted profiles
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
    d.reload_config().await; // a profile just created in the TUI must be selectable
    let model_cfg = {
        let c = d.config.lock().unwrap();
        match c.setups.as_ref().and_then(|m| m.get(&req.profile)).cloned() {
            Some(m) => m,
            None => return (StatusCode::NOT_FOUND, "no such profile").into_response(),
        }
    };
    let (sp, folder) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
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
    let (_sp, workspace) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
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
    let (_sp, dir) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if req.command.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty command").into_response();
    }
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&req.command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        // On timeout the output future is dropped — kill the child then, or it
        // keeps running detached forever with no handle to find or stop it.
        .kill_on_drop(true)
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
    let (stdout, t1) = clip_output(&out.stdout, 20_000);
    let (stderr, t2) = clip_output(&out.stderr, 20_000);
    Json(serde_json::json!({
        "exit_code": out.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": t1 || t2,
    }))
    .into_response()
}

/// Resolve a session id to its on-disk workspace folder, or an error response.
/// Resolve the directory git should run in. The value is either a session id
/// (→ that session's workspace) or, for git on a plain folder with no session
/// (e.g. the file explorer), a direct directory path.
fn resolve_session_dir(session: &str) -> Result<PathBuf, Response> {
    if let Some(sp) = state_path_for_id(session) {
        if let Ok(bytes) = std::fs::read(&sp) {
            if let Ok(state) = deserialize_state(&bytes) {
                let dir = PathBuf::from(&state.workspace);
                if !state.workspace.is_empty() && dir.is_dir() {
                    return Ok(dir);
                }
            }
        }
    }
    // Not a session id → treat it as a folder path (no-session git).
    let dir = PathBuf::from(session);
    if dir.is_dir() {
        return Ok(dir);
    }
    Err((StatusCode::NOT_FOUND, "no such session or directory").into_response())
}

/// Lossy-decode bytes and clip to `max` chars, returning (text, was_truncated).
fn clip_output(b: &[u8], max: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > max {
        (s.chars().take(max).collect::<String>() + "\u{2026}", true)
    } else {
        (s.into_owned(), false)
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
    let push_daemon = daemon.clone();
    let push_session = session.clone();
    let push = tokio::spawn(async move {
        let daemon = push_daemon;
        let session = push_session;
        let mut last_mtime = None;
        let mut last_count: Option<usize> = None;
        let mut last_head: u64 = 0;
        let mut last_tail: u64 = 0;
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
                                        // Rate limits are PROVIDER-scoped. Only ChatGPT sessions get
                                        // the account-wide overlay; every other provider gets NO
                                        // rate_limit — including scrubbing a stale snapshot persisted
                                        // before a model switch (it showed ChatGPT's monthly limits
                                        // on an anthropic-compatible chat).
                                        if daemon.session_provider(&session).await == "chatgpt" {
                                            if let Some(g) = crate::chatgpt::read_global_usage() {
                                                if let Ok(gv) = serde_json::to_value(&g) {
                                                    o.insert("rate_limit".into(), gv);
                                                }
                                            }
                                        } else {
                                            o.remove("rate_limit");
                                        }
                                    }
                                    let count = state.events.len();
                                    let head = events_head_fp(&state);
                                    // Full snapshot on connect / compaction (head changed) /
                                    // shrink / REWRITE — an interrupt rolls events back and
                                    // appends, which can land on the same count with different
                                    // content; verify the last event the client saw is intact.
                                    let snapshot = match last_count {
                                        None => true,
                                        Some(lc) => {
                                            head != last_head
                                                || count < lc
                                                || events_fp_at(&state, lc.wrapping_sub(1))
                                                    != last_tail
                                        }
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
                                    last_tail = events_fp_at(&state, count.wrapping_sub(1));
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
    events_fp_at(state, 0)
}

/// Fingerprint of the event at `idx` (0 when out of range / empty).
fn events_fp_at(state: &crate::harness::HarnessState, idx: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(event) = state.events.get(idx) {
        if let Ok(s) = serde_json::to_string(event) {
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
