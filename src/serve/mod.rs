//! Headless control daemon. Runs alongside (never replacing) the TUI: it manages
//! sessions across the device and exposes them over HTTP + WebSocket so a remote
//! client (mobile app) can browse folders, open a session in any folder, list every
//! session on the box, and stream/drive one. Every endpoint is token-authed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::config::{InferenceProfileConfig, SnippetConfig, save_config, workspaces_root};
use crate::harness::{HarnessEvent, LoopInput};
use crate::mission_control as mc;
use crate::session::{
    list_device_sessions,
    read_session_profile, read_session_sidecar, read_session_meta, read_session_state, read_session_state_tail, replay_notification_events,
    session_id_for_state_path, start_session_with_browser_summary,
    state_path_for_id, subscribe_device_events, write_session_profile,
    SessionRole,
};

pub mod agent_routes;
pub(crate) use agent_routes::*;
mod browser;
pub mod config_routes;
pub(crate) use config_routes::*;
mod coordination;
pub use coordination::{COORDINATION_THREAD, queue_coordination_wake};
mod direct;
mod fs;
mod git;
mod lifecycle;
pub mod maintenance;
pub(crate) use maintenance::*;
mod mission_control;
pub use mission_control::{mission_error, validated_owned_paths};
mod recurring;
pub mod session_routes;
pub(crate) use session_routes::*;
use crate::term::SessionTerms;
pub mod sidecar;
mod transcribe;
mod tunnel;
pub mod ws;
pub(crate) use ws::*;

pub use self::lifecycle::*;
pub use self::tunnel::ensure_cloudflared_foreground;

use self::browser::{BrowserManager, RegisterMessage};
use self::fs::*;
use self::git::*;
use self::tunnel::{ensure_cloudflared, start_cloudflared_quick};

pub(crate) struct LiveSession {
    input_tx: UnboundedSender<LoopInput>,
    join: JoinHandle<Result<crate::harness::HarnessState, String>>,
    state_path: PathBuf,
    /// The profile this session's model was built from (per-conversation override,
    /// in-memory only — reverts to the global active profile on daemon restart).
    profile: Option<String>,
    /// Live token stream for attached clients (TUI/mobile). Shared with the
    /// harness so /attach can push partial answer/thinking without waiting for
    /// the next state write.
    stream: crate::llm::StreamHandle,
    /// Interactive human PTYs for this session (not used by the agent bash tool).
    terms: Arc<SessionTerms>,
}

pub(crate) fn live_from_handle(handle: crate::session::SessionHandle, profile: Option<String>) -> LiveSession {
    let cwd = {
        let from_state = read_session_meta(&handle.state_path)
            .map(|s| PathBuf::from(s.workspace))
            .filter(|p| p.is_dir());
        from_state
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
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
pub(crate) fn apply_profile(cfg: &mut SnippetConfig, profile: &Option<String>) {
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
    // Store-or-file: a session ported to the database has no state file, so
    // reading the file directly would 404 every migrated session.
    let Some(state) = read_session_meta(&sp) else {
        return Err((StatusCode::NOT_FOUND, "session state unreadable").into_response());
    };
    let folder = PathBuf::from(&state.workspace);
    if state.workspace.is_empty() || !folder.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "session workspace missing").into_response());
    }
    Ok((sp, folder))
}

pub(crate) struct Daemon {
    config: std::sync::Mutex<SnippetConfig>,
    config_path: PathBuf,
    token: String,
    hostname: String,
    sessions: Mutex<HashMap<String, LiveSession>>,
    /// Daemon-wide interactive shells, NOT tied to any session.
    ///
    /// A shell belongs to the machine: switching or closing a session must not
    /// kill it. The pty machinery was already sessionless (`SessionTerms::new`);
    /// only the transport was not, because `/attach` requires a live session.
    /// `/shells` exposes the same `wire: term` frames over a socket that needs no
    /// session at all.
    shells: Arc<crate::term::SessionTerms>,
    /// Serializes git WRITE operations daemon-wide so a user's git action can't
    /// race the agent's edits (or another git write) on the same index.
    git_write: Mutex<()>,
    /// Connected browser-extension sockets and their pending command waiters.
    browser: BrowserManager,
    /// Recent idempotency nonces for inbound client inputs: `"session:nonce"` →
    /// first-seen time. Prevents duplicate user messages and decision retries when
    /// the mobile client resends after a reconnect.
    seen_nonces: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// Queue entries removed/steered while an in-flight step still owns the
    /// harness state. The attach stream applies this shared overlay immediately
    /// for every connected client; the harness later persists the real removal.
    queue_hidden: std::sync::Mutex<HashMap<String, Vec<String>>>,
    queue_revision: AtomicU64,
    mission_control_root: PathBuf,
    store: crate::store::Store,
    coordination_events:
        tokio::sync::broadcast::Sender<crate::coordination::types::CoordinationEvent>,
    recurring_root: PathBuf,
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

/// Working directory for daemon-wide shells.
///
/// Deliberately the daemon's own cwd (`~` when started by the service manager),
/// NOT a session workspace: a global shell belongs to the machine, so it must
/// not silently follow whichever session happened to be open. A session that
/// wants its own workspace shell already gets one through its `wire: term`.
fn global_shell_cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

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

    /// Claim a client request id, returning false when it is a replay.
    ///
    /// Two levels, both in-process or in the store: an in-memory map for the
    /// current run, and `request_nonces` for anything that outlives it. There
    /// used to be a third — a session's `.nonces.json` sidecar, read as a
    /// compatibility fallback from when sessions lived in files. It is gone:
    /// the files held nothing the store did not, the entries were provably
    /// older than every stored one, and a missing file read returning `None`
    /// meant the fallback could rot silently without failing a single test.
    fn accept_nonce(&self, session_id: &str, nonce: &str) -> bool {
        let key = format!("{session_id}:{nonce}");
        let mut map = self.seen_nonces.lock().unwrap();
        if map.contains_key(&key) {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let Ok(inserted) = self.store.claim_request_nonce(session_id, nonce, now) else {
            return false;
        };
        if !inserted {
            map.insert(key, std::time::Instant::now());
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
        // Read through the store-or-file reader: a session ported to the
        // database has no state file, and reading the file directly would make
        // every such session unopenable.
        let state = read_session_meta(&sp)?;
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
        // Role-aware resume. ONE place decides the role — see `start_role_aware`.
        // A session opened as Mission Control must come back with the MC
        // prompt/tools/lane restrictions, and an agent session with its
        // specialized identity, rather than silently downgrading either.
        let handle = self.start_role_aware(&cfg, sp.clone(), None).await;
        let tx = handle.input_tx.clone();
        let live = live_from_handle(handle, profile);
        let stream = live.stream.clone();
        sessions.insert(id.to_string(), live);
        Some((tx, sp, stream))
    }

    /// Start (or resume) a session with the ROLE it was created with.
    ///
    /// The single place that decides role. Two callers need it and previously
    /// only one had it: `ensure_live` dispatched on the `.role` sidecar, while
    /// `set_session_model` always started a plain session. Switching the model
    /// mid-conversation therefore DOWNGRADED a Mission Control or agent session
    /// to an ordinary one — losing the MC prompt/tools/lane restrictions until
    /// the next daemon restart restored them from the sidecar.
    ///
    /// Falls back to a plain session whenever the role's home is unavailable, so
    /// a missing identity can never wedge a session.
    async fn start_role_aware(
        &self,
        cfg: &SnippetConfig,
        sp: PathBuf,
        initial: Option<String>,
    ) -> crate::session::SessionHandle {
        // Cloned per call: a `StreamHandle` is an `Arc<Mutex<StreamBuffer>>`, so
        // cloning is a refcount bump, and each call site needs its own value
        // because the match arms move it.
        let stream = || {
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::llm::StreamBuffer::default(),
            )))
        };
        // One reader, one shape. The arms are on the enum rather than on a
        // hand-parsed string, so a grammar this function does not know about is
        // now impossible to write.
        let sidecar = read_session_sidecar(&sp);
        if let Some(sidecar) = sidecar.as_ref()
            && sidecar.role == SessionRole::MissionControl
        {
            return crate::session::start_mission_control_session(
                cfg,
                sp,
                initial,
                true,
                stream(),
                Some(self.browser.summary_provider()),
            );
        }
        if let Some(agent_id) = sidecar.as_ref().and_then(|s| s.agent_id.as_deref()) {
            // Identity revision is re-read on every resume, so an agent whose
            // identity was updated comes back with the new guidance.
            match crate::coordination::AgentHome::new(
                crate::coordination::agents_root(&self.mission_control_root),
                agent_id,
            ) {
                // An agent's INBOX is its coordination session: it answers direct
                // messages and dispatches work, with no workspace tools. The same
                // agent runs the full coding runtime in a work session, so the
                // choice is made on the session, not on the agent.
                Ok(home)
                    if crate::session::is_inbox_session_id(&session_id_for_state_path(&sp)) =>
                {
                    match crate::session::start_specialized_coordination_session(
                        cfg,
                        sp.clone(),
                        initial.clone(),
                        true,
                        stream(),
                        Some(self.browser.summary_provider()),
                        home,
                    ) {
                        Ok(handle) => return handle,
                        Err(error) => {
                            eprintln!(
                                "[coordination] agent `{agent_id}` coordination identity \
                                 unavailable ({error}); running as an ordinary session"
                            );
                        }
                    }
                }
                Ok(home) => match crate::session::start_specialized_agent_session(
                    cfg,
                    sp.clone(),
                    initial.clone(),
                    true,
                    stream(),
                    Some(self.browser.summary_provider()),
                    home,
                ) {
                    Ok(handle) => return handle,
                    Err(error) => {
                        eprintln!(
                            "[coordination] agent `{agent_id}` identity unavailable ({error}); \
                             running as an ordinary session"
                        );
                    }
                },
                Err(_) => {}
            }
        }
        start_session_with_browser_summary(
            cfg,
            sp,
            initial,
            true,
            stream(),
            Some(self.browser.summary_provider()),
        )
    }

    /// Run a session on a named inference profile, restarting its loop.
    ///
    /// A model is bound when the loop STARTS (`build_model_for_session`), and
    /// nothing switches it per turn. So changing the profile of a live session
    /// means replacing the loop — that is why this is shared with
    /// `POST /session/model` rather than applied in place.
    ///
    /// Stops the old loop first, so its in-flight turn is abandoned rather than
    /// left generating on a model the caller just replaced.
    ///
    /// `persist` decides whether this becomes the session's own model or only
    /// this run's. They are different questions:
    ///
    /// - `POST /session/model` — the user is pinning THIS chat to a model. The
    ///   choice outlives the run, so it is written to `sessions.profile` and
    ///   survives a daemon restart or a resume.
    /// - A dispatch — Mission Control is choosing a model for ONE assignment.
    ///   The next task may want a different one, and the session's own default
    ///   is not the dispatcher's to overwrite. So the profile is in-memory only:
    ///   it drives this run, and a later start falls back to whatever the
    ///   session was already set to.
    pub(crate) async fn run_session_on_profile(
        &self,
        session: &str,
        profile: &str,
        persist: bool,
    ) -> Result<(), String> {
        self.reload_config().await; // a profile added in the TUI must be usable
        let model_cfg = {
            let c = self.config.lock().unwrap();
            c.setups
                .as_ref()
                .and_then(|m| m.get(profile))
                .cloned()
                .ok_or_else(|| format!("no such profile `{profile}`"))?
        };
        let (sp, folder) =
            load_session_workspace(session).map_err(|_| format!("unknown session `{session}`"))?;
        let cfg = {
            let c = self.config.lock().unwrap();
            let mut w = c.for_workspace(folder);
            w.model = model_cfg;
            w.active_setup = Some(profile.to_string());
            w
        };
        let mut sessions = self.sessions.lock().await;
        if let Some(old) = sessions.remove(session) {
            old.join.abort();
        }
        // Role-aware restart. Switching the model must NOT downgrade the
        // session: a Mission Control or agent session keeps its prompt, tools,
        // and lane limits, which a plain restart would silently drop.
        let handle = self.start_role_aware(&cfg, sp.clone(), None).await;
        if persist {
            write_session_profile(&sp, profile); // outlives this run
        }
        sessions.insert(
            session.to_string(),
            live_from_handle(handle, Some(profile.to_string())),
        );
        Ok(())
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
        let Some(state) = read_session_meta(&sp) else {
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
        // ONE resolver for every role. This path used to handle only
        // `mission_control`, so a config reload silently rebuilt a specialized
        // agent as an ordinary session — losing its identity, prompt and tool
        // set, with nothing logged. Routing both readers through
        // `start_role_aware` makes that asymmetry unrepresentable.
        let handle = self.start_role_aware(&cfg, sp.clone(), None).await;
        sessions.insert(id.to_string(), live_from_handle(handle, profile));
        RebuildOutcome::Rebuilt
    }

    /// Mark a queued entry invisible immediately for every attached client.
    /// The harness may be borrowing its state in an in-flight step, so it still
    /// receives the original control input and persists the durable mutation at
    /// the next safe boundary.
    async fn hide_queued(&self, id: &str, queue_id: &str) {
        let path = self
            .sessions
            .lock()
            .await
            .get(id)
            .map(|s| s.state_path.clone())
            .or_else(|| state_path_for_id(id));
        let Some(path) = path else {
            return;
        };
        let Some(state) = read_session_meta(&path) else {
            return;
        };
        let Some(item) = state.queued_inputs.iter().find(|item| item.id == queue_id) else {
            return;
        };
        let mut hidden = self.queue_hidden.lock().unwrap();
        let entries = hidden.entry(id.to_string()).or_default();
        if !entries.contains(&item.id) {
            entries.push(item.id.clone());
            self.queue_revision.fetch_add(1, Ordering::Release);
        }
    }

    /// Send a loop input to a session. Audio attachment markers are expanded here,
    /// before the input reaches the harness, so every model/provider receives the
    /// same transcript plus the original attachment reference.
    pub(crate) async fn deliver(&self, id: &str, input: LoopInput) {
        match &input {
            LoopInput::Unqueue(queue_id) | LoopInput::SteerQueued(queue_id) => {
                self.hide_queued(id, &queue_id).await
            }
            _ => {}
        }
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
        let Some(state) = read_session_meta(&sp) else {
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
        // ROLE-AWARE revive. This used to call the plain standard start, so any
        // session revived by a delivery came back as an ordinary coding session —
        // an agent's inbox got bash and workspace tools it must never have, and a
        // specialized agent lost its identity. The role is a property of the
        // session, and a delivery is not a reason to change it.
        let handle = self.start_role_aware(&cfg, sp.clone(), initial).await;
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
    let store = crate::store::Store::open(crate::store::default_db_path())
        .map_err(|error| format!("open store: {error}"))?;
    let daemon: Shared = Arc::new(Daemon {
        config: std::sync::Mutex::new(config),
        config_path,
        token,
        hostname: machine_hostname(),
        sessions: Mutex::new(HashMap::new()),
        shells: crate::term::SessionTerms::new(global_shell_cwd()),
        git_write: Mutex::new(()),
        browser: BrowserManager::default(),
        seen_nonces: std::sync::Mutex::new(HashMap::new()),
        queue_hidden: std::sync::Mutex::new(HashMap::new()),
        queue_revision: AtomicU64::new(0),
        mission_control_root: mc::MissionControlStore::default_root(None),
        store,
        coordination_events: tokio::sync::broadcast::channel(256).0,
        recurring_root: crate::recurring::default_root(),
    });

    // Register the built-in agents before anything can address them.
    mission_control::register_builtin_agents(&daemon);

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
        tokio::spawn(async move { mission_control::dispatch_loop(d).await });
    }
    {
        let d = daemon.clone();
        tokio::spawn(async move { direct::direct_dispatch_loop(d).await });
        tokio::spawn(crate::bg::watch_loop());
    }
    {
        let d = daemon.clone();
        tokio::spawn(async move { recurring::tick_loop(d).await });
    }
    {
        let d = daemon.clone();
        tokio::spawn(async move { coordination::coordination_wake_loop(d).await });
    }
    // The upload endpoint carries the file base64-encoded inside a JSON body, which
    // inflates it by ~4/3. Size the request-body limit so a ~1 GB file still fits
    // once encoded (≈1.33 GB) plus headroom for the JSON envelope. Every other route
    // keeps axum's small default body limit.
    const MAX_UPLOAD_FILE_BYTES: usize = 1024 * 1024 * 1024;
    const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_FILE_BYTES / 3 * 4 + 64 * 1024;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/agents", get(list_agents).post(create_agent))
        .route("/agents/{agent_id}/board", get(agent_board))
        .route("/agents/{agent_id}/status", post(set_agent_status))
        .merge(coordination::router())
        .route(
            "/coordination/direct/messages",
            get(direct::direct_list_messages).post(direct::direct_send_message),
        )
        .route("/coordination/direct/threads", get(direct::direct_list_threads))
        .route("/coordination/direct/read", post(direct::direct_mark_read))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/sessions/counts", get(session_counts))
        .route("/usage", get(usage_summary))
        .route("/notifications", get(notification_replay))
        .route("/notifications/replay", get(notification_replay))
        .merge(recurring::router())
        .merge(mission_control::router())
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
        .route("/shells", get(shells_ws))
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
        .route("/git/worktrees", get(git_worktrees))
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

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

#[derive(Deserialize, Clone)]
pub(crate) struct Auth {
    pub(crate) token: Option<String>,
}

#[derive(Deserialize)]
struct BrowserCommandReq {
    #[serde(alias = "deviceName")]
    device_name: String,
    method: String,
    #[serde(default)]
    args: serde_json::Value,
}


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

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

// GET /sessions[?folder=] — device sessions (optionally scoped to one folder),
// each with a `running` flag.
async fn list_sessions(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let mut sessions = list_device_sessions();
    // A folder's sessions include those in worktrees made from it.
    if let Some(folder) = q.folder.as_deref().filter(|f| !f.is_empty()) {
        sessions.retain(|s| s.folder == folder || s.origin_folder.as_deref() == Some(folder));
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

#[derive(Deserialize)]
struct UsageQuery {
    token: Option<String>,
    #[serde(default)]
    since: Option<i64>,
}

async fn usage_summary(State(d): State<Shared>, Query(q): Query<UsageQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let totals = match d.store.usage_totals(q.since) {
        Ok(rows) => rows,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let sessions: HashMap<String, u64> = d
        .store
        .usage_sessions_by_provider(q.since)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let chatgpt_rate = crate::chatgpt::read_global_usage().filter(|rate| rate.is_reported());
    let mut order: Vec<String> = Vec::new();
    let mut providers: HashMap<String, serde_json::Value> = HashMap::new();
    for row in totals {
        let entry = providers.entry(row.provider.clone()).or_insert_with(|| {
            order.push(row.provider.clone());
            let rate_limits = match (&chatgpt_rate, row.provider.as_str()) {
                (Some(rate), "chatgpt") => vec![serde_json::to_value(rate).unwrap_or_default()],
                _ => Vec::new(),
            };
            serde_json::json!({
                "provider": row.provider,
                "model": row.model,
                "sessions": sessions.get(&row.provider).copied().unwrap_or(0),
                "calls": 0,
                "total_tokens": 0,
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "cache_read_tokens": 0,
                "cache_creation_tokens": 0,
                "rate_limits_supported": crate::config::provider_reports_rate_limits(&row.provider),
                "rate_limits": rate_limits,
                "models": [],
            })
        });
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        for (key, value) in [
            ("calls", row.calls),
            ("total_tokens", row.total_tokens),
            ("prompt_tokens", row.prompt_tokens),
            ("completion_tokens", row.completion_tokens),
            ("cache_read_tokens", row.cache_read_tokens),
            ("cache_creation_tokens", row.cache_creation_tokens),
        ] {
            let current = obj[key].as_u64().unwrap_or(0);
            obj.insert(key.into(), serde_json::json!(current.saturating_add(value)));
        }
        if let Some(models) = obj.get_mut("models").and_then(|m| m.as_array_mut()) {
            models.push(serde_json::to_value(&row).unwrap_or_default());
        }
    }
    let list: Vec<serde_json::Value> = order
        .into_iter()
        .filter_map(|p| providers.remove(&p))
        .collect();
    Json(serde_json::json!({ "providers": list, "since": q.since })).into_response()
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
    /// Work in a new git worktree of the folder, or in the folder itself.
    /// Required: where a session's edits land is the caller's choice.
    workspace: crate::session::WorkspaceMode,
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
    let folder = crate::session::session_workspace(&folder, req.workspace);
    let base_state = {
        let c = d.config.lock().unwrap();
        c.for_workspace(folder.clone()).state_path
    };
    // Default session vs new conversation branch.
    let (sp, resume) = if req.new_conversation {
        let path = base_state
            .join("conversations")
            .join(uuid::Uuid::new_v4().to_string());
        (path, false)
    } else {
        (base_state.clone(), req.resume)
    };
    // Canonical (filename stripped) so this matches `session_id_for_state_path`
    // and any row migrated from the old path-shaped ids.
    let id = crate::conversations::canonical_session_id(
        &sp
            .strip_prefix(workspaces_root())
            .unwrap_or(&sp)
            .display()
            .to_string(),
    )
    .0;
    // Effective model: a persisted per-conversation override is AUTHORITATIVE on
    // resume — the app re-sends a profile on plain navigation (foregrounding,
    // reopening the chat), and honoring it silently reverted the model the user
    // set for this chat via /session/model. An explicit profile only seeds a
    // conversation that has no override yet (e.g. new_conversation).
    let persisted = read_session_profile(&sp);
    let profile = persisted.clone().or_else(|| req.profile.clone());
    let cfg = {
        let c = d.config.lock().unwrap();
        let mut w = c.for_workspace(folder.clone());
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
    Json(serde_json::json!({ "id": id, "folder": folder.display().to_string() })).into_response()
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
    /// xAI only: attach the built-in X search server tool.
    #[serde(default)]
    x_search: bool,
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
                x_search: m.x_search,
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


#[cfg(test)]
mod tests;


