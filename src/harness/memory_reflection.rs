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
                    "CONVERSATION TO COMPACT — fold ALL of it into one Antigravity-style <CONTEXT_SUMMARY>:\n{window_text}\n\n\
                     Call write_table ONCE with every section filled.{}",
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

    /// Learning pass run after compaction: a bounded worker that curates the
    /// per-workspace memory (facts, pointers, how-to playbooks) from the freshly
    /// compacted session table. Mirrors `run_agentic_summary`'s tool-loop shape.
    /// Best-effort: errors are surfaced to the caller, which treats them as non-fatal.
    pub(super) async fn run_memory_reflection(
        &self,
        model: &mut dyn AgentModel,
        summary_table: &str,
    ) -> Result<(), ToolError> {
        // Tight cap — each turn is a full model round-trip and this runs after
        // every main-session compaction (was 8, then 5). Reflection should converge
        // in 1–2 writes; a higher cap just let the model re-save the same entry
        // over and over until it timed out on the cap.
        const MAX_TURNS: usize = 4;

        let store = crate::memory::MemoryStore::for_workspace(self.context.workspace_root());
        let global = crate::memory::MemoryStore::global();
        let index_budget = self.config.memory_index_budget_chars;
        let entry_budget = self.config.memory_entry_budget_chars;
        let max_entries = self.config.memory_max_entries;
        let tools = memory_reflector_tools();
        let ws = self.context.workspace_root().display().to_string();
        let mut feedback = "(review the current index/entries, then extract the reusable procedure(s) and key facts)".to_string();
        let mut writes = 0usize;
        self.debug_log(&format!(
            "memory reflection: start (existing entries={}, index={}b)",
            store.list_entries().len(),
            store.read_index().len()
        ));

        for turn in 1..=MAX_TURNS {
            let index = store.read_index();
            let entries = store.list_entries();
            let patterns = global.read_patterns();
            let user = format!(
                "WORKSPACE: {ws}\n\nWHAT JUST HAPPENED (compacted session table):\n{summary_table}\n\nCURRENT MEMORY INDEX:\n{index}\n\nEXISTING ENTRIES: {entries}\n\nCURRENT REUSABLE PATTERNS (global):\n{patterns}\n\nLAST RESULT: {feedback}\n\nTurn {turn}/{MAX_TURNS} · {writes} write(s) so far. Make exactly one tool call. Capture what helps a FUTURE session: (a) workspace FACTS/pointers and how-to PLAYBOOK(s) for THIS project via memory_write; (b) any GENERALIZABLE PATTERN this session demonstrated — a reusable technique (situation → approach → why) that would help in ANY project — via memory_pattern (include the existing patterns plus the new/refined one). Write each distinct thing ONCE; NEVER re-save or 'polish' something you already wrote this pass. Once the durable value is captured (usually 1–2 writes) and the index points to workspace entries, call finalize. Only finalize with nothing written if the session was genuinely trivial.",
                index = if index.trim().is_empty() {
                    "(empty)".to_string()
                } else {
                    index
                },
                entries = if entries.is_empty() {
                    "(none)".to_string()
                } else {
                    entries.join(", ")
                },
                patterns = if patterns.trim().is_empty() {
                    "(none yet)".to_string()
                } else {
                    patterns
                },
                writes = writes,
            );
            let messages = vec![
                HarnessMessage::System {
                    content: MEMORY_REFLECTOR_SYSTEM.to_string(),
                },
                HarnessMessage::User { content: user },
            ];
            let output = model.generate(&messages, &tools, true, None).await?;
            let Some(call) = output.calls.first() else {
                feedback = "no tool call received — call exactly one tool".to_string();
                continue;
            };
            let arg_str = |k: &str| {
                call.arguments
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            match call.tool_name.as_str() {
                "finalize" => {
                    self.debug_log(&format!(
                        "memory reflection: finalized after {writes} write(s)"
                    ));
                    return Ok(());
                }
                "memory_read" => {
                    let id = arg_str("id");
                    feedback = match store.read_entry(&id) {
                        Ok(c) => format!("entry `{id}`:\n{c}"),
                        Err(e) => e,
                    };
                }
                "memory_write" => {
                    let id = arg_str("id");
                    let content = arg_str("content");
                    feedback = match store.write_entry(&id, &content, entry_budget, max_entries) {
                        Ok(()) => {
                            writes += 1;
                            self.debug_log(&format!(
                                "memory reflection: wrote entry `{id}` ({}b)",
                                content.len()
                            ));
                            format!(
                                "entry `{id}` saved. Do NOT re-write or 'polish' it. If the index needs it, call memory_index once — then finalize."
                            )
                        }
                        Err(e) => e,
                    };
                }
                "memory_index" => {
                    let content = arg_str("content");
                    feedback = match store.write_index(&content, index_budget) {
                        Ok(()) => {
                            writes += 1;
                            self.debug_log("memory reflection: updated index");
                            "index updated".to_string()
                        }
                        Err(e) => e,
                    };
                }
                "memory_delete" => {
                    let id = arg_str("id");
                    feedback = match store.delete_entry(&id) {
                        Ok(()) => format!("entry `{id}` deleted"),
                        Err(e) => e,
                    };
                }
                "memory_pattern" => {
                    let content = arg_str("content");
                    feedback = match global.add_pattern(&content, crate::memory::patterns_budget())
                    {
                        Ok(true) => {
                            writes += 1;
                            self.debug_log("memory reflection: added global pattern");
                            "pattern added. If nothing else remains, finalize.".to_string()
                        }
                        Ok(false) => {
                            "identical pattern already stored — don't re-add it; finalize if done."
                                .to_string()
                        }
                        Err(e) => e,
                    };
                }
                other => feedback = format!("unknown tool `{other}`"),
            }
        }
        self.debug_log(&format!(
            "memory reflection: hit turn cap after {writes} write(s)"
        ));
        Ok(())
    }
}
