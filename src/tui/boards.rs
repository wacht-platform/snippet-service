//! Daemon-backed panels in the side pane: agents, the task board, scheduled
//! jobs, usage and the vault — the views the apps show from the daemon's API.
//!
//! Each panel's data is fetched in the background into a shared slot, keyed by
//! what was asked for, and refreshed while the panel is on screen. Rendering
//! reads whatever the slot holds, so a slow daemon never blocks a frame.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::shell::{Focus, PaneTab};
use super::theme::*;
use super::{App, Screen};

/// One fetch's outcome.
#[derive(Clone, Default)]
pub(crate) struct Fetched {
    pub(crate) value: Option<Value>,
    pub(crate) error: Option<String>,
    pub(crate) loading: bool,
    pub(crate) at: Option<Instant>,
}

/// Fetched panel data, shared with the background tasks that fill it.
#[derive(Clone, Default)]
pub(crate) struct Boards {
    slots: Arc<Mutex<HashMap<String, Fetched>>>,
    /// The outcome of the last action (pause, delete…), shown under the panel.
    pub(crate) notice: Arc<Mutex<Option<String>>>,
}

impl Boards {
    pub(crate) fn get(&self, key: &str) -> Fetched {
        self.slots.lock().ok().and_then(|s| s.get(key).cloned()).unwrap_or_default()
    }

    fn stale(&self, key: &str, max_age: Duration) -> bool {
        let f = self.get(key);
        !f.loading && f.at.is_none_or(|at| at.elapsed() >= max_age)
    }

    /// Fetch `path` into `key` in the background.
    pub(crate) fn fetch(
        &self,
        info: crate::serve::sidecar::DaemonInfo,
        key: String,
        path: String,
        query: Vec<(&'static str, String)>,
    ) {
        if let Ok(mut s) = self.slots.lock() {
            s.entry(key.clone()).or_default().loading = true;
        }
        let slots = self.slots.clone();
        tokio::spawn(async move {
            let result = crate::serve::sidecar::api_json(
                &info,
                reqwest::Method::GET,
                &path,
                &query,
                None,
            )
            .await;
            if let Ok(mut s) = slots.lock() {
                let slot = s.entry(key).or_default();
                slot.loading = false;
                slot.at = Some(Instant::now());
                match result {
                    Ok(v) => {
                        slot.value = Some(v);
                        slot.error = None;
                    }
                    // Keep the last good data on screen; say what failed.
                    Err(e) => slot.error = Some(e),
                }
            }
        });
    }

    /// Put data in a slot directly (tests render panels without a daemon).
    #[cfg(test)]
    pub(crate) fn seed(&self, key: &str, value: Value) {
        if let Ok(mut s) = self.slots.lock() {
            s.insert(key.into(), Fetched { value: Some(value), at: Some(Instant::now()), ..Default::default() });
        }
    }

    /// Mark `key` for refetch on the next poll.
    pub(crate) fn invalidate(&self, key: &str) {
        if let Ok(mut s) = self.slots.lock() {
            if let Some(slot) = s.get_mut(key) {
                slot.at = None;
            }
        }
    }

    /// Run an action (POST/PUT/DELETE) and refetch `refresh` when it lands.
    pub(crate) fn act(
        &self,
        info: crate::serve::sidecar::DaemonInfo,
        method: reqwest::Method,
        path: String,
        query: Vec<(&'static str, String)>,
        body: Option<Value>,
        done: String,
        refresh: String,
    ) {
        let notice = self.notice.clone();
        let slots = self.slots.clone();
        tokio::spawn(async move {
            let result =
                crate::serve::sidecar::api_json(&info, method, &path, &query, body).await;
            if let Ok(mut n) = notice.lock() {
                *n = Some(match result {
                    Ok(_) => done,
                    Err(e) => e,
                });
            }
            if let Ok(mut s) = slots.lock() {
                if let Some(slot) = s.get_mut(&refresh) {
                    slot.at = None;
                }
            }
        });
    }
}

/// The key and request a panel shows, or None for session-local tabs.
pub(crate) fn board_request(
    app: &App,
    tab: PaneTab,
) -> Option<(String, String, Vec<(&'static str, String)>, Duration)> {
    let _ = app;
    match tab {
        PaneTab::Agents => Some(("agents".into(), "/agents".into(), vec![], Duration::from_secs(10))),
        _ => None,
    }
}

impl App {
    /// Keep the visible panel's data fresh; called every tick.
    pub(crate) fn poll_boards(&mut self) {
        let Some(info) = self.sidecar.clone() else {
            return;
        };
        if !self.shell.pane_visible(self.shell.width) {
            return;
        }
        if let Some((key, path, query, max_age)) = board_request(self, self.shell.tab) {
            if self.boards.stale(&key, max_age) {
                self.boards.fetch(info.clone(), key, path, query);
            }
        }
        // An open agent also needs its coordination board.
        if self.shell.tab == PaneTab::Agents {
            if let Some(id) = self.selected_agent_id().filter(|_| self.shell.pane_detail.is_some()) {
                let key = format!("board:{id}");
                if self.boards.stale(&key, Duration::from_secs(10)) {
                    self.boards.fetch(
                        info,
                        key,
                        format!("/agents/{}/board", urlencode(&id)),
                        vec![("limit", "40".into())],
                    );
                }
            }
        }
    }

    pub(crate) fn selected_agent_id(&self) -> Option<String> {
        let agents = self.boards.get("agents").value?;
        let list = agents.as_array()?;
        let i = self.shell.pane_detail.unwrap_or(self.shell.pane_index);
        list.get(i).and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string)
    }

    /// How many rows the current panel lists, for cursor movement.
    pub(crate) fn board_len(&self, tab: PaneTab) -> usize {
        match tab {
            PaneTab::Agents => self.boards.get("agents").value.and_then(|v| v.as_array().map(Vec::len)).unwrap_or(0),
            _ => 0,
        }
    }

    /// Open the dedicated Mission Control session in the main view. The HTTP
    /// call runs on the next tick (key handlers are synchronous).
    pub(crate) fn open_mission_control(&mut self) {
        self.pending_mission_control = true;
        self.status = "Opening Mission Control…".into();
    }

    pub(crate) async fn apply_pending_mission_control(&mut self) {
        if !std::mem::take(&mut self.pending_mission_control) {
            return;
        }
        let Some(info) = self.sidecar.clone() else {
            self.status = "snippet serve is not available".into();
            return;
        };
        match crate::serve::sidecar::open_mission_control(&info).await {
            Ok(id) => {
                // Mission Control lives outside this workspace: attach by its
                // own path. Its task and report envelopes render as cards.
                self.switch_conversation("mission-control");
                self.active_state_path = crate::config::workspaces_root().join(&id);
                self.shell.focus = Focus::Composer;
                self.screen = Screen::Main;
                self.spawn_loop(None, true);
                self.status = "Mission Control".into();
            }
            Err(e) => self.status = format!("Mission Control: {e}"),
        }
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub(crate) fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .map(|x| match x {
            Value::String(s) => s.trim().to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count > width {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

/// A status word's tone: done green, working amber, failed red, else faint.
pub(crate) fn status_color(status: &str) -> ratatui::style::Color {
    match status.to_lowercase().as_str() {
        "done" | "completed" | "complete" | "reported" | "idle" | "enabled" => success(),
        "running" | "active" | "working" | "in_progress" | "dispatched" | "claimed" | "busy" => warn(),
        "failed" | "blocked" | "error" | "cancelled" => danger(),
        "waiting_for_input" | "pending" | "queued" => accent(),
        _ => faint(),
    }
}

fn faint_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(faint())))
}

/// A list row: status dot, title, and a right-aligned status word.
fn row(title: &str, meta: &str, status: &str, width: usize, selected: bool) -> Line<'static> {
    let label = status.replace('_', " ");
    let right = if label.is_empty() { 0 } else { label.chars().count() + 1 };
    let meta_w = if meta.is_empty() { 0 } else { meta.chars().count().min(14) + 1 };
    let title_w = width.saturating_sub(3 + right + meta_w);
    let mut spans = vec![
        Span::styled(" ● ", Style::default().fg(status_color(status))),
        Span::styled(pad(title, title_w), Style::default().fg(text())),
    ];
    if meta_w > 0 {
        spans.push(Span::styled(format!(" {}", pad(meta, meta_w - 1)), Style::default().fg(faint())));
    }
    if right > 0 {
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(status_color(status))));
    }
    Line::from(spans).style(Style::default().bg(if selected { surface3() } else { surface1() }))
}

/// Wrap `text` to `width`, prefixed by `indent` spaces.
fn wrap(text: &str, width: usize, indent: usize, style: Style) -> Vec<Line<'static>> {
    let avail = width.saturating_sub(indent).max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > avail {
                out.push(Line::from(Span::styled(format!("{}{line}", " ".repeat(indent)), style)));
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(Line::from(Span::styled(format!("{}{line}", " ".repeat(indent)), style)));
    }
    out
}

fn section(label: &str) -> Line<'static> {
    Line::from(Span::styled(label.to_string(), Style::default().fg(faint()).add_modifier(Modifier::BOLD)))
}

/// Loading / error / empty states shared by every panel.
fn state_lines(f: &Fetched, empty: &str, is_empty: bool) -> Option<Vec<Line<'static>>> {
    if f.value.is_none() {
        return Some(vec![match &f.error {
            Some(e) => Line::from(Span::styled(format!("Couldn't load: {e}"), Style::default().fg(danger()))),
            None => faint_line("Loading…"),
        }]);
    }
    if is_empty {
        return Some(vec![faint_line(empty)]);
    }
    None
}

/// The body of a daemon-backed panel.
pub(crate) fn board_lines(app: &App, tab: PaneTab, width: usize) -> Vec<Line<'static>> {
    let focused = app.shell.focus == Focus::Pane;
    let mut lines = Vec::new();
    match tab {
        PaneTab::Agents => {
            let f = app.boards.get("agents");
            let agents = f.value.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(i) = app.shell.pane_detail.filter(|i| *i < agents.len()) {
                lines.extend(agent_detail(app, &agents[i], width));
            } else if let Some(state) = state_lines(&f, "No agents registered.", agents.is_empty()) {
                lines.extend(state);
            } else {
                for (i, a) in agents.iter().enumerate() {
                    let name = if s(a, "display_name").is_empty() { s(a, "id") } else { s(a, "display_name") };
                    lines.push(row(&name, &s(a, "role"), &s(a, "status"), width, focused && i == app.shell.pane_index));
                }
            }
        }
        _ => {}
    }
    if let Some(n) = app.boards.notice.lock().ok().and_then(|n| n.clone()) {
        lines.push(Line::from(""));
        lines.push(faint_line(&n));
    }
    lines
}

fn agent_detail(app: &App, a: &Value, width: usize) -> Vec<Line<'static>> {
    let id = s(a, "id");
    let name = if s(a, "display_name").is_empty() { id.clone() } else { s(a, "display_name") };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("‹ ", Style::default().fg(accent())),
            Span::styled(name, Style::default().fg(text()).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", s(a, "status")), Style::default().fg(status_color(&s(a, "status")))),
        ]),
        faint_line(&[s(a, "role"), s(a, "kind"), id.clone()].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ")),
    ];
    let caps: Vec<String> = a
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|c| c.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !caps.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("Capabilities"));
        lines.extend(wrap(&caps.join(", "), width, 1, Style::default().fg(soft())));
    }
    lines.push(Line::from(""));
    lines.push(section("Board"));
    let f = app.boards.get(&format!("board:{id}"));
    let entries = f
        .value
        .as_ref()
        .and_then(|v| v.get("entries"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(state) = state_lines(&f, "Nothing dispatched, reported or noted yet.", entries.is_empty()) {
        lines.extend(state);
    } else {
        for e in entries.iter().take(40) {
            let kind = s(e, "kind");
            lines.push(Line::from(vec![
                Span::styled(" ● ", Style::default().fg(status_color(&kind))),
                Span::styled(kind.clone(), Style::default().fg(status_color(&kind))),
                Span::styled(
                    {
                        let ws = s(e, "workspace");
                        let tail = ws.rsplit('/').next().unwrap_or("").to_string();
                        if tail.is_empty() { String::new() } else { format!("  {tail}") }
                    },
                    Style::default().fg(faint()),
                ),
            ]));
            lines.extend(wrap(&s(e, "summary"), width, 3, Style::default().fg(soft())));
        }
    }
    let _ = short;
    lines
}
