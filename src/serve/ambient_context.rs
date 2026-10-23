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

pub(crate) fn mission_control(d: &Shared) -> Value {
    let now = chrono::Utc::now();
    let rows = d
        .store
        .load_recent_conversation(crate::mission_control::SESSION_ID, 24)
        .unwrap_or_default();
    let mut chat: Vec<Value> = Vec::new();
    let mut budget = 2000usize;
    for row in rows.iter().rev() {
        if chat.len() >= 6 || budget < 80 {
            break;
        }
        let (who, via, raw, limit, end) = match &row.message {
            crate::llm::HarnessMessage::User { content } => {
                if crate::harness::is_daemon_envelope(content) {
                    continue;
                }
                match super::ambient_watch::inner_turn(content) {
                    Some(inner) => ("user", "spoken", inner, 240, false),
                    None => ("user", "typed", content.clone(), 240, false),
                }
            }
            crate::llm::HarnessMessage::Assistant { content, .. } => {
                if content.trim().is_empty() {
                    continue;
                }
                ("assistant", "chat", content.clone(), 300, true)
            }
            _ => continue,
        };
        let text = redact(&clip(&plain_text(&raw), limit, end));
        if text.is_empty() {
            continue;
        }
        let secs = chrono::DateTime::parse_from_rfc3339(&row.created_at)
            .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(3600);
        budget = budget.saturating_sub(text.len());
        chat.push(json!({"who": who, "via": via, "when": bucket_ago(secs), "text": text}));
    }
    chat.reverse();
    let pending = crate::session::state_path_for_id(crate::mission_control::SESSION_ID)
        .and_then(|p| crate::session::read_session_state(&p))
        .and_then(|s| s.pending_question)
        .map(|q| redact(&clip(&super::autonomy::question_text(&q), 300, false)));
    json!({
        "waiting_for_user": pending.is_some(),
        "pending_question": pending,
        "recent_chat": chat,
    })
}
