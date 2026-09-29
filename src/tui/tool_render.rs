use ratatui::prelude::*;
use serde_json::Value;
use super::markdown::{wrap_code_line, wrap_one};
use super::theme::*;
use super::fmt_si;

const AGENT: usize = 3;

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
        "change_files" => {
            let changes = arguments
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let first = changes
                .first()
                .and_then(|c| c.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let shown = match changes.len() {
                0 | 1 => first,
                n => format!("{first} (+{} more)", n - 1),
            };
            ("Change".into(), shown)
        }
        "view_image" => ("View".into(), arg("path")),
        "web_search" => ("Web".into(), arg("query")),
        "web_read" => ("Fetch".into(), arg("url")),
        // A labelled command reads as its label; a bare one as Bash(command).
        "bash" => {
            let label = arg("label");
            if label.trim().is_empty() {
                ("Bash".into(), ellipsize_one_line(&arg("command"), 90))
            } else {
                (ellipsize_one_line(label.trim(), 90), String::new())
            }
        }
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
        "change_files" => vec![(str_field("summary").to_string(), subtle())],
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
    let success = data.get("exit_code").and_then(Value::as_i64) == Some(0);
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
        _ => return tool_result_lines(tool_name, result, width),
    }

    let items: Vec<(String, Style)> = items.into_iter().filter(|(s, _)| !s.is_empty()).collect();
    if items.is_empty() {
        return tool_result_lines(tool_name, result, width);
    }
    if tool_name == "bash" {
        result_block_verbatim(items, width)
    } else {
        result_block(items, width)
    }
}

fn bash_result_items_expanded(data: &Value, max: usize) -> Vec<(String, Style)> {
    let success = data.get("exit_code").and_then(Value::as_i64) == Some(0);
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

pub(super) struct RunStep {
    pub(super) tool: String,
    pub(super) args: Value,
    pub(super) result: Option<Value>,
}

impl RunStep {
    pub(super) fn failed(&self) -> bool {
        self.result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            == Some("error")
    }
}

pub(super) struct FileChange {
    pub(super) path: String,
    pub(super) added: usize,
    pub(super) removed: usize,
    pub(super) deleted: bool,
}

fn line_count(v: Option<&Value>) -> usize {
    let s = v.and_then(Value::as_str).unwrap_or("").trim_end();
    if s.is_empty() { 0 } else { s.lines().count() }
}

pub(super) fn file_changes(steps: &[RunStep]) -> Vec<FileChange> {
    let mut out: Vec<FileChange> = Vec::new();
    for step in steps.iter().filter(|s| s.tool == "change_files" && !s.failed()) {
        let changes = step.args.get("changes").and_then(Value::as_array);
        for c in changes.into_iter().flatten() {
            let action = c.get("action").and_then(Value::as_str).unwrap_or("");
            let path = c.get("path").and_then(Value::as_str).unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let path = if action == "move" {
                format!("{path} → {}", c.get("to").and_then(Value::as_str).unwrap_or(""))
            } else {
                path.to_string()
            };
            let idx = match out.iter().position(|f| f.path == path) {
                Some(i) => i,
                None => {
                    out.push(FileChange { path, added: 0, removed: 0, deleted: false });
                    out.len() - 1
                }
            };
            let entry = &mut out[idx];
            match action {
                "replace" => {
                    entry.added += line_count(c.get("with"));
                    entry.removed += line_count(c.get("find"));
                }
                "create" => entry.added += line_count(c.get("content")),
                "delete" => entry.deleted = true,
                _ => {}
            }
        }
    }
    out
}

pub(super) fn activity_summary(steps: &[RunStep]) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
    let count = |tools: &[&str]| steps.iter().filter(|s| tools.contains(&s.tool.as_str())).count();
    let change_steps = count(&["change_files"]);
    let changed = file_changes(steps).len();
    let runs = count(&["bash", "manage_process"]);
    let searches = count(&["web_search", "web_read"]);
    let mut groups: Vec<(usize, String)> = Vec::new();
    if change_steps > 0 {
        groups.push((change_steps, format!("changed {changed} {}", plural(changed, "file", "files"))));
    }
    if runs > 0 {
        groups.push((runs, format!("ran {runs} {}", plural(runs, "command", "commands"))));
    }
    if searches > 0 {
        groups.push((searches, format!("{searches} web {}", plural(searches, "search", "searches"))));
    }
    if groups.is_empty() {
        return format!("{} steps", steps.len());
    }
    let rest = steps.len() - groups.iter().map(|g| g.0).sum::<usize>();
    let mut parts: Vec<String> = groups.into_iter().map(|g| g.1).collect();
    if rest > 0 {
        parts.push(format!("{rest} more"));
    }
    let sentence = parts.join(", ");
    let mut chars = sentence.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => sentence,
    }
}

pub(super) fn run_summary_lines(steps: &[RunStep], width: usize) -> Vec<Line<'static>> {
    let failed = steps.iter().filter(|s| s.failed()).count();
    let dot = if failed > 0 { danger() } else { success() };
    let mut head = vec![
        Span::styled("● ", Style::default().fg(dot)),
        Span::styled(activity_summary(steps), Style::default().fg(soft())),
    ];
    if failed > 0 {
        head.push(Span::styled(format!(" · {failed} failed"), Style::default().fg(danger())));
    }
    let mut lines = vec![Line::from(head)];
    let changes = file_changes(steps);
    let budget = width.saturating_sub(16).max(10);
    for change in changes.iter().take(5) {
        let mut path = change.path.clone();
        if path.chars().count() > budget {
            let tail: String = path.chars().rev().take(budget - 1).collect::<Vec<_>>().into_iter().rev().collect();
            path = format!("…{tail}");
        }
        let mut row = vec![
            Span::styled("  └ ", Style::default().fg(faint())),
            Span::styled(path, Style::default().fg(muted())),
        ];
        if change.deleted {
            row.push(Span::styled("  deleted", Style::default().fg(danger())));
        } else if change.added + change.removed > 0 {
            row.push(Span::styled(format!("  +{}", change.added), Style::default().fg(success())));
            row.push(Span::styled(format!(" −{}", change.removed), Style::default().fg(danger())));
        }
        lines.push(Line::from(row));
    }
    if changes.len() > 5 {
        lines.push(Line::from(Span::styled(
            format!("    … {} more files", changes.len() - 5),
            Style::default().fg(faint()),
        )));
    }
    lines
}
