use std::path::PathBuf;
use std::time::Duration;

use crate::config::SnippetConfig;
use crate::harness::LoopInput;
use super::app::*;
use super::settings::*;
use super::*;

impl App {
    pub(crate) fn open_profiles(&mut self) {
        self.options.config.ensure_setups();
        let names = self.options.config.profile_names();
        self.profiles_selected_index = self
            .options
            .config
            .active_setup
            .as_ref()
            .and_then(|a| names.iter().position(|n| n == a))
            .unwrap_or(0);
        self.screen = Screen::Profiles;
        self.input_clear();
    }

    /// Open the connect/editor form for a profile — `Some(name)` edits it, `None`
    /// starts a new one. Saving writes back to that profile (and activates it).
    pub(crate) fn open_profile_editor(&mut self, name: Option<String>) {
        self.original_config = Some(self.options.config.clone());
        let existing = name.as_ref().and_then(|n| {
            self.options
                .config
                .setups
                .as_ref()
                .and_then(|m| m.get(n))
                .cloned()
        });
        match existing {
            Some(cfg) => {
                self.form_provider = cfg.provider.clone();
                self.form_api_key = cfg.api_key.clone();
                self.form_model = cfg.model.clone();
                self.form_base_url = cfg.base_url.clone();
                self.form_reasoning_effort = cfg.reasoning_effort.clone();
                self.form_context_window = cfg.context_window.to_string();
                self.form_compact_at_pct = cfg.compact_at_pct.to_string();
                self.form_x_search = cfg.x_search;
            }
            None => {
                self.form_provider = "openai".to_string();
                let (base, model) = provider_defaults(&self.form_provider);
                self.form_base_url = base;
                self.form_model = model;
                self.form_api_key = String::new();
                self.form_reasoning_effort = Some("medium".to_string());
                self.form_x_search = false;
                let (context_window, compact_at_pct) =
                    provider_context_defaults(&self.form_provider);
                self.form_context_window = context_window.to_string();
                self.form_compact_at_pct = compact_at_pct.to_string();
            }
        }
        self.editing_profile = name;
        self.form_model_query = String::new();
        self.form_fetched_models = None;
        self.models_fetch_status = String::new();
        self.form_focus = SettingsField::Provider;
        self.return_to_profiles = true;
        self.login_active = true;
        self.screen = Screen::Profiles;
        self.input_clear();
    }

    /// The config the current chat's loop should run with: the global config, but
    /// with this conversation's persisted per-chat model override applied (if any).
    /// Same precedence as the serve daemon, so TUI and app agree.
    pub(crate) fn effective_config(&self) -> SnippetConfig {
        let mut cfg = self.options.config.clone();
        if let Some(name) = crate::session::read_session_profile(&self.active_state_path) {
            if let Some(m) = cfg.setups.as_ref().and_then(|s| s.get(&name)).cloned() {
                cfg.model = m;
                cfg.active_setup = Some(name);
            }
        }
        cfg
    }

    /// Re-resolve the cached (provider, model) for the active chat. Call after
    /// anything that can change it: session switch, profile activation, /model.
    pub(crate) fn refresh_effective_model(&mut self) {
        let cfg = self.effective_config();
        self.effective_model = (cfg.model.provider.clone(), cfg.model.model.clone());
    }

    /// Set a profile as the GLOBAL default (the model new chats use). A chat that
    /// has its own per-chat override keeps it (override wins), so we only restart
    /// the current loop when it has no override.
    pub(crate) fn activate_profile(&mut self, name: &str) {
        // Switching the model restarts the loop — never kill a mid-turn run (a
        // /goal or lane could be working). Same guard /model uses.
        if self.agent_busy() {
            self.status = "agent is working — stop it (Esc) before switching models".to_string();
            return;
        }
        if self.options.config.activate(name) {
            let _ = self.save_config_file();
            let has_override =
                crate::session::read_session_profile(&self.active_state_path).is_some();
            let resumed = if has_override {
                false
            } else {
                self.restart_loop_for_config()
            };
            self.screen = Screen::Main;
            self.refresh_effective_model();
            self.status = if has_override {
                format!(
                    "✓ global default · {} · {} (this chat keeps its own model)",
                    self.options.config.model.provider, self.options.config.model.model,
                )
            } else {
                format!(
                    "✓ {} · {}{}",
                    self.options.config.model.provider,
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                )
            };
        } else {
            // Feedback even on the no-op path — a silent Enter reads as "broken".
            self.status = format!("profile `{name}` not found (or already active)");
        }
    }

    /// Toggle a profile as the delegation model — the one `delegate_task` sub-agents
    /// run on. Selecting the current delegate clears it (delegation falls back to the
    /// active model). Takes effect on the next session/lane; no running loop is killed.
    pub(crate) fn toggle_delegate_profile(&mut self, name: &str) {
        let cfg = &mut self.options.config;
        if cfg.delegate_setup.as_deref() == Some(name) {
            cfg.delegate_setup = None;
            self.status = format!("delegation → active model (cleared “{name}”)");
        } else if cfg.setups.as_ref().is_some_and(|m| m.contains_key(name)) {
            cfg.delegate_setup = Some(name.to_string());
            self.status = format!("✓ lanes delegate to “{name}”");
        } else {
            self.status = format!("profile `{name}` not found");
            return;
        }
        let _ = self.save_config_file();
    }

    /// Set a profile for THIS chat only (a persisted per-conversation override),
    /// without changing the global default. Restarts the chat's loop with it.
    pub(crate) fn activate_profile_local(&mut self, name: &str) {
        if self.agent_busy() {
            self.status = "agent is working — stop it (Esc) before switching models".to_string();
            return;
        }
        let Some(model) = self
            .options
            .config
            .setups
            .as_ref()
            .and_then(|m| m.get(name))
            .cloned()
        else {
            self.status = format!("profile `{name}` not found");
            return;
        };
        crate::session::write_session_profile(&self.active_state_path, name);

        // The daemon owns the live session, so local config alone is not enough.
        if let Some(info) = self.sidecar.clone() {
            let session = crate::serve::sidecar::state_path_to_session_id(&self.active_state_path);
            let profile = name.to_string();
            self.model_switch_handle = Some(tokio::spawn(async move {
                crate::serve::sidecar::set_session_model(&info, &session, &profile).await
            }));
            self.screen = Screen::Main;
            self.status = format!(
                "switching this chat to {} · {}…",
                model.provider, model.model
            );
            return;
        }

        let resumed = self.restart_loop_for_config();
        self.screen = Screen::Main;
        self.refresh_effective_model();
        self.status = format!(
            "✓ this chat · {} · {}{}",
            model.provider,
            model.model,
            if resumed { " · resumed" } else { "" },
        );
    }

    /// Close the login form, optionally restoring the pre-login config (Esc).

    pub(crate) fn conversations_dir(&self) -> PathBuf {
        let parent = self
            .options
            .config
            .state_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let dir = parent.join("conversations");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// The state path a picker entry names.
    ///
    /// `default` is the workspace-root state; anything else is a saved
    /// conversation under `conversations/`. THE one mapping — `switch_conversation`
    /// and the delete/rename paths all need it, and a drift between them would
    /// mutate a different session than the one the user selected.
    pub(crate) fn state_path_for_conversation(&self, name: &str) -> PathBuf {
        if name == "default" {
            self.options.config.state_path.clone()
        } else {
            self.conversations_dir().join(format!("{name}.json"))
        }
    }

    /// Whether the active conversation already exists. Sessions live in the
    /// store, most with no state file, so the file is only one of the signals:
    /// the daemon's catalog and the store itself are the inventory.
    pub(crate) fn session_known(&self) -> bool {
        if self.active_state_path.exists() {
            return true;
        }
        let id = crate::session::session_id_for_state_path(&self.active_state_path);
        if self
            .daemon_sessions
            .as_ref()
            .is_some_and(|rows| rows.iter().any(|s| s.id == id))
        {
            return true;
        }
        crate::session::list_device_sessions()
            .iter()
            .any(|s| s.id == id)
    }

    /// The conversation this workspace was last used in.
    ///
    /// Read from the store, which is the only inventory: a filesystem walk would
    /// miss every session that has no state file, which is all of them.
    pub(crate) fn find_last_active_conversation(&self) -> Option<String> {
        let default_id = crate::session::session_id_for_state_path(
            &self.options.config.state_path,
        );
        let folder = self.options.config.workspace.clone();
        crate::session::list_device_sessions()
            .into_iter()
            .filter(|s| s.folder == folder)
            .max_by_key(|s| s.last_active)
            .map(|s| {
                if s.id == default_id {
                    "default".to_string()
                } else {
                    s.conversation
                }
            })
    }

    /// Delete a saved conversation (resume picker `d`).
    ///
    /// Queues the mutation for the next tick when a daemon is available: the
    /// daemon owns every session, so routing through it keeps the store row and
    /// its messages/events consistent — deleting files alone leaves a migrated
    /// conversation readable. Falls back to the local delete with no daemon.
    pub(crate) fn delete_conversation(&mut self, c: &Conversation) {
        if self.sidecar.is_some() {
            // Drop it from the catalog now. The op is deferred to the next tick,
            // so without this the entry the user just deleted keeps rendering
            // until the daemon round-trip lands.
            self.forget_session(&c.id);
            self.pending_session_op = Some(PendingSessionOp::Delete { session_id: c.id.clone() });
            return;
        }
        if let Some(path) = crate::session::state_path_for_id(&c.id) {
            crate::session::remove_session_files(&path);
        }
    }

    /// Set a saved conversation's title override (resume picker `r`).
    pub(crate) fn rename_conversation(&mut self, c: &Conversation, title: &str) {
        if self.sidecar.is_some() {
            if let Some(rows) = self.daemon_sessions.as_mut()
                && let Some(row) = rows.iter_mut().find(|s| s.id == c.id)
            {
                row.title = title.to_string();
            }
            self.pending_session_op = Some(PendingSessionOp::Rename {
                session_id: c.id.clone(),
                title: title.to_string(),
            });
            return;
        }
        if let Some(path) = crate::session::state_path_for_id(&c.id) {
            let _ = crate::session::set_session_title(&path, title);
        }
    }

    /// Drop one session from the cached catalog, so a queued mutation renders
    /// immediately instead of after the next daemon refresh.
    pub(crate) fn forget_session(&mut self, session_id: &str) {
        if let Some(rows) = self.daemon_sessions.as_mut() {
            rows.retain(|s| s.id != session_id);
        }
    }

    /// unrelated refresh.
    pub(crate) async fn apply_pending_session_op(&mut self) {
        let Some(op) = self.pending_session_op.take() else {
            return;
        };
        let Some(info) = self.sidecar.clone() else {
            return;
        };
        let result = match &op {
            PendingSessionOp::Delete { session_id } => {
                crate::serve::sidecar::delete_session(&info, session_id).await
            }
            PendingSessionOp::Rename { session_id, title } => {
                crate::serve::sidecar::rename_session(&info, session_id, title).await
            }
        };
        match result {
            Ok(()) => {
                self.refresh_daemon_sessions(true).await;
                // Drop the picker snapshot so it rebuilds from the fresh catalog.
                self.conv_cache = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// The folder this TUI's sessions belong to: the main checkout when the
    /// current workspace is a git worktree made from it, else the workspace.
    pub(crate) fn workspace_origin(&self) -> Option<crate::session::WorktreeOrigin> {
        let workspace = &self.options.config.workspace;
        let mut cache = self.origin_cache.borrow_mut();
        let fresh = cache.as_ref().is_some_and(|(path, at, _)| {
            path == workspace && at.elapsed() < Duration::from_secs(5)
        });
        if !fresh {
            let origin = crate::session::worktree_origin(workspace);
            *cache = Some((workspace.clone(), std::time::Instant::now(), origin));
        }
        cache.as_ref().and_then(|(_, _, origin)| origin.clone())
    }

    pub(crate) fn home_folder(&self) -> PathBuf {
        let workspace = &self.options.config.workspace;
        self.workspace_origin()
            .map(|origin| origin.folder)
            .unwrap_or_else(|| workspace.clone())
    }

    /// Sessions in the home folder and in every git worktree made from it,
    /// newest first. Read from the daemon's catalog when there is one, else
    /// straight from the store.
    pub(crate) fn list_conversations(&self) -> Vec<Conversation> {
        let home = self.home_folder();
        let rows: Vec<crate::serve::sidecar::SessionRow> = match self.daemon_sessions.as_ref() {
            Some(rows) => rows.clone(),
            None => crate::session::list_device_sessions()
                .into_iter()
                .map(|s| crate::serve::sidecar::SessionRow {
                    id: s.id,
                    folder: s.folder,
                    origin_folder: s.origin_folder,
                    branch: s.branch,
                    conversation: s.conversation,
                    title: s.title,
                    status: s.status,
                    last_active: s.last_active,
                })
                .collect(),
        };
        let home_str = home.display().to_string();
        let mut list: Vec<_> = rows
            .into_iter()
            .filter(|s| s.folder == home_str || s.origin_folder.as_deref() == Some(home_str.as_str()))
            // A workspace's root `default` state exists before anyone uses it;
            // only a titled one is a session worth resuming.
            .filter(|s| {
                !s.conversation.is_empty()
                    && (s.conversation != "default" || !s.title.trim().is_empty())
            })
            .collect();
        list.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        list.into_iter()
            .map(|s| {
                let title = if s.title.trim().is_empty() {
                    "empty session".to_string()
                } else {
                    s.title.trim().to_string()
                };
                Conversation {
                    desc: format!("({}) — {}", relative_age(s.last_active), shorten(title.clone())),
                    title,
                    status: s.status,
                    last_active: s.last_active,
                    branch: s.origin_folder.as_ref().and(s.branch),
                    folder: PathBuf::from(s.folder),
                    name: s.conversation,
                    id: s.id,
                }
            })
            .collect()
    }

    /// Whether `c` is the session on screen.
    pub(crate) fn is_active(&self, c: &Conversation) -> bool {
        c.name == self.active_conversation && c.folder == self.options.config.workspace
    }

    /// Resume a session from the picker or sidebar, moving to its worktree
    /// first when it works in one.
    pub(crate) fn open_conversation(&mut self, c: &Conversation) {
        if c.folder != self.options.config.workspace {
            self.options.config = self.options.config.for_workspace(c.folder.clone());
        }
        self.resume_conversation(&c.name);
    }

    /// Start a new session. In a git repository the new-session screen asks
    /// where it works first; elsewhere it works in the folder.
    pub(crate) fn begin_new_session(&mut self, name: String) {
        let home = self.home_folder();
        let Some((_, worktrees)) = crate::session::repo_worktrees(&home) else {
            self.start_new_session(NewChoice::Folder(home), name);
            return;
        };
        let mut choices = vec![NewChoice::Worktree(home.clone()), NewChoice::Folder(home)];
        choices.extend(
            worktrees
                .into_iter()
                .map(|w| NewChoice::Existing { path: w.path, branch: w.branch }),
        );
        self.new_choices = choices;
        self.new_choice_index = 0;
        self.new_session_name = name;
        self.screen = Screen::NewSession;
        self.status = "↑↓ choose · Enter start · Esc cancel".to_string();
    }

    /// Start the new session where `choice` says. It is created on the first
    /// message, with this choice.
    pub(crate) fn start_new_session(&mut self, choice: NewChoice, name: String) {
        let (folder, mode) = match choice {
            NewChoice::Worktree(folder) => (folder, crate::session::WorkspaceMode::Worktree),
            NewChoice::Folder(folder) => (folder, crate::session::WorkspaceMode::Folder),
            NewChoice::Existing { path, .. } => (path, crate::session::WorkspaceMode::Folder),
        };
        if folder != self.options.config.workspace {
            self.options.config = self.options.config.for_workspace(folder);
        }
        self.switch_conversation(&name);
        self.new_workspace = Some(mode);
        self.screen = Screen::Main;
        self.status = match mode {
            crate::session::WorkspaceMode::Worktree => {
                "New session in a new worktree — it is created with your first message.".to_string()
            }
            crate::session::WorkspaceMode::Folder => String::new(),
        };
    }

    /// Poll the daemon catalog without putting an HTTP round trip on the render
    /// loop. A stalled local daemon must not make terminal input wait on a request.
    pub(crate) async fn refresh_daemon_sessions(&mut self, force: bool) {
        if self
            .daemon_sessions_refresh
            .as_ref()
            .is_some_and(|refresh| refresh.is_finished())
        {
            let refresh = self
                .daemon_sessions_refresh
                .take()
                .expect("checked is_some");
            self.daemon_sessions_refreshed_at = Some(std::time::Instant::now());
            if let Ok(Ok(rows)) = refresh.await {
                self.daemon_sessions = Some(rows);
            }
        }

        let Some(info) = self.sidecar.clone() else {
            if let Some(refresh) = self.daemon_sessions_refresh.take() {
                refresh.abort();
            }
            self.daemon_sessions = None;
            self.daemon_sessions_refreshed_at = None;
            return;
        };

        if force {
            if let Some(refresh) = self.daemon_sessions_refresh.take() {
                refresh.abort();
            }
        } else if self.daemon_sessions_refresh.is_some()
            || self
                .daemon_sessions_refreshed_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }

        let folder = self.home_folder();
        self.daemon_sessions_refresh = Some(tokio::spawn(async move {
            crate::serve::sidecar::list_sessions(&info, Some(folder.as_path())).await
        }));
    }

    pub(crate) fn open_checkpoint_picker(&mut self, action: CheckpointAction) {
        let count = self
            .state
            .as_ref()
            .map(|s| s.checkpoints.len())
            .unwrap_or(0);
        if count == 0 {
            self.status = "No checkpoints yet — one is taken before each request.".to_string();
            return;
        }
        if action == CheckpointAction::Rewind && self.agent_busy() {
            self.status = "Agent is working — stop it (Esc) before rewinding.".to_string();
            return;
        }
        self.checkpoint_selected_index = count.saturating_sub(1);
        self.screen = match action {
            CheckpointAction::Rewind => Screen::RewindCheckpointSelection,
            CheckpointAction::Fork => Screen::ForkCheckpointSelection,
        };
        self.status = String::new();
    }

    pub(crate) fn confirm_checkpoint_selection(&mut self) {
        let Some(checkpoint) = self
            .state
            .as_ref()
            .and_then(|s| s.checkpoints.get(self.checkpoint_selected_index))
            .cloned()
        else {
            self.screen = Screen::Main;
            return;
        };
        let id = checkpoint.id;
        if self.screen == Screen::RewindCheckpointSelection {
            self.rewind_to(&id);
        } else {
            self.fork_at(&id);
        }
        self.screen = Screen::Main;
    }

    /// Restore workspace + conversation to the checkpoint whose id starts with `id_prefix`.
    /// Prefers the daemon `/session/rewind` path so history is truncated on disk and
    /// the live loop is notified; local-only mutation is a last resort.
    pub(crate) fn rewind_to(&mut self, id_prefix: &str) {
        if self.agent_busy() {
            self.status = "Agent is working — stop it (Esc) before rewinding.".to_string();
            return;
        }
        let Some(state) = self.state.as_ref() else {
            self.status = "No active session.".to_string();
            return;
        };
        let Some(checkpoint) = state
            .checkpoints
            .iter()
            .rev()
            .find(|c| c.id.starts_with(id_prefix) || c.id == id_prefix)
        else {
            self.status = format!("No checkpoint matching '{id_prefix}'.");
            return;
        };
        let id = checkpoint.id.clone();
        let label = checkpoint.label.clone();

        // Sidecar mode: durable rewind via daemon (FS + state file + live loop).
        if let Some(info) = self.sidecar.clone() {
            let session = crate::serve::sidecar::state_path_to_session_id(&self.active_state_path);
            let info2 = info.clone();
            let id2 = id.clone();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    crate::serve::sidecar::rewind_session(&info2, &session, &id2).await
                })
            });
            match result {
                Ok(()) => {
                    // Optimistic local truncate until the next snapshot/delta arrives.
                    if let Some(st) = self.state.as_mut() {
                        let _ = st.apply_checkpoint_rewind(&id);
                    }
                    self.sent_turn_pending = false;
                    self.status = format!("Rewound to: {label}");
                }
                Err(error) => self.error = Some(format!("rewind failed: {error}")),
            }
            return;
        }

        // No daemon — local FS restore + in-memory truncate (legacy / tests).
        let workspace = self.options.config.workspace.clone();
        match crate::checkpoint::restore(&workspace, &id) {
            Ok(()) => {
                if let Some(st) = self.state.as_mut() {
                    let _ = st.apply_checkpoint_rewind(&id);
                }
                self.status = format!("Rewound workspace and history to: {label}");
            }
            Err(error) => self.error = Some(format!("rewind failed: {error}")),
        }
    }

    /// Branch a NEW conversation from a checkpoint id/prefix or an event index.
    /// Source session is left intact. Opens the fork immediately.
    pub(crate) fn fork_at(&mut self, arg: &str) {
        let Some(state) = self.state.clone() else {
            self.status = "No active session.".to_string();
            return;
        };
        if state.events.is_empty() {
            self.status = "Nothing to fork — session has no events yet.".to_string();
            return;
        }

        let arg = arg.trim();
        let point = if arg.is_empty() {
            // Bare /fork → branch from the latest checkpoint, else full history.
            if let Some(cp) = state.checkpoints.last() {
                crate::session::ForkPoint {
                    event_end: cp.event_index.min(state.events.len()),
                    message_end: cp.message_index.min(state.messages.len()),
                }
            } else {
                crate::session::ForkPoint {
                    event_end: state.events.len(),
                    message_end: state.messages.len(),
                }
            }
        } else if let Ok(idx) = arg.parse::<usize>() {
            match crate::session::resolve_fork_point(&state, None, Some(idx)) {
                Ok(p) => p,
                Err(e) => {
                    self.status = e;
                    return;
                }
            }
        } else {
            match crate::session::resolve_fork_point(&state, Some(arg), None) {
                Ok(p) => p,
                Err(e) => {
                    self.status = e;
                    return;
                }
            }
        };

        match crate::session::write_forked_conversation(&self.active_state_path, &state, point) {
            Ok(forked) => {
                // Session id is like `workspaces/<ws>/conversations/<uuid>.json` —
                // TUI conversations are addressed by the uuid stem.
                let name = forked
                    .state_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    self.status = format!("Forked → {}", forked.id);
                    return;
                }
                self.switch_conversation(&name);
                self.status = format!("Forked → {name}  ({})", forked.title);
            }
            Err(e) => self.error = Some(format!("fork failed: {e}")),
        }
    }

    pub(crate) fn switch_conversation(&mut self, name: &str) {
        // Tear down any resident agent SYNCHRONOUSLY. The interactive loop
        // idle-blocks on input and never exits on its own, so a mere async
        // Interrupt would still leave the task "alive" when the next spawn_loop
        // checks `agent_alive()` — making the resume spawn a silent no-op, after
        // which the user's first message falls through to a fresh (resume=false)
        // session that overwrites the one they picked. Abort + clear so the next
        // spawn starts clean.
        if let Some(tx) = self.input_tx.take() {
            let _ = tx.send(LoopInput::Interrupt);
        }
        if let Some(handle) = self.agent.take() {
            handle.abort();
        }
        // Drop the WS attachment so the next spawn_loop opens a fresh /attach
        // for the new conversation (daemon keeps owning the old session).
        self.sidecar_attach = None;
        self.pending_sidecar_attach = None;

        // Update active_conversation and active_state_path
        self.active_conversation = name.to_string();
        self.active_state_path = self.state_path_for_conversation(name);

        // Force a fresh state read for the new session's file.
        self.last_state_stamp = None;
        self.state = None;
        self.scroll = 0;
        self.status = String::new();
        // Drop anything held for the PREVIOUS conversation: queued messages must
        // never fire into the newly-switched session, stale busy flags must not
        // trigger a phantom flush, and a lingering error must not mask status.
        self.pending_steers.clear();
        self.was_busy = false;
        self.sent_turn_pending = false;
        self.error = None;
        // Re-seed the compaction counter so the new conversation's EXISTING
        // compaction history doesn't fire the "new compaction" animation on its
        // first tick (same as startup). Also drop any leftover animation hold.
        self.seen_compactions = usize::MAX;
        self.compaction_anim_until = None;
        // The new chat may carry its own model override — re-resolve the header label.
        self.refresh_effective_model();
        // In sidecar mode, queue an attach for the newly selected conversation so
        // the next tick binds WS /attach without waiting for the first keystroke.
        if self.sidecar.is_some() {
            self.pending_sidecar_attach = Some(PendingSidecarAttach {
                initial: None,
                resume: true,
            });
        }
    }

    pub(crate) fn handle_slash_command(&mut self, text: &str) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.is_empty() {
            return;
        }

        let cmd = parts[0];
        match cmd {
            "/new" => {
                let name = if parts.len() > 1 {
                    let n = parts[1].to_string();
                    // A name collision would silently OPEN the existing session
                    // instead of creating a new one — surface it instead.
                    if self.list_conversations().iter().any(|c| c.name == n) {
                        self.status = format!(
                            "`{n}` already exists — /resume {n} to open it, or pick another name"
                        );
                        return;
                    }
                    n
                } else {
                    uuid::Uuid::new_v4().to_string()
                };
                self.status = String::new();
                self.begin_new_session(name);
            }
            "/resume" => {
                let target_name = if parts.len() > 1 {
                    Some(parts[1].to_string())
                } else {
                    None
                };

                match target_name {
                    // Switching to a named session tears down any resident agent
                    // (in switch_conversation), so it works even mid-run. Validate
                    // FIRST: a typo must not abandon the current session into a
                    // phantom empty one named after the typo.
                    Some(name) => {
                        // The same name can exist in the folder and a worktree;
                        // the one in the current workspace wins.
                        let workspace = self.options.config.workspace.clone();
                        let mut matches: Vec<_> = self
                            .list_conversations()
                            .into_iter()
                            .filter(|c| c.name == name)
                            .collect();
                        matches.sort_by_key(|c| c.folder != workspace);
                        let Some(target) = matches.into_iter().next() else {
                            self.status = format!(
                                "no session named `{name}` — bare /resume opens the picker"
                            );
                            return;
                        };
                        self.open_conversation(&target);
                        return;
                    }
                    None => {
                        // Bare /resume opens the picker (arrow keys, Enter to
                        // resume, `r` rename, `dd` delete) when there's anything
                        // to pick from.
                        let convs = self.list_conversations();
                        if !convs.is_empty() {
                            // Snapshot once for the picker — per-keystroke rescans
                            // of every session file made it laggy (see conv_cache).
                            self.conv_cache = Some(convs);
                            self.screen = Screen::ResumeSelection;
                            self.resume_selected_index = 0;
                            self.resume_pending_delete = false;
                            self.resume_rename = None;
                            self.status =
                                "↑↓ select · Enter resume · r rename · dd delete · Esc cancel"
                                    .to_string();
                            return;
                        }
                        // Nothing saved: resume the current session if one exists.
                        if self.agent_alive() {
                            self.status = "Agent is already running.".to_string();
                            return;
                        }
                        if !self.session_known() {
                            if let Some(last_active) = self.find_last_active_conversation() {
                                self.switch_conversation(&last_active);
                            }
                        }
                    }
                }

                if self.session_known() {
                    self.spawn_loop(None, true);
                } else {
                    self.status =
                        "No saved session to resume. Start a new one with /new or type a task."
                            .to_string();
                }
            }
            "/model" => {
                if self.agent_busy() {
                    self.status =
                        "Agent is working. Stop it (Esc) before changing the model.".to_string();
                    return;
                }
                self.open_profiles();
                self.status = String::new();
            }
            "/rewind" => {
                if parts.len() > 1 {
                    self.rewind_to(parts[1]);
                } else {
                    self.open_checkpoint_picker(CheckpointAction::Rewind);
                }
            }
            "/fork" => {
                let arg = text.strip_prefix("/fork").unwrap_or("").trim();
                if arg.is_empty() {
                    self.open_checkpoint_picker(CheckpointAction::Fork);
                } else {
                    self.fork_at(arg);
                }
            }
            "/mode" => {
                let manual = !self.options.config.manual_approval;
                self.options.config.manual_approval = manual;
                let _ = self.save_config_file();
                let mode = if manual {
                    crate::harness::ApprovalMode::Manual
                } else {
                    crate::harness::ApprovalMode::Auto
                };
                let _ = self.send_loop_input(LoopInput::SetMode(mode));
                self.status = String::new();
            }
            "/compact" => {
                if self.agent_alive() {
                    if self.send_loop_input(LoopInput::Compact).is_ok() {
                        if let Ok(mut stream) = self.stream.lock() {
                            stream.text = "Compacting history…".to_string();
                            stream.thinking.clear();
                        }
                        self.status = String::new();
                    } else {
                        self.status =
                            "Failed to send compact request to the agent loop.".to_string();
                    }
                } else {
                    self.status = "No active session to compact.".to_string();
                }
            }
            "/goal" => {
                let rest = text.strip_prefix("/goal").unwrap_or("").trim();
                if rest.eq_ignore_ascii_case("cancel") || rest.eq_ignore_ascii_case("stop") {
                    if self.agent_alive() {
                        let _ = self.send_loop_input(LoopInput::CancelGoal);
                        self.status = "Cancelling the goal…".to_string();
                    } else {
                        self.status = "No active goal.".to_string();
                    }
                } else if rest.eq_ignore_ascii_case("resume")
                    || rest.eq_ignore_ascii_case("continue")
                {
                    if self.agent_alive() {
                        let _ = self.send_loop_input(LoopInput::ResumeGoal);
                        self.status = "Resuming the paused goal…".to_string();
                    } else {
                        self.status = "No active session to resume the goal.".to_string();
                    }
                } else if rest.is_empty() {
                    self.status =
                        "Usage: /goal <what to accomplish>   ·   /goal resume   ·   /goal cancel"
                            .to_string();
                } else {
                    // The agent must be running to receive the goal; start it if idle.
                    if !self.agent_alive() {
                        self.spawn_loop(None, true);
                    }
                    if self
                        .send_loop_input(LoopInput::SetGoal(rest.to_string()))
                        .is_ok()
                    {
                        self.status =
                            "Goal set — the agent will drive toward it. /goal cancel to stop."
                                .to_string();
                    } else {
                        // Sidecar attach may still be pending — queue the goal for after attach.
                        self.status = "Starting session for goal…".to_string();
                        if self.pending_sidecar_attach.is_some() {
                            // Will be delivered after attach if we stash it as initial.
                            // For now just mark pending and let user retry if needed.
                        } else {
                            self.status = "Couldn't start the agent loop for the goal.".to_string();
                        }
                    }
                }
            }
            "/term" => {
                if parts.get(1).is_some_and(|p| *p == "new") {
                    self.new_term();
                } else {
                    self.open_term();
                }
            }
            "/mission" => {
                self.open_mission_control();
            }
            "/theme" => {
                self.status = "AMOLED is the only theme.".to_string();
            }
            "/recur" => self.handle_recur_command(text),
            other => {
                self.status = format!(
                    "Unknown command: {other}. Type /new, /resume, /rewind, /fork, /model, /term, or /recur."
                );
            }
        }
    }

    pub(crate) fn current_session_id(&self) -> String {
        if self.active_state_path == crate::mission_control::session_state_path()
            || crate::mission_control::is_session_id(
                &crate::serve::sidecar::state_path_to_session_id(&self.active_state_path),
            )
        {
            return crate::mission_control::SESSION_ID.to_string();
        }
        crate::serve::sidecar::state_path_to_session_id(&self.active_state_path)
    }

    pub(crate) fn handle_recur_command(&mut self, text: &str) {
        let rest = text.strip_prefix("/recur").unwrap_or("").trim();
        let root = crate::recurring::default_root();
        if rest.is_empty() || rest.eq_ignore_ascii_case("list") {
            match crate::recurring::list_jobs(&root) {
                Ok(jobs) if jobs.is_empty() => {
                    self.status =
                        "No recurring jobs. /recur add every 5m <prompt>  ·  /recur add <session> every 1h @plan.md"
                            .into();
                }
                Ok(jobs) => {
                    let lines: Vec<String> = jobs
                        .iter()
                        .map(|j| {
                            let flag = if !j.enabled {
                                "paused"
                            } else if j.queued {
                                "queued"
                            } else {
                                "on"
                            };
                            let short = if j.id.len() >= 8 { &j.id[..8] } else { &j.id };
                            format!(
                                "{short}  {flag}  {}  → {}  {}",
                                j.schedule.display(),
                                j.session_id,
                                j.title
                            )
                        })
                        .collect();
                    self.status = format!("{} job(s):\n{}", jobs.len(), lines.join("\n"));
                }
                Err(e) => self.status = format!("recur list failed: {e}"),
            }
            return;
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let verb = parts.next().unwrap_or("").to_ascii_lowercase();
        let tail = parts.next().unwrap_or("").trim();
        match verb.as_str() {
            "add" => {
                // /recur add every 5m <prompt>            → this chat
                // /recur add daily 09:00 @notes/plan.md
                // /recur add <session-id> every 5m …      → that chat (from MC or any)
                let mut tokens = tail.split_whitespace();
                let first = tokens.next().unwrap_or("");
                let looks_like_schedule = first.eq_ignore_ascii_case("every")
                    || first.eq_ignore_ascii_case("daily")
                    || first.eq_ignore_ascii_case("at")
                    || first.eq_ignore_ascii_case("in");
                let (session_id, schedule_raw, rest) = if looks_like_schedule {
                    let spec = tokens.next().unwrap_or("");
                    let rest: String = tokens.collect::<Vec<_>>().join(" ");
                    (self.current_session_id(), format!("{first} {spec}"), rest)
                } else if first.eq_ignore_ascii_case("mc")
                    || first.eq_ignore_ascii_case("mission-control")
                {
                    let kind = tokens.next().unwrap_or("");
                    let spec = tokens.next().unwrap_or("");
                    let rest: String = tokens.collect::<Vec<_>>().join(" ");
                    (
                        crate::mission_control::SESSION_ID.to_string(),
                        format!("{kind} {spec}"),
                        rest,
                    )
                } else {
                    // first token is a target session id
                    let kind = tokens.next().unwrap_or("");
                    let spec = tokens.next().unwrap_or("");
                    let rest: String = tokens.collect::<Vec<_>>().join(" ");
                    (first.to_string(), format!("{kind} {spec}"), rest)
                };
                let (prompt, plan_path) = split_recur_prompt_and_plan(&rest);
                if prompt.is_empty() && plan_path.is_none() {
                    self.status =
                        "Usage: /recur add every 5m <prompt>   ·   /recur add <session> every 5m @plan.md   ·   /recur add daily 09:00 @plan.md"
                            .into();
                    return;
                }
                match crate::recurring::Schedule::parse(&schedule_raw) {
                    Ok(schedule) => {
                        let title_src = if !prompt.is_empty() {
                            prompt.as_str()
                        } else {
                            plan_path.as_deref().unwrap_or("scheduled")
                        };
                        let title: String = title_src.chars().take(48).collect();
                        match crate::recurring::create_job(
                            &root,
                            &title,
                            &session_id,
                            &prompt,
                            schedule,
                            plan_path.as_deref(),
                        ) {
                            Ok(job) => {
                                self.status = format!(
                                    "Recurring {} → {} ({})",
                                    job.schedule.display(),
                                    job.session_id,
                                    &job.id[..8.min(job.id.len())]
                                );
                            }
                            Err(e) => self.status = format!("recur add failed: {e}"),
                        }
                    }
                    Err(e) => self.status = format!("{e}"),
                }
            }
            "pause" | "off" => match resolve_recur_id(&root, tail) {
                Ok(id) => match crate::recurring::set_enabled(&root, &id, false) {
                    Ok(job) => {
                        self.status = format!("Paused {}", short_id(&job.id));
                    }
                    Err(e) => self.status = format!("recur pause failed: {e}"),
                },
                Err(e) => self.status = e,
            },
            "on" | "resume" | "unpause" => match resolve_recur_id(&root, tail) {
                Ok(id) => match crate::recurring::set_enabled(&root, &id, true) {
                    Ok(job) => {
                        self.status = format!("Enabled {}", short_id(&job.id));
                    }
                    Err(e) => self.status = format!("recur on failed: {e}"),
                },
                Err(e) => self.status = e,
            },
            "rm" | "remove" | "delete" => match resolve_recur_id(&root, tail) {
                Ok(id) => match crate::recurring::delete_job(&root, &id) {
                    Ok(()) => self.status = format!("Removed {}", short_id(&id)),
                    Err(e) => self.status = format!("recur rm failed: {e}"),
                },
                Err(e) => self.status = e,
            },
            _ => {
                self.status =
                    "Usage: /recur list · add every 5m|daily 09:00 <prompt> · add <session> every 5m … @plan.md · pause|on|rm <id>"
                        .into();
            }
        }
    }


}

pub(crate) fn short_id(id: &str) -> &str {
    if id.len() >= 8 { &id[..8] } else { id }
}

pub(crate) fn resolve_recur_id(root: &std::path::Path, prefix: &str) -> Result<String, String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Err("Usage: /recur pause|on|rm <id>".into());
    }
    let jobs = crate::recurring::list_jobs(root).map_err(|e| format!("recur: {e}"))?;
    let matches: Vec<_> = jobs
        .into_iter()
        .filter(|j| j.id == prefix || j.id.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(format!("no recurring job matching `{prefix}`")),
        many => Err(format!(
            "ambiguous id `{prefix}` ({} matches) — use more of the id",
            many.len()
        )),
    }
}

/// Split trailing `@path` (or `file:path`) off a `/recur add` rest string.
pub(crate) fn split_recur_prompt_and_plan(rest: &str) -> (String, Option<String>) {
    let rest = rest.trim();
    if rest.is_empty() {
        return (String::new(), None);
    }
    if let Some(path) = rest
        .strip_prefix('@')
        .or_else(|| rest.strip_prefix("file:"))
    {
        let path = path.trim();
        if !path.is_empty() {
            return (String::new(), Some(path.to_string()));
        }
    }
    if let Some((prompt, path)) = rest.rsplit_once(" @") {
        let path = path.trim();
        if !path.is_empty() && !path.contains(char::is_whitespace) {
            return (prompt.trim().to_string(), Some(path.to_string()));
        }
    }
    (rest.to_string(), None)
}

/// A session in the picker and sidebar: where it works and its name there.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Conversation {
    pub(crate) id: String,
    pub(crate) folder: PathBuf,
    pub(crate) name: String,
    pub(crate) title: String,
    /// Picker line: "(2h) — title".
    pub(crate) desc: String,
    pub(crate) status: String,
    pub(crate) last_active: i64,
    /// Set for a session in a git worktree of the home folder.
    pub(crate) branch: Option<String>,
}

/// Where a new session can work.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NewChoice {
    /// A new worktree of this folder, on its own branch.
    Worktree(PathBuf),
    /// The folder itself.
    Folder(PathBuf),
    /// A worktree that already exists.
    Existing { path: PathBuf, branch: Option<String> },
}
