use super::*;

// --- Agentic compaction: the living "context table" ---

/// (name, description, required) — the sections the summarizer maintains.
pub(super) const SUMMARY_SECTIONS: &[(&str, &str, bool)] = &[
    (
        "objective",
        "what the user ultimately wants, and for whom",
        true,
    ),
    (
        "state",
        "where things stand now: files changed, what works/doesn't, plus exact paths/IDs/values worth keeping verbatim",
        true,
    ),
    (
        "task_progress",
        "for every task described as started, in progress, or underway: state what is completed, what remains, and a measurable amount or percentage when available; never say only that it started",
        true,
    ),
    (
        "actions",
        "what was actually done, in order — the condensed trail; include completion status for each started task",
        false,
    ),
    (
        "decisions",
        "key decisions and user corrections, verbatim where wording matters",
        false,
    ),
    (
        "open_issues",
        "exact error strings and genuinely unresolved/open work",
        false,
    ),
    ("next_steps", "what to do next", false),
];

pub(super) const MEMORY_REFLECTOR_SYSTEM: &str = r#"# memory_reflector
[identity]
role = "worker that curates a coding agent's PERSISTENT memory — per-workspace facts/playbooks AND a global library of reusable patterns"
input = "each turn: the workspace path, a compacted table of the session that just ran, the current memory index, the existing entry ids, the current global patterns, your last tool result, and the turn counter"
purpose = "carry forward what helps FUTURE sessions — in THIS folder (facts/playbooks) and in ANY project (reusable patterns)"

[what_to_keep]
durable = "workspace scope: stable facts (architecture, where things live, conventions), pointers to key files, and how-to PLAYBOOKS for recurring tasks here (steps that worked + gotchas)"
patterns = "GLOBAL scope: a generalizable TECHNIQUE this session demonstrated that transfers to any project — one line, situation → approach → why. APPEND it with memory_pattern; skip when an existing pattern already covers it. Extract one whenever the session showed a technique worth reapplying anywhere, not just a project fact."
learning = "when this session revealed a better way or a pitfall, fold it into the relevant playbook (workspace) or pattern (global) so next time is faster"
skip = "ephemeral task state, one-off details, and anything already obvious from the code — that belongs in the session table, not here"

[how]
entries = "memory_write(id, content) stores a full note under a short kebab-case id; prefer UPDATING an existing entry over creating a near-duplicate (memory_read it first)"
index = "memory_index(content) REPLACES the always-loaded index — keep it lean: one short line per entry (label, one-line summary, id). It must fit its budget; oversize writes are rejected, so compress"
evidence = "exact paths, commands, and IDs verbatim; no speculation, no padding"

[finalize]
write_once = "write each entry ONCE. Do NOT re-save or 'polish' an entry you already wrote in this pass — it changes little and just burns turns. Aim for 1–2 writes total (an entry, then the index), then finalize."
bias_to_capture = "if the session did REAL work (edits, debugging, a build, multi-step task), write at least one entry before finalizing — prefer UPDATING an existing id that matches the table over a new near-duplicate. Finalizing empty is only correct when the session was genuinely trivial. User lasting prefs → note them in a playbook line if memory_rule is unavailable here."
when = "finalize as soon as the index and entries reflect the durable procedures/facts from this session — usually within 1–2 writes"
how = "call finalize (one tool call per turn)""#;

pub(super) fn memory_reflector_tools() -> Vec<crate::llm::NativeToolDefinition> {
    use crate::llm::NativeToolDefinition;
    let id_schema = json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "kebab-case entry id" } },
        "required": ["id"],
        "additionalProperties": false
    });
    vec![
        NativeToolDefinition {
            name: "memory_read".to_string(),
            description: "Read the full content of an existing entry by id.".to_string(),
            input_schema: id_schema.clone(),
        },
        NativeToolDefinition {
            name: "memory_write".to_string(),
            description: "Create or replace an entry (durable fact, pointer, or how-to playbook) under a short kebab-case id.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "kebab-case entry id" },
                    "content": { "type": "string" }
                },
                "required": ["id", "content"],
                "additionalProperties": false
            }),
        },
        NativeToolDefinition {
            name: "memory_index".to_string(),
            description: "Replace the always-loaded index — one short line per entry (label, summary, id). Must fit the budget.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "content": { "type": "string" } },
                "required": ["content"],
                "additionalProperties": false
            }),
        },
        NativeToolDefinition {
            name: "memory_delete".to_string(),
            description: "Delete an entry by id (also drop its line from the index).".to_string(),
            input_schema: id_schema,
        },
        NativeToolDefinition {
            name: "memory_pattern".to_string(),
            description: "APPEND one GLOBAL reusable pattern: a generalizable technique (one line: situation → approach → why) that transfers to ANY project — not a fact about this workspace. Skip it if an existing pattern already covers the technique.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "content": { "type": "string", "description": "one pattern line: situation → approach → why" } },
                "required": ["content"],
                "additionalProperties": false
            }),
        },
        NativeToolDefinition {
            name: "finalize".to_string(),
            description: "Finish — memory reflects all durable learnings from this session.".to_string(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
    ]
}

pub(super) const SUMMARIZER_SYSTEM: &str = r#"# compaction_summarizer
[identity]
role = "you compress a coding agent's whole conversation into ONE dense context table that REPLACES the raw history"
stakes = "the raw messages are then discarded — this table is all that survives. Anything you leave out is lost forever; anything you pad is re-sent on every future turn and wastes tokens. Maximize signal per token."

[preserve]  # carry these forward — verbatim where the exact value/wording matters
goal = "the user's actual objective and any hard constraints or preferences they stated"
state = "the CURRENT state: which files were created/changed, what works, what's broken or unverified"
specifics = "every exact path, identifier, function/type/symbol name, command, config value, URL, version, and error string — copy these literally, never paraphrase them"
decisions = "key decisions and the user's corrections in their OWN words"
open = "genuinely unresolved problems and in-flight work"
progress = "for EVERY task you mention as started, underway, or in progress, explicitly record: completed scope, remaining scope, and a measurable amount/percentage/count when available; if the amount is unknown, say that plainly. Never leave a task as merely 'started'."
next = "the immediate next step, so the agent resumes without re-deriving it"
recent_bias = "spend MORE detail on the most recent activity than on old activity — precise current state + what was mid-flight + the next action; that is what lets the agent continue seamlessly"

[drop]  # do NOT carry these — they are the bulk of the tokens and add nothing
noise = "resolved intermediate steps, superseded/abandoned attempts, verbose tool output (keep the CONCLUSION, not the dump), acknowledgements and chit-chat, restated instructions, and anything trivially re-readable from the code itself"

[method]
fold = "if a PRIOR TABLE is present, update it in place — keep what's still true, add what's new, delete what's stale or superseded; do not just append"
dense = "terse markdown bullets, not prose. Facts and values, not sentences. No preamble, no narration of this process, no filler adjectives. For each task described as started or in progress, include completed scope, remaining scope, and measurable progress when available."
budget = "the whole table must fit ~6k tokens. If told it is OVER BUDGET, compress the largest/oldest sections first (drop the lowest-value detail) while keeping every exact value and the recent thread — never re-expand."

[how]
one_call = "call write_table ONCE, filling every section from the entire conversation. That single call is the whole job. You are asked again only if you left a required section empty or the table is over budget — otherwise you are done in one shot.""#;

pub(super) fn summarizer_tools() -> Vec<crate::llm::NativeToolDefinition> {
    // One tool that writes the ENTIRE table in a single call — a string field per
    // section. Filling it in one shot is the whole job (the loop only comes back
    // for a missing required section or a budget trim), which is what keeps the
    // big conversation window from being re-sent turn after turn.
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, desc, req) in SUMMARY_SECTIONS {
        properties.insert(
            name.to_string(),
            json!({ "type": "string", "description": desc }),
        );
        if *req {
            required.push(*name);
        }
    }
    vec![crate::llm::NativeToolDefinition {
        name: "write_table".to_string(),
        description: "Write the ENTIRE context table in one call — fill every section with dense \
            markdown bullets. This table replaces the raw history, so anything you omit is lost. \
            You are only asked again to fill a required section you left empty or to compress to fit \
            the budget."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }]
}

pub(super) fn toml_block(body: &str) -> String {
    format!("\"\"\"\n{}\n\"\"\"", body.replace("\"\"\"", "'''"))
}

pub(super) fn required_missing(sections: &BTreeMap<&'static str, String>) -> Vec<&'static str> {
    SUMMARY_SECTIONS
        .iter()
        .filter(|(name, _, req)| {
            *req && sections
                .get(name)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .map(|(n, ..)| *n)
        .collect()
}

pub(super) fn assemble_sections(sections: &BTreeMap<&'static str, String>) -> String {
    let body = SUMMARY_SECTIONS
        .iter()
        .filter_map(|(name, ..)| {
            sections
                .get(name)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|b| format!("{name} = {}", toml_block(b)))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("[compacted_window]\n{body}")
}

/// Rough token estimate of a request we're about to send — ~4 chars/token over
/// text content (tool-call arguments, tool results) plus a small per-message
/// overhead. Fallback only when the provider omits usage; when usage is present
/// the harness trusts `prompt_tokens` directly.
///
/// Inlined `image_base64` is stripped before counting (providers send images as
/// multimodal parts, not as text). Each image adds a flat vision-token pad so a
/// no-usage gateway still sees *some* image cost instead of zero or megabytes/4.
pub(super) fn tools_token_overhead(defs: &[NativeToolDefinition]) -> u64 {
    let chars: usize = defs
        .iter()
        .map(|d| d.name.len() + d.description.len() + d.input_schema.to_string().len() + 8)
        .sum();
    (chars / 4) as u64
}

/// Flat per-image pad used only by the manual estimate fallback. Real billing
/// varies by model/resolution; this is deliberately coarse — better than
/// counting base64 as text (~chars/4) which overstates by ~10–50×.
pub(super) const ESTIMATED_VISION_TOKENS_PER_IMAGE: u64 = 1_200;

pub(super) fn estimate_message_tokens(m: &HarnessMessage) -> u64 {
    let mut chars: usize = 8; // role/framing overhead
    let mut images: u64 = 0;
    match m {
        HarnessMessage::User { content }
        | HarnessMessage::System { content }
        | HarnessMessage::Summary { content, .. } => chars += content.len(),
        HarnessMessage::Assistant {
            content,
            tool_calls,
        } => {
            chars += content.len();
            for c in tool_calls {
                chars += c.name.len() + c.arguments.to_string().len() + 8;
            }
        }
        HarnessMessage::ToolResult {
            tool_name, content, ..
        } => {
            chars += tool_name.len();
            let (cleaned, image) = crate::llm::split_inlined_image(content);
            chars += cleaned.to_string().len();
            if image.is_some() {
                images += 1;
            }
        }
    }
    (chars / 4) as u64 + images.saturating_mul(ESTIMATED_VISION_TOKENS_PER_IMAGE)
}

pub(super) fn estimate_prompt_tokens(messages: &[HarnessMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

pub(super) fn compact_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home).display().to_string();
        if text == home {
            return "~".to_string();
        }
        if let Some(rest) = text.strip_prefix(&(home + "/")) {
            return format!("~/{rest}");
        }
    }
    text
}

pub(super) fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        t.chars().take(n).collect::<String>() + "…"
    } else {
        t.to_string()
    }
}

/// Render the prior table + the new activity window as plain text for the summarizer.
/// A short one-line summary of a mutating tool call for the approval prompt:
/// the shell command for `bash`, otherwise the target path.
pub(super) fn approval_summary(tool_name: &str, args: &Value) -> String {
    let raw = match tool_name {
        "bash" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        _ => args.get("path").and_then(Value::as_str).unwrap_or(""),
    };
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 120 {
        s.chars().take(120).collect::<String>() + "…"
    } else {
        s
    }
}

pub(super) fn render_window(
    prior_table: &str,
    older: &[HarnessMessage],
    original_request: &str,
    recent_focus: usize,
) -> String {
    let mut out = String::new();
    if !prior_table.trim().is_empty() {
        out.push_str("PRIOR TABLE (update this in place):\n");
        out.push_str(prior_table.trim());
        out.push_str("\n\n");
    } else if !original_request.trim().is_empty() {
        out.push_str(&format!(
            "ORIGINAL REQUEST: {}\n\n",
            clip(original_request, 600)
        ));
    }
    out.push_str("CONVERSATION TO SUMMARIZE:\n");
    // Overall budget: this window is re-sent on EVERY summarizer turn (up to 16),
    // so an uncapped render of a huge history multiplies into serious input cost.
    // Render newest-first mentally: when over budget, elide the OLDEST lines —
    // the prior table already carries the old context.
    const WINDOW_BUDGET_CHARS: usize = 120_000; // ~35k tokens
    let mut rendered: Vec<String> = Vec::with_capacity(older.len());
    // Mark the most recent messages so the summarizer captures them in extra detail.
    let focus_start = older.len().saturating_sub(recent_focus);
    for (i, m) in older.iter().enumerate() {
        if i == focus_start && focus_start > 0 {
            rendered.push(
                "\n=== MOST RECENT ACTIVITY — capture this in extra detail (what just happened, current state, next steps) ===".to_string(),
            );
        }
        let line = match m {
            HarnessMessage::User { content } => format!("USER: {}", clip(content, 600)),
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut s = String::new();
                if !content.trim().is_empty() {
                    s.push_str(&format!("ASSISTANT: {}", clip(content, 600)));
                }
                for c in tool_calls {
                    s.push_str(&format!(
                        "\nTOOL_CALL {}({})",
                        c.name,
                        clip(&c.arguments.to_string(), 300)
                    ));
                }
                s
            }
            HarnessMessage::ToolResult {
                tool_name, content, ..
            } => {
                format!(
                    "TOOL_RESULT {tool_name}: {}",
                    clip(&content.to_string(), 600)
                )
            }
            HarnessMessage::Summary { kind, content } => format!("[{kind}] {}", clip(content, 600)),
            HarnessMessage::System { content } => format!("SYSTEM: {}", clip(content, 300)),
        };
        if !line.trim().is_empty() {
            rendered.push(line);
        }
    }
    let total: usize = rendered.iter().map(|l| l.chars().count() + 1).sum();
    if total > WINDOW_BUDGET_CHARS {
        // Drop oldest lines until the window fits; note the elision once.
        let mut over = total - WINDOW_BUDGET_CHARS;
        let mut skip = 0usize;
        for line in &rendered {
            if over == 0 {
                break;
            }
            over = over.saturating_sub(line.chars().count() + 1);
            skip += 1;
        }
        out.push_str(&format!(
            "[…{skip} oldest message(s) elided to fit the window budget — rely on the PRIOR TABLE for that span]\n"
        ));
        for line in &rendered[skip..] {
            out.push_str(line);
            out.push('\n');
        }
    } else {
        for line in &rendered {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

