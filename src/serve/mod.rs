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
use axum::routing::{delete, get, post, put};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::config::{InferenceProfileConfig, SnippetConfig, save_config, workspaces_root};
use crate::coordination::{
    HandoffMode, NotificationMarker, Task, TaskLink, TaskLinkKind, TaskResult, TaskStatus,
};
use crate::harness::{GoalStatus, HarnessEvent, LoopInput};
use crate::mission_control::{self, ManagedSession};
use crate::recurring::{self, Schedule};
use crate::session::{
    SessionRole, SessionSidecar, list_device_sessions, prepare_new_session_workspace,
    read_session_profile, read_session_sidecar, read_session_state, replay_notification_events,
    session_id_for_state_path, start_mission_control_session, start_session_with_browser_summary,
    state_path_for_id, status_str, subscribe_device_events, write_session_profile,
    write_session_sidecar,
};

mod browser;
mod direct;
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
    let cwd = {
        let from_state = read_session_state(&handle.state_path)
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
    // Store-or-file: a session ported to the database has no state file, so
    // reading the file directly would 404 every migrated session.
    let Some(state) = read_session_state(&sp) else {
        return Err((StatusCode::NOT_FOUND, "session state unreadable").into_response());
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
        let state = read_session_state(&sp)?;
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
    async fn run_session_on_profile(
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
        let Some(state) = read_session_state(&sp) else {
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
        let Some(state) = read_session_state(&path) else {
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
    async fn deliver(&self, id: &str, input: LoopInput) {
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
        let Some(state) = read_session_state(&sp) else {
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
        mission_control_root: mission_control::MissionControlStore::default_root(None),
        store,
        coordination_events: tokio::sync::broadcast::channel(256).0,
        recurring_root: recurring::default_root(),
    });

    // Register the built-in agents before anything can address them.
    register_builtin_agents(&daemon);

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
    {
        let d = daemon.clone();
        tokio::spawn(async move { direct::direct_dispatch_loop(d).await });
    }
    {
        let d = daemon.clone();
        tokio::spawn(async move { recurring_tick_loop(d).await });
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
        .route("/agents/build", post(build_agent_from_prompt))
        .route("/agents/{agent_id}/board", get(agent_board))
        .route(
            "/coordination/tasks",
            get(coordination_list_tasks).post(coordination_create_task),
        )
        .route(
            "/coordination/tasks/{task_id}",
            get(coordination_get_task).patch(coordination_update_task),
        )
        .route(
            "/coordination/tasks/{task_id}/status",
            post(coordination_set_task_status),
        )
        .route(
            "/coordination/tasks/{task_id}/links",
            get(coordination_task_links).post(coordination_link_tasks),
        )
        .route(
            "/coordination/tasks/{task_id}/links/{other_id}",
            delete(coordination_unlink_tasks),
        )
        .route(
            "/coordination/tasks/{task_id}/agents",
            get(coordination_task_agents).post(coordination_add_task_agent),
        )
        .route(
            "/coordination/tasks/{task_id}/agents/{agent_id}",
            delete(coordination_remove_task_agent),
        )
        .route(
            "/coordination/threads/{thread_id}/events",
            get(coordination_events),
        )
        .route(
            "/coordination/threads/{thread_id}/messages",
            post(coordination_post_message),
        )
        .route("/coordination/events", get(coordination_events_ws))
        .route(
            "/coordination/direct/messages",
            get(direct::direct_list_messages).post(direct::direct_send_message),
        )
        .route("/coordination/direct/threads", get(direct::direct_list_threads))
        .route("/coordination/direct/read", post(direct::direct_mark_read))
        .route("/sessions", get(list_sessions).post(open_session))
        .route("/sessions/counts", get(session_counts))
        .route("/usage", get(usage_summary))
        .route("/notifications/replay", get(notification_replay))
        .route("/recurring", get(list_recurring).post(create_recurring))
        .route(
            "/recurring/{id}",
            put(update_recurring).delete(delete_recurring),
        )
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
        if read_session_state(&s.state_path)
            .is_some_and(|state| state.status == crate::harness::HarnessStatus::Running)
        {
            return true;
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

#[derive(Deserialize, Clone)]
struct Auth {
    token: Option<String>,
}

#[derive(Deserialize)]
struct AgentsQuery {
    token: Option<String>,
    /// Keyset cursor: the previous page's last `(display_name, id)`.
    #[serde(default)]
    after_name: Option<String>,
    #[serde(default)]
    after_id: Option<String>,
    #[serde(default = "default_agent_page_limit")]
    limit: u32,
}
fn default_agent_page_limit() -> u32 {
    200
}

async fn list_agents(State(d): State<Shared>, Query(q): Query<AgentsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let after = match (q.after_name.as_deref(), q.after_id.as_deref()) {
        (Some(name), Some(id)) => Some((name, id)),
        // A lone cursor half is a caller error, not a silent full listing.
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "after_name and after_id must be provided together",
            )
                .into_response();
        }
        (None, None) => None,
    };
    match d.store.list_agents_page(after, q.limit.clamp(1, 500)) {
        Ok(agents) => Json(agents).into_response(),
        Err(error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("store: {error}")).into_response()
        }
    }
}

#[derive(Deserialize)]
struct AgentBoardQuery {
    token: Option<String>,
    /// Only rows about this folder.
    #[serde(default)]
    workspace: Option<String>,
    /// Substring to find in the recorded summaries.
    #[serde(default)]
    contains: Option<String>,
    /// `dispatched`, `reported`, or `noted`.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default = "default_agent_page_limit")]
    limit: u32,
}

/// GET /agents/{agent_id}/board — one agent's coordination memory.
///
/// Read-only: the board is written by the agent as it works, and by the
/// dispatcher/report paths. This is what makes an agent's history inspectable
/// rather than invisible — what it sent, what came back, what it concluded.
async fn agent_board(
    State(d): State<Shared>,
    Query(q): Query<AgentBoardQuery>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let kind = match q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        None => None,
        Some(raw) => match crate::coordination::BoardEntryKind::parse(raw) {
            Some(kind) => Some(kind),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown kind `{raw}` — use dispatched, reported, or noted"),
                )
                    .into_response();
            }
        },
    };
    let query = crate::coordination::BoardQuery {
        workspace: q.workspace.as_deref().filter(|w| !w.trim().is_empty()),
        contains: q.contains.as_deref().filter(|c| !c.trim().is_empty()),
        kind,
    };
    match d
        .store
        .read_board(&agent_id, &query, q.limit.clamp(1, 200))
    {
        Ok(entries) => Json(serde_json::json!({
            "agent_id": agent_id,
            "entries": entries,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read board: {error}"),
        )
            .into_response(),
    }
}

/// Record one event in a session's transcript WITHOUT starting a turn.
///
/// The one place a notice is written, so a live loop and a dormant session
/// produce identical transcripts. Prefers the LIVE loop when the session is
/// running, so the entry lands in the loop's own state and cannot be clobbered
/// by a later full rewrite. A dormant session has no loop to hold it, so it goes
/// straight to the store — which is where a live loop reads from anyway.
///
/// `notice_text` decides whether the event also enters the model's context.
async fn record_notice(d: &Daemon, session_id: &str, event: HarnessEvent) {
    // Callers pass the id as it was recorded elsewhere — which is not always the
    // stored key, since a model routinely drops the `/state.json` suffix.
    // Recording under the bare form would match no session and be dropped below,
    // silently losing the entry. Resolve to the canonical id first so both forms
    // name the same conversation.
    let canonical = crate::session::state_path_for_id(session_id)
        .map(|path| crate::session::session_id_for_state_path(&path));
    let session_id = canonical.as_deref().unwrap_or(session_id);
    // A FINISHED loop is a dead entry: `send` into its closed channel reports
    // success and the notice is gone. `deliver` guards on this; without the same
    // guard here, a message sent to a session whose loop has stopped — after an
    // interrupt, say — never reaches its transcript even though the daemon
    // accepted it. Filtering finished loops falls through to the store write
    // below, which is where a resumed loop reads from anyway.
    let live = {
        let sessions = d.sessions.lock().await;
        sessions
            .get(session_id)
            .filter(|s| !s.join.is_finished())
            .map(|s| s.input_tx.clone())
    };
    if let Some(tx) = live {
        // A notice, not a `UserMessage`: the loop records it and stays parked.
        let _ = tx.send(LoopInput::Notice(event));
        return;
    }
    // Dormant: no loop to deliver into. Write straight to the store, but only
    // when the session actually has a row — `session_events` has a foreign key to
    // `sessions`, so an insert for an unknown session would just fail.
    if !d.store.has_conversation(session_id).unwrap_or(false) {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let _ = d
        .store
        .append_conversation_events(session_id, std::slice::from_ref(&event), &now);
    if let Some(text) = crate::harness::notice_text(&event) {
        let _ = d.store.append_conversation_messages(
            session_id,
            &[crate::llm::HarnessMessage::User { content: text }],
            &now,
        );
    }
}

/// Record a direct message in a session's transcript WITHOUT starting a turn.
///
/// Both directions use this: what a session sent out, and what came back. The
/// event is for the UIs; the paired message is so the model sees the exchange
/// next turn.
///
/// Private to `serve`; its child module `direct` reaches it via `super::`.
async fn record_agent_message(
    d: &Shared,
    session_id: &str,
    agent_id: &str,
    body: &str,
    outbound: bool,
) {
    record_notice(
        d,
        session_id,
        HarnessEvent::AgentMessage {
            agent_id: agent_id.to_string(),
            body: body.to_string(),
            outbound,
        },
    )
    .await;
}

/// Note on Mission Control's transcript that work was dispatched on its behalf.
///
/// The user can file a task directly, which bypasses Mission Control entirely —
/// so without this the coordinator's record would show the task appearing from
/// nowhere. It is a NOTICE, never a wake: the work is already routed to a worker
/// and will report back on its own, so waking Mission Control would spend a turn
/// on something that needs no decision.
async fn record_dispatch_notice(d: &Daemon, task: &crate::coordination::Task) {
    // Mission Control dispatching through its own tool already has the tool call
    // and result in its transcript. Recording a notice too would tell it twice,
    // so this is for dispatches made AROUND it — the case where its record would
    // otherwise show work appearing from nowhere.
    if task.created_by_id == crate::mission_control::SESSION_ID {
        return;
    }
    let target = crate::session::state_path_for_id(&task.session_id)
        .map(|path| crate::session::session_id_for_state_path(&path))
        .unwrap_or_else(|| task.session_id.clone());
    let by = if task.created_by_kind == "human" {
        "You".to_string()
    } else {
        format!("{}:{}", task.created_by_kind, task.created_by_id)
    };
    record_notice(
        d,
        crate::mission_control::SESSION_ID,
        HarnessEvent::TaskDispatched {
            task_id: task.id.clone(),
            title: task.title.clone(),
            session_id: target,
            by,
        },
    )
    .await;
}

#[derive(Deserialize)]
struct AgentReq {
    id: String,
    display_name: String,
    handle: String,
    #[serde(default = "default_agent_kind")]
    kind: crate::coordination::types::AgentKind,
    #[serde(default = "default_agent_status")]
    status: crate::coordination::types::AgentStatus,
    #[serde(default = "default_agent_role")]
    role: crate::coordination::types::AgentRole,
    #[serde(default)]
    capabilities: Vec<String>,
}
fn default_agent_kind() -> crate::coordination::types::AgentKind {
    crate::coordination::types::AgentKind::Worker
}
fn default_agent_status() -> crate::coordination::types::AgentStatus {
    crate::coordination::types::AgentStatus::Active
}
fn default_agent_role() -> crate::coordination::types::AgentRole {
    crate::coordination::types::AgentRole::Implementer
}

async fn create_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Json(req): Json<AgentReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    if req.id.trim().is_empty() || req.handle.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "id and handle are required").into_response();
    }
    let agent = crate::coordination::types::Agent {
        id: req.id,
        display_name: req.display_name,
        handle: req.handle,
        kind: req.kind,
        status: req.status,
        role: req.role,
        capabilities: req.capabilities,
    };
    let home = match crate::coordination::AgentHome::new(
        crate::coordination::agents_root(&d.mission_control_root),
        &agent.id,
    ) {
        Ok(home) => home,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let default_identity = format!(
        "# {}\n\nAgent handle: @{}\n\nThis identity is awaiting its first build and research pass.\n",
        agent.display_name, agent.handle
    );
    if let Err(error) = home.ensure_layout(&default_identity) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create agent home: {error}"),
        )
            .into_response();
    }
    match d.store.create_agent(&agent) {
        Ok(()) => (StatusCode::CREATED, Json(agent)).into_response(),
        Err(error) => (StatusCode::CONFLICT, format!("create agent: {error}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CoordinationEventsQuery {
    token: Option<String>,
    #[serde(default)]
    after_sequence: u64,
    #[serde(default = "default_coord_event_limit")]
    limit: u32,
}
fn default_coord_event_limit() -> u32 {
    100
}

async fn coordination_events(
    State(d): State<Shared>,
    axum::extract::Path(thread_id): axum::extract::Path<String>,
    Query(q): Query<CoordinationEventsQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d
        .store
        .events_for_thread(&thread_id, q.after_sequence, q.limit.clamp(1, 500))
    {
        Ok(events) => Json(events).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read coordination events: {error}"),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CoordinationMessageReq {
    actor_kind: String,
    actor_id: String,
    body: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn coordination_post_message(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(thread_id): axum::extract::Path<String>,
    Json(req): Json<CoordinationMessageReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    if req.body.trim().is_empty() || req.actor_id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "actor_id and body are required").into_response();
    }
    let event = crate::coordination::types::CoordinationEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        thread_id: thread_id.clone(),
        partition_key: format!("thread:{thread_id}"),
        sequence: 0,
        event_type: "message.posted".into(),
        actor_kind: req.actor_kind,
        actor_id: req.actor_id,
        payload_version: 1,
        payload: serde_json::json!({"body":req.body}),
        causation_id: None,
        correlation_id: None,
        idempotency_key: if req.idempotency_key.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            req.idempotency_key
        },
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    match d.store.append_event(&event) {
        Ok(saved) => {
            let _ = d.coordination_events.send(saved.clone());
            // A board post is a message TO someone: wake Mission Control so it
            // can respond. Without this the human just sees their own message
            // and no agent ever participates. Skipped for Mission Control's own
            // posts so a reply can't wake the session that wrote it.
            if should_wake_mission_control(&saved.actor_id) {
                let daemon = d.clone();
                // Hand over a bounded slice of the room, excluding the message
                // just posted (the envelope carries it separately as `body`), so
                // the wake reads as a group chat rather than one isolated line.
                let history: Vec<_> = d
                    .store
                    .recent_events_for_thread(
                        &saved.thread_id,
                        crate::coordination_tools::BOARD_HISTORY_ON_WAKE + 1,
                    )
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.sequence < saved.sequence)
                    .collect();
                let envelope = board_message_envelope(&saved, &history);
                tokio::spawn(async move {
                    daemon
                        .deliver(
                            crate::mission_control::SESSION_ID,
                            LoopInput::UserMessage(envelope),
                        )
                        .await;
                });
            }
            Json(saved).into_response()
        }
        Err(error) => (
            StatusCode::CONFLICT,
            format!("post coordination message: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_events_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let mut rx = d.coordination_events.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        while let Ok(event) = rx.recv().await {
            if socket
                .send(Message::Text(
                    serde_json::json!({"wire":"coordination_event","event":event})
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

/// The task board's own columns are narrower than the assignment states — a task
/// is what a HUMAN filed, so it carries Todo/InProgress/Blocked/Done/Cancelled
/// and nothing about dispatch.
#[derive(Deserialize)]
struct CoordinationTasksQuery {
    token: Option<String>,
    /// Keyset cursor: the previous page's last `(created_at, id)`.
    #[serde(default)]
    after_created: Option<String>,
    #[serde(default)]
    after_id: Option<String>,
    #[serde(default = "default_agent_page_limit")]
    limit: u32,
    /// Board column to show. Omitted returns every column.
    #[serde(default)]
    status: Option<String>,
    /// "What is this agent on" — the roster filter.
    #[serde(default)]
    agent_id: Option<String>,
}

fn parse_task_status(value: &str) -> Result<crate::coordination::TaskStatus, String> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|_| format!("unknown task status: {value}"))
}

#[derive(Deserialize)]
struct CoordinationTaskReq {
    /// Optional so a client can supply its own id (offline creation); the daemon
    /// generates one otherwise.
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    description: String,
    /// The session that should do the work. REQUIRED: a task with no target can
    /// never be dispatched — the loop claims it, finds nowhere to deliver, and
    /// parks it Blocked — so filing one would create dead weight.
    session_id: String,
    #[serde(default)]
    priority: i64,
}

#[derive(Deserialize)]
struct CoordinationTaskPatch {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
}

#[derive(Deserialize)]
struct CoordinationTaskStatusReq {
    status: String,
}

#[derive(Deserialize)]
struct CoordinationTaskLinkReq {
    to_task_id: String,
    /// `blocks` (default) or `relates_to`.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct CoordinationTaskAgentReq {
    agent_id: String,
    #[serde(default)]
    role: String,
}

/// Tasks on the board, newest priority first. The board is the human's view:
/// creating and moving a task never touches the dispatch machinery, which is
/// Mission Control's job once it picks the task up.
async fn coordination_list_tasks(
    State(d): State<Shared>,
    Query(q): Query<CoordinationTasksQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let after = match (q.after_created.as_deref(), q.after_id.as_deref()) {
        (Some(created), Some(id)) => Some((created, id)),
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "after_created and after_id must be provided together",
            )
                .into_response();
        }
        (None, None) => None,
    };
    let status = match q.status.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(raw) => match parse_task_status(raw) {
            Ok(status) => Some(status),
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        },
        None => None,
    };
    let filter = crate::coordination::TaskFilter {
        status,
        agent_id: q.agent_id.as_deref().filter(|s| !s.trim().is_empty()),
    };
    match d
        .store
        .list_tasks_page(&filter, after, q.limit.clamp(1, 500))
    {
        Ok(tasks) => Json(tasks).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list tasks: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_create_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    Json(req): Json<CoordinationTaskReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let title = req.title.trim();
    if title.is_empty() {
        return (StatusCode::BAD_REQUEST, "title is required").into_response();
    }
    // An inbox is an agent's mailbox, not a place work runs: it holds the
    // coordination runtime, which has no workspace tools and cannot report a
    // task. Filing work there leaves it InProgress forever, so it is refused
    // here for the same reason a nonexistent session is.
    if crate::session::is_inbox_session_id(req.session_id.trim()) {
        return (
            StatusCode::BAD_REQUEST,
            "session_id names an agent's inbox; route to a work session instead",
        )
            .into_response();
    }
    // The target must resolve, or the task would be filed against a session no
    // dispatch could ever reach.
    let Some(target_path) = crate::session::state_path_for_id(req.session_id.trim()) else {
        return (StatusCode::BAD_REQUEST, "session_id must name a real session").into_response();
    };
    let Some(state) = crate::session::read_session_state(&target_path) else {
        return (StatusCode::BAD_REQUEST, "session_id must name a real session").into_response();
    };
    // Canonical, so the task binds to the id the runtime will report with.
    // Storing the id as typed let a task filed against `x/state.json` become
    // unreportable by the session whose canonical id is `x`.
    let session_id = crate::session::session_id_for_state_path(&target_path);
    // Dispatch delivers through the MANAGED session record, so a target that is
    // not managed yet would be claimed, found undeliverable, and parked Blocked
    // after the failure ceiling. Registering it here is what makes a task filed
    // against any real session runnable — the same step Mission Control's own
    // task tool performs.
    if mission_control::get_session(&d.mission_control_root, &session_id).is_err() {
        if let Err(error) = mission_control::create_session(
            &d.mission_control_root,
            &session_id,
            state.title.as_deref().unwrap_or("Managed session"),
            std::path::Path::new(&state.workspace),
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("register managed session: {error}"),
            )
                .into_response();
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let id = req
        .id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let task = crate::coordination::Task::filed_by_human(
        id,
        title.to_string(),
        req.description.trim().to_string(),
        session_id.clone(),
        req.priority,
        now,
    );
    match d.store.create_task(&task) {
        Ok(()) => (StatusCode::CREATED, Json(task)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create task: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_get_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.get_task(&task_id) {
        Ok(Some(task)) => Json(task).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("get task: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_update_task(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(req): Json<CoordinationTaskPatch>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let now = chrono::Utc::now().to_rfc3339();
    let title = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let description = req.description.as_deref().map(str::trim);
    match d
        .store
        .update_task(&task_id, title, description, req.priority, &now)
    {
        Ok(true) => match d.store.get_task(&task_id) {
            Ok(Some(task)) => Json(task).into_response(),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "task vanished").into_response(),
        },
        Ok(false) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update task: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_set_task_status(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(req): Json<CoordinationTaskStatusReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let status = match parse_task_status(req.status.trim()) {
        Ok(status) => status,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let now = chrono::Utc::now().to_rfc3339();
    match d.store.set_task_status(&task_id, &status, &now) {
        Ok(true) => match d.store.get_task(&task_id) {
            Ok(Some(task)) => Json(task).into_response(),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "task vanished").into_response(),
        },
        Ok(false) => (StatusCode::NOT_FOUND, "no such task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("set task status: {error}"),
        )
            .into_response(),
    }
}

/// Every edge touching the task, both directions, plus the blockers resolved to
/// ids. A reader needs the incoming edges as much as the outgoing ones — that is
/// what makes a blocked task render as blocked.
async fn coordination_task_links(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let links = match d.store.task_links(&task_id) {
        Ok(links) => links,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task links: {error}"),
            )
                .into_response();
        }
    };
    let blockers = match d.store.blockers_of(&task_id) {
        Ok(blockers) => blockers,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task blockers: {error}"),
            )
                .into_response();
        }
    };
    Json(serde_json::json!({ "links": links, "blocked_by": blockers })).into_response()
}

async fn coordination_link_tasks(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(req): Json<CoordinationTaskLinkReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let to = req.to_task_id.trim();
    if to.is_empty() {
        return (StatusCode::BAD_REQUEST, "to_task_id is required").into_response();
    }
    if to == task_id {
        return (StatusCode::BAD_REQUEST, "a task cannot link to itself").into_response();
    }
    let kind = match req.kind.as_deref().unwrap_or("blocks") {
        "blocks" => crate::coordination::TaskLinkKind::Blocks,
        "relates_to" => crate::coordination::TaskLinkKind::RelatesTo,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown link kind: {other}"),
            )
                .into_response();
        }
    };
    // Both ends must exist, or the edge dangles and the board draws a blank node.
    for id in [task_id.as_str(), to] {
        match d.store.get_task(id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return (StatusCode::NOT_FOUND, format!("no such task: {id}")).into_response();
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("read task: {error}"),
                )
                    .into_response();
            }
        }
    }
    let link = crate::coordination::TaskLink {
        from_task_id: task_id,
        to_task_id: to.to_string(),
        kind,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    match d.store.link_tasks(&link) {
        Ok(()) => (StatusCode::CREATED, Json(link)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("link tasks: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_unlink_tasks(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path((task_id, other_id)): axum::extract::Path<(String, String)>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    // Both directions are removed: the caller names the pair, and which end was
    // stored as `from` is an implementation detail it should not have to know.
    let mut removed = false;
    for (from, to) in [
        (task_id.as_str(), other_id.as_str()),
        (other_id.as_str(), task_id.as_str()),
    ] {
        for kind in [
            crate::coordination::TaskLinkKind::Blocks,
            crate::coordination::TaskLinkKind::RelatesTo,
        ] {
            match d.store.unlink_tasks(from, to, &kind) {
                Ok(true) => removed = true,
                Ok(false) => {}
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("unlink tasks: {error}"),
                    )
                        .into_response();
                }
            }
        }
    }
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no such link").into_response()
    }
}

async fn coordination_task_agents(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.store.list_task_agents(&task_id) {
        Ok(agents) => Json(agents).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task agents: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_add_task_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(req): Json<CoordinationTaskAgentReq>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let agent_id = req.agent_id.trim();
    if agent_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "agent_id is required").into_response();
    }
    let now = chrono::Utc::now().to_rfc3339();
    match d
        .store
        .add_task_agent(&task_id, agent_id, req.role.trim(), &now)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("add task agent: {error}"),
        )
            .into_response(),
    }
}

async fn coordination_remove_task_agent(
    State(d): State<Shared>,
    Query(q): Query<Auth>,
    axum::extract::Path((task_id, agent_id)): axum::extract::Path<(String, String)>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d
        .store
        .remove_task_agent(&task_id, &agent_id, &chrono::Utc::now().to_rfc3339())
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "agent is not on this task").into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("remove task agent: {error}"),
        )
            .into_response(),
    }
}

/// Whether a board post should wake Mission Control. Posts authored by Mission
/// Control itself are skipped, so a reply cannot re-wake the session that wrote
/// it (which would loop).
fn should_wake_mission_control(actor_id: &str) -> bool {
    actor_id != crate::mission_control::SESSION_ID
}

/// The board thread every participant shares: the human app posts here, and the
/// daemon routes assignments and worker posts to the same thread so nobody has
/// to guess an id.
pub const COORDINATION_THREAD: &str = "system";

/// Collapse a message body to a single line for the history digest, so a
/// multi-line message can't break the field-per-line layout.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The envelope handed to Mission Control when someone posts to the board — the
/// shared room. Wraps the message so it is not mistaken for a direct chat turn,
/// and carries a bounded slice of recent history so the room reads as a group
/// chat rather than a single isolated line.
///
/// Field order matters: `body` is deliberately last, so a client can take
/// everything up to the closing tag as the message — including newlines — and
/// the internal `rules` and history never run into the sender's text.
fn board_message_envelope(
    event: &crate::coordination::types::CoordinationEvent,
    history: &[crate::coordination::types::CoordinationEvent],
) -> String {
    let body = event
        .payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let mut digest = String::new();
    if history.is_empty() {
        digest.push_str("history: (start of the room)\n");
    } else {
        digest.push_str(&format!(
            "history: last {} message(s), oldest first\n",
            history.len()
        ));
        for prior in history {
            let prior_body = one_line(
                prior
                    .payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            );
            digest.push_str(&format!(
                "  {} [{}] {}: {}\n",
                prior.sequence, prior.actor_kind, prior.actor_id, prior_body
            ));
        }
    }
    format!(
        "[coordination_board_message]\nthread_id: {}\nfrom_id: {}\nfrom_kind: {}\n\
         rules: board message, not an ordinary chat turn. This is a shared group room, not a direct \
         message. Decide whether it needs a response, a handoff, or an assignment; reply on this same \
         thread with post_coordination_message so everyone sees it. If no action is needed, say so on \
         the thread rather than staying silent. Only the recent history is included — call \
         read_coordination_thread to see more.\n{history}body: {body}\n\
         [/coordination_board_message]",
        event.thread_id,
        event.actor_id,
        event.actor_kind,
        history = digest,
        body = body,
    )
}






#[derive(Deserialize)]
struct NotificationReplayQuery {
    token: Option<String>,
    #[serde(default)]
    since: u64,
}

async fn notification_replay(
    State(d): State<Shared>,
    Query(q): Query<NotificationReplayQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    Json(serde_json::json!({
        "events": replay_notification_events(q.since),
    }))
    .into_response()
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

async fn usage_summary(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let config = d.config.lock().unwrap().clone();
    let mut totals: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for session in list_device_sessions() {
        let Some(path) = state_path_for_id(&session.id) else {
            continue;
        };
        let Some(state) = read_session_state(&path) else {
            continue;
        };
        let profile_name = read_session_profile(&path);
        let model = profile_name
            .as_ref()
            .and_then(|name| config.setups.as_ref()?.get(name))
            .unwrap_or(&config.model);
        let provider = model.provider.clone();
        // Does this provider report rate-limit data AT ALL? Only ChatGPT parses
        // the Codex headers; every other model hardcodes an empty snapshot, and
        // opencode returns no such headers even in principle. So an empty list
        // means two very different things — "cannot report" vs "hasn't reported
        // yet" — and the UI must tell them apart instead of showing one generic
        // empty state for both.
        let reports_limits = crate::config::provider_reports_rate_limits(&provider);
        let entry = totals.entry(provider.clone()).or_insert_with(|| {
            serde_json::json!({
                "provider": provider,
                "profile": profile_name,
                "model": model.model,
                "sessions": 0,
                "total_tokens": 0,
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "cache_read_tokens": 0,
                "rate_limits_supported": reports_limits,
                "rate_limits": []
            })
        });
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        obj.insert(
            "sessions".into(),
            serde_json::json!(obj["sessions"].as_u64().unwrap_or(0) + 1),
        );
        for (key, value) in [
            ("total_tokens", state.total_tokens),
            ("prompt_tokens", state.prompt_tokens),
            ("completion_tokens", state.completion_tokens),
            ("cache_read_tokens", state.cache_read_tokens),
        ] {
            let current = obj[key].as_u64().unwrap_or(0);
            obj.insert(key.into(), serde_json::json!(current.saturating_add(value)));
        }
        // NOTE: no per-session rate_limit is attributed to a provider here.
        //
        // `HarnessState.rate_limit` can only ever hold a CHATGPT snapshot —
        // `chatgpt.rs` is the only model that parses rate-limit headers
        // (`openai.rs` hardcodes `None`, as do anthropic/gemini/xai), and
        // opencode's API returns no such headers at all. So reading it while
        // iterating a session of ANY provider mis-attributed a ChatGPT figure to
        // whatever that session ran: an `opencode-go` session displayed ChatGPT's
        // numbers as its own. ChatGPT is handled once, globally, below.
    }
    // ChatGPT Codex limits are account-wide, not session-local. Include the same
    // reported snapshot used by the live chat Usage panel here as well.
    if let Some(rate) = crate::chatgpt::read_global_usage().filter(|rate| rate.is_reported()) {
        if let Some(entry) = totals.get_mut("chatgpt") {
            if let Some(obj) = entry.as_object_mut() {
                let rates = obj
                    .get_mut("rate_limits")
                    .and_then(|v| v.as_array_mut())
                    .expect("rate_limits array");
                // ChatGPT limits are account-wide. Replace session-local/history
                // entries with the freshest global snapshot, rather than exposing
                // stale duplicate windows in the provider Usage screen.
                rates.clear();
                let value = serde_json::to_value(rate).unwrap_or_default();
                rates.push(value);
            }
        }
    }
    Json(serde_json::json!({
        "providers": totals.into_values().collect::<Vec<_>>()
    }))
    .into_response()
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
    let mut folder = PathBuf::from(&req.folder);
    if !folder.is_dir() {
        return (StatusCode::BAD_REQUEST, "not a directory").into_response();
    }
    // Git repos get an isolated worktree for any NEW session (first open of
    // this folder, or an explicit new conversation). Resume of an existing
    // chat stays in its original folder so old sessions are untouched.
    let original_state = {
        let c = d.config.lock().unwrap();
        c.for_workspace(folder.clone()).state_path
    };
    // A session lives in the store, so `state_path.exists()` alone reports
    // "new" for every migrated workspace — which created a second worktree and
    // a second session beside the real one.
    let has_existing =
        original_state.exists() || crate::session::store_default_session_id(&folder).is_some();
    if req.new_conversation || !has_existing {
        folder = prepare_new_session_workspace(&folder);
    }
    let base_state = {
        let c = d.config.lock().unwrap();
        c.for_workspace(folder.clone()).state_path
    };
    // Default → the folder's single `state.json`. New conversation → a fresh
    // `conversations/<uuid>.json` (started blank), so the folder's existing
    // session(s) are preserved and a new one appears in the list.
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
    x_search: Option<bool>,
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
            mc.base_url = InferenceProfileConfig::default().base_url;
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
        if let Some(x_search) = req.x_search {
            mc.x_search = x_search;
        }
        c.upsert_profile(&name, mc);
        if req.set_active {
            c.activate(&name);
        }
        save_config(&c, &d.config_path).map(|_| name)
    };
    match result {
        Ok(name) => {
            Json(serde_json::json!({ "name": name })).into_response()
        }
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
        Ok(_) => {
            Json(serde_json::json!({ "active": req.name })).into_response()
        }
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
            None if req.provider.is_some() => crate::config::InferenceProfileConfig {
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
        Ok(_) => {
            Json(serde_json::json!({ "delegate": name })).into_response()
        }
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
        Ok(_) => {
            Json(serde_json::json!({ "removed": q.name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct SessionModelReq {
    session: String,
    profile: String,
}

// POST /session/model {session, profile} — pin one conversation to a profile.
// Rebuilds its loop on the chosen model, resuming from disk, and persists the
// choice so it survives a daemon restart.
async fn set_session_model(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SessionModelReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    // One implementation, shared with dispatch: both need the same
    // resolve → abort → restart-on-new-model sequence, and a second copy would
    // be free to drift on the role-aware details. The DIFFERENCE is persistence:
    // this route is the user pinning the chat's model, so `true`.
    match d
        .run_session_on_profile(&req.session, &req.profile, true)
        .await
    {
        Ok(()) => {
            Json(serde_json::json!({ "session": req.session, "profile": req.profile }))
                .into_response()
        }
        Err(error) => (StatusCode::NOT_FOUND, error).into_response(),
    }
}

fn session_event_page(
    events: &[crate::harness::HarnessEvent],
    before: Option<usize>,
    limit: Option<usize>,
) -> (usize, usize, bool) {
    let end = before.unwrap_or(events.len()).min(events.len());
    let size = limit.unwrap_or(160).clamp(1, 500);
    let start = end.saturating_sub(size);
    (start, end, start > 0)
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

    // Load current state (store or file — authoritative for history indices).
    let Some(mut state) = read_session_state(&sp) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read session state",
        )
            .into_response();
    };

    let label = match state.apply_checkpoint_rewind(&checkpoint_id) {
        Ok(label) => label,
        Err(_) => return (StatusCode::NOT_FOUND, "checkpoint not found").into_response(),
    };

    // Persist truncated history first so any client re-read sees the cut.
    // `write_session_state` targets whichever store holds the session, so a
    // rewind does not silently write a file a DB session will never read.
    if let Err(error) = crate::session::write_session_state(&sp, &state) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write state: {error}"),
        )
            .into_response();
    }
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

/// Change-detector for a session, from whichever store holds it: the store's
/// row timestamp plus its log lengths, else the state file's bytes.
///
/// The attach stream re-checks on a timer, so this must stay cheap and must NOT
/// load the transcript — not loading it is the whole point of having moved it
/// into the database.
fn session_state_fingerprint(store: &crate::store::Store, state_path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let id = session_id_for_state_path(state_path);
    if let Ok(Some(fingerprint)) = store.conversation_fingerprint(&id) {
        fingerprint.hash(&mut hasher);
        return Some(hasher.finish());
    }
    let bytes = std::fs::read(state_path).ok()?;
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

fn resolve_session_dir(session: &str) -> Result<PathBuf, Response> {
    if let Some(sp) = state_path_for_id(session) {
        if let Some(state) = read_session_state(&sp) {
            let dir = PathBuf::from(&state.workspace);
            if !state.workspace.is_empty() && dir.is_dir() {
                return Ok(dir);
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

    // Prefer the live session's own path when it is running, so the fork sees a
    // turn that has not flushed yet. The reader resolves whichever store holds
    // the session, so a database-backed session forks exactly like a file one.
    // This used to be a nested if/else cascade that repeated the same read four
    // times, which is how the file-only assumption got baked in four times over.
    let state = {
        let sessions = d.sessions.lock().await;
        let path = sessions
            .get(&req.session)
            .filter(|live| !live.join.is_finished())
            .map(|live| live.state_path.clone())
            .unwrap_or_else(|| sp.clone());
        drop(sessions);
        match read_session_state(&path) {
            Some(state) => state,
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session state unreadable",
                )
                    .into_response();
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

#[derive(Deserialize)]
struct ShellsQuery {
    token: Option<String>,
}

// WS /shells — daemon-WIDE interactive shells, with no session at all.
//
// A shell belongs to the machine: switching or closing a session must not kill
// it. `/attach` cannot serve this because it calls `ensure_live`, so a socket
// with no live session gets a 404. This route carries the same `wire: term`
// frames over a socket that requires no session.
//
// Lifecycle is still explicit: the client creates and closes shells through the
// same `open`/`new`/`close` ops, so a dropped socket never destroys a pty.
async fn shells_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(q): Query<ShellsQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let terms = d.shells.clone();
    ws.on_upgrade(move |socket| handle_shells_ws(socket, terms))
}

async fn handle_shells_ws(socket: WebSocket, terms: Arc<crate::term::SessionTerms>) {
    use base64::Engine;
    let (mut sender, mut receiver) = socket.split();
    let term_client = terms.subscribe();
    let push_terms = terms.clone();
    let mut term_seq: u64 = 0;

    // Reconnect support: report what already exists, so a client that
    // reattaches rebuilds its strip instead of showing nothing while the ptys
    // keep running.
    let existing: Vec<serde_json::Value> = terms
        .list()
        .into_iter()
        .map(|(id, alive)| serde_json::json!({ "id": id, "alive": alive }))
        .collect();
    let hello = serde_json::json!({ "wire": "term", "op": "list", "shells": existing });
    if let Ok(json) = serde_json::to_string(&hello) {
        if sender.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    let push = tokio::spawn(async move {
        loop {
            // Drain the PTY so idle shells still fire BEL / OSC.
            let _ = push_terms.take_snapshots();
            for (id, chunk, cols, rows, alive) in push_terms.poll_client(&term_client) {
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(t.as_str()) {
                    if val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        apply_term_client(&terms, &val);
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
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
    let (history_tx, history_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize)>();
    let history_request_tx = history_tx.clone();

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
        let mut history_rx = history_rx;
        let term_client = terms.as_ref().map(|t| t.subscribe());
        let mut last_state_fingerprint = None;
        let mut last_events: Vec<crate::harness::HarnessEvent> = Vec::new();
        let mut last_event_offset = 0usize;
        let mut last_stream_fp: u64 = 0;
        let mut last_queue_revision = 0;
        let mut attach_revision: u64 = 0;
        let mut term_seq: u64 = 0;
        loop {
            let queue_revision = daemon.queue_revision.load(Ordering::Acquire);
            if let Some(fingerprint) = session_state_fingerprint(&daemon.store, &state_path) {
                if Some(fingerprint) != last_state_fingerprint
                    || queue_revision != last_queue_revision
                {
                    if let Some(mut state) = read_session_state(&state_path) {
                        let hidden = {
                            let mut overlays = daemon.queue_hidden.lock().unwrap();
                            let entries = overlays.entry(session.clone()).or_default();
                            entries
                                .retain(|id| state.queued_inputs.iter().any(|item| &item.id == id));
                            entries.clone()
                        };
                        if !hidden.is_empty() {
                            state
                                .queued_inputs
                                .retain(|item| !hidden.contains(&item.id));
                        }
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
                            let first_attach = last_events.is_empty();
                            let snapshot = if first_attach {
                                const INITIAL_ATTACH_EVENTS: usize = 160;
                                let start = count.saturating_sub(INITIAL_ATTACH_EVENTS);
                                last_event_offset = start;
                                last_events = state.events[start..].to_vec();
                                if let Some(o) = v.as_object_mut() {
                                    o.insert(
                                        "events".into(),
                                        serde_json::to_value(&state.events[start..])
                                            .unwrap_or_default(),
                                    );
                                    o.insert("event_offset".into(), serde_json::json!(start));
                                }
                                true
                            } else {
                                count < last_event_offset
                                    || count < last_event_offset + last_events.len()
                                    || state.events
                                        [last_event_offset..last_event_offset + last_events.len()]
                                        != last_events[..]
                            };
                            attach_revision = attach_revision.wrapping_add(1);
                            if let Some(o) = v.as_object_mut() {
                                o.insert("revision".into(), serde_json::json!(attach_revision));
                                if snapshot {
                                    o.insert("wire".into(), serde_json::json!("snapshot"));
                                } else {
                                    let start = last_event_offset + last_events.len();
                                    let tail = serde_json::to_value(&state.events[start..])
                                        .unwrap_or_default();
                                    o.remove("events");
                                    o.insert("wire".into(), serde_json::json!("delta"));
                                    o.insert("new_events".into(), tail);
                                    o.insert(
                                        "event_count".into(),
                                        serde_json::json!(count - last_event_offset),
                                    );
                                }
                            }
                            if !first_attach {
                                last_events = state.events[last_event_offset..].to_vec();
                            }
                            last_queue_revision = queue_revision;
                            if let Ok(json) = serde_json::to_string(&v) {
                                if sender.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                                last_state_fingerprint = Some(fingerprint);
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
            if let Ok((before, limit)) = history_rx.try_recv() {
                if let Some(state) = read_session_state(&state_path) {
                    let (start, end, has_older) =
                        session_event_page(&state.events, Some(before), Some(limit));
                    let frame = serde_json::json!({
                        "wire": "history",
                        "events": &state.events[start..end],
                        "start": start,
                        "end": end,
                        "has_older": has_older,
                    });
                    if sender
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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
                    if val.get("kind").and_then(|k| k.as_str()) == Some("history") {
                        let before =
                            val.get("before").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let limit =
                            val.get("limit").and_then(|v| v.as_u64()).unwrap_or(160) as usize;
                        let _ = history_request_tx.send((before, limit));
                        continue;
                    }
                    if let Some(nonce) = val.get("nonce").and_then(|n| n.as_str()) {
                        if let Some(kind) = val.get("kind").and_then(|k| k.as_str()) {
                            // User messages and approval/question answers are both
                            // retried across reconnects and must be idempotent.
                            let idempotent = matches!(
                                kind,
                                "user_message"
                                    | "answer"
                                    | "approve"
                                    | "approve_all"
                                    | "deny"
                                    | "queue"
                                    | "unqueue"
                                    | "steer_queued"
                                    | "drop_queued"
                            );
                            if idempotent && !daemon.accept_nonce(&session, nonce) {
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

// WS /events — device-wide firehose. Emits a compact event on status changes
// (including running) so the session list can update live, even for chats the
// app isn't painting. `notify` is true when the event should also raise an OS
// banner; the app still receives every frame for UI.
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

fn allow_device_event(daemon: &Daemon, event: &serde_json::Value) -> bool {
    let settings = mission_control::load_settings(&daemon.mission_control_root);
    if settings.notification_policy == "none" {
        return false;
    }
    if settings.notification_policy != "mission_control_only" {
        return true;
    }
    let session = event.get("session").and_then(|v| v.as_str()).unwrap_or("");
    settings.mission_control_session_id.as_deref() == Some(session)
        || session == mission_control::SESSION_ID
}

async fn handle_events_ws(socket: WebSocket, daemon: Shared) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = subscribe_device_events();
    let push = tokio::spawn(async move {
        // Idle PTYs still need a pump so BEL / OSC fire when nobody is attached.
        // 5s is plenty — the firehose itself is push, not this tick.
        let mut harvest = tokio::time::interval(Duration::from_secs(5));
        harvest.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Ok(e) => {
                            let mut e = e;
                            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                            // Always push to the UI. OS banners stay policy-gated
                            // and never fire just because a chat started running.
                            let notify = kind != "running" && kind != "models" && allow_device_event(&daemon, &e);
                            if let Some(obj) = e.as_object_mut() {
                                obj.insert("notify".into(), serde_json::Value::Bool(notify));
                            }
                            if sender
                                .send(Message::Text(e.to_string().into()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
                _ = harvest.tick() => {
                    let live = daemon.sessions.lock().await;
                    for sess in live.values() {
                        sess.terms.pump();
                    }
                }
            }
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
            shells: crate::term::SessionTerms::new(global_shell_cwd()),
            git_write: Mutex::new(()),
            browser: BrowserManager::default(),
            seen_nonces: std::sync::Mutex::new(HashMap::new()),
            queue_hidden: std::sync::Mutex::new(HashMap::new()),
            queue_revision: AtomicU64::new(0),
            mission_control_root: tempfile::tempdir().expect("temporary directory").keep(),
            store: crate::store::Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(256).0,
            recurring_root: tempfile::tempdir().expect("temporary directory").keep(),
        }
    }

    #[test]
    fn session_event_page_returns_bounded_backward_cursor() {
        let events = vec![
            crate::harness::HarnessEvent::UserInput { text: "one".into() },
            crate::harness::HarnessEvent::AssistantText { text: "two".into() },
            crate::harness::HarnessEvent::UserInput {
                text: "three".into(),
            },
        ];
        assert_eq!(session_event_page(&events, None, None), (0, 3, false));
        assert_eq!(session_event_page(&events, Some(3), Some(2)), (1, 3, true));
        assert_eq!(
            session_event_page(&events, Some(1), Some(50)),
            (0, 1, false)
        );
        assert_eq!(
            session_event_page(&events, Some(999), Some(0)),
            (2, 3, true)
        );
    }

    /// A nonce sidecar is ignored: the store is the only record now.
    ///
    /// Retirement guard. The fallback used to read a session's `.nonces.json`
    /// and treat anything in it as already-sent. If that read ever came back,
    /// a stale file would silently suppress a legitimate request — the failure
    /// would look like "my message vanished", with nothing in the logs.
    #[test]
    fn a_legacy_nonces_file_is_ignored() {
        let dir = tempdir().expect("temporary directory");
        let session_id = "ws-2-def456";
        let session_dir = dir.path().join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("state.nonces.json"),
            r#"["already-sent"]"#,
        )
        .unwrap();

        let mut daemon = test_daemon();
        daemon.store = crate::store::Store::open_in_memory().unwrap();

        assert!(
            daemon.accept_nonce(session_id, "already-sent"),
            "a nonce present only in a legacy file must be ACCEPTED — the file is retired"
        );
        assert!(
            !daemon.accept_nonce(session_id, "already-sent"),
            "but the store still rejects a genuine replay within the same run"
        );
    }

    #[test]
    fn nonce_is_rejected_after_daemon_state_is_recreated() {
        let mut first = test_daemon();
        first.store = crate::store::Store::open_in_memory().unwrap();
        let store = first.store.clone();

        assert!(first.accept_nonce("session", "nonce-1"));
        assert!(!first.accept_nonce("session", "nonce-1"));

        let mut restarted = test_daemon();
        restarted.store = store;
        assert!(!restarted.accept_nonce("session", "nonce-1"));
        assert!(restarted.accept_nonce("session", "nonce-2"));
    }

    // -- Coordination route handlers -----------------------------------------
    // These drive the real axum handlers (with their extractors) so auth,
    // validation, and store wiring are covered, not just the DB beneath them.

    fn authed_daemon() -> Shared {
        let mut daemon = test_daemon();
        daemon.token = "test-token".into();
        Arc::new(daemon)
    }

    fn with_token() -> Auth {
        Auth {
            token: Some("test-token".into()),
        }
    }

    fn create_agent_req(id: &str) -> AgentReq {
        AgentReq {
            id: id.into(),
            display_name: "Web Research Specialist".into(),
            handle: format!("handle-{id}"),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            role: crate::coordination::types::AgentRole::Researcher,
            capabilities: vec!["web_search".into()],
        }
    }

    fn agents_query(token: Option<&str>) -> AgentsQuery {
        AgentsQuery {
            token: token.map(str::to_string),
            after_name: None,
            after_id: None,
            limit: default_agent_page_limit(),
        }
    }

    #[tokio::test]
    async fn agents_route_rejects_an_unauthenticated_request() {
        let d = authed_daemon();
        let response = list_agents(State(d), Query(agents_query(None))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn agents_route_requires_id_and_handle() {
        let d = authed_daemon();
        let mut req = create_agent_req("researcher");
        req.handle = "  ".into();
        let response = create_agent(State(d), Query(with_token()), Json(req)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn agents_route_creates_then_lists() {
        let d = authed_daemon();
        let created = create_agent(
            State(d.clone()),
            Query(with_token()),
            Json(create_agent_req("researcher")),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);

        // A duplicate id is a conflict, not a silent overwrite.
        let duplicate = create_agent(
            State(d.clone()),
            Query(with_token()),
            Json(create_agent_req("researcher")),
        )
        .await;
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);

        let listed = list_agents(State(d), Query(agents_query(Some("test-token")))).await;
        assert_eq!(listed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn message_route_requires_a_body() {
        let d = authed_daemon();
        let response = coordination_post_message(
            State(d),
            Query(with_token()),
            axum::extract::Path("t1".to_string()),
            Json(CoordinationMessageReq {
                actor_kind: "agent".into(),
                actor_id: "mission-control".into(),
                body: "   ".into(),
                idempotency_key: String::new(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A human post must wake Mission Control (that is what makes the board
    /// interactive); Mission Control's own reply must not, or it would loop.
    #[test]
    fn board_posts_wake_mission_control_except_its_own() {
        assert!(should_wake_mission_control("human"));
        assert!(should_wake_mission_control("rust-pr-reviewer"));
        assert!(!should_wake_mission_control(
            crate::mission_control::SESSION_ID
        ));
    }

    #[test]
    fn board_message_envelope_carries_sender_thread_and_reply_rule() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: COORDINATION_THREAD.into(),
            partition_key: format!("thread:{COORDINATION_THREAD}"),
            sequence: 7,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "please investigate X"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = board_message_envelope(&event, &[]);
        assert!(envelope.starts_with("[coordination_board_message]"));
        assert!(envelope.contains("thread_id: system"));
        assert!(envelope.contains("from_id: human"));
        assert!(envelope.contains("from_kind: human"));
        assert!(envelope.contains("body: please investigate X"));
        // The reply contract is what stops a silent no-op.
        assert!(envelope.contains("post_coordination_message"));
        assert!(envelope.contains("[/coordination_board_message]"));
    }

    /// The body is last so a client can read everything up to the closing tag as
    /// the message; this pins that ordering so it can't silently regress.
    #[test]
    fn board_message_body_is_the_final_field_before_the_closing_tag() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e2".into(),
            thread_id: COORDINATION_THREAD.into(),
            partition_key: format!("thread:{COORDINATION_THREAD}"),
            sequence: 8,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "line one\nline two"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k2".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = board_message_envelope(&event, &[]);
        let after_body = envelope.split_once("body: ").expect("body field present").1;
        assert!(after_body.starts_with("line one\nline two"));
        assert!(
            after_body
                .trim_end()
                .ends_with("[/coordination_board_message]")
        );
    }

    /// The wake must read as a group room: recent messages are included, and the
    /// digest can't be confused with the new message or the field layout.
    #[test]
    fn board_message_envelope_includes_bounded_history() {
        let prior =
            |seq: u64, who: &str, body: &str| crate::coordination::types::CoordinationEvent {
                event_id: format!("e{seq}"),
                thread_id: COORDINATION_THREAD.into(),
                partition_key: format!("thread:{COORDINATION_THREAD}"),
                sequence: seq,
                event_type: "message.posted".into(),
                actor_kind: "agent".into(),
                actor_id: who.into(),
                payload_version: 1,
                payload: serde_json::json!({"body": body}),
                causation_id: None,
                correlation_id: None,
                idempotency_key: format!("k{seq}"),
                created_at: "2020-01-01T00:00:00Z".into(),
            };
        let current = prior(5, "human", "what is the status?");
        let history = [
            prior(3, "mission-control", "started the review"),
            prior(4, "reviewer", "found two issues"),
        ];

        let envelope = board_message_envelope(&current, &history);
        // Both prior turns appear, attributed.
        assert!(envelope.contains("mission-control: started the review"));
        assert!(envelope.contains("reviewer: found two issues"));
        assert!(envelope.contains("history: last 2 message(s)"));
        // The new message is still the final body, and history precedes it.
        let body_at = envelope.find("body: what is the status?").unwrap();
        let history_at = envelope.find("history:").unwrap();
        assert!(history_at < body_at);
        // A multi-line prior body is collapsed so it can't break the layout.
        let wrapped = board_message_envelope(&current, &[prior(1, "a", "line one\nline two")]);
        assert!(wrapped.contains("a: line one line two"));
    }

    /// With no history the room is honestly reported as just starting.
    #[test]
    fn board_message_envelope_marks_an_empty_room() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: COORDINATION_THREAD.into(),
            partition_key: format!("thread:{COORDINATION_THREAD}"),
            sequence: 1,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "first ever message"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let envelope = board_message_envelope(&event, &[]);
        assert!(envelope.contains("history: (start of the room)"));
    }

    #[tokio::test]
    async fn message_route_publishes_to_the_live_event_feed() {
        let d = authed_daemon();
        // The websocket handler forwards everything from this broadcast channel,
        // so receiving here proves a posted event reaches live subscribers.
        let mut feed = d.coordination_events.subscribe();

        coordination_post_message(
            State(d.clone()),
            Query(with_token()),
            axum::extract::Path("live".to_string()),
            Json(CoordinationMessageReq {
                actor_kind: "agent".into(),
                actor_id: "mission-control".into(),
                body: "hello live".into(),
                idempotency_key: String::new(),
            }),
        )
        .await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), feed.recv())
            .await
            .expect("event delivered to live subscribers")
            .expect("channel open");
        assert_eq!(event.thread_id, "live");
        assert_eq!(event.payload["body"], "hello live");
    }

    #[tokio::test]
    async fn message_route_posts_and_replays_by_cursor() {
        let d = authed_daemon();
        for body in ["first", "second"] {
            let posted = coordination_post_message(
                State(d.clone()),
                Query(with_token()),
                axum::extract::Path("t1".to_string()),
                Json(CoordinationMessageReq {
                    actor_kind: "agent".into(),
                    actor_id: "mission-control".into(),
                    body: body.into(),
                    idempotency_key: String::new(),
                }),
            )
            .await;
            assert_eq!(posted.status(), StatusCode::OK);
        }

        // Full replay returns both; a cursor past the first returns only the tail.
        let all = d.store.events_for_thread("t1", 0, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].sequence, 1);
        assert_eq!(all[1].sequence, 2);
        let tail = d.store.events_for_thread("t1", 1, 10).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, 2);
    }

    // -- Coordination visibility routes -------------------------------------





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
        // `pending` is what clients sent when `Todo` was named `Pending`; accept
        // it permanently so a stored client config does not silently stop working.
        "pending" | "todo" => Some(TaskStatus::Todo),
        "in_progress" => Some(TaskStatus::InProgress),
        "blocked" => Some(TaskStatus::Blocked),
        "done" | "completed" => Some(TaskStatus::Done),
        "failed" => Some(TaskStatus::Failed),
        "cancelled" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

fn mission_task_view(task: &Task) -> serde_json::Value {
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
        "handoff": task.handoff,
        "handoff_mode": task.handoff_mode,
        "result": task.result,
        "notifications": task.notifications,
        "dispatch_failures": task.dispatch_failures,
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
    let Ok(tasks) = d.store.list_tasks(None, None) else {
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
    match d.store.list_tasks(None, None) {
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
        Err(error) => mission_error(error.to_string()),
    }
}

async fn dispatch_mission_task(d: &Daemon, task_id: &str) -> Result<Task, String> {
    let root = &d.mission_control_root;
    let now = chrono::Utc::now().to_rfc3339();
    // Atomic claim: Todo → InProgress exactly once. Concurrent dispatchers
    // (loop + REST paths) lose the race here and return without delivering.
    let task = d
        .store
        .claim_task_for_dispatch(task_id, &now)
        .map_err(|error| error.to_string())?;
    if task.status != TaskStatus::InProgress || task.reporting_session.is_none() {
        // Not claimed by us (already in progress/terminal) — nothing to do.
        return Ok(task);
    }
    if task.session_id.trim().is_empty() {
        release_failed_claim(d, &task, "task has no target session")?;
        return Err("task has no target session".to_string());
    }
    // Blocked-by edges live in task_links now, so a dependency is read as the
    // reverse edge rather than an embedded field on the task row.
    for blocker_id in d
        .store
        .blockers_of(task_id)
        .map_err(|error| error.to_string())?
    {
        let other = match d.store.get_task(&blocker_id) {
            Ok(Some(other)) => other,
            Ok(None) => {
                let error = format!("dependency {blocker_id} no longer exists");
                release_failed_claim(d, &task, &error)?;
                return Err(error);
            }
            Err(error) => {
                let message = error.to_string();
                release_failed_claim(d, &task, &message)?;
                return Err(message);
            }
        };
        if other.status != TaskStatus::Done {
            let reason = format!("waiting on dependency {blocker_id}");
            let blocked = d
                .store
                .update_task_in(task_id, &now, |task| {
                    task.status = TaskStatus::Blocked;
                    task.reporting_session = None;
                    task.notifications.push(NotificationMarker {
                        target: "mission_control".to_string(),
                        kind: "blocked".to_string(),
                        message: reason.clone(),
                        delivered: false,
                    });
                })
                .map_err(|error| error.to_string())?;
            // Report the stored (blocked) state, not the pre-update claim.
            return Ok(blocked);
        }
    }
    let managed = match mission_control::get_session(root, &task.session_id) {
        Ok(managed) => managed,
        Err(error) => {
            release_failed_claim(d, &task, &error)?;
            return Err(error);
        }
    };
    if managed.status != mission_control::SessionStatus::Active {
        let error = "target session is archived".to_string();
        release_failed_claim(d, &task, &error)?;
        return Err(error);
    }
    let conflicts = d
        .store
        .task_path_conflicts(task_id, &task.owned_paths)
        .map_err(|error| error.to_string())?;
    if !conflicts.is_empty() {
        // Queue behind the current owner. This is waiting, not a dispatch
        // failure — do not burn the retry ceiling.
        let mut owners = Vec::new();
        for conflict in &conflicts {
            if !owners.contains(&conflict.0) {
                owners.push(conflict.0.clone());
            }
        }
        let reason = format!("waiting on workspace owner(s): {}", owners.join(", "));
        let blocked = d
            .store
            .update_task_in(task_id, &now, |task| {
                task.status = TaskStatus::Blocked;
                task.reporting_session = None;
                task.dispatch_failures = 0;
                if !task.notifications.iter().any(|n| n.message == reason) {
                    task.notifications.push(NotificationMarker {
                        target: "mission_control".to_string(),
                        kind: "blocked".to_string(),
                        message: reason.clone(),
                        delivered: false,
                    });
                }
            })
            .map_err(|error| error.to_string())?;
        // Record the ordering constraint so `unblock_ready_tasks` can requeue
        // this once the owner finishes — the edge IS the dependency.
        for owner in &owners {
            let _ = d.store.link_tasks(&TaskLink {
                from_task_id: owner.clone(),
                to_task_id: task_id.to_string(),
                kind: TaskLinkKind::Blocks,
                created_at: now.clone(),
            });
        }
        return Ok(blocked);
    }
    let handoff = task
        .handoff
        .as_ref()
        .map(|handoff| handoff.description.as_str())
        .unwrap_or("");
    // Fresh-mode handoffs must be self-contained briefings; resume-mode ones
    // assume the session already holds the context.
    let mode_line = match task.handoff_mode {
        HandoffMode::Fresh => {
            "handoff_mode: fresh (self-contained briefing; the session has no prior context)\n"
        }
        HandoffMode::Resume => "",
    };
    let text = format!(
        "[mission_control_task]\ntask_id: {}\ntitle: {}\n{}scope: {}\nworkspace: {}\nexpected_report: scope done; files changed; verification; blockers\nrules: do not confirm scope; begin immediately. If handoff_mode is fresh, this envelope is the complete briefing — do not ask for missing history. Stay in this session — it already has the context; do not spawn lanes unless the work is independently parallel. Stay in scope; do not manage other sessions. If a prior read-only report is gone, redo the evaluation from current sources and deliver it. You MUST call report_mission_task for task_id {} before you stop — even on a clean success with no errors. Status done if finished; blocked if you need a unique artifact or user decision; failed only for a hard stop. A silent finish leaves Mission Control unable to resume. Block only for a missing unique artifact or user decision, not compacted history.\n[/mission_control_task]",
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
    // Apply the profile BEFORE delivering, so the turn that picks up this
    // envelope runs on the model the dispatcher chose. A failure here is a
    // dispatch failure, not a silent downgrade: the task is released and
    // retried rather than delivered onto the wrong model.
    //
    // `persist: false` — this is a model for ONE assignment, not the session's
    // new default. Mission Control does not own the chat's model, and the next
    // task may want a different one, so the choice drives this run only.
    if let Some(profile) = task.profile.as_deref() {
        if let Err(error) = d.run_session_on_profile(&managed.id, profile, false).await {
            release_failed_claim(d, &task, &error)?;
            return Err(error);
        }
    }
    d.deliver(&managed.id, LoopInput::UserMessage(text)).await;
    // Record that this went out, on Mission Control's transcript, WITHOUT waking
    // it: the work is already routed and reports back on its own. This is what
    // keeps the coordinator's record complete when the user filed the task
    // directly rather than through Mission Control.
    record_dispatch_notice(d, &task).await;
    // Delivery succeeded — clear the retry counter so the ceiling stays
    // "consecutive failures" as documented.
    let task = d
        .store
        .update_task_in(task_id, &now, |t| t.dispatch_failures = 0)
        .map_err(|error| error.to_string())?;
    Ok(task)
}

/// Roll a claimed-but-undeliverable task back to Todo and record why, so the
/// loop can retry later instead of silently spinning with a stale claim. After
/// `MAX_DISPATCH_FAILURES` consecutive failures the task parks as Blocked so it
/// stops consuming the loop.
fn release_failed_claim(d: &Daemon, task: &Task, error: &str) -> Result<(), String> {
    const MAX_DISPATCH_FAILURES: u32 = 5;
    let now = chrono::Utc::now().to_rfc3339();
    d.store
        .update_task_in(&task.id, &now, |t| {
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
                t.status = TaskStatus::Todo;
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[derive(Deserialize)]
struct AgentBuildReq {
    prompt: String,
}

async fn build_agent_from_prompt(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<AgentBuildReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let prompt = req.prompt.trim();
    if prompt.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            "prompt must describe the desired agent",
        )
            .into_response();
    }
    let root = &d.mission_control_root;
    // Agent builds are routed through the dedicated MC session. Ensure that
    // session is materialized before creating the task, including on a fresh
    // daemon where the user has not opened Mission Control yet.
    let _ = mission_control_open(
        State(d.clone()),
        Query(a.clone()),
        Json(MissionOpenReq { profile: None }),
    )
    .await;
    if mission_control::get_session(root, crate::mission_control::SESSION_ID).is_err() {
        if let Err(error) = mission_control::create_session(
            root,
            crate::mission_control::SESSION_ID,
            "Mission Control",
            &crate::mission_control::workspace_path(),
        ) {
            return mission_error(format!("initialize Mission Control session: {error}"));
        }
    }
    let session_id = crate::mission_control::SESSION_ID;
    let task_id = uuid::Uuid::new_v4().to_string();
    let title = "Build specialized agent";
    let description = format!(
        concat!(
            "Build a specialized agent from this user brief:\n\n{}\n\n",
            "[AGENT_BUILD_JOB — not a project or workspace request]\n",
            "You are the agent builder. Do not create a project, do not create a new Mission Control session, do not ask the user to choose or confirm a folder, and do not route this request as ordinary work. Build the agent directly from the brief.\n",
            "Build the shared specialized-session path first: every specialized agent receives the established session system prompt plus a researched, bounded identity.md overlay. Keep scheduling, turn-taking, handoffs, and state in shared runtime code; do not create per-agent role modules or runtime.json. Then create the durable agent home under ~/.snippet/agents/<agent-id>/ and write identity.md. That markdown is the agent's whole identity — there is no profile or identity JSON to write, and the directory entry is recorded by registration. Tools are only narrow executable boundaries such as shell, third-party API, MCP, or vault-backed operations; do not emit workflow helpers as tools. Use web_search/web_read for research only when those schemas are present. Do not execute generated tools.\n",
            "When finished, report the exact agent id, shared-session validation, files created, research sources, executable tool proposals, and blockers. Call report_mission_task with the final result. If the brief is insufficient, make sensible defaults rather than asking a workspace question."
        ),
        prompt
    );
    // The task lands in the SQLite store with the rest of coordination. It used
    // to be written to the JSON store while the dispatch path claimed from
    // SQLite, so this build could never actually dispatch.
    let task = Task::dispatched_to(
        task_id,
        session_id.to_string(),
        title.to_string(),
        description,
        Vec::new(),
        HandoffMode::Resume,
        "agent",
        crate::mission_control::SESSION_ID,
        chrono::Utc::now().to_rfc3339(),
    );
    if let Err(error) = d.store.create_task(&task) {
        return mission_error(error.to_string());
    }
    match dispatch_mission_task(&d, &task.id).await {
        Ok(task) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "build_id": task.id,
                "status": format!("{:?}", task.status).to_lowercase(),
                "task_id": task.id,
            })),
        )
            .into_response(),
        Err(error) => mission_error(error),
    }
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
    let task = crate::coordination::Task::dispatched_to(
        id,
        req.session_id.clone(),
        req.title.trim().to_string(),
        req.description.trim().to_string(),
        owned_paths,
        HandoffMode::Resume,
        "mission-control",
        "mission-control",
        chrono::Utc::now().to_rfc3339(),
    );
    if let Err(error) = d.store.create_task(&task) {
        return mission_error(error.to_string());
    }
    let task = if req.status.as_deref() == Some("pending") {
        task
    } else {
        match dispatch_mission_task(&d, &task.id).await {
            Ok(task) => task,
            Err(error) => {
                let now = chrono::Utc::now().to_rfc3339();
                match d.store.update_task_in(&task.id, &now, |task| {
                    task.status = TaskStatus::Blocked;
                    task.reporting_session = None;
                    task.notifications.push(NotificationMarker {
                        target: "mission_control".to_string(),
                        kind: "blocked".to_string(),
                        message: error,
                        delivered: false,
                    });
                }) {
                    Ok(task) => task,
                    Err(error) => return mission_error(error.to_string()),
                }
            }
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
    // Read the task from the SQLite store — the same store the dispatch path
    // claims from. Reading the JSON store here meant an update could target a
    // record the dispatcher never saw.
    let existing = match d.store.get_task(&id) {
        Ok(Some(task)) => task,
        Ok(None) => return mission_error("unknown task".to_string()),
        Err(error) => return mission_error(error.to_string()),
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
        Some(mode_raw) => match HandoffMode::parse(mode_raw) {
            Some(mode) => Some(mode),
            None => {
                return mission_error(format!(
                    "handoff_mode must be 'resume' or 'fresh', got '{mode_raw}'"
                ));
            }
        },
    };
    let updated = d
        .store
        .update_task_in(&id, &chrono::Utc::now().to_rfc3339(), |task| {
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
                // Read the flags BEFORE the move: `task.status = status`
                // consumes it, and `status.is_terminal()` would then borrow a
                // moved value.
                let clears_binding = status.is_terminal() || status == TaskStatus::Blocked;
                task.status = status;
                if clears_binding {
                    task.reporting_session = None;
                }
            }
        });
    match updated {
        Ok(task) if task.status == TaskStatus::Todo => match dispatch_mission_task(&d, &id).await {
            Ok(task) => Json(mission_task_view(&task)).into_response(),
            Err(error) => mission_error(error),
        },
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error.to_string()),
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
    match d.store.complete_task(
        &id,
        TaskStatus::Cancelled,
        TaskResult {
            summary: "Archived by Mission Control.".to_string(),
            ..Default::default()
        },
        &chrono::Utc::now().to_rfc3339(),
    ) {
        Ok(task) => Json(mission_task_view(&task)).into_response(),
        Err(error) => mission_error(error.to_string()),
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
    let tasks = d.store.list_tasks(None, None).unwrap_or_default();
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
    write_session_sidecar(
        &state_path,
        &SessionSidecar {
            role: SessionRole::MissionControl,
            agent_id: None,
        },
    );
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

// The session role sidecar lives in `session.rs`: it is written by the daemon
// and READ by the session list, so it cannot belong to either one. This module
// used to own it and hand-parse the grammar, which is how the reader here and
// the reader in `session.rs` came to disagree.

/// Give the built-in agents a directory row so a peer can ADDRESS them.
///
/// Both built-ins are registered here, and the distinction between them is the
/// point: Mission Control is the coordinator that dispatches work; `snippet` is
/// the general coding agent that work is dispatched TO by default. They are two
/// identities, not one identity with a fallback.
///
/// A row alone is not enough for `snippet`: a turn is held by an agent identity,
/// and `ensure_agent_session` refuses an agent with no identity home, so the home
/// is materialized here too. Without it the default agent could be named and
/// assigned to but never actually start.
///
/// Idempotent by construction — this runs on every boot, so it uses
/// `upsert_agent` (a plain INSERT would fail the primary key on the second
/// start) and `ensure_layout` (which only writes a home that is missing).
fn register_builtin_agents(d: &Shared) {
    let coordinator = crate::coordination::types::Agent {
        id: crate::mission_control::SESSION_ID.to_string(),
        display_name: "Mission Control".into(),
        handle: "mission-control".into(),
        kind: crate::coordination::types::AgentKind::MissionControl,
        status: crate::coordination::types::AgentStatus::Active,
        role: crate::coordination::types::AgentRole::Coordinator,
        capabilities: vec![
            "orchestration".into(),
            "agent-directory".into(),
            "task-dispatch".into(),
        ],
    };
    let default_worker = crate::coordination::types::Agent {
        id: crate::coordination::SNIPPET_AGENT_ID.to_string(),
        display_name: "Snippet".into(),
        handle: "snippet".into(),
        kind: crate::coordination::types::AgentKind::Worker,
        status: crate::coordination::types::AgentStatus::Active,
        role: crate::coordination::types::AgentRole::Implementer,
        capabilities: vec![
            "coding".into(),
            "bash".into(),
            "files".into(),
            "web-search".into(),
        ],
    };
    for agent in [&coordinator, &default_worker] {
        // The default worker needs a home before it can hold a turn; the
        // coordinator works through the board and never takes one.
        if agent.id == crate::coordination::SNIPPET_AGENT_ID {
            let home = match crate::coordination::AgentHome::new(
                crate::coordination::agents_root(&d.mission_control_root),
                &agent.id,
            ) {
                Ok(home) => home,
                Err(error) => {
                    eprintln!(
                        "[coordination] invalid built-in agent id `{}`: {error}",
                        agent.id
                    );
                    continue;
                }
            };
            let identity = "# Snippet\n\nYou are Snippet, the general coding agent. You own work \
                            end to end: read the relevant code before you change it, make the \
                            smallest change that achieves the goal, and verify it with the \
                            narrowest check that proves it.\n\nThis identity is the default. \
                            Mission Control dispatches work here unless a user names a more \
                            specialized agent.\n";
            if let Err(error) = home.ensure_layout(identity) {
                eprintln!(
                    "[coordination] could not create the home for agent `{}`: {error}",
                    agent.id
                );
                continue;
            }
        }
        if let Err(error) = d.store.upsert_agent(agent) {
            // A failure here must not stop the daemon: the board and sessions work
            // without the directory row. The agent just cannot be addressed until
            // the next successful boot.
            eprintln!(
                "[coordination] could not register agent `{}`: {error}",
                agent.id
            );
        }
    }
}

/// Dispatch loop: claims and delivers every Pending task, then re-queues any
/// Blocked tasks whose dependencies have completed. Failures roll the claim
/// back with a retry counter; at the ceiling the task parks as Blocked.
async fn mission_control_dispatch_loop(daemon: Shared) {
    loop {
        let now = chrono::Utc::now().to_rfc3339();
        let _ = daemon.store.unblock_ready_tasks(&now);
        let tasks = daemon
            .store
            .list_tasks(None, Some(&TaskStatus::Todo))
            .unwrap_or_default();
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
    let Ok(tasks) = daemon.store.list_tasks(None, None) else {
        return;
    };
    for task in tasks {
        for (index, marker) in task.notifications.iter().enumerate() {
            if marker.delivered || marker.target != "mission_control" {
                continue;
            }
            let text = format!(
                "[mission_task_report]\ntask_id: {}\ntitle: {}\nstatus: {}\nsummary: {}\n[/mission_task_report]",
                task.id, task.title, marker.kind, marker.message
            );
            daemon
                .deliver(mission_control::SESSION_ID, LoopInput::UserMessage(text))
                .await;
            let _ = daemon
                .store
                .update_task_in(&task.id, &chrono::Utc::now().to_rfc3339(), |t| {
                    if let Some(n) = t.notifications.get_mut(index) {
                        n.delivered = true;
                    }
                });
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

fn session_busy_for_recurring(id: &str) -> bool {
    let Some(sp) = state_path_for_id(id) else {
        return false;
    };
    let Some(state) = read_session_state(&sp) else {
        return false;
    };
    recurring::session_is_busy(&status_str(state.status))
        || state
            .lanes
            .iter()
            .any(|l| matches!(l.status, crate::lanes::LaneStatus::Running))
        || matches!(
            state.goal.as_ref().map(|g| g.status),
            Some(GoalStatus::Active | GoalStatus::Paused)
        )
}

async fn recurring_tick_loop(daemon: Shared) {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let due = recurring::due_jobs(&daemon.recurring_root, now).unwrap_or_default();
        for job in due {
            if session_busy_for_recurring(&job.session_id) {
                let _ = recurring::mark_queued(&daemon.recurring_root, &job.id);
                continue;
            }
            let workspace = state_path_for_id(&job.session_id)
                .and_then(|sp| read_session_state(&sp))
                .map(|s| PathBuf::from(s.workspace))
                .filter(|p| p.is_dir());
            match job.render_goal(workspace.as_deref()) {
                Ok(text) => {
                    // Scheduled jobs are goal-only: every fire sets an
                    // autonomous goal the agent drives to complete_goal and
                    // reports the outcome of.
                    daemon
                        .deliver(&job.session_id, LoopInput::SetGoal(text))
                        .await;
                    let _ = recurring::mark_fired(&daemon.recurring_root, &job.id, now);
                }
                Err(error) => {
                    let _ = recurring::mark_error(&daemon.recurring_root, &job.id, &error);
                }
            }
        }
        // Queued fires wait on an in-flight goal. Poll fast so the next goal
        // starts as soon as `complete_goal` persists Idle — not on the 15s tick.
        let wait = if recurring::has_queued(&daemon.recurring_root) {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(15)
        };
        tokio::time::sleep(wait).await;
    }
}

async fn list_recurring(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match recurring::list_jobs(&d.recurring_root) {
        Ok(jobs) => Json(jobs).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct RecurringCreateReq {
    #[serde(default)]
    title: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    schedule: String,
    /// `goal` (default) or `message` — scheduled one-off chat turns.
    #[serde(default)]
    delivery: Option<String>,
}

#[derive(Deserialize)]
struct RecurringUpdateReq {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn create_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<RecurringCreateReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let schedule = match Schedule::parse(&req.schedule) {
        Ok(s) => s,
        Err(error) => return mission_error(error),
    };
    let delivery = match req.delivery.as_deref() {
        None | Some("") | Some("goal") => recurring::Delivery::Goal,
        Some("message") => recurring::Delivery::Message,
        Some(other) => {
            return mission_error(format!("delivery must be goal or message, got `{other}`"));
        }
    };
    match recurring::create_job_with(
        &d.recurring_root,
        &req.title,
        &req.session_id,
        &req.prompt,
        schedule,
        req.plan_path.as_deref(),
        delivery,
    ) {
        Ok(job) => {
            Json(job).into_response()
        }
        Err(error) => mission_error(error),
    }
}

async fn update_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<RecurringUpdateReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let schedule = match req.schedule.as_deref() {
        Some(raw) if !raw.trim().is_empty() => match Schedule::parse(raw) {
            Ok(s) => Some(s),
            Err(error) => return mission_error(error),
        },
        _ => None,
    };
    match recurring::update_job(&d.recurring_root, &id, |job| {
        if let Some(title) = req.title.as_deref() {
            let title = title.trim();
            if !title.is_empty() {
                job.title = title.to_string();
            }
        }
        if let Some(session_id) = req.session_id.as_deref() {
            let session_id = session_id.trim();
            if !session_id.is_empty() {
                job.session_id = session_id.to_string();
            }
        }
        if let Some(prompt) = req.prompt.as_deref() {
            job.prompt = prompt.trim().to_string();
        }
        if let Some(plan_path) = req.plan_path.as_deref() {
            let trimmed = plan_path.trim();
            job.plan_path = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        if let Some(schedule) = schedule.clone() {
            job.schedule = schedule;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            job.next_run_at = job.schedule.next_after(now);
            job.queued = false;
        }
        if let Some(enabled) = req.enabled {
            job.enabled = enabled;
            if !enabled {
                job.queued = false;
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if job.next_run_at < now {
                    job.next_run_at = job.schedule.next_after(now);
                }
            }
        }
    }) {
        Ok(job) => {
            Json(job).into_response()
        }
        Err(error) => mission_error(error),
    }
}

async fn delete_recurring(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match recurring::delete_job(&d.recurring_root, &id) {
        Ok(()) => {
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(error) => mission_error(error),
    }
}
