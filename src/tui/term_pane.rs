use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::*;
use super::*;

pub(crate) struct TermPane {
    pub(crate) id: String,
    pub(crate) vt: crate::term::VtScreen,
    pub(crate) alive: bool,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) seq: u64,
    /// True after the first incremental `out`. Later `snapshot` frames are
    /// stale full-scrollback copies and must not wipe the live screen.
    pub(crate) live: bool,
    /// Daemon PTY is not opened until the first real pane size is known.
    /// Opening at 80×24 then resizing is what scrambled fish's prompt.
    pub(crate) opened: bool,
    /// `true` = this pane should be created with `op: new`.
    pub(crate) fresh: bool,
}



impl App {
    pub(crate) fn open_term(&mut self) {
        if self.term_panes.is_empty() {
            self.term_panes.push(TermPane {
                id: "0".into(),
                vt: crate::term::VtScreen::new(80, 24),
                alive: false,
                cols: 80,
                rows: 24,
                seq: 0,
                live: false,
                opened: false,
                fresh: false,
            });
            self.term_focus = 0;
        }
        self.screen = Screen::Term;
        self.status = "Session shell · Esc leaves · Ctrl-T next · Ctrl-N new".to_string();
    }

    pub(crate) fn new_term(&mut self) {
        if self.term_panes.len() >= 8 {
            self.status = "At most 8 shells per session".to_string();
            return;
        }
        let id = self
            .term_panes
            .iter()
            .filter_map(|p| p.id.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .to_string();
        let (cols, rows) = self
            .term_panes
            .get(self.term_focus)
            .map(|p| (p.cols.max(2), p.rows.max(2)))
            .unwrap_or((80, 24));
        self.term_panes.push(TermPane {
            id,
            vt: crate::term::VtScreen::new(cols as usize, rows as usize),
            alive: false,
            cols,
            rows,
            seq: 0,
            live: false,
            opened: false,
            fresh: true,
        });
        self.term_focus = self.term_panes.len() - 1;
        self.screen = Screen::Term;
    }

    pub(crate) fn close_focused_term(&mut self) {
        if self.term_panes.is_empty() {
            self.screen = Screen::Main;
            return;
        }
        let id = self.term_panes[self.term_focus].id.clone();
        if let Some(a) = self.sidecar_attach.as_ref() {
            let _ = a.send_term(serde_json::json!({
                "wire": "term",
                "op": "close",
                "id": id,
            }));
        }
        self.term_panes.remove(self.term_focus);
        if self.term_panes.is_empty() {
            self.term_focus = 0;
            self.screen = Screen::Main;
            return;
        }
        self.term_focus = self.term_focus.min(self.term_panes.len() - 1);
    }

    pub(crate) fn close_term_at(&mut self, idx: usize) {
        if idx >= self.term_panes.len() {
            return;
        }
        let id = self.term_panes[idx].id.clone();
        if let Some(a) = self.sidecar_attach.as_ref() {
            let _ = a.send_term(serde_json::json!({
                "wire": "term",
                "op": "close",
                "id": id,
            }));
        }
        self.term_panes.remove(idx);
        if self.term_panes.is_empty() {
            self.term_focus = 0;
            self.screen = Screen::Main;
            return;
        }
        if self.term_focus > idx {
            self.term_focus -= 1;
        }
        self.term_focus = self.term_focus.min(self.term_panes.len() - 1);
    }

    /// Ctrl-D sent EOF; when the child exits, drop the tab instead of
    /// leaving a dead pane that still needs Ctrl-W.
    pub(crate) fn reap_dead_terms(&mut self) {
        let dead: Vec<usize> = self
            .term_panes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.opened && p.live && !p.alive)
            .map(|(i, _)| i)
            .collect();
        for idx in dead.into_iter().rev() {
            self.close_term_at(idx);
        }
    }

    pub(crate) fn send_term_open(&mut self) {
        let Some(pane) = self.term_panes.get_mut(self.term_focus) else {
            return;
        };
        if pane.opened {
            return;
        }
        let Some(a) = self.sidecar_attach.as_ref() else {
            // Do not latch opened — tick will retry once /attach is up.
            return;
        };
        let fresh = pane.fresh;
        if a.send_term(serde_json::json!({
            "wire": "term",
            "op": if fresh { "new" } else { "open" },
            "id": pane.id,
            "cols": pane.cols,
            "rows": pane.rows,
        }))
        .is_ok()
        {
            pane.opened = true;
        }
    }

    pub(crate) fn send_term_bytes(&self, bytes: &[u8]) {
        use base64::Engine;
        let Some(pane) = self.term_panes.get(self.term_focus) else {
            return;
        };
        if let Some(a) = self.sidecar_attach.as_ref() {
            let _ = a.send_term(serde_json::json!({
                "wire": "term",
                "op": "in",
                "id": pane.id,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                "cols": pane.cols,
                "rows": pane.rows,
            }));
        }
    }

    pub(crate) fn drain_term_frames(&mut self) {
        use base64::Engine;
        let Some(attach) = self.sidecar_attach.as_mut() else {
            return;
        };
        while let Ok(v) = attach.term_rx.try_recv() {
            let id = v
                .get("id")
                .and_then(|s| s.as_str())
                .unwrap_or("0")
                .to_string();
            let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            let idx = if let Some(i) = self.term_panes.iter().position(|p| p.id == id) {
                i
            } else {
                self.term_panes.push(TermPane {
                    id: id.clone(),
                    vt: crate::term::VtScreen::new(80, 24),
                    alive: false,
                    cols: 80,
                    rows: 24,
                    seq: 0,
                    live: false,
                    opened: true,
                    fresh: false,
                });
                self.term_panes.len() - 1
            };
            let pane = &mut self.term_panes[idx];
            if seq != 0 && seq == pane.seq {
                continue;
            }
            if seq != 0 {
                pane.seq = seq;
            }
            pane.alive = v
                .get("alive")
                .and_then(|a| a.as_bool())
                .unwrap_or(pane.alive);
            let op = v.get("op").and_then(|o| o.as_str()).unwrap_or("");
            let data = v
                .get("data")
                .and_then(|d| d.as_str())
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .unwrap_or_default();
            if op == "snapshot" {
                // Raw PTY history replayed into a sized screen is what
                // glued `ls` onto the prompt. Incremental `out` is truth.
                continue;
            }
            if op == "out" {
                pane.live = true;
            }
            if !data.is_empty() {
                pane.vt.feed(&data);
            }
            // DSR/DA is answered on the daemon. Two clients writing
            // CSI 6n replies stacked fish's prompt.
            let _ = pane.vt.take_replies();
        }
    }

    pub(crate) fn encode_term_key(key: KeyEvent) -> Option<Vec<u8>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(c) if ctrl => {
                let b = c.to_ascii_lowercase() as u8;
                if (b'a'..=b'z').contains(&b) {
                    Some(vec![b - b'a' + 1])
                } else {
                    Some(c.to_string().into_bytes())
                }
            }
            KeyCode::Char(c) => Some(c.to_string().into_bytes()),
            KeyCode::Enter => Some(vec![b'\r']),
            KeyCode::Tab => Some(vec![b'\t']),
            KeyCode::Backspace => Some(vec![0x7f]),
            KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
            KeyCode::Up => Some(b"\x1b[A".to_vec()),
            KeyCode::Down => Some(b"\x1b[B".to_vec()),
            KeyCode::Right => Some(b"\x1b[C".to_vec()),
            KeyCode::Left => Some(b"\x1b[D".to_vec()),
            KeyCode::Home => Some(b"\x1b[H".to_vec()),
            KeyCode::End => Some(b"\x1b[F".to_vec()),
            KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
            KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
            KeyCode::Esc => None,
            _ => None,
        }
    }


}
