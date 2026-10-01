use super::markdown::*;
use super::theme::*;
use super::tool_render::*;
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
    let mut prev_plan = false;

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

        if matches!(event, HarnessEvent::ToolCall { .. }) {
            let mut run: Vec<RunStep> = Vec::new();
            let mut cur = Some(event);
            while let Some(HarnessEvent::ToolCall { tool_name, arguments }) = cur {
                let mut result = None;
                if let Some(HarnessEvent::ToolResult { tool_name: rn, result: r }) = events.peek() {
                    if rn == tool_name {
                        result = Some(r.clone());
                        events.next();
                    }
                }
                if !HIDDEN_TOOL_ROWS.contains(&tool_name.as_str()) {
                    run.push(RunStep { tool: tool_name.clone(), args: arguments.clone(), result });
                }
                cur = if matches!(events.peek(), Some(HarnessEvent::ToolCall { .. })) {
                    events.next()
                } else {
                    None
                };
            }
            if run.is_empty() {
                continue;
            }

            set_speaker(&mut lines, &mut speaker, &mut tag_pending, true);
            if !prev_tool_row
                && !lines.is_empty()
                && lines.last().map_or(true, |l| !l.spans.is_empty())
            {
                lines.push(Line::from(""));
            }

            let running = state.status == HarnessStatus::Running;
            let live = running && events.peek().is_none();
            let expanded = app.tools_expanded;
            if run.len() > 1 && !live && !expanded {
                lines.extend(run_summary_lines(&run, content_w));
            } else {
                for step in &run {
                    let status = match &step.result {
                        _ if step.failed() => ToolRowStatus::Failed,
                        None if live => ToolRowStatus::Running,
                        _ => ToolRowStatus::Done,
                    };
                    lines.extend(tool_call_head_lines_status(&step.tool, &step.args, content_w, status));
                    if expanded {
                        lines.extend(tool_call_preview(&step.tool, &step.args, content_w));
                        if let Some(result) = &step.result {
                            lines.extend(tool_result_lines_expanded(&step.tool, result, content_w));
                        }
                    } else if let (true, Some(result)) = (step.failed(), &step.result) {
                        lines.extend(tool_result_lines(&step.tool, result, content_w));
                    }
                }
            }
            prev_tool_row = true;
            prev_plan = false;
            continue;
        }

        // Lane lifecycle and reports live in the dedicated lanes screen; keep the
        // conversation canvas free of duplicate lane status and report previews.
        if matches!(
            event,
            HarnessEvent::LaneSpawned { .. }
                | HarnessEvent::LaneCancelled { .. }
                | HarnessEvent::LaneCompleted { .. }
        ) {
            continue;
        }

        let pending_question = matches!(event, HarnessEvent::UserQuestion { .. })
            && state.status == HarnessStatus::WaitingForInput
            && events.peek().is_none();
        if pending_question {
            continue;
        }
        if let Some(card) = event_card(event) {
            if lines.last().is_some_and(|l| !l.spans.is_empty()) {
                lines.push(Line::from(""));
            }
            lines.extend(card_lines(&card, width));
            speaker = None;
            tag_pending = false;
            prev_tool_row = false;
            prev_plan = true;
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

        if (prev_tool_row || prev_plan)
            && !lines.is_empty()
            && lines.last().map_or(true, |l| !l.spans.is_empty())
        {
            lines.push(Line::from(""));
        }
        push_tagged(&mut lines, rendered, !is_user, &mut tag_pending);
        prev_tool_row = false;
        prev_plan = matches!(event, HarnessEvent::PlanUpdated { .. });
    }
    for pending in &app.pending_steers {
        if pending.trim().is_empty() {
            continue;
        }
        set_speaker(&mut lines, &mut speaker, &mut tag_pending, false);
        if prev_tool_row && !lines.is_empty() && lines.last().map_or(true, |l| !l.spans.is_empty())
        {
            lines.push(Line::from(""));
        }
        push_tagged(
            &mut lines,
            event_lines(
                &HarnessEvent::Steer {
                    text: pending.clone(),
                },
                content_w,
            ),
            false,
            &mut tag_pending,
        );
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
                Span::styled(
                    match state.events.last() {
                        _ if state.status != HarnessStatus::Running => "delegated work running…",
                        Some(HarnessEvent::ToolCall { .. }) => "working…",
                        _ if !live.is_empty() => "writing…",
                        _ => "thinking…",
                    },
                    subtle(),
                ),
            ]));
        }
    }
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
            | HarnessEvent::PlanUpdated { .. }
            | HarnessEvent::FilePresented { .. }
            | HarnessEvent::UserQuestion { .. }
            | HarnessEvent::ApprovalRequest { .. }
            | HarnessEvent::LaneSpawned { .. }
            | HarnessEvent::LaneCancelled { .. }
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
    if let Some(card) = event_card(event) {
        return card_lines(&card, width);
    }
    match event {
        HarnessEvent::UserInput { text } => user_lines(text, width),
        // Steers are still your words mid-run — same column as user messages, no
        // extra ↳ gutter (that stacked with the speaker tag and broke indent).
        HarnessEvent::Steer { text } => steer_lines(text, width),
        HarnessEvent::AssistantText { text } => {
            indent_block(render_prose(text, width.saturating_sub(SPINE)), SPINE)
        }
        HarnessEvent::Retired => Vec::new(),
        HarnessEvent::PlanUpdated { steps, explanation } => {
            let mut lines = Vec::new();
            if let Some(why) = explanation {
                lines.extend(marker_block("·", faint(), why, width));
            }
            for step in steps {
                let (mark, color) = match step.status {
                    PlanStatus::Done => ("✓", success()),
                    PlanStatus::InProgress => ("▸", accent()),
                    PlanStatus::Pending => ("○", muted()),
                };
                let mut block = marker_block(mark, color, &step.step, width);
                if step.status == PlanStatus::Done {
                    for line in &mut block {
                        for span in line.spans.iter_mut().skip(1) {
                            span.style = span.style.fg(muted());
                        }
                    }
                }
                lines.extend(block);
            }
            lines
        }
        // A direct message to or from another agent. Both directions render, so
        // the exchange reads as one conversation rather than a reply appearing
        // with nothing before it.
        HarnessEvent::AgentMessage {
            agent_id,
            body,
            outbound,
        } => marker_block(
            if *outbound { "→" } else { "←" },
            if *outbound { muted() } else { lane() },
            &format!(
                "{} {agent_id}: {body}",
                if *outbound { "to" } else { "from" }
            ),
            width,
        ),
        HarnessEvent::FilePresented { path, caption } => {
            present_file_lines(path, caption.as_deref(), width)
        }
        // Work routed on this session's behalf by someone else (usually the
        // user). A quiet aside: it is already dispatched, so it is a record of
        // what went out, not a decision to make.
        HarnessEvent::TaskDispatched {
            task_id,
            title,
            session_id,
            by,
        } => marker_block(
            "→",
            muted(),
            &format!("{by} dispatched: {title}\ntask {task_id} → {session_id}"),
            width,
        ),
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
        HarnessEvent::LaneCancelled { title, reason, .. } => marker_block(
            "×",
            muted(),
            &format!("cancelled: {title} — {reason}"),
            width,
        ),
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
    let cleaned = strip_attachment_markers(&strip_pasted_blocks(text));
    let (prose, audio) = split_audio_sections(&cleaned);
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
    lines.extend(attachment_lines(text, width));
    if lines.is_empty() {
        // Pure attachment / empty after strip — keep a single blank body so the
        // speaker tag still has a row.
        lines.push(Line::from(vec![Span::raw(" ".repeat(SPINE))]));
    }
    lines
}

/// Mid-run steer: same column as user text, quiet label, no enter-arrow glyph.
fn steer_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let cleaned = strip_attachment_markers(&strip_pasted_blocks(text));
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
    lines.extend(attachment_lines(text, width));
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
static PASTED_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\[pasted text — \d+ lines?\]\n([\s\S]*?)\n\[/pasted text\]")
        .expect("valid regex")
});
static ATTACHED_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\[attached (image|file) — ([^\]/]*)(/[^\]]+)\]").expect("valid regex")
});

fn strip_pasted_blocks(text: &str) -> String {
    PASTED_RE.replace_all(text, "").into_owned()
}

/// One row per attachment (its kind and file name) and one per pasted block
/// (its size and first line), in the content column under the message.
fn attachment_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let label = Style::default().fg(muted());
    let name = Style::default().fg(self::text());
    let mut rows: Vec<(String, String)> = Vec::new();
    for caps in ATTACHED_RE.captures_iter(text) {
        let path = caps[3].trim();
        let file = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string();
        let lower = file.to_lowercase();
        let kind = if &caps[1] == "image" {
            "image"
        } else if caps[2].contains("pasted text") {
            "pasted"
        } else if [".m4a", ".mp3", ".wav", ".ogg", ".opus", ".webm", ".aac", ".flac"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            "audio"
        } else {
            "file"
        };
        rows.push((kind.to_string(), file));
    }
    for caps in PASTED_RE.captures_iter(text) {
        let body = &caps[1];
        let count = body.lines().count().max(1);
        let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        rows.push((
            "pasted".to_string(),
            format!("{count} line{} · {first}", if count == 1 { "" } else { "s" }),
        ));
    }
    let room = width.saturating_sub(SPINE + 8).max(8);
    rows.into_iter()
        .map(|(kind, text)| {
            let shown: String = if text.chars().count() > room {
                text.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
            } else {
                text
            };
            Line::from(vec![
                Span::raw(" ".repeat(SPINE)),
                Span::styled(format!("{kind:<7} "), label),
                Span::styled(shown, name),
            ])
        })
        .collect()
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
        LaneStatus::Cancelled => ("cancelled", muted()),
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
    let verb_style = if arg.trim().is_empty() {
        Style::default().fg(soft())
    } else {
        Style::default().fg(text()).add_modifier(Modifier::BOLD)
    };
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
    let path_style = Style::default().fg(code()).add_modifier(Modifier::ITALIC);
    let green = Style::default().fg(success());
    let red = Style::default().fg(danger());
    const MAX: usize = 6;

    let mut items: Vec<(String, Style)> = Vec::new();

    match tool_name {
        "change_files" => {
            let changes = arguments
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (i, change) in changes.iter().enumerate() {
                let field = |key: &str| change.get(key).and_then(Value::as_str).unwrap_or("");
                if i > 0 {
                    items.push(("".to_string(), subtle()));
                }
                let action = field("action");
                let target = match action {
                    "move" => format!("→ move {} → {}", field("path"), field("to")),
                    _ => format!("→ {action} {}", field("path")),
                };
                items.push((target, path_style));
                let mut push_lines = |text: &str, style: Style| {
                    let total = text.lines().count();
                    for line in text.lines().take(MAX) {
                        items.push((format!("  {line}"), style));
                    }
                    if total > MAX {
                        items.push((format!("  … +{} more", total - MAX), subtle()));
                    }
                };
                match action {
                    "replace" => {
                        push_lines(field("find"), red);
                        push_lines(field("with"), green);
                    }
                    "create" => push_lines(field("content"), green),
                    _ => {}
                }
            }
        }
        "bash" => {
            let label = arg("label");
            if !label.is_empty() {
                items.push((format!("→ {label}"), path_style));
                items.push(("".to_string(), subtle()));
            }
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
        _ => return Vec::new(),
    }
    result_block_verbatim(items, width)
}

