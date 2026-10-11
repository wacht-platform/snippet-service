use super::*;

impl CodingHarness {
    pub(super) async fn run_agentic_summary(
        &self,
        model: &mut dyn AgentModel,
        window_text: &str,
    ) -> Result<String, ToolError> {
        const MAX_TURNS: usize = 4;
        const BUDGET_CHARS: usize = 9_000;
        const MEMORY_CHARS: usize = 16_000;

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
                    format!(
                        "MEMORY REPOS:\n{}\n\n",
                        clip(&render_repos(&repos), MEMORY_CHARS)
                    )
                } else if !saved.is_empty() {
                    format!(
                        "ALREADY SAVED TO MEMORY (point to these, do not resave):\n{}\n\n",
                        saved.join("\n")
                    )
                } else {
                    String::new()
                };
                let ask = if memory_open {
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
            }
            let output = model.generate(&messages, &tools, true, None).await?;

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
        for _ in 0..2 {
            let output = model.generate(&messages, &definitions, false, None).await?;
            let reply = output.content_text.clone().unwrap_or_default();
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
            if errors.is_empty() {
                break;
            }
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

pub(super) fn reflection_instruction(repos: &[crate::memory::MemoryRepo]) -> String {
    let mut files = String::new();
    for repo in repos {
        let scope = scope_name(&repo.kind);
        let names: Vec<String> = repo.files().into_iter().filter(|f| f.ends_with(".md")).collect();
        files.push_str(&format!("- {scope} ({}): {}\n", repo.root.display(), names.join(", ")));
        if repo.kind == crate::memory::ScopeKind::Session {
            for file in names {
                let text = repo.read_file(&file).unwrap_or_default();
                files.push_str(&format!("  --- {file} ---\n{}\n", clip(text.trim(), 4_000)));
            }
        }
    }
    format!("{REFLECTION_INSTRUCTION}\n\nMemory files now:\n{files}")
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
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    value.get("changes").is_some_and(Value::is_array).then_some(value)
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

