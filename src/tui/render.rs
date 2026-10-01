use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::app::*;
use super::theme::*;
use super::views::*;
use super::*;

pub(crate) fn render_connecting(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Multi-line pulse ring: reads large without any status text.
    pub(crate) const FRAMES: [&[&str]; 8] = [
        &[
            "  · · ·  ",
            " ·     · ",
            "·   ◆   ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·     · ",
            "·  ◆    ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·  ◆  · ",
            "·       ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·     · ",
            "·    ◆  ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·     · ",
            "·   ◆   ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·     · ",
            "·       ·",
            " ·  ◆  · ",
            "  · · ·  ",
        ],
        &[
            "  · · ·  ",
            " ·     · ",
            "·  ◆    ·",
            " ·     · ",
            "  · · ·  ",
        ],
        &[
            "  · ◆ ·  ",
            " ·     · ",
            "·       ·",
            " ·     · ",
            "  · · ·  ",
        ],
    ];
    let frame_i = (app.frame / 3) % FRAMES.len();
    let rows = FRAMES[frame_i];
    let body: Vec<Line<'static>> = rows
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                (*row).to_string(),
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    let block_h = body.len() as u16;
    let top = area.height.saturating_sub(block_h) / 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top),
            Constraint::Length(block_h),
            Constraint::Min(0),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(body).alignment(ratatui::layout::Alignment::Center),
        chunks[1],
    );
}


pub(crate) fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();

    if app.connecting_phase.is_some() {
        render_connecting(frame, area, app);
        return;
    }

    if app.screen == Screen::ResumeSelection {
        render_resume_selection(frame, area, app);
        return;
    }

    if app.screen == Screen::NewSession {
        render_new_session(frame, area, app);
        return;
    }

    if app.screen == Screen::RewindCheckpointSelection
        || app.screen == Screen::ForkCheckpointSelection
    {
        render_checkpoint_selection(frame, area, app);
        return;
    }

    if app.screen == Screen::Profiles {
        render_profiles(frame, area, app);
        return;
    }

    if app.screen == Screen::Lanes {
        render_lanes(frame, area, app);
        return;
    }

    if app.screen == Screen::Term {
        render_term(frame, area, app);
        return;
    }

    app.shell.width = area.width;
    let (sidebar_area, centre, pane_area) = shell_areas(app, area);
    if let Some(sidebar) = sidebar_area {
        render_sidebar(frame, sidebar, app);
    }
    if let Some(pane) = pane_area {
        render_pane(frame, pane, app);
    }
    let area = Rect {
        x: centre.x + u16::from(sidebar_area.is_some()),
        width: centre.width.saturating_sub(u16::from(sidebar_area.is_some()) + u16::from(pane_area.is_some())),
        ..centre
    };

    let sugg_h = suggestion_height(app);
    let input_h = input_height(app, area.width);
    // Compaction/prune status lives in the footer usage cluster (bottom-right).
    // Approval prompt (manual mode): rows directly above the input when a mutating
    // tool is awaiting y/n.
    let approval_h = approval_height(app, area.width);

    // Header, Content, Suggestions, Question, Approval, Input, Status, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                    // Header
            Constraint::Length(1),                    // gap under header
            Constraint::Min(10),                      // Content
            Constraint::Length(sugg_h),               // Suggestions
            Constraint::Length(question_height(app, area.width)), // Question
            Constraint::Length(approval_h),           // Approval prompt (above input)
            Constraint::Length(1),                    // gap above input
            Constraint::Length(input_h),              // Input (grows with wrapped lines)
            Constraint::Length(1),                    // Status message
            Constraint::Length(1),                    // Footer (metadata + usage)
        ])
        .split(area);

    let header_area = chunks[0];
    let header_rule_area = chunks[1];
    let content_area = chunks[2];
    let suggestions_area = chunks[3];
    let question_area = chunks[4];
    let approval_area = chunks[5];
    let input_area = chunks[7];
    let status_msg_area = chunks[8];
    let footer_area = chunks[9];

    render_header(frame, header_area, app);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(header_rule_area.width as usize),
            Style::default().fg(faint()),
        ))),
        header_rule_area,
    );
    render_history(frame, content_area, app);
    if sugg_h > 0 {
        render_suggestions(frame, suggestions_area, app);
    }
    render_question(frame, question_area, app);
    if approval_h > 0 {
        render_approval_bar(frame, approval_area, app);
    }
    render_input(frame, input_area, app);
    if app.error.is_none() && app.status.is_empty() {
        render_key_hints(frame, status_msg_area, app);
    } else {
        render_status_message(frame, status_msg_area, app);
    }
    render_status(frame, footer_area, app);
    render_palette(frame, frame.area(), app);
}


pub(crate) fn get_suggestions(app: &App) -> Vec<(String, String)> {
    if !app.input.starts_with('/') {
        return Vec::new();
    }

    if app.input.starts_with("/resume") {
        let query_part = if app.input.starts_with("/resume ") {
            &app.input["/resume ".len()..]
        } else {
            ""
        };

        let convs = app.list_conversations();
        return convs
            .into_iter()
            .filter(|c| c.name.starts_with(query_part))
            .map(|c| {
                let desc = match &c.branch {
                    Some(branch) => format!("⎇ {branch} {}", c.desc),
                    None => c.desc,
                };
                (format!("/resume {}", c.name), desc)
            })
            .collect();
    }

    if app.input.starts_with("/rewind") {
        let query = app.input.strip_prefix("/rewind ").unwrap_or("");
        let checkpoints = app
            .state
            .as_ref()
            .map(|s| s.checkpoints.clone())
            .unwrap_or_default();
        return checkpoints
            .iter()
            .rev()
            .map(|c| {
                let short = &c.id[..c.id.len().min(8)];
                (format!("/rewind {short}"), c.label.clone())
            })
            .filter(|(cmd, _)| cmd.contains(query))
            .collect();
    }

    if app.input.starts_with("/fork") {
        let query = app.input.strip_prefix("/fork ").unwrap_or("");
        let checkpoints = app
            .state
            .as_ref()
            .map(|s| s.checkpoints.clone())
            .unwrap_or_default();
        let mut out: Vec<(String, String)> = Vec::new();
        if query.is_empty() {
            out.push((
                "/fork".to_string(),
                "Branch at latest checkpoint (or full history)".to_string(),
            ));
        }
        for c in checkpoints.iter().rev() {
            let short = &c.id[..c.id.len().min(8)];
            let cmd = format!("/fork {short}");
            if query.is_empty()
                || short.contains(query)
                || c.label
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase())
            {
                out.push((cmd, c.label.clone()));
            }
        }
        return out;
    }

    if !app.input.contains(' ') {
        ALL_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&app.input))
            .map(|(cmd, desc)| (cmd.to_string(), desc.to_string()))
            .collect()
    } else {
        Vec::new()
    }
}

pub(crate) fn suggestion_height(app: &App) -> u16 {
    let matches_count = get_suggestions(app).len();
    if matches_count > 0 {
        matches_count as u16 + 1
    } else {
        0
    }
}

pub(crate) fn render_suggestions(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let matches = get_suggestions(app);

    let mut lines = Vec::new();
    for (idx, (cmd, desc)) in matches.iter().enumerate() {
        let is_selected = idx == app.suggestion_index;
        // Accent-bar flat style: the selected row gets a left ▍ bar + bold accent
        // command; others sit dim with a blank gutter. No full-width background.
        let (bar, cmd_style, desc_style) = if is_selected {
            (
                Span::styled("▍ ", Style::default().fg(accent())),
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                subtle(),
            )
        } else {
            (
                Span::raw("  "),
                Style::default().fg(self::text()),
                Style::default().fg(faint()),
            )
        };
        lines.push(Line::from(vec![
            bar,
            Span::styled(format!("{:<10}", cmd), cmd_style),
            Span::styled(format!("  {desc}"), desc_style),
        ]));
    }

    use ratatui::widgets::{Block, Borders};
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(faint()));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}


pub(crate) fn render_status_message(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let line = if let Some(ref err) = app.error {
        Line::from(vec![
            Span::styled(
                "error: ",
                Style::default().fg(danger()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.to_string(), Style::default().fg(danger())),
        ])
    } else {
        Line::from(vec![Span::styled(&app.status, subtle())])
    };
    frame.render_widget(Paragraph::new(line), area);
}


pub(crate) fn ansi_color(idx: u8) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::Gray,
        n => Color::Indexed(n),
    }
}


pub(crate) fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let has_events = app
        .state
        .as_ref()
        .map(|s| !s.events.is_empty())
        .unwrap_or(false);

    if !has_events {
        return;
    }

    let prompt_text = app
        .state
        .as_ref()
        .and_then(|s| s.title.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("snippet");

    let left = vec![Span::styled(
        prompt_text,
        Style::default().fg(text()).add_modifier(Modifier::BOLD),
    )];

    let (dot, label, color) = match app.state.as_ref().map(|s| s.status) {
        Some(HarnessStatus::Running) => ("●", "Working", accent()),
        Some(HarnessStatus::WaitingForInput) => ("●", "Waiting for you", warn()),
        Some(HarnessStatus::Failed) => ("●", "Failed", danger()),
        _ => ("○", "Idle", faint()),
    };
    let mut right: Vec<Span<'static>> = vec![
        Span::styled(format!("{dot} "), Style::default().fg(color)),
        Span::styled(label.to_string(), Style::default().fg(muted())),
    ];

    // Compact lane indicator in the header bar.
    if let Some(state) = &app.state {
        if !state.lanes.is_empty() {
            let running = state
                .lanes
                .iter()
                .filter(|l| l.status == LaneStatus::Running)
                .count();
            let total = state.lanes.len();
            let label = if running > 0 {
                format!("{running} of {total} delegated running   ")
            } else {
                format!("{total} delegated   ")
            };
            right.insert(0, Span::styled(label, Style::default().fg(faint())));
        }
    }

    let update = app.update_notice.lock().ok().and_then(|g| g.clone());
    if let Some(v) = update {
        right.insert(
            0,
            Span::styled(format!("⬆ v{v}   "), Style::default().fg(accent())),
        );
    }

    let left_line = Line::from(left);
    let right_line = Line::from(right);
    let right_w = right_line.width() as u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w + 1)])
        .split(area);
    frame.render_widget(Paragraph::new(left_line), cols[0]);
    frame.render_widget(
        Paragraph::new(right_line).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}


pub(crate) fn render_history(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // A small breathing gutter from the terminal edge (the transcript carries its
    // own left rhythm, so this stays minimal — a big outer inset over-padded it).
    let gutter = 1u16;
    let inner = Rect {
        x: area.x + gutter,
        y: area.y,
        width: area.width.saturating_sub(gutter * 2),
        height: area.height,
    };
    let width = (inner.width as usize).max(20);
    let height = inner.height as usize;

    // Empty state: no conversation yet (and not in the login form) — show a small
    // animated splash centered in the content area instead of a blank screen.
    let empty = !app.login_active
        && app.state.as_ref().map_or(true, |s| {
            s.events.is_empty()
                && s.title
                    .as_deref()
                    .is_none_or(|title| title.trim().is_empty())
        });
    if empty {
        let block = empty_state_lines(&app.cwd_display, &app.effective_model.1, width);
        // Sit a little above the vertical middle so it reads as a starting page,
        // not floating dead-center.
        let top = (height.saturating_sub(block.len()) / 3).max(1);
        let mut lines: Vec<Line<'static>> = std::iter::repeat(Line::from("")).take(top).collect();
        lines.extend(block);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let lines = transcript_lines(app, width);

    let max_scroll = lines.len().saturating_sub(height);
    app.max_scroll.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);

    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let window = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(window), inner);
}

/// The questions array of the active ask_user prompt, or empty when not waiting.

pub(crate) fn attachment_line(app: &App) -> Option<Line<'static>> {
    let n = app.attachments.len();
    if n == 0 {
        return None;
    }
    Some(Line::from(vec![
        Span::raw("   "),
        Span::styled(
            format!("📎 {n} attachment{}", if n == 1 { "" } else { "s" }),
            Style::default().fg(accent()),
        ),
        Span::styled("  ·  ⌫ removes", Style::default().fg(faint())),
    ]))
}


pub(crate) fn render_input(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders};

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));

    // While the login form is open, editing happens in the inline form above —
    // the input box just shows the controls.
    if app.login_active {
        let line = Line::from(vec![
            Span::styled(
                " ❯ ",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Tab next · ←/→ change · Enter connect · Esc cancel",
                Style::default().fg(faint()),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    if app.input.is_empty() {
        let has_events = app
            .state
            .as_ref()
            .map(|s| !s.events.is_empty())
            .unwrap_or(false);

        if !has_events {
            return;
        }

        let prompt = Line::from(vec![
            Span::styled(
                "✦ ",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("type a prompt to steer...", Style::default().fg(muted())),
        ]);
        let mut lines = Vec::new();
        lines.extend(lane_lines(app));
        lines.extend(queued_lines(app));
        if let Some(a) = attachment_line(app) {
            lines.push(a);
        }
        lines.push(prompt);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Wrap the prompt into display rows (honoring explicit newlines) and draw a
    // block cursor at its (row, col). The prompt glyph takes the first 2 columns;
    // continuation rows are indented to match.
    let text_w = (area.width as usize).saturating_sub(3).max(1);
    let (rows, (cursor_row, cursor_col)) = layout_input(&app.input, app.input_cursor, text_w);
    let white = Style::default().fg(self::text());
    let cursor_style = Style::default().fg(Color::Black).bg(blue());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    let lanes_block = lane_lines(app);
    let queued = queued_lines(app);
    let mut attach_offset = lanes_block.len() + queued.len();
    lines.extend(lanes_block);
    lines.extend(queued);
    if let Some(a) = attachment_line(app) {
        lines.push(a);
        attach_offset += 1;
    }
    for (k, row) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        if k == 0 {
            spans.push(Span::styled(
                "✦ ",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("  "));
        }
        if k == cursor_row {
            let rchars: Vec<char> = row.chars().collect();
            let col = cursor_col.min(rchars.len());
            let before: String = rchars[..col].iter().collect();
            spans.push(Span::styled(before, white));
            if col < rchars.len() {
                spans.push(Span::styled(rchars[col].to_string(), cursor_style));
                let after: String = rchars[col + 1..].iter().collect();
                spans.push(Span::styled(after, white));
            } else {
                spans.push(Span::styled("█", Style::default().fg(blue())));
            }
        } else {
            spans.push(Span::styled(row.clone(), white));
        }
        lines.push(Line::from(spans));
    }

    // Keep the cursor row on screen when the input is taller than the box.
    let visible = (area.height as usize).saturating_sub(2).max(1);
    let scroll = (cursor_row + attach_offset).saturating_sub(visible.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Live lane status — running lanes listed above the prompt (same quiet style as
/// the queue block) so "what's out working right now, since when" is always
/// visible without digging through the transcript. Finished lanes don't linger
/// here; their completion rows live in the transcript.

pub(crate) fn input_height(app: &App, width: u16) -> u16 {
    pub(crate) const MAX_ROWS: usize = 8;
    // One extra row for the "📎 N attachments" summary when files are queued.
    let attach: u16 = if app.attachments.is_empty() { 0 } else { 1 };
    // Queued-message preview rows (see queued_lines): header + up to 3 previews
    // + an overflow line when more are held.
    let qn = app.held_queue().len();
    let queued: u16 = if qn == 0 {
        0
    } else {
        (1 + qn.min(3) + usize::from(qn > 3)) as u16
    };
    // Live running-lane rows (see lane_lines), capped at 4.
    let lanes_h: u16 = app
        .state
        .as_ref()
        .map(|s| {
            s.lanes
                .iter()
                .filter(|l| l.status == LaneStatus::Running)
                .count()
                .min(4) as u16
        })
        .unwrap_or(0);
    let queued = queued + lanes_h;
    if app.input.is_empty() {
        return 3 + attach + queued;
    }
    let text_w = (width as usize).saturating_sub(3).max(1);
    let (rows, _) = layout_input(&app.input, app.input_cursor, text_w);
    (rows.len().clamp(1, MAX_ROWS) as u16) + 2 + attach + queued
}

/// Wrap `input` into display rows at `width` columns, honoring explicit `\n` as
/// hard breaks and soft-wrapping longer lines by character count. Returns the
/// rows plus the cursor's (row, col) so the caller can draw a block cursor.
pub(crate) fn layout_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let chars: Vec<char> = input.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    let mut cursor_rc = (0usize, 0usize);
    for (i, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            if cursor == i {
                cursor_rc = (rows.len(), col);
            }
            rows.push(std::mem::take(&mut cur));
            col = 0;
            continue;
        }
        if col == width {
            rows.push(std::mem::take(&mut cur));
            col = 0;
        }
        if cursor == i {
            cursor_rc = (rows.len(), col);
        }
        cur.push(*ch);
        col += 1;
    }
    if cursor >= chars.len() {
        cursor_rc = (rows.len(), col);
    }
    rows.push(cur);
    (rows, cursor_rc)
}

/// Render the compact inline login form: all fields on one panel, the focused
/// one highlighted. Editing is driven by `handle_login_key`.

pub(crate) fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let st = app.state.as_ref();
    let faint_style = Style::default().fg(faint());
    // A worktree reads as its project plus branch, not its generated folder.
    let origin = app.workspace_origin();
    let project = origin
        .as_ref()
        .map(|o| o.folder.clone())
        .unwrap_or_else(|| app.options.config.workspace.clone());
    let project = project
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let folder_name = match origin.and_then(|o| o.branch) {
        Some(branch) => format!("{project} ⎇ {branch}"),
        None => project,
    };
    let folder_name = folder_name.as_str();
    let model = if app.effective_model.1.is_empty() {
        "no model"
    } else {
        &app.effective_model.1
    };

    let mut left: Vec<Span<'static>> = vec![
        Span::styled(model.to_string(), Style::default().fg(soft()).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  ", faint_style),
        Span::styled(folder_name.to_string(), Style::default().fg(muted())),
    ];
    if st.is_some_and(|s| s.approval_mode == crate::harness::ApprovalMode::Manual) {
        left.push(Span::styled("  ·  ", faint_style));
        left.push(Span::styled("manual approval", Style::default().fg(warn())));
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if app.is_compacting() {
        right.push(Span::styled("compacting context", Style::default().fg(accent())));
    } else {
        let pct = st
            .filter(|s| s.context_window > 0 && s.last_prompt_tokens > 0)
            .map(|s| {
                ((s.last_prompt_tokens as f64 / s.context_window as f64) * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as usize
            })
            .unwrap_or(0);
        let color = if pct >= 90 {
            danger()
        } else if pct >= 75 {
            warn()
        } else {
            accent()
        };
        let filled = (pct * 8 + 50) / 100;
        right.push(Span::styled("context ", faint_style));
        right.push(Span::styled("━".repeat(filled), Style::default().fg(color)));
        right.push(Span::styled("━".repeat(8 - filled), Style::default().fg(border2())));
        right.push(Span::styled(format!(" {pct}%"), Style::default().fg(muted())));
    }

    let right_line = Line::from(right);
    let right_w = right_line.width() as u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w.saturating_add(1))])
        .split(area);
    frame.render_widget(Paragraph::new(Line::from(left)), cols[0]);
    frame.render_widget(
        Paragraph::new(right_line).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

/// SI-ish formatting for token counts: 1.2B, 91M, 425k, 512.
pub(crate) fn fmt_si(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.0}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Canonicalize the workspace path and abbreviate the home dir to `~`.

pub(crate) fn home_path(path: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty()
            && let Some(rest) = text.strip_prefix(home.as_ref())
        {
            return format!("~{rest}");
        }
    }
    text
}

