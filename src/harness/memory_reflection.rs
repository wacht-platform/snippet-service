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

    /// Curation after a request that did real work: the model sees every memory
    /// repo this session uses and the request's transcript, and answers with one
    /// set of entry changes. Each change is applied and each touched repo is
    /// committed with the session as the source. A set with rejected changes
    /// gets one retry carrying only the errors.
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
        let repos = self.memory_scopes();
        if repos.is_empty() {
            return Ok(());
        }
        let source = self
            .session_id()
            .map(|id| format!("snippet://session/{id}"))
            .unwrap_or_else(|| "snippet://session".to_string());
        let transcript = render_reflection_window(window);
        let tools = memory_reflector_tools();
        let mut feedback = String::new();
        let prev_effort = model.swap_reasoning_effort(Some("off".to_string()));
        let mut touched: Vec<String> = Vec::new();

        for _ in 0..MAX_TURNS {
            let user = format!(
                "MEMORY REPOS:\n{}\n\nTASK TRANSCRIPT:\n{transcript}{feedback}\n\nCall update_memory once.",
                clip(&render_repos(&repos), MEMORY_CHARS),
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
                .filter(|c| c.tool_name == "update_memory")
            else {
                feedback = "\n\nLAST ATTEMPT: no update_memory call was made.".to_string();
                continue;
            };
            let (applied, errors) = apply_changes(&repos, &call.arguments, &source);
            for (key, line) in &applied {
                self.debug_log(&format!("memory reflection: {key} {line}"));
                if !touched.contains(key) {
                    touched.push(key.clone());
                }
            }
            if errors.is_empty() {
                break;
            }
            feedback = format!(
                "\n\nALREADY APPLIED (do not resend):\n{}\n\nREJECTED — fix these and resend only them, or send no changes to drop them:\n{}",
                if applied.is_empty() {
                    "(nothing)".to_string()
                } else {
                    applied.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")
                },
                errors.join("\n")
            );
        }
        model.swap_reasoning_effort(prev_effort);
        for repo in repos.iter().filter(|r| touched.contains(&r.key)) {
            if let Err(e) = repo.commit_all("Remember lessons from a session") {
                self.debug_log(&format!("memory reflection commit {}: {e}", repo.key));
            }
        }
        Ok(())
    }
}

fn render_repos(repos: &[crate::memory::MemoryRepo]) -> String {
    const FILE_CHARS: usize = 6_000;
    let mut out = String::new();
    for repo in repos {
        let scope = match repo.kind {
            crate::memory::ScopeKind::Global => "global",
            crate::memory::ScopeKind::Project => "project",
            crate::memory::ScopeKind::Agent => "agent",
            crate::memory::ScopeKind::Session => "session",
        };
        out.push_str(&format!("\n=== scope: {scope} ({}) ===\n", repo.label));
        for file in repo.files().into_iter().filter(|f| f.ends_with(".md")) {
            let text = repo.read_file(&file).unwrap_or_default();
            out.push_str(&format!("--- {file} ---\n{}\n", clip(text.trim(), FILE_CHARS)));
        }
    }
    out
}

/// Apply one update_memory call. Returns what was applied (repo key, line) and
/// what was rejected.
fn apply_changes(
    repos: &[crate::memory::MemoryRepo],
    arguments: &Value,
    source: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let changes = arguments
        .get("changes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for change in changes {
        let text = |key: &str| change.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let scope = text("scope");
        let file = text("file");
        let Some(repo) = crate::memory::ScopeKind::parse(&scope)
            .and_then(|kind| repos.iter().find(|r| r.kind == kind))
        else {
            errors.push(format!("{scope}: no such scope in this session"));
            continue;
        };
        let stamped = |entry: String| {
            if entry.contains("[source:") {
                entry
            } else {
                format!("{} [source: {source}; added: {today}]", entry.trim())
            }
        };
        let result = match text("op").as_str() {
            "add" => repo.add_entry(&file, &stamped(text("entry"))),
            "replace" => repo.replace_entry(&file, &text("match"), &stamped(text("entry"))),
            "remove" => repo.remove_entry(&file, &text("match")),
            other => Err(format!("op must be add, replace or remove, not {other:?}")),
        };
        match result {
            Ok(line) => applied.push((repo.key.clone(), format!("{scope} {line}"))),
            Err(e) => errors.push(format!("{scope} {file}: {e}")),
        }
    }
    (applied, errors)
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
