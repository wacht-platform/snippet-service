//! Headless control daemon. Runs alongside (never replacing) the TUI: it manages
//! sessions across the device and exposes them over HTTP + WebSocket so a remote
//! client (mobile app) can browse folders, open a session in any folder, list every
//! session on the box, and stream/drive one. Every endpoint is token-authed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
use crate::harness::{LoopInput, deserialize_state, serialize_state};
use crate::mission_control::{self, ManagedSession, NotificationMarker, TaskRecord, TaskStatus};
use crate::session::{
    list_device_sessions, read_session_profile, start_mission_control_session,
    start_session_with_browser_summary, state_path_for_id, write_session_profile,
};

mod browser;
mod fs;
mod git;
mod lifecycle;
use crate::term::SessionTerms;
pub mod sidecar;
mod transcribe;
mod tunnel;

pub use self::lifecycle::*;
pub use self::tunnel::ensure_cloudflared_foreground;

use self::browser::{BrowserManager, RegisterMessage};
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
    /// Live token stream for attached clients (TUI/mobile). Shared with the
    /// harness so /attach can push partial answer/thinking without waiting for
    /// the next state.json write.
    stream: crate::llm::StreamHandle,
    /// Interactive human PTYs for this session (not used by the agent bash tool).
    terms: Arc<SessionTerms>,
}

fn live_from_handle(handle: crate::session::SessionHandle, profile: Option<String>) -> LiveSession {
    let cwd = handle
        .state_path
        .parent()
        .and_then(|p| {
            deserialize_state(&std::fs::read(p.join("state.json")).unwrap_or_default())
                .ok()
                .map(|s| PathBuf::from(s.workspace))
        })
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = {
        let from_state = std::fs::read(&handle.state_path)
            .ok()
            .and_then(|b| deserialize_state(&b).ok())
            .map(|s| PathBuf::from(s.workspace))
            .filter(|p| p.is_dir());
        from_state.unwrap_or(cwd)
    };
    LiveSession {
        input_tx: handle.input_tx,
        join: handle.join,
        state_path: handle.state_path,
        profile,
        stream: handle.stream.unwrap_or_else(|| {
            std::sync::Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()))
        }),
        terms: SessionTerms::new(cwd),
    }
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
    /// Connected browser-extension sockets and their pending command waiters.
    browser: BrowserManager,
    /// Recent idempotency nonces for inbound client inputs: `"session:nonce"` →
    /// first-seen time. Prevents duplicate user messages and decision retries when
    /// the mobile client resends after a reconnect.
    seen_nonces: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    mission_control_root: PathBuf,
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
        token
            .as_deref()
            .is_some_and(|t| token_matches(t, &self.token))
    }

    /// Returns true if this nonce is new. The ledger is persisted beside the
    /// conversation state so a daemon restart cannot accept the same retry twice.
    fn accept_nonce(&self, session_id: &str, nonce: &str, state_path: &Path) -> bool {
        let key = format!("{session_id}:{nonce}");
        let mut map = self.seen_nonces.lock().unwrap();
        if map.contains_key(&key) {
            return false;
        }

        let ledger_path = state_path.with_extension("nonces.json");
        let mut persisted = std::fs::read_to_string(&ledger_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        if persisted.iter().any(|n| n == nonce) {
            map.insert(key, std::time::Instant::now());
            return false;
        }

        // Bound the durable ledger. Nonces are unique client request IDs, so
        // retaining the most recent 512 is enough to cover reconnect retries
        // without allowing an unbounded sidecar to grow.
        persisted.push(nonce.to_string());
        if persisted.len() > 512 {
            let drop_count = persisted.len() - 512;
            persisted.drain(..drop_count);
        }
        let persisted_bytes = match serde_json::to_vec(&persisted) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };
        let tmp = ledger_path.with_extension("nonces.json.tmp");
        let persisted_ok = (|| {
            let mut file = std::fs::File::create(&tmp).ok()?;
            use std::io::Write;
            file.write_all(&persisted_bytes).ok()?;
            file.sync_all().ok()?;
            std::fs::rename(&tmp, &ledger_path).ok()?;
            Some(())
        })()
        .is_some();
        if !persisted_ok {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        map.insert(key, std::time::Instant::now());
        true
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
    async fn ensure_live(
        &self,
        id: &str,
    ) -> Option<(
        UnboundedSender<LoopInput>,
        PathBuf,
        crate::llm::StreamHandle,
    )> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(id) {
            return Some((s.input_tx.clone(), s.state_path.clone(), s.stream.clone()));
        }
        let sp = state_path_for_id(id)?;
        let bytes = std::fs::read(&sp).ok()?;
        let state = deserialize_state(&bytes).ok()?;
        let folder = PathBuf::from(&state.workspace);
        if state.workspace.is_empty() || !folder.is_dir() {
            return None;
        }
        let profile = read_session_profile(&sp);
        self.reload_config().await; // pick up profiles added from the TUI
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            apply_profile(&mut w, &profile);
            w
        };
        // Role-aware resume: a session opened as Mission Control must come back
        // with the MC prompt/tools/lane restrictions after a daemon restart,
        // not silently downgrade to an ordinary conversation session.
        let role = read_session_role(&sp);
        let handle = match role.as_deref() {
            Some("mission_control") => crate::session::start_mission_control_session(
                &cfg,
                sp.clone(),
                None,
                true,
                Some(std::sync::Arc::new(std::sync::Mutex::new(
                    crate::llm::StreamBuffer::default(),
                ))),
                Some(self.browser.summary_provider()),
            ),
            _ => start_session_with_browser_summary(
                &cfg,
                sp.clone(),
                None,
                true,
                Some(std::sync::Arc::new(std::sync::Mutex::new(
                    crate::llm::StreamBuffer::default(),
                ))),
                Some(self.browser.summary_provider()),
            ),
        };
        let tx = handle.input_tx.clone();
        let live = live_from_handle(handle, profile);
        let stream = live.stream.clone();
        sessions.insert(id.to_string(), live);
        Some((tx, sp, stream))
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
        if let Some(old) = sessions.remove(id) {
            old.join.abort();
        }
        let handle = start_session_with_browser_summary(
            &cfg,
            sp.clone(),
            None,
            true,
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(self.browser.summary_provider()),
        );
        sessions.insert(id.to_string(), live_from_handle(handle, profile));
        RebuildOutcome::Rebuilt
    }

    /// Send a loop input to a session. Audio attachment markers are expanded here,
    /// before the input reaches the harness, so every model/provider receives the
    /// same transcript plus the original attachment reference.
    async fn deliver(&self, id: &str, input: LoopInput) {
        let input = match input {
            LoopInput::UserMessage(text) => {
                match transcribe::prepare_message(self, text.clone()).await {
                    Ok(text) => LoopInput::UserMessage(text),
                    Err(error) => LoopInput::UserMessage(format!(
                        "{text}\n\n[Audio transcription unavailable: {error}. The original audio attachment remains available.]"
                    )),
                }
            }
            other => other,
        };
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
        let handle = start_session_with_browser_summary(
            &cfg,
            sp.clone(),
            initial,
            true,
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(self.browser.summary_provider()),
        );
        // Control inputs weren't consumed as the first turn — hand them to the
        // freshly-parked loop so it acts on them (idle-arm compaction, goal, etc.).
        if forward {
            let _ = handle.input_tx.send(input);
        }
        sessions.insert(id.to_string(), live_from_handle(handle, profile));
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
    crate::serve::lifecycle::stamp_binary_hash();
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
        browser: BrowserManager::default(),
        seen_nonces: std::sync::Mutex::new(HashMap::new()),
        mission_control_root: mission_control::MissionControlStore::default_root(None),
    });

    // Background self-update: periodically check for a newer release, replace the
    // binary in place, wait for every session to be between turns (so nothing
    // in-flight is lost), then restart to run the new code.
    if !crate::update::disabled() {
        let d = daemon.clone();
        tokio::spawn(async move { self_update_loop(d, supervised).await });
    }
    // Binary watch: detect external replacement (manual `cp` + `mv`) and
    // auto-restart so the new binary takes effect within ~30s.
    {
        let d = daemon.clone();
        tokio::spawn(async move { binary_watch_loop(d, supervised).await });
    }
    // Watch config.toml: when it changes (a profile edited in the app/TUI, an
    // added model, image support toggled, …) reload it and rebuild the model of
    // every live session so the change takes effect WITHOUT a manual model switch.
    {
        let d = daemon.clone();
        tokio::spawn(async move { config_watch_loop(d).await });
    }
    {
        let d = daemon.clone();
        tokio::spawn(async move { mission_control_dispatch_loop(d).await });
    }
    // The upload endpoint carries the file base64-encoded inside a JSON body, which
    // inflates it by ~4/3. Size the request-body limit so a ~1 GB file still fits
    // once encoded (≈1.33 GB) plus headroom for the JSON envelope. Every other route
    // keeps axum's small default body limit.
    const MAX_UPLOAD_FILE_BYTES: usize = 1024 * 1024 * 1024;
    const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_FILE_BYTES / 3 * 4 + 64 * 1024;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/sessions/counts", get(session_counts))
        .route("/mission-control/overview", get(mission_control_overview))
        .route(
            "/mission-control/settings",
            get(mission_control_settings).put(mission_control_update_settings),
        )
        .route("/mission-control/open", post(mission_control_open))
        .route(
            "/mission-control/tasks",
            get(mission_control_tasks).post(mission_control_create_task),
        )
        .route(
            "/mission-control/tasks/{id}",
            put(mission_control_update_task),
        )
        .route(
            "/mission-control/tasks/{id}/archive",
            post(mission_control_archive_task),
        )
        .route(
            "/mission-control/sessions",
            get(mission_control_sessions).post(mission_control_create_session),
        )
        .route(
            "/mission-control/sessions/{id}",
            put(mission_control_update_session),
        )
        .route(
            "/mission-control/sessions/{id}/archive",
            post(mission_control_archive_session),
        )
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
        .route("/browser/ws", get(browser_ws))
        .route("/browsers", get(list_browsers))
        .route("/browser/command", post(browser_command))
        .route("/config", get(get_config))
        .route("/config/profile", put(put_profile).delete(delete_profile))
        .route("/config/active", post(set_active))
        .route("/config/delegate", post(set_delegate))
        .route("/provider/models", post(provider_models))
        .route(
            "/vault",
            get(vault_list).put(vault_set).delete(vault_delete),
        )
        .route("/xai/login", post(xai_login))
        .route("/xai/status", get(xai_status))
        .route("/xai/logout", post(xai_logout))
        .route("/chatgpt/login", post(chatgpt_login))
        .route("/chatgpt/status", get(chatgpt_status))
        .route("/chatgpt/logout", post(chatgpt_logout))
        .route("/session/model", post(set_session_model))
        .route("/session/rewind", post(rewind_session))
        .route("/session/fork", post(fork_session))
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
        .route("/bg", post(bg_list))
        .route("/bg/kill", post(bg_kill))
        .route("/bg/log", post(bg_log))
        .route("/git/stash", post(git_stash))
        .with_state(daemon);
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("invalid bind address {host}:{port}: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

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
    println!(
        "serve up at {public_url} (token elided — `snippet serve --status` shows the connection)"
    );
    write_serve_state(&public_url, &token_for_print, host, port);
    crate::serve::lifecycle::stamp_binary_hash();

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
    let mut last_mtime = tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok());
    let mut pending: HashSet<String> = HashSet::new();

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if let Ok(mtime) = meta.modified() {
                if Some(mtime) != last_mtime {
                    last_mtime = Some(mtime);
                    daemon.reload_config().await;
                    let ids: Vec<String> = daemon.sessions.lock().await.keys().cloned().collect();
                    eprintln!(
                        "config.toml changed — reloaded; rebuilding {} live session model(s)",
                        ids.len()
                    );
                    pending.extend(ids);
                }
            }
        }

        if pending.is_empty() {
            continue;
        }
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
        if crate::update::download_and_replace(&client, &latest)
            .await
            .is_err()
        {
            continue;
        }
        #[allow(unused_assignments)]
        {
            staged = Some(latest);
        }
        wait_for_idle(&daemon).await;
        if supervised {
            trigger_restart();
        } else {
            self_restart_process();
        }
        return;
    }
}

/// Resolve the real filesystem path of the running binary, bypassing
/// `/proc/self/exe` which keeps the old inode after `mv`.
fn resolve_exe_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(exe) = std::env::current_exe() {
            // current_exe() returns the path string (e.g. /home/.../snippet)
            // even though /proc/self/exe points at the old inode — the PathBuf
            // itself is just the string, so stat() on it will follow the
            // current directory entry.
            if exe.exists() {
                return Some(exe);
            }
        }
        let link = std::fs::read_link("/proc/self/exe").ok()?;
        return Some(link);
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().ok()
    }
}

/// Watch the on-disk binary for external replacement (manual `cp` + `mv`).
/// When the inode or mtime of the exe path differs from what we were started
/// with, the binary has been swapped — restart to pick it up.
async fn binary_watch_loop(daemon: Shared, supervised: bool) {
    use std::time::Duration;
    const CHECK_EVERY: Duration = Duration::from_secs(30);
    let exe = match resolve_exe_path() {
        Some(p) => p,
        None => return,
    };
    let initial_meta = match std::fs::metadata(&exe) {
        Ok(m) => Some((inode_from_meta(&m), mtime_from_meta(&m))),
        Err(_) => None,
    };
    let (initial_inode, initial_mtime) = match initial_meta {
        Some(v) => v,
        None => return,
    };
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        let meta = match std::fs::metadata(&exe) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let cur_inode = inode_from_meta(&meta);
        let cur_mtime = mtime_from_meta(&meta);
        if cur_inode != initial_inode || cur_mtime != initial_mtime {
            wait_for_idle(&daemon).await;
            if supervised {
                trigger_restart();
            } else {
                self_restart_process();
            }
            return;
        }
    }
}

#[cfg(unix)]
fn inode_from_meta(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}
#[cfg(not(unix))]
fn inode_from_meta(_m: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn mtime_from_meta(m: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime()
}
#[cfg(not(unix))]
fn mtime_from_meta(_m: &std::fs::Metadata) -> i64 {
    0
}

/// Replace the current process with a fresh execution of itself. On Unix this
/// uses `exec()` so the PID, env vars (including `__SNIPPET_SERVE_WORKER`), and
/// file descriptors are preserved — the new binary picks up exactly where we
/// left off.
fn self_restart_process() {
    use std::os::unix::process::CommandExt;
    let exe = match resolve_exe_path() {
        Some(p) => p,
        None => return,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    // exec() replaces us in-place; it only returns on failure.
    let err = std::process::Command::new(&exe).args(&args).exec();
    eprintln!("failed to exec restart: {err}");
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

#[derive(Deserialize)]
struct BrowserCommandReq {
    #[serde(alias = "deviceName")]
    device_name: String,
    method: String,
    #[serde(default)]
    args: serde_json::Value,
}

/// GET /browsers — currently connected browser extensions.
async fn list_browsers(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({
        "browsers": d.browser.list().await,
    }))
    .into_response()
}

/// POST /browser/command — authenticated relay used by the future CLI.
async fn browser_command(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BrowserCommandReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.device_name.trim().is_empty() || req.method.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "device_name and method are required",
        )
            .into_response();
    }
    match d
        .browser
        .send_command_for_device_name(&req.device_name, &req.method, req.args)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "device_name": req.device_name,
            "method": req.method,
            "result": result,
        }))
        .into_response(),
        Err(error) => Json(serde_json::json!({
            "ok": false,
            "device_name": req.device_name,
            "method": req.method,
            "error": error,
        }))
        .into_response(),
    }
}

/// WS /browser/ws?token= — extension-initiated command channel.
async fn browser_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    ws.on_upgrade(move |socket| handle_browser_ws(socket, d))
}

async fn handle_browser_ws(socket: WebSocket, daemon: Shared) {
    let (mut sender, mut receiver) = socket.split();
    let Some(Ok(Message::Text(first))) = receiver.next().await else {
        return;
    };
    let Ok(register_value) = serde_json::from_str::<serde_json::Value>(first.as_str()) else {
        let _ = sender
            .send(Message::Text(
                serde_json::json!({"type": "error", "error": "first message must be JSON"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };
    if register_value
        .get("type")
        .and_then(serde_json::Value::as_str)
        != Some("register")
    {
        let _ = sender
            .send(Message::Text(
                serde_json::json!({"type": "error", "error": "first message must be register"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }
    let Ok(registration) = serde_json::from_value::<RegisterMessage>(register_value) else {
        let _ = sender
            .send(Message::Text(
                serde_json::json!({"type": "error", "error": "invalid register message"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };
    let (outbound, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let info = match daemon.browser.register(registration, outbound).await {
        Ok(info) => info,
        Err(error) => {
            let _ = sender
                .send(Message::Text(
                    serde_json::json!({"type": "error", "error": error})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let browser_id = info.browser_id.clone();
    if sender
        .send(Message::Text(
            serde_json::json!({
                "type": "registered",
                "protocol": 1,
                "browser": info.browser,
                "deviceName": info.device_name,
            })
            .to_string()
            .into(),
        ))
        .await
        .is_err()
    {
        daemon.browser.unregister(&browser_id).await;
        return;
    }

    let send_task = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = receiver.next().await {
        match message {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                    continue;
                };
                match value.get("type").and_then(serde_json::Value::as_str) {
                    Some("heartbeat") => {
                        daemon.browser.touch(&browser_id).await;
                        let _ = daemon
                            .browser
                            .send_message(
                                &browser_id,
                                Message::Text(
                                    serde_json::json!({
                                        "type": "heartbeat_ack",
                                        "at": value.get("at").cloned().unwrap_or(serde_json::Value::Null),
                                    })
                                    .to_string()
                                    .into(),
                                ),
                            )
                            .await;
                    }
                    Some("pong") => {
                        daemon.browser.touch(&browser_id).await;
                    }
                    Some("result") => {
                        let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
                            continue;
                        };
                        let ok = value
                            .get("ok")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        daemon
                            .browser
                            .complete(
                                id,
                                ok,
                                value.get("result").cloned(),
                                value
                                    .get("error")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string),
                            )
                            .await;
                        daemon.browser.touch(&browser_id).await;
                    }
                    Some("tab_event") => daemon.browser.touch(&browser_id).await,
                    _ => {}
                }
            }
            Message::Ping(payload) => {
                daemon.browser.touch(&browser_id).await;
                let _ = daemon
                    .browser
                    .send_message(&browser_id, Message::Pong(payload))
                    .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    send_task.abort();
    daemon.browser.unregister(&browser_id).await;
}

// GET /sessions[?folder=] — device sessions (optionally scoped to one folder),
// each with a `running` flag.
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
async fn open_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<OpenReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let folder = PathBuf::from(&req.folder);
    if !folder.is_dir() {
        return (StatusCode::BAD_REQUEST, "not a directory").into_response();
    }
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
    let id = sp
        .strip_prefix(workspaces_root())
        .unwrap_or(&sp)
        .display()
        .to_string();
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
        let handle = start_session_with_browser_summary(
            &cfg,
            sp.clone(),
            None,
            resume,
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(d.browser.summary_provider()),
        );
        if persisted.is_none() {
            if let Some(name) = req.profile.as_ref() {
                write_session_profile(&sp, name); // seed the initial override
            }
        }
        sessions.insert(id.clone(), live_from_handle(handle, profile));
    }
    Json(serde_json::json!({ "id": id, "folder": req.folder })).into_response()
}

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
async fn put_profile(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ProfileReq>,
) -> Response {
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
async fn set_active(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ActiveReq>,
) -> Response {
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
// POST /xai/login — begin the xAI device-code flow and poll for approval in the
// background (saving the token on success). Returns the code + URL for the app to
// show; the app then polls /xai/status until signed_in flips true.
async fn xai_login(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::xai_auth::begin_device_code_login().await {
        Ok(device) => {
            let poll = device.clone();
            tokio::spawn(async move {
                if let Ok(tokens) = crate::xai_auth::poll_for_tokens(poll).await {
                    let _ = crate::xai_auth::save_blocking(&tokens);
                }
            });
            Json(serde_json::json!({
                "user_code": device.user_code,
                "verification_uri": device.verification_uri,
                "expires_in": device.expires_in_s,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// GET /xai/status — whether an xAI subscription token is stored.
async fn xai_status(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "signed_in": crate::xai_auth::is_signed_in() })).into_response()
}

// POST /xai/logout — drop the stored xAI token.
async fn xai_logout(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::xai_auth::logout_blocking() {
        Ok(()) => Json(serde_json::json!({ "signed_in": false })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /chatgpt/login — begin the ChatGPT device-code flow; poll + save in the
// background. Returns the code + URL for the app to show.
async fn chatgpt_login(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::chatgpt_auth::begin_device_code_login().await {
        Ok(device) => {
            let user_code = device.user_code.clone();
            let url = device.verification_url.clone();
            tokio::spawn(async move {
                if let Ok(tokens) = crate::chatgpt_auth::complete_device_code_login(device).await {
                    let _ = crate::chatgpt_auth::save_blocking(&tokens);
                }
            });
            Json(serde_json::json!({
                "user_code": user_code,
                "verification_uri": url,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// GET /chatgpt/status — whether a ChatGPT subscription token is stored.
async fn chatgpt_status(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "signed_in": crate::chatgpt_auth::is_signed_in() })).into_response()
}

// POST /chatgpt/logout — drop the stored ChatGPT token.
async fn chatgpt_logout(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::chatgpt_auth::logout_blocking() {
        Ok(()) => Json(serde_json::json!({ "signed_in": false })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn vault_list(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "names": crate::vault::Vault::load().names() })).into_response()
}

// PUT /vault — store a secret (from the app's vault screen; TLS/tunnel carries it).
async fn vault_set(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<VaultSetReq>,
) -> Response {
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
async fn set_delegate(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<DelegateReq>,
) -> Response {
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
async fn set_session_model(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SessionModelReq>,
) -> Response {
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
    let handle = start_session_with_browser_summary(
        &cfg,
        sp.clone(),
        None,
        true,
        Some(std::sync::Arc::new(std::sync::Mutex::new(
            crate::llm::StreamBuffer::default(),
        ))),
        Some(d.browser.summary_provider()),
    );
    write_session_profile(&sp, &req.profile); // persist so it survives restart
    sessions.insert(
        req.session.clone(),
        live_from_handle(handle, Some(req.profile.clone())),
    );
    Json(serde_json::json!({ "session": req.session, "profile": req.profile })).into_response()
}

#[derive(Deserialize)]
struct RewindReq {
    session: String,
    checkpoint: String,
}

// POST /session/rewind {session, checkpoint} — restore workspace files AND
// truncate conversation history to that checkpoint. Always updates the state
// file so clients (mobile/TUI) see the truncated transcript immediately; also
// notifies a live loop when present so in-memory state matches.
async fn rewind_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RewindReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let (sp, workspace) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let checkpoint_id = req.checkpoint.clone();

    // Load current state from disk (authoritative for history indices).
    let Ok(bytes) = tokio::fs::read(&sp).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read session state",
        )
            .into_response();
    };
    let Ok(mut state) = deserialize_state(&bytes) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse session state",
        )
            .into_response();
    };

    let label = match state.apply_checkpoint_rewind(&checkpoint_id) {
        Ok(label) => label,
        Err(_) => return (StatusCode::NOT_FOUND, "checkpoint not found").into_response(),
    };

    // Persist truncated history first so any client re-read sees the cut.
    let Ok(updated_bytes) = serialize_state(&state) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to serialize state",
        )
            .into_response();
    };
    if tokio::fs::write(&sp, &updated_bytes).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to write state").into_response();
    }
    crate::session::write_session_meta(&sp, &state);

    // Keep a live loop in sync (it may overwrite disk on its next persist otherwise).
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session) {
            if !s.join.is_finished() {
                let _ = s.input_tx.send(LoopInput::Rewind {
                    checkpoint: checkpoint_id.clone(),
                });
            }
        }
    }

    // Restore workspace files to the shadow commit.
    let checkpoint_for_restore = checkpoint_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::checkpoint::restore(&workspace, &checkpoint_for_restore)
    })
    .await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({
            "restored": checkpoint_id,
            "label": label,
            "event_end": state.events.len(),
            "message_end": state.messages.len(),
        }))
        .into_response(),
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
async fn exec_in_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ExecReq>,
) -> Response {
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
            .into_response();
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

#[derive(Deserialize)]
struct BgReq {
    session: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

// POST /bg {session} — snapshot of the session's background processes.
async fn bg_list(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    Json(serde_json::json!({ "processes": crate::bg::list(&dir) })).into_response()
}

// POST /bg/kill {session, id} — terminate one background process.
async fn bg_kill(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    match crate::bg::kill_by_id(&dir, id) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})).into_response(),
    }
}

// POST /bg/log {session, id, tail?} — tail a background process's log.
async fn bg_log(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BgReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    let text = std::fs::read_to_string(crate::bg::log_path(&dir, id)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(req.tail.unwrap_or(400));
    Json(serde_json::json!({ "log": lines[start..].join("\n"), "truncated": start > 0 }))
        .into_response()
}

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
async fn delete_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<DeleteReq>,
) -> Response {
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
async fn rename_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RenameReq>,
) -> Response {
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
struct ForkReq {
    session: String,
    /// Checkpoint id (or unique prefix) — same cut as `/session/rewind`.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Inclusive event index to keep through (snapped to a provider-safe boundary).
    #[serde(default)]
    event_index: Option<usize>,
}

// POST /session/fork {session, checkpoint?|event_index?} — branch a NEW conversation
// at the chosen history point. Source session is left untouched. Workspace files are
// shared (not snapshotted); only conversation history is forked.
async fn fork_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ForkReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(sp) = state_path_for_id(&req.session) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };

    // Prefer live in-memory state when the session is running so the fork sees
    // events that may not have flushed yet; fall back to the on-disk snapshot.
    let state = {
        let sessions = d.sessions.lock().await;
        if let Some(live) = sessions.get(&req.session) {
            if !live.join.is_finished() {
                if let Ok(bytes) = std::fs::read(&live.state_path) {
                    if let Ok(s) = deserialize_state(&bytes) {
                        s
                    } else {
                        drop(sessions);
                        match std::fs::read(&sp)
                            .ok()
                            .and_then(|b| deserialize_state(&b).ok())
                        {
                            Some(s) => s,
                            None => {
                                return (StatusCode::INTERNAL_SERVER_ERROR, "bad session state")
                                    .into_response();
                            }
                        }
                    }
                } else {
                    drop(sessions);
                    match std::fs::read(&sp)
                        .ok()
                        .and_then(|b| deserialize_state(&b).ok())
                    {
                        Some(s) => s,
                        None => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "session state unreadable",
                            )
                                .into_response();
                        }
                    }
                }
            } else {
                drop(sessions);
                match std::fs::read(&sp)
                    .ok()
                    .and_then(|b| deserialize_state(&b).ok())
                {
                    Some(s) => s,
                    None => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "session state unreadable",
                        )
                            .into_response();
                    }
                }
            }
        } else {
            drop(sessions);
            match std::fs::read(&sp)
                .ok()
                .and_then(|b| deserialize_state(&b).ok())
            {
                Some(s) => s,
                None => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "session state unreadable",
                    )
                        .into_response();
                }
            }
        }
    };

    let point = match crate::session::resolve_fork_point(
        &state,
        req.checkpoint.as_deref(),
        req.event_index,
    ) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    match crate::session::write_forked_conversation(&sp, &state, point) {
        Ok(forked) => Json(serde_json::json!({
            "id": forked.id,
            "title": forked.title,
            "event_end": forked.event_end,
            "message_end": forked.message_end,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct AttachQuery {
    token: Option<String>,
    session: String,
}

// WS /attach?session= — stream this session's HarnessState + receive LoopInput.
async fn attach_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(q): Query<AttachQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.ensure_live(&q.session).await {
        Some((_, state_path, stream)) => {
            let terms = {
                let sessions = d.sessions.lock().await;
                sessions.get(&q.session).map(|s| s.terms.clone())
            };
            let daemon = d.clone();
            let session = q.session.clone();
            ws.on_upgrade(move |socket| {
                handle_ws(socket, daemon, session, state_path, stream, terms)
            })
        }
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn handle_ws(
    socket: WebSocket,
    daemon: Shared,
    session: String,
    state_path: PathBuf,
    stream: crate::llm::StreamHandle,
    terms: Option<std::sync::Arc<crate::term::SessionTerms>>,
) {
    let (mut sender, mut receiver) = socket.split();

    let push_daemon = daemon.clone();
    let push_session = session.clone();
    let push_state_path = state_path.clone();
    let push_stream = stream.clone();
    let push_terms = terms.clone();
    let push = tokio::spawn(async move {
        let daemon = push_daemon;
        let session = push_session;
        let state_path = push_state_path;
        let stream = push_stream;
        let terms = push_terms;
        let term_client = terms.as_ref().map(|t| t.subscribe());
        let mut last_mtime = None;
        let mut last_events: Vec<crate::harness::HarnessEvent> = Vec::new();
        let mut last_stream_fp: u64 = 0;
        let mut term_seq: u64 = 0;
        loop {
            if let Ok(meta) = tokio::fs::metadata(&state_path).await {
                if let Ok(mtime) = meta.modified() {
                    if Some(mtime) != last_mtime {
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
                                    let snapshot = if last_events.is_empty() {
                                        true
                                    } else {
                                        count < last_events.len()
                                            || state.events[..last_events.len()] != last_events[..]
                                    };
                                    if let Some(o) = v.as_object_mut() {
                                        if snapshot {
                                            o.insert("wire".into(), serde_json::json!("snapshot"));
                                        } else {
                                            let start = last_events.len();
                                            let tail = serde_json::to_value(&state.events[start..])
                                                .unwrap_or_default();
                                            o.remove("events");
                                            o.insert("wire".into(), serde_json::json!("delta"));
                                            o.insert("new_events".into(), tail);
                                            o.insert(
                                                "event_count".into(),
                                                serde_json::json!(count),
                                            );
                                        }
                                    }
                                    last_events = state.events.clone();
                                    if let Ok(json) = serde_json::to_string(&v) {
                                        if sender.send(Message::Text(json.into())).await.is_err() {
                                            break;
                                        }
                                        // Mark this mtime handled only after the full
                                        // state was read and delivered. Atomic state
                                        // replacement can briefly make the read miss;
                                        // leaving it unset retries that version next poll.
                                        last_mtime = Some(mtime);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            {
                use std::hash::{Hash, Hasher};
                let snap = crate::llm::StreamBuffer::snapshot(&stream);
                let think = crate::llm::StreamBuffer::snapshot_thinking(&stream);
                let visible = stream.try_lock().map(|b| b.text_visible).unwrap_or(false);
                let mut h = std::collections::hash_map::DefaultHasher::new();
                snap.hash(&mut h);
                think.hash(&mut h);
                visible.hash(&mut h);
                let fp = h.finish();
                if fp != last_stream_fp {
                    last_stream_fp = fp;
                    let frame = serde_json::json!({
                        "wire": "stream",
                        "text": snap,
                        "thinking": think,
                        "text_visible": visible,
                    });
                    if let Ok(json) = serde_json::to_string(&frame) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            if let Some(terms) = terms.as_ref() {
                use base64::Engine;
                // Incremental PTY bytes only. Replaying raw scrollback into a
                // sized vt100 (tmux/ghostty-style: emulator owns the grid;
                // clients get live bytes or a cell snapshot, never CSI history)
                // is what glued `ls` onto the fish prompt.
                let _ = terms.take_snapshots();
                let frames = match term_client.as_ref() {
                    Some(c) => terms.poll_client(c),
                    None => terms.poll_all(),
                };
                for (id, chunk, cols, rows, alive) in frames {
                    if chunk.is_empty() && alive {
                        continue;
                    }
                    term_seq = term_seq.wrapping_add(1);
                    let frame = serde_json::json!({
                        "wire": "term",
                        "op": "out",
                        "id": id,
                        "seq": term_seq,
                        "data": base64::engine::general_purpose::STANDARD.encode(&chunk),
                        "cols": cols,
                        "rows": rows,
                        "alive": alive,
                    });
                    if let Ok(json) = serde_json::to_string(&frame) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });

    // Each idempotent client input may carry a nonce. On reconnect the mobile
    // client resends with the same nonce so the server drops duplicates.
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                // Parse as raw Value first to extract the nonce without
                // changing the LoopInput serde format.
                let dominated = serde_json::from_str::<serde_json::Value>(t.as_str());
                if let Ok(val) = dominated {
                    if val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        if let Some(terms) = terms.as_ref() {
                            apply_term_client(terms, &val);
                        }
                        continue;
                    }
                    if let Some(nonce) = val.get("nonce").and_then(|n| n.as_str()) {
                        if let Some(kind) = val.get("kind").and_then(|k| k.as_str()) {
                            // User messages and approval/question answers are both
                            // retried across reconnects and must be idempotent.
                            let idempotent = matches!(
                                kind,
                                "user_message" | "answer" | "approve" | "approve_all" | "deny"
                            );
                            if idempotent && !daemon.accept_nonce(&session, nonce, &state_path) {
                                continue; // duplicate — drop silently
                            }
                        }
                    }
                }
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

fn apply_term_client(terms: &crate::term::SessionTerms, val: &serde_json::Value) {
    use base64::Engine;
    let op = val.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
    let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let id = val
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    match op {
        "open" | "new" => {
            // Honor the client's pane id. Allocating a different one left the
            // TUI painting an empty pane while output landed on an unseen id.
            let id = if op == "new" && id.is_empty() {
                terms.alloc_id()
            } else {
                id
            };
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
                // Do not snapshot raw scrollback — replaying it into vt100
                // at the current size scrambles fish/zsh/bash prompts.
            }
        }
        "resize" => {
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
            }
        }
        "in" => {
            // Typing must spawn the pane if `open`/`new` never landed
            // (blank Ctrl-N tab). Dropping `in` on a missing id is why
            // keys look captured but never paint.
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                if let Some(data) = val.get("data").and_then(|v| v.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        term.write(&bytes);
                    }
                }
            }
        }
        "close" => terms.close(&id),
        _ => {}
    }
}

// WS /events — device-wide notification firehose. Emits a compact event whenever a
// session leaves the running state (asked a question / needs approval / stopped /
// errored), so the app can notify even for sessions it isn't actively watching.
async fn events_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    ws.on_upgrade(move |socket| handle_events_ws(socket, d))
}

async fn handle_events_ws(socket: WebSocket, daemon: Shared) {
    use std::collections::{HashMap, HashSet};
    let (mut sender, mut receiver) = socket.split();
    let push = tokio::spawn(async move {
        let mut last: HashMap<String, String> = HashMap::new();
        let mut first = true;
        loop {
            // Re-read the notification policy every tick so setting changes and
            // MC-session re-opens apply to already-connected clients.
            let settings = mission_control::load_settings(&daemon.mission_control_root);
            let mission_id = settings.mission_control_session_id;
            let suppress_workers = settings.notification_policy == "mission_control_only";
            let suppress_all = settings.notification_policy == "none";
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
                if suppress_all
                    || (suppress_workers && mission_id.as_deref() != Some(s.id.as_str()))
                {
                    continue;
                }
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
            // BEL / OSC 9 / OSC 777. Harvest also drains idle PTYs so a
            // notification still fires when nobody is attached. Bytes drained
            // with no subscriber are stashed for the next /attach.
            {
                let live = daemon.sessions.lock().await;
                for (id, sess) in live.iter() {
                    let notes = sess.terms.harvest();
                    if notes.is_empty() {
                        continue;
                    }
                    let meta = sessions.iter().find(|s| s.id == *id);
                    let title = meta.map(|s| s.title.as_str()).unwrap_or("");
                    let folder = meta.map(|s| s.folder.as_str()).unwrap_or("");
                    let status = meta.map(|s| s.status.as_str()).unwrap_or("");
                    for n in notes {
                        out.push(serde_json::json!({
                            "session": id,
                            "title": title,
                            "workspace": folder,
                            "kind": "term",
                            "status": status,
                            "message": n.message,
                            "pane": n.pane,
                        }));
                    }
                }
            }
            for e in out {
                if sender
                    .send(Message::Text(e.to_string().into()))
                    .await
                    .is_err()
                {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_daemon() -> Daemon {
        Daemon {
            config: std::sync::Mutex::new(SnippetConfig::default()),
            config_path: PathBuf::new(),
            token: String::new(),
            hostname: String::from("test"),
            sessions: Mutex::new(HashMap::new()),
            git_write: Mutex::new(()),
            browser: BrowserManager::default(),
            seen_nonces: std::sync::Mutex::new(HashMap::new()),
            mission_control_root: tempfile::tempdir().expect("temporary directory").keep(),
        }
    }

    #[test]
    fn nonce_is_rejected_after_daemon_state_is_recreated() {
        let dir = tempdir().expect("temporary directory");
        let state_path = dir.path().join("state.json");
        let first = test_daemon();

        assert!(first.accept_nonce("session", "nonce-1", &state_path));
        assert!(!first.accept_nonce("session", "nonce-1", &state_path));

        let restarted = test_daemon();
        assert!(!restarted.accept_nonce("session", "nonce-1", &state_path));
        assert!(restarted.accept_nonce("session", "nonce-2", &state_path));
    }
}

#[derive(Deserialize)]
struct MissionListQuery {
    token: Option<String>,
    archived: Option<bool>,
}

#[derive(Deserialize)]
struct MissionTaskReq {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    session_id: String,
    /// `resume` (default) or `fresh` — how the handoff should be delivered.
    #[serde(default)]
    handoff_mode: Option<String>,
    #[serde(default)]
    owned_paths: Vec<String>,
    #[serde(default)]
    status: Option<String>,
}

/// Validate caller-supplied owned paths against a managed workspace: each must
/// resolve inside it. Returns the canonicalised list, defaulting to the whole
/// workspace when nothing valid is supplied. A claimed path may not exist yet
/// (the task will create it), so non-existent paths are resolved against their
/// nearest existing ancestor; the workspace is canonicalised once so symlinked
/// workspaces compare correctly.
pub fn validated_owned_paths(
    raw_paths: &[String],
    workspace: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut out = Vec::new();
    for raw in raw_paths {
        let path = std::path::PathBuf::from(raw);
        // Walk up to the nearest ancestor that exists, canonicalise that, and
        // re-append the non-existent remainder.
        let mut prefix = path.clone();
        let mut suffix = Vec::new();
        let resolved = loop {
            match prefix.canonicalize() {
                Ok(canonical) => {
                    let mut resolved = canonical;
                    for part in suffix.iter().rev() {
                        resolved.push(part);
                    }
                    break resolved;
                }
                Err(_) => match prefix.parent() {
                    Some(parent) => {
                        if let Some(name) = prefix.file_name() {
                            suffix.push(name.to_os_string());
                        }
                        prefix = parent.to_path_buf();
                    }
                    // Reached the filesystem root without finding anything
                    // that exists — treat as outside the workspace.
                    None => break path.clone(),
                },
            }
        };
        if resolved.starts_with(&root) {
            out.push(resolved);
        } else {
            return Err(format!(
                "owned path '{raw}' is outside the target session's workspace"
            ));
        }
    }
    if out.is_empty() {
        out.push(root);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct MissionSessionReq {
    #[serde(default)]
    session_id: String,
    folder: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Deserialize)]
struct MissionSessionUpdate {
    #[serde(default)]
    title: Option<String>,
}

fn task_status(raw: &str) -> Option<TaskStatus> {
    match raw {
        "pending" => Some(TaskStatus::Pending),
        "in_progress" => Some(TaskStatus::InProgress),
        "blocked" => Some(TaskStatus::Blocked),
        "done" | "completed" => Some(TaskStatus::Done),
        "failed" => Some(TaskStatus::Failed),
        "cancelled" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

fn mission_task_view(task: &TaskRecord) -> serde_json::Value {
    serde_json::json!({
        "id": task.id,
        "title": task.title,
        "description": task.description,
        "status": task.status,
        "session_id": task.session_id,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "archived": task.status.is_terminal(),
        "owned_paths": task.owned_paths,
        "dependencies": task.dependencies,
        "handoff": task.handoff,
        "result": task.result,
        "notifications": task.notifications,
    })
}

fn mission_session_view(session: &ManagedSession, task_count: usize) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "session_id": session.id,
        "folder": session.workspace,
        "title": session.label,
        "status": session.status,
        "created_at": session.created_at,
        "last_active_at": session.updated_at,
        "task_count": task_count,
        "archived": matches!(session.status, mission_control::SessionStatus::Archived),
        "metadata": session.tags,
    })
}

fn mission_error(error: String) -> Response {
    (StatusCode::BAD_REQUEST, error).into_response()
}

async fn mission_control_overview(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    let Ok(tasks) = mission_control::list_tasks(root, None, None) else {
        return mission_error("could not read Mission Control tasks".to_string());
    };
    let Ok(sessions) = mission_control::list_sessions(root, false) else {
        return mission_error("could not read Mission Control sessions".to_string());
    };
    let active_tasks = tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    let done_tasks = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Done)
        .count();
    let active_sessions = sessions
        .iter()
        .filter(|session| session.status == mission_control::SessionStatus::Active)
        .count();
    let recent_tasks = tasks
        .iter()
        .rev()
        .take(12)
        .map(mission_task_view)
        .collect::<Vec<_>>();
    let recent_sessions = sessions
        .iter()
        .rev()
        .take(12)
        .map(|session| {
            let count = tasks
                .iter()
                .filter(|task| task.session_id == session.id)
                .count();
            mission_session_view(session, count)
        })
        .collect::<Vec<_>>();
    let mc_session_id = Some(mission_control::SESSION_ID);
    Json(serde_json::json!({
        "active_tasks": active_tasks,
        "completed_tasks": done_tasks,
        "total_tasks": tasks.len(),
        "active_sessions": active_sessions,
        "total_sessions": sessions.len(),
        "recent_tasks": recent_tasks,
        "recent_sessions": recent_sessions,
        "mc_session_id": mc_session_id,
    }))
    .into_response()
}

async fn mission_control_tasks(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match mission_control::list_tasks(&d.mission_control_root, None, None) {
        Ok(tasks) => Json(
            tasks
                .iter()
                .filter(|task| {
                    q.archived
                        .is_none_or(|archived| archived == task.status.is_terminal())
                })
                .map(mission_task_view)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => mission_error(error),
    }
}

async fn dispatch_mission_task(d: &Daemon, task_id: &str) -> Result<TaskRecord, String> {
    let root = &d.mission_control_root;
    // Atomic claim: Pending → InProgress exactly once. Concurrent dispatchers
    // (loop + REST paths) lose the race here and return without delivering.
    let task = mission_control::claim_task_for_dispatch(root, task_id)?;
    if task.status != TaskStatus::InProgress || task.reporting_session.is_none() {
        // Not claimed by us (already in progress/terminal) — nothing to do.
        return Ok(task);
    }
    if task.session_id.trim().is_empty() {
        release_failed_claim(root, &task, "task has no target session")?;
        return Err("task has no target session".to_string());
    }
    for dependency in &task.dependencies {
        // A missing/unreadable dependency record must roll the claim back like
        // every other failure path — otherwise the task strands InProgress.
        let other = match mission_control::get_task(root, &dependency.task_id) {
            Ok(other) => other,
            Err(error) => {
                release_failed_claim(root, &task, &error)?;
                return Err(error);
            }
        };
        if other.status != TaskStatus::Done {
            let reason = format!("waiting on dependency {}", dependency.task_id);
            let blocked = mission_control::update_task(root, task_id, |task| {
                task.status = TaskStatus::Blocked;
                task.reporting_session = None;
                task.notifications
                    .push(mission_control::NotificationMarker {
                        target: "mission_control".to_string(),
                        kind: "blocked".to_string(),
                        message: reason.clone(),
                        delivered: false,
                    });
            })?;
            // Report the stored (blocked) state, not the pre-update claim.
            return Ok(blocked);
        }
    }
    let managed = match mission_control::get_session(root, &task.session_id) {
        Ok(managed) => managed,
        Err(error) => {
            release_failed_claim(root, &task, &error)?;
            return Err(error);
        }
    };
    if managed.status != mission_control::SessionStatus::Active {
        let error = "target session is archived".to_string();
        release_failed_claim(root, &task, &error)?;
        return Err(error);
    }
    let conflicts = mission_control::detect_conflicts(root, task_id, &task.owned_paths)?;
    if !conflicts.is_empty() {
        let error = format!(
            "workspace paths are owned by active task(s): {}",
            conflicts
                .iter()
                .map(|conflict| conflict.existing_task_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        release_failed_claim(root, &task, &error)?;
        return Err(error);
    }
    let handoff = task
        .handoff
        .as_ref()
        .map(|handoff| handoff.description.as_str())
        .unwrap_or("");
    // Fresh-mode handoffs must be self-contained briefings; resume-mode ones
    // assume the session already holds the context.
    let mode_line = match task.handoff_mode {
        mission_control::HandoffMode::Fresh => {
            "handoff_mode: fresh (self-contained briefing; the session has no prior context)\n"
        }
        mission_control::HandoffMode::Resume => "",
    };
    let text = format!(
        "[mission_control_task]\ntask_id: {}\ntitle: {}\n{}scope: {}\nworkspace: {}\nexpected_report: scope done; files changed; verification; blockers\nrules: do not confirm scope; begin immediately. If handoff_mode is fresh, this envelope is the complete briefing — do not ask for missing history. Stay in this session — it already has the context; do not spawn lanes unless the work is independently parallel. Stay in scope; do not manage other sessions. If a prior read-only report is gone, redo the evaluation from current sources and deliver it. When done/blocked/failed, call report_mission_task for task_id {} — it is bound to this session. Block only for a missing unique artifact or user decision, not compacted history.\n[/mission_control_task]",
        task.id,
        task.title,
        mode_line,
        if handoff.is_empty() {
            task.description.as_str()
        } else {
            handoff
        },
        managed.workspace.display(),
        task.id,
    );
    d.deliver(&managed.id, LoopInput::UserMessage(text)).await;
    // Delivery succeeded — clear the retry counter so the ceiling stays
    // "consecutive failures" as documented.
    mission_control::update_task(root, task_id, |t| t.dispatch_failures = 0)?;
    mission_control::get_task(root, task_id)
}

/// Roll a claimed-but-undeliverable task back to Pending and record why, so
/// the loop can retry later instead of silently spinning with a stale claim.
/// After `MAX_DISPATCH_FAILURES` consecutive failures the task is parked as
/// Blocked so it stops consuming the loop.
fn release_failed_claim(
    root: &std::path::Path,
    task: &TaskRecord,
    error: &str,
) -> Result<(), String> {
    const MAX_DISPATCH_FAILURES: u32 = 5;
    mission_control::update_task(root, &task.id, |t| {
        t.dispatch_failures += 1;
        t.reporting_session = None;
        if t.dispatch_failures >= MAX_DISPATCH_FAILURES {
            t.status = TaskStatus::Blocked;
            t.notifications.push(NotificationMarker {
                target: "mission_control".to_string(),
                kind: "blocked".to_string(),
                message: format!("dispatch failed {} times: {error}", t.dispatch_failures),
                delivered: false,
            });
        } else {
            t.status = TaskStatus::Pending;
        }
    })
    .map(|_| ())
}

async fn mission_control_create_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionTaskReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.title.trim().is_empty() || req.session_id.trim().is_empty() {
        return mission_error("title and session_id are required".to_string());
    }
    let root = &d.mission_control_root;
    let managed = match mission_control::get_session(root, &req.session_id) {
        Ok(session) => session,
        Err(error) => return mission_error(error),
    };
    // Every owned path must stay inside the session's workspace; otherwise a
    // caller could claim "/" or another project's tree and block all tasks.
    let owned_paths = match validated_owned_paths(&req.owned_paths, &managed.workspace) {
        Ok(paths) => paths,
        Err(error) => return mission_error(error),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let task = match mission_control::create_task(
        root,
        &id,
        &req.session_id,
        req.title.trim(),
        req.description.trim(),
        owned_paths,
    ) {
        Ok(task) => task,
        Err(error) => return mission_error(error),
    };
    let task = if req.status.as_deref() == Some("pending") {
        task
    } else {
        match dispatch_mission_task(&d, &task.id).await {
            Ok(task) => task,
            Err(error) => match mission_control::update_task(root, &task.id, |task| {
                task.status = TaskStatus::Blocked;
                task.reporting_session = None;
                task.notifications
                    .push(mission_control::NotificationMarker {
                        target: "mission_control".to_string(),
                        kind: "blocked".to_string(),
                        message: error,
                        delivered: false,
                    });
            }) {
                Ok(task) => task,
                Err(error) => return mission_error(error),
            },
        }
    };
    Json(mission_task_view(&task)).into_response()
}

async fn mission_control_update_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<MissionTaskReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    // Resolve the existing task up-front so owned-path updates can be
    // validated against the *effective* target session's workspace: the
    // session named in the request when retargeting, else the current one.
    let existing = match mission_control::get_task(root, &id) {
        Ok(task) => task,
        Err(error) => return mission_error(error),
    };
    let effective_session_id = if req.session_id.trim().is_empty() {
        existing.session_id.clone()
    } else {
        req.session_id.trim().to_string()
    };
    let workspace_for_paths: std::path::PathBuf =
        match mission_control::get_session(root, &effective_session_id) {
            Ok(session) => session.workspace,
            Err(error) => return mission_error(error),
        };
    let validated_paths = if req.owned_paths.is_empty() {
        None
    } else {
        Some(
            match validated_owned_paths(&req.owned_paths, &workspace_for_paths) {
                Ok(paths) => paths,
                Err(error) => return mission_error(error),
            },
        )
    };
    let status = req.status.as_deref().and_then(task_status);
    let handoff_mode = match req.handoff_mode.as_deref() {
        None => None,
        Some(mode_raw) => match mission_control::HandoffMode::parse(mode_raw) {
            Some(mode) => Some(mode),
            None => {
                return mission_error(format!(
                    "handoff_mode must be 'resume' or 'fresh', got '{mode_raw}'"
                ));
            }
        },
    };
    let updated = mission_control::update_task(root, &id, |task| {
        if !req.title.trim().is_empty() {
            task.title = req.title.trim().to_string();
        }
        if !req.description.trim().is_empty() {
            task.description = req.description.trim().to_string();
        }
        if !req.session_id.trim().is_empty() && req.session_id.trim() != existing.session_id {
            task.session_id = req.session_id.trim().to_string();
            task.reporting_session = None; // re-bind on retarget
        }
        if let Some(paths) = &validated_paths {
            task.owned_paths = paths.clone();
        }
        if let Some(mode) = handoff_mode {
            task.handoff_mode = mode;
        }
        if let Some(status) = status {
            task.status = status;
            if status.is_terminal() || status == TaskStatus::Blocked {
                task.reporting_session = None;
            }
        }
    });
    match updated {
        Ok(task) if task.status == TaskStatus::Pending => {
            match dispatch_mission_task(&d, &id).await {
                Ok(task) => Json(mission_task_view(&task)).into_response(),
                Err(error) => mission_error(error),
            }
        }
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error),
    }
}

async fn mission_control_archive_task(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::complete_task(
        &d.mission_control_root,
        &id,
        TaskStatus::Cancelled,
        mission_control::TaskResult {
            summary: "Archived by Mission Control.".to_string(),
            ..Default::default()
        },
    ) {
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error),
    }
}

async fn mission_control_sessions(
    State(d): State<Shared>,
    Query(q): Query<MissionListQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let root = &d.mission_control_root;
    let tasks = mission_control::list_tasks(root, None, None).unwrap_or_default();
    match mission_control::list_sessions(root, false) {
        Ok(sessions) => Json(
            sessions
                .iter()
                .filter(|session| {
                    q.archived.is_none_or(|archived| {
                        archived
                            == matches!(session.status, mission_control::SessionStatus::Archived)
                    })
                })
                .map(|session| {
                    mission_session_view(
                        session,
                        tasks
                            .iter()
                            .filter(|task| task.session_id == session.id)
                            .count(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => mission_error(error),
    }
}

async fn mission_control_create_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionSessionReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let folder = PathBuf::from(&req.folder);
    if !folder.is_dir() {
        return mission_error("folder is not a directory".to_string());
    }
    let session_id = if req.session_id.trim().is_empty() {
        let base_state = {
            d.config
                .lock()
                .unwrap()
                .for_workspace(folder.clone())
                .state_path
        };
        let path = base_state
            .parent()
            .map(|parent| {
                parent
                    .join("conversations")
                    .join(format!("{}.json", uuid::Uuid::new_v4()))
            })
            .unwrap_or(base_state);
        let id = path
            .strip_prefix(workspaces_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        let cfg = {
            let config = d.config.lock().unwrap();
            let mut workspace = config.for_workspace(folder.clone());
            apply_profile(&mut workspace, &req.profile);
            workspace
        };
        let handle = start_session_with_browser_summary(
            &cfg,
            path,
            None,
            false,
            Some(Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(d.browser.summary_provider()),
        );
        let mut live = d.sessions.lock().await;
        live.insert(id.clone(), live_from_handle(handle, req.profile.clone()));
        id
    } else {
        req.session_id.trim().to_string()
    };
    let label = if req.title.trim().is_empty() {
        "Managed session"
    } else {
        req.title.trim()
    };
    let root = &d.mission_control_root;
    let session = match mission_control::create_session(root, &session_id, label, &folder) {
        Ok(session) => session,
        Err(error) => return mission_error(error),
    };
    Json(mission_session_view(&session, 0)).into_response()
}

async fn mission_control_update_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<MissionSessionUpdate>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::update_session(&d.mission_control_root, &id, |session| {
        if let Some(title) = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
        {
            session.label = title.to_string();
        }
    }) {
        Ok(session) => Json(mission_session_view(&session, 0)).into_response(),
        Err(error) => mission_error(error),
    }
}

async fn mission_control_archive_session(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::archive_session(&d.mission_control_root, &id) {
        Ok(session) => Json(mission_session_view(&session, 0)).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct MissionOpenReq {
    #[serde(default)]
    profile: Option<String>,
}

async fn mission_control_open(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionOpenReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let home = match mission_control::ensure_home() {
        Ok(path) => path,
        Err(error) => return mission_error(error),
    };
    let state_path = mission_control::session_state_path();
    let id = mission_control::SESSION_ID.to_string();
    let resume = state_path.exists();
    let profile = req
        .profile
        .clone()
        .or_else(|| read_session_profile(&state_path));
    write_session_role(&state_path, "mission_control");
    let cfg = {
        let config = d.config.lock().unwrap();
        let mut workspace = config.for_workspace(home.clone());
        apply_profile(&mut workspace, &profile);
        workspace
    };
    let mut sessions = d.sessions.lock().await;
    if !sessions.contains_key(&id) {
        let handle = start_mission_control_session(
            &cfg,
            state_path,
            None,
            resume,
            Some(Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            ))),
            Some(d.browser.summary_provider()),
        );
        sessions.insert(id.clone(), live_from_handle(handle, profile));
        if !resume {
            if let Some(live) = sessions.get(&id) {
                let _ = live
                    .input_tx
                    .send(LoopInput::SetTitle("Mission Control".into()));
            }
        }
    }
    if let Err(error) = mission_control::set_mission_control_session(&d.mission_control_root, &id) {
        eprintln!("[mission-control] failed to persist active MC session: {error}");
    }
    Json(serde_json::json!({ "id": id, "folder": home })).into_response()
}

/// Persist the MC session id next to the session's state file so a daemon
/// restart can resume it with the Mission Control role (prompt + tools +
/// lane restrictions) instead of silently downgrading it to an ordinary
/// conversation session.
const MC_ROLE_SIDECAR_SUFFIX: &str = ".role";

fn write_session_role(state_path: &std::path::Path, role: &str) {
    let sidecar = PathBuf::from(format!("{}{MC_ROLE_SIDECAR_SUFFIX}", state_path.display()));
    if let Some(parent) = sidecar.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(sidecar, role);
}

pub fn read_session_role(state_path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(PathBuf::from(format!(
        "{}{MC_ROLE_SIDECAR_SUFFIX}",
        state_path.display()
    )))
    .ok()?;
    let role = raw.trim().to_string();
    (!role.is_empty()).then_some(role)
}

/// Dispatch loop: claims and delivers every Pending task, then re-queues any
/// Blocked tasks whose dependencies have completed. Failures roll the claim
/// back with a retry counter; at the ceiling the task parks as Blocked.
async fn mission_control_dispatch_loop(daemon: Shared) {
    loop {
        let root = &daemon.mission_control_root;
        let _ = mission_control::unblock_ready_tasks(root);
        let tasks =
            mission_control::list_tasks(root, None, Some(TaskStatus::Pending)).unwrap_or_default();
        for task in tasks {
            if let Err(error) = dispatch_mission_task(&daemon, &task.id).await {
                tracing_log_dispatch_failure(&task.id, &error);
            }
        }
        deliver_mission_control_reports(&daemon).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Worker `report_mission_task` only writes the JSON store. Push undelivered
/// markers into the Mission Control conversation so the user sees the result.
async fn deliver_mission_control_reports(daemon: &Daemon) {
    let root = &daemon.mission_control_root;
    let Ok(tasks) = mission_control::list_tasks(root, None, None) else {
        return;
    };
    for task in tasks {
        for (index, marker) in task.notifications.iter().enumerate() {
            if marker.delivered || marker.target != "mission_control" {
                continue;
            }
            let text = format!(
                "[mission_task_report]\ntask_id: {}\ntitle: {}\nstatus: {}\nsummary: {}\n[/mission_task_report]",
                task.id,
                task.title,
                marker.kind,
                marker.message
            );
            daemon
                .deliver(
                    mission_control::SESSION_ID,
                    LoopInput::UserMessage(text),
                )
                .await;
            let _ = mission_control::mark_notification_delivered(root, &task.id, index);
        }
    }
}

fn tracing_log_dispatch_failure(task_id: &str, error: &str) {
    eprintln!("[mission-control] dispatch {task_id} failed: {error}");
}

#[derive(Deserialize)]
struct MissionSettingsReq {
    notification_policy: String,
}

async fn mission_control_settings(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(mission_control::load_settings(&d.mission_control_root)).into_response()
}

async fn mission_control_update_settings(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<MissionSettingsReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match mission_control::set_notification_policy(
        &d.mission_control_root,
        &req.notification_policy,
    ) {
        Ok(settings) => Json(settings).into_response(),
        Err(error) => mission_error(error),
    }
}
