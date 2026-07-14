use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::config::SnippetConfig;
use crate::harness::{HarnessEvent, HarnessState, HarnessStatus, LoopInput};
use crate::lanes::LaneStatus;

mod markdown;
mod theme;
mod transcript;

use markdown::*;
use theme::*;
use transcript::*;

/// Meta tools render through their own dedicated events (Note / AssistantText /
/// UserQuestion / LaneSpawned), so their raw tool-call/result rows are hidden to
/// avoid duplication.
const HIDDEN_TOOL_ROWS: [&str; 7] =
    ["terminate_loop", "note", "notify_user", "ask_user", "delegate_task", "complete_goal", "monitor"];

/// Cap on bash/output preview lines shown inline before collapsing to a count.

const ALL_COMMANDS: &[(&str, &str)] = &[
    ("/new", "Start a new session"),
    ("/resume", "Resume a saved session"),
    ("/rewind", "Restore the workspace to a checkpoint"),
    ("/model", "Connect or change the AI model"),
    ("/compact", "Compact older conversation history now"),
    ("/goal", "Set an autonomous goal the agent drives to completion (/goal cancel to stop)"),
    ("/mode", "Toggle manual approval (bash & file edits ask y/n)"),
    ("/theme", "Switch the color theme"),
];

#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub config_path: PathBuf,
    pub config: SnippetConfig,
    /// A conversation id to resume on launch (from `--resume`).
    pub resume: Option<String>,
}

/// What to print after the TUI closes: how to get back in, and token usage.
struct ExitInfo {
    conversation: String,
    config_path: PathBuf,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    total_tokens: u64,
}

pub async fn run_tui(options: TuiOptions) -> Result<(), Box<dyn std::error::Error>> {
    // A panic anywhere in the TUI unwinds past `restore_terminal`, leaving the
    // user's shell in raw mode with mouse capture on. Restore first, then report.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        default_hook(info);
    }));
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, options).await;
    restore_terminal(&mut terminal)?;
    match result {
        Ok(info) => {
            info.print();
            Ok(())
        }
        Err(error) => Err(error),
    }
}

impl ExitInfo {
    /// Printed to the normal terminal after the alt-screen is torn down.
    fn print(&self) {
        if self.conversation.is_empty() {
            return;
        }
        if self.total_tokens > 0 {
            println!(
                "↑{} in · ↓{} out · ↻{} cached · {} total",
                fmt_si(self.prompt_tokens),
                fmt_si(self.completion_tokens),
                fmt_si(self.cache_read_tokens),
                fmt_si(self.total_tokens),
            );
        }
        let default_config = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".snippet/config.toml"))
            .unwrap_or_default();
        let config_flag = if self.config_path == default_config {
            String::new()
        } else {
            format!(" --config {}", self.config_path.display())
        };
        println!("snippet --resume {}{}", self.conversation, config_flag);
    }
}

/// A real conversation state file: `<name>.json`, but NOT the `<name>.meta.json`
/// sidecar (`Path::extension` of `foo.meta.json` is still `json`). Sidecars are
/// written after every persist, so treating them as sessions made the resume
/// picker list phantoms — and resuming one wrote real state over the sidecar.
fn is_conversation_json(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| !s.ends_with(".meta"))
            .unwrap_or(false)
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<TuiTerminal, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    // Request the kitty keyboard protocol where supported (iTerm2, kitty, WezTerm,
    // Ghostty…) so modified keys like Shift+Enter are reported distinctly. Plain
    // terminals (Apple Terminal) ignore it and keep the Alt+Enter fallback.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    ResumeSelection,
    ThemeSelection,
    Profiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Provider,
    ApiKey,
    Model,
    BaseUrl,
    Reasoning,
    ContextWindow,
    Compaction,
}

/// Providers offered by the login form, in display order.
const LOGIN_PROVIDERS: &[&str] = &["openai", "chatgpt", "anthropic", "gemini", "openrouter", "openai-compatible", "anthropic-compatible"];

/// Providers that talk to a user-supplied endpoint, so the login form shows the
/// Base URL field (and can fetch models keyless).
fn provider_needs_base_url(provider: &str) -> bool {
    provider == "openai-compatible" || provider == "anthropic-compatible"
}

/// Single source of truth for a provider's default base URL and model.
fn provider_defaults(provider: &str) -> (String, String) {
    match provider {
        "openai" => ("https://api.openai.com/v1".to_string(), "gpt-5.5".to_string()),
        // ChatGPT-subscription (OAuth) — no base URL / API key; model is a Codex slug.
        "chatgpt" => (String::new(), "gpt-5.1-codex".to_string()),
        "anthropic" => (String::new(), "claude-opus-4-8".to_string()),
        // Anthropic-Messages-compatible gateway — user supplies base_url + model.
        "anthropic-compatible" => (String::new(), String::new()),
        // Native Gemini adapter — no base URL (it has its own endpoint), like Anthropic.
        "gemini" => (String::new(), "gemini-3.5-flash".to_string()),
        "openrouter" => (
            "https://openrouter.ai/api/v1".to_string(),
            "anthropic/claude-opus-4-8".to_string(),
        ),
        // openai-compatible: no sensible model default — the user picks from the
        // endpoint's fetched list or types one.
        _ => ("http://localhost:11434/v1".to_string(), String::new()),
    }
}

fn provider_context_defaults(provider: &str) -> (u64, u8) {
    match provider {
        "openai" | "chatgpt" | "anthropic" | "gemini" => (250_000, 90),
        "openai-compatible" | "anthropic-compatible" => (130_000, 90),
        // Keep openrouter aligned with the hosted-provider defaults unless the
        // user overrides it per profile.
        "openrouter" => (250_000, 90),
        _ => (130_000, 90),
    }
}

/// The model candidates shown inline in the login form. The list is filtered by
/// the current model text, with prefix matches ranked first and the active value
/// pinned into the results so arrowing through suggestions stays stable.
fn login_model_rows(app: &App) -> Vec<String> {
    let all: Vec<String> = match &app.form_fetched_models {
        Some(fetched) => fetched.clone(),
        None => get_provider_models(&app.form_provider)
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let query = app.form_model.trim().to_ascii_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for model in all {
        let lower = model.to_ascii_lowercase();
        let matches = query.is_empty()
            || lower.starts_with(&query)
            || lower.contains(&query);
        if !matches || !seen.insert(model.clone()) {
            continue;
        }
        if !query.is_empty() && lower.starts_with(&query) {
            prefix.push(model);
        } else {
            contains.push(model);
        }
    }

    let current = app.form_model.trim();
    if !current.is_empty() && seen.insert(current.to_string()) {
        prefix.insert(0, current.to_string());
    }

    prefix.extend(contains);
    prefix.truncate(6);
    prefix
}

struct App {
    options: TuiOptions,
    input: String,
    /// Cursor position in `input`, as a CHAR index (0..=char_count).
    input_cursor: usize,
    /// Submitted inputs (oldest first) for Up/Down history recall.
    input_history: Vec<String>,
    /// Position while navigating history; None = editing the live draft.
    history_pos: Option<usize>,
    /// The in-progress input saved when history navigation begins.
    history_draft: String,
    /// Collapsed pastes: (placeholder shown in the input, real content). A big
    /// paste shows as a compact chip and expands back on send.
    pasted_blocks: Vec<(String, String)>,
    /// Pending file attachments (images/files) dropped or pasted — kept OUT of the
    /// input text. Shown as a compact "📎 N attachments" line above the prompt and
    /// appended to the message on send. Tuple: (is_image, absolute_path, filename).
    attachments: Vec<(bool, String, String)>,
    status: String,
    /// Set by the startup self-update task to the version it installed; shown in
    /// the header as a "restart to apply" hint.
    update_notice: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    error: Option<String>,
    state: Option<HarnessState>,
    /// The resident conversation loop. Spawned once, lives across turns.
    agent: Option<JoinHandle<Result<HarnessState, String>>>,
    /// Channel to steer / answer / interrupt the resident loop.
    input_tx: Option<UnboundedSender<LoopInput>>,
    /// Transcript scrollback offset, in rendered lines from the bottom
    /// (0 = follow the tail).
    scroll: usize,
    /// Largest valid scroll offset, recomputed each frame from the rendered
    /// transcript so key handlers can clamp.
    max_scroll: Cell<usize>,
    /// Home-abbreviated, canonicalized workspace path for the status bar.
    cwd_display: String,
    /// Animation frame counter for the "working…" spinner.
    frame: usize,
    quit: bool,
    active_conversation: String,
    active_state_path: PathBuf,
    suggestion_index: usize,
    screen: Screen,
    resume_selected_index: usize,
    /// In the resume picker: a `d` was pressed once; a second `d` confirms delete.
    resume_pending_delete: bool,
    /// While renaming a saved session in the resume picker: the in-progress title
    /// buffer (None = not renaming).
    resume_rename: Option<String>,
    /// Theme picker cursor + the index to restore if the picker is cancelled.
    theme_selected_index: usize,
    theme_original_index: usize,
    /// Inline model suggestion cursor in the login form.
    model_picker_index: usize,
    /// Profiles screen cursor; which profile (if any) the editor is editing; and
    /// whether closing the editor should return to the profiles list.
    profiles_selected_index: usize,
    editing_profile: Option<String>,
    return_to_profiles: bool,
    /// Hold the compaction animation until this instant (time-based, so a fast
    /// render loop during streaming doesn't burn through it).
    compaction_anim_until: Option<std::time::Instant>,
    seen_compactions: usize,
    /// Inputs typed while the agent is executing — held and submitted when the run
    /// finishes (or is stopped), instead of steering mid-run.
    queued_inputs: std::collections::VecDeque<String>,
    /// Was the agent busy on the previous tick? Drives the queue flush on the
    /// busy → not-busy edge.
    was_busy: bool,
    /// Set the instant we hand the loop a new turn (send a message / spawn) and
    /// cleared once the persisted state catches up. `self.state` is read from the
    /// mtime-gated state file, so it lags the live loop: without this, a message
    /// sent in that window sees a stale `Idle`, gets delivered mid-run, and the
    /// harness folds it into a `[steer]` instead of a new turn — it "disappears".
    /// Treating the agent as busy here makes the follow-up queue instead.
    sent_turn_pending: bool,
    /// The (provider, model) actually driving THIS chat — the per-chat profile
    /// override when set, else the global default. Cached because resolving it
    /// reads the profile sidecar from disk; refreshed on session switch / profile
    /// change so the header and rate-limit gate never show the wrong model.
    effective_model: (String, String),
    /// Snapshot of the session list while the resume picker is open. Rebuilding it
    /// re-reads + deserializes EVERY session file; doing that per keystroke/frame
    /// made the picker laggy at scale. Populated on picker open, refreshed after
    /// rename/delete, dropped on close.
    conv_cache: Option<Vec<(String, String)>>,
    /// Account-wide ChatGPT usage (read from the shared sidecar), shown globally
    /// whenever signed in — not tied to the active chat's own last request.
    global_usage: Option<crate::llm::RateLimitSnapshot>,
    form_provider: String,
    form_api_key: String,
    form_model: String,
    form_model_query: String,
    form_base_url: String,
    form_reasoning_effort: Option<String>,
    form_context_window: String,
    form_compact_at_pct: String,
    form_focus: SettingsField,
    form_fetched_models: Option<Vec<String>>,
    models_fetch_handle: Option<tokio::task::JoinHandle<Result<Vec<String>, String>>>,
    /// In-flight ChatGPT sign-in task (browser OAuth or device-code flow), polled in `tick`).
    chatgpt_login_handle:
        Option<tokio::task::JoinHandle<Result<crate::chatgpt_auth::ChatGptTokens, String>>>,
    /// Current device-code prompt, shown while the poll task is running.
    chatgpt_device_code: Option<crate::chatgpt_auth::DeviceCodeInfo>,
    /// In-flight device-code *begin* task (fetches the code to display), polled in `tick`.
    chatgpt_device_begin_handle:
        Option<tokio::task::JoinHandle<Result<crate::chatgpt_auth::DeviceCodeInfo, String>>>,
    models_fetch_status: String,
    original_config: Option<crate::config::SnippetConfig>,
    last_state_modified: Option<std::time::SystemTime>,
    /// When true, the compact inline model-connect form is shown and owns key
    /// input. It edits the shared `form_*` state.
    login_active: bool,
    /// Interactive ask_user picker state. `q_index` is the question being
    /// answered (questions are answered in order), `q_sel` the highlighted choice
    /// for the current question, `q_answers` the (question_text, answer) pairs
    /// collected so far, and `q_token` a fingerprint of the current question set
    /// used to detect a fresh ask and reset the cursor.
    q_index: usize,
    q_sel: usize,
    q_answers: Vec<(String, String)>,
    q_token: String,
    /// Live text the running agent is streaming this turn. Shared with the agent
    /// task; rendered as a transient block at the transcript tail and cleared
    /// whenever a newer persisted state loads (the turn has committed).
    stream: crate::llm::StreamHandle,
}

impl App {
    fn new(options: TuiOptions) -> Self {
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
            attachments: Vec::new(),
            status,
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
            theme_selected_index: 0,
            theme_original_index: 0,
            model_picker_index: 0,
            profiles_selected_index: 0,
            editing_profile: None,
            return_to_profiles: false,
            compaction_anim_until: None,
            seen_compactions: usize::MAX, // uninitialized; first tick seeds it, no flash

            queued_inputs: std::collections::VecDeque::new(),
            sent_turn_pending: false,
            effective_model: (String::new(), String::new()),
            conv_cache: None,
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
            form_focus: SettingsField::Provider,
            form_fetched_models: None,
            models_fetch_handle: None,
            chatgpt_login_handle: None,
            chatgpt_device_code: None,
            chatgpt_device_begin_handle: None,
            models_fetch_status: String::new(),
            update_notice: std::sync::Arc::new(std::sync::Mutex::new(None)),
            original_config: None,
            last_state_modified: None,
            login_active: false,
            q_index: 0,
            q_sel: 0,
            q_answers: Vec::new(),
            q_token: String::new(),
            stream: crate::llm::StreamHandle::default(),
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

        app
    }

    /// Open the profiles screen (the model page). Migrates a lone `[model]` into a
    /// named profile so everything is managed uniformly, then selects the active one.
    fn open_profiles(&mut self) {
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
    fn open_profile_editor(&mut self, name: Option<String>) {
        self.original_config = Some(self.options.config.clone());
        let existing = name
            .as_ref()
            .and_then(|n| self.options.config.setups.as_ref().and_then(|m| m.get(n)).cloned());
        match existing {
            Some(cfg) => {
                self.form_provider = cfg.provider.clone();
                self.form_api_key = cfg.api_key.clone();
                self.form_model = cfg.model.clone();
                self.form_base_url = cfg.base_url.clone();
                self.form_reasoning_effort = cfg.reasoning_effort.clone();
                self.form_context_window = cfg.context_window.to_string();
                self.form_compact_at_pct = cfg.compact_at_pct.to_string();
            }
            None => {
                self.form_provider = "openai".to_string();
                let (base, model) = provider_defaults(&self.form_provider);
                self.form_base_url = base;
                self.form_model = model;
                self.form_api_key = String::new();
                self.form_reasoning_effort = Some("medium".to_string());
                let (context_window, compact_at_pct) = provider_context_defaults(&self.form_provider);
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
    fn effective_config(&self) -> SnippetConfig {
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
    fn refresh_effective_model(&mut self) {
        let cfg = self.effective_config();
        self.effective_model = (cfg.model.provider.clone(), cfg.model.model.clone());
    }

    /// Set a profile as the GLOBAL default (the model new chats use). A chat that
    /// has its own per-chat override keeps it (override wins), so we only restart
    /// the current loop when it has no override.
    fn activate_profile(&mut self, name: &str) {
        // Switching the model restarts the loop — never kill a mid-turn run (a
        // /goal or lane could be working). Same guard /model uses.
        if self.agent_busy() {
            self.status = "agent is working — stop it (Esc) before switching models".to_string();
            return;
        }
        if self.options.config.activate(name) {
            let _ = self.save_config_file();
            let has_override = crate::session::read_session_profile(&self.active_state_path).is_some();
            let resumed = if has_override { false } else { self.restart_loop_for_config() };
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

    /// Set a profile for THIS chat only (a persisted per-conversation override),
    /// without changing the global default. Restarts the chat's loop with it.
    fn activate_profile_local(&mut self, name: &str) {
        if self.agent_busy() {
            self.status = "agent is working — stop it (Esc) before switching models".to_string();
            return;
        }
        let Some(model) = self.options.config.setups.as_ref().and_then(|m| m.get(name)).cloned() else {
            self.status = format!("profile `{name}` not found");
            return;
        };
        crate::session::write_session_profile(&self.active_state_path, name);
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
    fn close_login(&mut self, restore: bool) {
        if restore {
            if let Some(orig) = self.original_config.take() {
                self.options.config = orig;
            }
        } else {
            self.original_config = None;
        }
        self.login_active = false;
        // When opened from the profiles screen, return there (refreshed) rather than
        // dropping to the transcript.
        if self.return_to_profiles {
            self.return_to_profiles = false;
            self.editing_profile = None;
            self.open_profiles();
        }
        // When closing login during an active session, the conversation is preserved.
    }

    fn conversations_dir(&self) -> PathBuf {
        let parent = self.options.config.state_path.parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let dir = parent.join("conversations");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn find_last_active_conversation(&self) -> Option<String> {
        let dir = self.conversations_dir();
        let mut best_path: Option<PathBuf> = None;
        let mut best_time = std::time::SystemTime::UNIX_EPOCH;

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_conversation_json(&path) {
                    if let Ok(metadata) = std::fs::metadata(&path) {
                        if let Ok(modified) = metadata.modified() {
                            if modified > best_time {
                                best_time = modified;
                                best_path = Some(path);
                            }
                        }
                    }
                }
            }
        }

        // Also check default state_path if it exists
        let default_path = &self.options.config.state_path;
        if default_path.exists() {
            if let Ok(metadata) = std::fs::metadata(default_path) {
                if let Ok(modified) = metadata.modified() {
                    if modified > best_time {
                        best_path = Some(default_path.clone());
                    }
                }
            }
        }

        best_path.and_then(|p| {
            if p == *default_path {
                Some("default".to_string())
            } else {
                p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
            }
        })
    }

    /// Delete a saved conversation file + its metadata sidecar (resume picker `d`).
    fn delete_conversation(&self, name: &str) {
        let path = self.conversations_dir().join(format!("{name}.json"));
        crate::session::remove_session_files(&path);
    }

    /// Set a saved conversation's title override (resume picker `r`).
    fn rename_conversation(&self, name: &str, title: &str) {
        let path = self.conversations_dir().join(format!("{name}.json"));
        let _ = crate::session::set_session_title(&path, title);
    }

    fn list_conversations(&self) -> Vec<(String, String)> {
        let dir = self.conversations_dir();
        let mut list = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_conversation_json(&path) {
                    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                    if name.is_empty() || name == "default" {
                        continue;
                    }

                    let mut desc = "empty session".to_string();
                    let mut mod_time = std::time::SystemTime::UNIX_EPOCH;

                    if let Ok(metadata) = std::fs::metadata(&path) {
                        if let Ok(m) = metadata.modified() {
                            mod_time = m;
                        }
                    }

                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(state) = crate::harness::deserialize_state(&bytes) {
                            if let Some(t) = state.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                                desc = t.to_string();
                            } else if !state.user_request.is_empty() {
                                desc = state.user_request.clone();
                            }
                        }
                    }

                    let duration = std::time::SystemTime::now().duration_since(mod_time).unwrap_or_default();
                    let relative = if duration.as_secs() < 60 {
                        "just now".to_string()
                    } else if duration.as_secs() < 3600 {
                        format!("{}m ago", duration.as_secs() / 60)
                    } else if duration.as_secs() < 86400 {
                        format!("{}h ago", duration.as_secs() / 3600)
                    } else {
                        format!("{}d ago", duration.as_secs() / 86400)
                    };

                    list.push((name, desc, mod_time, relative));
                }
            }
        }

        let default_path = &self.options.config.state_path;
        if default_path.exists() {
            let mut desc = "default session".to_string();
            let mut mod_time = std::time::SystemTime::UNIX_EPOCH;
            if let Ok(metadata) = std::fs::metadata(default_path) {
                if let Ok(m) = metadata.modified() {
                    mod_time = m;
                }
            }
            // Skip a contentless default state — a fresh install otherwise shows a
            // phantom "default session" entry with nothing to resume into.
            let mut has_content = false;
            if let Ok(bytes) = std::fs::read(default_path) {
                if let Ok(state) = crate::harness::deserialize_state(&bytes) {
                    has_content = !state.user_request.is_empty() || !state.events.is_empty();
                    if !state.user_request.is_empty() {
                        desc = state.user_request.clone();
                    }
                }
            }
            if has_content {
                let duration = std::time::SystemTime::now().duration_since(mod_time).unwrap_or_default();
                let relative = if duration.as_secs() < 60 {
                    "just now".to_string()
                } else if duration.as_secs() < 3600 {
                    format!("{}m ago", duration.as_secs() / 60)
                } else if duration.as_secs() < 86400 {
                    format!("{}h ago", duration.as_secs() / 3600)
                } else {
                    format!("{}d ago", duration.as_secs() / 86400)
                };
                list.push(("default".to_string(), desc, mod_time, relative));
            }
        }

        list.sort_by(|a, b| b.2.cmp(&a.2));

        list.into_iter()
            .map(|(name, desc, _, relative)| {
                let short_desc = if desc.chars().count() > 40 {
                    format!("{}...", desc.chars().take(37).collect::<String>())
                } else {
                    desc
                };
                (name, format!("({}) — {}", relative, short_desc))
            })
            .collect()
    }

    /// Restore the workspace to the checkpoint whose id starts with `id_prefix`.
    fn rewind_to(&mut self, id_prefix: &str) {
        if self.agent_busy() {
            self.status = "Agent is working — stop it (Esc) before rewinding.".to_string();
            return;
        }
        let Some(state) = &self.state else {
            self.status = "No active session.".to_string();
            return;
        };
        let Some(checkpoint) = state
            .checkpoints
            .iter()
            .rev()
            .find(|c| c.id.starts_with(id_prefix))
        else {
            self.status = format!("No checkpoint matching '{id_prefix}'.");
            return;
        };
        let (id, label) = (checkpoint.id.clone(), checkpoint.label.clone());
        let workspace = self.options.config.workspace.clone();
        match crate::checkpoint::restore(&workspace, &id) {
            Ok(()) => self.status = format!("Rewound workspace to: {label}"),
            Err(error) => self.error = Some(format!("rewind failed: {error}")),
        }
    }

    fn switch_conversation(&mut self, name: &str) {
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

        // Update active_conversation and active_state_path
        self.active_conversation = name.to_string();
        if name == "default" {
            self.active_state_path = self.options.config.state_path.clone();
        } else {
            self.active_state_path = self.conversations_dir().join(format!("{}.json", name));
        }

        // Force a fresh state read for the new session's file.
        self.last_state_modified = None;
        self.state = None;
        self.scroll = 0;
        self.status = String::new();
        // Drop anything held for the PREVIOUS conversation: queued messages must
        // never fire into the newly-switched session, stale busy flags must not
        // trigger a phantom flush, and a lingering error must not mask status.
        self.queued_inputs.clear();
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
    }

    fn handle_slash_command(&mut self, text: &str) {
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
                    if self.list_conversations().iter().any(|(existing, _)| existing == &n) {
                        self.status =
                            format!("`{n}` already exists — /resume {n} to open it, or pick another name");
                        return;
                    }
                    n
                } else {
                    uuid::Uuid::new_v4().to_string()
                };
                self.switch_conversation(&name);
                self.status = String::new();
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
                        if !self.list_conversations().iter().any(|(existing, _)| existing == &name) {
                            self.status =
                                format!("no session named `{name}` — bare /resume opens the picker");
                            return;
                        }
                        self.switch_conversation(&name)
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
                            self.status = "↑↓ select · Enter resume · r rename · dd delete · Esc cancel".to_string();
                            return;
                        }
                        // Nothing saved: resume the current session if one exists.
                        if self.agent_alive() {
                            self.status = "Agent is already running.".to_string();
                            return;
                        }
                        if !self.active_state_path.exists() {
                            if let Some(last_active) = self.find_last_active_conversation() {
                                self.switch_conversation(&last_active);
                            }
                        }
                    }
                }

                if self.active_state_path.exists() {
                    self.spawn_loop(None, true);
                } else {
                    self.status = "No saved session to resume. Start a new one with /new or type a task.".to_string();
                }
            }
            "/model" => {
                if self.agent_busy() {
                    self.status = "Agent is working. Stop it (Esc) before changing the model.".to_string();
                    return;
                }
                self.open_profiles();
                self.status = String::new();
            }
            "/rewind" => {
                if parts.len() > 1 {
                    self.rewind_to(parts[1]);
                } else {
                    let n = self.state.as_ref().map(|s| s.checkpoints.len()).unwrap_or(0);
                    self.status = if n == 0 {
                        "No checkpoints yet — one is taken before each request.".to_string()
                    } else {
                        format!("{n} checkpoint(s). Type /rewind <id> (Tab to pick) to restore.")
                    };
                }
            }
            "/mode" => {
                let manual = !self.options.config.manual_approval;
                self.options.config.manual_approval = manual;
                let _ = self.save_config_file();
                if let Some(tx) = &self.input_tx {
                    let mode = if manual {
                        crate::harness::ApprovalMode::Manual
                    } else {
                        crate::harness::ApprovalMode::Auto
                    };
                    let _ = tx.send(LoopInput::SetMode(mode));
                }
                self.status = String::new();
            }
            "/compact" => {
                if let Some(tx) = &self.input_tx {
                    if tx.send(LoopInput::Compact).is_ok() {
                        if let Ok(mut stream) = self.stream.lock() {
                            stream.text = "Compacting history…".to_string();
                            stream.thinking.clear();
                        }
                        self.status = String::new();
                    } else {
                        self.status = "Failed to send compact request to the agent loop.".to_string();
                    }
                } else {
                    self.status = "No active session to compact.".to_string();
                }
            }
            "/goal" => {
                let rest = text.strip_prefix("/goal").unwrap_or("").trim();
                if rest.eq_ignore_ascii_case("cancel") || rest.eq_ignore_ascii_case("stop") {
                    match &self.input_tx {
                        Some(tx) => {
                            let _ = tx.send(LoopInput::CancelGoal);
                            self.status = "Cancelling the goal…".to_string();
                        }
                        None => self.status = "No active goal.".to_string(),
                    }
                } else if rest.is_empty() {
                    self.status =
                        "Usage: /goal <what to accomplish>   ·   /goal cancel".to_string();
                } else {
                    // The agent must be running to receive the goal; start it if idle.
                    if !self.agent_alive() {
                        self.spawn_loop(None, true);
                    }
                    match self.input_tx.clone() {
                        Some(tx) if tx.send(LoopInput::SetGoal(rest.to_string())).is_ok() => {
                            self.status = "Goal set — the agent will drive toward it. /goal cancel to stop.".to_string();
                        }
                        _ => self.status = "Couldn't start the agent loop for the goal.".to_string(),
                    }
                }
            }
            "/theme" => {
                if parts.len() > 1 {
                    if set_theme_by_name(parts[1]) {
                        self.persist_theme();
                        self.status = format!("Theme: {}", parts[1]);
                    } else {
                        let names = PRESETS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ");
                        self.status = format!("Unknown theme '{}'. Available: {names}", parts[1]);
                    }
                } else {
                    self.theme_original_index = current_theme_index();
                    self.theme_selected_index = current_theme_index();
                    self.screen = Screen::ThemeSelection;
                    self.status = String::new();
                }
            }
            other => {
                self.status =
                    format!("Unknown command: {other}. Type /new, /resume, /rewind, /model, or /theme.");
            }
        }
    }

    /// Persist the active theme's name to config so it survives restarts.
    fn persist_theme(&mut self) {
        self.options.config.theme = PRESETS.get(current_theme_index()).map(|p| p.name.to_string());
        let _ = self.save_config_file();
    }

    /// Tab order of the login form fields (Base URL only for openai-compatible).
    fn login_focus_order(&self) -> Vec<SettingsField> {
        // ChatGPT-subscription signs in via OAuth — no API key / base URL fields.
        if self.form_provider == "chatgpt" {
            return vec![
                SettingsField::Provider,
                SettingsField::Model,
                SettingsField::Reasoning,
                SettingsField::ContextWindow,
                SettingsField::Compaction,
            ];
        }
        let mut order = vec![SettingsField::Provider, SettingsField::ApiKey];
        if provider_needs_base_url(&self.form_provider) {
            order.push(SettingsField::BaseUrl);
        }
        order.push(SettingsField::Model);
        order.push(SettingsField::Reasoning);
        order.push(SettingsField::ContextWindow);
        order.push(SettingsField::Compaction);
        order
    }

    /// Move focus between login fields. Lazily fetches the model list the first
    /// time focus lands on the Model field with a key present.
    fn login_move_focus(&mut self, forward: bool) {
        let order = self.login_focus_order();
        let cur = order.iter().position(|f| *f == self.form_focus).unwrap_or(0);
        let next = if forward {
            (cur + 1) % order.len()
        } else if cur == 0 {
            order.len() - 1
        } else {
            cur - 1
        };
        self.form_focus = order[next];

        // Keyless endpoints are legitimate for openai-compatible (local Ollama,
        // LM Studio…): fetch on a base_url alone there; other providers need a key.
        let can_fetch = !self.form_api_key.trim().is_empty()
            || (provider_needs_base_url(&self.form_provider) && !self.form_base_url.trim().is_empty());
        if self.form_focus == SettingsField::Model
            && self.form_fetched_models.is_none()
            && can_fetch
        {
            self.trigger_models_fetch();
        }
    }

    /// `←`/`→` on the focused field: cycle provider or model.
    fn login_adjust(&mut self, forward: bool) {
        match self.form_focus {
            SettingsField::Provider => self.change_provider(forward),
            SettingsField::Model => self.login_cycle_model(forward),
            SettingsField::Reasoning => self.login_cycle_reasoning(forward),
            SettingsField::Compaction => self.login_cycle_compaction_pct(forward),
            _ => {}
        }
    }


    fn login_cycle_reasoning(&mut self, forward: bool) {
        const OPTIONS: [&str; 5] = ["off", "low", "medium", "high", "xhigh"];
        let current = self
            .form_reasoning_effort
            .as_deref()
            .unwrap_or("medium")
            .to_ascii_lowercase();
        let idx = OPTIONS.iter().position(|v| *v == current).unwrap_or(2);
        let next = if forward {
            (idx + 1) % OPTIONS.len()
        } else if idx == 0 {
            OPTIONS.len() - 1
        } else {
            idx - 1
        };
        self.form_reasoning_effort = Some(OPTIONS[next].to_string());
    }

    fn login_cycle_compaction_pct(&mut self, forward: bool) {
        let current = self
            .form_compact_at_pct
            .trim()
            .parse::<u8>()
            .ok()
            .unwrap_or(85)
            .clamp(50, 95);
        let next = if forward {
            current.saturating_add(5).min(95)
        } else {
            current.saturating_sub(5).max(50)
        };
        self.form_compact_at_pct = next.to_string();
    }

    /// Cycle the chosen model through the full candidate list (fetched or
    /// fallback), independent of any typed text.
    fn login_cycle_model(&mut self, forward: bool) {
        let all: Vec<String> = match &self.form_fetched_models {
            Some(fetched) => fetched.clone(),
            None => get_provider_models(&self.form_provider)
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        if all.is_empty() {
            return;
        }
        let next = match all.iter().position(|m| *m == self.form_model) {
            Some(i) if forward => (i + 1) % all.len(),
            Some(0) => all.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.form_model = all[next].clone();
        self.model_picker_index = 0;
    }

    /// Type a character into the focused text field.
    fn login_edit_char(&mut self, c: char) {
        match self.form_focus {
            SettingsField::ApiKey => self.form_api_key.push(c),
            SettingsField::BaseUrl => self.form_base_url.push(c),
            SettingsField::Model => {
                self.form_model.push(c);
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => {
                if c.is_ascii_digit() {
                    self.form_context_window.push(c);
                }
            }
            SettingsField::Compaction => {
                if c.is_ascii_digit() {
                    self.form_compact_at_pct.push(c);
                }
            }
            _ => {}
        }
    }

    /// Paste into the focused login-form field. Fields are single-line, so pasted
    /// newlines and edge whitespace are stripped (a pasted key/model/URL often has
    /// a trailing newline).
    fn login_paste(&mut self, text: &str) {
        let cleaned = text.replace(['\n', '\r'], "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            return;
        }
        match self.form_focus {
            SettingsField::ApiKey => self.form_api_key.push_str(cleaned),
            SettingsField::BaseUrl => self.form_base_url.push_str(cleaned),
            SettingsField::Model => {
                self.form_model.push_str(cleaned);
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => self
                .form_context_window
                .push_str(&cleaned.chars().filter(|c| c.is_ascii_digit()).collect::<String>()),
            SettingsField::Compaction => self
                .form_compact_at_pct
                .push_str(&cleaned.chars().filter(|c| c.is_ascii_digit()).collect::<String>()),
            _ => {}
        }
    }

    fn login_backspace(&mut self) {
        match self.form_focus {
            SettingsField::ApiKey => {
                self.form_api_key.pop();
            }
            SettingsField::BaseUrl => {
                self.form_base_url.pop();
            }
            SettingsField::Model => {
                self.form_model.pop();
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => {
                self.form_context_window.pop();
            }
            SettingsField::Compaction => {
                self.form_compact_at_pct.pop();
            }
            _ => {}
        }
    }

    /// Validate the form and connect: persist the config and close the form.
    fn login_connect(&mut self) {
        // ChatGPT-subscription has no API key — it signs in via OAuth (or reuses an
        // existing sign-in) instead of validating a key.
        if self.form_provider == "chatgpt" {
            self.start_chatgpt_login(crate::chatgpt_auth::ChatGptLoginMethod::Browser);
            return;
        }
        if self.form_api_key.trim().is_empty() {
            self.form_focus = SettingsField::ApiKey;
            self.status = "An API key is required to connect.".to_string();
            return;
        }
        if self.form_model.trim().is_empty() {
            self.form_focus = SettingsField::Model;
            self.status = "Pick or type a model to connect.".to_string();
            return;
        }
        let context_window = self
            .form_context_window
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|v| *v >= 8_000)
            .unwrap_or_else(|| provider_context_defaults(&self.form_provider).0);
        let compact_at_pct = self
            .form_compact_at_pct
            .trim()
            .parse::<u8>()
            .ok()
            .unwrap_or_else(|| provider_context_defaults(&self.form_provider).1)
            .clamp(50, 95);
        match self.save_settings_to_file_with_limits(context_window, compact_at_pct) {
            Ok(_) => {
                self.close_login(false);
                // The resident loop builds its model once at spawn, so a live
                // (idle) loop won't pick up the change until it's restarted; it
                // resumes the persisted conversation, so nothing is lost.
                let resumed = self.restart_loop_for_config();
                self.status = format!(
                    "✓ Connected — {} · {}{}",
                    self.options.config.model.provider,
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Begin a ChatGPT sign-in flow. Reuses an existing sign-in if there
    /// is one; otherwise starts either the browser OAuth flow or the device-code flow.
    fn start_chatgpt_login(&mut self, method: crate::chatgpt_auth::ChatGptLoginMethod) {
        if self.chatgpt_login_handle.is_some() {
            self.status = match method {
                crate::chatgpt_auth::ChatGptLoginMethod::Browser => {
                    "Sign-in already in progress — finish it in your browser.".to_string()
                }
                crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode => {
                    "Sign-in already in progress — finish the device-code flow first.".to_string()
                }
            };
            return;
        }
        if crate::chatgpt_auth::is_signed_in() {
            self.finish_chatgpt_login(None);
            return;
        }
        self.chatgpt_device_code = None;
        match method {
            crate::chatgpt_auth::ChatGptLoginMethod::Browser => {
                self.status = "Opening browser — finish signing in to ChatGPT…".to_string();
                self.chatgpt_login_handle = Some(tokio::spawn(async move {
                    crate::chatgpt_auth::login(crate::chatgpt_auth::ChatGptLoginMethod::Browser)
                        .await
                }));
            }
            crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode => {
                // Fetch the device code off-thread (NEVER block_on inside the async
                // runtime — that panics). tick() surfaces the code and starts polling.
                self.status = "Starting device-code sign-in…".to_string();
                self.chatgpt_device_begin_handle =
                    Some(tokio::spawn(
                        async move { crate::chatgpt_auth::begin_device_code_login().await },
                    ));
            }
        }
    }

    /// Persist the chatgpt provider + model once a sign-in is in place and connect.
    fn finish_chatgpt_login(&mut self, email: Option<String>) {
        if self.form_model.trim().is_empty() {
            self.form_model = "gpt-5.1-codex".to_string();
        }
        self.form_api_key = String::new();
        self.form_base_url = String::new();
        self.chatgpt_device_code = None;
        match self.save_settings_to_file() {
            Ok(_) => {
                self.close_login(false);
                let resumed = self.restart_loop_for_config();
                let who = email.map(|e| format!(" as {e}")).unwrap_or_default();
                self.status = format!(
                    "✓ Signed in to ChatGPT{who} — {}{}",
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn logout_chatgpt(&mut self) {
        match crate::chatgpt_auth::logout_blocking() {
            Ok(()) => {
                self.chatgpt_device_code = None;
                if self.form_provider == "chatgpt" {
                    self.form_api_key.clear();
                    self.form_base_url.clear();
                }
                if self.options.config.model.provider == "chatgpt" {
                    let name = self.options.config.active_setup.clone();
                    if let Some(name) = name {
                        if let Some(setups) = self.options.config.setups.as_mut() {
                            if let Some(cfg) = setups.get_mut(&name) {
                                cfg.api_key.clear();
                            }
                        }
                    }
                    let _ = self.save_config_file();
                }
                self.status = "Signed out of ChatGPT.".to_string();
            }
            Err(error) => {
                self.error = Some(format!("ChatGPT logout failed: {error}"));
            }
        }
    }

    /// Apply a config change to the resident loop by restarting it. A live loop is
    /// aborted (it's idle, waiting for input — guards ensure it isn't mid-turn) and
    /// respawned with `resume`, continuing the conversation with the new model.
    /// Returns `true` if a loop was actually restarted. No-op when none is running —
    /// the next `spawn_loop` already uses the new config.
    fn restart_loop_for_config(&mut self) -> bool {
        if !self.agent_alive() {
            return false;
        }
        if let Some(handle) = self.agent.take() {
            handle.abort();
        }
        self.input_tx = None;
        self.spawn_loop(None, true);
        true
    }

    fn init_settings_form(&mut self) {
        self.form_provider = self.options.config.model.provider.clone();
        self.form_api_key = self.options.config.model.api_key.clone();
        self.form_model = self.options.config.model.model.clone();
        self.form_model_query = String::new();
        self.form_base_url = self.options.config.model.base_url.clone();
        self.form_reasoning_effort = self.options.config.model.reasoning_effort.clone().or(Some("medium".to_string()));
        self.form_context_window = self.options.config.model.context_window.to_string();
        self.form_compact_at_pct = self.options.config.model.compact_at_pct.to_string();
        self.form_focus = SettingsField::Provider;
        self.form_fetched_models = None;
        self.models_fetch_status = String::new();
    }

    fn trigger_models_fetch(&mut self) {
        if let Some(ref handle) = self.models_fetch_handle {
            handle.abort();
        }
        self.form_fetched_models = None;
        self.models_fetch_status = "Fetching available models from provider...".to_string();
        
        let provider = self.form_provider.clone();
        let api_key = self.form_api_key.clone();
        let base_url = self.form_base_url.clone();

        self.models_fetch_handle = Some(tokio::spawn(async move {
            fetch_models_from_provider(provider, api_key, base_url).await
        }));
    }

    fn save_config_file(&self) -> Result<(), String> {
        let toml_str = toml::to_string_pretty(&self.options.config)
            .map_err(|e| format!("failed to serialize config: {e}"))?;

        // Never overwrite the config with something we can't read back — guards
        // against a serialization ordering bug silently corrupting the user's file.
        toml::from_str::<crate::config::SnippetConfig>(&toml_str)
            .map_err(|e| format!("refusing to write config that won't round-trip: {e}"))?;

        std::fs::write(&self.options.config_path, toml_str)
            .map_err(|e| format!("failed to write config: {e}"))?;
        crate::config::set_private(&self.options.config_path);
        Ok(())
    }

    fn save_settings_to_file(&mut self) -> Result<(), String> {
        let context_window = provider_context_defaults(&self.form_provider).0;
        let compact_at_pct = provider_context_defaults(&self.form_provider).1;
        self.save_settings_to_file_with_limits(context_window, compact_at_pct)
    }

    fn save_settings_to_file_with_limits(
        &mut self,
        context_window: u64,
        compact_at_pct: u8,
    ) -> Result<(), String> {
        // Start from the profile being edited (preserving its other fields like
        // temperature/reasoning) or the active config when adding a new one.
        let mut model_config = self
            .editing_profile
            .as_ref()
            .and_then(|n| self.options.config.setups.as_ref().and_then(|m| m.get(n)).cloned())
            .unwrap_or_else(|| self.options.config.model.clone());

        model_config.provider = self.form_provider.clone();
        model_config.api_key = self.form_api_key.clone();
        model_config.model = self.form_model.clone();
        model_config.base_url = self.form_base_url.clone();
        model_config.reasoning_effort = self
            .form_reasoning_effort
            .clone()
            .filter(|v| !v.trim().is_empty());

        model_config.context_window = context_window;
        model_config.compact_at_pct = compact_at_pct;

        // Write into the named profile (editing the same key, or a fresh unique one)
        // and make it active.
        let key = self
            .editing_profile
            .clone()
            .unwrap_or_else(|| self.options.config.unique_profile_key(&self.form_provider));
        self.options.config.upsert_profile(&key, model_config);
        self.options.config.activate(&key);
        self.editing_profile = Some(key);

        self.save_config_file()?;

        self.status = String::new();
        Ok(())
    }

    fn change_provider(&mut self, next: bool) {
        let current_idx = LOGIN_PROVIDERS
            .iter()
            .position(|p| *p == self.form_provider)
            .unwrap_or(0);
        let next_idx = if next {
            (current_idx + 1) % LOGIN_PROVIDERS.len()
        } else if current_idx == 0 {
            LOGIN_PROVIDERS.len() - 1
        } else {
            current_idx - 1
        };
        self.form_provider = LOGIN_PROVIDERS[next_idx].to_string();

        let (base_url, model) = provider_defaults(&self.form_provider);
        self.form_base_url = base_url;
        self.form_model = model;
        let (context_window, compact_at_pct) = provider_context_defaults(&self.form_provider);
        self.form_context_window = context_window.to_string();
        self.form_compact_at_pct = compact_at_pct.to_string();
        // Keep the user's current reasoning preference if present; otherwise default to medium.
        if self.form_reasoning_effort.is_none() {
            self.form_reasoning_effort = Some("medium".to_string());
        }
        self.form_model_query = String::new();
        // The previous provider's model list no longer applies.
        self.form_fetched_models = None;
    }

    fn agent_alive(&self) -> bool {
        self.agent
            .as_ref()
            .map(|handle| !handle.is_finished())
            .unwrap_or(false)
    }

    /// `true` only while the agent is actively processing a turn (or a lane is).
    /// The resident loop stays *alive* between turns waiting for input, so
    /// `agent_alive()` is the wrong test for "is it safe to act now" — use this for
    /// guards like `/model` and `/rewind` so they aren't blocked when merely idle.
    fn agent_busy(&self) -> bool {
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
    fn is_compacting(&self) -> bool {
        if self.compaction_anim_until.is_some_and(|t| std::time::Instant::now() < t) {
            return true;
        }
        self.state.as_ref().is_some_and(|s| {
            s.status == HarnessStatus::Running
                && matches!(
                    s.events.last(),
                    Some(HarnessEvent::SystemDecision { step, .. })
                        if step == "history_compaction_pass"
                )
        })
    }

    /// The mutating tool call currently awaiting approval (manual mode), if any.
    fn pending_approval(&self) -> Option<(String, String, usize, usize)> {
        let s = self.state.as_ref()?;
        if s.status != HarnessStatus::WaitingForInput {
            return None;
        }
        match s.events.last() {
            Some(HarnessEvent::ApprovalRequest { tool_name, summary, index, total }) => {
                Some((tool_name.clone(), summary.clone(), *index, *total))
            }
            _ => None,
        }
    }

    /// True when the active model is in manual (approval) mode.
    fn is_manual_mode(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.approval_mode == crate::harness::ApprovalMode::Manual)
            .unwrap_or(self.options.config.manual_approval)
    }


    fn scroll_up(&mut self, lines: usize) {
        self.scroll = (self.scroll + lines).min(self.max_scroll.get());
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Submit the input box: start the resident loop if it isn't running, else
    /// send the text as a steer / answer into the live loop.
    /// Commit the answer to the current ask_user question. A choice question
    /// resolves to the selected option's value; a free-text question to the typed
    /// input. When the last question is answered, the whole set is sent to the
    /// loop as one `[answer]`.
    fn answer_current_question(&mut self) {
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
    fn finish_answer(&mut self, text: String) {
        if let Some(tx) = self.input_tx.clone().filter(|_| self.agent_alive()) {
            if tx.send(LoopInput::Answer(text)).is_err() {
                self.error = Some("agent loop is no longer accepting input".to_string());
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

    fn input_clear(&mut self) {
        self.input.truncate(0);
        self.input_cursor = 0;
        self.pasted_blocks.clear();
        self.attachments.clear();
    }

    /// Replace the whole input (e.g. from a slash-command autocomplete) and put
    /// the cursor at the end.
    fn input_set(&mut self, value: String) {
        self.input_cursor = value.chars().count();
        self.input = value;
    }

    fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Replace the input from a recalled history entry (clears any paste chips).
    fn recall_set(&mut self, value: String) {
        self.pasted_blocks.clear();
        self.input_set(value);
    }

    /// True when the cursor sits on the first / last line of the input.
    fn input_on_first_line(&self) -> bool {
        let end = self.input_byte_at(self.input_cursor);
        !self.input[..end].contains('\n')
    }
    fn input_on_last_line(&self) -> bool {
        let start = self.input_byte_at(self.input_cursor);
        !self.input[start..].contains('\n')
    }

    /// Recall the previous (older) history entry. Returns false when there's none.
    fn recall_history_prev(&mut self) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = self.expand_input();
                self.input_history.len() - 1
            }
            Some(0) => return true, // already at the oldest
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        self.recall_set(self.input_history[pos].clone());
        true
    }

    /// Recall the next (newer) entry, restoring the draft past the newest. False
    /// when already editing the draft.
    fn recall_history_next(&mut self) -> bool {
        match self.history_pos {
            None => false,
            Some(p) if p + 1 < self.input_history.len() => {
                self.history_pos = Some(p + 1);
                self.recall_set(self.input_history[p + 1].clone());
                true
            }
            Some(_) => {
                self.history_pos = None;
                let draft = std::mem::take(&mut self.history_draft);
                self.recall_set(draft);
                true
            }
        }
    }

    /// Move the cursor up / down one line in multi-line input, preserving column.
    fn input_up(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let line_start = chars[..cur].iter().rposition(|&c| c == '\n').map(|i| i + 1).unwrap_or(0);
        if line_start == 0 {
            return;
        }
        let col = cur - line_start;
        let prev_end = line_start - 1;
        let prev_start = chars[..prev_end].iter().rposition(|&c| c == '\n').map(|i| i + 1).unwrap_or(0);
        self.input_cursor = prev_start + col.min(prev_end - prev_start);
    }
    fn input_down(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let line_start = chars[..cur].iter().rposition(|&c| c == '\n').map(|i| i + 1).unwrap_or(0);
        let col = cur - line_start;
        let Some(nl) = chars[cur..].iter().position(|&c| c == '\n').map(|i| cur + i) else {
            return;
        };
        let next_start = nl + 1;
        let next_end = chars[next_start..]
            .iter()
            .position(|&c| c == '\n')
            .map(|i| next_start + i)
            .unwrap_or(chars.len());
        self.input_cursor = next_start + col.min(next_end - next_start);
    }

    /// Byte offset of a given char index (clamped to the string end).
    fn input_byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn input_insert(&mut self, c: char) {
        let at = self.input_byte_at(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
    }

    /// Handle a text paste. A big / multi-line paste collapses to a compact chip
    /// in the input (expanded on send) so it doesn't overflow; a small single-line
    /// paste is inserted inline. (Screenshots come via Ctrl+V — see
    /// `paste_clipboard_image` — not through here.)
    fn input_paste(&mut self, text: &str) {
        // Dragging a file/screenshot into the terminal pastes its path — attach it.
        if let Some(path) = Self::dropped_file(text) {
            self.attach_dropped(&path);
            return;
        }
        let text = text.replace('\r', "");
        let lines = text.lines().count().max(1);
        if lines > 1 || text.chars().count() > 200 {
            let n = self.pasted_blocks.len() + 1;
            let marker = format!("[Pasted #{n} · {lines} line{}]", if lines == 1 { "" } else { "s" });
            for c in marker.chars() {
                self.input_insert(c);
            }
            self.pasted_blocks.push((marker, text));
        } else {
            for c in text.chars() {
                self.input_insert(c);
            }
        }
        self.suggestion_index = 0;
    }

    /// Grab an image from the system clipboard (a screenshot) and attach it: write
    /// it to the workspace temp dir and drop a chip that expands to its path on
    /// send, so the agent can `read_image` it. macOS via `osascript`; Linux via
    /// `wl-paste` (Wayland) or `xclip` (X11). Multiple screenshots accumulate as
    /// separate chips.
    fn paste_clipboard_image(&mut self) {
        let dir = self
            .options
            .config
            .workspace
            .join(".snippet")
            .join("scratch")
            .join("images");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            self.status = format!("couldn't create image temp dir: {error}");
            return;
        }
        let dest = dir.join(format!("{}.png", uuid::Uuid::new_v4()));

        let ok = if cfg!(target_os = "macos") {
            // Write the clipboard's PNG data to `dest`; errors (no image on the
            // clipboard) are caught and surfaced as a status message.
            let script = format!(
                "set f to open for access (POSIX file \"{}\") with write permission\n\
                 try\n\
                   write (the clipboard as «class PNGf») to f\n\
                   close access f\n\
                 on error errm\n\
                   close access f\n\
                   error errm\n\
                 end try",
                dest.display()
            );
            let ran = std::process::Command::new("osascript").arg("-e").arg(&script).output();
            matches!(&ran, Ok(out) if out.status.success())
        } else {
            // Linux: try Wayland's wl-paste, then X11's xclip. Each writes PNG
            // bytes to stdout; capture into the dest file.
            let mut wrote = false;
            for (cmd, args) in [
                ("wl-paste", vec!["--type", "image/png"]),
                ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
            ] {
                if let Ok(out) = std::process::Command::new(cmd).args(&args).output() {
                    if out.status.success() && !out.stdout.is_empty() {
                        wrote = std::fs::write(&dest, &out.stdout).is_ok();
                        if wrote {
                            break;
                        }
                    }
                }
            }
            wrote
        };
        let ok = ok && std::fs::metadata(&dest).map(|m| m.len() > 0).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&dest);
            self.status = if cfg!(target_os = "macos") {
                "No image on the clipboard — copy a screenshot first.".to_string()
            } else {
                "No clipboard image — copy a screenshot first (needs wl-paste or xclip on Linux)."
                    .to_string()
            };
            return;
        }
        self.attachments.push((true, dest.display().to_string(), "screenshot".to_string()));
        self.status = "📎 attached screenshot".to_string();
    }

    /// If `text` is exactly one existing file path (as a terminal pastes when you
    /// drag a file in — possibly quoted or with backslash-escaped spaces), return it.
    fn dropped_file(text: &str) -> Option<std::path::PathBuf> {
        let mut s = text.trim();
        if s.len() >= 2
            && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
        {
            s = &s[1..s.len() - 1];
        }
        if s.is_empty() {
            return None;
        }
        let unescaped = s.replace("\\ ", " ").replace("\\\\", "\\");
        let p = std::path::PathBuf::from(&unescaped);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    }

    /// Copy a dropped file into the workspace scratch dir and add a chip that
    /// expands to its path on send (images → read_image, others → read).
    fn attach_dropped(&mut self, src: &std::path::Path) {
        let is_img = matches!(
            src.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif")
        );
        let subdir = if is_img { "images" } else { "files" };
        let dir = self.options.config.workspace.join(".snippet").join("scratch").join(subdir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            self.status = format!("couldn't attach: {error}");
            return;
        }
        let fname = src.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let dest = dir.join(format!("{}-{fname}", uuid::Uuid::new_v4().simple()));
        if let Err(error) = std::fs::copy(src, &dest) {
            self.status = format!("couldn't attach {fname}: {error}");
            return;
        }
        self.attachments.push((is_img, dest.display().to_string(), fname.to_string()));
        self.status = format!("📎 attached {fname}");
    }

    /// Expand any paste chips in the current input back to their real content.
    fn expand_input(&self) -> String {
        let mut out = self.input.clone();
        for (marker, content) in &self.pasted_blocks {
            out = out.replace(marker, content);
        }
        out
    }

    /// The message to send: the expanded input plus any pending attachments, each
    /// appended as an explicit marker (images → read_image, files → read) so the
    /// agent opens them. Attachments live outside the input text and are cleared
    /// with it on send.
    fn message_for_send(&self) -> String {
        let mut out = self.expand_input();
        // Exactly the marker shape `strip_attachment_markers` hides from the
        // transcript: `[attached image — …]` / `[attached file — …]` (the em-dash
        // must immediately follow "image"/"file " — no filename before it).
        for (is_img, path, _name) in &self.attachments {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&if *is_img {
                format!("[attached image — call read_image on this exact path to view it: {path}]")
            } else {
                format!("[attached file — read it at this exact path: {path}]")
            });
        }
        out
    }

    fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            // Nothing to the left in the text — pop the most recent attachment so
            // a mis-attached file can be removed (the pill count drops by one).
            if self.attachments.pop().is_some() {
                let left = self.attachments.len();
                self.status = if left == 0 {
                    "attachment removed".to_string()
                } else {
                    format!("attachment removed · 📎 {left} left")
                };
            }
            return;
        }
        let start = self.input_byte_at(self.input_cursor - 1);
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor -= 1;
    }

    fn input_delete(&mut self) {
        if self.input_cursor >= self.input_len() {
            return;
        }
        let start = self.input_byte_at(self.input_cursor);
        let end = self.input_byte_at(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
    }

    /// Char index of the previous word boundary: skip whitespace, then word chars.
    fn input_prev_word(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.input_cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// Char index of the next word boundary: skip whitespace, then word chars.
    fn input_next_word(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let n = chars.len();
        let mut i = self.input_cursor.min(n);
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn input_delete_word_back(&mut self) {
        let target = self.input_prev_word();
        if target == self.input_cursor {
            return;
        }
        let start = self.input_byte_at(target);
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor = target;
    }

    fn input_delete_word_forward(&mut self) {
        let target = self.input_next_word();
        if target == self.input_cursor {
            return;
        }
        let start = self.input_byte_at(self.input_cursor);
        let end = self.input_byte_at(target);
        self.input.replace_range(start..end, "");
    }

    fn input_delete_to_start(&mut self) {
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(0..end, "");
        self.input_cursor = 0;
    }

    fn input_delete_to_end(&mut self) {
        let start = self.input_byte_at(self.input_cursor);
        self.input.truncate(start);
    }

    fn input_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    fn input_right(&mut self) {
        if self.input_cursor < self.input_len() {
            self.input_cursor += 1;
        }
    }

    fn submit(&mut self) {
        // The inline login form captures keys in handle_login_key; submit() is
        // not reached while it is active.
        if self.login_active {
            self.login_connect();
            return;
        }

        let text = self.message_for_send();
        let text = text.trim().to_string();
        if text.is_empty() {
            if !self.agent_alive() {
                self.status = "Enter a task before starting.".to_string();
            }
            return;
        }
        // Record for Up/Down history recall (skip consecutive duplicates).
        if self.input_history.last().map(String::as_str) != Some(text.as_str()) {
            self.input_history.push(text.clone());
        }
        self.history_pos = None;
        self.input_clear();
        self.scroll = 0;

        if text.starts_with('/') {
            self.handle_slash_command(&text);
            return;
        }

        // While the agent is executing, hold the message instead of steering the
        // running turn — it's submitted when the run finishes (or is stopped). Esc
        // stops the run, which flushes the queue immediately.
        if self.agent_busy() {
            self.queued_inputs.push_back(text);
            self.status = if self.is_compacting() {
                "compacting context — your message will send once it's done".to_string()
            } else {
                format!(
                    "queued ({}) — sends when the run finishes · Esc to stop & send now",
                    self.queued_inputs.len()
                )
            };
            return;
        }

        self.submit_text(text);
    }

    /// Send one input to the loop now (answer a pending question, steer an idle
    /// resident loop, or spawn a fresh run if none is alive). Used by `submit` when
    /// not busy and by the queue flush.
    fn submit_text(&mut self, text: String) {
        self.scroll = 0;
        if let Some(tx) = self.input_tx.clone().filter(|_| self.agent_alive()) {
            let waiting = self
                .state
                .as_ref()
                .map(|s| s.status == HarnessStatus::WaitingForInput)
                .unwrap_or(false);
            let input = if waiting {
                LoopInput::Answer(text)
            } else {
                LoopInput::UserMessage(text)
            };
            if tx.send(input).is_err() {
                self.error = Some("agent loop is no longer accepting input".to_string());
            } else {
                // The loop is now working; hold this locally until the state file
                // confirms, so a fast follow-up queues instead of steering mid-run.
                // `was_busy` too, so the flush edge still fires if the whole turn
                // completes before the next state refresh.
                self.sent_turn_pending = true;
                self.was_busy = true;
            }
        } else {
            // Resume the existing conversation rather than starting fresh — after an
            // interrupt the agent has died but the transcript is intact; resume=false
            // would clobber it into a new conversation.
            self.spawn_loop(Some(text), true);
            self.sent_turn_pending = true;
            self.was_busy = true;
        }
    }

    /// Submit everything queued when the agent goes idle/stopped — as a BURST of
    /// individual messages. The first opens the turn; the rest land before the
    /// first model call and are folded in as steers, so the agent still sees the
    /// full set up front in ONE turn, while each message keeps its own frame
    /// (and its own attachments) instead of being blurred into a joined blob.
    fn flush_queued_input(&mut self) {
        let held: Vec<String> = self.queued_inputs.drain(..).collect();
        for text in held {
            self.submit_text(text);
        }
        self.status = String::new();
    }

    fn spawn_loop(&mut self, initial: Option<String>, resume: bool) {
        if self.agent_alive() {
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
        let cfg = self.effective_config();
        let handle = crate::session::start_session(
            &cfg,
            self.active_state_path.clone(),
            initial,
            resume,
            Some(self.stream.clone()),
        );
        self.input_tx = Some(handle.input_tx);
        self.agent = Some(handle.join);
    }

    fn interrupt_or_quit(&mut self) {
        // Interrupt only while a turn is actually executing. The resident loop
        // stays ALIVE between turns by design, so gating on agent_alive() made
        // Ctrl+C unable to quit whenever a session was loaded — the terminal
        // convention (Ctrl+C exits an idle program) applies when merely idle.
        if self.agent_busy() {
            if let Some(tx) = &self.input_tx {
                let _ = tx.send(LoopInput::Interrupt);
            }
            self.status = "Interrupting...".to_string();
        } else {
            self.quit = true;
        }
    }

    async fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.refresh_state().await;

        // Refresh the account-wide ChatGPT usage a few times a second (tiny file,
        // signed-in users only) so the footer figure is global, not per-chat.
        if self.frame % 8 == 0 && crate::chatgpt_auth::is_signed_in() {
            self.global_usage = crate::chatgpt::read_global_usage();
        }

        // Flush a queued input on the busy → not-busy edge: the run just finished or
        // was stopped, so submit the next held message as its own turn.
        let busy = self.agent_busy();
        if self.was_busy && !busy && !self.queued_inputs.is_empty() {
            self.flush_queued_input();
        }
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
            .chatgpt_device_begin_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            let handle = self.chatgpt_device_begin_handle.take().expect("checked is_some");
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

    async fn refresh_state(&mut self) {
        if let Ok(metadata) = tokio::fs::metadata(&self.active_state_path).await {
            if let Ok(modified) = metadata.modified() {
                if Some(modified) == self.last_state_modified {
                    return;
                }
                self.last_state_modified = Some(modified);
                // The persisted state has caught up with our optimistic send — from
                // here the real status governs busy/idle (and the flush edge).
                self.sent_turn_pending = false;
                // A newer persisted state means the turn that was streaming has
                // committed its text into events — drop the live buffer so the
                // committed copy doesn't render twice.
                crate::llm::StreamBuffer::clear(&self.stream);
            }
        } else {
            self.state = None;
            self.last_state_modified = None;
            return;
        }

        match tokio::fs::read(&self.active_state_path).await {
            Ok(bytes) => match crate::harness::deserialize_state(&bytes) {
                Ok(state) => self.state = Some(state),
                // An unreadable file (e.g. saved by an older build) shouldn't pin a
                // red error in the footer. Show the session empty; starting a new
                // run overwrites it cleanly.
                Err(_) => {
                    self.state = None;
                    self.status =
                        "This session's saved state is unreadable — start a new task to replace it."
                            .to_string();
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.state = None;
                self.last_state_modified = None;
            }
            Err(error) => {
                self.error = Some(format!("state read error: {error}"));
                self.last_state_modified = None;
            }
        }
    }
}

async fn run_app(
    terminal: &mut TuiTerminal,
    options: TuiOptions,
) -> Result<ExitInfo, Box<dyn std::error::Error>> {
    let mut app = App::new(options);
    app.refresh_effective_model();
    app.refresh_state().await;
    if app.options.config.resume_on_start || app.options.resume.is_some() {
        app.spawn_loop(None, true);
    }

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
        terminal.draw(|frame| render(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                // With the kitty protocol a key can arrive as Press/Repeat/Release;
                // act on Press/Repeat only so a key isn't handled twice.
                Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(&mut app, key),
                Event::Paste(text) => {
                    // Route paste to the login form when it's open, else the prompt.
                    if app.login_active {
                        app.login_paste(&text);
                    } else {
                        app.input_paste(&text);
                    }
                }
                // Mouse wheel scrolls the transcript (chat canvas).
                Event::Mouse(me) => match me.kind {
                    MouseEventKind::ScrollUp => app.scroll_up(3),
                    MouseEventKind::ScrollDown => app.scroll_down(3),
                    _ => {}
                },
                _ => {}
            }
        }
        app.tick().await;
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

fn handle_key(app: &mut App, key: KeyEvent) {
    // An error banner shows until the user acts again — one keypress means it's
    // been seen. Without this, `error` (cleared only on spawn) permanently masks
    // every later status line ("queued (1)…", "Session deleted", …).
    app.error = None;

    if app.login_active {
        handle_login_key(app, key);
        return;
    }

    // While a mutating tool waits for approval (manual mode), keys are y/n/Esc only.
    if app.screen == Screen::Main && app.pending_approval().is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(tx) = &app.input_tx {
                    let _ = tx.send(LoopInput::Approve);
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Approve this and stop prompting for the rest of the RUN — run-scoped
                // only. Never persist to the config: a one-key unblock must not
                // silently remove the manual-approval gate for every future session.
                // Only offered when multiple approvals are pending (matches the bar).
                let multi = app.pending_approval().map(|(_, _, _, t)| t > 1).unwrap_or(false);
                if multi {
                    if let Some(tx) = &app.input_tx {
                        let _ = tx.send(LoopInput::ApproveAll);
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(tx) = &app.input_tx {
                    let _ = tx.send(LoopInput::Deny);
                }
            }
            KeyCode::Esc => {
                if let Some(tx) = &app.input_tx {
                    let _ = tx.send(LoopInput::Interrupt);
                }
                app.status = "Interrupting…".to_string();
            }
            _ => {}
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        // Global shortcuts first.
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('d') => {
                app.quit = true;
                return;
            }
            KeyCode::Char('c') => {
                app.interrupt_or_quit();
                return;
            }
            KeyCode::Char('r') => {
                app.spawn_loop(None, true);
                return;
            }
            // Paste a screenshot from the clipboard (macOS) as an attached image.
            KeyCode::Char('v') => {
                app.paste_clipboard_image();
                return;
            }
            // Cancel messages queued for after the current run.
            KeyCode::Char('x') => {
                if !app.queued_inputs.is_empty() {
                    let n = app.queued_inputs.len();
                    app.queued_inputs.clear();
                    app.status =
                        format!("cancelled {n} queued message{}", if n == 1 { "" } else { "s" });
                }
                return;
            }
            _ => {}
        }
        // Readline-style line editing for the prompt (main screen only).
        if app.screen == Screen::Main && !app.login_active {
            match key.code {
                KeyCode::Char('w') => {
                    app.input_delete_word_back();
                    app.suggestion_index = 0;
                }
                KeyCode::Char('u') => {
                    app.input_delete_to_start();
                    app.suggestion_index = 0;
                }
                KeyCode::Char('k') => app.input_delete_to_end(),
                KeyCode::Char('a') => app.input_cursor = 0,
                KeyCode::Char('e') => app.input_cursor = app.input_len(),
                KeyCode::Left => app.input_cursor = app.input_prev_word(),
                KeyCode::Right => app.input_cursor = app.input_next_word(),
                _ => {}
            }
        }
        return;
    }

    if app.screen == Screen::Profiles {
        let names = app.options.config.profile_names();
        let total = names.len();
        let rows = total + 1; // profiles + the "Add a model" row
        match key.code {
            KeyCode::Up => {
                app.profiles_selected_index = (app.profiles_selected_index + rows - 1) % rows;
            }
            KeyCode::Down => {
                app.profiles_selected_index = (app.profiles_selected_index + 1) % rows;
            }
            KeyCode::Enter => {
                if app.profiles_selected_index >= total {
                    app.open_profile_editor(None);
                } else {
                    let name = names[app.profiles_selected_index].clone();
                    // Enter sets the model for THIS chat (local override); with no live
                    // chat there's nothing to scope to, so fall back to the global default.
                    if app.agent_alive() {
                        app.activate_profile_local(&name);
                    } else {
                        app.activate_profile(&name);
                    }
                }
            }
            KeyCode::Char('g') => {
                if app.profiles_selected_index < total {
                    let name = names[app.profiles_selected_index].clone();
                    app.activate_profile(&name); // global default for all chats
                }
            }
            KeyCode::Char('a') => app.open_profile_editor(None),
            KeyCode::Char('e') => {
                if app.profiles_selected_index < total {
                    let name = names[app.profiles_selected_index].clone();
                    app.open_profile_editor(Some(name));
                }
            }
            KeyCode::Char('d') => {
                if total <= 1 {
                    app.status = "Can't delete the only profile.".to_string();
                } else if app.profiles_selected_index < total {
                    let name = names[app.profiles_selected_index].clone();
                    app.options.config.remove_profile(&name);
                    let _ = app.save_config_file();
                    let new_total = app.options.config.profile_names().len();
                    if app.profiles_selected_index >= new_total {
                        app.profiles_selected_index = new_total.saturating_sub(1);
                    }
                    app.status = format!("Removed profile “{name}”");
                }
            }
            KeyCode::Esc => app.screen = Screen::Main,
            _ => {}
        }
        return;
    }


    if app.screen == Screen::ThemeSelection {
        let count = PRESETS.len();
        match key.code {
            KeyCode::Up => {
                app.theme_selected_index = (app.theme_selected_index + count - 1) % count;
                set_theme_index(app.theme_selected_index); // live preview
            }
            KeyCode::Down => {
                app.theme_selected_index = (app.theme_selected_index + 1) % count;
                set_theme_index(app.theme_selected_index);
            }
            KeyCode::Enter => {
                set_theme_index(app.theme_selected_index);
                app.persist_theme();
                let label = PRESETS.get(app.theme_selected_index).map(|p| p.label).unwrap_or("");
                app.status = format!("Theme: {label}");
                app.screen = Screen::Main;
            }
            KeyCode::Esc => {
                set_theme_index(app.theme_original_index); // revert live preview
                app.screen = Screen::Main;
                app.status = String::new();
            }
            _ => {}
        }
        return;
    }

    if app.screen == Screen::ResumeSelection {
        // Use the snapshot taken when the picker opened (see conv_cache) —
        // re-scanning every session file per keypress made the picker laggy.
        let convs = match &app.conv_cache {
            Some(c) => c.clone(),
            None => {
                let c = app.list_conversations();
                app.conv_cache = Some(c.clone());
                c
            }
        };
        if convs.is_empty() {
            match key.code {
                KeyCode::Esc => {
                    app.screen = Screen::Main;
                    app.status = "Type a task and press Enter.".to_string();
                }
                _ => {}
            }
            return;
        }

        // Rename mode owns all keys: build the title buffer until Enter/Esc.
        if app.resume_rename.is_some() {
            match key.code {
                KeyCode::Char(c) => {
                    let b = app.resume_rename.as_mut().unwrap();
                    b.push(c);
                    let s = b.clone();
                    app.status = format!("Rename: {s}_  (Enter to save · Esc to cancel)");
                }
                KeyCode::Backspace => {
                    let b = app.resume_rename.as_mut().unwrap();
                    b.pop();
                    let s = b.clone();
                    app.status = format!("Rename: {s}_  (Enter to save · Esc to cancel)");
                }
                KeyCode::Enter => {
                    let idx = app.resume_selected_index.min(convs.len() - 1);
                    let name = convs[idx].0.clone();
                    let title = app.resume_rename.take().unwrap_or_default();
                    app.rename_conversation(&name, title.trim());
                    app.conv_cache = Some(app.list_conversations());
                    let short: String = title.trim().chars().take(40).collect();
                    app.status = if short.is_empty() {
                        "Title cleared.".to_string()
                    } else {
                        format!("Renamed to “{short}”.")
                    };
                }
                KeyCode::Esc => {
                    app.resume_rename = None;
                    app.status = "Rename cancelled.".to_string();
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Up => {
                app.resume_pending_delete = false;
                app.resume_selected_index = if app.resume_selected_index == 0 {
                    convs.len() - 1
                } else {
                    app.resume_selected_index - 1
                };
            }
            KeyCode::Down => {
                app.resume_pending_delete = false;
                app.resume_selected_index = (app.resume_selected_index + 1) % convs.len();
            }
            KeyCode::Char('d') => {
                let idx = app.resume_selected_index.min(convs.len() - 1);
                let (name, title) = convs[idx].clone();
                if app.resume_pending_delete {
                    app.delete_conversation(&name);
                    app.conv_cache = Some(app.list_conversations());
                    app.resume_pending_delete = false;
                    let remaining = convs.len() - 1;
                    if remaining == 0 {
                        app.screen = Screen::Main;
                        app.status = "Session deleted. No saved sessions left.".to_string();
                    } else {
                        if app.resume_selected_index >= remaining {
                            app.resume_selected_index = remaining - 1;
                        }
                        app.status = "Session deleted.".to_string();
                    }
                } else {
                    app.resume_pending_delete = true;
                    let short: String = title.chars().take(48).collect();
                    app.status = format!("Press d again to delete \"{short}\", or Esc/↑↓ to cancel.");
                }
            }
            KeyCode::Char('r') => {
                app.resume_pending_delete = false;
                app.resume_rename = Some(String::new());
                app.status = "Rename: type a new title, Enter to save, Esc to cancel.".to_string();
            }
            KeyCode::Enter => {
                app.resume_pending_delete = false;
                app.conv_cache = None;
                let selected_idx = app.resume_selected_index.min(convs.len().saturating_sub(1));
                let name = convs[selected_idx].0.clone();
                app.switch_conversation(&name);
                app.screen = Screen::Main;
                if app.active_state_path.exists() {
                    app.spawn_loop(None, true);
                } else {
                    app.status = "No saved session to resume. Start a new one with /new or type a task.".to_string();
                }
            }
            KeyCode::Esc => {
                app.resume_pending_delete = false;
                app.conv_cache = None;
                app.screen = Screen::Main;
                app.status = "Type a task and press Enter.".to_string();
            }
            _ => {}
        }
        return;
    }

    // The inline login form owns all key input while active.
    if app.login_active {
        handle_login_key(app, key);
        return;
    }

    // A pending ask_user question owns navigation/selection keys.
    if handle_question_key(app, key) {
        return;
    }

    let matches = get_suggestions(app);
    let mut handled = false;

    if !matches.is_empty() {
        if app.suggestion_index >= matches.len() {
            app.suggestion_index = 0;
        }

        match key.code {
            KeyCode::Tab => {
                // Autocomplete the input to the highlighted command. A command
                // that takes an argument (already contains a space, e.g.
                // "/resume <name>") fills in as-is; a bare command gets a
                // trailing space, ready to run or extend.
                let selected = matches[app.suggestion_index].0.clone();
                app.input_set(if selected.contains(' ') {
                    selected
                } else {
                    format!("{selected} ")
                });
                app.suggestion_index = 0;
                handled = true;
            }
            KeyCode::Down => {
                app.suggestion_index = (app.suggestion_index + 1) % matches.len();
                handled = true;
            }
            KeyCode::BackTab | KeyCode::Up => {
                app.suggestion_index = if app.suggestion_index == 0 {
                    matches.len() - 1
                } else {
                    app.suggestion_index - 1
                };
                handled = true;
            }
            KeyCode::Enter => {
                let selected_cmd = &matches[app.suggestion_index].0;
                if app.input == *selected_cmd
                    || selected_cmd.starts_with("/resume ")
                    || selected_cmd.starts_with("/rewind ")
                    || selected_cmd.starts_with("/profile ")
                {
                    app.input_set(selected_cmd.clone());
                    app.submit();
                } else {
                    app.input_set(format!("{} ", selected_cmd));
                    app.suggestion_index = 0;
                }
                handled = true;
            }
            _ => {}
        }
    }

    if !handled {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            // Alt/Shift+Enter inserts a newline so prompts can be multi-line;
            // plain Enter submits.
            KeyCode::Enter if alt || shift => {
                app.input_insert('\n');
                app.suggestion_index = 0;
            }
            KeyCode::Enter => app.submit(),
            // Up/Down recall input history at the edges of the prompt; move the
            // cursor by line in the middle of a multi-line prompt; fall back to
            // scrolling the transcript when there's no history to recall.
            KeyCode::Up => {
                if app.input_on_first_line() {
                    if !app.recall_history_prev() {
                        app.scroll_up(1);
                    }
                } else {
                    app.input_up();
                }
            }
            KeyCode::Down => {
                if app.input_on_last_line() {
                    if !app.recall_history_next() {
                        app.scroll_down(1);
                    }
                } else {
                    app.input_down();
                }
            }
            KeyCode::PageUp => app.scroll_up(10),
            KeyCode::PageDown => app.scroll_down(10),
            // Home/End move the cursor when editing; scroll the transcript when the
            // prompt is empty.
            KeyCode::Home if !app.input.is_empty() => app.input_cursor = 0,
            KeyCode::End if !app.input.is_empty() => app.input_cursor = app.input_len(),
            KeyCode::Home => app.scroll_up(usize::MAX),
            KeyCode::End => app.scroll = 0,
            // Cursor movement — Alt/Option + ←/→ jumps by word.
            KeyCode::Left if alt => app.input_cursor = app.input_prev_word(),
            KeyCode::Right if alt => app.input_cursor = app.input_next_word(),
            KeyCode::Left => app.input_left(),
            KeyCode::Right => app.input_right(),
            KeyCode::Esc => {
                if !app.input.is_empty() {
                    app.input_clear();
                    app.suggestion_index = 0;
                } else if app.agent_alive() {
                    if let Some(tx) = &app.input_tx {
                        let _ = tx.send(LoopInput::Interrupt);
                    }
                    app.status = "Interrupting...".to_string();
                }
            }
            // Alt/Option + Backspace deletes the word before the cursor.
            KeyCode::Backspace if alt => {
                app.input_delete_word_back();
                app.suggestion_index = 0;
            }
            KeyCode::Backspace => {
                app.input_backspace();
                app.suggestion_index = 0;
            }
            KeyCode::Delete if alt => {
                app.input_delete_word_forward();
                app.suggestion_index = 0;
            }
            KeyCode::Delete => {
                app.input_delete();
                app.suggestion_index = 0;
            }
            // Readline word ops when the terminal sends Option as Meta (Alt+b/f/d).
            KeyCode::Char('b') if alt => app.input_cursor = app.input_prev_word(),
            KeyCode::Char('f') if alt => app.input_cursor = app.input_next_word(),
            KeyCode::Char('d') if alt => {
                app.input_delete_word_forward();
                app.suggestion_index = 0;
            }
            // Typing is allowed while the agent works — it becomes a steer on Enter.
            KeyCode::Char(c) if !alt => {
                app.input_insert(c);
                app.suggestion_index = 0;
            }
            _ => {}
        }
    }
}

/// Copy text to the clipboard, best-effort: a native tool if present, plus an
/// OSC52 escape so it also works over SSH / inside tmux.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    fn pipe(cmd: &str, args: &[&str], text: &str) -> bool {
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(text.as_bytes());
        }
        matches!(child.wait(), Ok(s) if s.success())
    }
    let _ = (cfg!(target_os = "macos") && pipe("pbcopy", &[], text))
        || pipe("wl-copy", &[], text)
        || pipe("xclip", &["-selection", "clipboard"], text);
    // OSC52 reaches the terminal's own clipboard (works over SSH / in tmux).
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = out.write_all(format!("\x1b]52;c;{b64}\x07").as_bytes());
    let _ = out.flush();
}

/// Key handling for the compact inline login form. Tab/↑/↓ move between fields,
/// ←/→ change the provider or model, typing edits the focused text field, Enter
/// connects, Esc cancels.
fn handle_login_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('c') if app.chatgpt_device_code.is_some() => {
            let code = app.chatgpt_device_code.as_ref().unwrap().user_code.clone();
            copy_to_clipboard(&code);
            app.status = format!("Copied code {code} to clipboard.");
        }
        KeyCode::Char('u') if app.chatgpt_device_code.is_some() => {
            let url = app.chatgpt_device_code.as_ref().unwrap().verification_url.clone();
            copy_to_clipboard(&url);
            app.status = "Copied sign-in URL to clipboard.".to_string();
        }
        KeyCode::Esc => {
            app.close_login(true);
            app.status = String::new();
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.close_login(true);
            app.status = String::new();
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL)
            && app.form_provider == "chatgpt" =>
        {
            app.start_chatgpt_login(crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode);
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL)
            && app.form_provider == "chatgpt" =>
        {
            app.logout_chatgpt();
        }
        KeyCode::Enter => app.login_connect(),
        KeyCode::Tab => app.login_move_focus(true),
        KeyCode::BackTab => app.login_move_focus(false),
        // Up/Down move between fields everywhere (incl. the Model field); the model
        // value is changed with Left/Right.
        KeyCode::Down => app.login_move_focus(true),
        KeyCode::Up => app.login_move_focus(false),
        KeyCode::Left => app.login_adjust(false),
        KeyCode::Right => app.login_adjust(true),
        KeyCode::Backspace => app.login_backspace(),
        // Only insert PLAIN characters: a Ctrl/Alt-chorded key reaching this
        // catch-all would silently type its letter into the focused field — into
        // the masked API key, invisibly, until auth fails.
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            app.login_edit_char(c)
        }
        _ => {}
    }
}

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();

    if app.screen == Screen::ResumeSelection {
        render_resume_selection(frame, area, app);
        return;
    }

    if app.screen == Screen::ThemeSelection {
        render_theme_selection(frame, area, app);
        return;
    }


    if app.screen == Screen::Profiles {
        render_profiles(frame, area, app);
        return;
    }

    let sugg_h = suggestion_height(app);
    let input_h = input_height(app, area.width);
    // A dedicated compaction-progress row sits directly above the input while the
    // history is being compacted (1 row when active, 0 otherwise).
    let compact_h: u16 = if app.is_compacting() { 1 } else { 0 };
    // Approval prompt (manual mode): 2 rows directly above the input when a mutating
    // tool is awaiting y/n (mutually exclusive with compaction).
    let approval_h: u16 = if app.pending_approval().is_some() { 6 } else { 0 };

    // Header, Content, Suggestions, Question, Compaction, Approval, Input, Status, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // Header
            Constraint::Length(1),                  // gap under header
            Constraint::Min(10),                     // Content
            Constraint::Length(sugg_h),              // Suggestions
            Constraint::Length(question_height(app)), // Question
            Constraint::Length(compact_h),          // Compaction progress
            Constraint::Length(approval_h),         // Approval prompt (above input)
            Constraint::Length(1),                  // gap above input
            Constraint::Length(input_h),            // Input (grows with wrapped lines)
            Constraint::Length(1),                  // Status message
            Constraint::Length(1),                  // Footer (metadata)
        ])
        .split(area);

    let header_area = chunks[0];
    let content_area = chunks[2];
    let suggestions_area = chunks[3];
    let question_area = chunks[4];
    let compaction_area = chunks[5];
    let approval_area = chunks[6];
    let input_area = chunks[8];
    let status_msg_area = chunks[9];
    let footer_area = chunks[10];

    render_header(frame, header_area, app);
    render_history(frame, content_area, app);
    if sugg_h > 0 {
        render_suggestions(frame, suggestions_area, app);
    }
    render_question(frame, question_area, app);
    if compact_h > 0 {
        render_compaction_bar(frame, compaction_area, app);
    }
    if approval_h > 0 {
        render_approval_bar(frame, approval_area, app);
    }
    render_input(frame, input_area, app);
    render_status_message(frame, status_msg_area, app);
    render_status(frame, footer_area, app);
}

/// The approval prompt shown above the input while a mutating tool awaits y/n.
fn render_approval_bar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders, Wrap};
    let Some((tool, summary, index, total)) = app.pending_approval() else {
        return;
    };
    let title = if total > 1 {
        format!(" approve · {tool}   {index}/{total} ")
    } else {
        format!(" approve · {tool} ")
    };
    let prefix = if tool == "bash" { "$ " } else { "" };
    let cmd = if summary.trim().is_empty() {
        "(no preview)".to_string()
    } else {
        summary
    };
    // "approve all" only makes sense with more than one pending in this batch.
    let mut action_spans = vec![
        Span::styled("  ✓ y ", Style::default().fg(success()).add_modifier(Modifier::BOLD)),
        Span::styled("approve   ", subtle()),
    ];
    if total > 1 {
        action_spans.push(Span::styled("⏩ a ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)));
        action_spans.push(Span::styled("approve all   ", subtle()));
    }
    action_spans.extend([
        Span::styled("✗ n ", Style::default().fg(danger()).add_modifier(Modifier::BOLD)),
        Span::styled("deny   ", subtle()),
        Span::styled("esc ", Style::default().fg(muted()).add_modifier(Modifier::BOLD)),
        Span::styled("stop", subtle()),
    ]);
    let actions = Line::from(action_spans);
    let body = vec![
        Line::from(Span::styled(format!("  {prefix}{cmd}"), Style::default().fg(self::text()))),
        Line::from(""),
        actions,
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(warn()))
        .title(Span::styled(title, Style::default().fg(warn()).add_modifier(Modifier::BOLD)));
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

/// An animated "compacting" progress line shown directly above the input box while
/// the history is being compacted.
fn render_compaction_bar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    const BARS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    let f = app.frame as usize;
    let mut spans = vec![Span::styled(
        " ✦ compacting context  ",
        Style::default().fg(accent()).add_modifier(Modifier::BOLD),
    )];
    for i in 0..16usize {
        let phase = (f / 2 + i * 2) % 14;
        let h = if phase < 7 { phase } else { 14 - phase };
        spans.push(Span::styled(BARS[h.min(7)], Style::default().fg(accent())));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One-line auth/endpoint status for a profile card.
fn profile_status(cfg: &crate::config::ModelConfig) -> String {
    match cfg.provider.as_str() {
        "chatgpt" => {
            if crate::chatgpt_auth::is_signed_in() {
                "✓ signed in".to_string()
            } else {
                "not signed in — Enter to sign in".to_string()
            }
        }
        "openai-compatible" => {
            let host = cfg
                .base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            host.split('/').next().filter(|h| !h.is_empty()).unwrap_or("custom endpoint").to_string()
        }
        _ => {
            if cfg.api_key.trim().is_empty() {
                "no api key".to_string()
            } else {
                "api key set".to_string()
            }
        }
    }
}

/// The profiles screen — every saved provider config as a card, one active. Enter
/// activates, `e` edits, `a` adds, `d` deletes.
fn render_profiles(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    // Lane count only when something is actually running — "0 active lanes" on
    // the models page was pure noise.
    let active_lanes = app
        .state
        .as_ref()
        .map(|s| s.lanes.iter().filter(|l| l.status == LaneStatus::Running).count())
        .unwrap_or(0);
    let mut header_spans = vec![
        Span::styled(" snippet", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  models", subtle()),
    ];
    if active_lanes > 0 {
        header_spans.push(Span::styled(
            format!("  ·  {} active lane{}", active_lanes, if active_lanes == 1 { "" } else { "s" }),
            Style::default().fg(lane()),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    let names = app.options.config.profile_names();
    let total = names.len();
    let active = app.options.config.active_setup.clone().unwrap_or_default();
    let setups = app.options.config.setups.as_ref();
    let sel = app.profiles_selected_index.min(total); // index `total` == the Add row

    // Window over profile cards (3 lines each); the Add row always shows at the end.
    let list_h = (chunks[1].height as usize).saturating_sub(2);
    let visible = (list_h.saturating_sub(2) / 3).max(1);
    let focus = sel.min(total.saturating_sub(1));
    let start = if total > 0 && focus >= visible { focus + 1 - visible } else { 0 };
    let end = (start + visible).min(total);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if start > 0 {
        lines.push(Line::from(Span::styled(format!("  ↑ {start} more"), Style::default().fg(faint()))));
    }
    for i in start..end {
        let name = &names[i];
        let is_sel = i == sel;
        let is_active = *name == active;
        let mut head = vec![
            Span::styled(
                if is_sel { "▍ " } else { "  " },
                Style::default().fg(accent()),
            ),
            Span::styled(
                name.clone(),
                if is_sel {
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self::text()).add_modifier(Modifier::BOLD)
                },
            ),
        ];
        if is_active {
            head.push(Span::styled("   ● active", Style::default().fg(success())));
        }
        lines.push(Line::from(head));
        if let Some(cfg) = setups.and_then(|m| m.get(name)) {
            let model = if cfg.model.is_empty() {
                "(no model)".to_string()
            } else {
                cfg.model.clone()
            };
            lines.push(Line::from(Span::styled(
                format!("     {model} · {}", profile_status(cfg)),
                subtle(),
            )));
        }
        lines.push(Line::from(""));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more", total - end),
            Style::default().fg(faint()),
        )));
    }

    let add_sel = sel >= total;
    lines.push(Line::from(vec![
        Span::styled(if add_sel { "▍ " } else { "  " }, Style::default().fg(accent())),
        Span::styled(
            "+ Add a model",
            if add_sel {
                Style::default().fg(accent()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(faint())
            },
        ),
    ]));

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));
    frame.render_widget(Paragraph::new(lines).block(block), chunks[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ move  ·  ↵ this chat  ·  g global default  ·  e edit  ·  a add  ·  d delete  ·  Esc",
            subtle(),
        ))),
        chunks[2],
    );

    if app.login_active {
        let popup_width = area.width.saturating_sub(8).min(96).max(64);
        let popup_height = area.height.saturating_sub(6).min(26).max(16);
        let popup = Rect {
            x: area.x + (area.width.saturating_sub(popup_width)) / 2,
            y: area.y + (area.height.saturating_sub(popup_height)) / 2,
            width: popup_width,
            height: popup_height,
        };
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(Span::styled(" model setup ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(login_lines(app, inner.width as usize)).wrap(Wrap { trim: false }), inner);
    }
}

fn render_resume_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};

    // Per-frame render: never rescan disk here — the picker key handler keeps
    // conv_cache fresh (populated on open, refreshed after rename/delete).
    let convs = match &app.conv_cache {
        Some(c) => c.clone(),
        None => app.list_conversations(),
    };

    // Split area: Header (1), List (Min 10), Footer (1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(area);

    // Render Header
    let header_text = vec![
        Span::styled("● snipett", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
        Span::styled("  |  Select a session to resume", Style::default().fg(self::text())),
    ];
    frame.render_widget(Paragraph::new(Line::from(header_text)), chunks[0]);

    // Render List — windowed so a long list never clips the selection off-screen.
    let mut lines = Vec::new();
    if convs.is_empty() {
        lines.push(Line::from(Span::styled("  No saved conversations found.", subtle())));
    } else {
        let total = convs.len();
        let selected_idx = app.resume_selected_index.min(total - 1);
        // chunks[1] borders (TOP|BOTTOM) take 2 rows; reserve 2 more for the
        // ↑/↓ "N more" hints so the visible window always fits.
        let visible = (chunks[1].height as usize).saturating_sub(4).max(1);
        let start = if selected_idx >= visible {
            selected_idx + 1 - visible
        } else {
            0
        };
        let end = (start + visible).min(total);
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("  ↑ {} more", start),
                Style::default().fg(faint()),
            )));
        }
        for (offset, (name, desc)) in convs[start..end].iter().enumerate() {
            let is_selected = start + offset == selected_idx;
            let line = if is_selected {
                Line::from(vec![
                    Span::styled("▍ ", Style::default().fg(accent())),
                    Span::styled(format!("{:<38} ", name), Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                    Span::styled(desc.to_string(), subtle()),
                ])
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<38} ", name), Style::default().fg(self::text())),
                    Span::styled(desc.to_string(), Style::default().fg(faint())),
                ])
            };
            lines.push(line);
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ↓ {} more", total - end),
                Style::default().fg(faint()),
            )));
        }
    }

    let list_block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));

    frame.render_widget(Paragraph::new(lines).block(list_block), chunks[1]);

    // Render Footer
    let footer_text = "↑/↓ scroll  ·  Enter resume  ·  r rename  ·  d delete  ·  Esc go back";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer_text, subtle()))),
        chunks[2]
    );
}

/// The `/theme` picker: presets with a live color swatch; arrowing previews the
/// whole UI in that theme (set in the key handler), Enter applies + persists.
fn render_theme_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(area);

    let header = vec![
        Span::styled(" snipett", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  theme", subtle()),
    ];
    frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

    let total = PRESETS.len();
    let sel = app.theme_selected_index.min(total.saturating_sub(1));
    // Window so a long preset list never clips the selection off-screen.
    let visible = (chunks[1].height as usize).saturating_sub(4).max(1);
    let start = if sel >= visible { sel + 1 - visible } else { 0 };
    let end = (start + visible).min(total);
    let mut lines = Vec::new();
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more", start),
            Style::default().fg(faint()),
        )));
    } else {
        lines.push(Line::from(""));
    }
    for (idx, preset) in PRESETS.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let bar = if selected {
            Span::styled("▍ ", Style::default().fg(accent()))
        } else {
            Span::raw("  ")
        };
        let label_style = if selected {
            Style::default().fg(accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self::text())
        };
        // Swatch in THIS preset's own colors, so each row previews its palette.
        let t = preset.theme;
        let mut spans = vec![bar, Span::styled(format!("{:<18}", preset.label), label_style)];
        for color in [t.accent, t.success, t.warn, t.danger, t.lane, t.code] {
            spans.push(Span::styled("●", Style::default().fg(color)));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more", total - end),
            Style::default().fg(faint()),
        )));
    }

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));
    frame.render_widget(Paragraph::new(lines).block(block), chunks[1]);

    let footer = "↑/↓ preview  ·  Enter apply  ·  Esc cancel";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, subtle()))),
        chunks[2],
    );
}

/// Models offered by the picker: the live-fetched list (uncapped) or the static
/// fallback for the provider, filtered by the search query (case-insensitive).

fn get_suggestions(app: &App) -> Vec<(String, String)> {
    if !app.input.starts_with('/') {
        return Vec::new();
    }

    if app.input.starts_with("/resume") {
        let query_part = if app.input.starts_with("/resume ") {
            &app.input["/resume ".len()..]
        } else {
            ""
        };

        let convs = app.list_conversations();
        return convs
            .into_iter()
            .filter(|(name, _)| name.starts_with(query_part))
            .map(|(name, desc)| (format!("/resume {}", name), desc))
            .collect();
    }

    if app.input.starts_with("/rewind") {
        let query = app.input.strip_prefix("/rewind ").unwrap_or("");
        let checkpoints = app
            .state
            .as_ref()
            .map(|s| s.checkpoints.clone())
            .unwrap_or_default();
        return checkpoints
            .iter()
            .rev()
            .map(|c| {
                let short = &c.id[..c.id.len().min(8)];
                (format!("/rewind {short}"), c.label.clone())
            })
            .filter(|(cmd, _)| cmd.contains(query))
            .collect();
    }

    if !app.input.contains(' ') {
        ALL_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&app.input))
            .map(|(cmd, desc)| (cmd.to_string(), desc.to_string()))
            .collect()
    } else {
        Vec::new()
    }
}

fn suggestion_height(app: &App) -> u16 {
    let matches_count = get_suggestions(app).len();
    if matches_count > 0 {
        matches_count as u16 + 1
    } else {
        0
    }
}

fn render_suggestions(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let matches = get_suggestions(app);

    let mut lines = Vec::new();
    for (idx, (cmd, desc)) in matches.iter().enumerate() {
        let is_selected = idx == app.suggestion_index;
        // Accent-bar flat style: the selected row gets a left ▍ bar + bold accent
        // command; others sit dim with a blank gutter. No full-width background.
        let (bar, cmd_style, desc_style) = if is_selected {
            (
                Span::styled("▍ ", Style::default().fg(accent())),
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                subtle(),
            )
        } else {
            (
                Span::raw("  "),
                Style::default().fg(self::text()),
                Style::default().fg(faint()),
            )
        };
        lines.push(Line::from(vec![
            bar,
            Span::styled(format!("{:<10}", cmd), cmd_style),
            Span::styled(format!("  {desc}"), desc_style),
        ]));
    }

    use ratatui::widgets::{Block, Borders};
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(faint()));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_status_message(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = if let Some(ref err) = app.error {
        Line::from(vec![
            Span::styled("error: ", Style::default().fg(danger()).add_modifier(Modifier::BOLD)),
            Span::styled(err.to_string(), Style::default().fg(danger())),
        ])
    } else {
        Line::from(vec![
            Span::styled(&app.status, subtle()),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // The model actually driving THIS chat (per-chat override wins) — not the
    // global default, which is misleading after "set model for this chat".
    let model = app.effective_model.1.clone();
    let name = " snipett";
    let mut spans = vec![
        Span::styled(name, Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", Style::default().fg(muted())),
        Span::styled(model.clone(), Style::default().fg(muted())),
        Span::raw(" "),
    ];
    // Active-goal badge — the agent is autonomously driving toward a /goal.
    let goal_label = app
        .state
        .as_ref()
        .and_then(|s| s.goal.as_ref())
        .and_then(|g| match g.status {
            crate::harness::GoalStatus::Active => Some("◇ goal".to_string()),
            crate::harness::GoalStatus::Paused => Some("◇ goal · paused".to_string()),
            _ => None,
        });
    let goal_len = goal_label.as_ref().map(|l| l.chars().count() + 3).unwrap_or(0);
    if let Some(l) = &goal_label {
        spans.push(Span::styled("· ", Style::default().fg(muted())));
        spans.push(Span::styled(
            format!("{l} "),
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        ));
    }
    // A self-update landed this session → a right-aligned "restart to apply" hint.
    let update = app.update_notice.lock().ok().and_then(|g| g.clone());
    let notice = update.map(|v| format!("⬆ updated to v{v} — restart to apply"));
    let notice_len = notice.as_ref().map(|n| n.chars().count() + 1).unwrap_or(0);

    // Fill the rest of the row with a thin rule for a clean header rather than a
    // bare glyph floating in empty space (leaving room for any update hint).
    let used = name.chars().count() + 3 + model.chars().count() + 1 + goal_len;
    let rule = (area.width as usize).saturating_sub(used + 1 + notice_len);
    if rule > 0 {
        spans.push(Span::styled("─".repeat(rule), Style::default().fg(faint())));
    }
    if let Some(notice) = notice {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(notice, Style::default().fg(accent())));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_history(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Inset the transcript with a symmetric gutter on BOTH sides so content sits
    // in a comfortable column instead of running edge-to-edge (which reads dense).
    let gutter = 4u16;
    let inner = Rect {
        x: area.x + gutter,
        y: area.y,
        width: area.width.saturating_sub(gutter * 2),
        height: area.height,
    };
    let width = (inner.width as usize).max(20);
    let height = inner.height as usize;

    // Empty state: no conversation yet (and not in the login form) — show a small
    // animated splash centered in the content area instead of a blank screen.
    let empty = !app.login_active
        && app
            .state
            .as_ref()
            .map_or(true, |s| s.events.is_empty() && s.user_request.trim().is_empty());
    if empty {
        let block = empty_state_lines(app.frame, width);
        let top = height.saturating_sub(block.len()) / 2;
        let mut lines: Vec<Line<'static>> = std::iter::repeat(Line::from("")).take(top).collect();
        lines.extend(block);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let lines = transcript_lines(app, width);

    let max_scroll = lines.len().saturating_sub(height);
    app.max_scroll.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);

    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let window = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(window), inner);
}

// --- Question panel (interactive ask_user picker) ---

/// The questions array of the active ask_user prompt, or empty when not waiting.
fn questions_of(app: &App) -> Vec<Value> {
    app.state
        .as_ref()
        .filter(|s| s.status == HarnessStatus::WaitingForInput)
        .and_then(|s| s.pending_question.as_ref())
        .and_then(|p| p.get("questions"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn q_text(question: &Value) -> String {
    question
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("(question)")
        .to_string()
}

/// Selectable options for a question as `(value, label)`. Empty = free-text
/// (the answer is typed in the input box instead of picked).
fn q_options(question: &Value) -> Vec<(String, String)> {
    let kind = question
        .get("answer_kind")
        .and_then(|k| k.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("free_text");
    let ak = question.get("answer_kind");
    let label_or = |k: &str, fallback: &str| {
        ak.and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    match kind {
        "single_choice" => ak
            .and_then(|a| a.get("choices"))
            .and_then(Value::as_array)
            .map(|cs| {
                cs.iter()
                    .map(|c| {
                        let value = c.get("value").and_then(Value::as_str).unwrap_or("").to_string();
                        let label = c
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| value.clone());
                        let value = if value.is_empty() { label.clone() } else { value };
                        (value, label)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "yes_no" => vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
        "confirm" => vec![
            ("confirm".into(), label_or("confirm_label", "Confirm")),
            ("cancel".into(), label_or("cancel_label", "Cancel")),
        ],
        _ => Vec::new(),
    }
}

/// Reset the picker cursor when a fresh question set arrives, and keep the
/// indices in range.
fn ensure_q_init(app: &mut App) {
    let qs = questions_of(app);
    // Fingerprint by ASK, not just question text: prefix with the count of
    // user_question events so the agent asking the SAME question twice in a row
    // still resets the picker (text alone left stale q_index/q_sel/q_answers).
    let asks = app
        .state
        .as_ref()
        .map(|s| {
            s.events
                .iter()
                .filter(|e| matches!(e, HarnessEvent::UserQuestion { .. }))
                .count()
        })
        .unwrap_or(0);
    let token = format!(
        "{asks}\u{1}{}",
        qs.iter().map(q_text).collect::<Vec<_>>().join("\u{1}")
    );
    if token != app.q_token {
        app.q_token = token;
        app.q_index = 0;
        app.q_sel = 0;
        app.q_answers.clear();
    }
    let len = qs.len().max(1);
    if app.q_index >= len {
        app.q_index = len - 1;
    }
    if let Some(q) = qs.get(app.q_index) {
        let opts = q_options(q);
        if !opts.is_empty() && app.q_sel >= opts.len() {
            app.q_sel = 0;
        }
    }
}

/// Intercept navigation/selection keys while an ask_user question is pending.
/// Returns true if the key was consumed. Free-text questions let typing fall
/// through to the input box (only Enter is intercepted, to commit the answer).
fn handle_question_key(app: &mut App, key: KeyEvent) -> bool {
    if pending_question_text(app).is_none() {
        return false;
    }
    ensure_q_init(app);
    let qs = questions_of(app);
    let q = qs.get(app.q_index.min(qs.len().saturating_sub(1))).cloned();
    let opts = q.as_ref().map(q_options).unwrap_or_default();
    let is_choice = !opts.is_empty();

    match key.code {
        KeyCode::Up if is_choice => {
            app.q_sel = if app.q_sel == 0 { opts.len() - 1 } else { app.q_sel - 1 };
            true
        }
        KeyCode::Down if is_choice => {
            app.q_sel = (app.q_sel + 1) % opts.len();
            true
        }
        KeyCode::Enter => {
            app.answer_current_question();
            true
        }
        // Swallow stray typing while a pure picker is focused; let Esc through to
        // the normal interrupt path.
        KeyCode::Char(_) | KeyCode::Backspace if is_choice => true,
        _ => false,
    }
}

fn pending_question_text(app: &App) -> Option<String> {
    let state = app.state.as_ref()?;
    if state.status != HarnessStatus::WaitingForInput {
        return None;
    }
    question_text(state.pending_question.as_ref()?)
}

fn question_text(pending: &Value) -> Option<String> {
    let questions = pending.get("questions").and_then(Value::as_array)?;
    let rendered = questions
        .iter()
        .filter_map(|q| q.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("  |  ");
    (!rendered.is_empty()).then_some(rendered)
}

fn question_height(app: &App) -> u16 {
    let qs = questions_of(app);
    let Some(q) = qs.get(app.q_index.min(qs.len().saturating_sub(1))) else {
        return 0;
    };
    let opts = q_options(q);
    // blank + question(+counter) + body + controls
    let body = if opts.is_empty() { 2 } else { opts.len() + 1 };
    ((2 + body).min(16)) as u16
}

/// Render the interactive picker: the current question, its options (or a
/// free-text hint), and a controls line. Sits pinned above the input box.
fn render_question(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let qs = questions_of(app);
    let Some(question) = qs.get(app.q_index.min(qs.len().saturating_sub(1))) else {
        return;
    };

    let accent = accent();
    let faint = faint();
    let dim = muted();
    let yellow = warn();

    let width = (area.width as usize).max(20);
    let counter = if qs.len() > 1 {
        format!("  ({}/{})", app.q_index + 1, qs.len())
    } else {
        String::new()
    };

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];

    let q_line = wrap_one(&q_text(question), width.saturating_sub(counter.len() + 3))
        .into_iter()
        .next()
        .unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(q_line, Style::default().fg(self::text()).add_modifier(Modifier::BOLD)),
        Span::styled(counter, Style::default().fg(faint)),
    ]));

    let opts = q_options(question);
    if opts.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", Style::default().fg(accent)),
            Span::styled("type your answer below, then press ↵", Style::default().fg(faint)),
        ]));
        lines.push(Line::from(Span::styled(
            "  ↵ submit · Esc cancel",
            Style::default().fg(faint),
        )));
    } else {
        let sel = app.q_sel.min(opts.len() - 1);
        for (i, (_value, label)) in opts.iter().enumerate() {
            let focused = i == sel;
            lines.push(Line::from(vec![
                Span::styled(
                    if focused { "  ▸ " } else { "    " },
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.clone(),
                    if focused {
                        Style::default().fg(self::text()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(dim)
                    },
                ),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "  ↑/↓ choose · ↵ select · Esc cancel",
            Style::default().fg(faint),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// The compact "📎 N attachments" summary shown above the prompt when files are
/// queued (they live outside the input text). None when there are none.
fn attachment_line(app: &App) -> Option<Line<'static>> {
    let n = app.attachments.len();
    if n == 0 {
        return None;
    }
    Some(Line::from(vec![
        Span::raw("   "),
        Span::styled(
            format!("📎 {n} attachment{}", if n == 1 { "" } else { "s" }),
            Style::default().fg(accent()),
        ),
        Span::styled("  ·  ⌫ removes", Style::default().fg(faint())),
    ]))
}

fn render_input(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));

    // While the login form is open, editing happens in the inline form above —
    // the input box just shows the controls.
    if app.login_active {
        let line = Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled(
                "Tab next · ←/→ change · Enter connect · Esc cancel",
                Style::default().fg(faint()),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    if app.input.is_empty() {
        // When a free-text ask_user question is pending, prompt for the answer.
        let placeholder = {
            let qs = questions_of(app);
            match qs.get(app.q_index.min(qs.len().saturating_sub(1))) {
                Some(q) if q_options(q).is_empty() => "Type your answer, then press ↵…",
                _ => "Type a prompt or a slash command…",
            }
        };
        let prompt = Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled(placeholder, Style::default().fg(faint())),
        ]);
        let mut lines = Vec::new();
        lines.extend(lane_lines(app));
        lines.extend(queued_lines(app));
        if let Some(a) = attachment_line(app) {
            lines.push(a);
        }
        lines.push(prompt);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Wrap the prompt into display rows (honoring explicit newlines) and draw a
    // block cursor at its (row, col). The prompt glyph takes the first 2 columns;
    // continuation rows are indented to match.
    let text_w = (area.width as usize).saturating_sub(3).max(1);
    let (rows, (cursor_row, cursor_col)) = layout_input(&app.input, app.input_cursor, text_w);
    let white = Style::default().fg(self::text());
    let cursor_style = Style::default().fg(Color::Black).bg(blue());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    let lanes_block = lane_lines(app);
    let queued = queued_lines(app);
    let mut attach_offset = lanes_block.len() + queued.len();
    lines.extend(lanes_block);
    lines.extend(queued);
    if let Some(a) = attachment_line(app) {
        lines.push(a);
        attach_offset += 1;
    }
    for (k, row) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        if k == 0 {
            spans.push(Span::styled(" ❯ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::raw("   "));
        }
        if k == cursor_row {
            let rchars: Vec<char> = row.chars().collect();
            let col = cursor_col.min(rchars.len());
            let before: String = rchars[..col].iter().collect();
            spans.push(Span::styled(before, white));
            if col < rchars.len() {
                spans.push(Span::styled(rchars[col].to_string(), cursor_style));
                let after: String = rchars[col + 1..].iter().collect();
                spans.push(Span::styled(after, white));
            } else {
                spans.push(Span::styled("█", Style::default().fg(blue())));
            }
        } else {
            spans.push(Span::styled(row.clone(), white));
        }
        lines.push(Line::from(spans));
    }

    // Keep the cursor row on screen when the input is taller than the box.
    let visible = (area.height as usize).saturating_sub(2).max(1);
    let scroll = (cursor_row + attach_offset).saturating_sub(visible.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Live lane status — running lanes listed above the prompt (same quiet style as
/// the queue block) so "what's out working right now, since when" is always
/// visible without digging through the transcript. Finished lanes don't linger
/// here; their completion rows live in the transcript.
fn lane_lines(app: &App) -> Vec<Line<'static>> {
    let Some(state) = &app.state else { return Vec::new() };
    let running: Vec<&crate::lanes::LaneRecord> = state
        .lanes
        .iter()
        .filter(|l| l.status == LaneStatus::Running)
        .collect();
    if running.is_empty() {
        return Vec::new();
    }
    let rail = Span::styled(" │ ", Style::default().fg(faint()));
    running
        .iter()
        .take(4)
        .map(|l| {
            let elapsed = chrono::DateTime::parse_from_rfc3339(&l.started_at)
                .ok()
                .map(|t| {
                    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0);
                    if secs < 60 {
                        format!("{secs}s")
                    } else {
                        format!("{}m", secs / 60)
                    }
                })
                .unwrap_or_default();
            let mut title: String = l.title.chars().take(48).collect();
            if l.title.chars().count() > 48 {
                title.push('…');
            }
            Line::from(vec![
                rail.clone(),
                Span::styled("◆ ", Style::default().fg(lane())),
                Span::styled(title, Style::default().fg(muted())),
                Span::styled(
                    format!(" — running {elapsed}"),
                    Style::default().fg(faint()),
                ),
            ])
        })
        .collect()
}

/// Messages held for after the current run — a quiet block above the prompt so
/// the user can SEE what will fire (and cancel with Ctrl+X). Header first, then
/// up to 3 previews behind a dim rail, then an overflow count. They send as ONE
/// combined message on idle (see flush_queued_input).
fn queued_lines(app: &App) -> Vec<Line<'static>> {
    let n = app.queued_inputs.len();
    if n == 0 {
        return Vec::new();
    }
    let rail = Span::styled(" │ ", Style::default().fg(faint()));
    let header = if n == 1 {
        "queued — sends when the run finishes · Ctrl+X to cancel".to_string()
    } else {
        format!("queued ({n}) — send together when the run finishes · Ctrl+X to cancel")
    };
    let mut lines = vec![Line::from(vec![
        rail.clone(),
        Span::styled(header, Style::default().fg(faint())),
    ])];
    for q in app.queued_inputs.iter().take(3) {
        let first = q.lines().next().unwrap_or("");
        let mut text: String = first.chars().take(72).collect();
        if first.chars().count() > 72 || q.lines().count() > 1 {
            text.push('…');
        }
        lines.push(Line::from(vec![
            rail.clone(),
            Span::styled(text, Style::default().fg(muted())),
        ]));
    }
    let extra = n.saturating_sub(3);
    if extra > 0 {
        lines.push(Line::from(vec![
            rail,
            Span::styled(format!("… +{extra} more"), Style::default().fg(faint())),
        ]));
    }
    lines
}

/// Height (incl. top/bottom borders) the input box needs for the current prompt,
/// clamped so it grows with wrapped/multi-line input but never dominates the view.
fn input_height(app: &App, width: u16) -> u16 {
    const MAX_ROWS: usize = 8;
    // One extra row for the "📎 N attachments" summary when files are queued.
    let attach: u16 = if app.attachments.is_empty() { 0 } else { 1 };
    // Queued-message preview rows (see queued_lines): header + up to 3 previews
    // + an overflow line when more are held.
    let qn = app.queued_inputs.len();
    let queued: u16 = if qn == 0 {
        0
    } else {
        (1 + qn.min(3) + usize::from(qn > 3)) as u16
    };
    // Live running-lane rows (see lane_lines), capped at 4.
    let lanes_h: u16 = app
        .state
        .as_ref()
        .map(|s| s.lanes.iter().filter(|l| l.status == LaneStatus::Running).count().min(4) as u16)
        .unwrap_or(0);
    let queued = queued + lanes_h;
    if app.input.is_empty() {
        return 3 + attach + queued;
    }
    let text_w = (width as usize).saturating_sub(3).max(1);
    let (rows, _) = layout_input(&app.input, app.input_cursor, text_w);
    (rows.len().clamp(1, MAX_ROWS) as u16) + 2 + attach + queued
}

/// Wrap `input` into display rows at `width` columns, honoring explicit `\n` as
/// hard breaks and soft-wrapping longer lines by character count. Returns the
/// rows plus the cursor's (row, col) so the caller can draw a block cursor.
fn layout_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let chars: Vec<char> = input.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    let mut cursor_rc = (0usize, 0usize);
    for (i, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            if cursor == i {
                cursor_rc = (rows.len(), col);
            }
            rows.push(std::mem::take(&mut cur));
            col = 0;
            continue;
        }
        if col == width {
            rows.push(std::mem::take(&mut cur));
            col = 0;
        }
        if cursor == i {
            cursor_rc = (rows.len(), col);
        }
        cur.push(*ch);
        col += 1;
    }
    if cursor >= chars.len() {
        cursor_rc = (rows.len(), col);
    }
    rows.push(cur);
    (rows, cursor_rc)
}

/// Render the compact inline login form: all fields on one panel, the focused
/// one highlighted. Editing is driven by `handle_login_key`.
fn login_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    if !app.login_active {
        return Vec::new();
    }

    let accent = accent();
    let dim = muted();
    let w = self::text();
    let faint = muted();
    let rule = self::faint();
    let focus = app.form_focus;

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));

    // Title rule
    let pad = width.min(56).saturating_sub(18).max(3);
    lines.push(Line::from(vec![
        Span::styled("── ", Style::default().fg(rule)),
        Span::styled("connect a model", Style::default().fg(accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}", "─".repeat(pad)), Style::default().fg(rule)),
    ]));
    lines.push(Line::from(""));

    // One labelled field row, focused or not, with caller-supplied value spans.
    let field_row = |label: &str, focused: bool, value: Vec<Span<'static>>| -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                if focused { "  ▸ " } else { "    " },
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{label:<12}"),
                Style::default()
                    .fg(if focused { w } else { dim })
                    .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
            ),
            Span::raw("  "),
        ];
        spans.extend(value);
        Line::from(spans)
    };

    // Adjustable (provider/model) value, wrapped in ‹ › when focused.
    let chooser = |text: String, focused: bool, suffix: &str| -> Vec<Span<'static>> {
        if focused {
            vec![
                Span::styled("‹ ", Style::default().fg(accent)),
                Span::styled(text, Style::default().fg(w).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" ›{suffix}"), Style::default().fg(accent)),
            ]
        } else {
            vec![Span::styled(format!("{text}{suffix}"), Style::default().fg(w))]
        }
    };

    // Provider
    let p_focus = focus == SettingsField::Provider;
    lines.push(field_row(
        "provider",
        p_focus,
        chooser(app.form_provider.clone(), p_focus, ""),
    ));

    // ChatGPT-subscription signs in via OAuth — no API key / base URL.
    if app.form_provider == "chatgpt" {
        let signed_in = crate::chatgpt_auth::is_signed_in();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ChatGPT account", Style::default().fg(w).add_modifier(Modifier::BOLD)),
            Span::styled("  ·  browser or device-code sign in", Style::default().fg(dim)),
        ]));
        if signed_in {
            lines.push(Line::from(Span::styled(
                "    ✓ signed in".to_string(),
                Style::default().fg(success()),
            )));
            lines.push(Line::from(Span::styled(
                "    Enter = use this account  ·  Ctrl-L = sign out".to_string(),
                Style::default().fg(faint),
            )));
        } else if let Some(info) = &app.chatgpt_device_code {
            lines.push(Line::from(Span::styled(
                format!("    Device code: {}", info.user_code),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("    Open {} to complete sign-in", info.verification_url),
                Style::default().fg(w),
            )));
            lines.push(Line::from(Span::styled(
                "    c = copy code  ·  u = copy URL  ·  Enter = browser sign-in".to_string(),
                Style::default().fg(faint),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "    Enter = sign in with browser  ·  Ctrl-D = device code".to_string(),
                Style::default().fg(accent),
            )));
        }
        lines.push(Line::from(""));
    } else {
        // API key (masked)
        let k_focus = focus == SettingsField::ApiKey;
        let key_len = app.form_api_key.chars().count();
        let key_val = if key_len == 0 {
            vec![Span::styled(
                if k_focus { "█".to_string() } else { "(required)".to_string() },
                Style::default().fg(if k_focus { accent } else { faint }),
            )]
        } else {
            let mut v = vec![Span::styled("•".repeat(key_len), Style::default().fg(w))];
            if k_focus {
                v.push(Span::styled("█", Style::default().fg(accent)));
            }
            v
        };
        lines.push(field_row("api key", k_focus, key_val));
    }

    // Base URL (openai-compatible only)
    if provider_needs_base_url(&app.form_provider) {
        let u_focus = focus == SettingsField::BaseUrl;
        let mut url_val = vec![Span::styled(
            app.form_base_url.clone(),
            Style::default().fg(if app.form_base_url.is_empty() { faint } else { w }),
        )];
        if u_focus {
            url_val.push(Span::styled("█", Style::default().fg(accent)));
        }
        lines.push(field_row("base url", u_focus, url_val));
    }

    // Model
    let m_focus = focus == SettingsField::Model;
    let model_text = if app.form_model.is_empty() {
        "(pick one)".to_string()
    } else {
        app.form_model.clone()
    };
    lines.push(field_row("model", m_focus, chooser(model_text, m_focus, " ▾")));

    // Reasoning / thinking effort
    let r_focus = focus == SettingsField::Reasoning;
    let reasoning = app
        .form_reasoning_effort
        .clone()
        .unwrap_or_else(|| "medium".to_string());
    let reasoning_hint = match app.form_provider.as_str() {
        "anthropic" | "anthropic-compatible" => "thinking",
        "gemini" => "thinking",
        _ => "reasoning",
    };
    lines.push(field_row(
        reasoning_hint,
        r_focus,
        chooser(reasoning, r_focus, ""),
    ));

    let cw_focus = focus == SettingsField::ContextWindow;
    let mut cw_val = vec![Span::styled(
        app.form_context_window.clone(),
        Style::default().fg(if app.form_context_window.is_empty() { faint } else { w }),
    )];
    if cw_focus {
        cw_val.push(Span::styled("█", Style::default().fg(accent)));
    }
    lines.push(field_row("context", cw_focus, cw_val));

    let cp_focus = focus == SettingsField::Compaction;
    lines.push(field_row(
        "compact at",
        cp_focus,
        chooser(format!("{}%", app.form_compact_at_pct.trim()), cp_focus, ""),
    ));

    // Model dropdown — shown while the Model field is focused.
    if m_focus {
        let rows = login_model_rows(app);
        if rows.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "          (no suggestions — type a model id)",
                Style::default().fg(faint),
            )]));
        } else {
            for row in rows {
                let is_cur = row == app.form_model;
                lines.push(Line::from(vec![
                    Span::styled(
                        if is_cur { "          ▸ " } else { "            " },
                        Style::default().fg(accent),
                    ),
                    Span::styled(
                        row,
                        if is_cur {
                            Style::default().fg(w)
                        } else {
                            Style::default().fg(dim)
                        },
                    ),
                ]));
            }
        }
    }

    if !app.models_fetch_status.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("    {}", app.models_fetch_status),
            Style::default().fg(faint),
        )]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "    Context window = max prompt budget. Compaction starts near {}% of it.",
            app.form_compact_at_pct.trim()
        ),
        Style::default().fg(faint),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(width.min(56)),
        Style::default().fg(rule),
    )]));
    lines.push(Line::from(""));
    lines
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // cwd on the left, token accounting on the right. The model now lives in the
    // header, so it's dropped here. Arrow glyphs (↑ ↓ ↻) are width-1 everywhere;
    // no emoji (they spill a cell and misalign the row).
    let st = app.state.as_ref();
    let dim = Style::default().fg(muted());
    let faint = Style::default().fg(faint());

    let mut right: Vec<Span<'static>> = Vec::new();
    // ChatGPT-subscription usage is account-wide — show the GLOBAL snapshot
    // whenever signed in, regardless of which chat (or provider) is in view, so
    // it never disappears just because the current chat hasn't hit ChatGPT.
    let rl = crate::chatgpt_auth::is_signed_in()
        .then(|| app.global_usage.as_ref())
        .flatten();
    if let Some(rl) = rl {
        let mut parts = Vec::new();
        for w in [rl.primary.as_ref(), rl.secondary.as_ref()].into_iter().flatten() {
            let left = (100.0 - w.used_percent).clamp(0.0, 100.0);
            parts.push(format!("{} {:.0}%", rate_window_label(w.window_minutes), left));
        }
        if !parts.is_empty() {
            right.push(Span::styled(format!("◷ {} left", parts.join(" · ")), Style::default().fg(warn())));
            right.push(Span::styled("   ", faint));
        }
    }
    right.extend([
        Span::styled(format!("↑{}", fmt_si(st.map(|s| s.prompt_tokens).unwrap_or(0))), dim),
        Span::raw(" "),
        Span::styled(format!("↓{}", fmt_si(st.map(|s| s.completion_tokens).unwrap_or(0))), dim),
    ]);
    let running_lanes = st
        .map(|s| s.lanes.iter().filter(|lane| lane.status == LaneStatus::Running).count())
        .unwrap_or(0);
    if running_lanes > 0 {
        right.push(Span::styled(format!("  ◆{}", running_lanes), Style::default().fg(lane())));
    }
    // Active file watches (`monitor`) — the agent is listening for appends.
    let watching = st.map(|s| s.watches.len()).unwrap_or(0);
    if watching > 0 {
        right.push(Span::styled(format!("  ◉{}", watching), Style::default().fg(warn())));
    }
    let cache = st.map(|s| s.cache_read_tokens).unwrap_or(0);
    if cache > 0 {
        right.push(Span::styled(format!(" ↻{}", fmt_si(cache)), dim));
    }
    right.push(Span::styled(" · ", faint));
    right.push(Span::styled(
        format!("ctx {} ", fmt_si(st.map(|s| s.last_prompt_tokens).unwrap_or(0))),
        dim,
    ));

    let mut left = vec![Span::styled(format!(" {}", app.cwd_display), dim)];
    if app.is_manual_mode() {
        left.push(Span::styled("  ⚠ manual", Style::default().fg(warn())));
    }
    let right_line = Line::from(right);
    let right_w = right_line.width() as u16;

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w + 1)])
        .split(area);

    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(right_line).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

/// Short label for a rate-limit window length (minutes), ±5% tolerance.
fn rate_window_label(minutes: i64) -> String {
    let near = |t: i64| (minutes - t).abs() as f64 <= (t as f64) * 0.05;
    if near(300) {
        "5h".to_string()
    } else if near(1440) {
        "daily".to_string()
    } else if near(10080) {
        "wk".to_string()
    } else if near(43200) {
        "mo".to_string()
    } else if minutes > 0 {
        format!("{}h", (minutes / 60).max(1))
    } else {
        "limit".to_string()
    }
}

/// SI-ish formatting for token counts: 91M, 425k, 128k, 512.
fn fmt_si(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.0}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Canonicalize the workspace path and abbreviate the home dir to `~`.
fn home_path(path: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty()
            && let Some(rest) = text.strip_prefix(home.as_ref())
        {
            return format!("~{rest}");
        }
    }
    text
}

fn get_provider_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &[
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3",
            "gpt-5.3-codex",
            "gpt-4o",
        ],
        "anthropic" => &[
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "claude-fable-5",
        ],
        "gemini" => &[
            "gemini-3.5-flash",
            "gemini-3.1-pro",
            "gemini-3.1-flash-lite",
            "gemini-3-pro",
        ],
        "openrouter" => &[
            "anthropic/claude-opus-4-8",
            "anthropic/claude-sonnet-4-6",
            "google/gemini-3.1-pro",
            "deepseek/deepseek-v4-pro",
            "qwen/qwen3-coder",
            "openai/gpt-5.5",
        ],
        // openai-compatible points at an arbitrary endpoint — there are no
        // sensible static suggestions; the model list is fetched from its
        // /models endpoint (or typed by the user).
        // ChatGPT-subscription Codex backend (OAuth, no key). The 5.6 family
        // requires the codex client-identity headers (see chatgpt.rs).
        "chatgpt" => &[
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.1-codex",
            "gpt-5.4-mini",
        ],
        "openai-compatible" => &[],
        _ => &[],
    }
}

async fn fetch_models_from_provider(
    provider: String,
    api_key: String,
    base_url: String,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    match provider.as_str() {
        "openai" => {
            let url = "https://api.openai.com/v1/models";
            let res = client.get(url)
                .bearer_auth(api_key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "anthropic" | "anthropic-compatible" => {
            let raw = if provider == "anthropic-compatible" && !base_url.trim().is_empty() {
                base_url.trim().trim_end_matches('/').to_string()
            } else {
                "https://api.anthropic.com".to_string()
            };
            // Tolerate a base that already includes /v1 (matches the messages URL logic).
            let url = if raw.ends_with("/v1") {
                format!("{raw}/models")
            } else {
                format!("{raw}/v1/models")
            };
            let res = client.get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "gemini" => {
            let url = format!("https://generativelanguage.googleapis.com/v1beta/models?key={}", api_key);
            let res = client.get(&url)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("models").and_then(|m| m.as_array()) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        let stripped = name.strip_prefix("models/").unwrap_or(name);
                        models.push(stripped.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "openrouter" => {
            let url = "https://openrouter.ai/api/v1/models";
            let mut req = client.get(url);
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key);
            }
            let res = req.send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "openai-compatible" => {
            let mut url = base_url;
            if !url.ends_with("/models") {
                if url.ends_with('/') {
                    url.push_str("models");
                } else {
                    url.push_str("/models");
                }
            }
            let mut req = client.get(&url);
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key);
            }
            let res = req.send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            } else if let Some(arr) = data.get("models").and_then(|m| m.as_array()) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        models.push(name.to_string());
                    }
                }
            } else if let Some(arr) = data.as_array() {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    } else if let Some(s) = item.as_str() {
                        models.push(s.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        _ => Err(format!("Unsupported provider: {}", provider)),
    }
}




