use std::io;
use std::path::PathBuf;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::config::SnippetConfig;
use crate::harness::{HarnessEvent, HarnessStatus, PlanStatus};
use crate::lanes::LaneStatus;

pub mod mascot;
pub mod tool_render;
pub mod transcript;
mod markdown;
mod theme;

mod app;
mod commands;
mod input;
mod keybindings;
mod render;
mod settings;
mod boards;
mod cards;
mod chrome;
mod panels;
mod shell;
mod term_pane;
mod views;
#[cfg(test)]
mod tests;

pub(crate) use app::*;
pub(crate) use render::*;
pub(crate) use term_pane::*;
pub(crate) use views::*;
pub(crate) use shell::*;
use cards::*;

use markdown::*;
use theme::*;
use transcript::*;

/// avoid duplication.
const HIDDEN_TOOL_ROWS: [&str; 9] = [
    "terminate_loop",
    "update_plan",
    "notify_user",
    "ask_user",
    "delegate_task",
    "cancel_delegated_task",
    "complete_goal",
    "monitor",
    "present_file",
];

/// Cap on bash/output preview lines shown inline before collapsing to a count.

const ALL_COMMANDS: &[(&str, &str)] = &[
    ("/new", "Start a new session"),
    ("/resume", "Resume a saved session"),
    ("/mission", "Open Mission Control"),
    ("/rewind", "Restore the workspace to a checkpoint"),
    ("/fork", "Branch a new session from a checkpoint or event"),
    ("/model", "Connect or change the AI model"),
    ("/compact", "Compact older conversation history now"),
    (
        "/goal",
        "Set an autonomous goal the agent drives to completion (/goal cancel to stop)",
    ),
    (
        "/mode",
        "Toggle manual approval (bash & file edits ask y/n)",
    ),
    (
        "/term",
        "Open this session's interactive shell (Ctrl-T; Esc leaves)",
    ),
    (
        "/recur",
        "Schedule a goal or message: /recur list · add every 5m <prompt> · add at 14:00 <msg> (one-off) · add <session> every 5m … · add … @plan.md · pause|on|rm <id>",
    ),
];



#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub config_path: PathBuf,
    pub config: SnippetConfig,
    /// A conversation id to resume on launch (from `--resume`).
    pub resume: Option<String>,
}


/// What to print after the TUI closes: how to get back in, and token usage.
pub(crate) struct ExitInfo {
    pub(crate) conversation: String,
    pub(crate) config_path: PathBuf,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) total_tokens: u64,
}


pub async fn run_tui(options: TuiOptions) -> Result<(), Box<dyn std::error::Error>> {
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

/// "just now" / "5m ago" / "3h ago" / "2d ago" from a last-active stamp.
///
/// One definition: the disk walk and the daemon catalog both label entries with
/// it, and two copies would drift on the boundary rounding.

pub(crate) fn relative_age(last_active: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(last_active);
    let secs = now.saturating_sub(last_active).max(0) as u64;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Clip a label to 40 chars with an ellipsis, so a long title cannot push the
/// picker's layout around.
pub(crate) fn shorten(desc: String) -> String {
    if desc.chars().count() > 40 {
        format!("{}...", desc.chars().take(37).collect::<String>())
    } else {
        desc
    }
}

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;


pub(crate) fn setup_terminal() -> Result<TuiTerminal, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture on by default so wheel scrolls the transcript/lanes.
    // Ctrl+M toggles it off when you want native drag-select (or use Shift+drag
    // in most terminals while capture is on).
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
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

pub(crate) fn restore_terminal(terminal: &mut TuiTerminal) -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

