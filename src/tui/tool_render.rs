use ratatui::prelude::*;
use serde_json::Value;
use super::markdown::{wrap_code_line, wrap_one};
use super::theme::*;
use super::fmt_si;

const AGENT: usize = 3;

/// Tools whose collapsed row is incomplete without an expanded body.
pub(super) fn tool_is_expandable(tool_name: &str, arguments: &Value, result: Option<&Value>) -> bool {
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
            let label = arg("label");
            let cmd = arg("command");
            let text = if !label.is_empty() { label } else { cmd };
            ("Bash".into(), ellipsize_one_line(&text, 90))
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
