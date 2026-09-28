use super::*;

impl CodingHarness {
    pub(super) async fn accept_user_message(
        &self,
        state: &mut HarnessState,
        vars: &mut LoopVars,
        text: String,
    ) {
        let answering =
            state.status == HarnessStatus::WaitingForInput && state.pending_question.is_some();
        // Snapshot the workspace before acting on a NEW request, so the whole turn
        // (direct edits + any lane changes + bash) can be rewound. An answer
        // continues a turn already checkpointed.
        if !answering {
            // First real request seeds the session title (app sessions open
            // empty, so title starts blank).
            if state
                .title
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                state.title = Some(text.clone());
            }
            self.finish_checkpoint(state, vars).await;
            self.begin_checkpoint(state, vars, &text);
            // Fresh request: prior-turn loop/thought/failure state belongs to
            // the past run.
            vars.last_turn_had_repeat = false;
            vars.last_thought = None;
            vars.turns_this_request = 0;
            vars.consecutive_failed_turns = 0;
        }
        state.pending_question = None;
        state.messages.push(HarnessMessage::User {
            content: if answering {
                format!("[answer]\n{text}")
            } else {
                text.clone()
            },
        });
        state.events.push(HarnessEvent::UserInput { text });
        state.status = HarnessStatus::Running;
        vars.empty_reply_reprompts = 0;
        self.bump_activity();
    }

    /// Run a user-requested compaction pass now, surfacing the compaction UI
    /// (Running + the pass event). Leaves `state.status` Running; callers decide
    /// what follows and persist it.
    pub(super) async fn run_manual_compaction(
        &self,
        model: &mut dyn AgentModel,
        state: &mut HarnessState,
        lanes: &LaneManager,
    ) -> Result<(), ToolError> {
        let before_len = state.messages.len();
        state.status = HarnessStatus::Running;
        state.compacting = true;
        self.persist(state, lanes).await?;
        let result = self.compact_history_agentic(model, state, true).await;
        state.compacting = false;
        result?;
        if state.messages.len() >= before_len {
            state.events.push(HarnessEvent::SystemDecision {
                step: "history_compaction_skipped".to_string(),
                reasoning: "Manual compaction ran, but there was no additional older history left to shrink beyond the preserved recent tail.".to_string(),
            });
        }
        Ok(())
    }

    /// Stamp base64 image bytes onto vision tool results so the model can SEE the
    /// image (`view_image` results, which carry `/data/mime` and a path). Done per-turn on
    /// the cloned request only (never persisted).
    pub(super) fn inline_images(&self, messages: &mut [HarnessMessage], supports_images: bool) {
        use base64::{Engine, engine::general_purpose::STANDARD};
        // Skip absurdly large images so a request can't balloon unboundedly.
        const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
        // Inlined images cost their image tokens again on EVERY subsequent turn,
        // so a long session that read a few screenshots pays for all of them
        // forever. Age out by TURN, not by count: everything the model read in
        // the last few assistant turns stays visible (a 10-screenshot batch is
        // still fully visible while it's being worked on); older results carry a
        // note instead — the model re-reads if it truly needs one back.
        const IMAGE_TURN_WINDOW: usize = 3;
        // Safety cap across the inlined set so one batch can't balloon a request.
        const MAX_TOTAL_IMAGE_BYTES: usize = 24 * 1024 * 1024;

        let is_vision_result = |tool_name: &str| tool_name == "view_image";

        let mut assistant_turns = 0usize;
        let mut cutoff = 0usize;
        for (i, m) in messages.iter().enumerate().rev() {
            if matches!(m, HarnessMessage::Assistant { .. }) {
                assistant_turns += 1;
                if assistant_turns >= IMAGE_TURN_WINDOW {
                    cutoff = i;
                    break;
                }
            }
        }
        // Budget pass, newest first, so when a batch exceeds the total cap it's
        // the OLDEST images that drop to notes.
        let mut allowed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut total_inlined = 0usize;
        for (index, message) in messages.iter().enumerate().rev() {
            let HarnessMessage::ToolResult {
                tool_name, content, ..
            } = message
            else {
                continue;
            };
            if !is_vision_result(tool_name) || index < cutoff {
                continue;
            }
            let Some(path) = content.pointer("/data/path").and_then(Value::as_str) else {
                continue;
            };
            let Ok(resolved) = self.context.resolve_workspace_path(path) else {
                continue;
            };
            let Ok(len) = std::fs::metadata(&resolved).map(|m| m.len() as usize) else {
                continue;
            };
            if len == 0 || len > MAX_IMAGE_BYTES || total_inlined + len > MAX_TOTAL_IMAGE_BYTES {
                continue;
            }
            total_inlined += len;
            allowed.insert(index);
        }
        for (index, message) in messages.iter_mut().enumerate() {
            let HarnessMessage::ToolResult {
                tool_name, content, ..
            } = message
            else {
                continue;
            };
            if !is_vision_result(tool_name) {
                continue;
            }
            let Some(path) = content
                .pointer("/data/path")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            // Text-only model: never inline image bytes (it 400s and poisons every
            // later turn). Leave a note so the model knows an image was read.
            if !supports_images {
                if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                    data.insert(
                        "image_note".to_string(),
                        Value::String(format!(
                            "[image at {path} not shown — the current model is text-only]"
                        )),
                    );
                }
                continue;
            }
            // Aged out of the inline window (or over the total budget): note
            // instead of bytes.
            if !allowed.contains(&index) {
                if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                    data.insert(
                        "image_note".to_string(),
                        Value::String(format!(
                            "[image at {path} was shown in an earlier turn — call view_image again if you need to see it now]"
                        )),
                    );
                }
                continue;
            }
            let Ok(resolved) = self.context.resolve_workspace_path(&path) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&resolved) else {
                continue;
            };
            if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                continue;
            }
            let encoded = STANDARD.encode(&bytes);
            if let Some(data) = content.get_mut("data").and_then(Value::as_object_mut) {
                data.insert("image_base64".to_string(), Value::String(encoded));
            }
        }
    }

    /// Snapshot the workspace before a turn so the user can `/rewind` to it.
    /// Best-effort — a failure (no git, etc.) is skipped, never blocking the turn.
    pub(super) fn begin_checkpoint(&self, state: &HarnessState, vars: &mut LoopVars, prompt: &str) {
        let label: String = prompt.chars().take(80).collect();
        let workspace = self.context.workspace_root().to_path_buf();
        let snap_label = label.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::checkpoint::snapshot_diagnostic(&workspace, &snap_label)
        });
        vars.pending_checkpoint = Some(PendingCheckpoint {
            snapshot,
            label,
            created_at: chrono::Utc::now().to_rfc3339(),
            event_index: state.events.len(),
            message_index: state.messages.len(),
            compactions: state.compactions,
        });
    }

    pub(super) async fn finish_checkpoint(&self, state: &mut HarnessState, vars: &mut LoopVars) {
        let Some(pending) = vars.pending_checkpoint.take() else {
            return;
        };
        let workspace = self.context.workspace_root().to_path_buf();
        let id = match pending.snapshot.await {
            Ok(Ok(id)) => id,
            Ok(Err(error)) => {
                self.debug_log(&format!(
                    "checkpoint skipped: workspace={} label={:?} error={error}",
                    workspace.display(),
                    pending.label
                ));
                return;
            }
            Err(error) => {
                self.debug_log(&format!(
                    "checkpoint skipped: workspace={} label={:?} snapshot task failed: {error}",
                    workspace.display(),
                    pending.label
                ));
                return;
            }
        };
        state.checkpoints.push(CheckpointRecord {
            id,
            label: pending.label,
            created_at: pending.created_at,
            event_index: pending.event_index,
            message_index: pending.message_index,
            compactions: pending.compactions,
        });
        // Cap retained records so a long session doesn't bloat persisted state.
        const MAX_CHECKPOINTS: usize = 8;
        let len = state.checkpoints.len();
        if len <= MAX_CHECKPOINTS {
            return;
        }
        let dropped: Vec<String> = state
            .checkpoints
            .drain(..len - MAX_CHECKPOINTS)
            .map(|c| c.id)
            .collect();
        // Drop ONLY this session's aged-out snapshots: the shadow repo is shared
        // by every session in the workspace. Runs in the background; the shadow
        // lock keeps it from overlapping the next snapshot.
        let keep: Vec<String> = state.checkpoints.iter().map(|c| c.id.clone()).collect();
        tokio::task::spawn_blocking(move || crate::checkpoint::prune(&workspace, &keep, &dropped));
    }

    pub(super) fn new_lane_manager(
        &self,
        factory: Option<ModelFactory>,
        lane_tx: mpsc::UnboundedSender<LaneResult>,
        progress_tx: mpsc::UnboundedSender<crate::lanes::LaneProgress>,
        state: &HarnessState,
    ) -> LaneManager {
        let lane_root = self
            .config
            .state_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join("lanes"))
            .unwrap_or_else(|| self.context.workspace_root().join(".snippet/lanes"));
        LaneManager::new(
            factory,
            self.context.workspace_root().to_path_buf(),
            lane_root,
            lane_tx,
            progress_tx,
            self.config.exa_api_key.clone(),
        )
        .with_records(state.lanes.clone())
    }

    /// Non-blocking drain of steers + lane reports between iterations. Returns
    /// `(interrupted, wants_compact)` — a `/compact` seen here must be run by the
    /// caller (`apply_input` can't do it; it has no model access).
    pub(super) fn drain_pending(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        input_rx: &mut mpsc::UnboundedReceiver<LoopInput>,
        lane_rx: &mut mpsc::UnboundedReceiver<LaneResult>,
        progress_rx: &mut mpsc::UnboundedReceiver<crate::lanes::LaneProgress>,
        watch_rx: &mut mpsc::UnboundedReceiver<WatchEvent>,
    ) -> (bool, bool) {
        let mut interrupted = false;
        let mut wants_compact = false;
        while let Ok(input) = input_rx.try_recv() {
            if matches!(input, LoopInput::Compact) {
                wants_compact = true;
                continue;
            }
            interrupted |= self.apply_input(state, input);
        }
        while let Ok(result) = lane_rx.try_recv() {
            self.inject_lane_result(state, lanes, &result);
        }
        while let Ok(progress) = progress_rx.try_recv() {
            lanes.record_progress(&progress);
        }
        while let Ok(event) = watch_rx.try_recv() {
            self.inject_watch_event(state, watches, &event);
        }
        (interrupted, wants_compact)
    }

    /// Record an event addressed to this session WITHOUT starting a turn.
    ///
    /// The event lands in the transcript and in the model's context, so the
    /// session's agent knows the exchange happened when it next runs — but the
    /// loop stays parked, because a notice is information, not a request.
    pub(super) fn record_notice(&self, state: &mut HarnessState, event: HarnessEvent) {
        if let Some(text) = notice_text(&event) {
            state.messages.push(HarnessMessage::User { content: text });
        }
        state.events.push(event);
    }

    /// Fold notices buffered during a step into the transcript.
    ///
    /// The interrupt paths discard the in-flight turn with a truncate, and a
    /// notice is not part of that turn — it is a record of something that already
    /// happened and that the store has already accepted. Dropping one would lose
    /// the message from the transcript entirely. Anything else stays buffered.
    #[cfg(test)]
    pub(super) fn record_pending_notices(
        &self,
        state: &mut HarnessState,
        pending: &mut Vec<LoopInput>,
    ) {
        pending.retain(|input| match input {
            LoopInput::Notice(event) => {
                self.record_notice(state, event.clone());
                false
            }
            _ => true,
        });
    }

    /// Apply any inputs buffered during a step when the run is interrupted.
    ///
    /// When a step is interrupted (e.g. user stops inference), the in-flight assistant
    /// turn is closed. Any messages sent by the user during or right before stopping —
    /// such as sending a held queued message immediately via `SteerQueued`, or a
    /// mid-run `UserMessage`/`Answer`, or queueing a message — must not be discarded.
    /// They are folded into the transcript and persistent state so that the user's
    /// sent words survive and are preserved across interrupts.
    pub(super) fn apply_interrupted_pending(
        &self,
        state: &mut HarnessState,
        pending: &mut Vec<LoopInput>,
    ) {
        // First pass: register any newly queued inputs so subsequent SteerQueued/Unqueue
        // commands in the same batch can resolve them by ID.
        for input in pending.iter_mut() {
            if let LoopInput::Queue(item) = input {
                queue_held(state, item.clone());
                item.text.clear();
            }
        }

        // Second pass: apply all remaining inputs (steers, user messages, unqueue, notices, mode/title/goal).
        for input in std::mem::take(pending) {
            match input {
                LoopInput::Notice(event) => {
                    self.record_notice(state, event);
                }
                LoopInput::Queue(item) => {
                    queue_held(state, item);
                }
                LoopInput::Unqueue(id) => {
                    take_queued(state, &id);
                }
                LoopInput::DropQueued => {
                    state.queued_inputs.clear();
                }
                LoopInput::SteerQueued(id) => {
                    if let Some(text) = take_queued(state, &id) {
                        state.messages.push(HarnessMessage::User {
                            content: format!("[steer]\n{text}"),
                        });
                        state.events.push(HarnessEvent::Steer { text });
                        self.bump_activity();
                    }
                }
                LoopInput::UserMessage(text) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        state.messages.push(HarnessMessage::User {
                            content: format!("[steer]\n{text}"),
                        });
                        state.events.push(HarnessEvent::Steer { text });
                        self.bump_activity();
                    }
                }
                LoopInput::Answer(text) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        state.messages.push(HarnessMessage::User {
                            content: format!("[answer]\n{text}"),
                        });
                        state.events.push(HarnessEvent::UserInput { text });
                        self.bump_activity();
                    }
                }
                LoopInput::SetMode(mode) => {
                    state.approval_mode = mode;
                }
                LoopInput::SetTitle(title) => {
                    let t = title.trim();
                    state.title = if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    };
                }
                LoopInput::SetGoal(text) => {
                    self.begin_goal(state, text);
                }
                LoopInput::ResumeGoal => {
                    self.resume_goal(state);
                }
                LoopInput::CancelGoal => {
                    self.end_goal(state);
                }
                LoopInput::Rewind { checkpoint } => {
                    let _ = state.apply_checkpoint_rewind(&checkpoint);
                }
                LoopInput::Compact
                | LoopInput::Interrupt
                | LoopInput::Approve
                | LoopInput::ApproveAll
                | LoopInput::Deny => {}
            }
        }
    }

    /// Apply one queued input while a run is active: a message/answer becomes a
    /// `[steer]`, an interrupt returns `true`. Shared by the between-iteration
    /// drain and the buffered-input drain.
    pub(super) fn apply_input(&self, state: &mut HarnessState, input: LoopInput) -> bool {
        match input {
            LoopInput::Notice(event) => {
                self.record_notice(state, event);
                false
            }
            LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    state.messages.push(HarnessMessage::User {
                        content: format!("[steer]\n{text}"),
                    });
                    state.events.push(HarnessEvent::Steer { text });
                    self.bump_activity();
                }
                false
            }
            LoopInput::Compact => {
                // Manual compaction is handled directly by the outer interactive loop;
                // it should not inject a steer or schedule another model turn.
                false
            }
            LoopInput::SetMode(mode) => {
                state.approval_mode = mode;
                false
            }
            LoopInput::Queue(item) => {
                queue_held(state, item);
                false
            }
            LoopInput::Unqueue(id) => {
                take_queued(state, &id);
                false
            }
            LoopInput::SteerQueued(id) => {
                if let Some(text) = take_queued(state, &id) {
                    state.messages.push(HarnessMessage::User {
                        content: format!("[steer]\n{text}"),
                    });
                    state.events.push(HarnessEvent::Steer { text });
                    self.bump_activity();
                }
                false
            }
            LoopInput::DropQueued => {
                state.queued_inputs.clear();
                false
            }
            LoopInput::SetTitle(title) => {
                let t = title.trim();
                state.title = if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                };
                false
            }
            // Approve/Deny are only meaningful while a tool call is awaiting approval
            // inside a step; arriving here (between turns) they're stray no-ops.
            LoopInput::Approve | LoopInput::ApproveAll | LoopInput::Deny => false,
            LoopInput::SetGoal(text) => {
                self.begin_goal(state, text);
                false
            }
            LoopInput::ResumeGoal => {
                self.resume_goal(state);
                false
            }
            LoopInput::CancelGoal => {
                self.end_goal(state);
                false
            }
            LoopInput::Rewind { checkpoint } => {
                let _ = state.apply_checkpoint_rewind(&checkpoint);
                false
            }
            LoopInput::Interrupt => true,
        }
    }

    /// Start (or replace) an autonomous goal and prompt the agent toward it now.
    /// Called from `/goal` (idle or mid-run). The loop then re-drives the agent
    /// each idle turn until the goal completes, the user cancels, or it's paused.
    pub(super) fn begin_goal(&self, state: &mut HarnessState, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        state.goal = Some(Goal {
            text: text.clone(),
            dir: String::new(),
            status: GoalStatus::Active,
            autonomous_turns: 0,
            resume_at: 0,
        });
        state.messages.push(HarnessMessage::User {
            content: goal_start_directive(&text),
        });
        state.events.push(HarnessEvent::SystemDecision {
            step: "goal_set".to_string(),
            reasoning: text,
        });
        state.status = HarnessStatus::Running;
    }

    /// Resume a paused rate-limited goal without replacing its text.
    pub(super) fn resume_goal(&self, state: &mut HarnessState) {
        let Some(goal) = state.goal.as_mut() else {
            return;
        };
        if goal.status != GoalStatus::Paused {
            return;
        }
        goal.status = GoalStatus::Active;
        goal.resume_at = 0;
        let text = goal.text.clone();
        let dir = goal.dir.clone();
        state.messages.push(HarnessMessage::User {
            content: goal_continue_directive(&text, &dir),
        });
        state.events.push(HarnessEvent::SystemDecision {
            step: "goal_resumed".to_string(),
            reasoning: text,
        });
        state.status = HarnessStatus::Running;
    }

    /// Cancel the active goal (user-initiated) — tell the agent to stop and wind down.
    pub(super) fn end_goal(&self, state: &mut HarnessState) {
        let Some(goal) = state.goal.as_mut() else {
            return;
        };
        goal.status = GoalStatus::Cancelled;
        let (text, dir) = (goal.text.clone(), goal.dir.clone());
        state.messages.push(HarnessMessage::User {
            content: goal_cancel_directive(&text, &dir),
        });
        state.events.push(HarnessEvent::SystemDecision {
            step: "goal_cancelled".to_string(),
            reasoning: text,
        });
        state.status = HarnessStatus::Running;
    }

    /// Take one autonomous turn toward the active goal: bump the counter and push
    /// the continue/self-check directive as a fresh user turn. Called from the idle
    /// point when a goal is Active and nothing else is pending.
    pub(super) fn drive_goal_turn(&self, state: &mut HarnessState, vars: &mut LoopVars) {
        let (text, dir, n) = match state.goal.as_mut() {
            Some(g) => {
                g.autonomous_turns += 1;
                (g.text.clone(), g.dir.clone(), g.autonomous_turns)
            }
            None => return,
        };
        let directive = if n % GOAL_SELF_CHECK_EVERY == 0 {
            goal_selfcheck_directive(&text, &dir, n)
        } else {
            goal_continue_directive(&text, &dir)
        };
        state
            .messages
            .push(HarnessMessage::User { content: directive });
        // A goal turn is a fresh turn: reset per-turn loop bookkeeping so discovery
        // and progress tracking start clean (no checkpoint — the goal start already
        // seeded one, and per-turn snapshots would flood the shadow git).
        *vars = LoopVars::default();
        state.status = HarnessStatus::Running;
    }

    /// Fold a file-watch wake into history: advance the persisted tail offset and
    /// inject a `[file_watch]` envelope carrying the appended text, mirroring how
    /// lane reports arrive. The follow-up id is demoted to an internal handle so
    /// the agent speaks of the watch by subject, never "watch-1".
    pub(super) fn inject_watch_event(
        &self,
        state: &mut HarnessState,
        watches: &mut WatchManager,
        event: &WatchEvent,
    ) {
        watches.advance_offset(&event.id, event.new_offset);
        state.watches = watches.records().to_vec();
        let skipped_note = if event.skipped > 0 {
            format!(
                "\n(…{} earlier bytes of this burst omitted — read the file for the full text)",
                event.skipped
            )
        } else {
            String::new()
        };
        // Watched files can carry vault secrets too (a process logging its env) —
        // scrub the appended text like any tool result.
        let appended = crate::vault::Vault::load().scrub_str(event.appended.trim_end());
        state.messages.push(HarnessMessage::User {
            content: format!(
                "[file_watch]\nsubject = \"{}\"\npath = \"{}\"\nappended:{skipped_note}\n{}\n[orchestration] Act on this if it needs action; stay quiet and end the turn if it doesn't. Remove the watch (monitor action:\"remove\") once it has served its purpose.\n[follow_up_id = \"{}\"]  # internal handle for monitor remove ONLY — refer to this work by its SUBJECT to the user, never by this id\n[/file_watch]",
                event.label, event.path, appended, event.id
            ),
        });
        let preview: String = appended.trim().chars().take(120).collect();
        state.events.push(HarnessEvent::SystemDecision {
            step: "file_watch".to_string(),
            reasoning: format!("\"{}\" — {} grew: {preview}", event.label, event.path),
        });
    }

    pub(super) fn inject_lane_result(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        result: &LaneResult,
    ) {
        if !lanes.record_result(result) {
            return;
        }
        let body = match result.status {
            // Prefer the full report (actions + findings + summary); fall back to
            // the concise summary so the parent agent sees what the lane actually did.
            LaneStatus::Completed => result
                .report
                .clone()
                .or_else(|| result.summary.clone())
                .unwrap_or_else(|| "completed".to_string()),
            // A failure is a decision point, not a dead end — spell the options
            // out so the orchestrator recovers instead of stalling or re-briefing
            // from scratch.
            LaneStatus::Failed => format!(
                "FAILED: {}\nRecover deliberately: follow up THIS lane (delegate_task with lane_id=\"{}\") \
                 with a narrower or corrected brief — it keeps everything it learned — or do the slice \
                 yourself if it's small. Don't silently drop this part of the work.",
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string()),
                result.id,
            ),
            LaneStatus::Cancelled => "cancelled".to_string(),
            LaneStatus::Running => "still running".to_string(),
        };
        // Orchestration cue: tell the agent whether it's still waiting on others
        // or holding the complete picture — the difference between a progress
        // note and the final synthesis.
        let outstanding = lanes.active_count();
        let cue = if outstanding > 0 {
            format!(
                "{outstanding} lane(s) still out — fold this in (or note progress) and keep waiting; don't finalize yet."
            )
        } else {
            "ALL delegated lanes have now reported. You hold the complete picture: synthesize the results \
             into the deliverable now (verify load-bearing findings against the cited file:line first). \
             Don't restart the investigation yourself; follow up a specific lane if something is missing."
                .to_string()
        };
        state.messages.push(HarnessMessage::User {
            content: format!(
                "[lane_report]\nsubject = \"{}\"\nstatus = {:?}\n{}\n[orchestration] {}\n[follow_up_id = \"{}\"]  # internal handle for a delegate_task follow-up ONLY — refer to this work by its SUBJECT to the user, never by this id\n[/lane_report]",
                result.title, result.status, body, cue, result.id
            ),
        });
        state.events.push(HarnessEvent::LaneCompleted {
            id: result.id.clone(),
            title: result.title.clone(),
            status: result.status,
            summary: result.summary.clone(),
        });
    }
}

