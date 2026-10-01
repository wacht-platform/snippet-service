use super::*;

pub(super) struct Card {
    pub(super) glyph: &'static str,
    pub(super) tone: Color,
    pub(super) kind: String,
    pub(super) reference: Option<String>,
    pub(super) status: String,
    pub(super) title: Option<String>,
    pub(super) body: String,
    pub(super) footer: Option<String>,
}

const CARD_BODY_LINES: usize = 6;

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn field(text: &str, name: &str) -> String {
    let prefix = format!("{name}:");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn last_body(text: &str, close: &str) -> String {
    let end = text.rfind(close).unwrap_or(text.len());
    match text[..end].rfind("\nbody: ") {
        Some(start) => text[start + "\nbody: ".len()..end].trim().to_string(),
        None => String::new(),
    }
}

fn task_scope(text: &str) -> String {
    let inline = field(text, "scope");
    if !inline.is_empty() {
        return inline;
    }
    let Some(start) = text.find("\nscope:\n") else {
        return String::new();
    };
    let rest = &text[start + "\nscope:\n".len()..];
    let cut = rest
        .find("\n\nBegin now.")
        .or_else(|| rest.find("[/mission_control_task]"))
        .unwrap_or(rest.len());
    rest[..cut].trim().to_string()
}

fn thread_ref(id: &str) -> String {
    match id.split_once(':') {
        Some((kind, rest)) if !kind.is_empty() => format!("{kind} {}", short(rest)),
        _ => short(id),
    }
}

pub(super) fn envelope_card(text: &str) -> Option<Card> {
    let t = text.trim();
    if t.contains("[mission_task_report]") {
        let status = field(t, "status");
        let (glyph, tone, label) = match status.as_str() {
            "failed" | "cancelled" => ("✗", danger(), "Failed"),
            "blocked" => ("!", danger(), "Blocked"),
            "in_progress" | "working" => ("◆", accent(), "Working"),
            _ => ("✓", success(), "Done"),
        };
        let id = field(t, "task_id");
        return Some(Card {
            glyph,
            tone,
            kind: "Task report".into(),
            reference: (!id.is_empty()).then(|| short(&id)),
            status: label.into(),
            title: Some(field(t, "title")).filter(|s| !s.is_empty()),
            body: field(t, "summary"),
            footer: None,
        });
    }
    if t.contains("[mission_control_task]") {
        let id = field(t, "task_id");
        let title = field(t, "title");
        return Some(Card {
            glyph: "◆",
            tone: accent(),
            kind: "Task from Mission Control".into(),
            reference: (!id.is_empty()).then(|| short(&id)),
            status: "New task".into(),
            title: Some(if title.is_empty() { "Task".into() } else { title }),
            body: task_scope(t),
            footer: None,
        });
    }
    if t.contains("[direct_message]") {
        let from = field(t, "from");
        let who = if field(t, "from_kind") == "human" || from == "local" || from.is_empty() {
            "you".to_string()
        } else {
            from
        };
        return Some(Card {
            glyph: "✉",
            tone: accent(),
            kind: format!("Message from {who}"),
            reference: None,
            status: "Message".into(),
            title: None,
            body: last_body(t, "[/direct_message]"),
            footer: None,
        });
    }
    if t.contains("[coordination_board_message]") {
        let from = field(t, "from_id");
        let thread = field(t, "thread_id");
        let task = field(t, "task_title");
        return Some(Card {
            glyph: "#",
            tone: muted(),
            kind: format!("Board · {}", if from.is_empty() { "someone" } else { &from }),
            reference: (!thread.is_empty()).then(|| thread_ref(&thread)),
            status: "Posted".into(),
            title: Some(task).filter(|s| !s.is_empty()),
            body: last_body(t, "[/coordination_board_message]"),
            footer: None,
        });
    }
    None
}

pub(super) fn event_card(event: &HarnessEvent) -> Option<Card> {
    match event {
        HarnessEvent::UserInput { text } | HarnessEvent::Steer { text } => envelope_card(text),
        HarnessEvent::AgentMessage {
            agent_id,
            body,
            outbound,
        } => Some(Card {
            glyph: if *outbound { "→" } else { "←" },
            tone: if *outbound { muted() } else { accent() },
            kind: if *outbound {
                format!("Message to {agent_id}")
            } else {
                format!("Reply from {agent_id}")
            },
            reference: None,
            status: if *outbound { "Sent" } else { "Reply" }.into(),
            title: None,
            body: body.clone(),
            footer: None,
        }),
        HarnessEvent::UserQuestion { questions } => {
            let items: Vec<String> = questions
                .get("questions")
                .and_then(Value::as_array)
                .map(|qs| {
                    qs.iter()
                        .filter_map(|q| q.get("text").and_then(Value::as_str))
                        .map(|t| format!("- {t}"))
                        .collect()
                })
                .unwrap_or_default();
            Some(Card {
                glyph: "?",
                tone: warn(),
                kind: "Asked you".into(),
                reference: None,
                status: if items.len() > 1 { format!("{} questions", items.len()) } else { "Question".into() },
                title: None,
                body: items.join("\n"),
                footer: None,
            })
        }
        HarnessEvent::TaskDispatched {
            task_id,
            title,
            session_id,
            by,
        } => Some(Card {
            glyph: "→",
            tone: muted(),
            kind: format!("Dispatched by {by}"),
            reference: Some(short(task_id)),
            status: "Dispatched".into(),
            title: Some(title.clone()),
            body: String::new(),
            footer: Some(format!("to {session_id}")),
        }),
        _ => None,
    }
}

pub(super) fn card_lines(card: &Card, width: usize) -> Vec<Line<'static>> {
    let frame = Style::default().fg(border2());
    let w = width.max(24);
    let inner = w - 4;
    let mut out = Vec::new();

    let status_w = card.status.chars().count();
    let mut meta = match &card.reference {
        Some(r) => format!("{} · {r}", card.kind),
        None => card.kind.clone(),
    };
    let meta_budget = w.saturating_sub(status_w + 11);
    if meta.chars().count() > meta_budget {
        meta = meta.chars().take(meta_budget.saturating_sub(1)).collect::<String>() + "…";
    }
    let used = meta.chars().count() + status_w + 10;
    out.push(Line::from(vec![
        Span::styled("╭─ ", frame),
        Span::styled(format!("{} ", card.glyph), Style::default().fg(card.tone)),
        Span::styled(meta, Style::default().fg(muted())),
        Span::styled(format!(" {}", "─".repeat(w.saturating_sub(used).max(1))), frame),
        Span::styled(format!(" {} ", card.status), Style::default().fg(card.tone)),
        Span::styled("─╮", frame),
    ]));

    let row = |content: Vec<Span<'static>>, len: usize| {
        let mut spans = vec![Span::styled("│ ", frame)];
        spans.extend(content);
        spans.push(Span::raw(" ".repeat(inner.saturating_sub(len))));
        spans.push(Span::styled(" │", frame));
        Line::from(spans)
    };

    if let Some(title) = &card.title {
        for seg in wrap_one(title, inner).into_iter().take(2) {
            let len = seg.chars().count();
            let style = Style::default().fg(text()).add_modifier(Modifier::BOLD);
            out.push(row(vec![Span::styled(seg, style)], len));
        }
    }

    let body = card.body.trim();
    if !body.is_empty() {
        let rendered = render_prose(body, inner);
        let total = rendered.len();
        let shown = if total > CARD_BODY_LINES + 1 { CARD_BODY_LINES } else { total };
        for mut line in rendered.into_iter().take(shown) {
            for span in &mut line.spans {
                if span.style.fg.is_none() || span.style.fg == Some(text()) {
                    span.style = span.style.fg(soft());
                }
            }
            let len: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            out.push(row(line.spans, len));
        }
        if shown < total {
            let more = format!("… {} more lines", total - shown);
            let len = more.chars().count();
            out.push(row(vec![Span::styled(more, Style::default().fg(faint()))], len));
        }
    }

    if let Some(footer) = &card.footer {
        for seg in wrap_one(footer, inner) {
            let len = seg.chars().count();
            out.push(row(vec![Span::styled(seg, Style::default().fg(muted()))], len));
        }
    }

    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(w - 2)),
        frame,
    )));
    out
}
