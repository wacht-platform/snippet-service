use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::harness::LoopInput;
use super::app::*;
use super::views::*;
use super::*;

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    // An error banner shows until the user acts again — one keypress means it's
    // been seen. Without this, `error` (cleared only on spawn) permanently masks
    // every later status line ("queued (1)…", "Session deleted", …).
    app.error = None;

    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('t')) {
        if app.screen == Screen::Term {
            if app.term_panes.len() > 1 {
                app.term_focus = (app.term_focus + 1) % app.term_panes.len();
            } else {
                app.screen = Screen::Main;
            }
        } else {
            app.open_term();
        }
        return;
    }

    if app.screen == Screen::Term {
        if key.code == KeyCode::Esc {
            app.screen = Screen::Main;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => {
                    app.quit = true;
                    return;
                }
                KeyCode::Char('n') => {
                    app.new_term();
                    return;
                }
                KeyCode::Char('w') => {
                    app.close_focused_term();
                    return;
                }
                _ => {}
            }
        }
        if let Some(bytes) = App::encode_term_key(key) {
            app.send_term_bytes(&bytes);
        }
        return;
    }

    if app.login_active {
        handle_login_key(app, key);
        return;
    }

    // While a mutating tool waits for approval (manual mode), keys are y/n/Esc only.
    if app.screen == Screen::Main && app.pending_approval().is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let _ = app.send_loop_input(LoopInput::Approve);
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Approve this and stop prompting for the rest of the RUN — run-scoped
                // only. Never persist to the config: a one-key unblock must not
                // silently remove the manual-approval gate for every future session.
                // Only offered when multiple approvals are pending (matches the bar).
                let multi = app
                    .pending_approval()
                    .map(|(_, _, _, t)| t > 1)
                    .unwrap_or(false);
                if multi {
                    let _ = app.send_loop_input(LoopInput::ApproveAll);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let _ = app.send_loop_input(LoopInput::Deny);
            }
            KeyCode::Esc => {
                let _ = app.send_loop_input(LoopInput::Interrupt);
                app.status = String::new();
            }
            _ => {}
        }
        return;
    }

    if app.screen == Screen::Main && handle_shell_key(app, key) {
        return;
    }

    // Dedicated lane navigation owns the ordinary navigation keys while open.
    if app.screen == Screen::Lanes && !key.modifiers.contains(KeyModifiers::CONTROL) {
        let count = app.state.as_ref().map(|s| s.lanes.len()).unwrap_or(0);
        match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Main;
                app.lanes_detail_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let next = app.lanes_selected_index.saturating_sub(1);
                if next != app.lanes_selected_index {
                    app.lanes_selected_index = next;
                    app.lanes_detail_scroll = 0;
                    // Keep expanded so switching lanes still shows the full report.
                }
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                let next = (app.lanes_selected_index + 1).min(count.saturating_sub(1));
                if next != app.lanes_selected_index {
                    app.lanes_selected_index = next;
                    app.lanes_detail_scroll = 0;
                }
            }
            // Scroll the RIGHT detail pane (not the lane list).
            KeyCode::PageUp => {
                app.lanes_detail_scroll = app.lanes_detail_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                app.lanes_detail_scroll = app.lanes_detail_scroll.saturating_add(10);
            }
            KeyCode::Home => app.lanes_detail_scroll = 0,
            KeyCode::End => app.lanes_detail_scroll = usize::MAX / 4,
            // Enter / Space always open full detail (idempotent expand, not a no-op toggle
            // that looks broken when content was already truncated off-screen).
            KeyCode::Enter | KeyCode::Char(' ') => {
                if app.lanes_detail_expanded {
                    // Second press collapses.
                    app.lanes_detail_expanded = false;
                    app.lanes_detail_scroll = 0;
                } else {
                    app.lanes_detail_expanded = true;
                    app.lanes_detail_scroll = 0;
                }
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
            // Open the dedicated delegated-lanes screen.
            KeyCode::Char('a') => {
                if app.screen == Screen::Lanes {
                    app.screen = Screen::Main;
                } else {
                    app.screen = Screen::Lanes;
                    app.lanes_selected_index = 0;
                    // Collapsed so Enter/Ctrl-O is an obvious expand.
                    app.lanes_detail_expanded = false;
                    app.lanes_detail_scroll = 0;
                }
                return;
            }
            // Ctrl-O opens the selected lane's full report while on the lanes screen.
            KeyCode::Char('o') => {
                if app.screen == Screen::Lanes {
                    app.lanes_detail_expanded = !app.lanes_detail_expanded;
                    app.lanes_detail_scroll = 0;
                } else {
                    app.tools_expanded = !app.tools_expanded;
                }
                return;
            }
            // Ctrl+G / Ctrl+S: steer now. Ctrl+S is often swallowed by XOFF
            // (software flow control) in the host terminal, so Ctrl+G is the
            // reliable chord. Ctrl+S still works when the tty actually delivers it.
            KeyCode::Char('s') | KeyCode::Char('g') => {
                app.steer_now();
                return;
            }
            // Cancel messages queued for after the current run.
            KeyCode::Char('x') => {
                if !app.held_queue().is_empty() {
                    let _ = app.send_loop_input(LoopInput::DropQueued);
                }
                return;
            }
            // Ctrl-K: Clear input text buffer
            KeyCode::Char('k') => {
                app.input.clear();
                app.input_cursor = 0;
                return;
            }
            // Ctrl-M: toggle mouse capture (wheel scroll vs native text select).
            KeyCode::Char('m') => {
                app.set_mouse_capture(!app.mouse_capture);
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
            KeyCode::Char('l') => {
                if app.profiles_selected_index < total {
                    let name = names[app.profiles_selected_index].clone();
                    app.toggle_delegate_profile(&name);
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

    if app.screen == Screen::RewindCheckpointSelection
        || app.screen == Screen::ForkCheckpointSelection
    {
        let count = app.state.as_ref().map(|s| s.checkpoints.len()).unwrap_or(0);
        match key.code {
            KeyCode::Up if count > 0 => {
                app.checkpoint_selected_index = if app.checkpoint_selected_index == 0 {
                    count - 1
                } else {
                    app.checkpoint_selected_index - 1
                };
            }
            KeyCode::Down if count > 0 => {
                app.checkpoint_selected_index = (app.checkpoint_selected_index + 1) % count;
            }
            KeyCode::Enter if count > 0 => app.confirm_checkpoint_selection(),
            KeyCode::Esc => {
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
                    app.status = String::new();
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
                    app.status =
                        format!("Press d again to delete \"{short}\", or Esc/↑↓ to cancel.");
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
                if app.session_known() {
                    app.spawn_loop(None, true);
                } else {
                    app.status =
                        "No saved session to resume. Start a new one with /new or type a task."
                            .to_string();
                }
            }
            KeyCode::Esc => {
                app.resume_pending_delete = false;
                app.conv_cache = None;
                app.screen = Screen::Main;
                app.status = String::new();
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
                    || selected_cmd.starts_with("/fork ")
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
                    let _ = app.send_loop_input(LoopInput::Interrupt);
                    app.status = String::new();
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
pub(crate) fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    pub(crate) fn pipe(cmd: &str, args: &[&str], text: &str) -> bool {
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
pub(crate) fn handle_login_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('c') if app.chatgpt_device_code.is_some() => {
            let code = app.chatgpt_device_code.as_ref().unwrap().user_code.clone();
            copy_to_clipboard(&code);
            app.status = format!("Copied code {code} to clipboard.");
        }
        KeyCode::Char('u') if app.chatgpt_device_code.is_some() => {
            let url = app
                .chatgpt_device_code
                .as_ref()
                .unwrap()
                .verification_url
                .clone();
            copy_to_clipboard(&url);
            app.status = "Copied sign-in URL to clipboard.".to_string();
        }
        KeyCode::Char('c') if app.xai_device_code.is_some() => {
            let code = app.xai_device_code.as_ref().unwrap().user_code.clone();
            copy_to_clipboard(&code);
            app.status = format!("Copied code {code} to clipboard.");
        }
        KeyCode::Char('u') if app.xai_device_code.is_some() => {
            let url = app
                .xai_device_code
                .as_ref()
                .unwrap()
                .verification_uri
                .clone();
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
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL) && app.form_provider == "chatgpt" =>
        {
            app.start_chatgpt_login(crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode);
        }
        KeyCode::Char('l')
            if key.modifiers.contains(KeyModifiers::CONTROL) && app.form_provider == "chatgpt" =>
        {
            app.logout_chatgpt();
        }
        KeyCode::Char('l')
            if key.modifiers.contains(KeyModifiers::CONTROL) && app.form_provider == "xai" =>
        {
            app.logout_xai();
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

/// Drive serve discovery/start while redrawing the connecting screen so startup
/// never looks frozen on an empty alternate screen.
pub(crate) async fn connect_serve_with_ui(
    terminal: &mut TuiTerminal,
    app: &mut App,
    config_path: &std::path::Path,
) -> Result<crate::serve::sidecar::DaemonInfo, String> {
    // Channel lets the discover task push phase labels without holding `&mut App`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let path = config_path.to_path_buf();
    let mut work = tokio::spawn(async move {
        crate::serve::sidecar::discover_or_start_with_progress(&path, move |phase| {
            let _ = tx.send(phase.to_string());
        })
        .await
    });

    let mut interval = tokio::time::interval(Duration::from_millis(80));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                while let Ok(phase) = rx.try_recv() {
                    app.connecting_phase = Some(phase);
                }
                app.frame = app.frame.wrapping_add(1);
                let _ = terminal.draw(|frame| render(frame, app));
            }
            result = &mut work => {
                while let Ok(phase) = rx.try_recv() {
                    app.connecting_phase = Some(phase);
                }
                return result.map_err(|e| format!("serve connect task: {e}"))?;
            }
        }
    }
}

/// Full-screen connecting state — big centered loader, no copy.

pub(crate) fn handle_question_key(app: &mut App, key: KeyEvent) -> bool {
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
            app.q_sel = if app.q_sel == 0 {
                opts.len() - 1
            } else {
                app.q_sel - 1
            };
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

