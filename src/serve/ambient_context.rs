use super::*;

use serde_json::{Value, json};

pub(crate) fn bucket_ago(secs: i64) -> &'static str {
    match secs {
        s if s < 20 => "just now",
        s if s < 60 => "under a minute ago",
        s if s < 300 => "a few minutes ago",
        _ => "over 5 minutes ago",
    }
}

fn redact(text: &str) -> String {
    let vault = crate::vault::Vault::load();
    let scrubbed = vault.scrub_str(text);
    scrubbed
        .split_whitespace()
        .map(|w| {
            let long = w.len() >= 24 && w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic());
            let known = ["sk-", "ghp_", "gho_", "xox", "AIza", "AKIA", "eyJ"].iter().any(|p| w.starts_with(p));
            if long || known { "[redacted]" } else { w }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn clip(text: &str, max: usize, keep_end: bool) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if keep_end {
        let tail: String = text.chars().skip(count - max).collect();
        format!("…{tail}")
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

fn plain_text(content: &str) -> String {
    let cut = content.find("[attached image").unwrap_or(content.len());
    let mut out = String::new();
    let mut fenced = false;
    for line in content[..cut].lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn summarize_tool(tc: &crate::llm::ToolCallRecord) -> Option<String> {
    let s = |k: &str| tc.arguments.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let summary = match tc.name.as_str() {
        "speak" => return None,
        "create_mission_task" | "update_mission_task" => {
            format!("{}: {}", tc.name, clip(s("title").or_else(|| s("task_id")).unwrap_or("task"), 80, false))
        }
        "inspect_session" | "stop_session" | "create_mission_session" => {
            format!("{}: {}", tc.name, clip(s("session_id").or_else(|| s("title")).unwrap_or("session"), 80, false))
        }
        "ambient_mode" => format!("ambient_mode: {}", s("mode").unwrap_or("")),
        "ambient_expect" => format!("ambient_expect: {} ({})", s("kind").unwrap_or(""), clip(s("about").unwrap_or(""), 60, false)),
        "schedule_wake" => format!("schedule_wake: {}", clip(s("note").unwrap_or(""), 70, false)),
        "ping_user" => format!("ping_user: {}", clip(s("title").unwrap_or(""), 70, false)),
        "list_mission_tasks" | "list_sessions" | "ambient_search" => tc.name.clone(),
        other => format!("{other}"),
    };
    Some(redact(&summary))
}

fn is_ignored_ambient_dismissal(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    (lower.contains("staying quiet") || lower.contains("no reply needed"))
        && (lower.contains("out-of-context")
            || lower.contains("not meant for me")
            || lower.contains("not addressed to me")
            || lower.contains("not for me"))
}

pub(crate) fn mission_control(d: &Shared) -> Value {
    let now = chrono::Utc::now();
    let rows = d
        .store
        .load_recent_conversation(crate::mission_control::SESSION_ID, 64)
        .unwrap_or_default();
    let mut chat: Vec<Value> = Vec::new();
    let mut tools: Vec<Value> = Vec::new();
    let mut budget = 2400usize;
    let mut skip_prev_user_ambient = false;

    for (idx, row) in rows.iter().enumerate().rev() {
        if chat.len() >= 8 && tools.len() >= 5 {
            break;
        }
        let secs = chrono::DateTime::parse_from_rfc3339(&row.created_at)
            .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(3600);
        let when = bucket_ago(secs);
        match &row.message {
            crate::llm::HarnessMessage::User { content } => {
                if skip_prev_user_ambient && super::ambient_watch::inner_turn(content).is_some() {
                    skip_prev_user_ambient = false;
                    continue;
                }
                skip_prev_user_ambient = false;
                if chat.len() >= 8 || budget < 80 || crate::harness::is_daemon_envelope(content) {
                    continue;
                }
                let (via, raw) = match super::ambient_watch::inner_turn(content) {
                    Some(inner) => ("spoken", inner),
                    None => ("typed", content.clone()),
                };
                let text = redact(&clip(&plain_text(&raw), 240, false));
                if !text.is_empty() {
                    budget = budget.saturating_sub(text.len());
                    chat.push(json!({"who": "user", "via": via, "when": when, "text": text}));
                }
            }
            crate::llm::HarnessMessage::Assistant { content, tool_calls } => {
                // Skip internal assistant replies to daemon wakes/board envelopes when no speak tool was called
                let prev_user_is_daemon = rows[..idx]
                    .iter()
                    .rev()
                    .find_map(|r| match &r.message {
                        crate::llm::HarnessMessage::User { content } => Some(crate::harness::is_daemon_envelope(content)),
                        _ => None,
                    })
                    .unwrap_or(false);
                let has_speak = tool_calls.iter().any(|tc| tc.name == "speak");
                if is_ignored_ambient_dismissal(content) && !has_speak {
                    skip_prev_user_ambient = true;
                    continue;
                }
                if prev_user_is_daemon && !has_speak {
                    continue;
                }
                for tc in tool_calls.iter().rev() {
                    if tc.name == "speak" && chat.len() < 8 && budget >= 80 {
                        if let Some(spoken) = tc.arguments.get("text").and_then(Value::as_str) {
                            let text = redact(&clip(spoken, 260, false));
                            if !text.is_empty() {
                                budget = budget.saturating_sub(text.len());
                                chat.push(json!({"who": "assistant", "via": "spoken", "when": when, "text": text}));
                            }
                        }
                    } else if tools.len() < 5 {
                        if let Some(summary) = summarize_tool(tc) {
                            tools.push(json!({"when": when, "action": summary}));
                        }
                    }
                }
                if !content.trim().is_empty() && !has_speak && chat.len() < 8 && budget >= 80 {
                    let text = redact(&clip(&plain_text(content), 280, true));
                    if !text.is_empty() {
                        budget = budget.saturating_sub(text.len());
                        chat.push(json!({"who": "assistant", "via": "chat", "when": when, "text": text}));
                    }
                }
            }
            _ => continue,
        }
    }
    chat.reverse();
    tools.reverse();
    let pending = crate::session::state_path_for_id(crate::mission_control::SESSION_ID)
        .and_then(|p| crate::session::read_session_state(&p))
        .and_then(|s| s.pending_question)
        .map(|q| redact(&clip(&super::autonomy::question_text(&q), 300, false)));
    json!({
        "waiting_for_user": pending.is_some(),
        "pending_question": pending,
        "recent_chat": chat,
        "recent_tools": tools,
    })
}
