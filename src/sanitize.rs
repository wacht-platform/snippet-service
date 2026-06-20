//! User-facing text hygiene. Ported from wacht `executor/agent_loop/response.rs`
//! (`sanitize_user_facing_message` and friends). Some weaker models leak their
//! chain-of-thought, or render tool calls as prose, into the visible reply. This
//! strips leading `[time]` prefixes and drops a message that is really an
//! internal reasoning dump or a hallucinated tool-call render.

/// Clean a candidate user-visible message. Returns `None` when the text is empty
/// after stripping or is clearly not meant for the user (reasoning dump /
/// hallucinated tool render) — the caller should then show nothing.
pub fn clean_user_text(raw: &str) -> Option<String> {
    let cleaned = strip_leading_time_prefix(raw).trim();
    if cleaned.is_empty()
        || looks_like_internal_reasoning_dump(cleaned)
        || looks_like_hallucinated_tool_render(cleaned)
    {
        return None;
    }
    Some(cleaned.to_string())
}

fn strip_leading_time_prefix(text: &str) -> &str {
    let mut current = text.trim_start();
    for _ in 0..3 {
        if !current.starts_with('[') {
            return current;
        }
        let close = match current[1..].find(']') {
            Some(idx) => idx + 1,
            None => return current,
        };
        if close > 60 {
            return current;
        }
        let inner = current[1..close].trim();
        if !looks_like_time_token(inner) {
            return current;
        }
        current = current[close + 1..].trim_start();
    }
    current
}

fn looks_like_time_token(s: &str) -> bool {
    if s == "just now" {
        return true;
    }
    if let Some(rest) = s.strip_prefix("at ") {
        return looks_like_absolute_time_token(rest);
    }
    if let Some(rest) = s.strip_prefix("in ") {
        return is_time_unit_token(rest);
    }
    if let Some(rest) = s.strip_suffix(" ago") {
        return is_time_unit_token(rest);
    }
    looks_like_absolute_time_token(s)
}

fn looks_like_absolute_time_token(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 10
        && bytes[0..4].iter().all(|b| b.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[7] == b'-'
}

fn is_time_unit_token(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let last = bytes[bytes.len() - 1];
    if !matches!(last, b's' | b'm' | b'h' | b'd') {
        return false;
    }
    let digits = &bytes[..bytes.len() - 1];
    !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit())
}

/// The model narrated tool calls as prose (`+ bash:`, `Action:`, repeated
/// blocks) instead of emitting real calls.
fn looks_like_hallucinated_tool_render(text: &str) -> bool {
    const PSEUDO_CALL_MARKERS: [&str; 14] = [
        "+ bash:",
        "+ read_file:",
        "+ write_file:",
        "+ edit_file:",
        "+ append_file:",
        "+ replace_file_content:",
        "+ list_files:",
        "+ search_files:",
        "+ search_content:",
        "+ view_outline:",
        "+ note:",
        "[note:",
        "Action: ",
        "Action Input:",
    ];
    let pseudo_count: usize = PSEUDO_CALL_MARKERS
        .iter()
        .map(|m| text.matches(m).count())
        .sum();
    let separator_lines = text.lines().filter(|l| l.trim() == "---").count();
    if pseudo_count >= 2 && separator_lines >= 2 {
        return true;
    }
    let mut block_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for chunk in text.split("\n\n") {
        let trimmed = chunk.trim();
        if trimmed.len() >= 60 {
            *block_counts.entry(trimmed).or_insert(0) += 1;
        }
    }
    block_counts.values().any(|c| *c >= 3)
}

/// The model leaked its internal reasoning into the visible reply.
fn looks_like_internal_reasoning_dump(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: [&str; 4] = [
        "the user is asking",
        "i need to perform",
        "let me think step by step",
        "internal reasoning",
    ];
    let marker_hits = MARKERS.iter().filter(|m| lower.contains(**m)).count();

    let numbered_lines = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
            digits > 0 && trimmed.chars().nth(digits) == Some('.')
        })
        .count();

    marker_hits >= 2 || (marker_hits >= 1 && numbered_lines >= 3)
}
