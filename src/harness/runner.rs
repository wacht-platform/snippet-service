use super::*;

impl CodingHarness {
    pub fn new(config: HarnessConfig, tools: ToolRegistry, context: ToolContext) -> Self {
        Self {
            config,
            tools,
            context,
            written_messages: std::sync::atomic::AtomicUsize::new(0),
            written_events: std::sync::atomic::AtomicUsize::new(0),
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
    pub(super) async fn persist_to_store(&self, state: &HarnessState) -> Result<(), ToolError> {
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
        let scalar = scalar_json(state).map_err(ToolError::msg)?;
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

            let rewritten = state.history_rewritten
                || self.written_messages.load(std::sync::atomic::Ordering::Acquire)
                    > state.messages.len()
                || self.written_events.load(std::sync::atomic::Ordering::Acquire) > state.events.len();
            if rewritten {
                store
                    .replace_conversation_messages(&id, &state.messages, &now)
                    .map_err(|e| e.to_string())?;
                store
                    .replace_conversation_events(&id, &state.events, &now)
                    .map_err(|e| e.to_string())?;
            } else {
                let from = self.written_messages.load(std::sync::atomic::Ordering::Acquire);
                if from < state.messages.len() {
                    store
                        .append_conversation_messages(&id, &state.messages[from..], &now)
                        .map_err(|e| e.to_string())?;
                }
                let from = self.written_events.load(std::sync::atomic::Ordering::Acquire);
                if from < state.events.len() {
                    store
                        .append_conversation_events(&id, &state.events[from..], &now)
                        .map_err(|e| e.to_string())?;
                    if state.events[from..]
                        .iter()
                        .any(|e| matches!(e, HarnessEvent::ToolResult { .. }))
                    {
                        crate::session::emit_device_event(serde_json::json!({
                            "kind": "activity",
                            "session": id,
                            "workspace": workspace,
                        }));
                    }
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

    /// One-shot run: drive the agent until it ends a turn (via `complete`), then
    /// return the outcome. Delegation is disabled (no model factory). Used by the
    /// library, by tests, and by each background lane.
    pub async fn run(
        &self,
        model: &mut dyn AgentModel,
        initial_request: impl Into<String>,
    ) -> Result<HarnessOutcome, ToolError> {
        let mut state = self
            .load_or_initialize_state(Some(initial_request.into()))
            .await?;
        self.compact_history_if_needed(model, &mut state).await?;
        if state.status == HarnessStatus::Completed {
            return Ok(HarnessOutcome {
                final_text: state.final_text,
                events: state.events,
                iterations: state.iterations,
            });
        }

        // Lanes are inert without a factory; this channel is never driven here.
        let (lane_tx, _lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<crate::lanes::LaneProgress>();
        let mut lanes = self.new_lane_manager(None, lane_tx, progress_tx, &state);
        // Watches are likewise inert on the one-shot path (nothing selects on the
        // channel) — present only so the meta-tool dispatch signature is uniform.
        let (watch_tx, _watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches = WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        let mut vars = LoopVars::default();
        let mut consecutive_errors = 0usize;

        let start = state.iterations + 1;
        // One-shot / lane runs are always Auto — force it so a resumed/forced Manual
        // state can't block forever on an approval channel that's never driven here.
        state.approval_mode = ApprovalMode::Auto;
        let (_approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalDecision>();
        for iteration in start..=self.config.runtime_backstop_iterations {
            state.iterations = iteration;
            self.persist(&mut state, &lanes).await?;

            match self
                .step(
                    model,
                    &mut state,
                    &mut lanes,
                    &mut watches,
                    &mut vars,
                    false,
                    None,
                    &mut approval_rx,
                )
                .await
            {
                StepResult::Continue => {
                    consecutive_errors = 0;
                }
                StepResult::TurnEnded { final_text, .. } => {
                    state.status = HarnessStatus::Completed;
                    state.final_text = final_text.clone();
                    self.persist(&mut state, &lanes).await?;
                    return Ok(HarnessOutcome {
                        final_text,
                        events: state.events,
                        iterations: iteration,
                    });
                }
                StepResult::ModelError { message, retryable } => {
                    state.events.push(HarnessEvent::ModelError {
                        message: message.clone(),
                    });
                    // Fatal errors (auth/permission/not-found/bad-request) never
                    // succeed on retry — give up at once instead of recovering.
                    let action = if retryable {
                        self.recover(&mut state, &mut consecutive_errors).await
                    } else {
                        RecoveryAction::GiveUp
                    };
                    match action {
                        RecoveryAction::Retry => {
                            self.persist(&mut state, &lanes).await?;
                            continue;
                        }
                        RecoveryAction::GiveUp => {
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            return Err(ToolError::msg(message));
                        }
                    }
                }
            }
        }

        state.status = HarnessStatus::Failed;
        self.persist(&mut state, &lanes).await?;
        Err(ToolError::msg(format!(
            "harness reached runtime backstop after {} iterations",
            self.config.runtime_backstop_iterations
        )))
    }

    /// Resident conversation run: a long-lived loop that processes turns, accepts
    /// mid-run steering, delegates background lanes, and folds lane reports back in.
    /// Returns the final state when the input channel closes or the user interrupts.
    pub async fn run_interactive(
        &self,
        model: &mut dyn AgentModel,
        initial_request: Option<String>,
        mut input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
    ) -> Result<HarnessState, ToolError> {
        let (lane_tx, mut lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let (progress_tx, mut progress_rx) =
            mpsc::unbounded_channel::<crate::lanes::LaneProgress>();
        let mut state = self.load_or_initialize_state(initial_request).await?;
        self.compact_history_if_needed(model, &mut state).await?;
        // A reopened terminal state (completed / failed / interrupted) starts idle
        // so the loop blocks for the next message instead of exiting at the top.
        // Interrupted is the important one: a session is left in that state whenever
        // the user switches away or hits Esc, and without this reset, resuming it
        // would break out of the loop immediately — the agent would die on load and
        // the user's next message would start a fresh session over it.
        if matches!(
            state.status,
            HarnessStatus::Completed | HarnessStatus::Failed | HarnessStatus::Interrupted
        ) {
            state.status = HarnessStatus::Idle;
        }
        let had_interrupted_lanes = state
            .lanes
            .iter()
            .any(|lane| lane.status == LaneStatus::Running);
        let mut lanes = self.new_lane_manager(factory, lane_tx, progress_tx, &state);
        // Child tasks do not survive a process restart, but each lane's harness
        // state does. Relaunch every lane that was persisted as running from its
        // last saved boundary and wake the parent so it can track the resumed work.
        if had_interrupted_lanes {
            lanes.resume_interrupted();
            state.status = HarnessStatus::Running;
        }
        // File watches (`monitor` meta-tool): re-arm any persisted from the state
        // so a daemon/TUI restart resumes tailing where it left off.
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches = WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        watches.restore(&state.watches);
        let mut vars = LoopVars::default();
        let mut consecutive_errors = 0usize;
        // Inputs that arrived while a step was running (the interrupt race consumes
        // input_rx, so non-interrupt messages are parked here until the next turn).
        let mut pending_inputs: Vec<LoopInput> = Vec::new();
        self.persist(&mut state, &lanes).await?;

        loop {
            // Apply any input buffered during a step. A message that arrived mid- or
            // post-turn wakes the loop so the next step addresses it. Moving this
            // before the idle-queue flush ensures mid-run queued messages land in
            // state.queued_inputs and auto-fire immediately when the turn ends.
            if !pending_inputs.is_empty() {
                let prior_status = state.status;
                let was_running = prior_status == HarnessStatus::Running;
                let mut had_user_msg = false;
                let mut wants_compact = false;
                // Mode/title/goal tweaks must hit disk so attach clients refresh —
                // without this, a lone SetMode while parked never persisted and
                // the desktop chip stayed stale until some later event.
                let mut needs_persist = false;
                for input in std::mem::take(&mut pending_inputs) {
                    match input {
                        // Buffered /compact must actually run — `apply_input`
                        // treated it as a no-op, silently swallowing the request.
                        LoopInput::Compact => wants_compact = true,
                        LoopInput::Queue(_) | LoopInput::Unqueue(_) | LoopInput::DropQueued => {
                            needs_persist = true;
                            self.apply_input(&mut state, input);
                        }
                        LoopInput::SteerQueued(id) => {
                            if let Some(text) = take_queued(&mut state, &id) {
                                had_user_msg = true;
                                if was_running {
                                    state.messages.push(HarnessMessage::User {
                                        content: format!("[steer]\n{text}"),
                                    });
                                    state.events.push(HarnessEvent::Steer { text });
                                    self.bump_activity();
                                } else {
                                    self.accept_user_message(&mut state, &mut vars, text).await;
                                    consecutive_errors = 0;
                                }
                            } else {
                                needs_persist = true;
                            }
                        }
                        LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            had_user_msg = true;
                            if was_running {
                                // Mid-run steer: the step continues; fold it in.
                                state.messages.push(HarnessMessage::User {
                                    content: format!("[steer]\n{text}"),
                                });
                                state.events.push(HarnessEvent::Steer { text });
                                self.bump_activity();
                            } else {
                                // The step ENDED while this was queued: it's the
                                // next real request (or the answer to the question
                                // that ended the turn), not a steer — take the full
                                // new-request path (checkpoint, budget/dedup reset,
                                // pending-question clearing).
                                self.accept_user_message(&mut state, &mut vars, text).await;
                                consecutive_errors = 0;
                            }
                        }
                        other => {
                            if matches!(
                                other,
                                LoopInput::SetMode(_)
                                    | LoopInput::SetTitle(_)
                                    | LoopInput::SetGoal(_)
                                    | LoopInput::ResumeGoal
                                    | LoopInput::CancelGoal
                                    | LoopInput::Notice(_)
                            ) {
                                needs_persist = true;
                            }
                            self.apply_input(&mut state, other);
                        }
                    }
                }
                if had_user_msg {
                    vars.empty_reply_reprompts = 0;
                }
                if wants_compact {
                    self.run_manual_compaction(model, &mut state, &lanes)
                        .await?;
                }
                if had_user_msg || was_running {
                    state.status = HarnessStatus::Running;
                    // Running path persists below; still flush meta now so a
                    // mid-run /mode flip reaches clients before the next step ends.
                    if needs_persist {
                        self.persist(&mut state, &lanes).await?;
                    }
                } else if wants_compact || needs_persist {
                    // Compaction ran while parked — return to the parked status
                    // rather than waking the model with nothing new.
                    state.status = prior_status;
                    self.persist(&mut state, &lanes).await?;
                }
            }

            // Held messages (typed while a run was in flight) fire as the next
            // turn once we're idle — never into waiting_for_input, where they'd
            // answer the agent's own question.
            if state.status == HarnessStatus::Idle && !state.queued_inputs.is_empty() {
                let held: Vec<QueuedInput> = std::mem::take(&mut state.queued_inputs);
                for (i, item) in held.into_iter().enumerate() {
                    let text = item.text;
                    if i == 0 {
                        self.accept_user_message(&mut state, &mut vars, text).await;
                        consecutive_errors = 0;
                    } else {
                        state.messages.push(HarnessMessage::User {
                            content: format!("[steer]\n{text}"),
                        });
                        state.events.push(HarnessEvent::Steer { text });
                        self.bump_activity();
                    }
                }
                self.persist(&mut state, &lanes).await?;
            }

            if state.status == HarnessStatus::Running {
                if !model.is_configured() {
                    state.events.push(HarnessEvent::ModelError {
                        message:
                            "No API key configured for this model. Add one in the model settings (app: Models · TUI: /model) before sending."
                                .to_string(),
                    });
                    state.status = HarnessStatus::Idle;
                    self.persist(&mut state, &lanes).await?;
                    continue;
                }
                let (interrupted, wants_compact) = self.drain_pending(
                    &mut state,
                    &mut lanes,
                    &mut watches,
                    &mut input_rx,
                    &mut lane_rx,
                    &mut progress_rx,
                    &mut watch_rx,
                );
                if interrupted {
                    state.status = HarnessStatus::Interrupted;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "interrupted".to_string(),
                        reasoning: "User interrupted the run.".to_string(),
                    });
                    self.persist(&mut state, &lanes).await?;
                    break;
                }
                if wants_compact {
                    self.run_manual_compaction(model, &mut state, &lanes)
                        .await?;
                }

                state.iterations += 1;
                self.persist(&mut state, &lanes).await?;

                // Compact before the next model call when the last prompt exceeded the
                // budget — not just once at startup.
                self.compact_history_if_needed(model, &mut state).await?;

                // Race the step against the input channel so an interrupt cancels
                // the in-flight model call immediately — otherwise the loop only
                // notices the interrupt at the next iteration, after waiting out the
                // whole HTTP request and its retry backoff. Non-interrupt messages
                // that land mid-step are buffered and applied at the next loop top.
                // Non-interrupt messages that land mid-step are buffered and applied at the
                // next loop top. The marks anchor where this step began so we can identify
                // events and tool calls produced by the in-flight turn on interrupt.
                let _msg_mark = state.messages.len();
                let evt_mark = state.events.len();
                // Bridge approvals from the input channel to the in-flight step: while
                // a mutating tool waits (manual mode), Approve/Deny arrive here and are
                // forwarded to the step over this channel; interrupt still cancels.
                let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<ApprovalDecision>();
                let outcome = {
                    let step_fut = self.step(
                        model,
                        &mut state,
                        &mut lanes,
                        &mut watches,
                        &mut vars,
                        true,
                        sink.as_ref(),
                        &mut approval_rx,
                    );
                    tokio::pin!(step_fut);
                    loop {
                        tokio::select! {
                            result = &mut step_fut => break Some(result),
                            msg = input_rx.recv() => match msg {
                                Some(LoopInput::Interrupt) | None => break None,
                                Some(LoopInput::Approve) => {
                                    let _ = approval_tx.send(ApprovalDecision::Approve);
                                }
                                Some(LoopInput::ApproveAll) => {
                                    let _ = approval_tx.send(ApprovalDecision::ApproveAll);
                                }
                                Some(LoopInput::Deny) => {
                                    let _ = approval_tx.send(ApprovalDecision::Deny);
                                }
                                // Queue/unqueue/drop land after this step — `state`
                                // is borrowed by the in-flight tool/model call.
                                Some(other) => pending_inputs.push(other),
                            }
                        }
                    }
                };

                let Some(result) = outcome else {
                    // Drain any remaining inputs buffered right before/with the interrupt
                    while let Ok(msg) = input_rx.try_recv() {
                        if !matches!(msg, LoopInput::Interrupt) {
                            pending_inputs.push(msg);
                        }
                    }
                    // Interrupted mid-step: clear the live stream sink, then close any
                    // unanswered tool calls with an interrupted result so assistant text
                    // and tool invocation records are preserved without breaking message pairing.
                    if let Some(sink) = sink.as_ref() {
                        StreamBuffer::clear(sink);
                    }
                    repair_unanswered_tool_events(&mut state.events, evt_mark);
                    repair_unanswered_tool_calls(&mut state.messages);
                    // Apply any pending inputs (such as queued messages sent immediately via SteerQueued,
                    // UserMessages, newly queued inputs, and notices) so they survive into the transcript.
                    self.apply_interrupted_pending(&mut state, &mut pending_inputs);
                    state.history_rewritten = true;
                    state.status = HarnessStatus::Interrupted;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "interrupted".to_string(),
                        reasoning: "User interrupted the run.".to_string(),
                    });
                    self.persist(&mut state, &lanes).await?;
                    break;
                };

                match result {
                    StepResult::Continue => {
                        consecutive_errors = 0;
                        // Step already persists after model text / each tool;
                        // flush again so end-of-step bookkeeping is on disk
                        // before the next model call.
                        self.persist(&mut state, &lanes).await?;
                    }
                    StepResult::TurnEnded { kind, final_text } => {
                        consecutive_errors = 0;
                        state.final_text = final_text;
                        state.status = match kind {
                            TurnEndKind::Ask => HarnessStatus::WaitingForInput,
                            TurnEndKind::Complete => HarnessStatus::Idle,
                        };
                        self.persist(&mut state, &lanes).await?;
                    }
                    StepResult::ModelError { message, retryable } => {
                        state.events.push(HarnessEvent::ModelError {
                            message: message.clone(),
                        });
                        // Goal mode: a rate limit PAUSES the goal rather than failing
                        // the session — the drive stops until the window resets. Record
                        // when that is (from the last rate-limit snapshot) so the UI can
                        // show it. The user can re-issue /goal to resume.
                        if is_rate_limit_error(&message) {
                            if let Some(goal) = state
                                .goal
                                .as_mut()
                                .filter(|g| g.status == GoalStatus::Active)
                            {
                                goal.status = GoalStatus::Paused;
                                goal.resume_at = state
                                    .rate_limit
                                    .as_ref()
                                    .and_then(earliest_reset)
                                    .unwrap_or(0);
                                let text = goal.text.clone();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "goal_paused".to_string(),
                                    reasoning: format!("rate limited — goal paused: {text}"),
                                });
                                state.status = HarnessStatus::Idle;
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            }
                        }
                        // Fatal errors never recover — fail at once, no backoff.
                        if !retryable {
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            break;
                        }
                        // Race the recovery backoff against the input channel so
                        // Esc cancels during the wait instead of after it.
                        let action = {
                            let recover_fut = self.recover(&mut state, &mut consecutive_errors);
                            tokio::pin!(recover_fut);
                            loop {
                                tokio::select! {
                                    a = &mut recover_fut => break Some(a),
                                    msg = input_rx.recv() => match msg {
                                        Some(LoopInput::Interrupt) | None => break None,
                                        Some(other) => pending_inputs.push(other),
                                    }
                                }
                            }
                        };
                        match action {
                            None => {
                                while let Ok(msg) = input_rx.try_recv() {
                                    if !matches!(msg, LoopInput::Interrupt) {
                                        pending_inputs.push(msg);
                                    }
                                }
                                self.apply_interrupted_pending(&mut state, &mut pending_inputs);
                                state.status = HarnessStatus::Interrupted;
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "interrupted".to_string(),
                                    reasoning: "User interrupted the run.".to_string(),
                                });
                                self.persist(&mut state, &lanes).await?;
                                break;
                            }
                            Some(RecoveryAction::Retry) => {
                                self.persist(&mut state, &lanes).await?;
                            }
                            Some(RecoveryAction::GiveUp) => {
                                state.status = HarnessStatus::Failed;
                                self.persist(&mut state, &lanes).await?;
                                break;
                            }
                        }
                    }
                }
            } else if state.status == HarnessStatus::Interrupted {
                break;
            } else {
                // Idle or WaitingForInput. When a goal is Active and we're Idle (not
                // blocked on a question), DRIVE the loop forward instead of waiting —
                // but a queued real input or a lane report is handled first (biased).
                let goal_driving = state.status == HarnessStatus::Idle
                    && matches!(&state.goal, Some(g) if g.status == GoalStatus::Active);
                tokio::select! {
                    biased;
                    input = input_rx.recv() => match input {
                        Some(LoopInput::UserMessage(text)) | Some(LoopInput::Answer(text)) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            self.accept_user_message(&mut state, &mut vars, text).await;
                            consecutive_errors = 0;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::Notice(event)) => {
                            self.record_notice(&mut state, event);
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::Compact) => {
                            self.run_manual_compaction(model, &mut state, &lanes).await?;
                            state.pending_question = None;
                            state.status = HarnessStatus::Idle;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetMode(mode)) => {
                            state.approval_mode = mode;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetTitle(title)) => {
                            let t = title.trim();
                            state.title = if t.is_empty() { None } else { Some(t.to_string()) };
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SetGoal(text)) => {
                            self.begin_goal(&mut state, text);
                            consecutive_errors = 0;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::ResumeGoal) => {
                            self.resume_goal(&mut state);
                            consecutive_errors = 0;
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::CancelGoal) => {
                            self.end_goal(&mut state);
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::Rewind { checkpoint }) => {
                            match state.apply_checkpoint_rewind(&checkpoint) {
                                Ok(_) => self.persist(&mut state, &lanes).await?,
                                Err(_) => {
                                    // Unknown checkpoint id — leave state unchanged.
                                }
                            }
                        }
                        // No tool call is pending while idle — nothing to approve.
                        Some(LoopInput::Approve) | Some(LoopInput::ApproveAll) | Some(LoopInput::Deny) => {}
                        Some(LoopInput::Queue(item)) => {
                            queue_held(&mut state, item);
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::Unqueue(id)) => {
                            take_queued(&mut state, &id);
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::SteerQueued(id)) => {
                            if let Some(text) = take_queued(&mut state, &id) {
                                self.accept_user_message(&mut state, &mut vars, text).await;
                                consecutive_errors = 0;
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        Some(LoopInput::DropQueued) => {
                            if !state.queued_inputs.is_empty() {
                                state.queued_inputs.clear();
                                self.persist(&mut state, &lanes).await?;
                            }
                        }
                        Some(LoopInput::Interrupt) | None => {
                            state.status = HarnessStatus::Interrupted;
                            self.persist(&mut state, &lanes).await?;
                            break;
                        }
                    },
                    Some(result) = lane_rx.recv() => {
                        self.inject_lane_result(&mut state, &mut lanes, &result);
                        // A lane reporting in while idle is new information to act on.
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        self.persist(&mut state, &lanes).await?;
                    }
                    Some(progress) = progress_rx.recv() => {
                        lanes.record_progress(&progress);
                        self.persist(&mut state, &lanes).await?;
                    }
                    Some(event) = watch_rx.recv() => {
                        // A watched file grew (and matched its filter) while idle —
                        // wake the agent with the appended text.
                        self.inject_watch_event(&mut state, &mut watches, &event);
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        consecutive_errors = 0;
                        self.persist(&mut state, &lanes).await?;
                    }
                    _ = std::future::ready(()), if goal_driving => {
                        // Autonomous goal, nothing else queued: take the next goal turn.
                        self.drive_goal_turn(&mut state, &mut vars);
                        consecutive_errors = 0;
                        self.persist(&mut state, &lanes).await?;
                    }
                }
            }
        }

        Ok(state)
    }
}

