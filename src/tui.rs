use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use ratatui::widgets::{Clear, Paragraph, Wrap};
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
    ("/compact", "Compact older conversation history now"),
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
const LOGIN_PROVIDERS: &[&str] = &["openai", "chatgpt", "anthropic", "gemini", "openrouter", "openai-compatible"];

/// Single source of truth for a provider's default base URL and model.
fn provider_defaults(provider: &str) -> (String, String) {
    match provider {
        "openai" => ("https://api.openai.com/v1".to_string(), "gpt-5.5".to_string()),
        // ChatGPT-subscription (OAuth) — no base URL / API key; model is a Codex slug.
        "chatgpt" => (String::new(), "gpt-5.1-codex".to_string()),
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

fn provider_context_defaults(provider: &str) -> (u64, u8) {
    match provider {
        "openai" | "chatgpt" | "anthropic" | "gemini" => (250_000, 90),
        "openai-compatible" => (130_000, 90),
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
    /// True while a compaction animation should be shown in the transcript.
    compacting: bool,
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
    /// Inputs typed while the agent is executing — held and submitted when the run
    /// finishes (or is stopped), instead of steering mid-run.
    queued_inputs: std::collections::VecDeque<String>,
    /// Was the agent busy on the previous tick? Drives the queue flush on the
    /// busy → not-busy edge.
    was_busy: bool,
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
            theme_selected_index: 0,
            theme_original_index: 0,
            model_picker_index: 0,
            profiles_selected_index: 0,
            editing_profile: None,
            return_to_profiles: false,
            queued_inputs: std::collections::VecDeque::new(),
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
            original_config: None,
            last_state_modified: None,
            login_active: false,
            q_index: 0,
            q_sel: 0,
            q_answers: Vec::new(),
            q_token: String::new(),
            stream: crate::llm::StreamHandle::default(),
            compacting: false,
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

    /// Activate a saved profile: persist, restart the loop with it, return to Main.
    fn activate_profile(&mut self, name: &str) {
        if self.options.config.activate(name) {
            let _ = self.save_config_file();
            let resumed = self.restart_loop_for_config();
            self.screen = Screen::Main;
            self.status = format!(
                "✓ {} · {}{}",
                self.options.config.model.provider,
                self.options.config.model.model,
                if resumed { " · resumed" } else { "" },
            );
        }
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
        if self.form_provider == "openai-compatible" {
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
            SettingsField::Reasoning => self.login_cycle_reasoning(forward),
            SettingsField::Compaction => self.login_cycle_compaction_pct(forward),
            _ => {}
        }
    }

    fn login_move_model_suggestion(&mut self, forward: bool) {
        let rows = login_model_rows(self);
        if rows.is_empty() {
            self.model_picker_index = 0;
            return;
        }
        if self.model_picker_index >= rows.len() {
            self.model_picker_index = 0;
        }
        self.model_picker_index = if forward {
            (self.model_picker_index + 1) % rows.len()
        } else if self.model_picker_index == 0 {
            rows.len() - 1
        } else {
            self.model_picker_index - 1
        };
        if let Some(chosen) = rows.get(self.model_picker_index) {
            self.form_model = chosen.clone();
        }
    }

    fn login_cycle_reasoning(&mut self, forward: bool) {
        const OPTIONS: [&str; 4] = ["off", "low", "medium", "high"];
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
        self.agent_alive()
            && self.state.as_ref().is_some_and(|s| {
                s.status == HarnessStatus::Running
                    || s.lanes.iter().any(|l| l.status == LaneStatus::Running)
            })
    }

    /// True while the harness is mid-compaction (recent compaction-pass event + still
    /// running) — used to hold input and label the wait.
    fn is_compacting(&self) -> bool {
        self.compacting
            || self.state.as_ref().is_some_and(|s| {
                s.status == HarnessStatus::Running
                    && matches!(
                        s.events.last(),
                        Some(HarnessEvent::SystemDecision { step, .. })
                            if step == "history_compaction_pass" || step == "history_compacted"
                    )
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

        // While the agent is executing, hold the message instead of steering the
        // running turn — it's submitted when the run finishes (or is stopped). Esc
        // stops the run, which flushes the queue immediately.
        if self.agent_busy() {
            self.queued_inputs.push_back(text);
            self.status = if self.is_compacting() {
                self.compacting = true;
                String::new()
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
            }
        } else {
            // Resume the existing conversation rather than starting fresh — after an
            // interrupt the agent has died but the transcript is intact; resume=false
            // would clobber it into a new conversation.
            self.spawn_loop(Some(text), true);
        }
    }

    /// Submit the next queued input when the agent goes idle/stopped. One per
    /// busy→idle edge so each becomes its own turn (no mid-run steering).
    fn flush_queued_input(&mut self) {
        if let Some(text) = self.queued_inputs.pop_front() {
            self.submit_text(text);
            self.status = if self.queued_inputs.is_empty() {
                String::new()
            } else {
                format!("{} more queued", self.queued_inputs.len())
            };
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
                    context_window_tokens: model_config.context_window,
                    compact_at_pct: model_config.compact_at_pct,
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

        // Flush a queued input on the busy → not-busy edge: the run just finished or
        // was stopped, so submit the next held message as its own turn.
        let busy = self.agent_busy();
        if self.was_busy && !busy && !self.queued_inputs.is_empty() {
            self.flush_queued_input();
        }
        self.was_busy = busy;

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
                    self.status = format!(
                        "Open {} and enter code {} — waiting for sign-in…",
                        info.verification_url, info.user_code
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
    if app.login_active {
        handle_login_key(app, key);
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
                    app.activate_profile(&name);
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
        KeyCode::Enter => {
            if app.form_provider == "chatgpt" && app.form_focus != SettingsField::Model {
                app.login_connect();
            } else if app.form_focus == SettingsField::Model {
                let rows = login_model_rows(app);
                if let Some(chosen) = rows.get(app.model_picker_index).cloned() {
                    app.form_model = chosen;
                }
                app.login_connect();
            } else {
                app.login_connect();
            }
        }
        KeyCode::Tab => app.login_move_focus(true),
        KeyCode::BackTab => app.login_move_focus(false),
        KeyCode::Down if app.form_focus == SettingsField::Model => app.login_move_model_suggestion(true),
        KeyCode::Up if app.form_focus == SettingsField::Model => app.login_move_model_suggestion(false),
        KeyCode::Down => app.login_move_focus(true),
        KeyCode::Up => app.login_move_focus(false),
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

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" snippet", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled("  ·  models", subtle()),
            Span::styled(
                format!("  ·  {} active lane{}", app.state.as_ref().map(|s| s.lanes.iter().filter(|l| l.status == LaneStatus::Running).count()).unwrap_or(0), if app.state.as_ref().map(|s| s.lanes.iter().filter(|l| l.status == LaneStatus::Running).count()).unwrap_or(0) == 1 { "" } else { "s" }),
                Style::default().fg(lane()),
            ),
        ])),
        chunks[0],
    );

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
            "↑/↓ move  ·  ↵ activate  ·  e edit  ·  a add  ·  d delete  ·  Esc",
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
    let footer_text = "↑/↓ scroll  ·  Enter resume selected  ·  Esc go back";
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
    let model = app.options.config.model.model.clone();
    let name = " snipett";
    let mut spans = vec![
        Span::styled(name, Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", Style::default().fg(muted())),
        Span::styled(model.clone(), Style::default().fg(muted())),
        Span::raw(" "),
    ];
    // Fill the rest of the row with a thin rule for a clean header rather than a
    // bare glyph floating in empty space.
    let used = name.chars().count() + 3 + model.chars().count() + 1;
    let rule = (area.width as usize).saturating_sub(used + 1);
    if rule > 0 {
        spans.push(Span::styled("─".repeat(rule), Style::default().fg(faint())));
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

/// Center a line horizontally within `width` by left-padding to half the slack.
fn center_line(line: Line<'static>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(line.width()) / 2;
    if pad == 0 {
        return line;
    }
    let mut spans = vec![Span::raw(" ".repeat(pad))];
    spans.extend(line.spans);
    Line::from(spans)
}

/// The empty-state splash: a shimmering wordmark, a waving bar row, and a hint —
/// animated by the frame counter so the idle screen feels alive.
fn empty_state_lines(frame_n: usize, width: usize) -> Vec<Line<'static>> {
    let f = frame_n;

    // The wordmark with a highlight that sweeps across it.
    let word: Vec<char> = "snipett".chars().collect();
    let sweep = (f / 2) % (word.len() + 6);
    let mut title = Vec::new();
    for (i, c) in word.iter().enumerate() {
        let style = if i == sweep {
            Style::default().fg(accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self::text()).add_modifier(Modifier::BOLD)
        };
        title.push(Span::styled(c.to_string(), style));
    }

    // A small equalizer wave that ripples left→right.
    const BARS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    let mut wave = Vec::new();
    for i in 0..11usize {
        let phase = (f / 2 + i * 2) % 14;
        let h = if phase < 7 { phase } else { 14 - phase }; // triangle 0..6..0
        wave.push(Span::styled(BARS[h.min(7)].to_string(), Style::default().fg(accent())));
        wave.push(Span::raw(" "));
    }

    vec![
        center_line(Line::from(title), width),
        Line::from(""),
        center_line(Line::from(wave), width),
        Line::from(""),
        Line::from(""),
        center_line(
            Line::from(Span::styled(
                "type a task and press Enter   ·   / for commands",
                subtle(),
            )),
            width,
        ),
    ]
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
            lines.extend(marker_block("✗ ", "", danger(), &last, width));
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
                                        .push(Span::styled(summary, Style::default().fg(muted())));
                                    events.next();
                                }
                            }
                        }
                    }
                }
            } else if state.status == HarnessStatus::Running {
                let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
                let label = format!("{spinner} running");
                let style = Style::default().fg(accent());
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
            lines.push(Line::from(Span::styled(seg, Style::default().fg(muted()))));
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
        let compacting = state.status == HarnessStatus::Running
            && matches!(
                state.events.last(),
                Some(HarnessEvent::SystemDecision { step, .. })
                    if step == "history_compaction_pass" || step == "history_compacted"
            );
        if compacting {
            // Compaction shown as motion — an accent wave — not a log line.
            const BARS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
            let mut spans = vec![Span::styled(
                "✦ compacting context  ",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )];
            for i in 0..9usize {
                let phase = (app.frame / 2 + i * 2) % 14;
                let h = if phase < 7 { phase } else { 14 - phase };
                spans.push(Span::styled(BARS[h.min(7)], Style::default().fg(accent())));
            }
            lines.push(Line::from(spans));
        } else {
            let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                ),
                Span::styled("working…", subtle()),
            ]));
        }
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
            marker_block("↳ ", "steer  ", accent(), text, width)
        }
        HarnessEvent::AssistantText { text } => render_prose(text, width),
        HarnessEvent::Note { entry } => marker_block("✎ ", "note  ", muted(), entry, width),
        HarnessEvent::SystemDecision { step, reasoning } => {
            if step == "history_compaction_pass" || step == "history_compaction_skipped" {
                // Not shown in scrollback — compaction is conveyed live by the
                // animated banner and, once done, by the divider below.
                Vec::new()
            } else if step == "history_compacted" {
                // A clean boundary; everything above it is collapsed by transcript_lines.
                vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "  ───────────  ✦ context compacted  ───────────",
                        Style::default().fg(muted()),
                    )),
                    Line::from(""),
                ]
            } else {
                marker_block(
                    "⚙ ",
                    "",
                    warn(),
                    &format!("{step} — {reasoning}"),
                    width,
                )
            }
        }
        HarnessEvent::ModelError { message } => {
            marker_block("✗ ", "", danger(), message, width)
        }
        HarnessEvent::UserQuestion { questions } => {
            let text = question_text(questions).unwrap_or_else(|| "(question)".to_string());
            marker_block("? ", "", warn(), &text, width)
        }
        HarnessEvent::LaneSpawned { id, title } => marker_block(
            "→ ",
            "",
            lane(),
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
            vec![(format!("✗ {tool_name}: {error}"), Style::default().fg(danger()))],
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
                Span::styled(seg, Style::default().fg(self::text())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(self::text())),
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
        LaneStatus::Completed => ("done", success()),
        LaneStatus::Failed => ("failed", danger()),
        LaneStatus::Running => ("running", lane()),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("◆ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("lane {id} · {title} "),
            Style::default().fg(self::text()).add_modifier(Modifier::BOLD),
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
            Span::styled("● ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled(verb, Style::default().fg(self::text()).add_modifier(Modifier::BOLD)),
        ])];
    }

    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&arg, arg_budget).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                Span::styled(verb.clone(), Style::default().fg(self::text()).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(muted())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(seg, Style::default().fg(muted())),
            ]));
        }
    }
    lines
}

/// A preview of what the call will do — content for writes, a +/- diff for edits.
fn tool_call_preview(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    let green = Style::default().fg(success());
    let red = Style::default().fg(danger());
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
        "bash" => {
            // Commands can be long or multi-line; show a compact single line (first
            // line, whitespace-collapsed, capped) with an ellipsis when elided.
            let cmd = arg("command");
            let first = cmd.lines().next().unwrap_or("").trim();
            let compact = first.split_whitespace().collect::<Vec<_>>().join(" ");
            let capped: String = compact.chars().take(110).collect();
            let elided = cmd.lines().count() > 1 || capped.chars().count() < compact.chars().count();
            ("Bash".into(), if elided { format!("{capped} …") } else { capped })
        }
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
            vec![(format!("✗ {message}"), Style::default().fg(danger()))],
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

    let items: Vec<(String, Style)> = items.into_iter().filter(|(t, _)| !t.is_empty()).collect();
    // Bash output is rendered verbatim so leading whitespace / column alignment is
    // preserved (word-wrap would strip indentation); other results word-wrap.
    if tool_name == "bash" {
        result_block_verbatim(items, width)
    } else {
        result_block(items, width)
    }
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
    let summary_style = if success { subtle() } else { Style::default().fg(danger()) };
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
    let base = Style::default().fg(self::text());
    let code_block = Style::default().fg(code());
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
    let header_style = Style::default().fg(self::text()).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(self::text());
    let faint = Style::default().fg(faint());

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
    let code = Style::default().fg(code());

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
        Span::styled("? ", Style::default().fg(yellow).add_modifier(Modifier::BOLD)),
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
        let line = Line::from(vec![
            Span::styled(" ❯ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled(placeholder, Style::default().fg(faint())),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    // Wrap the prompt into display rows (honoring explicit newlines) and draw a
    // block cursor at its (row, col). The prompt glyph takes the first 2 columns;
    // continuation rows are indented to match.
    let text_w = (area.width as usize).saturating_sub(3).max(1);
    let (rows, (cursor_row, cursor_col)) = layout_input(&app.input, app.input_cursor, text_w);
    let white = Style::default().fg(self::text());
    let cursor_style = Style::default().fg(Color::Black).bg(blue());

    let mut lines = Vec::with_capacity(rows.len());
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
                "    Enter = browser sign-in  ·  Ctrl-D = device code".to_string(),
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

    // Reasoning / thinking effort
    let r_focus = focus == SettingsField::Reasoning;
    let reasoning = app
        .form_reasoning_effort
        .clone()
        .unwrap_or_else(|| "medium".to_string());
    let reasoning_hint = match app.form_provider.as_str() {
        "anthropic" => "thinking",
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

    let mut right = vec![
        Span::styled(format!("↑{}", fmt_si(st.map(|s| s.prompt_tokens).unwrap_or(0))), dim),
        Span::raw(" "),
        Span::styled(format!("↓{}", fmt_si(st.map(|s| s.completion_tokens).unwrap_or(0))), dim),
    ];
    let running_lanes = st
        .map(|s| s.lanes.iter().filter(|lane| lane.status == LaneStatus::Running).count())
        .unwrap_or(0);
    if running_lanes > 0 {
        right.push(Span::styled(format!("  ◆{}", running_lanes), Style::default().fg(lane())));
    }
    let running_lanes = st
        .map(|s| s.lanes.iter().filter(|lane| lane.status == LaneStatus::Running).count())
        .unwrap_or(0);
    if running_lanes > 0 {
        right.push(Span::styled(format!("  ◆{}", running_lanes), Style::default().fg(lane())));
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

// --- Theme: a runtime-selectable palette. Every UI color routes through the
// active theme (switch with `/theme`, persisted to config). Presets below. ---
#[derive(Clone, Copy)]
struct Theme {
    accent: Color,
    text: Color,
    muted: Color,
    faint: Color,
    success: Color,
    danger: Color,
    warn: Color,
    lane: Color,
    code: Color,
}

struct ThemePreset {
    name: &'static str,
    label: &'static str,
    theme: Theme,
}

const MIDNIGHT: Theme = Theme {
    accent: Color::Rgb(96, 165, 250),
    text: Color::Rgb(222, 225, 230),
    muted: Color::Rgb(124, 130, 142),
    faint: Color::Rgb(82, 86, 96),
    success: Color::Rgb(126, 186, 120),
    danger: Color::Rgb(224, 108, 117),
    warn: Color::Rgb(214, 182, 106),
    lane: Color::Rgb(110, 184, 200),
    code: Color::Rgb(224, 196, 132),
};

const LIGHT: Theme = Theme {
    accent: Color::Rgb(37, 99, 235),
    text: Color::Rgb(30, 41, 59),
    muted: Color::Rgb(90, 105, 125),
    faint: Color::Rgb(176, 184, 198),
    success: Color::Rgb(21, 128, 76),
    danger: Color::Rgb(193, 41, 46),
    warn: Color::Rgb(168, 113, 10),
    lane: Color::Rgb(13, 124, 156),
    code: Color::Rgb(146, 64, 14),
};

const HIGH_CONTRAST: Theme = Theme {
    accent: Color::Rgb(125, 205, 255),
    text: Color::Rgb(255, 255, 255),
    muted: Color::Rgb(190, 196, 206),
    faint: Color::Rgb(120, 126, 136),
    success: Color::Rgb(120, 240, 130),
    danger: Color::Rgb(255, 112, 112),
    warn: Color::Rgb(255, 214, 92),
    lane: Color::Rgb(120, 232, 250),
    code: Color::Rgb(245, 222, 150),
};

const EMBER: Theme = Theme {
    accent: Color::Rgb(245, 158, 11),
    text: Color::Rgb(237, 224, 212),
    muted: Color::Rgb(168, 148, 130),
    faint: Color::Rgb(96, 84, 74),
    success: Color::Rgb(158, 188, 108),
    danger: Color::Rgb(228, 110, 92),
    warn: Color::Rgb(232, 180, 90),
    lane: Color::Rgb(206, 150, 110),
    code: Color::Rgb(230, 190, 130),
};

const NORD: Theme = Theme {
    accent: Color::Rgb(136, 192, 208),
    text: Color::Rgb(216, 222, 233),
    muted: Color::Rgb(129, 140, 158),
    faint: Color::Rgb(76, 86, 106),
    success: Color::Rgb(163, 190, 140),
    danger: Color::Rgb(191, 97, 106),
    warn: Color::Rgb(235, 203, 139),
    lane: Color::Rgb(129, 161, 193),
    code: Color::Rgb(143, 188, 187),
};

const DRACULA: Theme = Theme {
    accent: Color::Rgb(189, 147, 249),
    text: Color::Rgb(248, 248, 242),
    muted: Color::Rgb(130, 138, 165),
    faint: Color::Rgb(98, 114, 164),
    success: Color::Rgb(80, 250, 123),
    danger: Color::Rgb(255, 85, 85),
    warn: Color::Rgb(241, 250, 140),
    lane: Color::Rgb(139, 233, 253),
    code: Color::Rgb(255, 184, 108),
};

const GRUVBOX: Theme = Theme {
    accent: Color::Rgb(250, 189, 47),
    text: Color::Rgb(235, 219, 178),
    muted: Color::Rgb(168, 153, 132),
    faint: Color::Rgb(102, 92, 84),
    success: Color::Rgb(184, 187, 38),
    danger: Color::Rgb(251, 73, 52),
    warn: Color::Rgb(254, 128, 25),
    lane: Color::Rgb(142, 192, 124),
    code: Color::Rgb(131, 165, 152),
};

const TOKYO_NIGHT: Theme = Theme {
    accent: Color::Rgb(122, 162, 247),
    text: Color::Rgb(192, 202, 245),
    muted: Color::Rgb(140, 148, 184),
    faint: Color::Rgb(65, 72, 104),
    success: Color::Rgb(158, 206, 106),
    danger: Color::Rgb(247, 118, 142),
    warn: Color::Rgb(224, 175, 104),
    lane: Color::Rgb(125, 207, 255),
    code: Color::Rgb(187, 154, 247),
};

const CATPPUCCIN: Theme = Theme {
    accent: Color::Rgb(203, 166, 247),
    text: Color::Rgb(205, 214, 244),
    muted: Color::Rgb(147, 153, 178),
    faint: Color::Rgb(88, 91, 112),
    success: Color::Rgb(166, 227, 161),
    danger: Color::Rgb(243, 139, 168),
    warn: Color::Rgb(249, 226, 175),
    lane: Color::Rgb(137, 220, 235),
    code: Color::Rgb(250, 179, 135),
};

const SOLARIZED: Theme = Theme {
    accent: Color::Rgb(38, 139, 210),
    text: Color::Rgb(147, 161, 161),
    muted: Color::Rgb(101, 123, 131),
    faint: Color::Rgb(68, 93, 100),
    success: Color::Rgb(133, 153, 0),
    danger: Color::Rgb(220, 50, 47),
    warn: Color::Rgb(181, 137, 0),
    lane: Color::Rgb(42, 161, 152),
    code: Color::Rgb(203, 75, 22),
};

const PRESETS: &[ThemePreset] = &[
    ThemePreset { name: "midnight", label: "Midnight (dark)", theme: MIDNIGHT },
    ThemePreset { name: "light", label: "Light", theme: LIGHT },
    ThemePreset { name: "high-contrast", label: "High-contrast", theme: HIGH_CONTRAST },
    ThemePreset { name: "ember", label: "Ember (warm)", theme: EMBER },
    ThemePreset { name: "nord", label: "Nord", theme: NORD },
    ThemePreset { name: "dracula", label: "Dracula", theme: DRACULA },
    ThemePreset { name: "gruvbox", label: "Gruvbox", theme: GRUVBOX },
    ThemePreset { name: "tokyo-night", label: "Tokyo Night", theme: TOKYO_NIGHT },
    ThemePreset { name: "catppuccin", label: "Catppuccin", theme: CATPPUCCIN },
    ThemePreset { name: "solarized", label: "Solarized Dark", theme: SOLARIZED },
];

static THEME_INDEX: AtomicUsize = AtomicUsize::new(0);

fn theme() -> Theme {
    PRESETS
        .get(THEME_INDEX.load(Ordering::Relaxed))
        .map(|p| p.theme)
        .unwrap_or(MIDNIGHT)
}

fn set_theme_index(i: usize) {
    THEME_INDEX.store(i.min(PRESETS.len().saturating_sub(1)), Ordering::Relaxed);
}

/// Apply a theme by its config name; returns false if no preset matches.
fn set_theme_by_name(name: &str) -> bool {
    if let Some(i) = PRESETS.iter().position(|p| p.name.eq_ignore_ascii_case(name.trim())) {
        set_theme_index(i);
        true
    } else {
        false
    }
}

fn current_theme_index() -> usize {
    THEME_INDEX.load(Ordering::Relaxed)
}

fn accent() -> Color {
    theme().accent
}
fn text() -> Color {
    theme().text
}
fn muted() -> Color {
    theme().muted
}
fn faint() -> Color {
    theme().faint
}
fn success() -> Color {
    theme().success
}
fn danger() -> Color {
    theme().danger
}
fn warn() -> Color {
    theme().warn
}
fn lane() -> Color {
    theme().lane
}
fn code() -> Color {
    theme().code
}

fn subtle() -> Style {
    Style::default().fg(muted())
}

fn blue() -> Color {
    accent()
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




