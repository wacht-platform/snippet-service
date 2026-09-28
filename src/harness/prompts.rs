use super::*;

// --- Agentic compaction: the living "context table" ---

/// (name, description, required) — the sections the summarizer maintains.
pub(super) const SUMMARY_SECTIONS: &[(&str, &str, bool)] = &[
    (
        "user_requests",
        "Chronological numbered list (1., 2., 3., ...) of ALL user requests, instructions, preferences, and bug reports across the entire conversation history. Retain every single past user request so the original intent is never lost.",
        true,
    ),
    (
        "task_overview",
        "Task Overview: Core user goals, constraints, success criteria, and what the user ultimately wants.",
        true,
    ),
    (
        "progress",
        "Progress: Completed tasks, files created/modified, passing tests/builds, and tasks currently in progress (with measurable completion amounts; never say only that a task started).",
        true,
    ),
    (
        "technical_decisions",
        "Key Findings & Technical Decisions: Architectural choices, discovered constraints, root causes, resolved bugs, and exact verbatim error strings or symbols.",
        false,
    ),
    (
        "active_context",
        "Active Context: Workspace state, active git branch, modified files, key paths, and active background tasks/monitors.",
        false,
    ),
    (
        "next_steps",
        "Next Steps: Prioritized, ordered list of immediate actions to resume execution seamlessly.",
        true,
    ),
    (
        "commitments_and_constraints",
        "Commitments & Constraints: Hard invariants, user guidelines, formatting rules, line count limits (<= 1,500 lines), and non-negotiables.",
        false,
    ),
];

pub(super) const MEMORY_REFLECTOR_SYSTEM: &str = r#"# memory_reflector
[identity]
role = "you curate a coding agent's durable memory after it finished a task"
input = "the current memory (rules, learnings, notes table of contents, all with ids), the ids the agent read during the task, and the task transcript"
output = "ONE apply_memory_delta call: small, precise changes. Code merges them; you never rewrite memory wholesale"

[kinds]
rules = "short imperative directives obeyed every session. Add one ONLY when the user stated a lasting preference or requirement in this task. global=true when it applies to every project (writing style, general workflow), else project scope"
learnings = "one-line reusable lessons: situation → approach → why. Add one when the task showed a technique or pitfall worth reapplying. global=true when it transfers to any project"
notes = "project knowledge, one topic per note: where things live, how to build/test/deploy, architecture, conventions, gotchas. Filed in a kebab-case section tree (e.g. build, architecture/harness). Each note has a title and a one-line summary that the table of contents shows, so write the summary to answer 'should I open this?'"

[bookkeeping]
marks = "for every id the agent read, mark helpful if it moved the task forward, harmful if it was wrong, stale or misleading. Leave it unmarked if it didn't matter"
fix_stale = "when the task proved a note or bullet wrong or outdated, update it (and mark it harmful). Remove what is plainly obsolete"
update_over_add = "prefer updating an existing note or bullet over adding a near-duplicate; an add that overlaps an existing item is rejected"
sections = "reuse existing sections; set a section summary when you create a new section"

[skip]
ephemeral = "task progress, one-off details, and anything obvious from reading the code"
secrets = "never store secret values"
trivial = "a task with nothing durable to keep gets an empty delta — that is a correct answer""#;

pub(super) fn memory_reflector_tools() -> Vec<crate::llm::NativeToolDefinition> {
    vec![crate::llm::NativeToolDefinition {
        name: "apply_memory_delta".to_string(),
        description: "Apply small changes to memory. Every field is optional; send an empty object when nothing durable came out of the task.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "marks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "verdict": { "type": "string", "enum": ["helpful", "harmful"] }
                        },
                        "required": ["id", "verdict"]
                    }
                },
                "notes": {
                    "type": "array",
                    "description": "add: section, id, title, summary, body. update: id plus only the fields that change (section moves the note).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["add", "update"] },
                            "id": { "type": "string", "description": "kebab-case note id" },
                            "section": { "type": "string" },
                            "title": { "type": "string" },
                            "summary": { "type": "string" },
                            "body": { "type": "string", "description": "markdown" }
                        },
                        "required": ["op", "id"]
                    }
                },
                "bullets": {
                    "type": "array",
                    "description": "add: kind, text, optional global and section. update: id plus text and/or section.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["add", "update"] },
                            "kind": { "type": "string", "enum": ["rule", "learning"] },
                            "id": { "type": "string" },
                            "global": { "type": "boolean" },
                            "section": { "type": "string", "description": "short topic label" },
                            "text": { "type": "string" }
                        },
                        "required": ["op"]
                    }
                },
                "sections": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "section": { "type": "string" },
                            "summary": { "type": "string" }
                        },
                        "required": ["section", "summary"]
                    }
                },
                "remove": { "type": "array", "items": { "type": "string" } }
            }
        }),
    }]
}

pub(super) const SUMMARIZER_SYSTEM: &str = r#"# compaction_summarizer
[identity]
role = "you compress a coding agent's whole conversation into ONE dense Antigravity-style <CONTEXT_SUMMARY> that REPLACES the raw history"
stakes = "the raw messages are then archived and discarded — this context summary is all that survives in active working memory. Anything you leave out is lost from immediate working memory; anything you pad is re-sent on every future turn and wastes tokens. Maximize signal per token."

[user_requests]  # CRITICAL: chronological timeline of user intent
timeline = "maintain the 'user_requests' section as a strictly numbered list (1., 2., 3., ...) in chronological order. Carry forward all user requests from the PRIOR SUMMARY if present, and append any new user prompts from the conversation. Never lose a past user request, instruction, correction, or preference."

[preserve]  # carry these forward — verbatim where the exact value/wording matters
task_overview = "core user objective, hard constraints, and success criteria"
progress = "for EVERY task you mention as started, underway, or in progress, explicitly record: completed scope, remaining scope, and a measurable amount/percentage/count when available; if the amount is unknown, say that plainly. Never leave a task as merely 'started'."
technical_decisions = "key architectural findings, discovered root causes, technical decisions, and exact verbatim error strings, symbol names, and IDs"
active_context = "current workspace state: which files were created/changed, what works, what's broken or unverified, git branch, and active background tasks"
next_steps = "prioritized, ordered next actions so the agent can resume immediately without re-deriving"
commitments_and_constraints = "invariants, user rules, line limits (e.g. <= 1,500 lines per file), formatting preferences"

[drop]  # do NOT carry these — they are the bulk of tokens and add nothing
noise = "assistant chit-chat, conversational pleasantries, intermediate tool dumps (keep the CONCLUSION, not the raw output), restated instructions, and anything trivially re-readable from the code itself. Raw tool payloads are archived in SQLite and retrievable with `snippet history search` / `snippet history show`."

[method]
fold = "if a PRIOR SUMMARY is present, update it in place — preserve all past user requests, update the engineering sections, add what's new, delete what's stale or superseded; do not just blindly append"
dense = "terse markdown bullets under each section heading. Facts and values, not verbose prose. No preamble, no narration of this process, no filler adjectives."
budget = "the whole summary must fit ~6k tokens. If told it is OVER BUDGET, compress the largest/oldest sections first (drop the lowest-value detail) while keeping every exact value, the full user_requests list, and the recent thread — never re-expand."

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
        description: "Write the ENTIRE context summary in one call — fill every section with dense \
            markdown bullets and maintain the chronological numbered user_requests list. This summary replaces the raw history, so anything you omit is lost from active context. \
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
    let now = chrono::Utc::now().to_rfc3339();
    let mut out = String::new();
    out.push_str("<CONTEXT_SUMMARY>\n");
    out.push_str("The following is a summary of the conversation history that has been truncated to fit within the context window:\n\n");
    out.push_str(&format!("This summary was generated at {now}.\n\n"));

    if let Some(user_reqs) = sections
        .get("user_requests")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str("# User Requests\n");
        out.push_str("The following were the most recent user requests in chronological order:\n");
        out.push_str(user_reqs);
        out.push_str("\n\n");
    }

    out.push_str("# Previous Session Summary:\n<summary>\n");

    let mut section_idx = 1;
    let engineering_sections: &[(&str, &str)] = &[
        ("task_overview", "Task Overview"),
        ("progress", "Progress"),
        ("technical_decisions", "Key Findings & Technical Decisions"),
        ("active_context", "Active Context"),
        ("next_steps", "Next Steps"),
        ("commitments_and_constraints", "Commitments & Constraints"),
    ];

    for (key, title) in engineering_sections {
        if let Some(content) = sections
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            out.push_str(&format!("### {section_idx}. {title}\n"));
            out.push_str(content);
            out.push_str("\n\n");
            section_idx += 1;
        }
    }

    out.push_str("</summary>\n</CONTEXT_SUMMARY>");
    out
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
    let owned;
    let raw = match tool_name {
        "bash" => args.get("command").and_then(Value::as_str).unwrap_or(""),
        "change_files" => {
            owned = args
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .map(|c| {
                            format!(
                                "{} {}",
                                c.get("action").and_then(Value::as_str).unwrap_or("change"),
                                c.get("path").and_then(Value::as_str).unwrap_or("?")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            owned.as_str()
        }
        _ => args.get("path").and_then(Value::as_str).unwrap_or(""),
    };
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 120 {
        s.chars().take(120).collect::<String>() + "…"
    } else {
        s
    }
}

/// Extract chronological user requests from prior summary and new messages.
pub(super) fn extract_user_requests(
    prior_summary: &str,
    messages: &[HarnessMessage],
    original_request: &str,
) -> Vec<String> {
    let mut requests = Vec::new();

    // 1. Extract from prior summary if present
    if !prior_summary.trim().is_empty() {
        let mut in_user_requests = false;
        for line in prior_summary.lines() {
            let trimmed = line.trim();
            if trimmed == "# User Requests" {
                in_user_requests = true;
                continue;
            }
            if in_user_requests {
                if trimmed.starts_with('#') || trimmed.starts_with("<summary>") {
                    break;
                }
                if trimmed.starts_with("The following were") || trimmed.is_empty() {
                    continue;
                }
                // Line like "1. do something" or "- do something"
                let clean = if let Some(pos) = trimmed.find(". ") {
                    let prefix = &trimmed[..pos];
                    if prefix.chars().all(|c| c.is_ascii_digit()) {
                        trimmed[pos + 2..].trim().to_string()
                    } else {
                        trimmed.trim_start_matches('-').trim().to_string()
                    }
                } else {
                    trimmed.trim_start_matches('-').trim().to_string()
                };
                if !clean.is_empty() && !requests.iter().any(|r| r == &clean) {
                    requests.push(clean);
                }
            }
        }
    }

    // 2. If no prior requests found and original_request is given, add it
    if requests.is_empty() && !original_request.trim().is_empty() {
        requests.push(original_request.trim().to_string());
    }

    // 3. Extract new user messages from the window
    for m in messages {
        if let HarnessMessage::User { content } = m {
            let text = content.trim();
            // Skip internal summary envelopes or system orientation blocks
            if text.is_empty()
                || text.starts_with("[summary:")
                || text.starts_with("[Recent activity")
                || text.starts_with("<CONTEXT_SUMMARY>")
            {
                continue;
            }
            let clean = clip(text, 500);
            if !requests.iter().any(|r| r == &clean) {
                requests.push(clean);
            }
        }
    }

    requests
}

pub(super) fn render_window(
    prior_table: &str,
    older: &[HarnessMessage],
    original_request: &str,
    recent_focus: usize,
) -> String {
    let mut out = String::new();
    let user_requests = extract_user_requests(prior_table, older, original_request);
    if !user_requests.is_empty() {
        out.push_str("CHRONOLOGICAL USER REQUESTS TO PRESERVE (maintain this exact numbered list in 'user_requests'):\n");
        for (i, req) in user_requests.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, req));
        }
        out.push('\n');
    }

    if !prior_table.trim().is_empty() {
        out.push_str("PRIOR CONTEXT SUMMARY (update this in place):\n");
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

