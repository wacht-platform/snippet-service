use super::*;

impl CodingHarness {
    pub(super) async fn run_agentic_summary(
        &self,
        model: &mut dyn AgentModel,
        window_text: &str,
    ) -> Result<String, ToolError> {
        const MAX_TURNS: usize = 6;
        const BUDGET_CHARS: usize = 9_000;
        const MAX_READS: usize = 2;

        let repos = if self.config.memory_enabled && self.context.lane_origin().is_none() {
            self.memory_scopes()
        } else {
            Vec::new()
        };
        let source = self
            .session_id()
            .map(|id| format!("snippet://session/{id}"))
            .unwrap_or_else(|| "snippet://session".to_string());
        let mut memory_open = !repos.is_empty();
        let mut opened = String::new();
        let mut reads = 0;
        let mut saved: Vec<String> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        let mut over_budget_table: Option<String> = None;
        let mut feedback = String::new();
        let mut result: Option<String> = None;

        for _turn in 1..=MAX_TURNS {
            let user = if let Some(table) = &over_budget_table {
                format!(
                    "Compress this context summary to fit the ~2.5k-token budget. Cut the oldest, lowest-value \
                     detail; keep the complete user_requests list, exact values and next steps. Return the FULL summary via write_table.\n\n\
                     CURRENT SUMMARY:\n{table}\n\n{feedback}"
                )
            } else {
                let memory = if memory_open {
                    let opened = if opened.is_empty() {
                        String::new()
                    } else {
                        format!("OPENED TOPIC FILES:\n{opened}\n")
                    };
                    format!("MEMORY INDEXES:\n{}\n{opened}\n", render_indexes(&repos))
                } else if !saved.is_empty() {
                    format!(
                        "ALREADY SAVED TO MEMORY (point to these, do not resave):\n{}\n\n",
                        saved.join("\n")
                    )
                } else {
                    String::new()
                };
                let ask = if memory_open && reads < MAX_READS {
                    "Open the topic files you will change with read_memory if you need their entries, or call update_memory with what lasts and write_table with every required section."
                } else if memory_open {
                    "Call update_memory with what lasts, and write_table with every required section."
                } else {
                    "Call write_table once, with every required section filled."
                };
                format!(
                    "{memory}Conversation to compact:\n{window_text}\n\n{ask}{}",
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
            let mut tools = summarizer_tools();
            if memory_open {
                tools.extend(memory_reflector_tools());
                if reads < MAX_READS {
                    tools.push(memory_reader_tool());
                }
            }
            let output = model.generate(&messages, &tools, true, None).await?;

            if memory_open
                && reads < MAX_READS
                && let Some(call) = output.calls.iter().find(|c| c.tool_name == "read_memory")
            {
                reads += 1;
                let files = call.arguments.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
                opened.push_str(&read_topic_files(&repos, &files));
                if !output.calls.iter().any(|c| c.tool_name == "update_memory" || c.tool_name == "write_table") {
                    continue;
                }
            }

            if memory_open
                && let Some(call) = output.calls.iter().find(|c| c.tool_name == "update_memory")
            {
                memory_open = false;
                let (applied, errors) = apply_changes(&repos, &call.arguments, &source);
                for (key, line) in applied {
                    self.debug_log(&format!("compaction memory: {key} {line}"));
                    if !touched.contains(&key) {
                        touched.push(key);
                    }
                    saved.push(line);
                }
                for error in errors {
                    self.debug_log(&format!("compaction memory rejected: {error}"));
                }
            }

            let Some(call) = output.calls.iter().find(|c| c.tool_name == "write_table") else {
                feedback = "Call write_table once, filling every required section.".to_string();
                continue;
            };

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
            result = Some(assembled);
            break;
        }

        for repo in repos.iter().filter(|r| touched.contains(&r.key)) {
            if let Err(e) = repo.commit_all("Save working state before compaction") {
                self.debug_log(&format!("compaction memory commit {}: {e}", repo.key));
            }
        }
        if !touched.is_empty() {
            self.tidy_memory();
        }

        if let Some(table) = result {
            return Ok(table);
        }
        if let Some(table) = over_budget_table {
            return Ok(table.chars().take(BUDGET_CHARS).collect::<String>()
                + "\n…[trimmed to the summary budget]");
        }
        Err(ToolError::msg("summarizer did not produce a valid table"))
    }

    /// Curation after a request that did real work, as a branch of the
    /// conversation itself: the same messages, tools and reasoning settings the
    /// session just used, plus one instruction, so the provider serves the whole
    /// prefix from its prompt cache. The branch is never saved to the history.
    /// The model answers with JSON changes, which are applied and committed with
    /// the session as the source.
    pub(super) async fn reflect_on_request(
        &self,
        model: &mut dyn AgentModel,
        state: &HarnessState,
    ) -> Result<(), ToolError> {
        if !reflection_due(request_window(state)) {
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
        let goal_active = state
            .goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Active);
        let definitions = self.definitions_for(true, goal_active);
        let mut messages = state.messages.clone();
        messages.push(HarnessMessage::User {
            content: reflection_instruction(&repos),
        });
        let mut touched: Vec<String> = Vec::new();
        let mut reads = 0;
        let mut retries = 0;
        loop {
            let output = model.generate(&messages, &definitions, false, None).await?;
            let reply = output.content_text.clone().unwrap_or_default();
            if reads < REFLECTION_READS
                && let Some(files) = parse_reflection_read(&reply)
            {
                reads += 1;
                messages.push(HarnessMessage::Assistant {
                    content: reply,
                    tool_calls: Vec::new(),
                });
                messages.push(HarnessMessage::User {
                    content: reflection_opened(&repos, &files),
                });
                continue;
            }
            let Some(changes) = parse_reflection(&reply) else {
                self.debug_log("memory reflection: no JSON changes in the reply");
                break;
            };
            let (applied, errors) = apply_changes(&repos, &changes, &source);
            for (key, line) in &applied {
                self.debug_log(&format!("memory reflection: {key} {line}"));
                if !touched.contains(key) {
                    touched.push(key.clone());
                }
            }
            if errors.is_empty() || retries == 1 {
                break;
            }
            retries += 1;
            messages.push(HarnessMessage::Assistant {
                content: reply,
                tool_calls: Vec::new(),
            });
            messages.push(HarnessMessage::User {
                content: reflection_retry(&applied, &errors),
            });
        }
        commit_reflection(&repos, &touched);
        Ok(())
    }
}

/// Whether a finished request did enough work to be worth reflecting on.
pub(super) fn reflection_due(window: &[HarnessMessage]) -> bool {
    window
        .iter()
        .filter(|m| matches!(m, HarnessMessage::ToolResult { .. }))
        .count()
        >= 3
}

pub(super) const REFLECTION_READS: usize = 2;

pub(super) fn reflection_instruction(repos: &[crate::memory::MemoryRepo]) -> String {
    format!("{REFLECTION_INSTRUCTION}\n\nMemory now, by each repo's MEMORY.md and index:\n{}", render_indexes(repos))
}

pub(super) fn reflection_opened(repos: &[crate::memory::MemoryRepo], files: &[Value]) -> String {
    format!(
        "[reflection] The files you asked for:\n{}\nNow reply with only the {{\"changes\": [...]}} object.",
        read_topic_files(repos, files)
    )
}

/// The files a reflection reply asks to open first, from `{"read": [...]}`.
pub(super) fn parse_reflection_read(text: &str) -> Option<Vec<Value>> {
    let value = json_object(text)?;
    if value.get("changes").is_some() {
        return None;
    }
    value.get("read").and_then(Value::as_array).filter(|files| !files.is_empty()).cloned()
}

pub(super) fn reflection_retry(applied: &[(String, String)], errors: &[String]) -> String {
    format!(
        "[reflection] Already applied (do not resend):\n{}\n\nRejected; fix these and resend only them as the same JSON, or send {{\"changes\": []}} to drop them:\n{}",
        if applied.is_empty() {
            "(nothing)".to_string()
        } else {
            applied.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")
        },
        errors.join("\n")
    )
}

/// The `{"changes": [...]}` object in a reflection reply, fenced or bare.
pub(super) fn parse_reflection(text: &str) -> Option<Value> {
    let value = json_object(text)?;
    value.get("changes").is_some_and(Value::is_array).then_some(value)
}

fn json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

pub(super) fn commit_reflection(repos: &[crate::memory::MemoryRepo], touched: &[String]) {
    for repo in repos.iter().filter(|r| touched.contains(&r.key)) {
        if let Err(e) = repo.commit_all("Remember lessons from a session") {
            eprintln!("memory reflection commit {}: {e}", repo.key);
        }
    }
}

fn scope_name(kind: &crate::memory::ScopeKind) -> &'static str {
    match kind {
        crate::memory::ScopeKind::Global => "global",
        crate::memory::ScopeKind::Project => "project",
        crate::memory::ScopeKind::Agent => "agent",
        crate::memory::ScopeKind::Session => "session",
    }
}

pub(super) fn render_indexes(repos: &[crate::memory::MemoryRepo]) -> String {
    let mut out = String::new();
    for repo in repos {
        out.push_str(&format!(
            "\n=== scope: {} ({}) ===\n{}\n",
            scope_name(&repo.kind),
            repo.root.display(),
            clip(repo.entry_point().trim(), 4_000)
        ));
    }
    out
}

/// The text of the topic files a reflection or compaction asked to open, each
/// given as `{"scope", "file"}`.
pub(super) fn read_topic_files(repos: &[crate::memory::MemoryRepo], files: &[Value]) -> String {
    const MAX_FILES: usize = 8;
    const FILE_CHARS: usize = 6_000;
    let mut out = String::new();
    for item in files.iter().take(MAX_FILES) {
        let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let (scope, file) = (text("scope"), text("file"));
        let body = crate::memory::ScopeKind::parse(&scope)
            .and_then(|kind| repos.iter().find(|r| r.kind == kind))
            .ok_or_else(|| format!("no {scope:?} scope in this session"))
            .and_then(|repo| repo.read_file(&crate::memory::topic_file(&file)?))
            .unwrap_or_else(|e| format!("({e})"));
        out.push_str(&format!("--- {scope} {file} ---\n{}\n", clip(body.trim(), FILE_CHARS)));
    }
    if files.len() > MAX_FILES {
        out.push_str(&format!("({} more not opened; ask for at most {MAX_FILES})\n", files.len() - MAX_FILES));
    }
    out
}

/// Apply one update_memory call. Returns what was applied (repo key, line) and
/// what was rejected.
pub(super) fn apply_changes(
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
        let expires = text("expires");
        let expires = expires.trim();
        let stamped = |entry: String| {
            if entry.contains("[source:") {
                entry
            } else if is_date(expires) {
                format!("{} [source: {source}; added: {today}; expires: {expires}]", entry.trim())
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

fn is_date(text: &str) -> bool {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok()
}

/// The messages of the request that just ended: from its checkpoint when one
/// was taken since the last compaction, else from the last user message.
pub(super) fn request_window(state: &HarnessState) -> &[HarnessMessage] {
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

