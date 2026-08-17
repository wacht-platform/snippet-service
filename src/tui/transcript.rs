use super::markdown::*;
use super::theme::*;
use super::*;

/// The empty-state welcome: calm, left-aligned context + a compact command
/// legend. No animation — a quiet starting page in the Terminal Ink palette.
pub(super) fn empty_state_lines(_cwd: &str, _model: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let width = width.max(50);

    let center = |s: &str, style: Style| -> Line<'static> {
        let len = s.chars().count();
        let pad = width.saturating_sub(len) / 2;
        Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(s.to_string(), style),
        ])
    };

    let title_style = Style::default()
        .fg(Color::Rgb(165, 180, 252))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(71, 85, 105));

    lines.push(Line::from(""));
    lines.push(center("t                                          T", dim));
    lines.push(center("G                                           ", dim));
    lines.push(Line::from(""));

    let green = Style::default().fg(Color::Rgb(74, 222, 128));
    let cyan = Style::default().fg(Color::Rgb(125, 207, 245));
    let purple = Style::default().fg(Color::Rgb(189, 147, 249));

    // Crisp Vector Catgirl Pet Mascot Artwork
    lines.push(center(
        "          /\\___/\\       /\\___/\\          ",
        purple,
    ));
    lines.push(center("         (  o.o  )     (  o.o  )  ~♥     ", green));
    lines.push(center("          >  ^  <       >  ^  <          ", cyan));
    lines.push(center("      . - ~ ~ ~ ~ ~ ~ ~ ~ ~ ~ ~ - .      ", green));
    lines.push(center("    /   .-----------------------.   \\    ", green));
    lines.push(center(
        "   /   /    (◕ ◡ ◕)   SNIPPET    \\   \\   ",
        cyan.add_modifier(Modifier::BOLD),
    ));
    lines.push(center("  |   |     \\  ♥  /   MATRIX PET  |   |  ", purple));
    lines.push(center("   \\   \\     `---'               /   /   ", green));
    lines.push(center("     ~ - . _ . _ . _ . _ . _ . - ~       ", green));
    lines.push(Line::from(""));

    // Block Pixel Title SNIPPET
    lines.push(center(
        "███████ ███    ██ ███████ ███████ ██████  ███████ ████████",
        title_style,
    ));
    lines.push(center(
        "██      ████   ██    ███  ██      ██   ██ ██         ██   ",
        title_style,
    ));
    lines.push(center(
        "███████ ██ ██  ██   ███   █████   ██████  █████      ██   ",
        title_style,
    ));
    lines.push(center(
        "     ██ ██  ██ ██  ███    ██      ██      ██         ██   ",
        title_style,
    ));
    lines.push(center(
        "███████ ██   ████ ███████ ███████ ██      ███████    ██   ",
        title_style,
    ));
    lines.push(Line::from(""));

    lines.push(center("g                                          g", dim));
    lines.push(center("   t                                        ", dim));

    lines
}

/// Arm the speaker tag when the turn's speaker changes (so the next rendered line
/// gets the "You"/"Snippet" tag). A blank line separates one turn from the next.
fn set_speaker(
    lines: &mut Vec<Line<'static>>,
    speaker: &mut Option<bool>,
    tag_pending: &mut bool,
    agent: bool,
) {
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

    if !has_user_events {
        if let Some(request) = state
            .initial_request()
            .filter(|text| !text.trim().is_empty())
        {
            speaker = Some(false);
            tag_pending = true;
            push_tagged(
                &mut lines,
                user_lines(request, content_w),
                false,
                &mut tag_pending,
            );
        }
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
            push_tagged(
                &mut lines,
                marker_block("✗", danger(), &last, content_w),
                true,
                &mut tag_pending,
            );
            prev_tool_row = false;
            continue;
        }

        // Tool call: render `• verb arg` cleanly without double-tagging
        if let HarnessEvent::ToolCall {
            tool_name,
            arguments,
        } = event
        {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                // Drop the paired hidden result too, so no orphan row renders.
                if let Some(HarnessEvent::ToolResult { tool_name: rn, .. }) = events.peek() {
                    if HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                        events.next();
                    }
                }
                continue;
            }

            set_speaker(&mut lines, &mut speaker, &mut tag_pending, true);

            // If transitioning from prose text to a tool call, add a blank line above the tool run
            if !prev_tool_row
                && !lines.is_empty()
                && lines.last().map_or(true, |l| !l.spans.is_empty())
            {
                lines.push(Line::from(""));
            }

            // Pair call + result into one Cursor-style row. Ctrl-O (tools_expanded)
            // reveals arg previews and fuller result bodies; collapsed stays one line.
            let expanded = app.tools_expanded;
            let mut status = if state.status == HarnessStatus::Running {
                ToolRowStatus::Running
            } else {
                ToolRowStatus::Done
            };
            let mut result_value: Option<Value> = None;
            if let Some(HarnessEvent::ToolResult {
                tool_name: rn,
                result,
            }) = events.peek()
            {
                if rn == tool_name && !HIDDEN_TOOL_ROWS.contains(&rn.as_str()) {
                    let failed = result.get("status").and_then(Value::as_str) == Some("error");
                    status = if failed {
                        ToolRowStatus::Failed
                    } else {
                        ToolRowStatus::Done
                    };
                    result_value = Some(result.clone());
                    events.next();
                }
            } else if state.status != HarnessStatus::Running {
                status = ToolRowStatus::Done;
            }

            let mut call_lines =
                tool_call_head_lines_status(tool_name, arguments, content_w, status);

            let can_expand = tool_is_expandable(tool_name, arguments, result_value.as_ref());
            if can_expand {
                let hint = if expanded {
                    "(ctrl+o to collapse)"
                } else {
                    "(ctrl+o to expand)"
                };
                if let Some(first) = call_lines.first_mut() {
                    let need = hint.chars().count() + 1;
                    if content_w > first.width() + need {
                        first.spans.push(Span::raw(" "));
                        first.spans.push(Span::styled(
                            hint.to_string(),
                            Style::default().fg(faint()).add_modifier(Modifier::DIM),
                        ));
                    }
                }
            }

            if matches!(status, ToolRowStatus::Running) {
                let spinner = SPINNER[(app.frame / 2) % SPINNER.len()];
                if let Some(first) = call_lines.first_mut() {
                    first.spans.push(Span::raw(" "));
                    first.spans.push(Span::styled(
                        spinner.to_string(),
                        Style::default().fg(accent()),
                    ));
                }
            }

            if expanded {
                call_lines.extend(tool_call_preview(tool_name, arguments, content_w));
                if let Some(result) = result_value.as_ref() {
                    call_lines.extend(tool_result_lines_expanded(tool_name, result, content_w));
                }
            } else if let Some(result) = result_value.as_ref() {
                // Collapsed: keep errors visible under the row; success stays one-line.
                if result.get("status").and_then(Value::as_str) == Some("error") {
                    call_lines.extend(tool_result_lines(tool_name, result, content_w));
                }
            }

            // Push tool rows flush with the content column (no speaker double-tag).
            lines.extend(call_lines);
            prev_tool_row = true;
            continue;
        }

        // Lane lifecycle and reports live in the dedicated lanes screen; keep the
        // conversation canvas free of duplicate lane status and report previews.
        if matches!(
            event,
            HarnessEvent::LaneSpawned { .. } | HarnessEvent::LaneCompleted { .. }
        ) {
            continue;
        }

        let rendered = event_lines(event, content_w);
        if rendered.is_empty() {
            continue;
        }
        let is_user = matches!(
            event,
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. }
        );

        set_speaker(&mut lines, &mut speaker, &mut tag_pending, !is_user);

        // Add vertical spacing between tool runs and prose text
        if prev_tool_row && !lines.is_empty() && lines.last().map_or(true, |l| !l.spans.is_empty())
        {
            lines.push(Line::from(""));
        }
        push_tagged(&mut lines, rendered, !is_user, &mut tag_pending);
        prev_tool_row = false;
    }
    let _ = prev_tool_row;

    // Live "working…" feedback at the tail while the agent is processing (or a lane is).
    let working = state.status == HarnessStatus::Running
        || state
            .lanes
            .iter()
            .any(|lane| lane.status == LaneStatus::Running);
    if working && app.agent_alive() {
        // Reasoning is live-only — hide it once the turn leaves Running so it
        // can't stick under the committed answer after the buffer clears late.
        let thinking = crate::llm::StreamBuffer::snapshot_thinking(&app.stream);
        let thinking = thinking.trim_end();
        // Hide reasoning once this turn has taken a tool/action — the action
        // is the UI, leftover thought is noise.
        let hide_thought = turn_has_visible_action(&state.events);
        if !thinking.is_empty() && !hide_thought {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(thinking_lines(thinking, content_w));
        }
        // Text the model is streaming this turn, shown live until it commits to a
        // durable AssistantText event (then refresh_state clears the buffer).
        let live = crate::llm::StreamBuffer::snapshot(&app.stream);
        let live = live.trim_end();
        if !live.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.extend(indent_block(
                render_prose(live, width.saturating_sub(SPINE)),
                SPINE,
            ));
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

fn turn_has_visible_action(events: &[HarnessEvent]) -> bool {
    for event in events.iter().rev() {
        match event {
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. } => return false,
            HarnessEvent::ToolCall { .. }
            | HarnessEvent::ToolResult { .. }
            | HarnessEvent::InvalidToolCall { .. }
            | HarnessEvent::Note { .. }
            | HarnessEvent::FilePresented { .. }
            | HarnessEvent::UserQuestion { .. }
            | HarnessEvent::ApprovalRequest { .. }
            | HarnessEvent::LaneSpawned { .. }
            | HarnessEvent::LaneCompleted { .. }
            | HarnessEvent::AssistantText { .. } => return true,
            _ => {}
        }
    }
    false
}

/// Render model thinking with the same Markdown-lite treatment as assistant
/// prose, but flatten the palette so it remains a quiet, dimmed aside.
fn thinking_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    render_prose(text, width.saturating_sub(SPINE))
        .into_iter()
        .map(|mut line| {
            for span in &mut line.spans {
                span.style = span.style.fg(muted()).add_modifier(Modifier::DIM);
            }
            line.spans.insert(0, Span::raw(" ".repeat(SPINE)));
            line
        })
        .collect()
}

/// Map one event to a block of styled, wrapped lines. Empty = hidden.
pub(super) fn event_lines(event: &HarnessEvent, width: usize) -> Vec<Line<'static>> {
    match event {
        HarnessEvent::UserInput { text } => user_lines(text, width),
        // Steers are still your words mid-run — same column as user messages, no
        // extra ↳ gutter (that stacked with the speaker tag and broke indent).
        HarnessEvent::Steer { text } => steer_lines(text, width),
        HarnessEvent::AssistantText { text } => {
            indent_block(render_prose(text, width.saturating_sub(SPINE)), SPINE)
        }
        HarnessEvent::Note { entry } => {
            // The agent's private scratchpad — recede it (faint + italic) so it
            // reads as a quiet aside, not content on par with the answer.
            let mut lines = marker_block("·", faint(), entry, width);
            for line in &mut lines {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::ITALIC | Modifier::DIM);
                }
            }
            lines
        }
        HarnessEvent::FilePresented { path, caption } => {
            present_file_lines(path, caption.as_deref(), width)
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
            } else if step == "tool_payloads_pruned" {
                tool_prune_divider(width)
            } else {
                marker_block("⚙", warn(), &format!("{step} — {reasoning}"), width)
            }
        }
        HarnessEvent::ModelError { message } => marker_block("✗", danger(), message, width),
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
            tool_call_lines(tool_name, arguments, width, false)
        }
        HarnessEvent::ToolResult { tool_name, result } => {
            if HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                return Vec::new();
            }
            tool_result_lines(tool_name, result, width)
        }
        HarnessEvent::InvalidToolCall { tool_name, error } => result_block(
            vec![(
                format!("✗ {tool_name}: {error}"),
                Style::default().fg(danger()),
            )],
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
            !((t.starts_with("[attached image —") || t.starts_with("[attached file —"))
                && t.ends_with(']'))
        })
        .collect();
    kept.join("\n").trim_end().to_string()
}

/// Split user/steer text into prose + optional audio-transcript sections that the
/// serve layer appends after voice attachments.
fn split_audio_sections(text: &str) -> (String, Vec<(String, String)>) {
    let mut prose = String::new();
    let mut audio: Vec<(String, String)> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        let header = t
            .strip_prefix("[Audio transcript for ")
            .and_then(|rest| rest.strip_suffix(']'))
            .map(str::trim);
        let fail = t
            .strip_prefix("[Audio transcription unavailable: ")
            .and_then(|rest| rest.strip_suffix(']'))
            .map(str::trim);
        if let Some(path) = header {
            let mut body = String::new();
            while let Some(next) = lines.peek() {
                let n = next.trim_start();
                if n.starts_with("[Audio transcript for ")
                    || n.starts_with("[Audio transcription unavailable: ")
                    || ((n.starts_with("[attached image —") || n.starts_with("[attached file —"))
                        && n.ends_with(']'))
                {
                    break;
                }
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(lines.next().unwrap());
            }
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_string();
            audio.push((name, body.trim().to_string()));
            continue;
        }
        if let Some(err) = fail {
            audio.push((
                "voice".to_string(),
                format!("(transcription unavailable: {err})"),
            ));
            continue;
        }
        if !prose.is_empty() {
            prose.push('\n');
        }
        prose.push_str(line);
    }
    (prose.trim_end().to_string(), audio)
}

fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    if text.trim().is_empty() {
        return;
    }
    for seg in wrap_one(text, width.saturating_sub(SPINE)) {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(seg, style),
        ]));
    }
}

fn audio_block_lines(name: &str, body: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let label = format!("voice · {name}");
    out.push(Line::from(vec![
        Span::raw(" ".repeat(SPINE)),
        Span::styled(
            label,
            Style::default().fg(muted()).add_modifier(Modifier::ITALIC),
        ),
    ]));
    let body_style = Style::default().fg(self::text());
    if body.trim().is_empty() {
        out.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(
                "(no speech detected)".to_string(),
                Style::default().fg(faint()),
            ),
        ]));
        return out;
    }
    for seg in wrap_one(body, width.saturating_sub(SPINE)) {
        out.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(seg, body_style),
        ]));
    }
    out
}

pub(super) fn user_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let cleaned = strip_attachment_markers(text);
    let (prose, audio) = split_audio_sections(&cleaned);
    // Your messages are the brightest, boldest text — the conversation's spine, so
    // your questions stand out from the agent's replies at a glance.
    let body = Style::default()
        .fg(self::text())
        .add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    push_wrapped(&mut lines, &prose, width, body);
    for (name, transcript) in &audio {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(audio_block_lines(name, transcript, width));
    }
    let (imgs, files) = count_attachments(text);
    if imgs + files > 0 {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(
                attachment_summary(imgs, files),
                Style::default().fg(muted()),
            ),
        ]));
    }
    if lines.is_empty() {
        // Pure attachment / empty after strip — keep a single blank body so the
        // speaker tag still has a row.
        lines.push(Line::from(vec![Span::raw(" ".repeat(SPINE))]));
    }
    lines
}

/// Mid-run steer: same column as user text, quiet label, no enter-arrow glyph.
fn steer_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let cleaned = strip_attachment_markers(text);
    let (prose, audio) = split_audio_sections(&cleaned);
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(SPINE)),
        Span::styled(
            "steer".to_string(),
            Style::default().fg(muted()).add_modifier(Modifier::ITALIC),
        ),
    ]));
    let body = Style::default().fg(self::text());
    push_wrapped(&mut lines, &prose, width, body);
    for (name, transcript) in &audio {
        if lines.len() > 1 {
            lines.push(Line::from(""));
        }
        lines.extend(audio_block_lines(name, transcript, width));
    }
    let (imgs, files) = count_attachments(text);
    if imgs + files > 0 {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(
                attachment_summary(imgs, files),
                Style::default().fg(muted()),
            ),
        ]));
    }
    lines
}

/// Agent-presented file card: path + optional caption, aligned to content column.
fn present_file_lines(path: &str, caption: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let mut lines = Vec::new();
    // Title row: "file  name" — no box-drawing glyph (terminals often mis-align them).
    let title = format!("file  {name}");
    for (i, seg) in wrap_one(&title, width.saturating_sub(SPINE))
        .into_iter()
        .enumerate()
    {
        let style = if i == 0 {
            Style::default().fg(accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(accent())
        };
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(SPINE)),
            Span::styled(seg, style),
        ]));
    }
    // Full path when it differs from the basename (muted second line).
    if name != path {
        push_wrapped(&mut lines, path, width, Style::default().fg(faint()));
    }
    if let Some(c) = caption.map(str::trim).filter(|c| !c.is_empty()) {
        push_wrapped(&mut lines, c, width, Style::default().fg(muted()));
    }
    lines
}

const TAG_W: usize = 2;

/// The inline speaker marker that opens a turn — a slim colored bar (amber for the
/// agent, muted for you) in a fixed gutter, so content hangs in an even column.
fn tag_span(agent: bool) -> Span<'static> {
    if agent {
        Span::styled("✦ ", Style::default().fg(accent()))
    } else {
        Span::styled(
            "> ",
            Style::default().fg(text()).add_modifier(Modifier::BOLD),
        )
    }
}

fn tag_pad() -> Span<'static> {
    Span::raw(" ".repeat(TAG_W))
}

/// Prepend the speaker column to a rendered block: the tag on the first line of a
/// turn, blank padding on the rest (so a multi-line message hangs in one column).
fn push_tagged(
    lines: &mut Vec<Line<'static>>,
    inner: Vec<Line<'static>>,
    agent: bool,
    tag_pending: &mut bool,
) {
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
    centered_divider(width, " ✦ context compacted ")
}

/// Quiet divider for mid-window tool-payload pruning (cheaper than full compact).
pub(super) fn tool_prune_divider(width: usize) -> Vec<Line<'static>> {
    centered_divider(width, " ▸ older tools pruned ")
}

fn centered_divider(width: usize, label: &str) -> Vec<Line<'static>> {
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
            Style::default()
                .fg(self::text())
                .add_modifier(Modifier::BOLD),
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
                    format!(
                        "… +{hidden} more line{} · ^O to expand",
                        if hidden == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(faint()).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    lines
}

pub(super) fn tool_call_lines(
    tool_name: &str,
    arguments: &Value,
    width: usize,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut lines = tool_call_head_lines_status(tool_name, arguments, width, ToolRowStatus::Done);
    if expanded {
        lines.extend(tool_call_preview(tool_name, arguments, width));
    }
    lines
}

/// Render the call header as Cursor-style: `● Verb(args)` with a status dot and
/// bold yellow verb. Optional expand hint is appended by the caller.
#[derive(Clone, Copy)]
pub(super) enum ToolRowStatus {
    Done,
    Running,
    Failed,
}

pub(super) fn tool_call_head_lines_status(
    tool_name: &str,
    arguments: &Value,
    width: usize,
    status: ToolRowStatus,
) -> Vec<Line<'static>> {
    let (verb, arg) = tool_call_parts(tool_name, arguments);
    // Reference UI: Title-case verb, path/args inside parentheses.
    let call = if arg.trim().is_empty() {
        verb.clone()
    } else {
        format!("{verb}({arg})")
    };

    let (dot_glyph, dot_color) = match status {
        ToolRowStatus::Done => ("●", success()),
        ToolRowStatus::Running => ("●", accent()),
        ToolRowStatus::Failed => ("●", danger()),
    };
    let verb_style = Style::default().fg(warn()).add_modifier(Modifier::BOLD);
    let arg_style = Style::default().fg(self::text());
    let paren_style = Style::default().fg(muted());

    // Budget for the call body after "● ".
    let prefix_w = 2; // "● "
    let budget = width.saturating_sub(prefix_w).max(12);

    let mut lines = Vec::new();
    if call.chars().count() <= budget {
        // Single line: color verb vs (args) separately when possible.
        let mut spans = vec![Span::styled(
            format!("{}{dot_glyph} ", " ".repeat(SPINE)),
            Style::default().fg(dot_color),
        )];
        if arg.trim().is_empty() {
            spans.push(Span::styled(verb, verb_style));
        } else {
            spans.push(Span::styled(verb, verb_style));
            spans.push(Span::styled("(".to_string(), paren_style));
            // Keep args on the same visual weight as body text; wrap below if needed.
            spans.push(Span::styled(arg.clone(), arg_style));
            spans.push(Span::styled(")".to_string(), paren_style));
        }
        lines.push(Line::from(spans));
        return lines;
    }

    // Long args: first line `● Verb(` then hanging-indent arg lines, then `)`.
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}{dot_glyph} ", " ".repeat(SPINE)),
            Style::default().fg(dot_color),
        ),
        Span::styled(verb, verb_style),
        Span::styled("(".to_string(), paren_style),
    ]));
    let hang = prefix_w + 2;
    let arg_budget = width.saturating_sub(hang).max(8);
    let wrapped = wrap_one(&arg, arg_budget);
    let last = wrapped.len().saturating_sub(1);
    for (i, seg) in wrapped.into_iter().enumerate() {
        let mut spans = vec![Span::raw(" ".repeat(hang))];
        if i == last {
            spans.push(Span::styled(seg, arg_style));
            spans.push(Span::styled(")".to_string(), paren_style));
        } else {
            spans.push(Span::styled(seg, arg_style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A preview of what the call will do — content for writes, a +/- diff for edits.
/// Redesigned for clarity and breathing room.
pub(super) fn tool_call_preview(
    tool_name: &str,
    arguments: &Value,
    width: usize,
) -> Vec<Line<'static>> {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    let path = arg("path");
    let path_style = Style::default().fg(code()).add_modifier(Modifier::ITALIC);
    let green = Style::default().fg(success());
    let red = Style::default().fg(danger());
    const MAX: usize = 6;

    let mut items: Vec<(String, Style)> = Vec::new();

    match tool_name {
        "write_file" => {
            let content = arg("content");
            let total = content.lines().count();

            // Header with file path
            items.push((format!("→ {}", path), path_style));
            items.push(("".to_string(), subtle()));

            // Content preview
            for line in content.lines().take(MAX) {
                items.push((format!("  {}", line), green));
            }
            if total > MAX {
                items.push(("".to_string(), subtle()));
                items.push((format!("  … +{} more lines", total - MAX), subtle()));
            }
        }
        "edit_file" => {
            let old = arg("old_string");
            let new = arg("new_string");
            let old_total = old.lines().count();
            let new_total = new.lines().count();

            // Header with file path
            items.push((format!("→ {}", path), path_style));
            items.push(("".to_string(), subtle()));

            // Old content (removed)
            if old_total > 0 {
                for line in old.lines().take(MAX) {
                    items.push((format!("  {}", line), red));
                }
                if old_total > MAX {
                    items.push((format!("  … +{} more", old_total - MAX), subtle()));
                }
                items.push(("".to_string(), subtle()));
            }

            // New content (added)
            if new_total > 0 {
                for line in new.lines().take(MAX) {
                    items.push((format!("  {}", line), green));
                }
                if new_total > MAX {
                    items.push((format!("  … +{} more", new_total - MAX), subtle()));
                }
            }
        }
        "append_file" => {
            let content = arg("content");
            let total = content.lines().count();

            items.push((format!("→ {}", path), path_style));
            items.push(("".to_string(), subtle()));
            for line in content.lines().take(MAX) {
                items.push((format!("  {}", line), green));
            }
            if total > MAX {
                items.push(("".to_string(), subtle()));
                items.push((format!("  … +{} more lines", total - MAX), subtle()));
            }
        }
        "bash" => {
            let cmd = arg("command");
            let total = cmd.lines().count().max(1);
            items.push(("command".to_string(), path_style));
            for line in cmd.lines().take(MAX) {
                items.push((format!("  {}", line), green));
            }
            if total > MAX {
                items.push((format!("  … +{} more lines", total - MAX), subtle()));
            }
        }
        "memory_write" => {
            let id = arg("id");
            let content = arg("content");
            items.push((format!("id  {id}"), path_style));
            push_text_preview(&mut items, content, MAX, green, subtle());
        }
        "memory_rule" => {
            let scope = arg("scope");
            let content = arg("content");
            items.push((format!("scope  {scope}"), path_style));
            push_text_preview(&mut items, content, MAX, green, subtle());
        }
        "memory_pattern" => {
            let action = arg("action");
            let content = arg("content");
            if !action.is_empty() {
                items.push((format!("action  {action}"), path_style));
            }
            push_text_preview(&mut items, content, MAX, green, subtle());
        }
        "memory_index" => {
            push_text_preview(&mut items, arg("content"), MAX, green, subtle());
        }
        "memory_delete" => {
            items.push((format!("id  {}", arg("id")), path_style));
        }
        "memory_read" => {
            items.push((format!("id  {}", arg("id")), path_style));
        }
        _ => return Vec::new(),
    }
    result_block_verbatim(items, width)
}

fn push_text_preview(
    items: &mut Vec<(String, Style)>,
    content: &str,
    max: usize,
    body: Style,
    more: Style,
) {
    let total = content.lines().count();
    if content.trim().is_empty() {
        items.push(("  (empty)".to_string(), more));
        return;
    }
    for line in content.lines().take(max) {
        items.push((format!("  {line}"), body));
    }
    if total > max {
        items.push((format!("  … +{} more lines", total - max), more));
    }
}

/// Tools whose collapsed row is incomplete without an expanded body.
fn tool_is_expandable(tool_name: &str, arguments: &Value, result: Option<&Value>) -> bool {
    let arg = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    match tool_name {
        "write_file" | "append_file" => !arg("content").trim().is_empty(),
        "edit_file" => !arg("old_string").is_empty() || !arg("new_string").is_empty(),
        "bash" => {
            let cmd = arg("command");
            cmd.lines().count() > 1
                || cmd.chars().count() > 80
                || result.map(result_has_body).unwrap_or(false)
        }
        "memory_write" | "memory_rule" | "memory_pattern" | "memory_index" => {
            !arg("content").trim().is_empty()
        }
        "read_file" | "search_content" | "list_files" | "web_read" | "view_outline"
        | "code_map" => result.map(result_has_body).unwrap_or(false),
        _ => {
            // Any tool whose header arg was truncated, or result has a body.
            let (_, shown) = tool_call_parts(tool_name, arguments);
            shown.contains('…') || result.map(result_has_body).unwrap_or(false)
        }
    }
}

fn result_has_body(result: &Value) -> bool {
    if result.get("status").and_then(Value::as_str) == Some("error") {
        return true;
    }
    let data = result.get("data").unwrap_or(result);
    for key in ["stdout", "stderr", "content", "text", "output"] {
        if data
            .get(key)
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
    }
    data.get("entries")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
        || data
            .get("matches")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
}

/// A tool call as (verb, argument) — e.g. ("Read", "src/auth.rs") — so the verb
/// and its target can be styled distinctly instead of a single `Read(path)` blob.
pub(super) fn tool_call_parts(tool_name: &str, arguments: &Value) -> (String, String) {
    let arg = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match tool_name {
        "read_file" => ("Read".into(), arg("path")),
        "write_file" => ("Write".into(), arg("path")),
        "edit_file" => ("Edit".into(), arg("path")),
        "list_files" => (
            "List".into(),
            arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
        ),
        "search_content" => ("Search".into(), arg("query")),
        "search_files" => {
            let pattern = arg("pattern");
            (
                "Search".into(),
                if pattern.is_empty() {
                    arg("query")
                } else {
                    pattern
                },
            )
        }
        "view_outline" => ("Outline".into(), arg("path")),
        "code_map" => {
            let q = arg("query");
            let path = arg("path");
            let detail = if !q.is_empty() && !path.is_empty() {
                format!("{path} · {q}")
            } else if !q.is_empty() {
                q
            } else if !path.is_empty() {
                path
            } else {
                ".".into()
            };
            ("Map".into(), detail)
        }
        "web_search" => ("Web".into(), arg("query")),
        "web_read" => ("Fetch".into(), arg("url")),
        "read_image" => ("Read".into(), arg("path")),
        "bash" => {
            // Commands can be long or multi-line; show a compact single line (first
            // line, whitespace-collapsed, capped) with an ellipsis when elided.
            let cmd = arg("command");
            ("Bash".into(), ellipsize_one_line(&cmd, 90))
        }
        "memory_write" => ("MemoryWrite".into(), {
            let id = arg("id");
            let n = arg("content").lines().count();
            if id.is_empty() {
                format!("{n} lines")
            } else {
                format!("{id} · {n} lines")
            }
        }),
        "memory_read" => ("MemoryRead".into(), arg("id")),
        "memory_delete" => ("MemoryDelete".into(), arg("id")),
        "memory_index" => {
            let n = arg("content").lines().count();
            ("MemoryIndex".into(), format!("{n} lines"))
        }
        "memory_rule" => {
            let scope = arg("scope");
            let n = arg("content").lines().count();
            (
                "MemoryRule".into(),
                if scope.is_empty() {
                    format!("{n} lines")
                } else {
                    format!("{scope} · {n} lines")
                },
            )
        }
        "memory_pattern" => {
            let action = arg("action");
            let n = arg("content").lines().count();
            (
                "MemoryPattern".into(),
                if action.is_empty() {
                    format!("{n} lines")
                } else {
                    format!("{action} · {n} lines")
                },
            )
        }
        "append_file" => ("Append".into(), arg("path")),
        _ => {
            let pretty = tool_name
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => format!("{}{}", f.to_uppercase(), c.as_str()),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            // Never dump full JSON for unknown tools — pick a short label field.
            let detail = arguments
                .get("path")
                .or_else(|| arguments.get("id"))
                .or_else(|| arguments.get("query"))
                .or_else(|| arguments.get("name"))
                .or_else(|| arguments.get("title"))
                .and_then(Value::as_str)
                .map(|s| ellipsize_one_line(s, 80))
                .unwrap_or_else(|| {
                    let raw = serde_json::to_string(arguments).unwrap_or_default();
                    ellipsize_one_line(&raw, 60)
                });
            (pretty, detail)
        }
    }
}

fn ellipsize_one_line(text: &str, max_chars: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let compact = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let multi = text.lines().count() > 1;
    if compact.chars().count() <= max_chars && !multi {
        return compact;
    }
    let capped: String = compact.chars().take(max_chars).collect();
    if capped.chars().count() < compact.chars().count() || multi {
        format!("{capped} …")
    } else {
        capped
    }
}

pub(super) fn tool_result_lines(
    tool_name: &str,
    result: &Value,
    width: usize,
) -> Vec<Line<'static>> {
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let data = result.get("data").unwrap_or(result);

    // Oversized output was spilled to a scratch file (no stdout/data here) — say so
    // explicitly instead of falling through to a misleading "no output".
    if result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
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
            vec![(format!("{count} entries"), subtle()), (names, subtle())]
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
            let chars = data
                .get("text")
                .and_then(Value::as_str)
                .map(|t| t.chars().count())
                .unwrap_or(0);
            vec![(format!("Read {chars} chars"), subtle())]
        }
        "view_outline" => {
            if data
                .get("is_directory")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let count = data
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|e| e.len())
                    .unwrap_or(0);
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
    let success = data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
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
    let summary_style = if success {
        subtle()
    } else {
        Style::default().fg(danger())
    };
    vec![(summary, summary_style)]
}

/// Expanded tool result body (Ctrl-O): show stdout/content samples, not just counts.
pub(super) fn tool_result_lines_expanded(
    tool_name: &str,
    result: &Value,
    width: usize,
) -> Vec<Line<'static>> {
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let data = result.get("data").unwrap_or(result);
    const MAX: usize = 24;

    if result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return tool_result_lines(tool_name, result, width);
    }
    if status == "error" {
        return tool_result_lines(tool_name, result, width);
    }

    let str_field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("");
    let mut items: Vec<(String, Style)> = Vec::new();
    let body = subtle();
    let more = Style::default().fg(faint());

    match tool_name {
        "bash" => {
            items.extend(bash_result_items_expanded(data, MAX));
        }
        "read_file" => {
            let content = str_field("content");
            let total = content.lines().count();
            items.push((format!("{total} lines"), body));
            for line in content.lines().take(MAX) {
                items.push((line.to_string(), body));
            }
            if total > MAX {
                items.push((format!("… +{} more lines", total - MAX), more));
            }
        }
        "search_content" => {
            let count = data.get("count").and_then(Value::as_u64).unwrap_or(0);
            items.push((format!("{count} matches"), body));
            if let Some(arr) = data.get("matches").and_then(Value::as_array) {
                for m in arr.iter().take(MAX) {
                    let line = m
                        .get("line")
                        .or_else(|| m.get("text"))
                        .or_else(|| m.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let path = m.get("path").and_then(Value::as_str).unwrap_or("");
                    let ln = m.get("line_number").or_else(|| m.get("line_no"));
                    let head = match (path.is_empty(), ln.and_then(Value::as_u64)) {
                        (false, Some(n)) => format!("{path}:{n}: {line}"),
                        (false, None) => format!("{path}: {line}"),
                        _ => line.to_string(),
                    };
                    if !head.is_empty() {
                        items.push((head, body));
                    }
                }
                if arr.len() > MAX {
                    items.push((format!("… +{} more", arr.len() - MAX), more));
                }
            }
        }
        "list_files" => {
            return tool_result_lines(tool_name, result, width);
        }
        "web_read" => {
            let text = str_field("text");
            let total = text.lines().count();
            items.push((format!("{total} lines"), body));
            for line in text.lines().take(MAX) {
                items.push((line.to_string(), body));
            }
            if total > MAX {
                items.push((format!("… +{} more lines", total - MAX), more));
            }
        }
        "memory_read" => {
            let content = data
                .get("content")
                .or_else(|| data.get("entry"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let total = content.lines().count();
            for line in content.lines().take(MAX) {
                items.push((line.to_string(), body));
            }
            if total > MAX {
                items.push((format!("… +{} more lines", total - MAX), more));
            }
            if content.is_empty() {
                items.push(("saved".to_string(), body));
            }
        }
        "memory_write" | "memory_rule" | "memory_pattern" | "memory_index" | "memory_delete" => {
            items.push(("saved".to_string(), body));
            if let Some(id) = data.get("id").and_then(Value::as_str) {
                items.push((format!("id  {id}"), body));
            }
        }
        _ => return tool_result_lines(tool_name, result, width),
    }

    let items: Vec<(String, Style)> = items.into_iter().filter(|(s, _)| !s.is_empty()).collect();
    if items.is_empty() {
        return tool_result_lines(tool_name, result, width);
    }
    if tool_name == "bash" || tool_name == "read_file" {
        result_block_verbatim(items, width)
    } else {
        result_block(items, width)
    }
}

fn bash_result_items_expanded(data: &Value, max: usize) -> Vec<(String, Style)> {
    let success = data
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let exit = data
        .get("exit_code")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("");
    let body = if success {
        subtle()
    } else {
        Style::default().fg(danger())
    };
    let more = Style::default().fg(faint());
    let mut items = Vec::new();
    items.push((
        if success {
            format!("exit {exit}")
        } else {
            format!("exited {exit}")
        },
        body,
    ));
    let mut shown = 0usize;
    for line in stdout.lines().chain(stderr.lines()) {
        if shown >= max {
            break;
        }
        items.push((line.to_string(), body));
        shown += 1;
    }
    let total = stdout.lines().count() + stderr.lines().count();
    if total > max {
        items.push((format!("… +{} more lines", total - max), more));
    }
    if total == 0 {
        items.push(("no output".to_string(), more));
    }
    items
}

/// Render result/output logical lines under a gutter, wrapped to width.
pub(super) fn result_block(items: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    result_block_inner(items, width, false)
}

/// Like `result_block` but preserves each line verbatim (indentation and runs of
/// spaces) instead of word-wrapping — used for code/diff previews where leading
/// whitespace is meaningful.
pub(super) fn result_block_verbatim(
    items: Vec<(String, Style)>,
    width: usize,
) -> Vec<Line<'static>> {
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
            // Plain indent — avoid ↳ (reads like an Enter key and breaks alignment
            // in some fonts). First line gets a light bar; wraps stay padded.
            let prefix = if first {
                format!("{}│ ", " ".repeat(AGENT))
            } else {
                " ".repeat(AGENT + 2)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(faint()).add_modifier(Modifier::DIM),
                ),
                Span::styled(seg, style.add_modifier(Modifier::DIM)),
            ]));
            first = false;
        }
    }
    lines
}
