use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use super::boards::s;
use super::shell::{Focus, PaneTab};
use super::theme::*;
use super::App;

#[derive(Default)]
pub(crate) struct PanelState {
    pub(crate) files_path: Option<PathBuf>,
    inbox_cursor: Option<(i64, u64)>,
    inbox_polled: Option<Instant>,
    pub(crate) inbox_unseen: usize,
}

const DAY: i64 = 24 * 60 * 60;

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
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

fn faint_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(faint())))
}

fn selected_bg(selected: bool) -> Style {
    Style::default().bg(if selected { surface3() } else { surface1() })
}

fn back_line(title: &str, meta: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("‹ ", Style::default().fg(accent())),
        Span::styled(title.to_string(), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
        Span::styled(if meta.is_empty() { String::new() } else { format!("  {meta}") }, Style::default().fg(faint())),
    ])
}

fn state(f: &super::boards::Fetched, empty: &str, is_empty: bool) -> Option<Vec<Line<'static>>> {
    if f.value.is_none() {
        return Some(vec![match &f.error {
            Some(e) => Line::from(Span::styled(format!("Couldn't load: {e}"), Style::default().fg(danger()))),
            None => faint_line("Loading…"),
        }]);
    }
    is_empty.then(|| vec![faint_line(empty)])
}

fn windowed(rows: Vec<Line<'static>>, selected: usize, visible: usize) -> Vec<Line<'static>> {
    let visible = visible.max(3);
    let start = selected.saturating_sub(visible.saturating_sub(2)).min(rows.len().saturating_sub(visible));
    rows.into_iter().skip(start).take(visible).collect()
}

fn notification_label(e: &Value) -> (&'static str, ratatui::style::Color) {
    match s(e, "kind").as_str() {
        "waiting" => ("Needs you", text()),
        "done" => ("Finished", success()),
        "error" => ("Failed", danger()),
        "idle" => ("Stopped", soft()),
        "direct_message.sent" => ("Message", accent()),
        "task.message" => ("Task message", accent()),
        _ => ("Update", soft()),
    }
}

fn notification_title(e: &Value) -> String {
    let title = s(e, "title");
    if !title.is_empty() {
        return title;
    }
    let body = e.get("payload").map(|p| s(p, "body")).unwrap_or_default();
    if !body.is_empty() {
        return body.lines().next().unwrap_or("").to_string();
    }
    let dest = e.get("destination").cloned().unwrap_or(Value::Null);
    s(&dest, "id")
}

fn ago(ts: i64) -> String {
    let secs = (now_secs() - ts).max(0);
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn git_color(code: &str) -> ratatui::style::Color {
    match code {
        "M" => warn(),
        "A" | "?" => success(),
        "D" => danger(),
        "R" | "C" => accent(),
        _ => soft(),
    }
}

impl App {
    fn files_root(&self) -> PathBuf {
        self.panels
            .files_path
            .clone()
            .unwrap_or_else(|| self.options.config.workspace.clone())
    }

    fn panel_key(&self, tab: PaneTab) -> Option<String> {
        match tab {
            PaneTab::Inbox => Some("inbox".into()),
            PaneTab::Procs => Some(format!("procs:{}", self.current_session_id())),
            PaneTab::Git => Some(format!("git:{}", self.current_session_id())),
            PaneTab::Files => Some(format!("fs:{}", self.files_root().display())),
            _ => None,
        }
    }

    fn panel_list(&self, tab: PaneTab) -> Vec<Value> {
        let Some(key) = self.panel_key(tab) else {
            return Vec::new();
        };
        let v = self.boards.get(&key).value.unwrap_or(Value::Null);
        let list = match tab {
            PaneTab::Inbox => v.get("events"),
            PaneTab::Procs => v.get("processes"),
            PaneTab::Git => v.get("files"),
            PaneTab::Files => v.get("entries"),
            _ => None,
        };
        let mut list = list.and_then(Value::as_array).cloned().unwrap_or_default();
        if tab == PaneTab::Inbox {
            list.reverse();
        }
        list
    }

    pub(crate) fn panel_len(&self, tab: PaneTab) -> usize {
        self.panel_list(tab).len()
    }

    fn panel_item(&self, tab: PaneTab) -> Option<Value> {
        let i = self.shell.pane_detail.unwrap_or(self.shell.pane_index);
        self.panel_list(tab).get(i).cloned()
    }

    fn inbox_query() -> Vec<(&'static str, String)> {
        vec![("since_created_at", (now_secs() - DAY).max(0).to_string()), ("limit", "500".into())]
    }

    pub(crate) fn poll_panels(&mut self, info: &crate::serve::sidecar::DaemonInfo) {
        self.poll_inbox_alerts(info);
        if !self.shell.pane_visible(self.shell.width) {
            return;
        }
        let tab = self.shell.tab;
        let Some(key) = self.panel_key(tab) else {
            return;
        };
        let session = self.current_session_id();
        let (method, path, query, body, max_age) = match tab {
            PaneTab::Inbox => (reqwest::Method::GET, "/notifications".to_string(), Self::inbox_query(), None, 15),
            PaneTab::Procs => (reqwest::Method::POST, "/bg".into(), vec![], Some(json!({ "session": session })), 3),
            PaneTab::Git => (reqwest::Method::POST, "/git/status".into(), vec![], Some(json!({ "session": session })), 5),
            PaneTab::Files => (
                reqwest::Method::GET,
                "/fs".into(),
                vec![("path", self.files_root().display().to_string())],
                None,
                10,
            ),
            _ => return,
        };
        if self.boards.stale(&key, Duration::from_secs(max_age)) {
            self.boards.fetch_with(info.clone(), key, method, path, query, body);
        }
        let Some(item) = self.shell.pane_detail.and_then(|_| self.panel_item(tab)) else {
            return;
        };
        let detail = match tab {
            PaneTab::Procs => Some((
                format!("proclog:{session}:{}", s(&item, "id")),
                reqwest::Method::POST,
                "/bg/log".to_string(),
                vec![],
                Some(json!({ "session": session, "id": s(&item, "id"), "tail": 400 })),
                if item.get("running").and_then(Value::as_bool).unwrap_or(false) { 2 } else { 3600 },
            )),
            PaneTab::Git => {
                let untracked = item.get("untracked").and_then(Value::as_bool).unwrap_or(false);
                let staged = !untracked && !item.get("unstaged").and_then(Value::as_bool).unwrap_or(false);
                Some((
                    format!("diff:{session}:{}:{staged}", s(&item, "path")),
                    reqwest::Method::POST,
                    "/git/diff".to_string(),
                    vec![],
                    Some(json!({ "session": session, "file": s(&item, "path"), "staged": staged, "untracked": untracked })),
                    5,
                ))
            }
            PaneTab::Files if !item.get("is_dir").and_then(Value::as_bool).unwrap_or(false) => Some((
                format!("file:{}", s(&item, "path")),
                reqwest::Method::GET,
                "/fs/file".to_string(),
                vec![("path", s(&item, "path"))],
                None,
                10,
            )),
            _ => None,
        };
        if let Some((key, method, path, query, body, max_age)) = detail
            && self.boards.stale(&key, Duration::from_secs(max_age))
        {
            self.boards.fetch_with(info.clone(), key, method, path, query, body);
        }
    }

    fn poll_inbox_alerts(&mut self, info: &crate::serve::sidecar::DaemonInfo) {
        if let Some(events) = self.boards.get("inbox").value.and_then(|v| v.get("events").cloned()) {
            let events = events.as_array().cloned().unwrap_or_default();
            let mine = self.current_session_id();
            let newest = events
                .iter()
                .filter_map(|e| Some((e.get("created_at")?.as_i64()?, e.get("event_id")?.as_u64()?)))
                .max();
            match self.panels.inbox_cursor {
                None => self.panels.inbox_cursor = Some(newest.unwrap_or((now_secs(), 0))),
                Some(seen) => {
                    let fresh: Vec<&Value> = events
                        .iter()
                        .filter(|e| {
                            let at = (
                                e.get("created_at").and_then(Value::as_i64).unwrap_or(0),
                                e.get("event_id").and_then(Value::as_u64).unwrap_or(0),
                            );
                            at > seen && s(e, "session") != mine
                        })
                        .collect();
                    if let Some(last) = fresh.last() {
                        let (label, _) = notification_label(last);
                        self.status = format!("{label}: {} — Ctrl-P inbox", notification_title(last));
                        if !(self.shell.tab == PaneTab::Inbox && self.shell.pane_visible(self.shell.width)) {
                            self.panels.inbox_unseen += fresh.len();
                        }
                    }
                    if let Some(newest) = newest.filter(|n| *n > seen) {
                        self.panels.inbox_cursor = Some(newest);
                    }
                }
            }
        }
        if self.shell.tab == PaneTab::Inbox && self.shell.pane_visible(self.shell.width) {
            self.panels.inbox_unseen = 0;
            return;
        }
        if self.panels.inbox_polled.is_some_and(|at| at.elapsed() < Duration::from_secs(20)) {
            return;
        }
        self.panels.inbox_polled = Some(Instant::now());
        self.boards.fetch_with(
            info.clone(),
            "inbox".into(),
            reqwest::Method::GET,
            "/notifications".into(),
            Self::inbox_query(),
            None,
        );
    }

    fn open_notification(&mut self, e: &Value) {
        let dest = e.get("destination").cloned().unwrap_or(Value::Null);
        match s(&dest, "type").as_str() {
            "task" => {
                let id = s(&dest, "id");
                self.shell.tab = PaneTab::Tasks;
                self.shell.pane_detail = None;
                self.shell.pane_scroll = 0;
                self.shell.pane_index = self
                    .boards
                    .get("tasks")
                    .value
                    .and_then(|v| v.as_array().and_then(|l| l.iter().position(|t| s(t, "id") == id)))
                    .unwrap_or(0);
            }
            "session" => {
                let id = s(&dest, "id");
                if id == crate::mission_control::SESSION_ID {
                    self.open_mission_control();
                    return;
                }
                if let Some(c) = self.list_conversations().into_iter().find(|c| c.id == id) {
                    self.shell.focus = Focus::Composer;
                    self.open_conversation(&c);
                } else {
                    self.status = "That session isn't in this folder's list.".into();
                }
            }
            _ => self.status = notification_title(e),
        }
    }

    pub(crate) fn handle_panel_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let tab = self.shell.tab;
        if !matches!(tab, PaneTab::Inbox | PaneTab::Procs | PaneTab::Git | PaneTab::Files) {
            return false;
        }
        if self.shell.pane_detail.is_some() {
            return false;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Right => {
                let Some(item) = self.panel_item(tab) else {
                    return true;
                };
                match tab {
                    PaneTab::Inbox => self.open_notification(&item),
                    PaneTab::Files if item.get("is_dir").and_then(Value::as_bool).unwrap_or(false) => {
                        self.panels.files_path = Some(PathBuf::from(s(&item, "path")));
                        self.shell.pane_index = 0;
                    }
                    _ => {
                        self.shell.pane_detail = Some(self.shell.pane_index);
                        self.shell.pane_scroll = 0;
                    }
                }
                true
            }
            KeyCode::Backspace if tab == PaneTab::Files => {
                let current = self.files_root();
                if let Some(parent) = current.parent() {
                    let name = current.file_name().map(|n| n.to_string_lossy().to_string());
                    self.panels.files_path = Some(parent.to_path_buf());
                    let key = format!("fs:{}", parent.display());
                    self.shell.pane_index = self
                        .boards
                        .get(&key)
                        .value
                        .and_then(|v| {
                            v.get("entries")?
                                .as_array()?
                                .iter()
                                .position(|e| Some(s(e, "name")) == name)
                        })
                        .unwrap_or(0);
                }
                true
            }
            KeyCode::Char('~') if tab == PaneTab::Files => {
                self.panels.files_path = None;
                self.shell.pane_index = 0;
                true
            }
            KeyCode::Char('x') if tab == PaneTab::Procs => {
                if let Some(p) = self.panel_item(tab)
                    .filter(|p| p.get("running").and_then(Value::as_bool).unwrap_or(false))
                {
                    let name = proc_name(&p);
                    self.board_confirm = Some(super::boards::Confirm {
                        prompt: format!("Stop {name}? y to confirm"),
                        action: super::boards::ConfirmAction::KillProcess(self.current_session_id(), s(&p, "id"), name),
                    });
                }
                true
            }
            _ => false,
        }
    }
}

fn proc_name(p: &Value) -> String {
    let label = s(p, "label");
    if label.is_empty() { s(p, "command") } else { label }
}

pub(crate) fn panel_hint(tab: PaneTab, detail: bool) -> Option<&'static str> {
    Some(match (tab, detail) {
        (PaneTab::Inbox, false) => "↑↓ move · Enter open · Tab next tab",
        (PaneTab::Procs, false) => "↑↓ move · Enter log · x stop · Tab next tab",
        (PaneTab::Git, false) => "↑↓ move · Enter diff · Tab next tab",
        (PaneTab::Files, false) => "↑↓ move · Enter open · ⌫ up · ~ workspace",
        _ => return None,
    })
}

pub(crate) fn panel_lines(app: &App, tab: PaneTab, width: usize) -> Vec<Line<'static>> {
    let focused = app.shell.focus == Focus::Pane;
    let visible = (app.shell.height as usize).saturating_sub(8);
    let list = app.panel_list(tab);
    let key = app.panel_key(tab).unwrap_or_default();
    let f = app.boards.get(&key);
    let sel = app.shell.pane_index;
    let mut lines = Vec::new();
    if let Some(i) = app.shell.pane_detail.filter(|i| *i < list.len()) {
        return detail_lines(app, tab, &list[i], width);
    }
    match tab {
        PaneTab::Inbox => {
            if let Some(st) = state(&f, "Nothing in the last day.", list.is_empty()) {
                return st;
            }
            let rows = list
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let (label, color) = notification_label(e);
                    let age = ago(e.get("created_at").and_then(Value::as_i64).unwrap_or(0));
                    super::chrome::Row {
                        selected: focused && i == sel,
                        dot: Some(color),
                        title: &notification_title(e),
                        emphasis: false,
                        meta: label,
                        right: Some((age, faint())),
                    }
                    .line(width)
                })
                .collect();
            lines.extend(windowed(rows, sel, visible));
        }
        PaneTab::Procs => {
            if let Some(st) = state(&f, "No background processes in this session.", list.is_empty()) {
                return st;
            }
            let rows = list
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let running = p.get("running").and_then(Value::as_bool).unwrap_or(false);
                    let status = if running {
                        "running".to_string()
                    } else {
                        let code = s(p, "status");
                        if code.is_empty() { "exited".into() } else { format!("exit {code}") }
                    };
                    let color = if running { warn() } else if s(p, "status") == "0" { success() } else { danger() };
                    super::chrome::Row {
                        selected: focused && i == sel,
                        dot: Some(color),
                        title: &proc_name(p),
                        emphasis: false,
                        meta: "",
                        right: Some((status, if running { warn() } else { faint() })),
                    }
                    .line(width)
                })
                .collect();
            lines.extend(windowed(rows, sel, visible));
        }
        PaneTab::Git => {
            let Some(v) = f.value.as_ref() else {
                return state(&f, "", false).unwrap_or_default();
            };
            if v.get("ok").and_then(Value::as_bool) == Some(false) {
                let error = s(v, "error");
                return vec![faint_line(if error.contains("not a git repository") {
                    "This workspace isn't a git repository."
                } else {
                    error.lines().next().unwrap_or("git status failed")
                })];
            }
            let mut head = vec![
                Span::styled("⎇ ", Style::default().fg(accent())),
                Span::styled(s(v, "branch"), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
            ];
            let ahead = v.get("ahead").and_then(Value::as_i64).unwrap_or(0);
            let behind = v.get("behind").and_then(Value::as_i64).unwrap_or(0);
            if ahead > 0 {
                head.push(Span::styled(format!("  ↑{ahead}"), Style::default().fg(success())));
            }
            if behind > 0 {
                head.push(Span::styled(format!("  ↓{behind}"), Style::default().fg(warn())));
            }
            lines.push(Line::from(head));
            lines.push(Line::from(""));
            if list.is_empty() {
                lines.push(faint_line("Working tree clean."));
                return lines;
            }
            let rows = list
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let x = s(e, "x");
                    let y = s(e, "y");
                    let code = if x == "?" { "?".to_string() } else if x.trim().is_empty() { y.clone() } else { x.clone() };
                    let staged = e.get("staged").and_then(Value::as_bool).unwrap_or(false);
                    let selected = focused && i == sel;
                    Line::from(vec![
                        Span::styled(if selected { "▍ " } else { "  " }, Style::default().fg(accent())),
                        Span::styled(format!("{} ", if code == "?" { "U" } else { code.as_str() }), Style::default().fg(git_color(&code)).add_modifier(Modifier::BOLD)),
                        Span::styled(pad(&s(e, "path"), width.saturating_sub(12)), Style::default().fg(if selected { text() } else { soft() })),
                        Span::styled(if staged { " staged " } else { "        " }, Style::default().fg(faint())),
                    ])
                    .style(selected_bg(selected))
                })
                .collect();
            lines.extend(windowed(rows, sel, visible.saturating_sub(2)));
        }
        PaneTab::Files => {
            let root = app.files_root();
            let shown = match std::env::var("HOME").ok().and_then(|h| root.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
                Some(rel) => format!("~/{}", rel.display()),
                None => root.display().to_string(),
            };
            lines.push(Line::from(Span::styled(
                pad(&shown, width),
                Style::default().fg(soft()).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            if let Some(st) = state(&f, "Empty folder.", list.is_empty()) {
                lines.extend(st);
                return lines;
            }
            let rows = list
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let dir = e.get("is_dir").and_then(Value::as_bool).unwrap_or(false);
                    let name = if dir { format!("{}/", s(e, "name")) } else { s(e, "name") };
                    super::chrome::Row {
                        selected: focused && i == sel,
                        dot: None,
                        title: &name,
                        emphasis: dir,
                        meta: "",
                        right: None,
                    }
                    .line(width)
                })
                .collect();
            lines.extend(windowed(rows, sel, visible.saturating_sub(2)));
        }
        _ => {}
    }
    lines.extend(super::boards::footer_lines(app, width));
    lines
}

fn detail_lines(app: &App, tab: PaneTab, item: &Value, width: usize) -> Vec<Line<'static>> {
    let session = app.current_session_id();
    match tab {
        PaneTab::Procs => {
            let mut lines = vec![back_line(&proc_name(item), &format!("pid {}", s(item, "pid"))), Line::from("")];
            let f = app.boards.get(&format!("proclog:{session}:{}", s(item, "id")));
            match f.value.as_ref() {
                None => lines.extend(state(&f, "", false).unwrap_or_default()),
                Some(v) => {
                    let log = s(v, "log");
                    if log.is_empty() {
                        lines.push(faint_line("No output yet."));
                    }
                    for l in log.lines() {
                        lines.push(Line::from(Span::styled(pad(l, width), Style::default().fg(soft()))));
                    }
                }
            }
            lines
        }
        PaneTab::Git => {
            let untracked = item.get("untracked").and_then(Value::as_bool).unwrap_or(false);
            let staged = !untracked && !item.get("unstaged").and_then(Value::as_bool).unwrap_or(false);
            let meta = if untracked { "new file" } else if staged { "staged" } else { "unstaged" };
            let mut lines = vec![back_line(&s(item, "path"), meta), Line::from("")];
            let f = app.boards.get(&format!("diff:{session}:{}:{staged}", s(item, "path")));
            match f.value.as_ref() {
                None => lines.extend(state(&f, "", false).unwrap_or_default()),
                Some(v) => {
                    let patch = s(v, "patch");
                    if patch.is_empty() {
                        lines.push(faint_line("No textual changes."));
                    }
                    for l in patch.lines().skip_while(|l| !l.starts_with("@@")) {
                        let color = if l.starts_with('+') {
                            success()
                        } else if l.starts_with('-') {
                            danger()
                        } else if l.starts_with("@@") {
                            accent()
                        } else {
                            soft()
                        };
                        lines.push(Line::from(Span::styled(pad(l, width), Style::default().fg(color))));
                    }
                }
            }
            lines
        }
        PaneTab::Files => {
            let mut lines = vec![back_line(&s(item, "name"), ""), Line::from("")];
            let f = app.boards.get(&format!("file:{}", s(item, "path")));
            match f.value.as_ref() {
                None => lines.extend(state(&f, "", false).unwrap_or_default()),
                Some(v) if v.get("binary").and_then(Value::as_bool).unwrap_or(false) => {
                    lines.push(faint_line("Binary file — not shown."));
                }
                Some(v) => {
                    let content = s(v, "content");
                    let total = content.lines().count().max(1);
                    let gutter = total.to_string().len();
                    for (n, l) in content.lines().enumerate() {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{:>gutter$} ", n + 1), Style::default().fg(faint())),
                            Span::styled(pad(&l.replace('\t', "    "), width.saturating_sub(gutter + 1)), Style::default().fg(soft())),
                        ]));
                    }
                    if v.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
                        lines.push(faint_line("… truncated"));
                    }
                }
            }
            lines
        }
        _ => Vec::new(),
    }
}

pub(crate) fn tab_badge(app: &App, tab: PaneTab) -> Option<(String, ratatui::style::Color)> {
    match tab {
        PaneTab::Inbox if app.panels.inbox_unseen > 0 => Some((format!(" {}", app.panels.inbox_unseen), warn())),
        PaneTab::Git => {
            let n = app.panel_len(PaneTab::Git);
            (n > 0).then(|| (format!(" {n}"), faint()))
        }
        PaneTab::Procs => {
            let n = app
                .panel_list(PaneTab::Procs)
                .iter()
                .filter(|p| p.get("running").and_then(Value::as_bool).unwrap_or(false))
                .count();
            (n > 0).then(|| (format!(" {n}"), success()))
        }
        _ => None,
    }
}
