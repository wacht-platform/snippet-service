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

    /// Run an action (POST/PUT/DELETE) and refetch `refresh` when it lands.
    #[allow(clippy::too_many_arguments)]
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
            if let Ok(mut s) = slots.lock()
                && let Some(slot) = s.get_mut(&refresh) {
                    slot.at = None;
                }
        });
    }
}

/// A panel's data: its slot key, API path, query and how long it stays fresh.
type BoardRequest = (String, String, Vec<(&'static str, String)>, Duration);

/// The key and request a panel shows, or None for session-local tabs.
pub(crate) fn board_request(app: &App, tab: PaneTab) -> Option<BoardRequest> {
    match tab {
        PaneTab::Agents => Some(("agents".into(), "/agents".into(), vec![], Duration::from_secs(10))),
        PaneTab::Tasks => Some((
            "tasks".into(),
            "/mission-control/tasks".into(),
            vec![("archived", "false".into())],
            Duration::from_secs(5),
        )),
        PaneTab::Jobs => Some(("jobs".into(), "/recurring".into(), vec![], Duration::from_secs(10))),
        PaneTab::Usage => {
            let range = USAGE_RANGES[app.usage_range % USAGE_RANGES.len()];
            let mut query = vec![];
            if let Some(since) = range_since(range.1) {
                query.push(("since", since.to_string()));
            }
            Some((format!("usage:{}", range.0), "/usage".into(), query, Duration::from_secs(20)))
        }
        PaneTab::Vault => Some(("vault".into(), "/vault".into(), vec![], Duration::from_secs(30))),
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
        if let Some((key, path, query, max_age)) = board_request(self, self.shell.tab)
            && self.boards.stale(&key, max_age) {
                self.boards.fetch(info.clone(), key, path, query);
            }
        // An open agent also needs its coordination board.
        if self.shell.tab == PaneTab::Agents
            && let Some(id) = self.selected_agent_id().filter(|_| self.shell.pane_detail.is_some()) {
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

    pub(crate) fn selected_agent_id(&self) -> Option<String> {
        let agents = self.boards.get("agents").value?;
        let list = agents.as_array()?;
        let i = self.shell.pane_detail.unwrap_or(self.shell.pane_index);
        list.get(i).and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string)
    }

    /// How many rows the current panel lists, for cursor movement.
    pub(crate) fn board_len(&self, tab: PaneTab) -> usize {
        match tab {
            PaneTab::Agents => list_len(&self.boards.get("agents")),
            PaneTab::Tasks => list_len(&self.boards.get("tasks")),
            PaneTab::Jobs => list_len(&self.boards.get("jobs")),
            PaneTab::Vault => vault_names(&self.boards.get("vault")).len(),
            _ => 0,
        }
    }

    fn board_item(&self, key: &str) -> Option<Value> {
        let v = self.boards.get(key).value?;
        v.as_array()?.get(self.shell.pane_index).cloned()
    }

    /// Panel-specific keys; true when handled.
    pub(crate) fn handle_board_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        // A pending confirmation takes the next key: y runs it, anything else
        // cancels.
        if let Some(confirm) = self.board_confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.run_confirmed(confirm.action);
            } else {
                self.set_notice("Cancelled.");
            }
            return true;
        }
        let Some(info) = self.sidecar.clone() else {
            return false;
        };
        match (self.shell.tab, key.code) {
            (PaneTab::Usage, KeyCode::Char('r')) => {
                self.usage_range = (self.usage_range + 1) % USAGE_RANGES.len();
                true
            }
            (PaneTab::Vault, KeyCode::Char('a')) => {
                self.vault_input = Some(VaultInput::default());
                true
            }
            (PaneTab::Vault, KeyCode::Char('d')) => {
                let names = vault_names(&self.boards.get("vault"));
                if let Some(name) = names.get(self.shell.pane_index) {
                    self.board_confirm = Some(Confirm {
                        prompt: format!("Delete secret {name}? y to confirm"),
                        action: ConfirmAction::DeleteSecret(name.clone()),
                    });
                }
                true
            }
            (PaneTab::Jobs, KeyCode::Char('p') | KeyCode::Char(' ')) => {
                if let Some(job) = self.board_item("jobs") {
                    let enabled = job.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                    let title = job_title(&job);
                    self.boards.act(
                        info,
                        reqwest::Method::PUT,
                        format!("/recurring/{}", urlencode(&s(&job, "id"))),
                        vec![],
                        Some(serde_json::json!({ "enabled": !enabled })),
                        format!("{} {title}.", if enabled { "Paused" } else { "Resumed" }),
                        "jobs".into(),
                    );
                }
                true
            }
            (PaneTab::Jobs, KeyCode::Char('d')) => {
                if let Some(job) = self.board_item("jobs") {
                    self.board_confirm = Some(Confirm {
                        prompt: format!("Delete {}? y to confirm", job_title(&job)),
                        action: ConfirmAction::DeleteJob(s(&job, "id"), job_title(&job)),
                    });
                }
                true
            }
            _ => false,
        }
    }

    fn run_confirmed(&mut self, action: ConfirmAction) {
        let Some(info) = self.sidecar.clone() else {
            return;
        };
        match action {
            ConfirmAction::DeleteJob(id, title) => self.boards.act(
                info,
                reqwest::Method::DELETE,
                format!("/recurring/{}", urlencode(&id)),
                vec![],
                None,
                format!("Deleted {title}."),
                "jobs".into(),
            ),
            ConfirmAction::DeleteSecret(name) => self.boards.act(
                info,
                reqwest::Method::DELETE,
                "/vault".into(),
                vec![("name", name.clone())],
                None,
                format!("Deleted {name}."),
                "vault".into(),
            ),
        }
        self.shell.pane_index = self.shell.pane_index.saturating_sub(1);
    }

    /// Typing into the vault's add prompt: the name, then the value (masked).
    pub(crate) fn vault_prompt_open(&self) -> bool {
        self.vault_input.is_some()
            && self.screen == Screen::Main
            && self.shell.focus == Focus::Pane
            && self.shell.tab == PaneTab::Vault
            && self.shell.pane_visible(self.shell.width)
    }

    pub(crate) fn vault_input_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(input) = self.vault_input.as_mut() else {
            return;
        };
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && key.code != KeyCode::Esc
        {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.vault_input = None;
                self.set_notice("Cancelled.");
            }
            KeyCode::Backspace => {
                if input.on_value { input.value.pop(); } else { input.name.pop(); }
            }
            KeyCode::Enter => {
                if !input.on_value {
                    if !input.name.trim().is_empty() {
                        input.on_value = true;
                    }
                } else if !input.value.is_empty() {
                    let input = self.vault_input.take().unwrap_or_default();
                    if let Some(info) = self.sidecar.clone() {
                        let name = input.name.trim().to_string();
                        self.boards.act(
                            info,
                            reqwest::Method::PUT,
                            "/vault".into(),
                            vec![],
                            Some(serde_json::json!({ "name": name, "value": input.value })),
                            format!("Saved {name}."),
                            "vault".into(),
                        );
                    }
                }
            }
            KeyCode::Char(c) => {
                if input.on_value {
                    input.value.push(c);
                } else if !c.is_whitespace() {
                    input.name.push(c);
                }
            }
            _ => {}
        }
    }

    /// Pasted text goes to the vault prompt when it is open; true if taken.
    pub(crate) fn vault_paste(&mut self, text: &str) -> bool {
        if !self.vault_prompt_open() {
            return false;
        }
        let Some(input) = self.vault_input.as_mut() else {
            return false;
        };
        let clean = text.trim_end_matches(['\n', '\r']);
        if input.on_value { input.value.push_str(clean); } else { input.name.push_str(clean.trim()); }
        true
    }

    fn set_notice(&self, text: &str) {
        if let Ok(mut n) = self.boards.notice.lock() {
            *n = Some(text.into());
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

/// An action that waits for a y.
pub(crate) struct Confirm {
    pub(crate) prompt: String,
    pub(crate) action: ConfirmAction,
}

pub(crate) enum ConfirmAction {
    DeleteJob(String, String),
    DeleteSecret(String),
}

/// The vault's two-step add prompt.
#[derive(Default)]
pub(crate) struct VaultInput {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) on_value: bool,
}

/// Usage ranges `r` cycles through: key, and seconds back (None = all time).
const USAGE_RANGES: [(&str, Option<i64>); 4] =
    [("all", None), ("today", Some(0)), ("7d", Some(7 * 86_400)), ("30d", Some(30 * 86_400))];

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `since` for a range: 0 means since local midnight.
fn range_since(back: Option<i64>) -> Option<i64> {
    let back = back?;
    if back == 0 {
        let now = chrono::Local::now();
        return now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|m| m.and_local_timezone(chrono::Local).single())
            .map(|m| m.timestamp());
    }
    Some(now_secs() - back)
}

fn range_label(key: &str) -> &'static str {
    match key {
        "today" => "Today",
        "7d" => "7 days",
        "30d" => "30 days",
        _ => "All time",
    }
}

fn vault_names(f: &Fetched) -> Vec<String> {
    f.value
        .as_ref()
        .and_then(|v| v.get("names"))
        .and_then(Value::as_array)
        .map(|n| n.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// 1234567 → "1.2M", as the apps print token counts.
fn si(n: i64) -> String {
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}B", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", f / 1e3)
    } else {
        n.to_string()
    }
}

fn window_label(minutes: i64) -> String {
    match minutes {
        m if m > 0 && m % 10_080 == 0 => format!("{}-week window", m / 10_080),
        m if m > 0 && m % 1_440 == 0 => format!("{}-day window", m / 1_440),
        m if m > 0 && m % 60 == 0 => format!("{}-hour window", m / 60),
        m if m > 0 => format!("{m}-minute window"),
        _ => "window".into(),
    }
}

fn list_len(f: &Fetched) -> usize {
    f.value.as_ref().and_then(Value::as_array).map_or(0, Vec::len)
}

fn job_title(job: &Value) -> String {
    let t = s(job, "title");
    if t.is_empty() { s(job, "id") } else { t }
}

/// "every 1h", "daily 02:30", "once" — as the apps word a schedule.
fn schedule_label(job: &Value) -> String {
    let Some(sch) = job.get("schedule") else {
        return String::new();
    };
    let n = |k: &str| sch.get(k).and_then(Value::as_i64).unwrap_or(0);
    match s(sch, "kind").as_str() {
        "interval" => {
            let secs = n("every_secs");
            if secs > 0 && secs % 86_400 == 0 {
                format!("every {}d", secs / 86_400)
            } else if secs > 0 && secs % 3600 == 0 {
                format!("every {}h", secs / 3600)
            } else if secs > 0 && secs % 60 == 0 {
                format!("every {}m", secs / 60)
            } else {
                format!("every {secs}s")
            }
        }
        "daily" => format!("daily {:02}:{:02}", n("hour"), n("minute")),
        "once" => "once".into(),
        other => other.to_string(),
    }
}

/// "next in 3h", "due now"; nothing for a paused job (its row says so).
fn next_run(job: &Value) -> String {
    if !job.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
        return String::new();
    }
    if job.get("queued").and_then(Value::as_bool).unwrap_or(false) {
        return "queued".into();
    }
    let next = job.get("next_run_at").and_then(Value::as_i64).unwrap_or(0);
    if next <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = next - now;
    if secs <= 0 {
        "due now".into()
    } else if secs < 3600 {
        format!("next in {}m", (secs + 59) / 60)
    } else if secs < 86_400 {
        format!("next in {}h", secs / 3600)
    } else {
        format!("next in {}d", secs / 86_400)
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

/// Tone for the daemon's statuses: tasks, agents, board kinds and jobs.
pub(crate) fn status_color(status: &str) -> ratatui::style::Color {
    match status {
        "done" | "reported" | "active" | "enabled" => success(),
        "in_progress" | "dispatched" | "draining" => warn(),
        "failed" | "blocked" | "cancelled" => danger(),
        "todo" => accent(),
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
        PaneTab::Tasks => {
            let f = app.boards.get("tasks");
            let tasks = f.value.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(i) = app.shell.pane_detail.filter(|i| *i < tasks.len()) {
                lines.extend(task_detail(&tasks[i], width));
            } else if let Some(state) = state_lines(&f, "No open tasks on the board.", tasks.is_empty()) {
                lines.extend(state);
            } else {
                for (i, t) in tasks.iter().enumerate() {
                    lines.push(row(&s(t, "title"), &short(&s(t, "id")), &s(t, "status"), width, focused && i == app.shell.pane_index));
                }
            }
        }
        PaneTab::Jobs => {
            let f = app.boards.get("jobs");
            let jobs = f.value.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(state) = state_lines(&f, "No scheduled jobs.", jobs.is_empty()) {
                lines.extend(state);
            } else {
                for (i, j) in jobs.iter().enumerate() {
                    let enabled = j.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                    let selected = focused && i == app.shell.pane_index;
                    lines.push(row(&job_title(j), "", if enabled { "enabled" } else { "paused" }, width, selected));
                    let meta = [schedule_label(j), next_run(j)].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                    lines.push(Line::from(Span::styled(format!("   {meta}"), Style::default().fg(faint())))
                        .style(Style::default().bg(if selected { surface3() } else { surface1() })));
                    let err = s(j, "last_error");
                    if !err.is_empty() {
                        lines.push(Line::from(Span::styled(format!("   {}", pad(&err, width.saturating_sub(3))), Style::default().fg(danger()))));
                    }
                }
            }
        }
        PaneTab::Usage => {
            let (key, _) = USAGE_RANGES[app.usage_range % USAGE_RANGES.len()];
            lines.push(Line::from(vec![
                Span::styled(range_label(key), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
                Span::styled("  r changes range", Style::default().fg(faint())),
            ]));
            lines.push(Line::from(""));
            let f = app.boards.get(&format!("usage:{key}"));
            let providers = f
                .value
                .as_ref()
                .and_then(|v| v.get("providers"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(state) = state_lines(&f, "No model calls in this period.", providers.is_empty()) {
                lines.extend(state);
            } else {
                for (i, p) in providers.iter().enumerate() {
                    if i > 0 {
                        lines.push(Line::from(""));
                    }
                    lines.extend(provider_lines(p, width));
                }
            }
        }
        PaneTab::Vault => {
            let f = app.boards.get("vault");
            let names = vault_names(&f);
            if let Some(input) = app.vault_input.as_ref() {
                lines.push(section("Add a secret"));
                let cursor = Span::styled("▏", Style::default().fg(accent()));
                lines.push(Line::from(vec![
                    Span::styled(" name   ", Style::default().fg(faint())),
                    Span::styled(input.name.clone(), Style::default().fg(text())),
                    if input.on_value { Span::raw("") } else { cursor.clone() },
                ]));
                if input.on_value {
                    lines.push(Line::from(vec![
                        Span::styled(" value  ", Style::default().fg(faint())),
                        // Never echo a secret, not even its length.
                        Span::styled(if input.value.is_empty() { "" } else { "••••••••" }, Style::default().fg(soft())),
                        cursor,
                    ]));
                }
                lines.push(faint_line(if input.on_value { " Enter saves · Esc cancels" } else { " Enter next · Esc cancels" }));
                lines.push(Line::from(""));
            }
            if let Some(state) = state_lines(&f, "No secrets stored. a adds one.", names.is_empty()) {
                lines.extend(state);
            } else {
                for (i, n) in names.iter().enumerate() {
                    let selected = focused && app.vault_input.is_none() && i == app.shell.pane_index;
                    lines.push(
                        Line::from(vec![
                            Span::styled(" ● ", Style::default().fg(faint())),
                            Span::styled(pad(n, width.saturating_sub(12)), Style::default().fg(text())),
                            Span::styled(" ••••••", Style::default().fg(faint())),
                        ])
                        .style(Style::default().bg(if selected { surface3() } else { surface1() })),
                    );
                }
            }
        }
        _ => {}
    }
    if let Some(c) = app.board_confirm.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(c.prompt.clone(), Style::default().fg(warn()))));
    } else if let Some(n) = app.boards.notice.lock().ok().and_then(|n| n.clone()) {
        lines.push(Line::from(""));
        lines.push(faint_line(&n));
    }
    lines
}

fn provider_lines(p: &Value, width: usize) -> Vec<Line<'static>> {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let sessions = n(p, "sessions");
    let calls = n(p, "calls");
    let mut lines = vec![
        Line::from(vec![
            Span::styled(s(p, "provider"), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    "  {sessions} session{} · {calls} call{}",
                    if sessions == 1 { "" } else { "s" },
                    if calls == 1 { "" } else { "s" }
                ),
                Style::default().fg(faint()),
            ),
        ]),
    ];
    if n(p, "total_tokens") > 0 {
        let cols = [
            ("Total", n(p, "total_tokens")),
            ("Input", n(p, "prompt_tokens")),
            ("Cached", n(p, "cache_read_tokens")),
            ("Output", n(p, "completion_tokens")),
        ];
        let cw = (width / 4).max(8);
        lines.push(Line::from(
            cols.iter().map(|(l, _)| Span::styled(pad(l, cw), Style::default().fg(faint()))).collect::<Vec<_>>(),
        ));
        lines.push(Line::from(
            cols.iter()
                .map(|(_, v)| Span::styled(pad(&si(*v), cw), Style::default().fg(text()).add_modifier(Modifier::BOLD)))
                .collect::<Vec<_>>(),
        ));
    }
    for m in p.get("models").and_then(Value::as_array).cloned().unwrap_or_default() {
        let model = s(&m, "model");
        if model.is_empty() {
            continue;
        }
        let right = format!("{} · {}×", si(n(&m, "total_tokens")), n(&m, "calls"));
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", pad(&model, width.saturating_sub(right.len() + 2))), Style::default().fg(code())),
            Span::styled(format!(" {right}"), Style::default().fg(faint())),
        ]));
    }
    let limits = p.get("rate_limits").and_then(Value::as_array).cloned().unwrap_or_default();
    if limits.is_empty() {
        let note = match p.get("rate_limits_supported").and_then(Value::as_bool) {
            Some(true) => "No rate-limit report yet.",
            Some(false) => "Subscription limits aren't exposed by this provider's API.",
            None => "No reported rate-limit usage.",
        };
        lines.extend(wrap(note, width, 0, Style::default().fg(faint())));
    }
    for r in limits {
        let label = window_label(n(&r, "window_minutes"));
        let resets = n(&r, "resets_at");
        // A window that already rolled over would state the previous one's
        // usage as current; say so instead.
        if resets > 0 && resets <= now_secs() {
            lines.push(Line::from(vec![
                Span::styled(format!(" {label}"), Style::default().fg(soft())),
                Span::styled("  rolled over · awaiting the next report", Style::default().fg(faint())),
            ]));
            continue;
        }
        let used = r.get("used_percent").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 100.0);
        let left = 100.0 - used;
        let color = if left < 20.0 { danger() } else if left < 50.0 { warn() } else { success() };
        let bar_w = width.saturating_sub(4).min(40);
        let filled = ((left / 100.0) * bar_w as f64).round() as usize;
        lines.push(Line::from(vec![
            Span::styled(format!(" {label}"), Style::default().fg(soft())),
            Span::styled(format!("  {left:.0}% left"), Style::default().fg(color)),
        ]));
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("█".repeat(filled), Style::default().fg(color)),
            Span::styled("░".repeat(bar_w - filled), Style::default().fg(faint())),
        ]));
    }
    lines
}

fn task_detail(t: &Value, width: usize) -> Vec<Line<'static>> {
    let status = s(t, "status");
    let mut lines = vec![
        Line::from(vec![
            Span::styled("‹ ", Style::default().fg(accent())),
            Span::styled(s(t, "title"), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(status.replace('_', " "), Style::default().fg(status_color(&status))),
            Span::styled(format!("  task {}", short(&s(t, "id"))), Style::default().fg(faint())),
        ]),
    ];
    for (label, key) in [("Briefing", "description"), ("Latest", "summary")] {
        let body = s(t, key);
        if !body.is_empty() {
            lines.push(Line::from(""));
            lines.push(section(label));
            lines.extend(wrap(&body, width, 1, Style::default().fg(soft())));
        }
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
    lines
}
