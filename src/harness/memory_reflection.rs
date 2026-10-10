use super::*;

impl CodingHarness {
    pub(super) async fn run_agentic_summary(
        &self,
        model: &mut dyn AgentModel,
        window_text: &str,
    ) -> Result<String, ToolError> {
        // The whole table is written in ONE call. Extra turns exist only to fill a
        // missing required section or compress to budget — and a pure over-budget
        // retry re-sends only the (small) table, never the full window again. This
        // is the token fix: the old per-section loop re-sent the entire window on
        // every one of up to 16 turns (~window × 16); now it's ~window × 1.
        const MAX_TURNS: usize = 4;
        const BUDGET_CHARS: usize = 21_000; // ~6k tokens at ~3.5 chars/token

        let tools = summarizer_tools();
        // The last full table we assembled (used for a cheap, window-free trim turn).
        let mut over_budget_table: Option<String> = None;
        let mut feedback = String::new();

        for _turn in 1..=MAX_TURNS {
            // Over-budget retry: hand back only the table to compress — the raw
            // window isn't needed to shrink an existing table, and re-sending it is
            // the whole cost we're avoiding. Every other turn sends the window.
            let user = if let Some(table) = &over_budget_table {
                format!(
                    "Compress this context summary to fit the ~6k-token budget. Drop the lowest-value \
                     detail from the largest/oldest sections; keep every exact path, id, error string, \
                     decision, the complete user_requests list, and the recent thread. Return the FULL summary via write_table.\n\n\
                     CURRENT SUMMARY:\n{table}\n\n{feedback}"
                )
            } else {
                format!(
                    "Conversation to compact; fold all of it into one Antigravity-style <CONTEXT_SUMMARY>:\n{window_text}\n\n\
                     Call write_table once, with every section filled.{}",
                    if feedback.is_empty() {
                        String::new()
                    } else {
                        format!("\n\n{feedback}")
                    }
                )
            };
            let messages = vec![
                HarnessMessage::System {
                    content: SUMMARIZER_SYSTEM.to_string(),
                },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output
                .calls
                .first()
                .filter(|c| c.tool_name == "write_table")
            else {
                feedback = "Call write_table once, filling every section.".to_string();
                continue;
            };

            // Pull each section out of the single call.
            let mut sections: BTreeMap<&'static str, String> = BTreeMap::new();
            for (name, ..) in SUMMARY_SECTIONS {
                if let Some(v) = call
                    .arguments
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    sections.insert(name, v.to_string());
                }
            }

            // Fallback: if user_requests was omitted by the model, restore the pre-extracted list
            if sections
                .get("user_requests")
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                if let Some(pos) = window_text.find("CHRONOLOGICAL USER REQUESTS TO PRESERVE") {
                    let rest = &window_text[pos..];
                    if let Some(start) = rest.find('\n') {
                        let candidate = &rest[start + 1..];
                        let end = candidate.find("\n\n").unwrap_or(candidate.len());
                        let req_text = candidate[..end].trim();
                        if !req_text.is_empty() {
                            sections.insert("user_requests", req_text.to_string());
                        }
                    }
                }
            }

            let missing = required_missing(&sections);
            if !missing.is_empty() {
                // Model left a required section empty — needs the window again.
                over_budget_table = None;
                feedback = format!(
                    "Required section(s) empty: {}. Fill them from the conversation.",
                    missing.join(", ")
                );
                continue;
            }

            let assembled = assemble_sections(&sections);
            if assembled.chars().count() > BUDGET_CHARS {
                let over = assembled.chars().count() - BUDGET_CHARS;
                over_budget_table = Some(assembled);
                feedback = format!("Currently ~{over} chars over budget.");
                continue;
            }
            return Ok(assembled);
        }

        // Turn cap hit: use the last over-budget table (hard-trimmed) if we have one,
        // else give up so the caller falls back to heuristic compaction.
        if let Some(table) = over_budget_table {
            return Ok(table.chars().take(BUDGET_CHARS).collect::<String>()
                + "\n…[trimmed to the 6k-token budget]");
        }
        Err(ToolError::msg("summarizer did not produce a valid table"))
    }

    /// ACE-style curation after a request that did real work: the model sees
    /// the memory, the ids the agent read and the request's transcript, and
    /// answers with one delta that `Memory` merges deterministically. A delta
    /// with rejected operations gets one retry carrying only the errors.
    pub(super) async fn reflect_on_request(
        &self,
        model: &mut dyn AgentModel,
        state: &HarnessState,
    ) -> Result<(), ToolError> {
        const MIN_TOOL_RESULTS: usize = 3;
        const MAX_TURNS: usize = 2;
        const MEMORY_CHARS: usize = 20_000;

        let window = request_window(state);
        let tool_results = window
            .iter()
            .filter(|m| matches!(m, HarnessMessage::ToolResult { .. }))
            .count();
        if tool_results < MIN_TOOL_RESULTS {
            return Ok(());
        }
        let memory = crate::memory::Memory::open(self.context.workspace_root());
        let read = memory.note_ids(&note_paths_read(window, self.context.workspace_root()));
        let transcript = render_reflection_window(window);
        let tools = memory_reflector_tools();
        let mut feedback = String::new();
        let prev_effort = model.swap_reasoning_effort(Some("off".to_string()));

        for _ in 0..MAX_TURNS {
            let user = format!(
                "CURRENT MEMORY:\n{}\n\nNOTES THE AGENT READ THIS TASK (mark each helpful or harmful if it mattered; mark rules and learnings that clearly shaped the work too): {}\n\nTASK TRANSCRIPT:\n{transcript}{feedback}\n\nCall apply_memory_delta once.",
                clip(&memory.full_listing(), MEMORY_CHARS),
                if read.is_empty() {
                    "(none)".to_string()
                } else {
                    read.join(", ")
                },
            );
            let messages = vec![
                HarnessMessage::System {
                    content: MEMORY_REFLECTOR_SYSTEM.to_string(),
                },
                HarnessMessage::User { content: user },
            ];
            let output = match model.generate(&messages, &tools, true, None).await {
                Ok(output) => output,
                Err(e) => {
                    model.swap_reasoning_effort(prev_effort);
                    return Err(e);
                }
            };
            let Some(call) = output
                .calls
                .first()
                .filter(|c| c.tool_name == "apply_memory_delta")
            else {
                feedback = "\n\nLAST ATTEMPT: no apply_memory_delta call was made.".to_string();
                continue;
            };
            let (applied, errors) = apply_delta(&memory, &call.arguments);
            for line in &applied {
                self.debug_log(&format!("memory reflection: {line}"));
            }
            if errors.is_empty() {
                break;
            }
            feedback = format!(
                "\n\nALREADY APPLIED (do not resend):\n{}\n\nREJECTED — fix these and resend only them, or send an empty delta to drop them:\n{}",
                if applied.is_empty() { "(nothing)".to_string() } else { applied.join("\n") },
                errors.join("\n")
            );
        }
        model.swap_reasoning_effort(prev_effort);
        Ok(())
    }
}

/// The messages of the request that just ended: from its checkpoint when one
/// was taken since the last compaction, else from the last user message.
fn request_window(state: &HarnessState) -> &[HarnessMessage] {
    let start = state
        .checkpoints
        .last()
        .filter(|c| c.compactions == state.compactions && c.message_index <= state.messages.len())
        .map(|c| c.message_index)
        .or_else(|| {
            state
                .messages
                .iter()
                .rposition(|m| matches!(m, HarnessMessage::User { .. }))
        })
        .unwrap_or(0);
    &state.messages[start..]
}

/// Markdown files the agent's shell commands named during the request.
fn note_paths_read(window: &[HarnessMessage], workspace: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for message in window {
        let HarnessMessage::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls.iter().filter(|c| c.name == "bash") {
            let command = call.arguments.get("command").and_then(Value::as_str).unwrap_or("");
            for path in crate::memory::markdown_paths_in_command(command, workspace) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn render_reflection_window(window: &[HarnessMessage]) -> String {
    const BUDGET_CHARS: usize = 60_000;
    let mut lines: Vec<String> = window
        .iter()
        .filter_map(|m| {
            let line = match m {
                HarnessMessage::User { content } => format!("USER: {}", clip(content, 1_500)),
                HarnessMessage::Assistant { content, tool_calls } => {
                    let mut s = String::new();
                    if !content.trim().is_empty() {
                        s.push_str(&format!("ASSISTANT: {}", clip(content, 1_500)));
                    }
                    for c in tool_calls {
                        s.push_str(&format!("\nCALL {}({})", c.name, clip(&c.arguments.to_string(), 500)));
                    }
                    s
                }
                HarnessMessage::ToolResult { tool_name, content, .. } => {
                    format!("RESULT {tool_name}: {}", clip(&crate::llm::render_tool_result(content), 800))
                }
                HarnessMessage::Summary { content, .. } => format!("EARLIER (summary): {}", clip(content, 4_000)),
                HarnessMessage::System { .. } => String::new(),
            };
            (!line.trim().is_empty()).then_some(line)
        })
        .collect();
    let mut total: usize = lines.iter().map(|l| l.chars().count() + 1).sum();
    let mut dropped = 0;
    while total > BUDGET_CHARS && lines.len() > 1 {
        total -= lines.remove(0).chars().count() + 1;
        dropped += 1;
    }
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!("[…{dropped} earliest messages omitted]\n"));
    }
    out.push_str(&lines.join("\n"));
    out
}

/// Merge one reflector delta through the same operations the CLI uses.
/// Returns what was applied and what was rejected, one line each.
fn apply_delta(memory: &crate::memory::Memory, delta: &Value) -> (Vec<String>, Vec<String>) {
    use crate::memory::{Kind, NoteEdit};
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    let items = |key: &str| delta.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let mut record = |what: String, result: Result<String, String>| match result {
        Ok(msg) => applied.push(msg),
        Err(e) => errors.push(format!("{what}: {e}")),
    };

    for section in items("sections") {
        let name = text(&section, "section").unwrap_or_default();
        let summary = text(&section, "summary").unwrap_or_default();
        record(format!("section {name}"), memory.set_section(&name, &summary));
    }
    for note in items("notes") {
        let id = text(&note, "id").unwrap_or_default();
        let result = match text(&note, "op").as_deref() {
            Some("add") => memory.add_note(
                &text(&note, "section").unwrap_or_default(),
                &id,
                &text(&note, "title").unwrap_or_default(),
                &text(&note, "summary").unwrap_or_default(),
                &text(&note, "body").unwrap_or_default(),
            ),
            Some("update") => memory.update_note(
                &id,
                NoteEdit {
                    title: text(&note, "title"),
                    summary: text(&note, "summary"),
                    body: text(&note, "body"),
                    section: text(&note, "section"),
                },
            ),
            _ => Err("op must be add or update".to_string()),
        };
        record(format!("note {id}"), result);
    }
    for bullet in items("bullets") {
        let result = match text(&bullet, "op").as_deref() {
            Some("add") => {
                let kind = match text(&bullet, "kind").as_deref() {
                    Some("rule") => Some(Kind::Rule),
                    Some("learning") => Some(Kind::Learning),
                    _ => None,
                };
                match kind {
                    Some(kind) => memory.add_bullet(
                        kind,
                        bullet.get("global").and_then(Value::as_bool).unwrap_or(false),
                        &text(&bullet, "section").unwrap_or_default(),
                        &text(&bullet, "text").unwrap_or_default(),
                    ),
                    None => Err("kind must be rule or learning".to_string()),
                }
            }
            Some("update") => memory.update_bullet(
                &text(&bullet, "id").unwrap_or_default(),
                text(&bullet, "text").as_deref(),
                text(&bullet, "section").as_deref(),
            ),
            _ => Err("op must be add or update".to_string()),
        };
        let what = text(&bullet, "id")
            .or_else(|| text(&bullet, "text"))
            .unwrap_or_default();
        record(format!("bullet {}", clip(&what, 60)), result);
    }
    for mark in items("marks") {
        let id = text(&mark, "id").unwrap_or_default();
        let helpful = text(&mark, "verdict").as_deref() != Some("harmful");
        record(format!("mark {id}"), memory.mark(&id, helpful));
    }
    for id in items("remove") {
        let id = id.as_str().unwrap_or_default().to_string();
        record(format!("remove {id}"), memory.remove(&id));
    }
    (applied, errors)
}
