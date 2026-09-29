use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Clear, Paragraph};

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Focus {
    #[default]
    Composer,
    Sidebar,
    Pane,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PaneTab {
    #[default]
    Tools,
    Plan,
    Lanes,
    Checkpoints,
}

impl PaneTab {
    pub(crate) const ALL: [PaneTab; 4] = [PaneTab::Tools, PaneTab::Plan, PaneTab::Lanes, PaneTab::Checkpoints];

    fn label(self) -> &'static str {
        match self {
            PaneTab::Tools => "Tools",
            PaneTab::Plan => "Plan",
            PaneTab::Lanes => "Lanes",
            PaneTab::Checkpoints => "Checkpoints",
        }
    }
}

#[derive(Default)]
pub(crate) struct ShellState {
    pub(crate) sidebar: Option<bool>,
    pub(crate) pane: Option<bool>,
    pub(crate) focus: Focus,
    pub(crate) sidebar_index: usize,
    pub(crate) tab: PaneTab,
    pub(crate) pane_index: usize,
    pub(crate) pane_detail: Option<usize>,
    pub(crate) pane_scroll: u16,
    pub(crate) palette: Option<Palette>,
    pub(crate) width: u16,
}

#[derive(Default)]
pub(crate) struct Palette {
    pub(crate) query: String,
    pub(crate) index: usize,
}

const SIDEBAR_W: u16 = 34;
const SIDEBAR_MIN_WINDOW: u16 = 110;
const PANE_MIN_WINDOW: u16 = 150;

impl ShellState {
    pub(crate) fn sidebar_visible(&self, width: u16) -> bool {
        width >= 80 && self.sidebar.unwrap_or(width >= SIDEBAR_MIN_WINDOW)
    }

    pub(crate) fn pane_visible(&self, width: u16) -> bool {
        width >= 100 && self.pane.unwrap_or(width >= PANE_MIN_WINDOW)
    }
}

pub(crate) fn shell_areas(app: &App, area: Rect) -> (Option<Rect>, Rect, Option<Rect>) {
    let side = app.shell.sidebar_visible(area.width);
    let pane = app.shell.pane_visible(area.width);
    let pane_w = (area.width as f32 * 0.34).clamp(40.0, 64.0) as u16;
    let mut constraints = Vec::new();
    if side {
        constraints.push(Constraint::Length(SIDEBAR_W));
    }
    constraints.push(Constraint::Min(40));
    if pane {
        constraints.push(Constraint::Length(pane_w));
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    let mut i = 0;
    let sidebar = side.then(|| {
        i += 1;
        cols[0]
    });
    let centre = cols[i];
    let pane = pane.then(|| cols[i + 1]);
    (sidebar, centre, pane)
}

fn fill(frame: &mut ratatui::Frame<'_>, area: Rect, color: Color) {
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(color)),
        area,
    );
}

fn pad(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n >= width {
        let mut t: String = text.chars().take(width.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        format!("{text}{}", " ".repeat(width - n))
    }
}

fn compact_age(ts: i64) -> String {
    let secs = (chrono::Utc::now().timestamp() - ts).max(0);
    match secs {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..2_419_200 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

fn status_dot(status: &str) -> Span<'static> {
    let (glyph, color) = match status {
        "running" => ("●", accent()),
        "waiting_for_input" => ("●", warn()),
        "failed" => ("●", danger()),
        _ => ("○", faint()),
    };
    Span::styled(glyph, Style::default().fg(color))
}

pub(crate) fn sidebar_rows(app: &App) -> Vec<(String, String, String, String)> {
    let Some(rows) = app.daemon_sessions.as_ref() else {
        return app
            .list_conversations()
            .into_iter()
            .map(|(name, desc)| (name, desc, String::new(), String::new()))
            .collect();
    };
    let mut rows: Vec<_> = rows
        .iter()
        .filter(|s| !s.conversation.is_empty())
        .filter(|s| s.conversation != "default" || !s.title.trim().is_empty())
        .collect();
    rows.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    rows.into_iter()
        .map(|s| {
            let title = if s.title.trim().is_empty() {
                "Untitled".to_string()
            } else {
                s.title.trim().to_string()
            };
            (s.conversation.clone(), title, s.status.clone(), compact_age(s.last_active))
        })
        .collect()
}

pub(crate) fn render_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    fill(frame, area, surface1());
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    let focused = app.shell.focus == Focus::Sidebar;
    let folder = app
        .options
        .config
        .workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Sessions", Style::default().fg(text()).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {folder}"), Style::default().fg(faint())),
        ]),
        Line::from(""),
    ];
    let rows = sidebar_rows(app);
    if rows.is_empty() {
        lines.push(Line::from(Span::styled("No sessions yet", Style::default().fg(faint()))));
    }
    let w = inner.width as usize;
    let visible = inner.height.saturating_sub(4) as usize;
    let start = app.shell.sidebar_index.saturating_sub(visible.saturating_sub(1));
    for (i, (name, title, status, age)) in rows.iter().enumerate().skip(start).take(visible) {
        let active = *name == app.active_conversation;
        let selected = focused && i == app.shell.sidebar_index;
        let bg = if selected { surface3() } else if active { surface2() } else { surface1() };
        let title_w = w.saturating_sub(4 + age.chars().count() + 1);
        let title_style = if active {
            Style::default().fg(text()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(soft())
        };
        lines.push(
            Line::from(vec![
                Span::styled(if active { "▍" } else { " " }, Style::default().fg(accent())),
                status_dot(status),
                Span::raw(" "),
                Span::styled(pad(title, title_w), title_style),
                Span::raw(" "),
                Span::styled(age.clone(), Style::default().fg(faint())),
            ])
            .style(Style::default().bg(bg)),
        );
    }
    frame.render_widget(Paragraph::new(lines), Rect { height: area.height.saturating_sub(2), y: area.y + 1, ..inner });
    let hint = if focused { "↑↓ move · Enter open · n new · Esc back" } else { "Ctrl-B sessions" };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(faint())))),
        Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..inner },
    );
}

pub(crate) fn turn_tools(app: &App) -> Vec<(String, Value, Option<Value>)> {
    let Some(state) = app.state.as_ref() else {
        return Vec::new();
    };
    let start = state
        .events
        .iter()
        .rposition(|e| matches!(e, HarnessEvent::UserInput { .. }))
        .unwrap_or(0);
    let mut out: Vec<(String, Value, Option<Value>)> = Vec::new();
    for event in &state.events[start..] {
        match event {
            HarnessEvent::ToolCall { tool_name, arguments } if !HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) => {
                out.push((tool_name.clone(), arguments.clone(), None));
            }
            HarnessEvent::ToolResult { tool_name, result } => {
                if let Some(slot) = out
                    .iter_mut()
                    .rev()
                    .find(|(name, _, r)| name == tool_name && r.is_none())
                {
                    slot.2 = Some(result.clone());
                }
            }
            _ => {}
        }
    }
    out
}

fn tool_row(tool: &str, args: &Value, result: Option<&Value>, width: usize, selected: bool) -> Line<'static> {
    let (verb, arg) = tool_render::tool_call_parts(tool, args);
    let failed = result.is_some_and(|r| r.get("status").and_then(Value::as_str) == Some("error"));
    let (glyph, color) = match (result, failed) {
        (None, _) => ("◐", accent()),
        (Some(_), true) => ("✕", danger()),
        (Some(_), false) => ("✓", success()),
    };
    let text_w = width.saturating_sub(4);
    let label = if arg.is_empty() { verb } else { format!("{verb} {arg}") };
    Line::from(vec![
        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
        Span::styled(pad(&label, text_w), Style::default().fg(if selected { text() } else { soft() })),
    ])
    .style(Style::default().bg(if selected { surface3() } else { surface1() }))
}

pub(crate) fn render_pane(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    fill(frame, area, surface1());
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), y: area.y + 1, height: area.height.saturating_sub(1) };
    let focused = app.shell.focus == Focus::Pane;
    let mut tabs = Vec::new();
    for (i, tab) in PaneTab::ALL.iter().enumerate() {
        if i > 0 {
            tabs.push(Span::raw("  "));
        }
        let active = *tab == app.shell.tab;
        tabs.push(Span::styled(
            tab.label(),
            if active {
                Style::default().fg(text()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(faint())
            },
        ));
    }
    let w = inner.width as usize;
    let mut lines = vec![Line::from(tabs), Line::from("")];
    match app.shell.tab {
        PaneTab::Tools => {
            let tools = turn_tools(app);
            if let Some(i) = app.shell.pane_detail.filter(|i| *i < tools.len()) {
                let (tool, args, result) = &tools[i];
                lines.push(Line::from(vec![
                    Span::styled("‹ ", Style::default().fg(accent())),
                    Span::styled(format!("step {} of {}", i + 1, tools.len()), Style::default().fg(faint())),
                ]));
                lines.push(Line::from(""));
                lines.extend(transcript::tool_call_head_lines_status(tool, args, w, match result {
                    None => transcript::ToolRowStatus::Running,
                    Some(r) if r.get("status").and_then(Value::as_str) == Some("error") => transcript::ToolRowStatus::Failed,
                    Some(_) => transcript::ToolRowStatus::Done,
                }));
                lines.extend(transcript::tool_call_preview(tool, args, w));
                if let Some(r) = result {
                    lines.extend(tool_render::tool_result_lines_expanded(tool, r, w));
                }
                let scroll = app.shell.pane_scroll as usize;
                let body: Vec<Line<'static>> = lines.drain(2..).skip(scroll).collect();
                lines.extend(body);
            } else if tools.is_empty() {
                lines.push(Line::from(Span::styled("No tool calls in this turn yet.", Style::default().fg(faint()))));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("{} step{} this turn", tools.len(), if tools.len() == 1 { "" } else { "s" }),
                    Style::default().fg(faint()),
                )));
                lines.push(Line::from(""));
                for (i, (tool, args, result)) in tools.iter().enumerate() {
                    lines.push(tool_row(tool, args, result.as_ref(), w, focused && i == app.shell.pane_index));
                }
            }
        }
        PaneTab::Plan => {
            let plan = app.state.as_ref().map(|s| s.plan.clone()).unwrap_or_default();
            if plan.is_empty() {
                lines.push(Line::from(Span::styled("No plan for this session.", Style::default().fg(faint()))));
            }
            for step in plan {
                let (glyph, color, style) = match step.status {
                    PlanStatus::Done => ("✓", success(), Style::default().fg(faint())),
                    PlanStatus::InProgress => ("▸", accent(), Style::default().fg(text()).add_modifier(Modifier::BOLD)),
                    PlanStatus::Pending => ("○", faint(), Style::default().fg(soft())),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                    Span::styled(pad(&step.step, w.saturating_sub(4)), style),
                ]));
            }
        }
        PaneTab::Lanes => {
            let lanes = app.state.as_ref().map(|s| s.lanes.clone()).unwrap_or_default();
            if lanes.is_empty() {
                lines.push(Line::from(Span::styled("No delegated work in this session.", Style::default().fg(faint()))));
            }
            for (i, lane) in lanes.iter().enumerate() {
                let (glyph, color, label) = match lane.status {
                    LaneStatus::Running => ("◐", accent(), "running"),
                    LaneStatus::Completed => ("✓", success(), "done"),
                    LaneStatus::Failed => ("✕", danger(), "failed"),
                    LaneStatus::Cancelled => ("○", faint(), "cancelled"),
                };
                let selected = focused && i == app.shell.pane_index;
                lines.push(
                    Line::from(vec![
                        Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                        Span::styled(pad(&lane.title, w.saturating_sub(5 + label.len())), Style::default().fg(text())),
                        Span::styled(format!(" {label}"), Style::default().fg(faint())),
                    ])
                    .style(Style::default().bg(if selected { surface3() } else { surface1() })),
                );
            }
            if !lanes.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Enter opens the full lanes view", Style::default().fg(faint()))));
            }
        }
        PaneTab::Checkpoints => {
            let cps = app.state.as_ref().map(|s| s.checkpoints.clone()).unwrap_or_default();
            if cps.is_empty() {
                lines.push(Line::from(Span::styled("No checkpoints yet.", Style::default().fg(faint()))));
            }
            for (i, cp) in cps.iter().rev().enumerate() {
                let selected = focused && i == app.shell.pane_index;
                lines.push(
                    Line::from(vec![
                        Span::styled(" ↺ ", Style::default().fg(faint())),
                        Span::styled(pad(&cp.label, w.saturating_sub(4)), Style::default().fg(soft())),
                    ])
                    .style(Style::default().bg(if selected { surface3() } else { surface1() })),
                );
            }
            if !cps.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Enter rewinds to the selected checkpoint", Style::default().fg(faint()))));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), Rect { height: inner.height.saturating_sub(1), ..inner });
    let hint = if focused {
        match (app.shell.tab, app.shell.pane_detail) {
            (PaneTab::Tools, Some(_)) => "↑↓ scroll · ← back · Tab next tab · Esc",
            _ => "↑↓ move · Enter open · Tab next tab · Esc",
        }
    } else {
        "Ctrl-L panel"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(faint())))),
        Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..inner },
    );
}

pub(crate) const PALETTE_ACTIONS: [(&str, &str); 14] = [
    ("new", "New session"),
    ("sessions", "Switch session…"),
    ("sidebar", "Toggle sessions sidebar"),
    ("pane", "Toggle side panel"),
    ("tools", "Show this turn's tools"),
    ("plan", "Show the plan"),
    ("lanes", "Show delegated work"),
    ("checkpoints", "Show checkpoints"),
    ("model", "Switch model…"),
    ("mode", "Toggle manual approval"),
    ("compact", "Compact history now"),
    ("term", "Open a terminal"),
    ("profiles", "Model profiles and settings"),
    ("quit", "Quit"),
];

pub(crate) fn palette_matches(query: &str) -> Vec<(&'static str, &'static str)> {
    let q = query.to_lowercase();
    PALETTE_ACTIONS
        .iter()
        .copied()
        .filter(|(id, label)| q.is_empty() || label.to_lowercase().contains(&q) || id.contains(&q))
        .collect()
}

pub(crate) fn render_palette(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(palette) = app.shell.palette.as_ref() else {
        return;
    };
    let w = 60.min(area.width.saturating_sub(4));
    let matches = palette_matches(&palette.query);
    let h = (matches.len() as u16 + 4).min(area.height.saturating_sub(4)).max(5);
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height / 6, width: w, height: h };
    frame.render_widget(Clear, rect);
    fill(frame, rect, surface2());
    let inner = Rect { x: rect.x + 2, width: rect.width.saturating_sub(4), y: rect.y + 1, height: rect.height.saturating_sub(2) };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("› ", Style::default().fg(accent())),
            Span::styled(palette.query.clone(), Style::default().fg(text())),
            Span::styled("▏", Style::default().fg(accent())),
        ]),
        Line::from(Span::styled("─".repeat(inner.width as usize), Style::default().fg(border2()))),
    ];
    for (i, (_, label)) in matches.iter().enumerate() {
        let selected = i == palette.index;
        lines.push(
            Line::from(Span::styled(
                pad(&format!(" {label}"), inner.width as usize),
                Style::default().fg(if selected { text() } else { soft() }),
            ))
            .style(Style::default().bg(if selected { surface3() } else { surface2() })),
        );
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(" No matching command", Style::default().fg(faint()))));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn render_key_hints(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let hints: &[(&str, &str)] = match app.shell.focus {
        Focus::Composer => &[("Enter", "send"), ("Ctrl-P", "commands"), ("Ctrl-B", "sessions"), ("Ctrl-L", "panel"), ("Esc", "stop")],
        Focus::Sidebar => &[("↑↓", "move"), ("Enter", "open"), ("n", "new"), ("Esc", "back")],
        Focus::Pane => &[("↑↓", "move"), ("Enter", "open"), ("Tab", "tab"), ("Esc", "back")],
    };
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(soft()).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(faint())));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

impl App {
    pub(crate) fn resume_conversation(&mut self, name: &str) {
        self.conv_cache = None;
        self.switch_conversation(name);
        self.screen = Screen::Main;
        if self.session_known() {
            self.spawn_loop(None, true);
        }
    }

    fn open_pane(&mut self, tab: PaneTab) {
        self.shell.pane = Some(true);
        self.shell.tab = tab;
        self.shell.pane_index = 0;
        self.shell.pane_detail = None;
        self.shell.pane_scroll = 0;
        self.shell.focus = Focus::Pane;
    }

    fn run_palette_action(&mut self, id: &str) {
        match id {
            "new" => self.handle_slash_command("/new"),
            "sessions" => {
                self.shell.sidebar = Some(true);
                self.shell.focus = Focus::Sidebar;
            }
            "sidebar" => self.shell.sidebar = Some(!self.shell.sidebar.unwrap_or(true)),
            "pane" => self.shell.pane = Some(!self.shell.pane.unwrap_or(true)),
            "tools" => self.open_pane(PaneTab::Tools),
            "plan" => self.open_pane(PaneTab::Plan),
            "lanes" => self.open_pane(PaneTab::Lanes),
            "checkpoints" => self.open_pane(PaneTab::Checkpoints),
            "model" => self.handle_slash_command("/model"),
            "mode" => self.handle_slash_command("/mode"),
            "compact" => self.handle_slash_command("/compact"),
            "term" => self.open_term(),
            "profiles" => self.open_profiles(),
            "quit" => self.quit = true,
            _ => {}
        }
    }

    fn pane_len(&self) -> usize {
        match self.shell.tab {
            PaneTab::Tools => turn_tools(self).len(),
            PaneTab::Plan => self.state.as_ref().map_or(0, |s| s.plan.len()),
            PaneTab::Lanes => self.state.as_ref().map_or(0, |s| s.lanes.len()),
            PaneTab::Checkpoints => self.state.as_ref().map_or(0, |s| s.checkpoints.len()),
        }
    }
}

pub(crate) fn handle_shell_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let width = app.shell.width;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if let Some(palette) = app.shell.palette.as_mut() {
        let matches = palette_matches(&palette.query);
        match key.code {
            KeyCode::Esc => app.shell.palette = None,
            KeyCode::Up => palette.index = palette.index.saturating_sub(1),
            KeyCode::Down => palette.index = (palette.index + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Backspace => {
                palette.query.pop();
                palette.index = 0;
            }
            KeyCode::Enter => {
                let id = matches.get(palette.index).map(|(id, _)| *id);
                app.shell.palette = None;
                if let Some(id) = id {
                    app.run_palette_action(id);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                palette.query.push(c);
                palette.index = 0;
            }
            _ => {}
        }
        return true;
    }

    if ctrl {
        match key.code {
            KeyCode::Char('p') => {
                app.shell.palette = Some(Palette::default());
                return true;
            }
            KeyCode::Char('b') => {
                if app.shell.sidebar_visible(width) && app.shell.focus == Focus::Sidebar {
                    app.shell.sidebar = Some(false);
                    app.shell.focus = Focus::Composer;
                } else {
                    app.shell.sidebar = Some(true);
                    app.shell.focus = Focus::Sidebar;
                    let rows = sidebar_rows(app);
                    app.shell.sidebar_index = rows
                        .iter()
                        .position(|(name, ..)| *name == app.active_conversation)
                        .unwrap_or(0);
                }
                return true;
            }
            KeyCode::Char('l') => {
                if app.shell.pane_visible(width) && app.shell.focus == Focus::Pane {
                    app.shell.pane = Some(false);
                    app.shell.focus = Focus::Composer;
                } else {
                    let tab = app.shell.tab;
                    app.open_pane(tab);
                }
                return true;
            }
            _ => return false,
        }
    }

    match app.shell.focus {
        Focus::Composer => false,
        Focus::Sidebar => {
            let rows = sidebar_rows(app);
            match key.code {
                KeyCode::Esc | KeyCode::Right => app.shell.focus = Focus::Composer,
                KeyCode::Up | KeyCode::Char('k') => {
                    app.shell.sidebar_index = app.shell.sidebar_index.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.shell.sidebar_index = (app.shell.sidebar_index + 1).min(rows.len().saturating_sub(1))
                }
                KeyCode::Enter => {
                    if let Some((name, ..)) = rows.get(app.shell.sidebar_index).cloned() {
                        app.shell.focus = Focus::Composer;
                        if name != app.active_conversation {
                            app.resume_conversation(&name);
                        }
                    }
                }
                KeyCode::Char('n') => {
                    app.shell.focus = Focus::Composer;
                    app.handle_slash_command("/new");
                }
                _ => {}
            }
            true
        }
        Focus::Pane => {
            if app.shell.tab == PaneTab::Tools && app.shell.pane_detail.is_some() {
                match key.code {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                        app.shell.pane_detail = None;
                        app.shell.pane_scroll = 0;
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.shell.pane_scroll = app.shell.pane_scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => app.shell.pane_scroll = app.shell.pane_scroll.saturating_add(1),
                    KeyCode::PageUp => app.shell.pane_scroll = app.shell.pane_scroll.saturating_sub(10),
                    KeyCode::PageDown => app.shell.pane_scroll = app.shell.pane_scroll.saturating_add(10),
                    _ => {}
                }
                return true;
            }
            let len = app.pane_len();
            match key.code {
                KeyCode::Esc | KeyCode::Left => app.shell.focus = Focus::Composer,
                KeyCode::Tab | KeyCode::BackTab => {
                    let i = PaneTab::ALL.iter().position(|t| *t == app.shell.tab).unwrap_or(0);
                    let n = PaneTab::ALL.len();
                    let next = if key.code == KeyCode::BackTab { (i + n - 1) % n } else { (i + 1) % n };
                    app.open_pane(PaneTab::ALL[next]);
                }
                KeyCode::Up | KeyCode::Char('k') => app.shell.pane_index = app.shell.pane_index.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    app.shell.pane_index = (app.shell.pane_index + 1).min(len.saturating_sub(1))
                }
                KeyCode::Enter | KeyCode::Right if len > 0 => match app.shell.tab {
                    PaneTab::Tools => {
                        app.shell.pane_detail = Some(app.shell.pane_index.min(len - 1));
                        app.shell.pane_scroll = 0;
                    }
                    PaneTab::Lanes => {
                        app.lanes_selected_index = app.shell.pane_index.min(len - 1);
                        app.screen = Screen::Lanes;
                    }
                    PaneTab::Checkpoints => {
                        let checkpoint = app
                            .state
                            .as_ref()
                            .and_then(|s| s.checkpoints.iter().rev().nth(app.shell.pane_index))
                            .map(|c| c.id.clone());
                        if let Some(id) = checkpoint {
                            app.open_checkpoint_picker(CheckpointAction::Rewind);
                            if let Some(pos) = app
                                .state
                                .as_ref()
                                .map(|s| s.checkpoints.iter().rev().position(|c| c.id == id).unwrap_or(0))
                            {
                                app.checkpoint_selected_index = pos;
                            }
                        }
                    }
                    PaneTab::Plan => {}
                },
                _ => {}
            }
            true
        }
    }
}
