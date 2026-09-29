use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::harness::{HarnessEvent, HarnessState, HarnessStatus, LoopInput};
use super::keybindings::*;
use super::render::*;
use super::term_pane::*;
use super::views::*;
use super::*;

pub(crate) struct PendingSidecarAttach {
    pub(crate) initial: Option<String>,
    pub(crate) resume: bool,
}

/// A session mutation the picker asked for, to be sent to the daemon on the next
/// tick.
///
/// `handle_key` is synchronous and the daemon call is not, so the key handler
/// records the intent here and the async tick performs it. The daemon owns every
/// session, so routing delete/rename through it is what keeps the store row, its
/// messages, and its events consistent — a file-only delete leaves a migrated
/// conversation readable.
#[derive(Debug, Clone)]
pub(crate) enum PendingSessionOp {
    Delete { session_id: String },
    Rename { session_id: String, title: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Main,
    ResumeSelection,
    Profiles,
    Lanes,
    Term,
    RewindCheckpointSelection,
    ForkCheckpointSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointAction {
    Rewind,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsField {
    Provider,
    ApiKey,
    Model,
    BaseUrl,
    Reasoning,
    ContextWindow,
    Compaction,
    XSearch,
}

pub(crate) struct App {
    pub(crate) options: TuiOptions,
    pub(crate) input: String,
    /// Cursor position in `input`, as a CHAR index (0..=char_count).
    pub(crate) input_cursor: usize,
    /// Submitted inputs (oldest first) for Up/Down history recall.
    pub(crate) input_history: Vec<String>,
    /// Position while navigating history; None = editing the live draft.
    pub(crate) history_pos: Option<usize>,
    /// The in-progress input saved when history navigation begins.
    pub(crate) history_draft: String,
    /// Collapsed pastes: (placeholder shown in the input, real content). A big
    /// paste shows as a compact chip and expands back on send.
    pub(crate) pasted_blocks: Vec<(String, String)>,
    pub(crate) shell: ShellState,
    /// Pending file attachments (images/files) dropped or pasted — kept OUT of the
    /// input text. Shown as a compact "📎 N attachments" line above the prompt and
    /// appended to the message on send. Tuple: (is_image, absolute_path, filename).
    pub(crate) attachments: Vec<(bool, String, String)>,
    pub(crate) status: String,
    /// Auto-dismiss bookkeeping for the transient status line: when it changes we
    /// stamp `status_since`, and `tick` clears the message a few seconds later.
    pub(crate) status_shown: String,
    pub(crate) status_since: Option<std::time::Instant>,
    /// When true, tool call rows show arg/result bodies (Ctrl-O). Collapsed is the
    /// default one-line `● Verb(args)` view.
    pub(crate) tools_expanded: bool,
    /// Set by the startup self-update task to the version it installed; shown in
    /// the header as a "restart to apply" hint.
    pub(crate) update_notice: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Daemon-backed side-pane panels (agents, tasks, jobs, usage, vault).
    pub(crate) boards: super::boards::Boards,
    /// Mission Control was asked for; opened on the next tick.
    pub(crate) pending_mission_control: bool,
    pub(crate) error: Option<String>,
    pub(crate) state: Option<HarnessState>,
    /// The resident conversation loop. Spawned once, lives across turns.
    pub(crate) agent: Option<JoinHandle<Result<HarnessState, String>>>,
    /// Channel to steer / answer / interrupt the resident loop.
    pub(crate) input_tx: Option<UnboundedSender<LoopInput>>,
    /// Transcript scrollback offset, in rendered lines from the bottom
    /// (0 = follow the tail).
    pub(crate) scroll: usize,
    /// Largest valid scroll offset, recomputed each frame from the rendered
    /// transcript so key handlers can clamp.
    pub(crate) max_scroll: Cell<usize>,
    /// Home-abbreviated, canonicalized workspace path for the status bar.
    pub(crate) cwd_display: String,
    /// Animation frame counter for the "working…" spinner.
    pub(crate) frame: usize,
    pub(crate) quit: bool,
    pub(crate) active_conversation: String,
    pub(crate) active_state_path: PathBuf,
    pub(crate) suggestion_index: usize,
    pub(crate) screen: Screen,
    pub(crate) resume_selected_index: usize,
    /// In the resume picker: a `d` was pressed once; a second `d` confirms delete.
    pub(crate) resume_pending_delete: bool,
    /// While renaming a saved session in the resume picker: the in-progress title
    /// buffer (None = not renaming).
    pub(crate) resume_rename: Option<String>,
    /// Inline model suggestion cursor in the login form.
    pub(crate) model_picker_index: usize,
    /// Profiles screen cursor; which profile (if any) the editor is editing; and
    /// whether closing the editor should return to the profiles list.
    pub(crate) profiles_selected_index: usize,
    pub(crate) lanes_selected_index: usize,
    /// Whether the selected lane's full report and activity history are visible.
    pub(crate) lanes_detail_expanded: bool,
    /// Scroll offset (lines from top) inside the selected-lane detail pane.
    pub(crate) lanes_detail_scroll: usize,
    pub(crate) checkpoint_selected_index: usize,
    /// When true, terminal mouse reporting is on (wheel scrolls the TUI; native
    /// drag-select needs Shift+drag in most terminals). On by default; Ctrl+M toggles.
    pub(crate) mouse_capture: bool,
    pub(crate) editing_profile: Option<String>,
    pub(crate) return_to_profiles: bool,
    /// Hold the compaction animation until this instant (time-based, so a fast
    /// render loop during streaming doesn't burn through it).
    pub(crate) compaction_anim_until: Option<std::time::Instant>,
    pub(crate) seen_compactions: usize,
    /// Steers already sent to the loop but not yet in `state.events`. The
    /// composer is cleared on send; without this the words vanish until the
    /// harness writes `HarnessEvent::Steer`.
    pub(crate) pending_steers: Vec<String>,
    /// Was the agent busy on the previous tick? Drives the queue flush on the
    /// busy → not-busy edge.
    pub(crate) was_busy: bool,
    /// Set the instant we hand the loop a new turn (send a message / spawn) and
    /// cleared once the persisted state catches up. `self.state` is read from the
    /// mtime-gated state file, so it lags the live loop: without this, a message
    /// sent in that window sees a stale `Idle`, gets delivered mid-run, and the
    /// harness hands it to the running turn instead of starting a new one — it "disappears".
    /// Treating the agent as busy here makes the follow-up queue instead.
    pub(crate) sent_turn_pending: bool,
    /// The (provider, model) actually driving THIS chat — the per-chat profile
    /// override when set, else the global default. Cached because resolving it
    /// reads the profile sidecar from disk; refreshed on session switch / profile
    /// change so the header and rate-limit gate never show the wrong model.
    pub(crate) effective_model: (String, String),
    /// Snapshot of the session list while the resume picker is open. Rebuilding it
    /// re-reads + deserializes EVERY session file; doing that per keystroke/frame
    /// made the picker laggy at scale. Populated on picker open, refreshed after
    /// rename/delete, dropped on close.
    pub(crate) conv_cache: Option<Vec<(String, String)>>,
    /// The daemon's session catalog for this workspace, refreshed periodically.
    ///
    /// The picker renders from this rather than walking the filesystem: the
    /// daemon owns sessions, and a migrated conversation has no state file at
    /// all, so a directory walk would leave it out of the list entirely.
    pub(crate) daemon_sessions: Option<Vec<crate::serve::sidecar::SessionRow>>,
    pub(crate) daemon_sessions_refresh:
        Option<tokio::task::JoinHandle<Result<Vec<crate::serve::sidecar::SessionRow>, String>>>,
    pub(crate) daemon_sessions_refreshed_at: Option<std::time::Instant>,
    /// Account-wide ChatGPT usage (read from the shared sidecar), shown globally
    /// whenever signed in — not tied to the active chat's own last request.
    pub(crate) global_usage: Option<crate::llm::RateLimitSnapshot>,
    pub(crate) form_provider: String,
    pub(crate) form_api_key: String,
    pub(crate) form_model: String,
    pub(crate) form_model_query: String,
    pub(crate) form_base_url: String,
    pub(crate) form_reasoning_effort: Option<String>,
    pub(crate) form_context_window: String,
    pub(crate) form_compact_at_pct: String,
    pub(crate) form_x_search: bool,
    pub(crate) form_focus: SettingsField,
    pub(crate) form_fetched_models: Option<Vec<String>>,
    pub(crate) models_fetch_handle: Option<tokio::task::JoinHandle<Result<Vec<String>, String>>>,
    /// In-flight model switch for the current daemon-backed conversation.
    pub(crate) model_switch_handle: Option<tokio::task::JoinHandle<Result<(), String>>>,
    /// In-flight ChatGPT sign-in task (browser OAuth or device-code flow), polled in `tick`).
    pub(crate) chatgpt_login_handle:
        Option<tokio::task::JoinHandle<Result<crate::chatgpt_auth::ChatGptTokens, String>>>,
    /// Current device-code prompt, shown while the poll task is running.
    pub(crate) chatgpt_device_code: Option<crate::chatgpt_auth::DeviceCodeInfo>,
    /// In-flight device-code *begin* task (fetches the code to display), polled in `tick`.
    pub(crate) chatgpt_device_begin_handle:
        Option<tokio::task::JoinHandle<Result<crate::chatgpt_auth::DeviceCodeInfo, String>>>,
    /// xAI (Grok/X subscription) device-code sign-in — same shape as ChatGPT's.
    pub(crate) xai_login_handle: Option<tokio::task::JoinHandle<Result<crate::xai_auth::XaiTokens, String>>>,
    pub(crate) xai_device_code: Option<crate::xai_auth::DeviceCodeInfo>,
    pub(crate) xai_device_begin_handle:
        Option<tokio::task::JoinHandle<Result<crate::xai_auth::DeviceCodeInfo, String>>>,
    pub(crate) models_fetch_status: String,
    pub(crate) original_config: Option<crate::config::SnippetConfig>,
    /// Change-detector for the active session's state, taken from the state
    /// itself rather than the file's mtime — a session in the database has no
    /// file to stat, so mtime would never change and the view would never update.
    pub(crate) last_state_stamp: Option<(String, usize)>,
    /// When true, the compact inline model-connect form is shown and owns key
    /// input. It edits the shared `form_*` state.
    pub(crate) login_active: bool,
    /// Interactive ask_user picker state. `q_index` is the question being
    /// answered (questions are answered in order), `q_sel` the highlighted choice
    /// for the current question, `q_answers` the (question_text, answer) pairs
    /// collected so far, and `q_token` a fingerprint of the current question set
    /// used to detect a fresh ask and reset the cursor.
    pub(crate) q_index: usize,
    pub(crate) q_sel: usize,
    pub(crate) q_answers: Vec<(String, String)>,
    pub(crate) q_token: String,
    /// Live text the running agent is streaming this turn. Shared with the agent
    /// task; rendered as a transient block at the transcript tail and cleared
    /// whenever a newer persisted state loads (the turn has committed).
    pub(crate) stream: crate::llm::StreamHandle,
    /// When set, the TUI is a pure client of the local serve daemon. The daemon
    /// owns `run_interactive` and every `state.json` write; the TUI only renders
    /// state and forwards input over WS `/attach`.
    pub(crate) sidecar: Option<crate::serve::sidecar::DaemonInfo>,
    /// Active WS attachment to the daemon for the current conversation.
    pub(crate) sidecar_attach: Option<crate::serve::sidecar::SidecarAttach>,
    /// Set by `spawn_loop` in sidecar mode; consumed by `ensure_sidecar_attached`.
    pub(crate) pending_sidecar_attach: Option<PendingSidecarAttach>,
    /// A session mutation requested from the picker, performed on the next tick
    /// (the key handler is sync; the daemon call is not).
    pub(crate) pending_session_op: Option<PendingSessionOp>,
    /// Interactive session PTYs (daemon-owned). Rendered when `screen == Term`.
    pub(crate) term_panes: Vec<TermPane>,
    pub(crate) term_focus: usize,
    /// When set, the main canvas shows a connecting screen instead of the empty
    /// transcript — used while discovering/starting the local serve daemon.
    pub(crate) connecting_phase: Option<String>,
}


impl App {
    pub(crate) fn new(options: TuiOptions) -> Self {
        // Apply the persisted theme (if any) before the first render.
        if let Some(name) = options.config.theme.as_deref() {
            set_theme_by_name(name);
        }
        let status = if options.config.resume_on_start {
            "Resuming the saved run...".to_string()
        } else {
            "Type a task and press Enter. Type /new for a new session, or /resume to resume the last session."
                .to_string()
        };
        let cwd_display = home_path(&options.config.workspace);
        let active_state_path = options.config.state_path.clone();
        let mut app = Self {
            options,
            input: String::new(),
            input_cursor: 0,
            input_history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            pasted_blocks: Vec::new(),
            shell: ShellState::default(),
            attachments: Vec::new(),
            status,
            status_shown: String::new(),
            status_since: None,
            tools_expanded: false,
            error: None,
            state: None,
            agent: None,
            input_tx: None,
            scroll: 0,
            max_scroll: Cell::new(0),
            cwd_display,
            frame: 0,
            quit: false,
            active_conversation: "default".to_string(),
            active_state_path,
            suggestion_index: 0,
            screen: Screen::Main,
            resume_selected_index: 0,
            resume_pending_delete: false,
            resume_rename: None,

            model_picker_index: 0,
            profiles_selected_index: 0,
            lanes_selected_index: 0,
            lanes_detail_expanded: false,
            lanes_detail_scroll: 0,
            checkpoint_selected_index: 0,
            mouse_capture: true,
            editing_profile: None,
            return_to_profiles: false,
            compaction_anim_until: None,
            seen_compactions: usize::MAX, // uninitialized; first tick seeds it, no flash

            pending_steers: Vec::new(),
            sent_turn_pending: false,
            effective_model: (String::new(), String::new()),
            conv_cache: None,
            daemon_sessions: None,
            daemon_sessions_refresh: None,
            daemon_sessions_refreshed_at: None,
            global_usage: None,
            was_busy: false,
            form_provider: String::new(),
            form_api_key: String::new(),
            form_model: String::new(),
            form_model_query: String::new(),
            form_base_url: String::new(),
            form_reasoning_effort: None,
            form_context_window: String::new(),
            form_compact_at_pct: String::new(),
            form_x_search: false,
            form_focus: SettingsField::Provider,
            form_fetched_models: None,
            models_fetch_handle: None,
            model_switch_handle: None,
            chatgpt_login_handle: None,
            chatgpt_device_code: None,
            chatgpt_device_begin_handle: None,
            xai_login_handle: None,
            xai_device_code: None,
            xai_device_begin_handle: None,
            models_fetch_status: String::new(),
            update_notice: std::sync::Arc::new(std::sync::Mutex::new(None)),
            boards: Default::default(),
            pending_mission_control: false,
            original_config: None,
            last_state_stamp: None,
            login_active: false,
            q_index: 0,
            q_sel: 0,
            q_answers: Vec::new(),
            q_token: String::new(),
            stream: crate::llm::StreamHandle::default(),
            sidecar: None,
            sidecar_attach: None,
            pending_sidecar_attach: None,
            pending_session_op: None,
            term_panes: Vec::new(),
            term_focus: 0,
            connecting_phase: Some("Looking for local serve…".to_string()),
        };
        app.init_settings_form();

        if let Some(id) = app.options.resume.clone() {
            // Explicit --resume <id> wins: reopen exactly that conversation.
            app.switch_conversation(&id);
        } else if app.options.config.resume_on_start {
            if let Some(last_active) = app.find_last_active_conversation() {
                app.switch_conversation(&last_active);
            }
        } else {
            let name = uuid::Uuid::new_v4().to_string();
            app.switch_conversation(&name);
        }

        let chatgpt_ready =
            app.options.config.model.provider == "chatgpt" && crate::chatgpt_auth::is_signed_in();
        if app.options.config.model.api_key.trim().is_empty() && !chatgpt_ready {
            app.status = "No model connected yet — type /model to connect one.".to_string();
        }

        // Daemon discovery is async (see run_app) — App::new stays sync.
        app
    }

    /// Open the profiles screen (the model page). Migrates a lone `[model]` into a
    /// named profile so everything is managed uniformly, then selects the active one.

    pub(crate) fn agent_alive(&self) -> bool {
        if let Some(attach) = &self.sidecar_attach {
            return attach.is_connected();
        }
        self.agent
            .as_ref()
            .map(|handle| !handle.is_finished())
            .unwrap_or(false)
    }

    pub(crate) fn send_loop_input(&mut self, input: LoopInput) -> Result<(), String> {
        if let Some(attach) = &self.sidecar_attach {
            return attach.send(input);
        }
        if let Some(tx) = &self.input_tx {
            return tx
                .send(input)
                .map_err(|_| "agent loop is no longer accepting input".to_string());
        }
        Err("no live session".to_string())
    }

    /// `true` only while the agent is actively processing a turn (or a lane is).
    /// The resident loop stays *alive* between turns waiting for input, so
    /// `agent_alive()` is the wrong test for "is it safe to act now" — use this for
    /// guards like `/model` and `/rewind` so they aren't blocked when merely idle.
    pub(crate) fn agent_busy(&self) -> bool {
        // `sent_turn_pending` covers the lag between handing off a turn and the
        // state file reflecting Running — so a fast follow-up queues, not steers.
        (self.sent_turn_pending && self.agent_alive())
            || (self.agent_alive()
                && self.state.as_ref().is_some_and(|s| {
                    s.status == HarnessStatus::Running
                        || s.lanes.iter().any(|l| l.status == LaneStatus::Running)
                }))
    }

    /// True while the harness is mid-compaction (recent compaction-pass event + still
    /// running) — used to hold input and label the wait.
    pub(crate) fn is_compacting(&self) -> bool {
        if self
            .compaction_anim_until
            .is_some_and(|t| std::time::Instant::now() < t)
        {
            return true;
        }
        self.state.as_ref().is_some_and(|s| {
            s.compacting
                || (s.status == HarnessStatus::Running
                    && matches!(
                        s.events.last(),
                        Some(HarnessEvent::SystemDecision { step, .. })
                            if step == "history_compaction_pass"
                    ))
        })
    }

    /// The mutating tool call currently awaiting approval (manual mode), if any.
    pub(crate) fn pending_approval(&self) -> Option<(String, String, usize, usize)> {
        let s = self.state.as_ref()?;
        if s.status != HarnessStatus::WaitingForInput {
            return None;
        }
        match s.events.last() {
            Some(HarnessEvent::ApprovalRequest {
                tool_name,
                summary,
                index,
                total,
            }) => Some((tool_name.clone(), summary.clone(), *index, *total)),
            _ => None,
        }
    }

    /// Toggle crossterm mouse capture. Off = native terminal text selection.
    /// On = wheel scrolls transcript/lanes (Shift+drag still selects in most terminals).
    pub(crate) fn set_mouse_capture(&mut self, on: bool) {
        if on == self.mouse_capture {
            return;
        }
        let result = if on {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        };
        if result.is_ok() {
            self.mouse_capture = on;
        }
    }

    pub(crate) fn scroll_up(&mut self, lines: usize) {
        self.scroll = (self.scroll + lines).min(self.max_scroll.get());
    }

    pub(crate) fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Submit the input box: start the resident loop if it isn't running, else
    /// send the text as a steer / answer into the live loop.
    /// Commit the answer to the current ask_user question. A choice question
    /// resolves to the selected option's value; a free-text question to the typed
    /// input. When the last question is answered, the whole set is sent to the
    /// loop as one `[answer]`.
    pub(crate) fn answer_current_question(&mut self) {
        let qs = questions_of(self);
        if qs.is_empty() {
            return;
        }
        let idx = self.q_index.min(qs.len() - 1);
        let question = &qs[idx];
        let opts = q_options(question);

        let answer = if opts.is_empty() {
            let typed = self.message_for_send();
            let typed = typed.trim().to_string();
            if typed.is_empty() {
                self.status = "Type an answer, then press Enter.".to_string();
                return;
            }
            self.input_clear();
            typed
        } else {
            opts[self.q_sel.min(opts.len() - 1)].0.clone()
        };

        self.q_answers.push((q_text(question), answer));
        self.q_sel = 0;
        self.q_index += 1;

        if self.q_index >= qs.len() {
            let combined = if self.q_answers.len() == 1 {
                self.q_answers[0].1.clone()
            } else {
                self.q_answers
                    .iter()
                    .enumerate()
                    .map(|(i, (q, a))| format!("{}. {} → {}", i + 1, q, a))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            self.finish_answer(combined);
        }
    }

    /// Send a completed answer set into the live loop and reset picker state.
    pub(crate) fn finish_answer(&mut self, text: String) {
        if self.agent_alive() {
            if let Err(error) = self.send_loop_input(LoopInput::Answer(text)) {
                self.error = Some(error);
            } else {
                // The answer resumes the turn — treat as busy so a fast follow-up
                // queues rather than steering (mirrors submit_text).
                self.sent_turn_pending = true;
                self.was_busy = true;
            }
        }
        self.input_clear();
        self.scroll = 0;
        self.q_token.clear();
        self.q_index = 0;
        self.q_sel = 0;
        self.q_answers.clear();
    }

    // --- Prompt line editing. Cursor is a CHAR index into `self.input`. ---


    pub(crate) fn spawn_loop(&mut self, initial: Option<String>, resume: bool) {
        if self.agent_alive() {
            // Already attached / running — if we have an initial message, deliver it.
            if let Some(text) = initial {
                let _ = self.send_loop_input(LoopInput::UserMessage(text));
                self.sent_turn_pending = true;
                self.was_busy = true;
            }
            return;
        }
        self.error = None;
        self.scroll = 0;
        // Fresh loop: clear any stale optimistic-busy flag (submit_text re-sets it
        // when it spawns with a message).
        self.sent_turn_pending = false;
        // Don't announce activity in the footer — the in-transcript spinner is the
        // live indicator, and the resident loop never "finishes" between turns so a
        // footer label here would just go stale.
        self.status = String::new();

        crate::llm::StreamBuffer::clear(&self.stream);

        // Daemon owns every session loop. Queue an async attach; tick runs it.
        // If the daemon isn't connected yet, attach will surface the error — there
        // is no in-process agent fallback.
        self.pending_sidecar_attach = Some(PendingSidecarAttach { initial, resume });
    }

    pub(crate) fn interrupt_or_quit(&mut self) {
        // Interrupt only while a turn is actually executing. The resident loop
        // stays ALIVE between turns by design, so gating on agent_alive() made
        // Ctrl+C unable to quit whenever a session was loaded — the terminal
        // convention (Ctrl+C exits an idle program) applies when merely idle.
        if self.agent_busy() {
            let _ = self.send_loop_input(LoopInput::Interrupt);
            self.status = String::new();
        } else {
            self.quit = true;
        }
    }

    pub(crate) async fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.ensure_sidecar_attached().await;
        // Keep the daemon's session catalog current. Without this the picker
        // falls back to the disk walk, which cannot see a migrated conversation.
        self.refresh_daemon_sessions(false).await;
        // Then apply any picker mutation queued by a key handler.
        self.apply_pending_session_op().await;
        self.apply_pending_mission_control().await;
        self.poll_boards();
        // If the sidecar dropped, clear attachment so the next spawn can recover.
        if self
            .sidecar_attach
            .as_ref()
            .is_some_and(|a| !a.is_connected())
        {
            self.sidecar_attach = None;
        }
        self.refresh_state().await;
        self.drain_term_frames();
        self.reap_dead_terms();
        // Open may have been skipped while /attach was still coming up.
        if self.sidecar_attach.is_some() && self.screen == Screen::Term {
            let saved = self.term_focus;
            for i in 0..self.term_panes.len() {
                if !self.term_panes[i].opened {
                    self.term_focus = i;
                    self.send_term_open();
                }
            }
            self.term_focus = saved.min(self.term_panes.len().saturating_sub(1));
        }

        // Transient status line auto-dismisses: stamp it when it changes, clear it
        // a few seconds later so confirmations ("✓ … resumed") don't linger.
        if self.status != self.status_shown {
            self.status_shown = self.status.clone();
            self.status_since = (!self.status.is_empty()).then(std::time::Instant::now);
        }
        if let Some(since) = self.status_since {
            if since.elapsed() > Duration::from_secs(4) {
                self.status.clear();
                self.status_shown.clear();
                self.status_since = None;
            }
        }

        // Refresh the account-wide ChatGPT usage a few times a second (tiny file,
        // signed-in users only) so the footer figure is global, not per-chat.
        if self.frame % 8 == 0 && crate::chatgpt_auth::is_signed_in() {
            self.global_usage = crate::chatgpt::read_global_usage();
        }

        // The daemon flushes held messages when a run lands on idle. Local busy
        // still tracks the lag between send and persisted Running.
        let busy = self.agent_busy();
        // Interrupting a turn usually leaves the resident loop ALIVE (idle), so the
        // agent-finished branch never clears the "Interrupting..." footer — clear it
        // here the moment the loop is observed no longer busy.
        if !busy && self.status.starts_with("Interrupting") {
            self.status = String::new();
        }
        self.was_busy = busy;

        // Hold the animation briefly when a new compaction lands.
        let compactions = self
            .state
            .as_ref()
            .map(|s| {
                s.events
                    .iter()
                    .filter(|e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"))
                    .count()
            })
            .unwrap_or(0);
        if compactions > self.seen_compactions {
            self.compaction_anim_until =
                Some(std::time::Instant::now() + Duration::from_millis(1500));
        }
        self.seen_compactions = compactions;

        if self
            .models_fetch_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.models_fetch_handle.take().expect("checked is_some");
            match handle.await {
                Ok(Ok(models)) => {
                    let count = models.len();
                    // Seed a model only if none is chosen yet (e.g. a custom
                    // endpoint with no default); never override a real choice.
                    if self.form_model.trim().is_empty() {
                        if let Some(first) = models.first() {
                            self.form_model = first.clone();
                        }
                    }
                    self.form_fetched_models = Some(models);
                    self.models_fetch_status = format!("Loaded {count} models from provider.");
                }
                Ok(Err(error)) => {
                    self.models_fetch_status = format!("Fetch failed: {}", error);
                }
                Err(error) => {
                    self.models_fetch_status = format!("Fetch task crashed: {}", error);
                }
            }
        }

        if self
            .model_switch_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.model_switch_handle.take().expect("checked is_some");
            match handle.await {
                Ok(Ok(())) => {
                    self.sidecar_attach = None;
                    self.pending_sidecar_attach = Some(PendingSidecarAttach {
                        initial: None,
                        resume: true,
                    });
                    self.refresh_effective_model();
                    self.status = "✓ current chat model changed".to_string();
                }
                Ok(Err(error)) => {
                    self.status = format!("model switch failed: {error}");
                    self.refresh_effective_model();
                }
                Err(error) => {
                    self.status = format!("model switch task failed: {error}");
                    self.refresh_effective_model();
                }
            }
        }
        if self
            .chatgpt_device_begin_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self
                .chatgpt_device_begin_handle
                .take()
                .expect("checked is_some");
            match handle.await {
                Ok(Ok(info)) => {
                    copy_to_clipboard(&info.user_code);
                    self.status = format!(
                        "Code {} copied — open {} and paste it. Waiting for sign-in…",
                        info.user_code, info.verification_url
                    );
                    self.chatgpt_device_code = Some(info.clone());
                    self.chatgpt_login_handle = Some(tokio::spawn(async move {
                        crate::chatgpt_auth::complete_device_code_login(info).await
                    }));
                }
                Ok(Err(error)) => self.status = format!("device-code start failed: {error}"),
                Err(error) => self.status = format!("device-code task crashed: {error}"),
            }
        }

        if self
            .chatgpt_login_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.chatgpt_login_handle.take().expect("checked is_some");
            match handle.await {
                Ok(Ok(tokens)) => {
                    self.chatgpt_device_code = None;
                    let email = tokens.email.clone();
                    self.finish_chatgpt_login(email);
                }
                Ok(Err(error)) => {
                    self.chatgpt_device_code = None;
                    self.status = format!("ChatGPT sign-in failed: {error}");
                }
                Err(error) => {
                    self.chatgpt_device_code = None;
                    self.status = format!("ChatGPT sign-in task crashed: {error}");
                }
            }
        }

        // xAI device-code: the begin task fetched the code → show it and poll.
        if self
            .xai_device_begin_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self
                .xai_device_begin_handle
                .take()
                .expect("checked is_some");
            match handle.await {
                Ok(Ok(info)) => {
                    copy_to_clipboard(&info.user_code);
                    self.status = format!(
                        "Code {} copied — open {} and paste it. Waiting for sign-in…",
                        info.user_code, info.verification_uri
                    );
                    let poll = info.clone();
                    self.xai_device_code = Some(info);
                    self.xai_login_handle = Some(tokio::spawn(async move {
                        let tokens = crate::xai_auth::poll_for_tokens(poll).await?;
                        crate::xai_auth::save_blocking(&tokens)?;
                        Ok::<_, String>(tokens)
                    }));
                }
                Ok(Err(error)) => self.status = format!("xAI device-code start failed: {error}"),
                Err(error) => self.status = format!("xAI device-code task crashed: {error}"),
            }
        }
        if self
            .xai_login_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.xai_login_handle.take().expect("checked is_some");
            match handle.await {
                Ok(Ok(_)) => {
                    self.xai_device_code = None;
                    self.finish_xai_login();
                }
                Ok(Err(error)) => {
                    self.xai_device_code = None;
                    self.status = format!("xAI sign-in failed: {error}");
                }
                Err(error) => {
                    self.xai_device_code = None;
                    self.status = format!("xAI sign-in task crashed: {error}");
                }
            }
        }

        if self
            .agent
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.agent.take().expect("checked is_some");
            self.input_tx = None;
            match handle.await {
                Ok(Ok(_state)) => self.status = String::new(),
                Ok(Err(error)) => {
                    self.status = "Run failed.".to_string();
                    self.error = Some(error);
                }
                Err(error) => {
                    self.status = "Run task crashed.".to_string();
                    self.error = Some(error.to_string());
                }
            }
            self.refresh_state().await;
        }
    }

    pub(crate) async fn refresh_state(&mut self) {
        // Sidecar: state arrives over WS. Pull the latest snapshot/delta and
        // mirror the live stream handle the attach task keeps updated.
        let sidecar_update = self
            .sidecar_attach
            .as_ref()
            .map(|attach| (attach.stream.clone(), attach.state_rx.borrow().clone()));
        if let Some((stream, maybe_state)) = sidecar_update {
            if let Some(state) = maybe_state {
                // A newer committed state clears optimistic-busy and the live
                // stream is already managed by wire frames (snapshot clears it).
                if self.state.as_ref().map(|s| s.events.len()) != Some(state.events.len())
                    || self.state.as_ref().map(|s| s.status) != Some(state.status)
                {
                    self.sent_turn_pending = false;
                }
                self.state = Some(state);
                self.prune_pending_steers();
            }
            // Keep the TUI's stream handle pointing at the attach's buffer so
            // the renderer (which reads `app.stream`) stays live.
            self.stream = stream;
            return;
        }

        // Fallback path: no live attach yet (daemon still coming up, or the
        // attach failed). Read through the dual reader so a session in the
        // database renders here too — reading the file directly would show a
        // migrated conversation as empty.
        //
        // Change is detected from the state itself, not the file's mtime: a
        // database-backed session has no file to stat, so mtime would never move
        // and the view would freeze on its first paint.
        let Some(state) = crate::session::read_session_state(&self.active_state_path) else {
            self.state = None;
            self.last_state_stamp = None;
            return;
        };
        let stamp = (
            state.updated_at.clone(),
            state.events.len() + state.messages.len(),
        );
        if Some(&stamp) == self.last_state_stamp.as_ref() {
            return;
        }
        self.last_state_stamp = Some(stamp);
        // The persisted state has caught up with our optimistic send — from
        // here the real status governs busy/idle (and the flush edge).
        self.sent_turn_pending = false;
        // A newer persisted state means the turn that was streaming has
        // committed its text into events — drop the live buffer so the
        // committed copy doesn't render twice.
        crate::llm::StreamBuffer::clear(&self.stream);
        self.state = Some(state);
        self.prune_pending_steers();
    }

    /// If a session attach is pending, open/attach it via the local serve daemon.
    pub(crate) async fn ensure_sidecar_attached(&mut self) {
        let Some(pending) = self.pending_sidecar_attach.take() else {
            return;
        };
        // No daemon info (startup failed or prior attach cleared it) → enable/start.
        if self.sidecar.is_none() {
            match crate::serve::sidecar::discover_or_start(&self.options.config_path).await {
                Ok(info) => {
                    self.sidecar = Some(info);
                    self.status = String::new();
                }
                Err(error) => {
                    self.error = Some(error);
                    // Keep the pending intent so a later tick can retry once serve is up.
                    self.pending_sidecar_attach = Some(pending);
                    return;
                }
            }
        }
        let Some(info) = self.sidecar.clone() else {
            self.error = Some("snippet serve is not available".to_string());
            self.pending_sidecar_attach = Some(pending);
            return;
        };

        // A known session (state file, catalog or store) → attach by path and the
        // daemon resumes it. Only a genuinely new conversation is created with
        // POST /sessions; treating a stored session as new is what started a
        // fresh chat instead of resuming the one picked.
        if !self.session_known() {
            let folder = self.options.config.workspace.clone();
            let new_conversation = self.active_conversation != "default";
            match crate::serve::sidecar::open_session(
                &info,
                &folder,
                pending.resume && !new_conversation,
                new_conversation,
            )
            .await
            {
                Ok(session_id) => {
                    // Don't use state_path_for_id here — it canonicalize()s and
                    // fails before the first persist creates the file. Join the
                    // workspaces root directly.
                    let sp = crate::config::workspaces_root().join(&session_id);
                    self.active_state_path = sp.clone();
                    if let Some(stem) = sp.file_stem().and_then(|s| s.to_str()) {
                        if stem != "state" {
                            self.active_conversation = stem.to_string();
                        }
                    }
                }
                Err(error) => {
                    self.status = format!("sidecar open: {error}");
                }
            }
        }

        match crate::serve::sidecar::attach(&info, &self.active_state_path).await {
            Ok(attach) => {
                if let Some(text) = pending.initial {
                    let _ = attach.send(LoopInput::UserMessage(text));
                    self.sent_turn_pending = true;
                    self.was_busy = true;
                }
                self.stream = attach.stream.clone();
                self.sidecar_attach = Some(attach);
                self.input_tx = None;
                self.agent = None;
            }
            Err(error) => {
                // The session exists but the daemon can't resolve it yet — open
                // then re-attach.
                if self.session_known() {
                    let folder = self.options.config.workspace.clone();
                    let _ = crate::serve::sidecar::open_session(&info, &folder, true, false).await;
                    if let Ok(attach) =
                        crate::serve::sidecar::attach(&info, &self.active_state_path).await
                    {
                        if let Some(text) = pending.initial {
                            let _ = attach.send(LoopInput::UserMessage(text));
                            self.sent_turn_pending = true;
                            self.was_busy = true;
                        }
                        self.stream = attach.stream.clone();
                        self.sidecar_attach = Some(attach);
                        self.input_tx = None;
                        self.agent = None;
                        return;
                    }
                }
                // Drop cached daemon info so the next tick re-enables/starts and retries.
                self.sidecar = None;
                self.error = Some(format!("sidecar attach failed: {error}"));
                self.pending_sidecar_attach = Some(pending);
            }
        }
    }
}


pub(crate) async fn run_app(
    terminal: &mut TuiTerminal,
    options: TuiOptions,
) -> Result<ExitInfo, Box<dyn std::error::Error>> {
    let mut app = App::new(options);
    // Paint immediately so startup never sits on a blank alt-screen while serve
    // is discovered/started (that path can take several seconds).
    terminal.draw(|frame| render(frame, &mut app))?;

    // Serve daemon is the sole session runtime. Enable + start it if needed so
    // the TUI never runs `run_interactive` in-process (avoids TUI+mobile state races).
    let config_path = app.options.config_path.clone();
    match connect_serve_with_ui(terminal, &mut app, &config_path).await {
        Ok(info) => {
            app.sidecar = Some(info);
            app.connecting_phase = None;
            app.status = String::new();
        }
        Err(error) => {
            app.connecting_phase = None;
            app.error = Some(error);
        }
    }
    terminal.draw(|frame| render(frame, &mut app))?;

    app.refresh_effective_model();
    app.connecting_phase = Some("Opening session…".to_string());
    terminal.draw(|frame| render(frame, &mut app))?;
    app.refresh_state().await;
    // Always attach through the daemon when we have one (or will retry).
    app.spawn_loop(None, true);
    app.ensure_sidecar_attached().await;
    app.connecting_phase = None;

    // Best-effort self-update in the background: if a newer release exists, it's
    // downloaded and the binary is replaced in place; the header then shows a
    // "restart to apply" hint. Never blocks startup; failures are silent.
    if !crate::update::disabled() {
        let slot = app.update_notice.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            if let Some(version) = crate::update::check_and_update(&client).await {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(version);
                }
            }
        });
    }

    while !app.quit {
        app.tick().await;
        terminal.draw(|frame| render(frame, &mut app))?;

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                // With the kitty protocol a key can arrive as Press/Repeat/Release;
                // act on Press/Repeat only so a key isn't handled twice.
                Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(&mut app, key),
                Event::Paste(text) => {
                    if app.screen == Screen::Term {
                        app.send_term_bytes(text.as_bytes());
                    } else if app.login_active {
                        app.login_paste(&text);
                    } else {
                        app.input_paste(&text);
                    }
                }
                // Mouse wheel scrolls the transcript (chat canvas).
                Event::Mouse(me) => match me.kind {
                    MouseEventKind::ScrollUp => {
                        if app.screen == Screen::Lanes {
                            app.lanes_detail_scroll = app.lanes_detail_scroll.saturating_sub(3);
                        } else {
                            app.scroll_up(1);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if app.screen == Screen::Lanes {
                            app.lanes_detail_scroll = app.lanes_detail_scroll.saturating_add(3);
                        } else {
                            app.scroll_down(1);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    // Freshest token totals for the closing summary.
    app.refresh_state().await;
    let st = app.state.as_ref();
    Ok(ExitInfo {
        conversation: app.active_conversation.clone(),
        config_path: app.options.config_path.clone(),
        prompt_tokens: st.map(|s| s.prompt_tokens).unwrap_or(0),
        completion_tokens: st.map(|s| s.completion_tokens).unwrap_or(0),
        cache_read_tokens: st.map(|s| s.cache_read_tokens).unwrap_or(0),
        total_tokens: st.map(|s| s.total_tokens).unwrap_or(0),
    })
}

