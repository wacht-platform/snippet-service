use super::*;

impl CodingHarness {
    /// Bring a session loaded from either store into the current run.
    ///
    /// Both stores converge here so a resumed session behaves identically
    /// regardless of where it came from: the workspace and context window are
    /// refreshed, the system prefix is re-seeded (so new workspace memory lands),
    /// and any half-written tool batch is repaired so strict providers don't 400
    /// on the history forever after.
    pub(super) async fn resume_loaded_state(
        &self,
        mut state: HarnessState,
        seeded_system: String,
        initial_request: Option<String>,
    ) -> Result<HarnessState, ToolError> {
        // Migrate old metadata in memory; the next persist omits the legacy
        // `user_request` field and keeps the title as identity.
        normalize_state_title(&mut state);
        // Reflect the current run's folder (backfills pre-field states).
        state.workspace = self.context.workspace_root().display().to_string();
        state.context_window = self.config.context_window_tokens;
        // Refresh the system prefix so resumed sessions pick up the latest
        // workspace memory (guarded: no-op if messages[0] isn't System).
        if let Some(HarnessMessage::System { content }) = state.messages.first_mut() {
            *content = seeded_system;
        }
        // A crash mid tool-batch persists an assistant `tool_calls` message whose
        // later calls never got results; strict providers (Anthropic, DeepSeek)
        // 400 on that history forever after. Repair on load so a resumed session
        // is always well-formed.
        let loaded_messages = state.messages.len();
        repair_unanswered_tool_calls(&mut state.messages);
        if state.messages.len() != loaded_messages {
            state.history_rewritten = true;
        }
        repair_unanswered_tool_events(&mut state.events, 0);
        if let Some(request) = initial_request
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
        {
            state.status = HarnessStatus::Running;
            state.final_text = None;
            state.pending_question = None;
            state.messages.push(HarnessMessage::User {
                content: request.clone(),
            });
            state.events.push(HarnessEvent::UserInput { text: request });
            self.bump_activity();
            self.persist_state(&mut state).await?;
        }
        Ok(state)
    }

    pub(super) async fn load_or_initialize_state(
        &self,
        initial_request: Option<String>,
    ) -> Result<HarnessState, ToolError> {
        // Build the memory block once and fold it into the system prefix, so it
        // rides in the cached prompt and refreshes every session (including
        // resume). Within a session it stays fixed; the files show the live
        // state.
        let seeded_system = if self.config.memory_enabled {
            format!(
                "{}\n\n{}",
                self.config.system_prompt,
                crate::memory::Memory::open(self.context.workspace_root()).render_prompt()
            )
        } else {
            self.config.system_prompt.clone()
        };

        if self.config.resume
            && let Some(state) = self.load_from_store().await?
        {
            return self
                .resume_loaded_state(state, seeded_system, initial_request)
                .await;
        }

        // Fresh session in this folder: keep snippet's `.snippet/` workspace scratch
        // (bg processes, lanes) out of the user's git history. Main agent only —
        // lanes share the workspace and would just race on the same file.
        if self.context.owner() == "main" {
            ensure_snippet_gitignored(self.context.workspace_root());
        }

        let now = Utc::now().to_rfc3339();
        let request = initial_request
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        let mut messages = vec![HarnessMessage::System {
            content: seeded_system,
        }];
        let mut events = Vec::new();
        let (status, title) = match request.as_ref() {
            Some(text) => {
                messages.push(HarnessMessage::User {
                    content: text.clone(),
                });
                events.push(HarnessEvent::UserInput { text: text.clone() });
                (HarnessStatus::Running, Some(text.clone()))
            }
            None => (HarnessStatus::Idle, None),
        };
        let mut state = HarnessState {
            version: 1,
            status,
            created_at: now.clone(),
            updated_at: now.clone(),
            workspace: self.context.workspace_root().display().to_string(),
            title,
            legacy_request: String::new(),
            goal: None,
            compacting: false,
            turn_started_at: if matches!(status, HarnessStatus::Running) {
                Some(now.clone())
            } else {
                None
            },
            compacting_started_at: None,
            watches: Vec::new(),
            messages,
            events,
            iterations: 0,
            final_text: None,
            lanes: Vec::new(),
            pending_question: None,
            approval_mode: if self.config.manual_approval {
                ApprovalMode::Manual
            } else {
                ApprovalMode::Auto
            },
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_prompt_tokens: 0,
            cache_read_tokens: 0,
            checkpoints: Vec::new(),
            rate_limit: None,
            context_window: self.config.context_window_tokens,
            tool_payloads_pruned: false,
            queued_inputs: Vec::new(),
            plan: Vec::new(),
            history_rewritten: false,
            events_rewritten: false,
            compactions: 0,
        };
        self.persist_state(&mut state).await?;
        if request.is_some() {
            self.bump_activity();
        }
        Ok(state)
    }

    pub(super) async fn persist(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
    ) -> Result<(), ToolError> {
        state.lanes = lanes.records().to_vec();
        self.persist_state(state).await
    }

    pub(super) async fn persist_state(&self, state: &mut HarnessState) -> Result<(), ToolError> {
        stamp_activity_times(state);
        if self.config.state_path.is_none() {
            return Ok(());
        }
        state.updated_at = Utc::now().to_rfc3339();
        self.persist_to_store(state).await
    }

    /// List sort uses the store's `last_active` — bump only when the user
    /// actually sent a message (or a mid-run steer).
    pub(super) fn bump_activity(&self) {
        if let Some(path) = &self.config.state_path {
            crate::session::bump_session_activity(path);
        }
    }

    /// The durable session id: the bound id when present, else the state path's
    /// id, which is the same identity the session list and tools use.
    pub(super) fn session_id(&self) -> Option<String> {
        if let Some(id) = self.context.durable_session_id() {
            return Some(id.to_string());
        }
        self.config
            .state_path
            .as_deref()
            .map(crate::session::session_id_for_state_path)
    }

    pub(super) fn store(&self) -> Option<crate::store::Store> {
        let path = self.context.store_path()?;
        crate::store::Store::open(path).ok()
    }

    /// Save the session to the database: scalar state plus the transcript tail.
    ///
    /// Appends only the messages and events that are not already durable, so the
    /// common persist writes a handful of rows instead of re-serializing and
    /// recompressing the whole conversation. `history_rewritten` is the signal
    /// that a writer replaced the middle (compaction, rewind, rollback), where an
    /// append would duplicate or misorder — those fall back to a full replace.
    pub(super) async fn persist_to_store(&self, state: &mut HarnessState) -> Result<(), ToolError> {
        let Some(store) = self.store() else {
            return Ok(());
        };
        let Some(id) = self.session_id() else {
            return Ok(());
        };
        let workspace = self.context.workspace_root().display().to_string();
        let key = crate::config::workspace_key(self.context.workspace_root());
        let title = state.title.clone();
        let status = crate::session::status_str(state.status);
        let scalar = scalar_json_in_place(state).map_err(ToolError::msg)?;
        let now = state.updated_at.clone();

        // Read the status BEFORE overwriting it: the transition is the entire
        // content of the event, and `save_session_scalar` below destroys it.
        let prev_status = store
            .get_session_row(&id)
            .ok()
            .flatten()
            .map(|row| row.status)
            .unwrap_or_default();

        let result = async {
            store
                .save_session_scalar(
                    &id,
                    &key,
                    &workspace,
                    title.as_deref(),
                    &status,
                    &scalar,
                    &state.created_at,
                    &now,
                )
                .map_err(|e| e.to_string())?;

            let messages_rewritten = state.history_rewritten
                || self
                    .written_messages
                    .load(std::sync::atomic::Ordering::Acquire)
                    > state.messages.len();
            let events_rewritten = state.events_rewritten
                || self
                    .written_events
                    .load(std::sync::atomic::Ordering::Acquire)
                    > state.events.len();
            if messages_rewritten {
                store
                    .replace_conversation_messages(&id, &state.messages, &now)
                    .map_err(|e| e.to_string())?;
            } else {
                let from = self
                    .written_messages
                    .load(std::sync::atomic::Ordering::Acquire);
                if from < state.messages.len() {
                    store
                        .append_conversation_messages(&id, &state.messages[from..], &now)
                        .map_err(|e| e.to_string())?;
                }
            }
            if events_rewritten {
                store
                    .replace_conversation_events(&id, &state.events, &now)
                    .map_err(|e| e.to_string())?;
            } else {
                let from = self
                    .written_events
                    .load(std::sync::atomic::Ordering::Acquire);
                if from < state.events.len() {
                    store
                        .append_conversation_events(&id, &state.events[from..], &now)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok::<(), String>(())
        }
        .await;

        match result {
            Ok(()) => {
                self.written_messages
                    .store(state.messages.len(), std::sync::atomic::Ordering::Release);
                self.written_events
                    .store(state.events.len(), std::sync::atomic::Ordering::Release);
                state.history_rewritten = false;
                state.events_rewritten = false;
                // Park any work a dead session was doing. Done AFTER the write,
                // so the parked state never contradicts what the store holds.
                crate::session::park_failed_session_work(&id, &prev_status, state);
                crate::session::emit_status_transition(
                    &id,
                    &prev_status,
                    &status,
                    title.as_deref(),
                    &workspace,
                );
                Ok(())
            }
            Err(error) => Err(ToolError::msg(format!("persist session: {error}"))),
        }
    }

    /// Load a session from the database, if this store has it.
    ///
    /// Returns `None` when there is no row, which is what keeps a session that
    /// predates the store on its state file instead of silently re-initializing.
    pub(super) async fn load_from_store(&self) -> Result<Option<HarnessState>, ToolError> {
        let Some(store) = self.store() else {
            return Ok(None);
        };
        let Some(id) = self.session_id() else {
            return Ok(None);
        };
        let Some(scalar) = store
            .load_session_scalar(&id)
            .map_err(|e| ToolError::msg(format!("load session: {e}")))?
        else {
            return Ok(None);
        };
        let messages = store
            .load_conversation_messages(&id)
            .map_err(|e| ToolError::msg(format!("load messages: {e}")))?;
        let events = store
            .load_conversation_events(&id)
            .map_err(|e| ToolError::msg(format!("load events: {e}")))?;
        self.written_messages
            .store(messages.len(), std::sync::atomic::Ordering::Release);
        self.written_events
            .store(events.len(), std::sync::atomic::Ordering::Release);
        let state = state_from_scalar(&scalar, messages, events).map_err(ToolError::msg)?;
        Ok(Some(state))
    }
}

/// Keep compacting/thinking clocks anchored to when the activity actually
/// started, not when a client widget mounted. Called on every persist so
/// attached UIs can tick from a durable RFC3339 stamp.
pub(super) fn stamp_activity_times(state: &mut HarnessState) {
    let now = Utc::now().to_rfc3339();
    if state.compacting {
        if state.compacting_started_at.is_none() {
            state.compacting_started_at = Some(now.clone());
        }
    } else {
        state.compacting_started_at = None;
    }
    if state.status == HarnessStatus::Running && !state.compacting {
        if state.turn_started_at.is_none() {
            state.turn_started_at = Some(now);
        }
    } else if state.status != HarnessStatus::Running {
        state.turn_started_at = None;
    }
}

/// On a fresh session in a git work tree, make sure snippet's `.snippet/` scratch
/// (bg-process registry, lane state) is gitignored so it never lands in the user's
/// history. Best-effort and idempotent: creates `.gitignore` if it's missing,
/// appends the entry if absent, no-ops if already covered. Skips folders that
/// aren't in a git repo — a `.gitignore` there would be pointless clutter.
pub(super) fn ensure_snippet_gitignored(workspace: &Path) {
    if !in_git_work_tree(workspace) {
        return;
    }
    let gitignore = workspace.join(".gitignore");
    let covered = |content: &str| {
        content.lines().any(|line| {
            matches!(
                line.trim(),
                ".snippet" | ".snippet/" | "/.snippet" | "/.snippet/"
            )
        })
    };
    match std::fs::read_to_string(&gitignore) {
        Ok(content) => {
            if covered(&content) {
                return;
            }
            let mut updated = content;
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(".snippet/\n");
            let _ = std::fs::write(&gitignore, updated);
        }
        // No `.gitignore` yet (or unreadable) — create one with just the entry.
        Err(_) => {
            let _ = std::fs::write(&gitignore, ".snippet/\n");
        }
    }
}

/// Whether `dir` sits inside a git work tree — walk up for a `.git` marker (a dir
/// for a normal clone, a file for a worktree/submodule). No subprocess.
pub(super) fn in_git_work_tree(dir: &Path) -> bool {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return true;
        }
        cur = d.parent();
    }
    false
}
