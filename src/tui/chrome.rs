use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme::*;

pub(crate) fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count > width {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

pub(crate) fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(soft()).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(faint())));
    }
    Line::from(spans)
}

pub(crate) fn screen_frame(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    subtitle: &str,
    hints: &[(&str, &str)],
) -> Rect {
    let x = area.x + 1;
    let width = area.width.saturating_sub(2);
    let mut head = vec![Span::styled(title.to_string(), Style::default().fg(text()).add_modifier(Modifier::BOLD))];
    if !subtitle.is_empty() {
        head.push(Span::styled(format!("  {subtitle}"), Style::default().fg(muted())));
    }
    frame.render_widget(Paragraph::new(Line::from(head)), Rect { x, y: area.y, width, height: 1 });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width as usize), Style::default().fg(faint())))),
        Rect { x, y: area.y + 1, width, height: 1 },
    );
    if area.height > 3 {
        frame.render_widget(
            Paragraph::new(hint_line(hints)),
            Rect { x, y: area.y + area.height - 1, width, height: 1 },
        );
    }
    Rect { x, y: area.y + 3, width, height: area.height.saturating_sub(5) }
}

pub(crate) fn section(label: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {label}"), Style::default().fg(faint()).add_modifier(Modifier::BOLD)))
}

pub(crate) fn note(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), Style::default().fg(faint())))
}

pub(crate) struct Row<'a> {
    pub(crate) selected: bool,
    pub(crate) dot: Option<Color>,
    pub(crate) title: &'a str,
    pub(crate) emphasis: bool,
    pub(crate) meta: &'a str,
    pub(crate) right: Option<(String, Color)>,
}

impl Row<'_> {
    pub(crate) fn line(&self, width: usize) -> Line<'static> {
        let right = self.right.clone();
        let right_w = right.as_ref().map_or(0, |(r, _)| r.chars().count() + 1);
        let meta_w = if self.meta.is_empty() { 0 } else { (self.meta.chars().count() + 1).min(width / 3) };
        let lead = if self.dot.is_some() { 3 } else { 2 };
        let title_w = width.saturating_sub(lead + meta_w + right_w + 1);
        let mut title_style = Style::default().fg(if self.selected || self.emphasis { text() } else { soft() });
        if self.selected || self.emphasis {
            title_style = title_style.add_modifier(Modifier::BOLD);
        }
        let mut spans = vec![Span::styled(if self.selected { "▍" } else { " " }, Style::default().fg(accent()))];
        match self.dot {
            Some(color) => spans.push(Span::styled("● ", Style::default().fg(color))),
            None => spans.push(Span::raw(" ")),
        }
        spans.push(Span::styled(pad(self.title, title_w), title_style));
        if meta_w > 0 {
            spans.push(Span::styled(format!(" {}", pad(self.meta, meta_w - 1)), Style::default().fg(faint())));
        }
        if let Some((r, color)) = right {
            spans.push(Span::styled(format!(" {r}"), Style::default().fg(color)));
        }
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
        let line = Line::from(spans);
        if self.selected { line.style(Style::default().bg(surface3())) } else { line }
    }
}

pub(crate) fn sub_line(text: &str, selected: bool, indent: usize, width: usize) -> Line<'static> {
    let body = pad(&format!("{}{text}", " ".repeat(indent)), width);
    let line = Line::from(Span::styled(body, Style::default().fg(faint())));
    if selected { line.style(Style::default().bg(surface3())) } else { line }
}

pub(crate) fn window<T>(items: &[T], selected: usize, visible: usize) -> (usize, usize) {
    let visible = visible.max(1);
    let start = selected.saturating_sub(visible.saturating_sub(1)).min(items.len().saturating_sub(visible));
    (start, (start + visible).min(items.len()))
}

pub(crate) fn age(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| compact_secs(chrono::Utc::now().timestamp() - t.timestamp()))
        .unwrap_or_default()
}

pub(crate) fn compact_secs(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        86400..604800 => format!("{}d", secs / 86400),
        _ => format!("{}w", secs / 604800),
    }
}

pub(crate) fn thin_bar(fraction: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((fraction.clamp(0.0, 1.0)) * width as f64).round() as usize;
    vec![
        Span::styled("━".repeat(filled), Style::default().fg(color)),
        Span::styled("━".repeat(width.saturating_sub(filled)), Style::default().fg(border2())),
    ]
}
