use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use serde_json::Value;

use crate::harness::{HarnessEvent, HarnessStatus};
use super::app::*;
use super::settings::*;
use super::theme::*;
use super::*;

pub(crate) fn render_checkpoint_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use super::chrome::{Row, age, note, screen_frame, window};
    let checkpoints = app
        .state
        .as_ref()
        .map(|s| s.checkpoints.clone())
        .unwrap_or_default();
    let is_rewind = app.screen == Screen::RewindCheckpointSelection;
    let (title, subtitle, verb) = if is_rewind {
        ("Rewind", "the chat and its files go back to just before the chosen message", "rewind")
    } else {
        ("Fork", "a new chat starts from just before the chosen message", "fork")
    };
    let body = screen_frame(frame, area, title, subtitle, &[("↑↓", "choose"), ("Enter", verb), ("Esc", "cancel")]);
    let width = body.width as usize;
    let selected = app.checkpoint_selected_index.min(checkpoints.len().saturating_sub(1));
    let (start, end) = window(&checkpoints, selected, body.height as usize);
    let mut lines = Vec::new();
    for (index, checkpoint) in checkpoints.iter().enumerate().take(end).skip(start) {
        let short_id: String = checkpoint.id.chars().take(8).collect();
        lines.push(
            Row {
                selected: index == selected,
                dot: Some(accent()),
                title: &checkpoint.label,
                emphasis: false,
                meta: &short_id,
                right: Some((age(&checkpoint.created_at), faint())),
            }
            .line(width),
        );
    }
    if lines.is_empty() {
        lines.push(note("No checkpoints yet — one is taken before each message."));
    }
    frame.render_widget(Paragraph::new(lines), body);
}

pub(crate) fn render_lanes(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use super::chrome::{Row, note, screen_frame, window};

    let lanes = app.state.as_ref().map(|state| &state.lanes);
    let count = |status: LaneStatus| lanes.map_or(0, |items| items.iter().filter(|i| i.status == status).count());
    let (running, completed, failed) = (count(LaneStatus::Running), count(LaneStatus::Completed), count(LaneStatus::Failed));
    let total = lanes.map_or(0, |items| items.len());
    let subtitle = if total == 0 {
        String::new()
    } else {
        format!("{running} running · {completed} done · {failed} failed")
    };
    let body = screen_frame(
        frame,
        area,
        "Delegated work",
        &subtitle,
        &[
            ("↑↓", "lane"),
            ("Enter", if app.lanes_detail_expanded { "collapse" } else { "full report" }),
            ("PgUp/PgDn", "scroll"),
            ("Esc", "back"),
        ],
    );
    let list_w = 34.min(body.width / 3).max(20);
    let list_area = Rect { width: list_w, ..body };
    let rule_area = Rect { x: body.x + list_w, width: 1, ..body };
    let detail_inner = Rect { x: body.x + list_w + 2, width: body.width.saturating_sub(list_w + 2), ..body };
    frame.render_widget(
        Paragraph::new(vec![Line::from(Span::styled("│", Style::default().fg(border2()))); body.height as usize]),
        rule_area,
    );

    let lane_color = |status: LaneStatus| match status {
        LaneStatus::Running => warn(),
        LaneStatus::Completed => success(),
        LaneStatus::Failed => danger(),
        LaneStatus::Cancelled => faint(),
    };
    let mut list_lines = Vec::new();
    if let Some(items) = lanes {
        let (start, end) = window(items, app.lanes_selected_index, list_area.height as usize);
        for (index, item) in items.iter().enumerate().take(end).skip(start) {
            list_lines.push(
                Row {
                    selected: index == app.lanes_selected_index,
                    dot: Some(lane_color(item.status)),
                    title: &item.title,
                    emphasis: false,
                    meta: "",
                    right: None,
                }
                .line(list_area.width as usize),
            );
        }
    }
    if list_lines.is_empty() {
        list_lines.push(note("No delegated work yet."));
    }
    frame.render_widget(Paragraph::new(list_lines), list_area);

    let mut detail_lines = Vec::new();
    let prose_w = detail_inner.width.saturating_sub(1) as usize;
    if let Some(item) = lanes.and_then(|items| items.get(app.lanes_selected_index)) {
        let (dot, color) = ("●", lane_color(item.status));
        detail_lines.push(Line::from(vec![
            Span::styled(format!("{dot} "), Style::default().fg(color)),
            Span::styled(
                item.title.clone(),
                Style::default().fg(text()).add_modifier(Modifier::BOLD),
            ),
        ]));
        detail_lines.push(Line::from(""));

        let summary_body = match item.status {
            LaneStatus::Running => item.activity.as_deref().unwrap_or("working…"),
            LaneStatus::Completed => item
                .summary
                .as_deref()
                .or(item.report.as_deref())
                .unwrap_or("completed"),
            LaneStatus::Failed | LaneStatus::Cancelled => {
                item.error.as_deref().unwrap_or("cancelled")
            }
        };

        if app.lanes_detail_expanded {
            if let Some(handoff) = item.handoff.as_deref().filter(|s| !s.trim().is_empty()) {
                detail_lines.push(Line::from(Span::styled(
                    "Handoff",
                    Style::default().fg(faint()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(handoff, prose_w));
                detail_lines.push(Line::from(""));
            }
            if !item.activity_log.is_empty() {
                detail_lines.push(Line::from(Span::styled(
                    "Activity",
                    Style::default().fg(faint()).add_modifier(Modifier::BOLD),
                )));
                for entry in &item.activity_log {
                    let kind = entry.kind.trim();
                    if entry.text.contains('\n')
                        || entry.text.contains('`')
                        || entry.text.contains("**")
                    {
                        detail_lines.push(Line::from(Span::styled(
                            kind.to_string(),
                            Style::default().fg(faint()),
                        )));
                        detail_lines.extend(markdown::render_prose(&entry.text, prose_w));
                    } else {
                        let mut line = vec![Span::styled(
                            format!("{kind}  "),
                            Style::default().fg(faint()),
                        )];
                        let body = markdown::render_prose(
                            &entry.text,
                            prose_w.saturating_sub(kind.chars().count() + 2),
                        );
                        if let Some(first) = body.into_iter().next() {
                            line.extend(first.spans);
                        }
                        detail_lines.push(Line::from(line));
                    }
                }
                detail_lines.push(Line::from(""));
            }
            if let Some(summary) = item.summary.as_deref().filter(|s| !s.trim().is_empty()) {
                detail_lines.push(Line::from(Span::styled(
                    "Summary",
                    Style::default().fg(faint()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(summary, prose_w));
                detail_lines.push(Line::from(""));
            }
            if let Some(report) = item.report.as_deref().filter(|s| !s.trim().is_empty()) {
                detail_lines.push(Line::from(Span::styled(
                    "Report",
                    Style::default().fg(faint()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(report, prose_w));
            }
        } else {
            detail_lines.push(Line::from(Span::styled(
                "Summary",
                Style::default().fg(faint()).add_modifier(Modifier::BOLD),
            )));
            let preview: String = summary_body.chars().take(700).collect();
            let preview = if summary_body.chars().count() > 700 {
                format!("{preview}…")
            } else {
                preview
            };
            detail_lines.extend(markdown::render_prose(&preview, prose_w));
        }
    } else {
        detail_lines.push(Line::from(Span::styled("Choose a lane to see its work.", Style::default().fg(faint()))));
    }

    let visible_h = detail_inner.height as usize;
    let max_scroll = detail_lines.len().saturating_sub(visible_h.max(1));
    let scroll = app.lanes_detail_scroll.min(max_scroll);
    let shown: Vec<Line<'static>> = detail_lines
        .into_iter()
        .skip(scroll)
        .take(visible_h.max(1))
        .collect();
    frame.render_widget(Paragraph::new(shown), detail_inner);
}

fn keycap(key: &str, label: &str, color: Color) -> [Span<'static>; 2] {
    [
        Span::styled(format!("{key} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{label}   "), Style::default().fg(muted())),
    ]
}

fn card_block(glyph: &str, title: String, tone: Color, frame_color: Color) -> ratatui::widgets::Block<'static> {
    use ratatui::widgets::{Block, BorderType, Borders, Padding};
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{glyph} "), Style::default().fg(tone).add_modifier(Modifier::BOLD)),
            Span::styled(title, Style::default().fg(soft())),
            Span::raw(" "),
        ]))
}

fn approval_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some((tool, summary, _, total)) = app.pending_approval() else {
        return Vec::new();
    };
    let inner = width.saturating_sub(4).max(10);
    let prefix = if tool == "bash" { "$ " } else { "" };
    let preview = if summary.trim().is_empty() {
        "(no preview)".to_string()
    } else {
        format!("{prefix}{}", summary.trim())
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let wrapped = wrap_one(&preview, inner);
    let more = wrapped.len().saturating_sub(4);
    for seg in wrapped.into_iter().take(4) {
        lines.push(Line::from(Span::styled(seg, Style::default().fg(text()))));
    }
    if more > 0 {
        lines.push(Line::from(Span::styled(format!("… {more} more lines"), Style::default().fg(faint()))));
    }
    let mut actions = Vec::new();
    actions.extend(keycap("y", "approve", success()));
    if total > 1 {
        actions.extend(keycap("a", "approve all", accent()));
    }
    actions.extend(keycap("n", "deny", danger()));
    actions.extend(keycap("esc", "stop", soft()));
    lines.push(Line::from(actions));
    lines
}

pub(crate) fn approval_height(app: &App, width: u16) -> u16 {
    let n = approval_lines(app, width as usize).len();
    if n == 0 { 0 } else { n as u16 + 2 }
}

fn approval_subject(tool: &str) -> String {
    match tool {
        "bash" => "command".into(),
        "change_files" => "file changes".into(),
        other => other.replace('_', " "),
    }
}

pub(crate) fn render_approval_bar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some((tool, _, index, total)) = app.pending_approval() else {
        return;
    };
    let title = if total > 1 {
        format!("Approve {} · {index} of {total}", approval_subject(&tool))
    } else {
        format!("Approve {}", approval_subject(&tool))
    };
    let block = card_block("!", title, warn(), warn());
    frame.render_widget(
        Paragraph::new(approval_lines(app, area.width as usize)).block(block),
        area,
    );
}

/// One-line auth/endpoint status for a profile card.

pub(crate) fn profile_status(cfg: &crate::config::InferenceProfileConfig) -> String {
    match cfg.provider.as_str() {
        "claude-code" => "installed Claude Code CLI".to_string(),
        "antigravity" => "installed Antigravity CLI".to_string(),
        "chatgpt" => {
            if crate::chatgpt_auth::is_signed_in() {
                "✓ signed in".to_string()
            } else {
                "not signed in — Enter to sign in".to_string()
            }
        }
        // Grok signs in with the SuperGrok / X Premium subscription; it never
        // takes an API key, so "no api key" would be a false warning.
        "xai" | "grok" => {
            if crate::xai_auth::is_signed_in() {
                "✓ signed in".to_string()
            } else {
                // Sign-in lives in the profile's edit form.
                "not signed in — e to sign in".to_string()
            }
        }
        "openai-compatible" => {
            let host = cfg
                .base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            host.split('/')
                .next()
                .filter(|h| !h.is_empty())
                .unwrap_or("custom endpoint")
                .to_string()
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
pub(crate) fn render_profiles(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use super::chrome::{Row, screen_frame, sub_line};
    let active_lanes = app
        .state
        .as_ref()
        .map(|s| s.lanes.iter().filter(|l| l.status == LaneStatus::Running).count())
        .unwrap_or(0);
    let subtitle = if active_lanes > 0 {
        format!("{active_lanes} delegated running")
    } else {
        "the profiles chats and delegated work run on".to_string()
    };
    let body = screen_frame(
        frame,
        area,
        "Models",
        &subtitle,
        &[("↑↓", "move"), ("Enter", "this chat"), ("g", "global"), ("l", "delegate"), ("e", "edit"), ("a", "add"), ("d", "delete"), ("Esc", "back")],
    );
    let width = body.width as usize;
    let names = app.options.config.profile_names();
    let total = names.len();
    let active = app.options.config.active_setup.clone().unwrap_or_default();
    let delegate = app.options.config.delegate_setup.clone().unwrap_or_default();
    let setups = app.options.config.setups.as_ref();
    let sel = app.profiles_selected_index.min(total);

    let visible = ((body.height as usize).saturating_sub(2) / 3).max(1);
    let focus = sel.min(total.saturating_sub(1));
    let start = if total > 0 && focus >= visible { focus + 1 - visible } else { 0 };
    let end = (start + visible).min(total);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if start > 0 {
        lines.push(super::chrome::note(&format!("↑ {start} more")));
    }
    for (i, name) in names.iter().enumerate().take(end).skip(start) {
        let is_sel = i == sel;
        let mut badges = Vec::new();
        if *name == active {
            badges.push("active");
        }
        if !delegate.is_empty() && *name == delegate {
            badges.push("delegate");
        }
        let right = (!badges.is_empty()).then(|| {
            (badges.join(" · "), if *name == active { success() } else { accent() })
        });
        lines.push(
            Row { selected: is_sel, dot: None, title: name, emphasis: true, meta: "", right }.line(width),
        );
        if let Some(cfg) = setups.and_then(|m| m.get(name)) {
            let model = if cfg.model.is_empty() { "no model".to_string() } else { cfg.model.clone() };
            lines.push(sub_line(&format!("{model} · {}", profile_status(cfg)), is_sel, 2, width));
        }
        lines.push(Line::from(""));
    }
    if end < total {
        lines.push(super::chrome::note(&format!("↓ {} more", total - end)));
    }
    let add_sel = sel >= total;
    lines.push(
        Row { selected: add_sel, dot: None, title: "+ Add a model", emphasis: false, meta: "", right: None }
            .line(width),
    );
    frame.render_widget(Paragraph::new(lines), body);

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
        let block = card_block("◆", "Model setup".into(), accent(), border2());
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(login_lines(app, inner.width as usize)).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

/// Where a new session works, chosen before it starts in a git repository.
pub(crate) fn render_new_session(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use super::chrome::{Row, section, screen_frame, sub_line};
    use crate::tui::commands::NewChoice;
    let folder = app
        .home_folder()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder")
        .to_string();
    let body = screen_frame(
        frame,
        area,
        "New session",
        &format!("in {folder} — where should it work?"),
        &[("↑↓", "choose"), ("Enter", "start"), ("Esc", "cancel")],
    );
    let width = body.width as usize;
    let name_of = |path: &std::path::Path| path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let mut lines = Vec::new();
    let mut existing_header = false;
    for (i, choice) in app.new_choices.iter().enumerate() {
        let (title, detail) = match choice {
            NewChoice::Worktree(_) => (
                "New worktree".to_string(),
                "Its own branch and checkout; other sessions' edits stay apart.".to_string(),
            ),
            NewChoice::Folder(path) => (
                format!("This folder ({})", name_of(path)),
                "Work directly in the checkout.".to_string(),
            ),
            NewChoice::Existing { path, branch } => {
                if !existing_header {
                    existing_header = true;
                    lines.push(section("Existing worktrees"));
                }
                (format!("⎇ {}", branch.clone().unwrap_or_else(|| name_of(path))), path.display().to_string())
            }
        };
        let selected = i == app.new_choice_index;
        lines.push(Row { selected, dot: None, title: &title, emphasis: true, meta: "", right: None }.line(width));
        lines.push(sub_line(&detail, selected, 2, width));
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines), body);
}

pub(crate) fn render_resume_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use super::chrome::{Row, compact_secs, note, screen_frame, window};
    let convs = match &app.conv_cache {
        Some(c) => c.clone(),
        None => app.list_conversations(),
    };
    let folder = app
        .home_folder()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let subtitle = format!("{folder} · {} session{}", convs.len(), if convs.len() == 1 { "" } else { "s" });
    let hints: &[(&str, &str)] = if app.resume_rename.is_some() {
        &[("Enter", "save"), ("Esc", "cancel")]
    } else if app.resume_pending_delete {
        &[("d", "delete"), ("any key", "keep")]
    } else {
        &[("↑↓", "move"), ("Enter", "open"), ("r", "rename"), ("d", "delete"), ("Esc", "back")]
    };
    let body = screen_frame(frame, area, "Sessions", &subtitle, hints);
    let width = body.width as usize;
    let mut lines = Vec::new();
    if let Some(name) = app.resume_rename.as_ref() {
        lines.push(Line::from(vec![
            Span::styled(" Rename  ", Style::default().fg(faint())),
            Span::styled(name.clone(), Style::default().fg(text())),
            Span::styled("▏", Style::default().fg(accent())),
        ]));
        lines.push(Line::from(""));
    } else if app.resume_pending_delete {
        lines.push(Line::from(Span::styled(
            " Delete this session? d again to delete, any other key keeps it.",
            Style::default().fg(warn()),
        )));
        lines.push(Line::from(""));
    }
    if convs.is_empty() {
        lines.push(note("No saved sessions in this folder yet."));
    } else {
        let selected_idx = app.resume_selected_index.min(convs.len() - 1);
        let visible = (body.height as usize).saturating_sub(lines.len());
        let (start, end) = window(&convs, selected_idx, visible);
        for (offset, c) in convs[start..end].iter().enumerate() {
            let meta = c.branch.as_ref().map(|b| format!("⎇ {b}")).unwrap_or_default();
            let age = compact_secs(chrono::Utc::now().timestamp() - c.last_active);
            lines.push(
                Row {
                    selected: start + offset == selected_idx,
                    dot: Some(super::boards::status_color(&c.status)),
                    title: &c.title,
                    emphasis: false,
                    meta: &meta,
                    right: Some((age, faint())),
                }
                .line(width),
            );
        }
    }
    frame.render_widget(Paragraph::new(lines), body);
}

/// Models offered by the picker: the live-fetched list (uncapped) or the static
/// fallback for the provider, filtered by the search query (case-insensitive).


pub(crate) fn render_term(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    // Never call open_term() here — it sends on the sidecar and panics
    // ratatui if the pane list is empty or the socket is mid-draw.
    if app.term_panes.is_empty() {
        app.term_panes.push(TermPane {
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
        app.term_focus = 0;
    }
    let n = app.term_panes.len().max(1);
    app.term_focus = app.term_focus.min(n - 1);
    let tab_h: u16 = if n > 1 { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(tab_h), Constraint::Min(2)])
        .split(area);
    if tab_h > 0 {
        let mut spans = Vec::new();
        for (i, pane) in app.term_panes.iter().enumerate() {
            let label = format!(" {} ", i + 1);
            let style = if i == app.term_focus {
                Style::default().fg(accent()).add_modifier(Modifier::BOLD)
            } else {
                subtle()
            };
            spans.push(Span::styled(label, style));
            let _ = pane;
        }
        spans.push(Span::styled(
            "  Ctrl-T next · Ctrl-N new · Ctrl-W close",
            subtle(),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);
    }
    let body = chunks[1];
    let cols = body.width.max(2) as usize;
    let rows = body.height.max(2) as usize;
    let focus = app.term_focus;
    let (need_resize, need_open, id) = {
        let pane = &mut app.term_panes[focus];
        let need = cols as u16 != pane.cols || rows as u16 != pane.rows;
        if need {
            pane.cols = cols as u16;
            pane.rows = rows as u16;
            pane.vt.resize(cols, rows);
        }
        (need, !pane.opened, pane.id.clone())
    };
    if need_open {
        // Spawn at the measured pane size — never 80×24 then resize.
        app.send_term_open();
    } else if need_resize {
        if let Some(a) = app.sidecar_attach.as_ref() {
            let _ = a.send_term(serde_json::json!({
                "wire": "term",
                "op": "resize",
                "id": id,
                "cols": cols,
                "rows": rows,
            }));
        }
    }
    let pane = &app.term_panes[focus];
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    let (raw_cx, raw_cy) = pane.vt.cursor();
    // Pending wrap: emulator cx == cols (past last cell). Paint the block
    // on the next row so the cursor never vanishes mid-line.
    let (cx, cy) = if raw_cx >= pane.vt.cols {
        (0usize, (raw_cy + 1).min(pane.vt.rows.saturating_sub(1)))
    } else {
        (raw_cx, raw_cy)
    };
    for y in 0..rows.min(pane.vt.rows) {
        let mut spans = Vec::new();
        for x in 0..cols.min(pane.vt.cols) {
            let cell = pane.vt.cell(x, y);
            let mut fg = if cell.inverse {
                ansi_color(cell.bg)
            } else {
                ansi_color(cell.fg)
            };
            let bg = if cell.inverse {
                ansi_color(cell.fg)
            } else if cell.bg == 0 {
                Color::Reset
            } else {
                ansi_color(cell.bg)
            };
            if x == cx && y == cy {
                fg = Color::Black;
            }
            let mut style = Style::default().fg(fg);
            if x == cx && y == cy {
                style = style.bg(Color::White);
            } else if bg != Color::Reset {
                style = style.bg(bg);
            }
            if cell.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(cell.ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), body);
}


pub(crate) fn questions_of(app: &App) -> Vec<Value> {
    app.state
        .as_ref()
        .filter(|s| s.status == HarnessStatus::WaitingForInput)
        .and_then(|s| s.pending_question.as_ref())
        .and_then(|p| p.get("questions"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn q_text(question: &Value) -> String {
    question
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("(question)")
        .to_string()
}

/// One selectable answer to a question.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QOption {
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) recommended: bool,
}

impl QOption {
    fn plain(value: &str, label: String) -> Self {
        Self { value: value.into(), label, description: String::new(), recommended: false }
    }

    /// How the choice reads in the answer sent back: the label, with the value
    /// alongside when it says something the label doesn't ("android" for
    /// "Android" adds nothing).
    pub(crate) fn answer(&self) -> String {
        let norm = |x: &str| x.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        if norm(&self.value) == norm(&self.label) {
            self.label.clone()
        } else {
            format!("{} ({})", self.label, self.value)
        }
    }
}

pub(crate) fn q_kind(question: &Value) -> &str {
    question
        .get("answer_kind")
        .and_then(|k| k.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("free_text")
}

/// The question's tab label when several are asked together.
pub(crate) fn q_header(question: &Value, index: usize) -> String {
    question
        .get("header")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Q{}", index + 1))
}

/// Selectable options for a question, the recommended one first. Empty =
/// free-text (the answer is typed in the input box instead of picked).
pub(crate) fn q_options(question: &Value) -> Vec<QOption> {
    let ak = question.get("answer_kind");
    let label_or = |k: &str, fallback: &str| {
        ak.and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    let text = |c: &Value, k: &str| {
        c.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string()
    };
    match q_kind(question) {
        "single_choice" | "multi_choice" => {
            let mut opts: Vec<QOption> = ak
                .and_then(|a| a.get("choices"))
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .map(|c| QOption {
                                value: text(c, "value"),
                                label: text(c, "label"),
                                description: text(c, "description"),
                                recommended: c.get("recommended").and_then(Value::as_bool).unwrap_or(false),
                            })
                        .collect()
                })
                .unwrap_or_default();
            // Stable: the recommended option leads, the rest keep their order.
            opts.sort_by_key(|o| !o.recommended);
            opts
        }
        "yes_no" => vec![QOption::plain("yes", "Yes".into()), QOption::plain("no", "No".into())],
        "confirm" => vec![
            QOption::plain("confirm", label_or("confirm_label", "Confirm")),
            QOption::plain("cancel", label_or("cancel_label", "Cancel")),
        ],
        _ => Vec::new(),
    }
}

/// Reset the picker cursor when a fresh question set arrives, and keep the
/// indices in range.
pub(crate) fn ensure_q_init(app: &mut App) {
    let qs = questions_of(app);
    // Fingerprint by ASK, not just question text: prefix with the count of
    // user_question events so the agent asking the SAME question twice in a row
    // still resets the picker (text alone left stale q_index/q_sel/q_answers).
    let asks = app
        .state
        .as_ref()
        .map(|s| {
            s.events
                .iter()
                .filter(|e| matches!(e, HarnessEvent::UserQuestion { .. }))
                .count()
        })
        .unwrap_or(0);
    let token = format!(
        "{asks}\u{1}{}",
        qs.iter().map(q_text).collect::<Vec<_>>().join("\u{1}")
    );
    if token != app.q_token {
        app.q_token = token;
        app.q_index = 0;
        app.q_answers.clear();
        app.q_review = false;
        prepare_question(app, &qs);
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

/// Point the picker at the current question: the cursor on the first option
/// (the recommended one leads), and a multi-choice question's recommended
/// options already ticked.
pub(crate) fn prepare_question(app: &mut App, qs: &[Value]) {
    app.q_sel = 0;
    app.q_multi.clear();
    if let Some(q) = qs.get(app.q_index) {
        if q_kind(q) == "multi_choice" {
            for (i, o) in q_options(q).iter().enumerate() {
                if o.recommended {
                    app.q_multi.insert(i);
                }
            }
        }
    }
}

/// Intercept navigation/selection keys while an ask_user question is pending.
/// Returns true if the key was consumed. Free-text questions let typing fall
/// through to the input box (only Enter is intercepted, to commit the answer).

pub(crate) fn pending_question_text(app: &App) -> Option<String> {
    let state = app.state.as_ref()?;
    if state.status != HarnessStatus::WaitingForInput {
        return None;
    }
    question_text(state.pending_question.as_ref()?)
}

pub(crate) fn question_text(pending: &Value) -> Option<String> {
    let questions = pending.get("questions").and_then(Value::as_array)?;
    let rendered = questions
        .iter()
        .filter_map(|q| q.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("  |  ");
    (!rendered.is_empty()).then_some(rendered)
}

fn question_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let qs = questions_of(app);
    if qs.is_empty() {
        return Vec::new();
    }
    let inner = width.saturating_sub(4).max(10);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let bold = Style::default().fg(text()).add_modifier(Modifier::BOLD);
    let current = app.q_index.min(qs.len() - 1);

    // Several questions: one tab per question, answered ones ticked.
    if qs.len() > 1 {
        let mut tabs = Vec::new();
        for (i, q) in qs.iter().enumerate() {
            if i > 0 {
                tabs.push(Span::styled("  ", Style::default()));
            }
            let header = q_header(q, i);
            let answered = i < app.q_answers.len();
            let (label, style) = if !app.q_review && i == current {
                (header, Style::default().fg(accent()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
            } else if answered {
                (format!("✓ {header}"), Style::default().fg(success()))
            } else {
                (header, Style::default().fg(faint()))
            };
            tabs.push(Span::styled(label, style));
        }
        if app.q_review {
            tabs.push(Span::styled("  Review", Style::default().fg(accent()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)));
        }
        lines.push(Line::from(tabs));
        lines.push(Line::from(""));
    }

    // The last step of a multi-question set: every answer, then send.
    if app.q_review {
        lines.push(Line::from(Span::styled("Check your answers", bold)));
        for (i, (q, (_, answer))) in qs.iter().zip(app.q_answers.iter()).enumerate() {
            lines.push(Line::from(vec![
                Span::styled(format!(" {}  ", q_header(q, i)), Style::default().fg(faint())),
                Span::styled(answer.chars().take(inner.saturating_sub(14)).collect::<String>(), Style::default().fg(text())),
            ]));
        }
        lines.push(Line::from(""));
        let hints = [keycap("↵", "send", soft()), keycap("←", "edit", soft()), keycap("esc", "cancel", soft())];
        lines.push(Line::from(hints.into_iter().flatten().collect::<Vec<_>>()));
        return lines;
    }

    let question = &qs[current];
    let wrapped = wrap_one(&q_text(question), inner);
    let more = wrapped.len() > 5;
    for seg in wrapped.into_iter().take(if more { 4 } else { 5 }) {
        lines.push(Line::from(Span::styled(seg, bold)));
    }
    if more {
        lines.push(Line::from(Span::styled("…", Style::default().fg(faint()))));
    }
    lines.push(Line::from(""));
    let opts = q_options(question);
    let multi = q_kind(question) == "multi_choice";
    let back = current > 0;
    let mut hints = Vec::new();
    if opts.is_empty() {
        lines.push(Line::from(Span::styled("Type your answer in the box below", Style::default().fg(muted()))));
        hints.push(keycap("↵", "send", soft()));
    } else {
        let sel = app.q_sel.min(opts.len() - 1);
        for (i, opt) in opts.iter().enumerate().take(9) {
            let focused = i == sel;
            let mark = if multi {
                if app.q_multi.contains(&i) { "[x] " } else { "[ ] " }
            } else if focused {
                "▸ "
            } else {
                "  "
            };
            let badge = if opt.recommended { " recommended" } else { "" };
            let room = inner.saturating_sub(4 + mark.len() + badge.len());
            let label: String = opt.label.chars().take(room).collect();
            let pad = inner.saturating_sub(4 + mark.chars().count() + label.chars().count() + badge.len());
            let row_style = if focused { Style::default().bg(surface3()) } else { Style::default() };
            lines.push(
                Line::from(vec![
                    Span::styled(format!("{} ", i + 1), Style::default().fg(faint())),
                    Span::styled(mark.to_string(), Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                    Span::styled(label, if focused { bold } else { Style::default().fg(soft()) }),
                    Span::styled(badge.to_string(), Style::default().fg(accent())),
                    Span::raw(" ".repeat(pad)),
                ])
                .style(row_style),
            );
            if !opt.description.is_empty() {
                let indent = 2 + mark.chars().count();
                let desc: String = opt.description.chars().take(inner.saturating_sub(indent + 1)).collect();
                lines.push(Line::from(Span::styled(format!("{}{desc}", " ".repeat(indent)), Style::default().fg(faint()))));
            }
        }
        lines.push(Line::from(Span::styled("or type your own answer below", Style::default().fg(faint()))));
        hints.push(keycap(if multi { "1-9 space" } else { "1-9" }, if multi { "toggle" } else { "pick" }, soft()));
        hints.push(keycap("↵", if multi { "confirm" } else { "select" }, soft()));
    }
    if back {
        hints.push(keycap("←", "back", soft()));
    }
    hints.push(keycap("esc", "cancel", soft()));
    lines.push(Line::from(hints.into_iter().flatten().collect::<Vec<_>>()));
    lines
}

pub(crate) fn question_height(app: &App, width: u16) -> u16 {
    let n = question_lines(app, width as usize).len();
    if n == 0 { 0 } else { n as u16 + 2 }
}

pub(crate) fn render_question(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let qs = questions_of(app);
    if qs.is_empty() || area.height == 0 {
        return;
    }
    let title = if qs.len() > 1 {
        format!("Questions · {} of {}", (app.q_index + 1).min(qs.len()), qs.len())
    } else {
        "Question".to_string()
    };
    let block = card_block("?", title, accent(), border2());
    frame.render_widget(
        Paragraph::new(question_lines(app, area.width as usize)).block(block),
        area,
    );
}

/// The compact "📎 N attachments" summary shown above the prompt when files are
/// queued (they live outside the input text). None when there are none.

pub(crate) fn lane_lines(app: &App) -> Vec<Line<'static>> {
    let Some(state) = &app.state else {
        return Vec::new();
    };
    let running: Vec<&crate::lanes::LaneRecord> = state
        .lanes
        .iter()
        .filter(|l| l.status == LaneStatus::Running)
        .collect();
    if running.is_empty() {
        return Vec::new();
    }
    let rail = Span::styled(" │ ", Style::default().fg(faint()));
    running
        .iter()
        .take(4)
        .map(|l| {
            let elapsed = chrono::DateTime::parse_from_rfc3339(&l.started_at)
                .ok()
                .map(|t| {
                    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                        .num_seconds()
                        .max(0);
                    if secs < 60 {
                        format!("{secs}s")
                    } else {
                        format!("{}m", secs / 60)
                    }
                })
                .unwrap_or_default();
            let mut title: String = l.title.chars().take(48).collect();
            if l.title.chars().count() > 48 {
                title.push('…');
            }
            let mut spans = vec![
                rail.clone(),
                Span::styled("◆ ", Style::default().fg(lane())),
                Span::styled(title, Style::default().fg(muted())),
            ];
            if let Some(agent) = &l.agent {
                spans.push(Span::styled(format!(" [{agent}]"), Style::default().fg(muted())));
            }
            if let Some(profile) = &l.profile {
                spans.push(Span::styled(format!(" [{profile}]"), Style::default().fg(faint())));
            }
            spans.push(Span::styled(
                format!(" — running {elapsed}"),
                Style::default().fg(faint()),
            ));
            Line::from(spans)
        })
        .collect()
}

/// Messages held for after the current run — a quiet block above the prompt so
/// the user can SEE what will fire (and cancel with Ctrl+X). Header first, then
/// up to 3 previews behind a dim rail, then an overflow count. They send as ONE
/// combined message on idle (see flush_queued_input).

pub(crate) fn queued_lines(app: &App) -> Vec<Line<'static>> {
    let n = app.held_queue().len();
    if n == 0 {
        return Vec::new();
    }
    let rail = Span::styled(" │ ", Style::default().fg(faint()));
    let header = if n == 1 {
        "queued — sends when idle · Ctrl+G steers now · Ctrl+X cancel".to_string()
    } else {
        format!("queued ({n}) — send when idle · Ctrl+G steers now · Ctrl+X cancel")
    };
    let mut lines = vec![Line::from(vec![
        rail.clone(),
        Span::styled(header, Style::default().fg(faint())),
    ])];
    for q in app.held_queue().iter().take(3) {
        let first = q.text.lines().next().unwrap_or("");
        let mut text: String = first.chars().take(72).collect();
        if first.chars().count() > 72 || q.text.lines().count() > 1 {
            text.push('…');
        }
        lines.push(Line::from(vec![
            rail.clone(),
            Span::styled(text, Style::default().fg(muted())),
        ]));
    }
    let extra = n.saturating_sub(3);
    if extra > 0 {
        lines.push(Line::from(vec![
            rail,
            Span::styled(format!("… +{extra} more"), Style::default().fg(faint())),
        ]));
    }
    lines
}

/// Height (incl. top/bottom borders) the input box needs for the current prompt,
/// clamped so it grows with wrapped/multi-line input but never dominates the view.

pub(crate) fn login_lines(app: &App, width: usize) -> Vec<Line<'static>> {
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

    let pad = width.min(56).saturating_sub(18).max(3);
    lines.push(Line::from(vec![
        Span::styled("── ", Style::default().fg(rule)),
        Span::styled(
            "connect a model",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}", "─".repeat(pad)), Style::default().fg(rule)),
    ]));
    lines.push(Line::from(""));

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
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::raw("  "),
        ];
        spans.extend(value);
        Line::from(spans)
    };

    let chooser = |text: String, focused: bool, suffix: &str| -> Vec<Span<'static>> {
        if focused {
            vec![
                Span::styled("‹ ", Style::default().fg(accent)),
                Span::styled(text, Style::default().fg(w).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" ›{suffix}"), Style::default().fg(accent)),
            ]
        } else {
            vec![Span::styled(
                format!("{text}{suffix}"),
                Style::default().fg(w),
            )]
        }
    };

    let p_focus = focus == SettingsField::Provider;
    lines.push(field_row(
        "provider",
        p_focus,
        chooser(app.form_provider.clone(), p_focus, ""),
    ));

    // xAI (Grok/X subscription) signs in via a device code — no API key / base URL.
    if app.form_provider == "xai" {
        let signed_in = crate::xai_auth::is_signed_in();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "    xAI account",
                Style::default().fg(w).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  ·  SuperGrok / X Premium subscription",
                Style::default().fg(dim),
            ),
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
        } else if let Some(info) = &app.xai_device_code {
            lines.push(Line::from(Span::styled(
                format!("    Device code: {}", info.user_code),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("    Open {} to complete sign-in", info.verification_uri),
                Style::default().fg(w),
            )));
            lines.push(Line::from(Span::styled(
                "    c = copy code  ·  u = copy URL".to_string(),
                Style::default().fg(faint),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "    Enter = sign in with SuperGrok / X Premium".to_string(),
                Style::default().fg(accent),
            )));
        }
        lines.push(Line::from(""));
    } else if app.form_provider == "chatgpt" {
        let signed_in = crate::chatgpt_auth::is_signed_in();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "    ChatGPT account",
                Style::default().fg(w).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  ·  browser or device-code sign in",
                Style::default().fg(dim),
            ),
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
                "    c = copy code  ·  u = copy URL  ·  Enter = browser sign-in".to_string(),
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
        let k_focus = focus == SettingsField::ApiKey;
        let key_len = app.form_api_key.chars().count();
        let key_val = if key_len == 0 {
            vec![Span::styled(
                if k_focus {
                    "█".to_string()
                } else {
                    "(required)".to_string()
                },
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

    if provider_needs_base_url(&app.form_provider) {
        let u_focus = focus == SettingsField::BaseUrl;
        let mut url_val = vec![Span::styled(
            app.form_base_url.clone(),
            Style::default().fg(if app.form_base_url.is_empty() {
                faint
            } else {
                w
            }),
        )];
        if u_focus {
            url_val.push(Span::styled("█", Style::default().fg(accent)));
        }
        lines.push(field_row("base url", u_focus, url_val));
    }

    let m_focus = focus == SettingsField::Model;
    let model_text = if app.form_model.is_empty() {
        "(pick one)".to_string()
    } else {
        app.form_model.clone()
    };
    lines.push(field_row(
        "model",
        m_focus,
        chooser(model_text, m_focus, " ▾"),
    ));

    let r_focus = focus == SettingsField::Reasoning;
    let reasoning = app
        .form_reasoning_effort
        .clone()
        .unwrap_or_else(|| "medium".to_string());
    let reasoning_hint = match app.form_provider.as_str() {
        "anthropic" | "anthropic-compatible" => "thinking",
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
        Style::default().fg(if app.form_context_window.is_empty() {
            faint
        } else {
            w
        }),
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

    if app.form_provider == "xai" {
        let xs_focus = focus == SettingsField::XSearch;
        lines.push(field_row(
            "x search",
            xs_focus,
            chooser(
                if app.form_x_search {
                    "on".to_string()
                } else {
                    "off".to_string()
                },
                xs_focus,
                "",
            ),
        ));
    }

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
                        if is_cur {
                            "          ▸ "
                        } else {
                            "            "
                        },
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

