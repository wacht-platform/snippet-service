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
    use ratatui::widgets::{Block, Borders, Paragraph};
    let checkpoints = app
        .state
        .as_ref()
        .map(|s| s.checkpoints.clone())
        .unwrap_or_default();
    let is_rewind = app.screen == Screen::RewindCheckpointSelection;
    let title = if is_rewind {
        " Rewind current chat + files · choose a checkpoint "
    } else {
        " Fork a new chat · choose a checkpoint "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = inner.height.saturating_sub(3) as usize;
    let selected = app
        .checkpoint_selected_index
        .min(checkpoints.len().saturating_sub(1));
    let start = if selected >= visible && visible > 0 {
        selected + 1 - visible
    } else {
        0
    };
    let mut lines = Vec::new();
    for (index, checkpoint) in checkpoints
        .iter()
        .enumerate()
        .skip(start)
        .take(visible.max(1))
    {
        let is_selected = index == selected;
        let marker = if is_selected { "›" } else { " " };
        let short_id: String = checkpoint.id.chars().take(8).collect();
        let style = if is_selected {
            Style::default().fg(accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(text())
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled("● ", Style::default().fg(lane())),
            Span::styled(checkpoint.label.clone(), style),
            Span::styled(format!("  {short_id}"), Style::default().fg(faint())),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No checkpoints available.",
            muted(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    let footer = if is_rewind {
        "↑↓ select · Enter rewind · Esc cancel"
    } else {
        "↑↓ select · Enter fork · Esc cancel"
    };
    let footer_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, subtle()))),
        footer_area,
    );
}


pub(crate) fn render_lanes(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, Borders, Paragraph};

    let lanes = app.state.as_ref().map(|state| &state.lanes);
    let (running, completed, failed, total) = lanes
        .map(|items| {
            (
                items
                    .iter()
                    .filter(|item| item.status == LaneStatus::Running)
                    .count(),
                items
                    .iter()
                    .filter(|item| item.status == LaneStatus::Completed)
                    .count(),
                items
                    .iter()
                    .filter(|item| item.status == LaneStatus::Failed)
                    .count(),
                items.len(),
            )
        })
        .unwrap_or_default();

    // One title only — counts live here; no second "delegated work" banner.
    let title = if total == 0 {
        " Lanes ".to_string()
    } else {
        format!(" Lanes  ·  {total}  ·  {running} live  ·  {completed} ok  ·  {failed} fail ")
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .title(title);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(inner);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(1)])
        .split(chunks[0]);

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(faint()));
    let detail_title = if app.lanes_detail_expanded {
        " Detail "
    } else {
        " Detail  ·  Enter expands "
    };
    let detail_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(faint()))
        .title(detail_title);
    let list_inner = list_block.inner(panes[0]);
    let detail_inner = detail_block.inner(panes[1]);
    frame.render_widget(list_block, panes[0]);
    frame.render_widget(detail_block, panes[1]);

    // Left: › ● title only (status is the colored dot — no "done" word).
    let mut list_lines = Vec::new();
    if let Some(items) = lanes {
        for (index, item) in items.iter().enumerate() {
            let (glyph, color) = match item.status {
                LaneStatus::Running => ("●", lane()),
                LaneStatus::Completed => ("●", success()),
                LaneStatus::Failed => ("●", danger()),
                LaneStatus::Cancelled => ("●", muted()),
            };
            let selected = index == app.lanes_selected_index;
            let style = if selected {
                Style::default().fg(accent()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text())
            };
            let max_title = list_inner.width.saturating_sub(4) as usize;
            let mut title = item.title.clone();
            if max_title > 1 && title.chars().count() > max_title {
                title = title
                    .chars()
                    .take(max_title.saturating_sub(1))
                    .collect::<String>()
                    + "…";
            }
            list_lines.push(Line::from(vec![
                Span::styled(if selected { "›" } else { " " }, style),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(title, style),
            ]));
        }
    }
    if list_lines.is_empty() {
        list_lines.push(Line::from(Span::styled("No lanes yet.", muted())));
    }
    frame.render_widget(Paragraph::new(list_lines), list_inner);

    let mut detail_lines = Vec::new();
    let prose_w = detail_inner.width.saturating_sub(1) as usize;
    if let Some(item) = lanes.and_then(|items| items.get(app.lanes_selected_index)) {
        let (dot, color) = match item.status {
            LaneStatus::Running => ("●", lane()),
            LaneStatus::Completed => ("●", success()),
            LaneStatus::Failed => ("●", danger()),
            LaneStatus::Cancelled => ("●", muted()),
        };
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
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(handoff, prose_w));
                detail_lines.push(Line::from(""));
            }
            if !item.activity_log.is_empty() {
                detail_lines.push(Line::from(Span::styled(
                    "Activity",
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
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
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(summary, prose_w));
                detail_lines.push(Line::from(""));
            }
            if let Some(report) = item.report.as_deref().filter(|s| !s.trim().is_empty()) {
                detail_lines.push(Line::from(Span::styled(
                    "Report",
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                )));
                detail_lines.extend(markdown::render_prose(report, prose_w));
            }
        } else {
            detail_lines.push(Line::from(Span::styled(
                "Summary",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )));
            let preview: String = summary_body.chars().take(700).collect();
            let preview = if summary_body.chars().count() > 700 {
                format!("{preview}…")
            } else {
                preview
            };
            detail_lines.extend(markdown::render_prose(&preview, prose_w));
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(Span::styled(
                "Enter / Ctrl-O · full report",
                faint(),
            )));
        }
    } else {
        detail_lines.push(Line::from(Span::styled("↑↓ select a lane", muted())));
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

    let scroll_hint = if max_scroll > 0 {
        format!("  ·  PgUp/PgDn ({}/{})", scroll + 1, max_scroll + 1)
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("↑↓ lane   Enter expand   Esc back{scroll_hint}"),
            Style::default().fg(faint()),
        ))),
        chunks[1],
    );
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
    use ratatui::widgets::{Block, Borders};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    // Lane count only when something is actually running — "0 active lanes" on
    // the models page was pure noise.
    let active_lanes = app
        .state
        .as_ref()
        .map(|s| {
            s.lanes
                .iter()
                .filter(|l| l.status == LaneStatus::Running)
                .count()
        })
        .unwrap_or(0);
    let mut header_spans = vec![
        Span::styled(
            " snippet",
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  models", subtle()),
    ];
    if active_lanes > 0 {
        header_spans.push(Span::styled(
            format!(
                "  ·  {} active lane{}",
                active_lanes,
                if active_lanes == 1 { "" } else { "s" }
            ),
            Style::default().fg(lane()),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    let names = app.options.config.profile_names();
    let total = names.len();
    let active = app.options.config.active_setup.clone().unwrap_or_default();
    let delegate = app
        .options
        .config
        .delegate_setup
        .clone()
        .unwrap_or_default();
    let setups = app.options.config.setups.as_ref();
    let sel = app.profiles_selected_index.min(total); // index `total` == the Add row

    // Window over profile cards (3 lines each); the Add row always shows at the end.
    let list_h = (chunks[1].height as usize).saturating_sub(2);
    let visible = (list_h.saturating_sub(2) / 3).max(1);
    let focus = sel.min(total.saturating_sub(1));
    let start = if total > 0 && focus >= visible {
        focus + 1 - visible
    } else {
        0
    };
    let end = (start + visible).min(total);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {start} more"),
            Style::default().fg(faint()),
        )));
    }
    for i in start..end {
        let name = &names[i];
        let is_sel = i == sel;
        let is_active = *name == active;
        let mut head = vec![
            Span::styled(
                if is_sel { "▍ " } else { "  " },
                Style::default().fg(accent()),
            ),
            Span::styled(
                name.clone(),
                if is_sel {
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(self::text())
                        .add_modifier(Modifier::BOLD)
                },
            ),
        ];
        if is_active {
            head.push(Span::styled("   ● active", Style::default().fg(success())));
        }
        if !delegate.is_empty() && *name == delegate {
            head.push(Span::styled("   ⇣ delegate", Style::default().fg(lane())));
        }
        lines.push(Line::from(head));
        if let Some(cfg) = setups.and_then(|m| m.get(name)) {
            let model = if cfg.model.is_empty() {
                "(no model)".to_string()
            } else {
                cfg.model.clone()
            };
            lines.push(Line::from(Span::styled(
                format!("     {model} · {}", profile_status(cfg)),
                subtle(),
            )));
        }
        lines.push(Line::from(""));
    }
    if end < total {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more", total - end),
            Style::default().fg(faint()),
        )));
    }

    let add_sel = sel >= total;
    lines.push(Line::from(vec![
        Span::styled(
            if add_sel { "▍ " } else { "  " },
            Style::default().fg(accent()),
        ),
        Span::styled(
            "+ Add a model",
            if add_sel {
                Style::default().fg(accent()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(faint())
            },
        ),
    ]));

    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(faint()));
    frame.render_widget(Paragraph::new(lines).block(block), chunks[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ move  ·  ↵ this chat  ·  g global  ·  l delegate  ·  e edit  ·  a add  ·  d delete  ·  Esc",
            subtle(),
        ))),
        chunks[2],
    );

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
        let block = Block::default()
            .title(Span::styled(
                " model setup ",
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(login_lines(app, inner.width as usize)).wrap(Wrap { trim: false }),
            inner,
        );
    }
}


pub(crate) fn render_resume_selection(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    use ratatui::widgets::{Block, BorderType, Borders};

    let convs = match &app.conv_cache {
        Some(c) => c.clone(),
        None => app.list_conversations(),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(area);

    let header_text = vec![
        Span::styled(
            "✦ > snippet",
            Style::default().fg(lane()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  │  Select a session to resume",
            Style::default().fg(text()),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(header_text)), chunks[0]);

    let mut lines = Vec::new();
    if convs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No saved conversations found.",
            subtle(),
        )));
    } else {
        let total = convs.len();
        let selected_idx = app.resume_selected_index.min(total - 1);
        let visible = (chunks[1].height as usize).saturating_sub(4).max(1);
        let start = if selected_idx >= visible {
            selected_idx + 1 - visible
        } else {
            0
        };
        let end = (start + visible).min(total);
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("  ↑ {} more", start),
                Style::default().fg(faint()),
            )));
        }
        for (offset, (name, desc)) in convs[start..end].iter().enumerate() {
            let is_selected = start + offset == selected_idx;
            let line = if is_selected {
                Line::from(vec![
                    Span::styled("▶ 📁 ", Style::default().fg(accent())),
                    Span::styled(
                        format!("{:<36} ", name),
                        Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(desc.to_string(), subtle()),
                ])
            } else {
                Line::from(vec![
                    Span::styled("  📁 ", Style::default().fg(muted())),
                    Span::styled(format!("{:<36} ", name), Style::default().fg(text())),
                    Span::styled(desc.to_string(), Style::default().fg(faint())),
                ])
            };
            lines.push(line);
        }
        if end < total {
            lines.push(Line::from(Span::styled(
                format!("  ↓ {} more", total - end),
                Style::default().fg(faint()),
            )));
        }
    }

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(faint()));

    frame.render_widget(Paragraph::new(lines).block(list_block), chunks[1]);

    // Render Footer
    let footer_text = "↑/↓ scroll  ·  Enter resume  ·  r rename  ·  d delete  ·  Esc go back";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer_text, subtle()))),
        chunks[2],
    );
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

/// Selectable options for a question as `(value, label)`. Empty = free-text
/// (the answer is typed in the input box instead of picked).
pub(crate) fn q_options(question: &Value) -> Vec<(String, String)> {
    let kind = question
        .get("answer_kind")
        .and_then(|k| k.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("free_text");
    let ak = question.get("answer_kind");
    let label_or = |k: &str, fallback: &str| {
        ak.and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    match kind {
        "single_choice" => ak
            .and_then(|a| a.get("choices"))
            .and_then(Value::as_array)
            .map(|cs| {
                cs.iter()
                    .map(|c| {
                        let value = c
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let label = c
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| value.clone());
                        let value = if value.is_empty() {
                            label.clone()
                        } else {
                            value
                        };
                        (value, label)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "yes_no" => vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
        "confirm" => vec![
            ("confirm".into(), label_or("confirm_label", "Confirm")),
            ("cancel".into(), label_or("cancel_label", "Cancel")),
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
        app.q_sel = 0;
        app.q_answers.clear();
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
    let Some(question) = qs.get(app.q_index.min(qs.len().saturating_sub(1))) else {
        return Vec::new();
    };
    let inner = width.saturating_sub(4).max(10);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let bold = Style::default().fg(text()).add_modifier(Modifier::BOLD);
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
    let hints = if opts.is_empty() {
        lines.push(Line::from(Span::styled(
            "Type your answer in the box below",
            Style::default().fg(muted()),
        )));
        vec![keycap("↵", "send", accent()), keycap("esc", "cancel", soft())]
    } else {
        let sel = app.q_sel.min(opts.len() - 1);
        for (i, (_value, label)) in opts.iter().enumerate().take(8) {
            let focused = i == sel;
            let label: String = label.chars().take(inner.saturating_sub(2)).collect();
            let pad = inner.saturating_sub(label.chars().count() + 2);
            let row = if focused {
                Line::from(vec![
                    Span::styled("▸ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                    Span::styled(label, bold),
                    Span::raw(" ".repeat(pad)),
                ])
                .style(Style::default().bg(surface3()))
            } else {
                Line::from(vec![Span::raw("  "), Span::styled(label, Style::default().fg(soft()))])
            };
            lines.push(row);
        }
        vec![
            keycap("↑↓", "choose", accent()),
            keycap("↵", "select", accent()),
            keycap("esc", "cancel", soft()),
        ]
    };
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
        format!("Question · {} of {}", app.q_index + 1, qs.len())
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
                spans.push(Span::styled(format!(" [{agent}]"), Style::default().fg(accent())));
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

