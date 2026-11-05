//! Stops Mission Control forwarding a live conversation to workers: while the
//! user is talking or on a call, work goes out only on their own quoted words.

use crate::tools::{ToolContext, ToolError};

const VOICE_OPEN_SECS: i64 = 300;
const WORDS_WINDOW_SECS: i64 = 900;

pub const GO_AHEAD_DESCRIPTION: &str = "Needed while a voice conversation is open or the user is on a call: their exact words that make this go (a complete request, or a yes to your plan), which are checked against what they said. A thought, an opinion, a correction or a question asking your view is not a go-ahead. For work that doesn't come from this conversation, `standing: <why>`.";

pub fn check(ctx: &ToolContext, go_ahead: Option<&str>) -> Result<(), ToolError> {
    if ctx.mission_control_root().is_none() {
        return Ok(());
    }
    let on_call = crate::ambient::call_quiet();
    let voice_open = crate::ambient::seconds_since_user_turn().is_some_and(|s| s < VOICE_OPEN_SECS);
    if !on_call && !voice_open {
        return Ok(());
    }
    let go_ahead = go_ahead.unwrap_or("").trim();
    if go_ahead.strip_prefix("standing:").is_some_and(|why| !why.trim().is_empty()) {
        return Ok(());
    }
    let why = if on_call { "The user is on a call" } else { "A voice conversation is open and they may still be working it out" };
    if go_ahead.is_empty() {
        return Err(ToolError::msg(format!(
            "{why}, so nothing goes to a worker yet. Reply to what they said, gather what they want changed and play it back with your plan. Call again with go_ahead set to their exact words once they've made a complete request or said yes. Work that doesn't come from this conversation (a goal's next step, an answer to a worker) takes go_ahead \"standing: <why>\"."
        )));
    }
    if said_recently(ctx, go_ahead) {
        return Ok(());
    }
    Err(ToolError::msg(format!(
        "\"{go_ahead}\" isn't in what the user said in the last 15 minutes. Quote their exact words, or ask them."
    )))
}

fn normalise(text: &str) -> String {
    let kept: String = text.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn said_recently(ctx: &ToolContext, words: &str) -> bool {
    let wanted = normalise(words);
    if wanted.is_empty() {
        return false;
    }
    let Ok(store) = ctx.store() else {
        return false;
    };
    let rows: Vec<(String, String)> = store
        .with_connection(|c| {
            let mut q = c.prepare("SELECT created_at, payload_json FROM session_events WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 300")?;
            let rows = q.query_map([crate::mission_control::SESSION_ID], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    rows.iter().any(|(at, payload)| {
        let recent = chrono::DateTime::parse_from_rfc3339(at).is_ok_and(|t| now - t.timestamp() < WORDS_WINDOW_SECS);
        recent && user_text(payload).is_some_and(|text| normalise(&text).contains(&wanted))
    })
}

fn user_text(payload: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(payload).ok()?;
    if !matches!(event["kind"].as_str()?, "user_input" | "steer") {
        return None;
    }
    let text = event["text"].as_str()?.trim();
    (!text.starts_with('[')).then(|| text.to_string())
}
