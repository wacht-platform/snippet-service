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
    Agents,
    Tasks,
    Jobs,
    Usage,
    Vault,
    Inbox,
    Procs,
    Git,
    Files,
}

impl PaneTab {
    pub(crate) const ALL: [PaneTab; 13] = [
        PaneTab::Tools,
        PaneTab::Plan,
        PaneTab::Lanes,
        PaneTab::Checkpoints,
        PaneTab::Agents,
        PaneTab::Tasks,
        PaneTab::Jobs,
        PaneTab::Usage,
        PaneTab::Vault,
        PaneTab::Inbox,
        PaneTab::Procs,
        PaneTab::Git,
        PaneTab::Files,
    ];

    fn label(self) -> &'static str {
        match self {
            PaneTab::Tools => "Tools",
            PaneTab::Plan => "Plan",
            PaneTab::Lanes => "Lanes",
            PaneTab::Checkpoints => "Checkpoints",
            PaneTab::Agents => "Agents",
            PaneTab::Tasks => "Tasks",
            PaneTab::Jobs => "Jobs",
            PaneTab::Usage => "Usage",
            PaneTab::Vault => "Vault",
            PaneTab::Inbox => "Inbox",
            PaneTab::Procs => "Procs",
            PaneTab::Git => "Git",
            PaneTab::Files => "Files",
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
    pub(crate) height: u16,
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

pub(crate) fn sidebar_rows(app: &App) -> Vec<crate::tui::commands::Conversation> {
    app.list_conversations()
}

pub(crate) fn render_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    fill(frame, area, surface1());
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    let focused = app.shell.focus == Focus::Sidebar;
    // Named by the home folder, so a worktree session reads as part of the
    // project it was made from.
    let folder = app
        .home_folder()
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
    for (i, row) in rows.iter().enumerate().skip(start).take(visible) {
        let active = app.is_active(row);
        let selected = focused && i == app.shell.sidebar_index;
        let bg = if selected { surface3() } else if active { surface2() } else { surface1() };
        let age = compact_age(row.last_active);
        let title = match &row.branch {
            Some(branch) => format!("{} ⎇ {branch}", row.title),
            None => row.title.clone(),
        };
        let status = &row.status;
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
                Span::styled(pad(&title, title_w), title_style),
                Span::raw(" "),
                Span::styled(age.clone(), Style::default().fg(faint())),
            ])
            .style(Style::default().bg(bg)),
        );
    }
    frame.render_widget(Paragraph::new(lines), Rect { height: area.height.saturating_sub(2), y: area.y + 1, ..inner });
    let hint = if focused { "↑↓ move · Enter open · n new · Esc back" } else { "Ctrl-F sessions" };
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

fn tab_strip(app: &App, width: usize) -> Line<'static> {
    let labels: Vec<String> = PaneTab::ALL
        .iter()
        .map(|t| {
            let badge = super::panels::tab_badge(app, *t).map(|(b, _)| b).unwrap_or_default();
            format!("{}{badge}", t.label())
        })
        .collect();
    let active = PaneTab::ALL.iter().position(|t| *t == app.shell.tab).unwrap_or(0);
    let cost = |lo: usize, hi: usize| {
        let body: usize = labels[lo..=hi].iter().map(|l| l.chars().count()).sum::<usize>() + 2 * (hi - lo);
        body + if lo > 0 { 2 } else { 0 } + if hi + 1 < labels.len() { 2 } else { 0 }
    };
    let (mut lo, mut hi) = (active, active);
    loop {
        let grew_right = hi + 1 < labels.len() && cost(lo, hi + 1) <= width;
        if grew_right {
            hi += 1;
        }
        let grew_left = lo > 0 && cost(lo - 1, hi) <= width;
        if grew_left {
            lo -= 1;
        }
        if !grew_right && !grew_left {
            break;
        }
    }
    let mut spans = Vec::new();
    if lo > 0 {
        spans.push(Span::styled("‹ ", Style::default().fg(faint())));
    }
    for (i, label) in labels.iter().enumerate().take(hi + 1).skip(lo) {
        if i > lo {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            label.clone(),
            if i == active {
                Style::default().fg(text()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(faint())
            },
        ));
    }
    if hi + 1 < labels.len() {
        spans.push(Span::styled(" ›", Style::default().fg(faint())));
    }
    Line::from(spans)
}

pub(crate) fn render_pane(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    fill(frame, area, surface1());
    let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), y: area.y + 1, height: area.height.saturating_sub(1) };
    let focused = app.shell.focus == Focus::Pane;
    let w = inner.width as usize;
    let mut lines = vec![tab_strip(app, w), Line::from("")];
    match app.shell.tab {
        PaneTab::Tools => {
            let tools = turn_tools(app);
            if let Some(i) = app.shell.pane_detail.filter(|i| *i < tools.len()) {
                let (tool, args, result) = &tools[i];
                lines.push(Line::from(vec![
                    Span::styled("‹ ", Style::default().fg(accent())),
                    Span::styled("back", Style::default().fg(faint())),
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
        PaneTab::Inbox | PaneTab::Procs | PaneTab::Git | PaneTab::Files => {
            let body = super::panels::panel_lines(app, app.shell.tab, w);
            let scroll = if app.shell.pane_detail.is_some() { app.shell.pane_scroll as usize } else { 0 };
            lines.extend(body.into_iter().skip(scroll));
        }
        PaneTab::Agents | PaneTab::Tasks | PaneTab::Jobs | PaneTab::Usage | PaneTab::Vault => {
            let body = super::boards::board_lines(app, app.shell.tab, w);
            let scroll = if app.shell.pane_detail.is_some() || app.shell.tab == PaneTab::Usage {
                app.shell.pane_scroll as usize
            } else {
                0
            };
            lines.extend(body.into_iter().skip(scroll));
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
            (_, Some(_)) => "↑↓ scroll · ← back · Tab next tab · Esc",
            (PaneTab::Jobs, None) => "↑↓ move · p pause/resume · d delete · Tab next tab",
            (PaneTab::Usage, None) => "↑↓ scroll · r range · Tab next tab · Esc",
            (PaneTab::Vault, None) => "↑↓ move · a add · d delete · Tab next tab",
            (tab, None) if super::panels::panel_hint(tab, false).is_some() => {
                super::panels::panel_hint(tab, false).unwrap_or_default()
            }
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

pub(crate) const PALETTE_ACTIONS: [(&str, &str); 25] = [
    ("new", "New session"),
    ("sessions", "Switch session…"),
    ("model", "Switch model…"),
    ("mode", "Toggle manual approval"),
    ("compact", "Compact history now"),
    ("steps", "Expand or fold tool steps"),
    ("term", "Open a terminal"),
    ("tools", "This turn's tools"),
    ("plan", "Plan"),
    ("lanes", "Delegated work"),
    ("checkpoints", "Checkpoints"),
    ("procs", "Background processes"),
    ("git", "Git changes"),
    ("files", "Browse files"),
    ("mission", "Open Mission Control"),
    ("agents", "Agents"),
    ("tasks", "Task board"),
    ("jobs", "Scheduled jobs"),
    ("inbox", "Notifications"),
    ("usage", "Usage and rate limits"),
    ("vault", "Vault secrets"),
    ("sidebar", "Toggle sessions sidebar"),
    ("pane", "Toggle side panel"),
    ("profiles", "Model profiles and settings"),
    ("quit", "Quit"),
];

fn palette_meta(id: &str) -> (&'static str, &'static str) {
    let section = match id {
        "new" | "sessions" | "model" | "mode" | "compact" | "steps" | "term" => "Session",
        "tools" | "plan" | "lanes" | "checkpoints" | "procs" | "git" | "files" => "This session",
        "mission" | "agents" | "tasks" | "jobs" | "inbox" | "usage" | "vault" => "Workspace",
        _ => "App",
    };
    let shortcut = match id {
        "sidebar" => "Ctrl-F",
        "pane" => "Ctrl-L",
        "steps" => "Ctrl-O",
        "term" => "Ctrl-T",
        "quit" => "Ctrl-C",
        "sessions" => "/resume",
        "model" => "/model",
        "new" => "/new",
        "compact" => "/compact",
        "mission" => "/mission",
        _ => "",
    };
    (section, shortcut)
}

pub(crate) fn palette_matches(query: &str) -> Vec<(&'static str, &'static str)> {
    let q = query.to_lowercase();
    PALETTE_ACTIONS
        .iter()
        .copied()
        .filter(|(id, label)| q.is_empty() || label.to_lowercase().contains(&q) || id.contains(&q))
        .collect()
}

pub(crate) fn render_palette(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, BorderType, Borders, Padding};
    let Some(palette) = app.shell.palette.as_ref() else {
        return;
    };
    let matches = palette_matches(&palette.query);
    let grouped = palette.query.is_empty();
    let mut rows: Vec<(Option<&str>, usize)> = Vec::new();
    let mut last = "";
    for (i, (id, _)) in matches.iter().enumerate() {
        let (section, _) = palette_meta(id);
        if grouped && section != last {
            if !rows.is_empty() {
                rows.push((Some(""), 0));
            }
            rows.push((Some(section), 0));
            last = section;
        }
        rows.push((None, i));
    }
    let w = 64.min(area.width.saturating_sub(4));
    let h = (rows.len() as u16 + 5).min(area.height.saturating_sub(4)).max(7);
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height / 8, width: w, height: h };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border2()))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(surface2()))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("› ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled("Commands ", Style::default().fg(soft())),
        ]));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let width = inner.width as usize;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(palette.query.clone(), Style::default().fg(text())),
            Span::styled("▏", Style::default().fg(accent())),
            Span::styled(
                if palette.query.is_empty() { " Type to filter" } else { "" },
                Style::default().fg(faint()),
            ),
        ]),
        Line::from(Span::styled("─".repeat(width), Style::default().fg(border2()))),
    ];
    let visible = (inner.height as usize).saturating_sub(2).max(1);
    let sel_row = rows.iter().position(|(h, i)| h.is_none() && *i == palette.index).unwrap_or(0);
    let start = sel_row.saturating_sub(visible.saturating_sub(2)).min(rows.len().saturating_sub(visible));
    for (header, i) in rows.iter().skip(start).take(visible) {
        if let Some(label) = header {
            lines.push(Line::from(Span::styled(
                label.to_string(),
                Style::default().fg(faint()).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        let (id, label) = matches[*i];
        let (section, shortcut) = palette_meta(id);
        let selected = *i == palette.index;
        let right = if grouped { shortcut.to_string() } else if shortcut.is_empty() { section.to_string() } else { format!("{shortcut} · {section}") };
        let label_w = width.saturating_sub(right.chars().count() + 3);
        lines.push(
            Line::from(vec![
                Span::styled(if selected { "▍ " } else { "  " }, Style::default().fg(accent())),
                Span::styled(
                    pad(label, label_w),
                    if selected {
                        Style::default().fg(text()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(soft())
                    },
                ),
                Span::styled(format!(" {right}"), Style::default().fg(faint())),
            ])
            .style(Style::default().bg(if selected { surface3() } else { surface2() })),
        );
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::styled("No matching command", Style::default().fg(faint()))));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(crate) fn render_key_hints(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let hints: &[(&str, &str)] = match app.shell.focus {
        Focus::Composer => &[("Enter", "send"), ("Ctrl-P", "commands"), ("Ctrl-F", "sessions"), ("Ctrl-L", "panel"), ("Ctrl-O", "steps"), ("Esc", "stop")],
        Focus::Sidebar => &[("↑↓", "move"), ("Enter", "open"), ("n", "new"), ("Esc", "back")],
        Focus::Pane => &[("↑↓", "move"), ("Enter", "open"), ("Tab", "tab"), ("Esc", "back")],
    };
    frame.render_widget(Paragraph::new(super::chrome::hint_line(hints)), area);
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
            "steps" => self.tools_expanded = !self.tools_expanded,
            "plan" => self.open_pane(PaneTab::Plan),
            "lanes" => self.open_pane(PaneTab::Lanes),
            "checkpoints" => self.open_pane(PaneTab::Checkpoints),
            "mission" => self.open_mission_control(),
            "agents" => self.open_pane(PaneTab::Agents),
            "tasks" => self.open_pane(PaneTab::Tasks),
            "jobs" => self.open_pane(PaneTab::Jobs),
            "usage" => self.open_pane(PaneTab::Usage),
            "vault" => self.open_pane(PaneTab::Vault),
            "inbox" => self.open_pane(PaneTab::Inbox),
            "procs" => self.open_pane(PaneTab::Procs),
            "git" => self.open_pane(PaneTab::Git),
            "files" => self.open_pane(PaneTab::Files),
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
            tab => self.board_len(tab),
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
            // Ctrl-F finds a session; Ctrl-B still works where tmux doesn't
            // claim it as its prefix.
            KeyCode::Char('f') | KeyCode::Char('b') => {
                if app.shell.sidebar_visible(width) && app.shell.focus == Focus::Sidebar {
                    app.shell.sidebar = Some(false);
                    app.shell.focus = Focus::Composer;
                } else {
                    app.shell.sidebar = Some(true);
                    app.shell.focus = Focus::Sidebar;
                    let rows = sidebar_rows(app);
                    app.shell.sidebar_index =
                        rows.iter().position(|c| app.is_active(c)).unwrap_or(0);
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
                    if let Some(row) = rows.get(app.shell.sidebar_index).cloned() {
                        app.shell.focus = Focus::Composer;
                        if !app.is_active(&row) {
                            app.open_conversation(&row);
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
            if app.shell.pane_detail.is_some() {
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
            if app.handle_board_key(key) || app.handle_panel_key(key) {
                return true;
            }
            if app.shell.tab == PaneTab::Usage {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => app.shell.pane_scroll = app.shell.pane_scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => app.shell.pane_scroll = app.shell.pane_scroll.saturating_add(1),
                    _ => {}
                }
                if matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Char('k') | KeyCode::Char('j')) {
                    return true;
                }
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
                    PaneTab::Agents | PaneTab::Tasks => {
                        app.shell.pane_detail = Some(app.shell.pane_index.min(len - 1));
                        app.shell.pane_scroll = 0;
                    }
                    PaneTab::Plan
                    | PaneTab::Jobs
                    | PaneTab::Usage
                    | PaneTab::Vault
                    | PaneTab::Inbox
                    | PaneTab::Procs
                    | PaneTab::Git
                    | PaneTab::Files => {}
                },
                _ => {}
            }
            true
        }
    }
}
