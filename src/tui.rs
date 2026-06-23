use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
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
use ratatui::widgets::Paragraph;
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::builtins::coding_tools;
use crate::config::SnippetConfig;
use crate::harness::{
    CodingHarness, HarnessConfig, HarnessEvent, HarnessState, HarnessStatus, LoopInput,
};
use crate::lanes::{LaneStatus, ModelFactory};
use crate::prompts::conversation_system_prompt;
use crate::tools::ToolContext;

/// Meta tools render through their own dedicated events (Note / AssistantText /
/// UserQuestion / LaneSpawned), so their raw tool-call/result rows are hidden to
/// avoid duplication.
const HIDDEN_TOOL_ROWS: [&str; 5] =
    ["terminate_loop", "note", "notify_user", "ask_user", "delegate_task"];

/// Cap on bash/output preview lines shown inline before collapsing to a count.

const ALL_COMMANDS: &[(&str, &str)] = &[
    ("/new", "Start a new session"),
    ("/resume", "Resume a saved session"),
    ("/rewind", "Restore the workspace to a checkpoint"),
    ("/model", "Connect or change the AI model"),
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

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<TuiTerminal, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
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
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableBracketedPaste)?;
    terminal.show_cursor()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    ResumeSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Provider,
    ApiKey,
    Model,
    BaseUrl,
}

/// Providers offered by the login form, in display order.
const LOGIN_PROVIDERS: &[&str] = &["openai", "anthropic", "gemini", "openrouter", "openai-compatible"];

/// Single source of truth for a provider's default base URL and model.
fn provider_defaults(provider: &str) -> (String, String) {
    match provider {
        "openai" => ("https://api.openai.com/v1".to_string(), "gpt-5.5".to_string()),
        "anthropic" => (String::new(), "claude-opus-4-8".to_string()),
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

/// The model candidates shown in the login form's picker dropdown: the
/// live-fetched list when available, else the static fallback (capped).
fn login_model_rows(app: &App) -> Vec<String> {
    match &app.form_fetched_models {
        Some(fetched) => fetched.iter().take(6).cloned().collect(),
        None => get_provider_models(&app.form_provider)
            .iter()
            .take(6)
            .map(|s| s.to_string())
            .collect(),
    }
}

struct App {
    options: TuiOptions,
    input: String,
    /// Cursor position in `input`, as a CHAR index (0..=char_count).
    input_cursor: usize,
    /// Collapsed pastes: (placeholder shown in the input, real content). A big
    /// paste shows as a compact chip and expands back on send.
    pasted_blocks: Vec<(String, String)>,
    status: String,
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
    form_provider: String,
    form_api_key: String,
    form_model: String,
    form_model_query: String,
    form_base_url: String,
    form_focus: SettingsField,
    form_fetched_models: Option<Vec<String>>,
    models_fetch_handle: Option<tokio::task::JoinHandle<Result<Vec<String>, String>>>,
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
            pasted_blocks: Vec::new(),
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
            form_provider: String::new(),
            form_api_key: String::new(),
            form_model: String::new(),
            form_model_query: String::new(),
            form_base_url: String::new(),
            form_focus: SettingsField::Provider,
            form_fetched_models: None,
            models_fetch_handle: None,
            models_fetch_status: String::new(),
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

        if app.options.config.model.api_key.trim().is_empty() {
            app.status = "No model connected yet — type /model to connect one.".to_string();
        }

        app
    }

    /// Open the compact inline login form, seeding the shared form fields from
    /// the current config and focusing the provider selector.
    fn open_login(&mut self) {
        self.original_config = Some(self.options.config.clone());
        self.init_settings_form();
        self.form_focus = SettingsField::Provider;
        self.login_active = true;
        self.input_clear();
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
        // When closing login during an active session, don't interrupt the conversation
        // Only close the login form and preserve the current session
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
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
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

    fn list_conversations(&self) -> Vec<(String, String)> {
        let dir = self.conversations_dir();
        let mut list = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
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
                            if !state.user_request.is_empty() {
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
            if let Ok(bytes) = std::fs::read(default_path) {
                if let Ok(state) = crate::harness::deserialize_state(&bytes) {
                    if !state.user_request.is_empty() {
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
            list.push(("default".to_string(), desc, mod_time, relative));
        }

        list.sort_by(|a, b| b.2.cmp(&a.2));

        list.into_iter()
            .map(|(name, desc, _, relative)| {
                let short_desc = if desc.len() > 40 {
                    format!("{}...", &desc[..37])
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
                    parts[1].to_string()
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
                    // (in switch_conversation), so it works even mid-run.
                    Some(name) => self.switch_conversation(&name),
                    None => {
                        // No target = resume the current session. If one is already
                        // running, it's already resumed — nothing to do.
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
                self.open_login();
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
            other => {
                self.status = format!("Unknown command: {}. Type /new, /resume, /rewind, or /model.", other);
            }
        }
    }

    /// Tab order of the login form fields (Base URL only for openai-compatible).
    fn login_focus_order(&self) -> Vec<SettingsField> {
        let mut order = vec![SettingsField::Provider, SettingsField::ApiKey];
        if self.form_provider == "openai-compatible" {
            order.push(SettingsField::BaseUrl);
        }
        order.push(SettingsField::Model);
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

        if self.form_focus == SettingsField::Model
            && self.form_fetched_models.is_none()
            && !self.form_api_key.trim().is_empty()
        {
            self.trigger_models_fetch();
        }
    }

    /// `←`/`→` on the focused field: cycle provider or model.
    fn login_adjust(&mut self, forward: bool) {
        match self.form_focus {
            SettingsField::Provider => self.change_provider(forward),
            SettingsField::Model => self.login_cycle_model(forward),
            _ => {}
        }
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
    }

    /// Type a character into the focused text field.
    fn login_edit_char(&mut self, c: char) {
        match self.form_focus {
            SettingsField::ApiKey => self.form_api_key.push(c),
            SettingsField::BaseUrl => self.form_base_url.push(c),
            SettingsField::Model => self.form_model.push(c),
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
            SettingsField::Model => self.form_model.push_str(cleaned),
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
            }
            _ => {}
        }
    }

    /// Validate the form and connect: persist the config and close the form.
    fn login_connect(&mut self) {
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
        match self.save_settings_to_file() {
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

        std::fs::write(&self.options.config_path, toml_str)
            .map_err(|e| format!("failed to write config: {e}"))?;
        Ok(())
    }

    fn save_settings_to_file(&mut self) -> Result<(), String> {
        let mut model_config = self.options.config.model.clone();

        model_config.provider = self.form_provider.clone();
        model_config.api_key = self.form_api_key.clone();
        model_config.model = self.form_model.clone();
        model_config.base_url = self.form_base_url.clone();

        model_config.context_window = match self.form_provider.as_str() {
            "anthropic" => 200_000,
            "gemini" => 1_000_000,
            _ => 128_000,
        };

        self.options.config.model = model_config;

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
        self.agent_alive()
            && self.state.as_ref().is_some_and(|s| {
                s.status == HarnessStatus::Running
                    || s.lanes.iter().any(|l| l.status == LaneStatus::Running)
            })
    }

    fn model_factory(&self) -> ModelFactory {
        let model_config = self.options.config.model.clone();
        Arc::new(move || {
            model_config.build_model()
        })
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
            let typed = self.expand_input();
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
    /// send, so the agent can `read_image` it. macOS-only (uses `osascript`).
    /// Multiple screenshots accumulate as separate chips.
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
        let ran = std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output();
        let ok = matches!(&ran, Ok(out) if out.status.success())
            && std::fs::metadata(&dest).map(|m| m.len() > 0).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&dest);
            self.status = "No image on the clipboard — copy a screenshot first.".to_string();
            return;
        }
        let n = self.pasted_blocks.len() + 1;
        let marker = format!("[Image #{n}: screenshot]");
        for c in marker.chars() {
            self.input_insert(c);
        }
        // On send the chip expands to the temp path; the agent reads it via read_image.
        self.pasted_blocks.push((marker, dest.display().to_string()));
        self.status = String::new();
    }

    /// Expand any paste chips in the current input back to their real content.
    fn expand_input(&self) -> String {
        let mut out = self.input.clone();
        for (marker, content) in &self.pasted_blocks {
            out = out.replace(marker, content);
        }
        out
    }

    fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
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

        let text = self.expand_input();
        let text = text.trim().to_string();
        if text.is_empty() {
            if !self.agent_alive() {
                self.status = "Enter a task before starting.".to_string();
            }
            return;
        }
        self.input_clear();
        self.scroll = 0;

        if text.starts_with('/') {
            self.handle_slash_command(&text);
            return;
        }

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
            }
        } else {
            // Resume the existing conversation rather than starting fresh — after an
            // interrupt the agent has died but the transcript is intact; resume=false
            // would clobber it into a new conversation.
            self.spawn_loop(Some(text), true);
        }
    }

    fn spawn_loop(&mut self, initial: Option<String>, resume: bool) {
        if self.agent_alive() {
            return;
        }
        self.error = None;
        self.scroll = 0;
        // Don't announce activity in the footer — the in-transcript spinner is the
        // live indicator, and the resident loop never "finishes" between turns so a
        // footer label here would just go stale.
        self.status = String::new();

        let (tx, rx) = mpsc::unbounded_channel();
        self.input_tx = Some(tx);

        let workspace = self.options.config.workspace.clone();
        let state_path = self.active_state_path.clone();
        let model_config = self.options.config.model.clone();
        let exa_api_key = self.options.config.exa_api_key.clone();
        let factory = self.model_factory();
        crate::llm::StreamBuffer::clear(&self.stream);
        let stream = self.stream.clone();

        self.agent = Some(tokio::spawn(async move {
            let mut model = model_config.build_model();
            let context = ToolContext::new(workspace).map_err(|error| error.to_string())?;
            let harness = CodingHarness::new(
                HarnessConfig {
                    system_prompt: conversation_system_prompt(),
                    state_path: Some(state_path),
                    resume,
                    exa_api_key: exa_api_key.clone(),
                    ..HarnessConfig::default()
                },
                coding_tools(exa_api_key),
                context,
            );
            harness
                .run_interactive(&mut model, initial, rx, Some(factory), Some(stream))
                .await
                .map_err(|error| error.to_string())
        }));
    }

    fn interrupt_or_quit(&mut self) {
        if self.agent_alive() {
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
    app.refresh_state().await;
    if app.options.config.resume_on_start || app.options.resume.is_some() {
        app.spawn_loop(None, true);
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

    if app.screen == Screen::ResumeSelection {
        let convs = app.list_conversations();
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

        match key.code {
            KeyCode::Up => {
                app.resume_selected_index = if app.resume_selected_index == 0 {
                    convs.len() - 1
                } else {
                    app.resume_selected_index - 1
                };
            }
            KeyCode::Down => {
                app.resume_selected_index = (app.resume_selected_index + 1) % convs.len();
            }
            KeyCode::Enter => {
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
            KeyCode::Up => app.scroll_up(1),
            KeyCode::Down => app.scroll_down(1),
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

/// Key handling for the compact inline login form. Tab/↑/↓ move between fields,
/// ←/→ change the provider or model, typing edits the focused text field, Enter
/// connects, Esc cancels.
fn handle_login_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.close_login(true);
            app.status = String::new();
        }
        KeyCode::Enter => app.login_connect(),
        KeyCode::Tab | KeyCode::Down => app.login_move_focus(true),
        KeyCode::BackTab | KeyCode::Up => app.login_move_focus(false),
        KeyCode::Left => app.login_adjust(false),
        KeyCode::Right => app.login_adjust(true),
        KeyCode::Backspace => app.login_backspace(),
        KeyCode::Char(c) => app.login_edit_char(c),
        _ => {}
    }
}

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();

    if app.screen == Screen::ResumeSelection {
        render_resume_selection(frame, area, app);
        return;
    }

    let sugg_h = suggestion_height(app);
    let input_h = input_height(app, area.width);

    // Vertical split: Header (1), Content (Min 10), Suggestions (0-8), Question (0-2), Input (grows), Status Message (1), Footer (1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // Header
            Constraint::Min(10),                     // Content
            Constraint::Length(sugg_h),              // Suggestions
            Constraint::Length(question_height(app)), // Question
            Constraint::Length(input_h),            // Input (grows with wrapped lines)
            Constraint::Length(1),                  // Status message
            Constraint::Length(1),                  // Footer (metadata)
        ])
        .split(area);
        
    let header_area = chunks[0];
    let content_area = chunks[1];
    let suggestions_area = chunks[2];
    let question_area = chunks[3];
    let input_area = chunks[4];
    let status_msg_area = chunks[5];
    let footer_area = chunks[6];

    render_header(frame, header_area, app);
    render_history(frame, content_area, app);
    if sugg_h > 0 {
        render_suggestions(frame, suggestions_area, app);
    }
    render_question(frame, question_area, app);
    render_input(frame, input_area, app);
    render_status_message(frame, status_msg_area, app);
    render_status(frame, footer_area, app);
}

fn render_resume_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};

    let convs = app.list_conversations();

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
        Span::styled("  |  Select a session to resume", Style::default().fg(TEXT)),
    ];
    frame.render_widget(Paragraph::new(Line::from(header_text)), chunks[0]);

    // Render List
    let mut lines = Vec::new();
    if convs.is_empty() {
        lines.push(Line::from(Span::styled("  No saved conversations found.", subtle())));
    } else {
        let selected_idx = app.resume_selected_index.min(convs.len().saturating_sub(1));
        for (idx, (name, desc)) in convs.iter().enumerate() {
            let is_selected = idx == selected_idx;

            let line = if is_selected {
                Line::from(vec![
                    Span::styled(format!("  ➤ {:<38} ", name), Style::default().fg(Color::Black).add_modifier(Modifier::BOLD)),
                    Span::styled(desc.to_string(), Style::default().fg(Color::Rgb(30, 41, 59))),
                ]).style(Style::default().bg(blue()))
            } else {
                Line::from(vec![
                    Span::styled(format!("    {:<38} ", name), Style::default().fg(blue())),
                    Span::styled(desc.to_string(), subtle()),
                ])
            };
            lines.push(line);
        }
    }

    let list_block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 40)));

    frame.render_widget(Paragraph::new(lines).block(list_block), chunks[1]);

    // Render Footer
    let footer_style = Style::default().fg(MUTED);
    let footer_text = "↑/↓ scroll  ·  Enter resume selected  ·  Esc go back";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer_text, footer_style))),
        chunks[2]
    );
}

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

        let line = if is_selected {
            Line::from(vec![
                Span::styled(format!("  ➤ {:<9} ", cmd), Style::default().fg(Color::Black).add_modifier(Modifier::BOLD)),
                Span::styled(desc.to_string(), Style::default().fg(Color::Rgb(30, 41, 59))),
            ]).style(Style::default().bg(blue()))
        } else {
            Line::from(vec![
                Span::styled(format!("    {:<9} ", cmd), Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
                Span::styled(desc.to_string(), subtle()),
            ])
        };
        lines.push(line);
    }

    use ratatui::widgets::{Block, Borders};
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 40)));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_status_message(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = if let Some(ref err) = app.error {
        Line::from(vec![
            Span::styled("error: ", Style::default().fg(DANGER).add_modifier(Modifier::BOLD)),
            Span::styled(err.to_string(), Style::default().fg(DANGER)),
        ])
    } else {
        Line::from(vec![
            Span::styled(&app.status, Style::default().fg(Color::Gray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let model = app.options.config.model.model.clone();
    let name = " snipett";
    let mut spans = vec![
        Span::styled(name, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", Style::default().fg(MUTED)),
        Span::styled(model.clone(), Style::default().fg(MUTED)),
        Span::raw(" "),
    ];
    // Fill the rest of the row with a thin rule for a clean header rather than a
    // bare glyph floating in empty space.
    let used = name.chars().count() + 3 + model.chars().count() + 1;
    let rule = (area.width as usize).saturating_sub(used + 1);
    if rule > 0 {
        spans.push(Span::styled("─".repeat(rule), Style::default().fg(FAINT)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_history(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Inset the transcript by a 2-column left margin for a consistent gutter (the
    // header/footer/input sit at a 1-column margin, so content reads as nested).
    let inner = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let width = (inner.width as usize).saturating_sub(1).max(20);
    let height = inner.height as usize;
    let lines = transcript_lines(app, width);

    let max_scroll = lines.len().saturating_sub(height);
    app.max_scroll.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);

    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let window = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(window), inner);
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(state) = &app.state else {
        // No session yet — keep the transcript empty (the status bar carries the
        // hint). Only the login form shows here, and only when it's open.
        return login_lines(app, width);
    };

    let mut lines = Vec::new();
    let has_user_events = state
        .events
        .iter()
        .any(|event| matches!(event, HarnessEvent::UserInput { .. }));

    // Blank line *between* blocks, but a run of consecutive tool rows (a call and
    // its result, then the next call…) packs tightly with no gaps — so a burst of
    // reads/greps collapses instead of spreading down the screen. Prose still gets
    // breathing room before and after a tool run.
    let mut first = true;
    let mut prev_tool_row = false;

    if !has_user_events && !state.user_request.is_empty() {
        lines.extend(user_lines(&state.user_request, width));
        first = false;
    }

    let mut events = state.events.iter().peekable();
    while let Some(event) = events.next() {
        // Collapse a run of consecutive model errors (transient retries) into a
        // single line with a count, so a retry storm doesn't flood the screen.
        if let HarnessEvent::ModelError { message } = event {
            let mut last = message.clone();
            let mut count = 1usize;
            while let Some(HarnessEvent::ModelError { message: next }) = events.peek() {
                last = next.clone();
                count += 1;
                events.next();
            }
            if count > 1 {
                last = format!("{last}  (×{count})");
            }
            if !first {
                lines.push(Line::from(""));
            }
            lines.extend(marker_block("✗ ", "", DANGER, &last, width));
            prev_tool_row = false;
            first = false;
            continue;
        }

        // Tool call: render `● Verb  arg` and, when the next event is a one-line
        // result for it, merge that summary onto the same row, right-aligned.
        if let HarnessEvent::ToolCall { tool_name, arguments } = event {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                // Drop the paired hidden result too, so no orphan row renders.
                if let Some(HarnessEvent::ToolResult { tool_name: rn, .. }) = events.peek() {
                    if HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                        events.next();
                    }
                }
                continue;
            }
            let mut call_lines = tool_call_lines(tool_name, arguments, width);
            // The result is pushed right after the call, so a call with NO following
            // result is the in-flight one (persisted just before execution). When the
            // result is present and one-line, merge it onto the row; when it's still
            // running, show a live spinner so a slow tool isn't a black box.
            let result_follows = matches!(events.peek(), Some(HarnessEvent::ToolResult { .. }));
            if result_follows {
                if call_lines.len() == 1 {
                    if let Some(HarnessEvent::ToolResult { tool_name: rn, result }) = events.peek() {
                        if !HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                            if let Some(summary) = tool_result_oneliner(rn, result) {
                                let pad = width
                                    .saturating_sub(call_lines[0].width() + summary.chars().count());
                                if pad >= 2 {
                                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                                    call_lines[0]
                                        .spans
                                        .push(Span::styled(summary, Style::default().fg(MUTED)));
                                    events.next();
                                }
                            }
                        }
                    }
                }
            } else if state.status == HarnessStatus::Running {
                let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
                let label = format!("{spinner} running");
                let style = Style::default().fg(ACCENT);
                if call_lines.len() == 1
                    && width > call_lines[0].width() + label.chars().count() + 2
                {
                    let pad = width - call_lines[0].width() - label.chars().count();
                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                    call_lines[0].spans.push(Span::styled(label, style));
                } else {
                    call_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, style),
                    ]));
                }
            }
            if !first && !prev_tool_row {
                lines.push(Line::from(""));
            }
            lines.extend(call_lines);
            prev_tool_row = true;
            first = false;
            continue;
        }

        let rendered = event_lines(event, width);
        if rendered.is_empty() {
            continue;
        }
        let is_tool_row = matches!(
            event,
            HarnessEvent::ToolCall { .. }
                | HarnessEvent::ToolResult { .. }
                | HarnessEvent::InvalidToolCall { .. }
        );
        if !first && !(prev_tool_row && is_tool_row) {
            lines.push(Line::from(""));
        }
        lines.extend(rendered);
        prev_tool_row = is_tool_row;
        first = false;
    }

    // Reasoning/thinking the model returned, shown dimmed and distinct from the
    // answer. DEBUG: rendered whenever present (not just while working) and never
    // cleared, while experimenting with the thinking display.
    let thinking = crate::llm::StreamBuffer::snapshot_thinking(&app.stream);
    let thinking = thinking.trim_end();
    if !thinking.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        for seg in wrap_one(thinking, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(seg, Style::default().fg(MUTED))));
        }
    }

    // Live "working…" feedback at the tail while the agent is processing (or a lane is).
    let working = state.status == HarnessStatus::Running
        || state.lanes.iter().any(|lane| lane.status == LaneStatus::Running);
    if working && app.agent_alive() {
        // Text the model is streaming this turn, shown live until it commits to a
        // durable AssistantText event (then refresh_state clears the buffer).
        let live = crate::llm::StreamBuffer::snapshot(&app.stream);
        let live = live.trim_end();
        if !live.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(render_prose(live, width));
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("working…", subtle()),
        ]));
    }
    // Append inline login Q&A if active
    lines.extend(login_lines(app, width));
    lines
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Map one event to a block of styled, wrapped lines. Empty = hidden.
fn event_lines(event: &HarnessEvent, width: usize) -> Vec<Line<'static>> {
    match event {
        HarnessEvent::UserInput { text } => user_lines(text, width),
        HarnessEvent::Steer { text } => {
            marker_block("↳ ", "steer  ", ACCENT, text, width)
        }
        HarnessEvent::AssistantText { text } => render_prose(text, width),
        HarnessEvent::Note { entry } => marker_block("✎ ", "note  ", MUTED, entry, width),
        HarnessEvent::SystemDecision { step, reasoning } => marker_block(
            "⚙ ",
            "",
            WARN,
            &format!("{step} — {reasoning}"),
            width,
        ),
        HarnessEvent::ModelError { message } => {
            marker_block("✗ ", "", DANGER, message, width)
        }
        HarnessEvent::UserQuestion { questions } => {
            let text = question_text(questions).unwrap_or_else(|| "(question)".to_string());
            marker_block("? ", "", WARN, &text, width)
        }
        HarnessEvent::LaneSpawned { id, title } => marker_block(
            "→ ",
            "",
            LANE,
            &format!("delegated {id}: {title}"),
            width,
        ),
        HarnessEvent::LaneCompleted {
            id,
            title,
            status,
            summary,
        } => lane_completed_lines(id, title, *status, summary.as_deref(), width),
        HarnessEvent::ToolCall {
            tool_name,
            arguments,
        } => {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                return Vec::new();
            }
            tool_call_lines(tool_name, arguments, width)
        }
        HarnessEvent::ToolResult { tool_name, result } => {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                return Vec::new();
            }
            tool_result_lines(tool_name, result, width)
        }
        HarnessEvent::InvalidToolCall { tool_name, error } => result_block(
            vec![(format!("✗ {tool_name}: {error}"), Style::default().fg(DANGER))],
            width,
        ),
    }
}

fn user_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let prefix = Span::styled(
        "› ",
        Style::default().fg(blue()).add_modifier(Modifier::BOLD),
    );
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(text, width.saturating_sub(2)).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                prefix.clone(),
                Span::styled(seg, Style::default().fg(TEXT)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(TEXT)),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![prefix]));
    }
    lines
}

/// A leading glyph + optional label, then wrapped body text in one color.
fn marker_block(
    glyph: &str,
    label: &str,
    color: Color,
    text: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let glyph_w = glyph.chars().count() + label.chars().count();
    let body_style = Style::default().fg(color);
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(text, width.saturating_sub(glyph_w))
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            let mut spans = vec![Span::styled(
                glyph.to_string(),
                body_style.add_modifier(Modifier::BOLD),
            )];
            if !label.is_empty() {
                spans.push(Span::styled(
                    label.to_string(),
                    body_style.add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(seg, body_style));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(glyph_w)),
                Span::styled(seg, body_style),
            ]));
        }
    }
    lines
}

fn lane_completed_lines(
    id: &str,
    title: &str,
    status: LaneStatus,
    summary: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let (tag, color) = match status {
        LaneStatus::Completed => ("done", SUCCESS),
        LaneStatus::Failed => ("failed", DANGER),
        LaneStatus::Running => ("running", LANE),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("◆ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("lane {id} · {title} "),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{tag}]"), Style::default().fg(color)),
    ])];
    if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
        lines.extend(result_block(
            vec![(summary.to_string(), subtle())],
            width,
        ));
    }
    lines
}

fn tool_call_lines(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let mut lines = tool_call_head_lines(tool_name, arguments, width);
    lines.extend(tool_call_preview(tool_name, arguments, width));
    lines
}

/// Render just the call header row(s): `● Verb  arg`, the verb in bold body text
/// and the argument muted, wrapping the argument under a hanging indent.
fn tool_call_head_lines(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let (verb, arg) = tool_call_parts(tool_name, arguments);
    let head = format!("{verb}  ");
    let indent = 2 + head.chars().count();
    let arg_budget = width.saturating_sub(indent).max(8);

    if arg.trim().is_empty() {
        return vec![Line::from(vec![
            Span::styled("● ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(verb, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        ])];
    }

    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&arg, arg_budget).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(verb.clone(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(MUTED)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(seg, Style::default().fg(MUTED)),
            ]));
        }
    }
    lines
}

/// A preview of what the call will do — content for writes, a +/- diff for edits.
fn tool_call_preview(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    let green = Style::default().fg(SUCCESS);
    let red = Style::default().fg(DANGER);
    const MAX: usize = 8;

    let mut items: Vec<(String, Style)> = Vec::new();
    match tool_name {
        "write_file" => {
            let content = arg("content");
            let total = content.lines().count();
            for line in content.lines().take(MAX) {
                items.push((format!("+ {line}"), green));
            }
            if total > MAX {
                items.push((format!("… +{} more lines", total - MAX), subtle()));
            }
        }
        "edit_file" => {
            let old = arg("old_string");
            let new = arg("new_string");
            let old_total = old.lines().count();
            for line in old.lines().take(MAX) {
                items.push((format!("- {line}"), red));
            }
            if old_total > MAX {
                items.push((format!("  … +{} more", old_total - MAX), subtle()));
            }
            let new_total = new.lines().count();
            for line in new.lines().take(MAX) {
                items.push((format!("+ {line}"), green));
            }
            if new_total > MAX {
                items.push((format!("  … +{} more", new_total - MAX), subtle()));
            }
        }
        _ => return Vec::new(),
    }
    result_block_verbatim(items, width)
}

/// A tool call as (verb, argument) — e.g. ("Read", "src/auth.rs") — so the verb
/// and its target can be styled distinctly instead of a single `Read(path)` blob.
fn tool_call_parts(tool_name: &str, arguments: &Value) -> (String, String) {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match tool_name {
        "read_file" => ("Read".into(), arg("path")),
        "write_file" => ("Write".into(), arg("path")),
        "edit_file" => ("Edit".into(), arg("path")),
        "replace_file_content" => (
            "Replace".into(),
            format!(
                "{} · lines {}-{}",
                arg("path"),
                arguments.get("start_line").and_then(Value::as_u64).unwrap_or(0),
                arguments.get("end_line").and_then(Value::as_u64).unwrap_or(0)
            ),
        ),
        "list_files" => (
            "List".into(),
            arguments.get("path").and_then(Value::as_str).unwrap_or(".").to_string(),
        ),
        "search_content" => ("Grep".into(), arg("query")),
        "view_outline" => ("Outline".into(), arg("path")),
        "web_search" => ("Web".into(), arg("query")),
        "web_read" => ("Fetch".into(), arg("url")),
        "bash" => ("Bash".into(), arg("command")),
        _ => (
            tool_name.to_string(),
            serde_json::to_string(arguments).unwrap_or_default(),
        ),
    }
}

/// A short, single-line result summary for the tools whose output is just a count
/// or status — so it can be merged onto the call row (`● Read  path     142 lines`).
/// Returns None for errors and for tools with multi-line output (bash, list), which
/// render as their own block below the call.
fn tool_result_oneliner(tool_name: &str, result: &Value) -> Option<String> {
    if result.get("status").and_then(Value::as_str) == Some("error") {
        return None;
    }
    let data = result.get("data").unwrap_or(result);
    let s = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("");
    let count = |key: &str| data.get(key).and_then(Value::as_u64).unwrap_or(0);
    let line = match tool_name {
        "read_file" => {
            let n = s("content").lines().count();
            format!("{n} {}", if n == 1 { "line" } else { "lines" })
        }
        "write_file" => "written".to_string(),
        "edit_file" => "updated".to_string(),
        "replace_file_content" => "replaced".to_string(),
        "search_content" => format!("{} matches", count("count")),
        "web_search" => format!("{} results", count("count")),
        "web_read" => format!("{} chars", s("text").chars().count()),
        "view_outline" => {
            if data.get("is_directory").and_then(Value::as_bool).unwrap_or(false) {
                let n = data.get("entries").and_then(Value::as_array).map(|e| e.len()).unwrap_or(0);
                format!("{n} entries")
            } else {
                let n = data.get("outline").and_then(Value::as_array).map(|o| o.len()).unwrap_or(0);
                format!("{n} decls")
            }
        }
        _ => return None,
    };
    Some(line)
}

fn tool_result_lines(tool_name: &str, result: &Value, width: usize) -> Vec<Line<'static>> {
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let data = result.get("data").unwrap_or(result);

    if status == "error" {
        let message = result
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("failed");
        return result_block(
            vec![(format!("✗ {message}"), Style::default().fg(DANGER))],
            width,
        );
    }

    let str_field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("");
    let items: Vec<(String, Style)> = match tool_name {
        "read_file" => {
            let lines = str_field("content").lines().count();
            vec![(format!("Read {lines} lines"), subtle())]
        }
        "write_file" => vec![(format!("Wrote {}", str_field("path")), subtle())],
        "edit_file" => vec![(format!("Updated {}", str_field("path")), subtle())],
        "replace_file_content" => vec![(format!("Replaced contiguous block in {}", str_field("path")), subtle())],
        "list_files" => {
            let entries = data.get("entries").and_then(Value::as_array);
            let count = entries.map(|e| e.len()).unwrap_or(0);
            let names = entries
                .map(|e| {
                    e.iter()
                        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                        .take(12)
                        .collect::<Vec<_>>()
                        .join("  ")
                })
                .unwrap_or_default();
            vec![
                (format!("{count} entries"), subtle()),
                (names, subtle()),
            ]
        }
        "search_content" => {
            let count = data.get("count").and_then(Value::as_u64).unwrap_or(0);
            vec![(format!("Found {count} content matches"), subtle())]
        }
        "web_search" => {
            let count = data.get("count").and_then(Value::as_u64).unwrap_or(0);
            vec![(format!("{count} web results"), subtle())]
        }
        "web_read" => {
            let chars = data.get("text").and_then(Value::as_str).map(|t| t.chars().count()).unwrap_or(0);
            vec![(format!("Read {chars} chars"), subtle())]
        }
        "view_outline" => {
            if data.get("is_directory").and_then(Value::as_bool).unwrap_or(false) {
                let count = data.get("entries").and_then(Value::as_array).map(|e| e.len()).unwrap_or(0);
                vec![(format!("Directory — {count} entries"), subtle())]
            } else {
                let outline = data.get("outline").and_then(Value::as_array);
                let count = outline.map(|o| o.len()).unwrap_or(0);
                vec![(format!("Outline has {count} code declarations"), subtle())]
            }
        }
        "bash" => bash_result_items(data),
        _ => vec![(status.to_string(), subtle())],
    };

    result_block(items.into_iter().filter(|(t, _)| !t.is_empty()).collect(), width)
}

fn bash_result_items(data: &Value) -> Vec<(String, Style)> {
    // Keep bash output minimal: one summary line + a tiny preview, never the full
    // dump (the model still has the complete output; the UI just shouldn't flood).
    const BASH_PREVIEW: usize = 3;
    let success = data.get("success").and_then(Value::as_bool).unwrap_or(false);
    let exit = data
        .get("exit_code")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("");

    let output: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let total = output.len();

    // Single concise summary line (red on failure).
    let noun = if total == 1 { "line" } else { "lines" };
    let summary = match (success, total) {
        (true, 0) => "ran · no output".to_string(),
        (true, n) => format!("ran · {n} {noun}"),
        (false, 0) => format!("exited {exit} · no output"),
        (false, n) => format!("exited {exit} · {n} {noun}"),
    };
    let summary_style = if success { subtle() } else { Style::default().fg(DANGER) };
    let mut items = vec![(summary, summary_style)];

    // Glimpse: the first few lines only.
    let shown = total.min(BASH_PREVIEW);
    for line in &output[..shown] {
        items.push((line.to_string(), subtle()));
    }
    if total > shown {
        items.push((
            format!("… +{} more lines", total - shown),
            subtle().add_modifier(Modifier::ITALIC),
        ));
    }
    items
}

/// Render result/output logical lines under a `⎿` gutter, wrapped to width.
fn result_block(items: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    result_block_inner(items, width, false)
}

/// Like `result_block` but preserves each line verbatim (indentation and runs of
/// spaces) instead of word-wrapping — used for code/diff previews where leading
/// whitespace is meaningful.
fn result_block_verbatim(items: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    result_block_inner(items, width, true)
}

fn result_block_inner(
    items: Vec<(String, Style)>,
    width: usize,
    verbatim: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut first = true;
    for (text, style) in items {
        let segs = if verbatim {
            wrap_code_line(&text, width.saturating_sub(4))
        } else {
            wrap_one(&text, width.saturating_sub(4))
        };
        for seg in segs {
            let prefix = if first { "  ⎿ " } else { "    " };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), subtle()),
                Span::styled(seg, style),
            ]));
            first = false;
        }
    }
    lines
}

// --- Markdown-lite prose rendering (assistant text) ---

fn render_prose(text: &str, width: usize) -> Vec<Line<'static>> {
    let base = Style::default().fg(TEXT);
    let code_block = Style::default().fg(CODE);
    let heading = Style::default().fg(blue()).add_modifier(Modifier::BOLD);

    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            i += 1;
            continue;
        }
        // Markdown table: a `|`-delimited header row immediately followed by a
        // `|---|:--:|` separator row of the same column count. Render aligned with
        // wrapped cells instead of dumping raw pipes.
        if !in_code && raw.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let header = split_table_cells(raw);
            let sep = split_table_cells(lines[i + 1]);
            if !header.is_empty() && header.len() == sep.len() {
                let aligns: Vec<CellAlign> = sep.iter().map(|c| cell_align(c)).collect();
                let mut body = Vec::new();
                let mut j = i + 2;
                while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                    body.push(split_table_cells(lines[j]));
                    j += 1;
                }
                out.extend(render_md_table(&header, &aligns, &body, width));
                i = j;
                continue;
            }
        }
        // Fenced code, or an unfenced block indented 4+ spaces / a tab (Markdown
        // indented code): render verbatim — preserve indentation and skip inline
        // markdown so underscores/asterisks inside identifiers aren't mangled.
        let indented_code =
            !in_code && !trimmed.is_empty() && (raw.starts_with("    ") || raw.starts_with('\t'));
        if in_code || indented_code {
            for seg in wrap_code_line(raw, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(seg, code_block),
                ]));
            }
            i += 1;
            continue;
        }
        if let Some(h) = heading_text(trimmed) {
            for seg in wrap_one(h, width) {
                out.push(Line::from(Span::styled(seg, heading)));
            }
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            let runs = parse_inline_md(rest, base);
            let mut bullet = wrap_runs(runs, width.saturating_sub(2));
            if let Some(first) = bullet.first_mut() {
                first.spans.insert(
                    0,
                    Span::styled("• ", Style::default().fg(blue())),
                );
            }
            for line in bullet.iter_mut().skip(1) {
                line.spans.insert(0, Span::raw("  "));
            }
            out.extend(bullet);
            i += 1;
            continue;
        }
        if raw.trim().is_empty() {
            out.push(Line::from(""));
            i += 1;
            continue;
        }
        let runs = parse_inline_md(trimmed, base);
        out.extend(wrap_runs(runs, width));
        i += 1;
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

#[derive(Clone, Copy)]
enum CellAlign {
    Left,
    Right,
    Center,
}

/// Split a markdown table row into trimmed cells, dropping the optional leading
/// and trailing pipes (`| a | b |` and `a | b` both → ["a", "b"]).
fn split_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// A markdown table delimiter row: every cell is dashes with optional `:` ends.
fn is_table_separator(line: &str) -> bool {
    let cells = split_table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn cell_align(sep: &str) -> CellAlign {
    let s = sep.trim();
    match (s.starts_with(':'), s.ends_with(':')) {
        (true, true) => CellAlign::Center,
        (false, true) => CellAlign::Right,
        _ => CellAlign::Left,
    }
}

/// Pad a rendered cell line to `w` columns per its alignment.
fn pad_table_line(mut line: Line<'static>, w: usize, align: CellAlign) -> Line<'static> {
    let pad = w.saturating_sub(line.width());
    if pad == 0 {
        return line;
    }
    match align {
        CellAlign::Left => line.spans.push(Span::raw(" ".repeat(pad))),
        CellAlign::Right => line.spans.insert(0, Span::raw(" ".repeat(pad))),
        CellAlign::Center => {
            let l = pad / 2;
            line.spans.insert(0, Span::raw(" ".repeat(l)));
            line.spans.push(Span::raw(" ".repeat(pad - l)));
        }
    }
    line
}

/// Render a markdown table: column widths from content (shrunk to fit `width`),
/// header bold + a rule, cells inline-md-styled and wrapped, separated by ` │ `.
fn render_md_table(
    header: &[String],
    aligns: &[CellAlign],
    body: &[Vec<String>],
    width: usize,
) -> Vec<Line<'static>> {
    let ncols = header
        .len()
        .max(body.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);
    let header_style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(TEXT);
    let faint = Style::default().fg(FAINT);

    let runs_for = |s: &str, header_row: bool| {
        parse_inline_md(s, if header_row { header_style } else { body_style })
    };
    let runs_w = |runs: &[(String, Style)]| runs.iter().map(|(t, _)| t.chars().count()).sum::<usize>();

    let mut natural = vec![1usize; ncols];
    for (c, h) in header.iter().enumerate().take(ncols) {
        natural[c] = natural[c].max(runs_w(&runs_for(h, true)));
    }
    for row in body {
        for (c, cell) in row.iter().enumerate().take(ncols) {
            natural[c] = natural[c].max(runs_w(&runs_for(cell, false)));
        }
    }

    // Column separator is " │ " (3 cols). Fit natural widths into the budget,
    // shrinking the widest columns first when they don't fit.
    let sep_total = 3 * ncols.saturating_sub(1);
    let avail = width.saturating_sub(sep_total).max(ncols * 3);
    let natural_sum: usize = natural.iter().sum::<usize>().max(1);
    let widths: Vec<usize> = if natural_sum <= avail {
        natural.clone()
    } else {
        let mut w: Vec<usize> = natural
            .iter()
            .map(|&n| (n * avail / natural_sum).max(3))
            .collect();
        let mut total: usize = w.iter().sum();
        while total > avail {
            let idx = (0..ncols).max_by_key(|&i| w[i]).unwrap_or(0);
            if w[idx] <= 3 {
                break;
            }
            w[idx] -= 1;
            total -= 1;
        }
        w
    };

    let render_row = |cells: &[String], header_row: bool| -> Vec<Line<'static>> {
        let wrapped: Vec<Vec<Line<'static>>> = (0..ncols)
            .map(|c| {
                let text = cells.get(c).map(String::as_str).unwrap_or("");
                let mut wl = wrap_runs(runs_for(text, header_row), widths[c].max(1));
                if wl.is_empty() {
                    wl.push(Line::from(""));
                }
                let align = aligns.get(c).copied().unwrap_or(CellAlign::Left);
                wl.into_iter().map(|l| pad_table_line(l, widths[c], align)).collect()
            })
            .collect();
        let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        (0..height)
            .map(|k| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for c in 0..ncols {
                    if c > 0 {
                        spans.push(Span::styled(" │ ", faint));
                    }
                    match wrapped[c].get(k) {
                        Some(l) => spans.extend(l.spans.clone()),
                        None => spans.push(Span::raw(" ".repeat(widths[c]))),
                    }
                }
                Line::from(spans)
            })
            .collect()
    };

    let mut out = render_row(header, true);
    let mut rule: Vec<Span<'static>> = Vec::new();
    for c in 0..ncols {
        if c > 0 {
            rule.push(Span::styled("─┼─", faint));
        }
        rule.push(Span::styled("─".repeat(widths[c]), faint));
    }
    out.push(Line::from(rule));
    for row in body {
        out.extend(render_row(row, false));
    }
    out
}

fn heading_text(line: &str) -> Option<&str> {
    for prefix in ["#### ", "### ", "## ", "# "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// Split a single logical line into styled runs, honouring `**bold**`,
/// `` `code` ``, and `*italic*` / `_italic_`.
fn parse_inline_md(text: &str, base: Style) -> Vec<(String, Style)> {
    let bold = base.add_modifier(Modifier::BOLD);
    let italic = base.add_modifier(Modifier::ITALIC);
    let code = Style::default().fg(CODE);

    let chars: Vec<char> = text.chars().collect();
    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let find = |from: usize, pat: char| (from..chars.len()).find(|&j| chars[j] == pat);
    let find2 =
        |from: usize| (from + 1..chars.len()).find(|&j| chars[j] == '*' && chars[j - 1] == '*');

    while i < chars.len() {
        let c = chars[i];
        if c == '`'
            && let Some(end) = find(i + 1, '`')
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 1..end].iter().collect(), code));
            i = end + 1;
            continue;
        }
        if c == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find2(i + 2)
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 2..end - 1].iter().collect(), bold));
            i = end + 1;
            continue;
        }
        if (c == '*' || c == '_')
            && i + 1 < chars.len()
            && chars[i + 1] != c
            && let Some(end) = find(i + 1, c)
            && end > i + 1
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 1..end].iter().collect(), italic));
            i = end + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut runs, &mut buf, base);
    runs
}

fn flush(runs: &mut Vec<(String, Style)>, buf: &mut String, style: Style) {
    if !buf.is_empty() {
        runs.push((std::mem::take(buf), style));
    }
}

/// Greedy word-wrap across styled runs, preserving each run's style.
fn wrap_runs(runs: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for (text, style) in runs {
        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let wlen = word.chars().count();
            if cur_w > 0 && cur_w + 1 + wlen > width {
                lines.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
            }
            if cur_w > 0 {
                cur.push(Span::raw(" "));
                cur_w += 1;
            }
            if wlen > width {
                if cur_w > 0 {
                    lines.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
                for chunk in hard_chunks(word, width) {
                    let clen = chunk.chars().count();
                    if clen == width {
                        lines.push(Line::from(vec![Span::styled(chunk, style)]));
                    } else {
                        cur.push(Span::styled(chunk, style));
                        cur_w = clen;
                    }
                }
            } else {
                cur.push(Span::styled(word.to_string(), style));
                cur_w += wlen;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Plain word-wrap (no styling) for a possibly multi-line string.
/// Wrap a code line to `width` while preserving ALL whitespace — leading indent
/// and internal alignment — unlike `wrap_one`, which collapses space runs. Tabs
/// expand to 4 spaces; continuation fragments are re-indented to the line's own
/// leading whitespace so nested structure stays readable after a wrap.
fn wrap_code_line(raw: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let expanded = raw.replace('\t', "    ");
    let chars: Vec<char> = expanded.chars().collect();
    if chars.len() <= width {
        return vec![expanded];
    }
    let indent: String = expanded.chars().take_while(|c| *c == ' ').collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let lead = if first { String::new() } else { indent.clone() };
        let budget = width.saturating_sub(lead.chars().count()).max(1);
        let end = (start + budget).min(chars.len());
        let mut seg = lead;
        seg.extend(&chars[start..end]);
        out.push(seg);
        start = end;
        first = false;
    }
    out
}

fn wrap_one(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cw = 0usize;
        for word in raw.split(' ') {
            if word.is_empty() {
                continue;
            }
            let wl = word.chars().count();
            if cw > 0 && cw + 1 + wl > width {
                out.push(std::mem::take(&mut cur));
                cw = 0;
            }
            if wl > width {
                if cw > 0 {
                    out.push(std::mem::take(&mut cur));
                    cw = 0;
                }
                for chunk in hard_chunks(word, width) {
                    let cl = chunk.chars().count();
                    if cl == width {
                        out.push(chunk);
                    } else {
                        cur = chunk;
                        cw = cl;
                    }
                }
                continue;
            }
            if cw > 0 {
                cur.push(' ');
                cw += 1;
            }
            cur.push_str(word);
            cw += wl;
        }
        out.push(cur);
    }
    out
}

fn hard_chunks(word: &str, width: usize) -> Vec<String> {
    word.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
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
    let token = qs.iter().map(q_text).collect::<Vec<_>>().join("\u{1}");
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

    let accent = Color::Rgb(96, 165, 250);
    let faint = Color::Rgb(120, 120, 130);
    let dim = Color::Rgb(160, 160, 170);
    let yellow = WARN;

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
        Span::styled("? ", Style::default().fg(yellow).add_modifier(Modifier::BOLD)),
        Span::styled(q_line, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
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
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
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

fn render_input(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(FAINT));

    // While the login form is open, editing happens in the inline form above —
    // the input box just shows the controls.
    if app.login_active {
        let line = Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                "Tab next · ←/→ change · Enter connect · Esc cancel",
                Style::default().fg(FAINT),
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
        let line = Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(placeholder, Style::default().fg(FAINT)),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    // Wrap the prompt into display rows (honoring explicit newlines) and draw a
    // block cursor at its (row, col). The prompt glyph takes the first 2 columns;
    // continuation rows are indented to match.
    let text_w = (area.width as usize).saturating_sub(3).max(1);
    let (rows, (cursor_row, cursor_col)) = layout_input(&app.input, app.input_cursor, text_w);
    let white = Style::default().fg(TEXT);
    let cursor_style = Style::default().fg(Color::Black).bg(blue());

    let mut lines = Vec::with_capacity(rows.len());
    for (k, row) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        if k == 0 {
            spans.push(Span::styled(" ❯ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
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
    let scroll = cursor_row.saturating_sub(visible.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Height (incl. top/bottom borders) the input box needs for the current prompt,
/// clamped so it grows with wrapped/multi-line input but never dominates the view.
fn input_height(app: &App, width: u16) -> u16 {
    const MAX_ROWS: usize = 8;
    if app.input.is_empty() {
        return 3;
    }
    let text_w = (width as usize).saturating_sub(3).max(1);
    let (rows, _) = layout_input(&app.input, app.input_cursor, text_w);
    (rows.len().clamp(1, MAX_ROWS) as u16) + 2
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

    let accent = Color::Rgb(96, 165, 250);
    let dim = MUTED;
    let w = TEXT;
    let faint = Color::Rgb(75, 85, 99);
    let rule = Color::Rgb(50, 50, 60);
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
                format!("{:9}", label),
                Style::default()
                    .fg(if focused { w } else { dim })
                    .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
            ),
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
    lines.push(field_row("provider", p_focus, chooser(app.form_provider.clone(), p_focus, "")));

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

    // Base URL (openai-compatible only)
    if app.form_provider == "openai-compatible" {
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
    let dim = Style::default().fg(MUTED);
    let faint = Style::default().fg(FAINT);

    let mut right = vec![
        Span::styled(format!("↑{}", fmt_si(st.map(|s| s.prompt_tokens).unwrap_or(0))), dim),
        Span::raw(" "),
        Span::styled(format!("↓{}", fmt_si(st.map(|s| s.completion_tokens).unwrap_or(0))), dim),
    ];
    let cache = st.map(|s| s.cache_read_tokens).unwrap_or(0);
    if cache > 0 {
        right.push(Span::styled(format!(" ↻{}", fmt_si(cache)), dim));
    }
    right.push(Span::styled(" · ", faint));
    right.push(Span::styled(
        format!("ctx {} ", fmt_si(st.map(|s| s.last_prompt_tokens).unwrap_or(0))),
        dim,
    ));

    let left_span = Span::styled(format!(" {}", app.cwd_display), dim);
    let right_line = Line::from(right);
    let right_w = right_line.width() as u16;

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w + 1)])
        .split(area);

    frame.render_widget(Paragraph::new(Line::from(left_span)), cols[0]);
    frame.render_widget(
        Paragraph::new(right_line).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
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

// --- Theme: one muted palette so colors stay consistent across the UI. The old
// code scattered raw ANSI (Red/Green/Yellow/Cyan) next to RGB blue, which is the
// main reason it read as garish. Everything routes through these now. ---
const ACCENT: Color = Color::Rgb(96, 165, 250); // primary — prompts, glyphs, headings
const TEXT: Color = Color::Rgb(222, 225, 230); // body text (softer than pure white)
const MUTED: Color = Color::Rgb(124, 130, 142); // secondary text, summaries, gutters
const FAINT: Color = Color::Rgb(82, 86, 96); // rules, borders, placeholders
const SUCCESS: Color = Color::Rgb(126, 186, 120); // added lines, ok
const DANGER: Color = Color::Rgb(224, 108, 117); // errors, removed lines
const WARN: Color = Color::Rgb(214, 182, 106); // runtime notices, questions
const LANE: Color = Color::Rgb(110, 184, 200); // delegated lanes
const CODE: Color = Color::Rgb(224, 196, 132); // code / diff text

fn subtle() -> Style {
    Style::default().fg(MUTED)
}

fn blue() -> Color {
    ACCENT
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
        "anthropic" => {
            let url = "https://api.anthropic.com/v1/models";
            let res = client.get(url)
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




