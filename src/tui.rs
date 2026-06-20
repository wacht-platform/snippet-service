use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
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
    ("/login", "Configure API provider inline"),
    ("/settings", "Open full settings screen"),
];

#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub config_path: PathBuf,
    pub config: SnippetConfig,
}

pub async fn run_tui(options: TuiOptions) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, options).await;
    restore_terminal(&mut terminal)?;
    result
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<TuiTerminal, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    ResumeSelection,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    Provider,
    ApiKey,
    Model,
    BaseUrl,
    SaveBtn,
    CancelBtn,
}

/// Providers offered by the login form and the settings screen, in display order.
const LOGIN_PROVIDERS: &[&str] = &["openai", "anthropic", "gemini", "openrouter", "openai-compatible"];

/// Single source of truth for a provider's default base URL and model, shared by
/// the login form and the settings screen so they can't drift apart.
fn provider_defaults(provider: &str) -> (String, String) {
    match provider {
        "openai" => ("https://api.openai.com/v1".to_string(), "gpt-5.5".to_string()),
        "anthropic" => (String::new(), "claude-opus-4-8".to_string()),
        "gemini" => (
            "https://generativelanguage.googleapis.com/v1beta/openai/".to_string(),
            "gemini-3.1-pro".to_string(),
        ),
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
    /// When true, the compact inline login form is shown and owns key input.
    /// It edits the shared `form_*` state, same as the settings screen.
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
        };
        app.init_settings_form();

        if app.options.config.resume_on_start {
            if let Some(last_active) = app.find_last_active_conversation() {
                app.switch_conversation(&last_active);
            }
        } else {
            let name = uuid::Uuid::new_v4().to_string();
            app.switch_conversation(&name);
        }

        if app.options.config.model.api_key.trim().is_empty() {
            app.status = "No model connected yet — type /login to connect one.".to_string();
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
        self.input.clear();
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
        self.status = format!("Switched to session: {}", name);
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
                self.status = format!("Started new session: {}. Type a task.", name);
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
            "/login" => {
                if self.agent_alive() {
                    self.status = "Agent is running. Finish or stop it before configuring login.".to_string();
                    return;
                }
                self.open_login();
                self.status = "Connect a model — Tab between fields, Enter to connect.".to_string();
            }
            "/settings" => {
                if self.agent_alive() {
                    self.status = "Agent is running. Finish or stop it before modifying settings.".to_string();
                    return;
                }
                self.original_config = Some(self.options.config.clone());
                self.init_settings_form();
                self.screen = Screen::Settings;
                self.status = "Configure your AI provider settings.".to_string();
            }
            other => {
                self.status = format!("Unknown command: {}. Type /new, /resume, /login, or /settings.", other);
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
                self.status = format!(
                    "✓ Connected — {} · {}",
                    self.options.config.model.provider, self.options.config.model.model
                );
            }
            Err(e) => self.error = Some(e),
        }
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

        self.status = format!("Saved settings! Using {} model.", self.options.config.model.model);
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

    fn change_model(&mut self, next: bool) {
        let matches: Vec<&str> = if let Some(ref fetched) = self.form_fetched_models {
            let query = self.form_model_query.to_lowercase();
            fetched.iter()
                .map(|s| s.as_str())
                .filter(|m| m.to_lowercase().contains(&query))
                .collect()
        } else {
            let standard_models = get_provider_models(&self.form_provider);
            let query = self.form_model_query.to_lowercase();
            standard_models.iter()
                .copied()
                .filter(|m| m.to_lowercase().contains(&query))
                .collect()
        };

        let active_list = if matches.is_empty() {
            if let Some(ref fetched) = self.form_fetched_models {
                fetched.iter().map(|s| s.as_str()).collect::<Vec<_>>()
            } else {
                get_provider_models(&self.form_provider).to_vec()
            }
        } else {
            matches
        };

        if active_list.is_empty() {
            return;
        }

        let current = self.form_model.trim();
        let current_idx = active_list.iter().position(|m| *m == current);
        let next_idx = match current_idx {
            Some(idx) => {
                if next {
                    (idx + 1) % active_list.len()
                } else {
                    if idx == 0 { active_list.len() - 1 } else { idx - 1 }
                }
            }
            None => 0,
        };
        self.form_model = active_list[next_idx].to_string();
    }

    fn move_focus(&mut self, next: bool) {
        let mut order = vec![
            SettingsField::Provider,
            SettingsField::ApiKey,
            SettingsField::Model,
        ];
        if self.form_provider == "openai-compatible" {
            order.push(SettingsField::BaseUrl);
        }
        order.push(SettingsField::SaveBtn);
        order.push(SettingsField::CancelBtn);

        let current_pos = order.iter().position(|f| *f == self.form_focus).unwrap_or(0);
        let next_pos = if next {
            (current_pos + 1) % order.len()
        } else {
            if current_pos == 0 { order.len() - 1 } else { current_pos - 1 }
        };
        self.form_focus = order[next_pos];

        if self.form_focus == SettingsField::Model || self.form_focus == SettingsField::BaseUrl {
            self.trigger_models_fetch();
        }
    }

    fn agent_alive(&self) -> bool {
        self.agent
            .as_ref()
            .map(|handle| !handle.is_finished())
            .unwrap_or(false)
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
            let typed = self.input.trim().to_string();
            if typed.is_empty() {
                self.status = "Type an answer, then press Enter.".to_string();
                return;
            }
            self.input.clear();
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
        self.input.clear();
        self.scroll = 0;
        self.q_token.clear();
        self.q_index = 0;
        self.q_sel = 0;
        self.q_answers.clear();
    }

    fn submit(&mut self) {
        // The inline login form captures keys in handle_login_key; submit() is
        // not reached while it is active.
        if self.login_active {
            self.login_connect();
            return;
        }

        let text = self.input.trim().to_string();
        if text.is_empty() {
            if !self.agent_alive() {
                self.status = "Enter a task before starting.".to_string();
            }
            return;
        }
        self.input.clear();
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
            self.spawn_loop(Some(text), false);
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
        let factory = self.model_factory();

        let locks_dir = state_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("locks");

        self.agent = Some(tokio::spawn(async move {
            let mut model = model_config.build_model();
            let locks = std::sync::Arc::new(crate::locks::LockRegistry::new(locks_dir));
            let context = ToolContext::with_locks(workspace, "main", locks)
                .map_err(|error| error.to_string())?;
            let harness = CodingHarness::new(
                HarnessConfig {
                    system_prompt: conversation_system_prompt(),
                    state_path: Some(state_path),
                    resume,
                    ..HarnessConfig::default()
                },
                coding_tools(),
                context,
            );
            harness
                .run_interactive(&mut model, initial, rx, Some(factory))
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
                Ok(Ok(_state)) => self.status = "Run stopped.".to_string(),
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
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(options);
    app.refresh_state().await;
    if app.options.config.resume_on_start {
        app.spawn_loop(None, true);
    }

    while !app.quit {
        terminal.draw(|frame| render(frame, &app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            handle_key(&mut app, key);
        }
        app.tick().await;
    }

    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('c') => app.interrupt_or_quit(),
            KeyCode::Char('d') => app.quit = true,
            KeyCode::Char('r') => app.spawn_loop(None, true),
            _ => {}
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

    if app.screen == Screen::Settings {
        match key.code {
            KeyCode::Esc => {
                if app.options.config.model.api_key.trim().is_empty() {
                    app.status = "API Key is required to start.".to_string();
                } else {
                    if let Some(orig) = app.original_config.take() {
                        app.options.config = orig;
                    }
                    app.screen = Screen::Main;
                    app.status = "Settings canceled.".to_string();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                app.move_focus(true);
            }
            KeyCode::BackTab | KeyCode::Up => {
                app.move_focus(false);
            }
            KeyCode::Enter => {
                match app.form_focus {
                    SettingsField::Provider => {
                        app.form_focus = SettingsField::ApiKey;
                    }
                    SettingsField::ApiKey => {
                        app.form_focus = SettingsField::Model;
                        app.trigger_models_fetch();
                    }
                    SettingsField::Model => {
                        if app.form_provider == "openai-compatible" {
                            app.form_focus = SettingsField::BaseUrl;
                        } else {
                            app.form_focus = SettingsField::SaveBtn;
                        }
                    }
                    SettingsField::BaseUrl => {
                        app.form_focus = SettingsField::SaveBtn;
                    }
                    SettingsField::SaveBtn => {
                        if app.form_api_key.trim().is_empty() {
                            app.status = "Error: API Key cannot be empty!".to_string();
                        } else {
                            match app.save_settings_to_file() {
                                Ok(_) => {
                                    app.original_config = None;
                                    app.screen = Screen::Main;
                                }
                                Err(e) => app.error = Some(e),
                            }
                        }
                    }
                    SettingsField::CancelBtn => {
                        if app.options.config.model.api_key.trim().is_empty() {
                            app.status = "API Key is required to start.".to_string();
                        } else {
                            if let Some(orig) = app.original_config.take() {
                                app.options.config = orig;
                            }
                            app.screen = Screen::Main;
                            app.status = "Settings canceled.".to_string();
                        }
                    }
                }
            }
            KeyCode::Left => {
                if app.form_focus == SettingsField::Provider {
                    app.change_provider(false);
                } else if app.form_focus == SettingsField::Model {
                    app.change_model(false);
                }
            }
            KeyCode::Right => {
                if app.form_focus == SettingsField::Provider {
                    app.change_provider(true);
                } else if app.form_focus == SettingsField::Model {
                    app.change_model(true);
                }
            }
            KeyCode::Char(' ') if app.form_focus == SettingsField::Provider || app.form_focus == SettingsField::Model => {
                match app.form_focus {
                    SettingsField::Provider => {
                        app.change_provider(true);
                    }
                    SettingsField::Model => {
                        app.change_model(true);
                    }
                    _ => {}
                }
            }
            KeyCode::Backspace => {
                match app.form_focus {
                    SettingsField::ApiKey => {
                        app.form_api_key.pop();
                    }
                    SettingsField::Model => {
                        app.form_model.pop();
                        app.form_model_query = app.form_model.clone();
                    }
                    SettingsField::BaseUrl => {
                        app.form_base_url.pop();
                    }
                    _ => {}
                }
            }
            KeyCode::Char(c) => {
                match app.form_focus {
                    SettingsField::ApiKey => {
                        app.form_api_key.push(c);
                    }
                    SettingsField::Model => {
                        app.form_model.push(c);
                        app.form_model_query = app.form_model.clone();
                    }
                    SettingsField::BaseUrl => {
                        app.form_base_url.push(c);
                    }
                    _ => {}
                }
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
                app.input = if selected.contains(' ') {
                    selected
                } else {
                    format!("{selected} ")
                };
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
                    || selected_cmd.starts_with("/profile ") 
                {
                    app.input = selected_cmd.clone();
                    app.submit();
                } else {
                    app.input = format!("{} ", selected_cmd);
                    app.suggestion_index = 0;
                }
                handled = true;
            }
            _ => {}
        }
    }

    if !handled {
        match key.code {
            KeyCode::Enter => app.submit(),
            KeyCode::Up => app.scroll_up(1),
            KeyCode::Down => app.scroll_down(1),
            KeyCode::PageUp => app.scroll_up(10),
            KeyCode::PageDown => app.scroll_down(10),
            KeyCode::Home => app.scroll_up(usize::MAX),
            KeyCode::End => app.scroll = 0,
            KeyCode::Esc => {
                if !app.input.is_empty() {
                    app.input.clear();
                    app.suggestion_index = 0;
                } else if app.agent_alive() {
                    if let Some(tx) = &app.input_tx {
                        let _ = tx.send(LoopInput::Interrupt);
                    }
                    app.status = "Interrupting...".to_string();
                }
            }
            KeyCode::Backspace => {
                app.input.pop();
                app.suggestion_index = 0;
            }
            // Typing is allowed while the agent works — it becomes a steer on Enter.
            KeyCode::Char(c) => {
                app.input.push(c);
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
            app.status = "Login cancelled.".to_string();
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

    if app.screen == Screen::Settings {
        render_settings(frame, area, app);
        return;
    }

    let sugg_h = suggestion_height(app);

    // Vertical split: Header (1), Content (Min 10), Suggestions (0-8), Question (0-2), Input (3), Status Message (1), Footer (1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // Header
            Constraint::Min(10),                     // Content
            Constraint::Length(sugg_h),              // Suggestions
            Constraint::Length(question_height(app)), // Question
            Constraint::Length(3),                  // Input
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
        Span::styled("  |  Select a session to resume", Style::default().fg(Color::White)),
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
    let footer_style = Style::default().fg(Color::DarkGray);
    let footer_text = "↑/↓ scroll  ·  Enter resume selected  ·  Esc go back";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer_text, footer_style))),
        chunks[2]
    );
}

fn render_settings(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders, BorderType};

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // Header
            Constraint::Length(2),                  // Guide
            Constraint::Min(16),                     // Main split panel (form + guide)
            Constraint::Length(1),                  // Footer help
        ])
        .split(area);

    // Header
    let header_text = vec![
        Span::styled("● snipett", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
        Span::styled("  |  Provider Onboarding & Settings", Style::default().fg(Color::White)),
    ];
    frame.render_widget(Paragraph::new(Line::from(header_text)), chunks[0]);

    // Subtitle / Guide
    let guide_text = "  Configure credentials and switch or create profiles. Snippet persists this globally in ~/.snippet/config.toml.";
    frame.render_widget(Paragraph::new(Line::from(Span::styled(guide_text, subtle()))), chunks[1]);

    // Main Split Layout: Info Panel (left) & Settings Form Panel (right)
    let main_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38), // Information Panel
            Constraint::Percentage(62), // Form Panel
        ])
        .split(chunks[2]);

    let left_panel_area = main_layout[0];
    let right_panel_area = main_layout[1];

    // --- Render Left Panel (Information & Guide) ---
    let guide_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(60, 60, 60)))
        .title(Span::styled(" Information & Capabilities ", Style::default().fg(Color::Gray)));

    let mut guide_lines = Vec::new();
    guide_lines.push(Line::from(""));

    match app.form_provider.as_str() {
        "anthropic" => {
            guide_lines.push(Line::from(vec![
                Span::styled("  Selected: ", Style::default().fg(Color::Gray)),
                Span::styled("Anthropic Claude", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
            ]));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from("  Industry-leading for coding & reasoning."));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Capabilities:", Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
            ]));
            guide_lines.push(Line::from("  • Prompt Caching: Enabled (cheaper/faster)"));
            guide_lines.push(Line::from("  • Context Window: 200k tokens"));
        }
        "openai" => {
            guide_lines.push(Line::from(vec![
                Span::styled("  Selected: ", Style::default().fg(Color::Gray)),
                Span::styled("OpenAI GPT", Style::default().fg(Color::Rgb(16, 185, 129)).add_modifier(Modifier::BOLD)),
            ]));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from("  Fast, reliable, state-of-the-art TUI"));
            guide_lines.push(Line::from("  coding capabilities and tool usage."));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Capabilities:", Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
            ]));
            guide_lines.push(Line::from("  • Reliable Tool Execution"));
            guide_lines.push(Line::from("  • Context Window: 128k tokens"));
        }
        "gemini" => {
            guide_lines.push(Line::from(vec![
                Span::styled("  Selected: ", Style::default().fg(Color::Gray)),
                Span::styled("Google Gemini", Style::default().fg(Color::Rgb(59, 130, 246)).add_modifier(Modifier::BOLD)),
            ]));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from("  Features a massive context window,"));
            guide_lines.push(Line::from("  making it optimal for large codebases."));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Capabilities:", Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
            ]));
            guide_lines.push(Line::from("  • Context Window: 1.0M tokens"));
        }
        "openrouter" => {
            guide_lines.push(Line::from(vec![
                Span::styled("  Selected: ", Style::default().fg(Color::Gray)),
                Span::styled("OpenRouter API", Style::default().fg(Color::Rgb(244, 63, 94)).add_modifier(Modifier::BOLD)),
            ]));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from("  Unified interface to access top models"));
            guide_lines.push(Line::from("  with flexible, competitive pricing."));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Capabilities:", Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
            ]));
            guide_lines.push(Line::from("  • Large variety of open & closed models"));
            guide_lines.push(Line::from("  • Context Window: up to 1.0M tokens"));
        }
        _ => {
            guide_lines.push(Line::from(vec![
                Span::styled("  Selected: ", Style::default().fg(Color::Gray)),
                Span::styled("OpenAI Compatible API", Style::default().fg(Color::Rgb(168, 85, 247)).add_modifier(Modifier::BOLD)),
            ]));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from("  Connect to local backends (Ollama/etc.)"));
            guide_lines.push(Line::from("  or custom hosted API endpoints."));
            guide_lines.push(Line::from(""));
            guide_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Requirements:", Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)),
            ]));
            guide_lines.push(Line::from("  • Requires Base URL configuration"));
            guide_lines.push(Line::from("  • Must support Chat Completions API"));
        }
    }

    frame.render_widget(Paragraph::new(guide_lines).block(guide_block), left_panel_area);

    // --- Render Right Panel (Step-by-step Setup Wizard) ---
    let step_num = match app.form_focus {
        SettingsField::Provider => 1,
        SettingsField::ApiKey => 2,
        SettingsField::Model | SettingsField::BaseUrl => 3,
        SettingsField::SaveBtn | SettingsField::CancelBtn => 4,
    };

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Step indicator
            Constraint::Min(12),   // Main wizard card
        ])
        .split(right_panel_area);

    let mut step_spans = Vec::new();
    
    // Step 1
    let s1_style = if step_num == 1 {
        Style::default().fg(blue()).add_modifier(Modifier::BOLD)
    } else if step_num > 1 {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    step_spans.push(Span::styled(" Provider ", s1_style));
    step_spans.push(Span::styled("──", Style::default().fg(Color::Rgb(60, 60, 60))));

    // Step 2
    let s2_style = if step_num == 2 {
        Style::default().fg(blue()).add_modifier(Modifier::BOLD)
    } else if step_num > 2 {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    step_spans.push(Span::styled(" API Key ", s2_style));
    step_spans.push(Span::styled("──", Style::default().fg(Color::Rgb(60, 60, 60))));

    // Step 3
    let s3_style = if step_num == 3 {
        Style::default().fg(blue()).add_modifier(Modifier::BOLD)
    } else if step_num > 3 {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    step_spans.push(Span::styled(" Model ", s3_style));
    step_spans.push(Span::styled("──", Style::default().fg(Color::Rgb(60, 60, 60))));

    // Step 4
    let s4_style = if step_num == 4 {
        Style::default().fg(blue()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    step_spans.push(Span::styled(" Confirm ", s4_style));

    frame.render_widget(
        Paragraph::new(Line::from(step_spans).alignment(ratatui::layout::Alignment::Center)),
        right_chunks[0]
    );

    let step_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(blue()))
        .title(Span::styled(format!(" Setup Step {step_num} of 4 "), Style::default().fg(Color::Gray)));

    let mut card_lines = Vec::new();
    card_lines.push(Line::from(""));

    match step_num {
        1 => {
            card_lines.push(Line::from("  Choose the AI provider:"));
            card_lines.push(Line::from(""));
            card_lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(" ◀ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {} ", app.form_provider), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(" ▶ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
            ]));
        }
        2 => {
            let focused = app.form_focus == SettingsField::ApiKey;
            card_lines.push(Line::from(format!("  Enter the API Key for {}:", app.form_provider)));
            card_lines.push(Line::from(""));

            let masked_key = if app.form_api_key.is_empty() {
                Span::styled("    [ enter API credentials... ]", Style::default().fg(Color::DarkGray))
            } else if focused {
                let bullets: String = std::iter::repeat('•').take(app.form_api_key.len()).collect();
                Span::styled(format!("    {}█", bullets), Style::default().fg(Color::White))
            } else {
                let bullets: String = std::iter::repeat('•').take(8.max(app.form_api_key.len())).collect();
                Span::styled(format!("    {}", bullets), Style::default().fg(Color::Gray))
            };
            card_lines.push(Line::from(masked_key));
            card_lines.push(Line::from(""));
            card_lines.push(Line::from("  (Leave empty if configured in env vars)"));
        }
        3 => {
            let focused_model = app.form_focus == SettingsField::Model;
            let focused_url = app.form_focus == SettingsField::BaseUrl;

            card_lines.push(Line::from("  Enter Model details:"));
            card_lines.push(Line::from(""));

            let model_span = if app.form_model.is_empty() {
                Span::styled("    [ type model name... ]", Style::default().fg(Color::DarkGray))
            } else if focused_model {
                Span::styled(format!("    {}█", app.form_model), Style::default().fg(Color::White))
            } else {
                Span::styled(format!("    {}", app.form_model), Style::default().fg(Color::Gray))
            };
            card_lines.push(Line::from(vec![
                Span::styled("  Model:    ", if focused_model { Style::default().fg(blue()).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) }),
                model_span,
            ]));
            card_lines.push(Line::from(""));
            card_lines.push(Line::from("  (Press Space or ◀/▶ to cycle standard models, or type to edit)"));

            if focused_model {
                if !app.models_fetch_status.is_empty() {
                    card_lines.push(Line::from(""));
                    card_lines.push(Line::from(vec![
                        Span::raw("  Status:   "),
                        Span::styled(&app.models_fetch_status, Style::default().fg(Color::Yellow)),
                    ]));
                }

                let matches: Vec<&str> = if let Some(ref fetched) = app.form_fetched_models {
                    let query = app.form_model_query.to_lowercase();
                    fetched.iter()
                        .map(|s| s.as_str())
                        .filter(|m| m.to_lowercase().contains(&query))
                        .collect()
                } else {
                    let standard_models = get_provider_models(&app.form_provider);
                    let query = app.form_model_query.to_lowercase();
                    standard_models.iter()
                        .copied()
                        .filter(|m| m.to_lowercase().contains(&query))
                        .collect()
                };

                let active_list = if matches.is_empty() {
                    if let Some(ref fetched) = app.form_fetched_models {
                        fetched.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                    } else {
                        get_provider_models(&app.form_provider).to_vec()
                    }
                } else {
                    matches
                };

                if !active_list.is_empty() {
                    card_lines.push(Line::from(""));
                    card_lines.push(Line::from(Span::styled("  Matching models:", Style::default().fg(Color::Gray))));
                    for m in active_list.iter().take(8) {
                        let is_current = *m == app.form_model;
                        let line = if is_current {
                            Line::from(vec![
                                Span::styled("    ➤ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
                                Span::styled(*m, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                            ])
                        } else {
                            Line::from(vec![
                                Span::raw("      "),
                                Span::styled(*m, Style::default().fg(Color::DarkGray)),
                            ])
                        };
                        card_lines.push(line);
                    }
                    if active_list.len() > 8 {
                        card_lines.push(Line::from(Span::styled(format!("      ... and {} more models", active_list.len() - 8), Style::default().fg(Color::DarkGray))));
                    }
                }
            }

            if app.form_provider == "openai-compatible" {
                card_lines.push(Line::from(""));
                let url_span = if app.form_base_url.is_empty() {
                    Span::styled("    [ type API Base URL... ]", Style::default().fg(Color::DarkGray))
                } else if focused_url {
                    Span::styled(format!("    {}█", app.form_base_url), Style::default().fg(Color::White))
                } else {
                    Span::styled(format!("    {}", app.form_base_url), Style::default().fg(Color::Gray))
                };
                card_lines.push(Line::from(vec![
                    Span::styled("  Base URL: ", if focused_url { Style::default().fg(blue()).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) }),
                    url_span,
                ]));
            }
        }
        _ => {
            let save_focused = app.form_focus == SettingsField::SaveBtn;
            let cancel_focused = app.form_focus == SettingsField::CancelBtn;

            let save_style = if save_focused {
                Style::default().bg(blue()).fg(Color::Black).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(blue()).add_modifier(Modifier::BOLD)
            };

            let cancel_style = if cancel_focused {
                Style::default().bg(Color::Rgb(239, 68, 68)).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            card_lines.push(Line::from("  Verify and save your settings:"));
            card_lines.push(Line::from(""));
            card_lines.push(Line::from(vec![
                Span::styled("    Provider: ", Style::default().fg(Color::Gray)),
                Span::styled(&app.form_provider, Style::default().fg(Color::White)),
            ]));
            card_lines.push(Line::from(vec![
                Span::styled("    Model:    ", Style::default().fg(Color::Gray)),
                Span::styled(&app.form_model, Style::default().fg(Color::White)),
            ]));
            if app.form_provider == "openai-compatible" {
                card_lines.push(Line::from(vec![
                    Span::styled("    Base URL: ", Style::default().fg(Color::Gray)),
                    Span::styled(&app.form_base_url, Style::default().fg(Color::White)),
                ]));
            }
            card_lines.push(Line::from(""));
            card_lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled("  Save Settings  ", save_style),
                Span::raw("      "),
                Span::styled("  Cancel  ", cancel_style),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(card_lines).block(step_block), right_chunks[1]);

    // Footer Help
    let footer_style = Style::default().fg(Color::DarkGray);
    let footer_text = match app.form_focus {
        SettingsField::Provider => "Space or ◀/▶ cycle provider  ·  Enter next step  ·  Shift-Tab go back  ·  Esc cancel",
        SettingsField::ApiKey => "Type API key  ·  Enter next step  ·  Shift-Tab/Up go back  ·  Esc cancel",
        SettingsField::Model => "Space or ◀/▶ cycle model  ·  Type to edit  ·  Enter next step  ·  Shift-Tab/Up go back  ·  Esc cancel",
        SettingsField::BaseUrl => "Type Base URL  ·  Enter next step  ·  Shift-Tab/Up go back  ·  Esc cancel",
        SettingsField::SaveBtn | SettingsField::CancelBtn => "◀/▶ or Tab to toggle  ·  Enter to select  ·  Shift-Tab/Up go back  ·  Esc cancel",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer_text, footer_style))),
        chunks[3]
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
            Span::styled("error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(err.to_string(), Style::default().fg(Color::Red)),
        ])
    } else {
        Line::from(vec![
            Span::styled(&app.status, Style::default().fg(Color::Gray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, _app: &App) {
    let text = vec![
        Span::styled("● snipett", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
    ];
    frame.render_widget(Paragraph::new(Line::from(text)), area);
}

fn render_history(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let width = (area.width as usize).saturating_sub(1).max(20);
    let height = area.height as usize;
    let lines = transcript_lines(app, width);

    let max_scroll = lines.len().saturating_sub(height);
    app.max_scroll.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);

    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let window = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(window), area);
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

    for event in &state.events {
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

    // Live "working…" feedback at the tail while the agent is processing (or a lane is).
    let working = state.status == HarnessStatus::Running
        || state.lanes.iter().any(|lane| lane.status == LaneStatus::Running);
    if working && app.agent_alive() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
            marker_block("↳ ", "steer  ", Color::Rgb(120, 200, 255), text, width)
        }
        HarnessEvent::AssistantText { text } => render_prose(text, width),
        HarnessEvent::Note { entry } => marker_block("✎ ", "note  ", Color::DarkGray, entry, width),
        HarnessEvent::SystemDecision { step, reasoning } => marker_block(
            "⚙ ",
            "",
            Color::Yellow,
            &format!("{step} — {reasoning}"),
            width,
        ),
        HarnessEvent::ModelError { message } => {
            marker_block("✗ ", "", Color::Red, message, width)
        }
        HarnessEvent::UserQuestion { questions } => {
            let text = question_text(questions).unwrap_or_else(|| "(question)".to_string());
            marker_block("? ", "", Color::Yellow, &text, width)
        }
        HarnessEvent::LaneSpawned { id, title } => marker_block(
            "→ ",
            "",
            Color::Cyan,
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
            vec![(format!("✗ {tool_name}: {error}"), Style::default().fg(Color::Red))],
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
                Span::styled(seg, Style::default().fg(Color::White)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(Color::White)),
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
        LaneStatus::Completed => ("done", Color::Green),
        LaneStatus::Failed => ("failed", Color::Red),
        LaneStatus::Running => ("running", Color::Cyan),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("◆ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("lane {id} · {title} "),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
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
    let header = tool_call_header(tool_name, arguments);
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&header, width.saturating_sub(2)).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
                Span::styled(seg, Style::default().fg(Color::White)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(Color::White)),
            ]));
        }
    }
    lines.extend(tool_call_preview(tool_name, arguments, width));
    lines
}

/// A preview of what the call will do — content for writes, a +/- diff for edits.
fn tool_call_preview(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);
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
    result_block(items, width)
}

fn tool_call_header(tool_name: &str, arguments: &Value) -> String {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    match tool_name {
        "read_file" => format!("Read({})", arg("path")),
        "write_file" => format!("Write({})", arg("path")),
        "edit_file" => format!("Update({})", arg("path")),
        "replace_file_content" => format!(
            "ReplaceContent({}, lines {}-{})",
            arg("path"),
            arguments.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            arguments.get("end_line").and_then(Value::as_u64).unwrap_or(0)
        ),
        "list_files" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
            format!("List({path})")
        }
        "search_content" => format!("Grep(\"{}\")", arg("query")),
        "view_outline" => format!("Outline({})", arg("path")),
        "bash" => format!("Bash({})", arg("command")),
        _ => format!(
            "{tool_name}({})",
            serde_json::to_string(arguments).unwrap_or_default()
        ),
    }
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
            vec![(format!("✗ {message}"), Style::default().fg(Color::Red))],
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
        "view_outline" => {
            let outline = data.get("outline").and_then(Value::as_array);
            let count = outline.map(|o| o.len()).unwrap_or(0);
            vec![(format!("Outline has {count} code declarations"), subtle())]
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
    let summary_style = if success { subtle() } else { Style::default().fg(Color::Red) };
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
    let mut lines = Vec::new();
    let mut first = true;
    for (text, style) in items {
        for seg in wrap_one(&text, width.saturating_sub(4)) {
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
    let base = Style::default().fg(Color::White);
    let code_block = Style::default().fg(Color::Rgb(224, 196, 132));
    let heading = Style::default().fg(blue()).add_modifier(Modifier::BOLD);

    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
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
            continue;
        }
        if let Some(h) = heading_text(trimmed) {
            for seg in wrap_one(h, width) {
                out.push(Line::from(Span::styled(seg, heading)));
            }
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
            continue;
        }
        if raw.trim().is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let runs = parse_inline_md(trimmed, base);
        out.extend(wrap_runs(runs, width));
    }
    if out.is_empty() {
        out.push(Line::from(""));
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
    let code = Style::default().fg(Color::Rgb(224, 196, 132));

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
    let yellow = Color::Yellow;

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
        Span::styled(q_line, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
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
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
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
        .border_style(Style::default().fg(Color::Rgb(40, 40, 40)));

    // While the login form is open, editing happens in the inline form above —
    // the input box just shows the controls.
    if app.login_active {
        let line = Line::from(vec![
            Span::styled("❧ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
            Span::styled(
                "Tab next · ←/→ change · Enter connect · Esc cancel",
                Style::default().fg(Color::Rgb(75, 85, 99)),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    let line = if app.input.is_empty() {
        // When a free-text ask_user question is pending, prompt for the answer.
        let placeholder = {
            let qs = questions_of(app);
            match qs.get(app.q_index.min(qs.len().saturating_sub(1))) {
                Some(q) if q_options(q).is_empty() => "Type your answer, then press ↵...",
                _ => "Type a prompt or a slash command...",
            }
        };
        Line::from(vec![
            Span::styled("❧ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
            Span::styled(placeholder, Style::default().fg(Color::Rgb(75, 85, 99))),
        ])
    } else {
        Line::from(vec![
            Span::styled("❧ ", Style::default().fg(blue()).add_modifier(Modifier::BOLD)),
            Span::styled(app.input.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(blue())),
        ])
    };

    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// Render the compact inline login form: all fields on one panel, the focused
/// one highlighted. Editing is driven by `handle_login_key`.
fn login_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    if !app.login_active {
        return Vec::new();
    }

    let accent = Color::Rgb(96, 165, 250);
    let dim = Color::DarkGray;
    let w = Color::White;
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
    let style = Style::default().fg(subtle().fg.unwrap());
    
    // ↑ input · ↓ output · ⚡ cache-read (cumulative) · ctx = current context
    // fill (prompt tokens of the most recent request, which grows as the
    // conversation accretes).
    let st = app.state.as_ref();
    let left_text = format!("📂 {}  |  ↑{} ↓{} ⚡{}  ·  ctx {}  ",
        app.cwd_display,
        fmt_si(st.map(|s| s.prompt_tokens).unwrap_or(0)),
        fmt_si(st.map(|s| s.completion_tokens).unwrap_or(0)),
        fmt_si(st.map(|s| s.cache_read_tokens).unwrap_or(0)),
        fmt_si(st.map(|s| s.last_prompt_tokens).unwrap_or(0)),
    );
    
    let model = app.options.config.model.model.clone();
    let right_text = format!("model: {}", model);
    
    let left_span = Span::styled(left_text, style);
    let right_span = Span::styled(right_text, style);
    
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(right_span.width() as u16),
        ])
        .split(area);
        
    frame.render_widget(Paragraph::new(Line::from(left_span)), cols[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right_span)).alignment(ratatui::layout::Alignment::Right),
        cols[1]
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

fn subtle() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn blue() -> Color {
    Color::Rgb(96, 165, 250)
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
            "gemini-3.1-pro",
            "gemini-3.1-flash",
            "gemini-3.5-flash",
            "gemini-3-pro",
            "gemini-3-flash",
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




