
use crate::harness::{HarnessStatus, LoopInput};
use super::app::*;

impl App {
    pub(crate) fn input_clear(&mut self) {
        self.input.truncate(0);
        self.input_cursor = 0;
        self.pasted_blocks.clear();
        self.attachments.clear();
    }

    /// Replace the whole input (e.g. from a slash-command autocomplete) and put
    /// the cursor at the end.
    pub(crate) fn input_set(&mut self, value: String) {
        self.input_cursor = value.chars().count();
        self.input = value;
    }

    pub(crate) fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Replace the input from a recalled history entry (clears any paste chips).
    pub(crate) fn recall_set(&mut self, value: String) {
        self.pasted_blocks.clear();
        self.input_set(value);
    }

    /// True when the cursor sits on the first / last line of the input.
    pub(crate) fn input_on_first_line(&self) -> bool {
        let end = self.input_byte_at(self.input_cursor);
        !self.input[..end].contains('\n')
    }
    pub(crate) fn input_on_last_line(&self) -> bool {
        let start = self.input_byte_at(self.input_cursor);
        !self.input[start..].contains('\n')
    }

    /// Recall the previous (older) history entry. Returns false when there's none.
    pub(crate) fn recall_history_prev(&mut self) -> bool {
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
    pub(crate) fn recall_history_next(&mut self) -> bool {
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
    pub(crate) fn input_up(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let line_start = chars[..cur]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if line_start == 0 {
            return;
        }
        let col = cur - line_start;
        let prev_end = line_start - 1;
        let prev_start = chars[..prev_end]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input_cursor = prev_start + col.min(prev_end - prev_start);
    }
    pub(crate) fn input_down(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let cur = self.input_cursor.min(chars.len());
        let line_start = chars[..cur]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = cur - line_start;
        let Some(nl) = chars[cur..]
            .iter()
            .position(|&c| c == '\n')
            .map(|i| cur + i)
        else {
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
    pub(crate) fn input_byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub(crate) fn input_insert(&mut self, c: char) {
        let at = self.input_byte_at(self.input_cursor);
        self.input.insert(at, c);
        self.input_cursor += 1;
    }

    /// Handle a text paste. A big / multi-line paste collapses to a compact chip
    /// in the input (expanded on send) so it doesn't overflow; a small single-line
    /// paste is inserted inline. (Screenshots come via Ctrl+V — see
    /// `paste_clipboard_image` — not through here.)
    pub(crate) fn input_paste(&mut self, text: &str) {
        // Dragging a file/screenshot into the terminal pastes its path — attach it.
        if let Some(path) = Self::dropped_file(text) {
            self.attach_dropped(&path);
            return;
        }
        let text = text.replace('\r', "");
        let lines = text.lines().count().max(1);
        if lines > 1 || text.chars().count() > 200 {
            let n = self.pasted_blocks.len() + 1;
            let marker = format!(
                "[Pasted #{n} · {lines} line{}]",
                if lines == 1 { "" } else { "s" }
            );
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
    pub(crate) fn paste_clipboard_image(&mut self) {
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
            let ran = std::process::Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .output();
            matches!(&ran, Ok(out) if out.status.success())
        } else {
            // Linux: try Wayland's wl-paste, then X11's xclip. Each writes PNG
            // bytes to stdout; capture into the dest file.
            let mut wrote = false;
            for (cmd, args) in [
                ("wl-paste", vec!["--type", "image/png"]),
                (
                    "xclip",
                    vec!["-selection", "clipboard", "-t", "image/png", "-o"],
                ),
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
        let ok = ok
            && std::fs::metadata(&dest)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
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
        self.attachments
            .push((true, dest.display().to_string(), "screenshot".to_string()));
    }

    /// If `text` is exactly one existing file path (as a terminal pastes when you
    /// drag a file in — possibly quoted or with backslash-escaped spaces), return it.
    pub(crate) fn dropped_file(text: &str) -> Option<std::path::PathBuf> {
        let mut s = text.trim();
        if s.len() >= 2
            && ((s.starts_with('\'') && s.ends_with('\''))
                || (s.starts_with('"') && s.ends_with('"')))
        {
            s = &s[1..s.len() - 1];
        }
        if s.is_empty() {
            return None;
        }
        let unescaped = s.replace("\\ ", " ").replace("\\\\", "\\");
        let p = std::path::PathBuf::from(&unescaped);
        if p.is_file() { Some(p) } else { None }
    }

    /// Copy a dropped file into the workspace scratch dir and add a chip that
    /// expands to its path on send (images → read_image, others → read).
    pub(crate) fn attach_dropped(&mut self, src: &std::path::Path) {
        let is_img = matches!(
            src.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif")
        );
        let subdir = if is_img { "images" } else { "files" };
        let dir = self
            .options
            .config
            .workspace
            .join(".snippet")
            .join("scratch")
            .join(subdir);
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
        self.attachments
            .push((is_img, dest.display().to_string(), fname.to_string()));
        /* attach pill is enough */
    }

    /// Expand any paste chips in the current input back to their real content.
    /// A paste bigger than this goes to a scratch FILE and is sent as an
    /// attachment path instead of inline text — the agent greps/reads it
    /// surgically, and the conversation doesn't carry the whole wall forever.
    pub(crate) const PASTE_ATTACH_CHARS: usize = 4000;
    pub(crate) const PASTE_ATTACH_LINES: usize = 60;

    pub(crate) fn expand_input(&self) -> String {
        let mut out = self.input.clone();
        for (marker, content) in &self.pasted_blocks {
            let lines = content.lines().count();
            let big = content.chars().count() > Self::PASTE_ATTACH_CHARS
                || lines > Self::PASTE_ATTACH_LINES;
            let replacement = if big {
                match self.write_paste_file(content) {
                    Ok(path) => format!(
                        "[attached file — pasted text ({lines} lines); read it at this exact path: {path}]"
                    ),
                    // Couldn't write the scratch file — fall back to inline so
                    // the message still carries the content.
                    Err(_) => content.clone(),
                }
            } else {
                content.clone()
            };
            out = out.replace(marker, &replacement);
        }
        out
    }

    /// Persist one big pasted block under the workspace scratch dir and return
    /// its path (same lifecycle as attached screenshots).
    pub(crate) fn write_paste_file(&self, content: &str) -> std::io::Result<String> {
        let dir = self
            .options
            .config
            .workspace
            .join(".snippet")
            .join("scratch")
            .join("pastes");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!(
            "paste-{}.txt",
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        std::fs::write(&path, content)?;
        Ok(path.display().to_string())
    }

    /// The message to send: the expanded input plus any pending attachments, each
    /// appended as an explicit marker (images → read_image, files → read) so the
    /// agent opens them. Attachments live outside the input text and are cleared
    /// with it on send.
    pub(crate) fn message_for_send(&self) -> String {
        let mut out = self.expand_input();
        // Exactly the marker shape `strip_attachment_markers` hides from the
        // transcript: `[attached image — …]` / `[attached file — …]` (the em-dash
        // must immediately follow "image"/"file " — no filename before it).
        for (is_img, path, _name) in &self.attachments {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&if *is_img {
                format!("[attached image — call view_image on this exact path to view it: {path}]")
            } else {
                format!("[attached file — read it at this exact path: {path}]")
            });
        }
        out
    }

    pub(crate) fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            // Nothing to the left in the text — pop the most recent attachment so
            // a mis-attached file can be removed (the pill count drops by one).
            let _ = self.attachments.pop();
            return;
        }
        let start = self.input_byte_at(self.input_cursor - 1);
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor -= 1;
    }

    pub(crate) fn input_delete(&mut self) {
        if self.input_cursor >= self.input_len() {
            return;
        }
        let start = self.input_byte_at(self.input_cursor);
        let end = self.input_byte_at(self.input_cursor + 1);
        self.input.replace_range(start..end, "");
    }

    /// Char index of the previous word boundary: skip whitespace, then word chars.
    pub(crate) fn input_prev_word(&self) -> usize {
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
    pub(crate) fn input_next_word(&self) -> usize {
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

    pub(crate) fn input_delete_word_back(&mut self) {
        let target = self.input_prev_word();
        if target == self.input_cursor {
            return;
        }
        let start = self.input_byte_at(target);
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(start..end, "");
        self.input_cursor = target;
    }

    pub(crate) fn input_delete_word_forward(&mut self) {
        let target = self.input_next_word();
        if target == self.input_cursor {
            return;
        }
        let start = self.input_byte_at(self.input_cursor);
        let end = self.input_byte_at(target);
        self.input.replace_range(start..end, "");
    }

    pub(crate) fn input_delete_to_start(&mut self) {
        let end = self.input_byte_at(self.input_cursor);
        self.input.replace_range(0..end, "");
        self.input_cursor = 0;
    }

    pub(crate) fn input_delete_to_end(&mut self) {
        let start = self.input_byte_at(self.input_cursor);
        self.input.truncate(start);
    }

    pub(crate) fn input_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    pub(crate) fn input_right(&mut self) {
        if self.input_cursor < self.input_len() {
            self.input_cursor += 1;
        }
    }

    pub(crate) fn submit(&mut self) {
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
                self.status = String::new();
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
        // While the agent is executing, hold the message (shown above the prompt).
        // Do not toast the footer — the input-area queue preview is enough.
        // Ctrl+S steers immediately instead of queueing (see handle_key).
        if self.agent_busy() {
            if let Some(st) = self.state.as_mut() {
                let item = crate::harness::QueuedInput {
                    id: uuid::Uuid::new_v4().to_string(),
                    text: text.clone(),
                };
                st.queued_inputs.push(item.clone());
                if let Err(error) = self.send_loop_input(LoopInput::Queue(item)) {
                    self.error = Some(error);
                }
            }
            return;
        }

        self.submit_text(text);
    }

    /// Send one input to the loop now (answer a pending question, steer an idle
    /// resident loop, or spawn a fresh run if none is alive). Used by `submit` when
    /// not busy and by the queue flush.
    pub(crate) fn submit_text(&mut self, text: String) {
        self.scroll = 0;
        if self.agent_alive() {
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
            if let Err(error) = self.send_loop_input(input) {
                self.error = Some(error);
            } else {
                // Hold busy locally until state catches up so a fast follow-up
                // queues instead of steering mid-run.
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

    /// Send the current input immediately as a mid-run steer (or normal submit if idle).
    /// Bound to Ctrl+G (and Ctrl+S when the host tty delivers it).
    pub(crate) fn steer_now(&mut self) {
        // Composer first. If it's empty but a message is already queued, that
        // queued text IS the steer — Ctrl+G must not no-op.
        let text = self.message_for_send().trim().to_string();
        if text.is_empty() {
            if self.held_queue().is_empty() {
                return;
            }
            let item = self.held_queue().first().cloned();
            if let Some(item) = item {
                self.pending_steers.push(item.text);
                let _ = self.send_loop_input(LoopInput::SteerQueued(item.id));
            }
            self.input_clear();
            self.scroll = 0;
            return;
        } else if !self.held_queue().is_empty() {
            let _ = self.send_loop_input(LoopInput::DropQueued);
        }
        if text.starts_with('/') {
            self.handle_slash_command(&text);
            self.input_clear();
            return;
        }
        // Record history like submit.
        if self.input_history.last().map(String::as_str) != Some(text.as_str()) {
            self.input_history.push(text.clone());
        }
        self.history_pos = None;
        self.input_clear();
        self.scroll = 0;
        // Keep the words on screen until the harness writes Steer / UserInput.
        self.pending_steers.push(text.clone());
        // Deliver now even if busy — harness folds UserMessage into [steer] mid-run.
        // `submit_text` retains its local busy marker until persisted state catches
        // up, so a fast Enter after this steer queues instead of becoming a second,
        // timing-dependent steer.
        self.submit_text(text);
    }

    pub(crate) fn prune_pending_steers(&mut self) {
        if self.pending_steers.is_empty() {
            return;
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let mut remaining = Vec::new();
        for pending in self.pending_steers.drain(..) {
            let still_waiting = !state.events.iter().rev().any(|e| match e {
                crate::harness::HarnessEvent::Steer { text }
                | crate::harness::HarnessEvent::UserInput { text } => text == &pending,
                _ => false,
            });
            if still_waiting {
                remaining.push(pending);
            }
        }
        self.pending_steers = remaining;
    }

    pub(crate) fn held_queue(&self) -> &[crate::harness::QueuedInput] {
        self.state
            .as_ref()
            .map(|s| s.queued_inputs.as_slice())
            .unwrap_or(&[])
    }


}
