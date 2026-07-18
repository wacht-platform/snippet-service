use super::*;
use super::markdown::*;
use super::theme::*;

/// The empty-state welcome: calm, left-aligned context + a compact command
/// legend. No animation — a quiet starting page in the Terminal Ink palette.
pub(super) fn empty_state_lines(cwd: &str, model: &str, width: usize) -> Vec<Line<'static>> {
    let _ = width;
    let pad = "   ";
    let dim = Style::default().fg(faint());
    let label = Style::default().fg(muted());
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(format!("{pad}▍ "), Style::default().fg(accent())),
        Span::styled("snippet", Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(Span::styled(
        format!("{pad}  a coding agent in your terminal"),
        dim,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{pad}{k:<10}"), dim),
            Span::styled(v, label),
        ])
    };
    lines.push(row("workspace", cwd.to_string()));
    lines.push(row("model", if model.is_empty() { "—".into() } else { model.to_string() }));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(format!("{pad}"), dim),
        Span::styled("Type a task and press ", label),
        Span::styled("⏎", Style::default().fg(accent())),
        Span::styled(" to begin.", label),
    ]));
    lines.push(Line::from(""));

    let cmd = |c: &str| Span::styled(c.to_string(), Style::default().fg(accent()));
    let sep = || Span::styled("   ·   ", dim);
    lines.push(Line::from(vec![
        Span::styled(pad.to_string(), dim),
        cmd("/model"), sep(),
        cmd("/theme"), sep(),
        cmd("/goal"), sep(),
        cmd("/resume"), sep(),
        cmd("/help"),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("{pad}"), dim),
        Span::styled("⏎ send   ·   ⇧⏎ newline   ·   / for commands", dim),
    ]));
    lines
}

/// Arm the speaker tag when the turn's speaker changes (so the next rendered line
/// gets the "You"/"Snippet" tag). A blank line separates one turn from the next.
fn set_speaker(lines: &mut Vec<Line<'static>>, speaker: &mut Option<bool>, tag_pending: &mut bool, agent: bool) {
    if *speaker == Some(agent) {
        return;
    }
    if speaker.is_some() {
        lines.push(Line::from(""));
    }
    *speaker = Some(agent);
    *tag_pending = true;
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
    // Mobile-app grammar: each turn opens with a "you" / "snippet" header, content
    // sits flush beneath it, and the header (not blank lines) separates speakers.
    let mut speaker: Option<bool> = None; // Some(true)=agent, Some(false)=you
    let mut prev_tool_row = false;

    // Content is rendered in a column to the RIGHT of the fixed speaker tag.
    let content_w = width.saturating_sub(TAG_W).max(20);
    let mut tag_pending = false;

    if !has_user_events && !state.user_request.is_empty() {
        speaker = Some(false);
        tag_pending = true;
        push_tagged(&mut lines, user_lines(&state.user_request, content_w), false, &mut tag_pending);
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
            set_speaker(&mut lines, &mut speaker, &mut tag_pending, true);
            push_tagged(&mut lines, marker_block("✗", danger(), &last, content_w), true, &mut tag_pending);
            prev_tool_row = false;
            continue;
        }

        // Tool call: render `● verb  arg` and, when the next event is a one-line
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
            let mut call_lines = tool_call_lines(tool_name, arguments, content_w);
            let result_follows = matches!(events.peek(), Some(HarnessEvent::ToolResult { .. }));
            if result_follows {
                if call_lines.len() == 1 {
                    if let Some(HarnessEvent::ToolResult { tool_name: rn, result }) = events.peek() {
                        if !HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                            if let Some(summary) = tool_result_oneliner(rn, result) {
                                let pad = content_w
                                    .saturating_sub(call_lines[0].width() + summary.chars().count());
                                if pad >= 2 {
                                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                                    call_lines[0]
                                        .spans
                                        .push(Span::styled(summary, Style::default().fg(faint())));
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
                    && content_w > call_lines[0].width() + label.chars().count() + 2
                {
                    let pad = content_w - call_lines[0].width() - label.chars().count();
                    call_lines[0].spans.push(Span::raw(" ".repeat(pad)));
                    call_lines[0].spans.push(Span::styled(label, style));
                } else {
                    call_lines.push(Line::from(vec![
                        Span::raw(" ".repeat(AGENT)),
                        Span::styled(label, style),
                    ]));
                }
            }
            set_speaker(&mut lines, &mut speaker, &mut tag_pending, true);
            push_tagged(&mut lines, call_lines, true, &mut tag_pending);
            prev_tool_row = true;
            continue;
        }

        // A lane's final report can be a whole page of text; render it collapsed to
        // a short preview unless the user has expanded lane output (Ctrl-O).
        let rendered = if let HarnessEvent::LaneCompleted { id, title, status, summary } = event {
            lane_completed_lines(id, title, *status, summary.as_deref(), content_w, app.lanes_expanded)
        } else {
            event_lines(event, content_w)
        };
        if rendered.is_empty() {
            continue;
        }
        let is_user = matches!(event, HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. });
        set_speaker(&mut lines, &mut speaker, &mut tag_pending, !is_user);
        push_tagged(&mut lines, rendered, !is_user, &mut tag_pending);
        prev_tool_row = matches!(
            event,
            HarnessEvent::ToolResult { .. } | HarnessEvent::InvalidToolCall { .. }
        );
    }
    let _ = prev_tool_row;

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
            lines.extend(indent_block(render_prose(live, width.saturating_sub(SPINE)), SPINE));
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

pub(super) const SPINE: usize = 0;
const AGENT: usize = 2;
const VERB_W: usize = 8;

fn user_gutter() -> Span<'static> {
    Span::styled("❯ ", Style::default().fg(accent()).add_modifier(Modifier::BOLD))
}

fn agent_gutter(glyph: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!("{}{glyph} ", " ".repeat(SPINE)),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn indent_block(lines: Vec<Line<'static>>, cols: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            l.spans.insert(0, Span::raw(" ".repeat(cols)));
            l
        })
        .collect()
}

/// Map one event to a block of styled, wrapped lines. Empty = hidden.
pub(super) fn event_lines(event: &HarnessEvent, width: usize) -> Vec<Line<'static>> {
    match event {
        HarnessEvent::UserInput { text } => user_lines(text, width),
        HarnessEvent::Steer { text } => {
            marker_block("↳", accent(), &strip_attachment_markers(text), width)
        }
        HarnessEvent::AssistantText { text } => indent_block(render_prose(text, width.saturating_sub(SPINE)), SPINE),
        HarnessEvent::Note { entry } => {
            // The agent's private scratchpad — recede it (faint + italic) so it
            // reads as a quiet aside, not content on par with the answer.
            let mut lines = marker_block("✎", faint(), entry, width);
            for line in &mut lines {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::ITALIC | Modifier::DIM);
                }
            }
            lines
        }
        HarnessEvent::FilePresented { path, caption } => {
            let text = match caption {
                Some(c) => format!("{path} — {c}"),
                None => path.clone(),
            };
            marker_block("▤", accent(), &text, width)
        }
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
                marker_block("⚙", warn(), &format!("{step} — {reasoning}"), width)
            }
        }
        HarnessEvent::ModelError { message } => {
            marker_block("✗", danger(), message, width)
        }
        HarnessEvent::UserQuestion { questions } => {
            // No "? " marker — questions almost always end with one already.
            let text = question_text(questions).unwrap_or_else(|| "(question)".to_string());
            marker_block("?", warn(), &text, width)
        }
        HarnessEvent::ApprovalRequest { .. } => {
            // While pending it's shown in the approval card above the input; the
            // outcome is logged via the `approval_resolved` decision. No transcript
            // line for the bare request.
            Vec::new()
        }
        // Subject only — lane ids are internal plumbing, not for the transcript.
        HarnessEvent::LaneSpawned { id: _, title } => {
            marker_block("→", lane(), &format!("delegated: {title}"), width)
        }
        HarnessEvent::LaneCompleted {
            id,
            title,
            status,
            summary,
        } => lane_completed_lines(id, title, *status, summary.as_deref(), width, false),
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
    // Your messages are the brightest, boldest text — the conversation's spine, so
    // your questions stand out from the agent's replies at a glance.
    let body = Style::default().fg(self::text()).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = wrap_one(&cleaned, width.saturating_sub(SPINE))
        .into_iter()
        .map(|seg| Line::from(vec![Span::raw(" ".repeat(SPINE)), Span::styled(seg, body)]))
        .collect();
    let (imgs, files) = count_attachments(text);
    if imgs + files > 0 {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(attachment_summary(imgs, files), Style::default().fg(muted())),
        ]));
    }
    lines
}

const TAG_W: usize = 2;

/// The inline speaker marker that opens a turn — a slim colored bar (amber for the
/// agent, muted for you) in a fixed gutter, so content hangs in an even column.
fn tag_span(agent: bool) -> Span<'static> {
    let color = if agent { accent() } else { muted() };
    Span::styled("▍ ", Style::default().fg(color))
}

fn tag_pad() -> Span<'static> {
    Span::raw(" ".repeat(TAG_W))
}

/// Prepend the speaker column to a rendered block: the tag on the first line of a
/// turn, blank padding on the rest (so a multi-line message hangs in one column).
fn push_tagged(lines: &mut Vec<Line<'static>>, inner: Vec<Line<'static>>, agent: bool, tag_pending: &mut bool) {
    for mut line in inner {
        let prefix = if *tag_pending {
            *tag_pending = false;
            tag_span(agent)
        } else {
            tag_pad()
        };
        line.spans.insert(0, prefix);
        lines.push(line);
    }
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
    color: Color,
    text: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let body_style = Style::default().fg(color);
    let mut lines = Vec::new();
    for (i, seg) in wrap_one(text, width.saturating_sub(AGENT))
        .into_iter()
        .enumerate()
    {
        if i == 0 {
            lines.push(Line::from(vec![
                agent_gutter(glyph, color),
                Span::styled(seg, body_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(AGENT)),
                Span::styled(seg, body_style),
            ]));
        }
    }
    lines
}

/// Lines shown from a completed lane's report before it's collapsed. A delegated
/// agent's final message is often a full page; the transcript shows this many
/// lines with a "+N more · ^O" hint until the user expands (`expanded`).
const LANE_PREVIEW_LINES: usize = 3;

pub(super) fn lane_completed_lines(
    id: &str,
    title: &str,
    status: LaneStatus,
    summary: Option<&str>,
    width: usize,
    expanded: bool,
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
        agent_gutter("◆", color),
        Span::styled(
            format!("{title} "),
            Style::default().fg(self::text()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{tag}]"), Style::default().fg(color)),
    ])];
    if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
        let body = result_block(vec![(summary.to_string(), subtle())], width);
        if expanded || body.len() <= LANE_PREVIEW_LINES {
            lines.extend(body);
        } else {
            let hidden = body.len() - LANE_PREVIEW_LINES;
            lines.extend(body.into_iter().take(LANE_PREVIEW_LINES));
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(AGENT)),
                Span::styled(
                    format!("… +{hidden} more line{} · ^O to expand", if hidden == 1 { "" } else { "s" }),
                    Style::default().fg(faint()).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
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
    let verb = verb.to_lowercase();
    // Terminals have one glyph size, so "smaller" is faked with intensity: the
    // whole tool row renders DIM (most emulators drop it a visual weight class),
    // verb slightly brighter than the argument, and only the amber `●` at full
    // strength. Prose stays bright and un-dimmed — a clear size-like hierarchy.
    let verb_style = Style::default().fg(muted()).add_modifier(Modifier::DIM);
    let arg_style = Style::default().fg(faint()).add_modifier(Modifier::DIM);
    let field = VERB_W.max(verb.chars().count());
    let arg_col = AGENT + field;
    let arg_budget = width.saturating_sub(arg_col).max(8);

    let dot = Span::styled(
        format!("{}● ", " ".repeat(SPINE)),
        Style::default().fg(accent()).add_modifier(Modifier::DIM),
    );
    if arg.trim().is_empty() {
        return vec![Line::from(vec![dot, Span::styled(verb, verb_style)])];
    }

    let mut lines = Vec::new();
    for (i, seg) in wrap_one(&arg, arg_budget).into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                dot.clone(),
                Span::styled(format!("{verb:<field$}"), verb_style),
                Span::styled(seg, arg_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(arg_col)),
                Span::styled(seg, arg_style),
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

    // Oversized output was spilled to a scratch file (no stdout/data here) — say so
    // explicitly instead of falling through to a misleading "no output".
    if result.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        let chars = result
            .pointer("/original_stats/char_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let where_ = result
            .get("saved_output_path")
            .and_then(Value::as_str)
            .map(|p| format!(" → {p}"))
            .unwrap_or_default();
        let head = if chars > 0 {
            format!("output too large ({} chars){where_}", fmt_si(chars))
        } else {
            format!("output too large{where_}")
        };
        return result_block(vec![(head, subtle().add_modifier(Modifier::ITALIC))], width);
    }

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
    let success = data.get("success").and_then(Value::as_bool).unwrap_or(false);
    let exit = data
        .get("exit_code")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("");

    let total = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|l| !l.trim().is_empty())
        .count();

    // Just a one-line summary — the command is already the call row above, and the
    // model has the full output; the UI doesn't echo it.
    let noun = if total == 1 { "line" } else { "lines" };
    let summary = match (success, total) {
        (true, 0) => "ran · no output".to_string(),
        (true, n) => format!("ran · {n} {noun}"),
        (false, 0) => format!("exited {exit} · no output"),
        (false, n) => format!("exited {exit} · {n} {noun}"),
    };
    let summary_style = if success { subtle() } else { Style::default().fg(danger()) };
    vec![(summary, summary_style)]
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
            wrap_code_line(&text, width.saturating_sub(AGENT + 2))
        } else {
            wrap_one(&text, width.saturating_sub(AGENT + 2))
        };
        for seg in segs {
            let prefix = if first {
                format!("{}↳ ", " ".repeat(AGENT))
            } else {
                " ".repeat(AGENT + 2)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(faint()).add_modifier(Modifier::DIM)),
                Span::styled(seg, style.add_modifier(Modifier::DIM)),
            ]));
            first = false;
        }
    }
    lines
}

pub fn preview_transcript(width: usize) -> String {
    fn plain2(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
    let mut pre = String::from("===== EMPTY STATE =====\n");
    pre.push_str(&plain2(&empty_state_lines("~/code/wacht", "grok 4.5 (high)", width)));
    pre.push_str("\n\n===== TRANSCRIPT =====\n");

    use serde_json::json;
    fn plain(lines: &[Line<'static>]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
    let cw = width.saturating_sub(TAG_W);
    let mut all: Vec<Line<'static>> = Vec::new();
    let mut tp = true;
    push_tagged(&mut all, user_lines("you good bro?", cw), false, &mut tp);
    all.push(Line::from("")); tp = true;
    push_tagged(&mut all, render_prose("Haha yeah, doing great! What do you want to build?", cw), true, &mut tp);
    all.push(Line::from("")); tp = true;
    push_tagged(&mut all, user_lines("fix the auth timeout bug", cw), false, &mut tp);
    all.push(Line::from("")); tp = true;
    let calls = [
        ("read_file", json!({"path":"src/auth.rs"}), Some("142 lines")),
        ("edit_file", json!({"path":"src/auth.rs","old_string":"5","new_string":"30"}), None),
    ];
    for (name, args, summary) in calls {
        let mut cl = tool_call_head_lines(name, &args, cw);
        if let Some(sm) = summary {
            let pad = cw.saturating_sub(cl[0].width() + sm.chars().count());
            if pad >= 2 { cl[0].spans.push(Span::raw(" ".repeat(pad))); cl[0].spans.push(Span::raw(sm.to_string())); }
        }
        push_tagged(&mut all, cl, true, &mut tp);
    }
    push_tagged(&mut all, render_prose("Bumped the 5s timeout to 30s and the tests pass now.", cw), true, &mut tp);
    format!("{pre}{}", plain(&all))
}
