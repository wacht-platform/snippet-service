use super::*;
use super::markdown::*;
use super::theme::*;

/// The empty-state splash: a shimmering wordmark, a waving bar row, and a hint —
/// animated by the frame counter so the idle screen feels alive.
pub(super) fn empty_state_lines(frame_n: usize, width: usize) -> Vec<Line<'static>> {
    let f = frame_n;

    // The wordmark with a highlight that sweeps across it.
    let word: Vec<char> = "snipett".chars().collect();
    let sweep = (f / 2) % (word.len() + 6);
    let mut title = Vec::new();
    for (i, c) in word.iter().enumerate() {
        let style = if i == sweep {
            Style::default().fg(accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self::text()).add_modifier(Modifier::BOLD)
        };
        title.push(Span::styled(c.to_string(), style));
    }

    // A small equalizer wave that ripples left→right.
    const BARS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    let mut wave = Vec::new();
    for i in 0..11usize {
        let phase = (f / 2 + i * 2) % 14;
        let h = if phase < 7 { phase } else { 14 - phase }; // triangle 0..6..0
        wave.push(Span::styled(BARS[h.min(7)].to_string(), Style::default().fg(accent())));
        wave.push(Span::raw(" "));
    }

    vec![
        center_line(Line::from(title), width),
        Line::from(""),
        center_line(Line::from(wave), width),
        Line::from(""),
        Line::from(""),
        center_line(
            Line::from(Span::styled(
                "type a task and press Enter   ·   / for commands",
                subtle(),
            )),
            width,
        ),
    ]
}

pub(super) fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(state) = &app.state else {
        // No session yet — keep the transcript empty (the status bar carries the
        // hint). Only the login form shows here, and only when it's open.
        return login_lines(app, width);
    };

    let mut lines = Vec::new();
    let has_user_events = state
        .events
        .iter()
        .any(|event| matches!(event, HarnessEvent::UserInput { .. }));

    // Blank line *between* blocks, but a run of consecutive tool rows (a call and
    // its result, then the next call…) packs tightly with no gaps — so a burst of
    // reads/greps collapses instead of spreading down the screen. Prose still gets
    // breathing room before and after a tool run.
    let mut first = true;
    let mut prev_tool_row = false;

    if !has_user_events && !state.user_request.is_empty() {
        lines.extend(user_lines(&state.user_request, width));
        first = false;
    }

    // After a compaction, clear the screen above it: render only from the last
    // compaction boundary down (a "✦ context compacted" divider, then any newer
    // activity). The full history still lives in `state` on disk — this only hides
    // the compacted-away messages from the view.
    let compact_start = state
        .events
        .iter()
        .rposition(|e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "history_compacted"))
        .unwrap_or(0);
    let mut events = state.events[compact_start..].iter().peekable();
    while let Some(event) = events.next() {
        // Collapse a run of consecutive model errors (transient retries) into a
        // single line with a count, so a retry storm doesn't flood the screen.
        if let HarnessEvent::ModelError { message } = event {
            let mut last = message.clone();
            let mut count = 1usize;
            while let Some(HarnessEvent::ModelError { message: next }) = events.peek() {
                last = next.clone();
                count += 1;
                events.next();
            }
            if count > 1 {
                last = format!("{last}  (×{count})");
            }
            if !first {
                lines.push(Line::from(""));
            }
            lines.extend(marker_block("✗ ", "", danger(), &last, width));
            prev_tool_row = false;
            first = false;
            continue;
        }

        // Tool call: render `● Verb  arg` and, when the next event is a one-line
        // result for it, merge that summary onto the same row, right-aligned.
        if let HarnessEvent::ToolCall { tool_name, arguments } = event {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                // Drop the paired hidden result too, so no orphan row renders.
                if let Some(HarnessEvent::ToolResult { tool_name: rn, .. }) = events.peek() {
                    if HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                        events.next();
                    }
                }
                continue;
            }
            let mut call_lines = tool_call_lines(tool_name, arguments, width);
            // The result is pushed right after the call, so a call with NO following
            // result is the in-flight one (persisted just before execution). When the
            // result is present and one-line, merge it onto the row; when it's still
            // running, show a live spinner so a slow tool isn't a black box.
            let result_follows = matches!(events.peek(), Some(HarnessEvent::ToolResult { .. }));
            if result_follows {
                if call_lines.len() == 1 {
                    if let Some(HarnessEvent::ToolResult { tool_name: rn, result }) = events.peek() {
                        if !HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                            if let Some(summary) = tool_result_oneliner(rn, result) {
                                let pad = width
                                    .saturating_sub(call_lines[0].width() + summary.chars().count());
                                if pad >= 2 {
                                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                                    call_lines[0]
                                        .spans
                                        .push(Span::styled(summary, Style::default().fg(muted())));
                                    events.next();
                                }
                            }
                        }
                    }
                }
            } else if state.status == HarnessStatus::Running {
                let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
                let label = format!("{spinner} running");
                let style = Style::default().fg(accent());
                if call_lines.len() == 1
                    && width > call_lines[0].width() + label.chars().count() + 2
                {
                    let pad = width - call_lines[0].width() - label.chars().count();
                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                    call_lines[0].spans.push(Span::styled(label, style));
                } else {
                    call_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, style),
                    ]));
                }
            }
            if !first && !prev_tool_row {
                lines.push(Line::from(""));
            }
            lines.extend(call_lines);
            prev_tool_row = true;
            first = false;
            continue;
        }

        let rendered = event_lines(event, width);
        if rendered.is_empty() {
            continue;
        }
        let is_tool_row = matches!(
            event,
            HarnessEvent::ToolCall { .. }
                | HarnessEvent::ToolResult { .. }
                | HarnessEvent::InvalidToolCall { .. }
        );
        if !first && !(prev_tool_row && is_tool_row) {
            lines.push(Line::from(""));
        }
        lines.extend(rendered);
        prev_tool_row = is_tool_row;
        first = false;
    }

    // Reasoning/thinking the model returned, shown dimmed and distinct from the
    // answer. DEBUG: rendered whenever present (not just while working) and never
    // cleared, while experimenting with the thinking display.
    let thinking = crate::llm::StreamBuffer::snapshot_thinking(&app.stream);
    let thinking = thinking.trim_end();
    if !thinking.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        for seg in wrap_one(thinking, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(seg, Style::default().fg(muted()))));
        }
    }

    // Live "working…" feedback at the tail while the agent is processing (or a lane is).
    let working = state.status == HarnessStatus::Running
        || state.lanes.iter().any(|lane| lane.status == LaneStatus::Running);
    if working && app.agent_alive() {
        // Text the model is streaming this turn, shown live until it commits to a
        // durable AssistantText event (then refresh_state clears the buffer).
        let live = crate::llm::StreamBuffer::snapshot(&app.stream);
        let live = live.trim_end();
        if !live.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(render_prose(live, width));
        }
        // Compaction has its own animated bar directly above the input box
        // (render_compaction_bar) — suppress the generic "working…" line then so
        // only the compaction animation shows.
        if !app.is_compacting() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default().fg(accent()).add_modifier(Modifier::BOLD),
                ),
                Span::styled("working…", subtle()),
            ]));
        }
    }
    // Append inline login Q&A if active
    lines.extend(login_lines(app, width));
    lines
}

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Map one event to a block of styled, wrapped lines. Empty = hidden.
pub(super) fn event_lines(event: &HarnessEvent, width: usize) -> Vec<Line<'static>> {
    match event {
        HarnessEvent::UserInput { text } => user_lines(text, width),
        HarnessEvent::Steer { text } => {
            marker_block("↳ ", "steer  ", accent(), &strip_attachment_markers(text), width)
        }
        HarnessEvent::AssistantText { text } => render_prose(text, width),
        HarnessEvent::Note { entry } => marker_block("✎ ", "note  ", muted(), entry, width),
        HarnessEvent::SystemDecision { step, reasoning } => {
            if step == "history_compaction_pass" {
                // Keep the live banner only during the turn; the durable
                // transcript entry comes from `history_compacted` below.
                Vec::new()
            } else if step == "history_compaction_skipped" {
                let _ = reasoning; // detail goes to the debug log, not the transcript
                Vec::new()
            } else if step == "history_compacted" {
                // A clean boundary; everything above it is collapsed by transcript_lines.
                // The verbose token detail lives in the debug log, not here.
                compaction_divider(width)
            } else {
                marker_block(
                    "⚙ ",
                    "",
                    warn(),
                    &format!("{step} — {reasoning}"),
                    width,
                )
            }
        }
        HarnessEvent::ModelError { message } => {
            marker_block("✗ ", "", danger(), message, width)
        }
        HarnessEvent::UserQuestion { questions } => {
            let text = question_text(questions).unwrap_or_else(|| "(question)".to_string());
            marker_block("? ", "", warn(), &text, width)
        }
        HarnessEvent::ApprovalRequest { .. } => {
            // While pending it's shown in the approval card above the input; the
            // outcome is logged via the `approval_resolved` decision. No transcript
            // line for the bare request.
            Vec::new()
        }
        // Subject only — lane ids are internal plumbing, not for the transcript.
        HarnessEvent::LaneSpawned { id: _, title } => marker_block(
            "→ ",
            "",
            lane(),
            &format!("delegated: {title}"),
            width,
        ),
        HarnessEvent::LaneCompleted {
            id,
            title,
            status,
            summary,
        } => lane_completed_lines(id, title, *status, summary.as_deref(), width),
        HarnessEvent::ToolCall {
            tool_name,
            arguments,
        } => {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                return Vec::new();
            }
            tool_call_lines(tool_name, arguments, width)
        }
        HarnessEvent::ToolResult { tool_name, result } => {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                return Vec::new();
            }
            tool_result_lines(tool_name, result, width)
        }
        HarnessEvent::InvalidToolCall { tool_name, error } => result_block(
            vec![(format!("✗ {tool_name}: {error}"), Style::default().fg(danger()))],
            width,
        ),
    }
}

/// Hide the app's `[attached image — …]` / `[attached file — …]` markers from the
/// rendered transcript — they're instructions for the agent, never shown to users.
pub(super) fn strip_attachment_markers(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !((t.starts_with("[attached image —") || t.starts_with("[attached file —")) && t.ends_with(']'))
        })
        .collect();
    kept.join("\n").trim_end().to_string()
}

pub(super) fn user_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let cleaned = strip_attachment_markers(text);
    let prefix = Span::styled(
        "› ",
        Style::default().fg(blue()).add_modifier(Modifier::BOLD),
    );
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&cleaned, width.saturating_sub(2)).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                prefix.clone(),
                Span::styled(seg, Style::default().fg(self::text())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(self::text())),
            ]));
        }
    }
    // Surface any attachments the message carried — kept visible even when it also
    // had text, so a sent attachment doesn't vanish from the transcript (dimmed).
    let (imgs, files) = count_attachments(text);
    if imgs + files > 0 {
        let leading = if lines.is_empty() {
            prefix.clone()
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![
            leading,
            Span::styled(attachment_summary(imgs, files), Style::default().fg(muted())),
        ]));
    } else if lines.is_empty() {
        lines.push(Line::from(vec![prefix]));
    }
    lines
}

/// Count `[attached image — …]` / `[attached file — …]` markers by kind.
fn count_attachments(text: &str) -> (usize, usize) {
    let (mut imgs, mut files) = (0usize, 0usize);
    for line in text.lines() {
        let t = line.trim_start();
        if !t.ends_with(']') {
            continue;
        }
        if t.starts_with("[attached image —") {
            imgs += 1;
        } else if t.starts_with("[attached file —") {
            files += 1;
        }
    }
    (imgs, files)
}

/// "📎 2 images · 1 file" from the per-kind counts.
fn attachment_summary(imgs: usize, files: usize) -> String {
    let mut parts = Vec::new();
    if imgs > 0 {
        parts.push(format!("{imgs} image{}", if imgs == 1 { "" } else { "s" }));
    }
    if files > 0 {
        parts.push(format!("{files} file{}", if files == 1 { "" } else { "s" }));
    }
    format!("📎 {}", parts.join(" · "))
}

/// A leading glyph + optional label, then wrapped body text in one color.
/// A clean, centered boundary marking where history was compacted.
pub(super) fn compaction_divider(width: usize) -> Vec<Line<'static>> {
    let label = " ✦ context compacted ";
    let side = width.saturating_sub(label.chars().count()) / 2;
    let dash = "─".repeat(side.min(36));
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(dash.clone(), Style::default().fg(faint())),
            Span::styled(label.to_string(), Style::default().fg(muted())),
            Span::styled(dash, Style::default().fg(faint())),
        ]),
        Line::from(""),
    ]
}

pub(super) fn marker_block(
    glyph: &str,
    label: &str,
    color: Color,
    text: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let glyph_w = glyph.chars().count() + label.chars().count();
    let body_style = Style::default().fg(color);
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(text, width.saturating_sub(glyph_w))
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            let mut spans = vec![Span::styled(
                glyph.to_string(),
                body_style.add_modifier(Modifier::BOLD),
            )];
            if !label.is_empty() {
                spans.push(Span::styled(
                    label.to_string(),
                    body_style.add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(seg, body_style));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(glyph_w)),
                Span::styled(seg, body_style),
            ]));
        }
    }
    lines
}

pub(super) fn lane_completed_lines(
    id: &str,
    title: &str,
    status: LaneStatus,
    summary: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let (tag, color) = match status {
        LaneStatus::Completed => ("done", success()),
        LaneStatus::Failed => ("failed", danger()),
        LaneStatus::Running => ("running", lane()),
    };
    // Subject only — the id is internal plumbing (kept in the signature for
    // callers that still have it, unused for display).
    let _ = id;
    let mut lines = vec![Line::from(vec![
        Span::styled("◆ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{title} "),
            Style::default().fg(self::text()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{tag}]"), Style::default().fg(color)),
    ])];
    if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
        lines.extend(result_block(
            vec![(summary.to_string(), subtle())],
            width,
        ));
    }
    lines
}

pub(super) fn tool_call_lines(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let mut lines = tool_call_head_lines(tool_name, arguments, width);
    lines.extend(tool_call_preview(tool_name, arguments, width));
    lines
}

/// Render just the call header row(s): `● Verb  arg`, the verb in bold body text
/// and the argument muted, wrapping the argument under a hanging indent.
pub(super) fn tool_call_head_lines(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let (verb, arg) = tool_call_parts(tool_name, arguments);
    let head = format!("{verb}  ");
    let indent = 2 + head.chars().count();
    let arg_budget = width.saturating_sub(indent).max(8);

    if arg.trim().is_empty() {
        return vec![Line::from(vec![
            Span::styled("● ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
            Span::styled(verb, Style::default().fg(self::text()).add_modifier(Modifier::BOLD)),
        ])];
    }

    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&arg, arg_budget).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                Span::styled(verb.clone(), Style::default().fg(self::text()).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(seg, Style::default().fg(muted())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(seg, Style::default().fg(muted())),
            ]));
        }
    }
    lines
}

/// A preview of what the call will do — content for writes, a +/- diff for edits.
pub(super) fn tool_call_preview(tool_name: &str, arguments: &Value, width: usize) -> Vec<Line<'static>> {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    let green = Style::default().fg(success());
    let red = Style::default().fg(danger());
    const MAX: usize = 8;

    let mut items: Vec<(String, Style)> = Vec::new();
    match tool_name {
        "write_file" => {
            let content = arg("content");
            let total = content.lines().count();
            for line in content.lines().take(MAX) {
                items.push((format!("+ {line}"), green));
            }
            if total > MAX {
                items.push((format!("… +{} more lines", total - MAX), subtle()));
            }
        }
        "edit_file" => {
            let old = arg("old_string");
            let new = arg("new_string");
            let old_total = old.lines().count();
            for line in old.lines().take(MAX) {
                items.push((format!("- {line}"), red));
            }
            if old_total > MAX {
                items.push((format!("  … +{} more", old_total - MAX), subtle()));
            }
            let new_total = new.lines().count();
            for line in new.lines().take(MAX) {
                items.push((format!("+ {line}"), green));
            }
            if new_total > MAX {
                items.push((format!("  … +{} more", new_total - MAX), subtle()));
            }
        }
        _ => return Vec::new(),
    }
    result_block_verbatim(items, width)
}

/// A tool call as (verb, argument) — e.g. ("Read", "src/auth.rs") — so the verb
/// and its target can be styled distinctly instead of a single `Read(path)` blob.
pub(super) fn tool_call_parts(tool_name: &str, arguments: &Value) -> (String, String) {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match tool_name {
        "read_file" => ("Read".into(), arg("path")),
        "write_file" => ("Write".into(), arg("path")),
        "edit_file" => ("Edit".into(), arg("path")),
        "replace_file_content" => (
            "Replace".into(),
            format!(
                "{} · lines {}-{}",
                arg("path"),
                arguments.get("start_line").and_then(Value::as_u64).unwrap_or(0),
                arguments.get("end_line").and_then(Value::as_u64).unwrap_or(0)
            ),
        ),
        "list_files" => (
            "List".into(),
            arguments.get("path").and_then(Value::as_str).unwrap_or(".").to_string(),
        ),
        "search_content" => ("Grep".into(), arg("query")),
        "view_outline" => ("Outline".into(), arg("path")),
        "web_search" => ("Web".into(), arg("query")),
        "web_read" => ("Fetch".into(), arg("url")),
        "bash" => {
            // Commands can be long or multi-line; show a compact single line (first
            // line, whitespace-collapsed, capped) with an ellipsis when elided.
            let cmd = arg("command");
            let first = cmd.lines().next().unwrap_or("").trim();
            let compact = first.split_whitespace().collect::<Vec<_>>().join(" ");
            let capped: String = compact.chars().take(110).collect();
            let elided = cmd.lines().count() > 1 || capped.chars().count() < compact.chars().count();
            ("Bash".into(), if elided { format!("{capped} …") } else { capped })
        }
        _ => (
            tool_name.to_string(),
            serde_json::to_string(arguments).unwrap_or_default(),
        ),
    }
}

/// A short, single-line result summary for the tools whose output is just a count
/// or status — so it can be merged onto the call row (`● Read  path     142 lines`).
/// Returns None for errors and for tools with multi-line output (bash, list), which
/// render as their own block below the call.
pub(super) fn tool_result_oneliner(tool_name: &str, result: &Value) -> Option<String> {
    if result.get("status").and_then(Value::as_str) == Some("error") {
        return None;
    }
    let data = result.get("data").unwrap_or(result);
    let s = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("");
    let count = |key: &str| data.get(key).and_then(Value::as_u64).unwrap_or(0);
    let line = match tool_name {
        "read_file" => {
            let n = s("content").lines().count();
            format!("{n} {}", if n == 1 { "line" } else { "lines" })
        }
        "write_file" => "written".to_string(),
        "edit_file" => "updated".to_string(),
        "replace_file_content" => "replaced".to_string(),
        "search_content" => format!("{} matches", count("count")),
        "web_search" => format!("{} results", count("count")),
        "web_read" => format!("{} chars", s("text").chars().count()),
        "view_outline" => {
            if data.get("is_directory").and_then(Value::as_bool).unwrap_or(false) {
                let n = data.get("entries").and_then(Value::as_array).map(|e| e.len()).unwrap_or(0);
                format!("{n} entries")
            } else {
                let n = data.get("outline").and_then(Value::as_array).map(|o| o.len()).unwrap_or(0);
                format!("{n} decls")
            }
        }
        _ => return None,
    };
    Some(line)
}

pub(super) fn tool_result_lines(tool_name: &str, result: &Value, width: usize) -> Vec<Line<'static>> {
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let data = result.get("data").unwrap_or(result);

    if status == "error" {
        let message = result
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("failed");
        return result_block(
            vec![(format!("✗ {message}"), Style::default().fg(danger()))],
            width,
        );
    }

    let str_field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("");
    let items: Vec<(String, Style)> = match tool_name {
        "read_file" => {
            let lines = str_field("content").lines().count();
            vec![(format!("Read {lines} lines"), subtle())]
        }
        "write_file" => vec![(format!("Wrote {}", str_field("path")), subtle())],
        "edit_file" => vec![(format!("Updated {}", str_field("path")), subtle())],
        "replace_file_content" => vec![(format!("Replaced contiguous block in {}", str_field("path")), subtle())],
        "list_files" => {
            let entries = data.get("entries").and_then(Value::as_array);
            let count = entries.map(|e| e.len()).unwrap_or(0);
            let names = entries
                .map(|e| {
                    e.iter()
                        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                        .take(12)
                        .collect::<Vec<_>>()
                        .join("  ")
                })
                .unwrap_or_default();
            vec![
                (format!("{count} entries"), subtle()),
                (names, subtle()),
            ]
        }
        "search_content" => {
            let count = data.get("count").and_then(Value::as_u64).unwrap_or(0);
            vec![(format!("Found {count} content matches"), subtle())]
        }
        "web_search" => {
            let count = data.get("count").and_then(Value::as_u64).unwrap_or(0);
            vec![(format!("{count} web results"), subtle())]
        }
        "web_read" => {
            let chars = data.get("text").and_then(Value::as_str).map(|t| t.chars().count()).unwrap_or(0);
            vec![(format!("Read {chars} chars"), subtle())]
        }
        "view_outline" => {
            if data.get("is_directory").and_then(Value::as_bool).unwrap_or(false) {
                let count = data.get("entries").and_then(Value::as_array).map(|e| e.len()).unwrap_or(0);
                vec![(format!("Directory — {count} entries"), subtle())]
            } else {
                let outline = data.get("outline").and_then(Value::as_array);
                let count = outline.map(|o| o.len()).unwrap_or(0);
                vec![(format!("Outline has {count} code declarations"), subtle())]
            }
        }
        "bash" => bash_result_items(data),
        _ => vec![(status.to_string(), subtle())],
    };

    let items: Vec<(String, Style)> = items.into_iter().filter(|(t, _)| !t.is_empty()).collect();
    // Bash output is rendered verbatim so leading whitespace / column alignment is
    // preserved (word-wrap would strip indentation); other results word-wrap.
    if tool_name == "bash" {
        result_block_verbatim(items, width)
    } else {
        result_block(items, width)
    }
}

pub(super) fn bash_result_items(data: &Value) -> Vec<(String, Style)> {
    // Keep bash output minimal: one summary line + a tiny preview, never the full
    // dump (the model still has the complete output; the UI just shouldn't flood).
    const BASH_PREVIEW: usize = 3;
    let success = data.get("success").and_then(Value::as_bool).unwrap_or(false);
    let exit = data
        .get("exit_code")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("");

    let output: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let total = output.len();

    // Single concise summary line (red on failure).
    let noun = if total == 1 { "line" } else { "lines" };
    let summary = match (success, total) {
        (true, 0) => "ran · no output".to_string(),
        (true, n) => format!("ran · {n} {noun}"),
        (false, 0) => format!("exited {exit} · no output"),
        (false, n) => format!("exited {exit} · {n} {noun}"),
    };
    let summary_style = if success { subtle() } else { Style::default().fg(danger()) };
    let mut items = vec![(summary, summary_style)];

    // Glimpse: the first few lines only.
    let shown = total.min(BASH_PREVIEW);
    for line in &output[..shown] {
        items.push((line.to_string(), subtle()));
    }
    if total > shown {
        items.push((
            format!("… +{} more lines", total - shown),
            subtle().add_modifier(Modifier::ITALIC),
        ));
    }
    items
}

/// Render result/output logical lines under a `⎿` gutter, wrapped to width.
pub(super) fn result_block(items: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    result_block_inner(items, width, false)
}

/// Like `result_block` but preserves each line verbatim (indentation and runs of
/// spaces) instead of word-wrapping — used for code/diff previews where leading
/// whitespace is meaningful.
pub(super) fn result_block_verbatim(items: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    result_block_inner(items, width, true)
}

pub(super) fn result_block_inner(
    items: Vec<(String, Style)>,
    width: usize,
    verbatim: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut first = true;
    for (text, style) in items {
        let segs = if verbatim {
            wrap_code_line(&text, width.saturating_sub(4))
        } else {
            wrap_one(&text, width.saturating_sub(4))
        };
        for seg in segs {
            let prefix = if first { "  ⎿ " } else { "    " };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), subtle()),
                Span::styled(seg, style),
            ]));
            first = false;
        }
    }
    lines
}
