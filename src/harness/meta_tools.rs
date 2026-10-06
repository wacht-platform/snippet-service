use super::*;

impl CodingHarness {
    pub(super) fn dispatch_meta(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        tool_name: &str,
        arguments: &Value,
    ) -> (Value, MetaControl) {
        match tool_name {
            "cancel_delegated_task" => {
                let lane_id = arguments
                    .get("lane_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let reason = arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty());
                match (lane_id, reason) {
                    (Some(lane_id), Some(reason)) => match lanes.cancel(lane_id, reason) {
                        Ok(title) => {
                            state.events.push(HarnessEvent::LaneCancelled {
                                id: lane_id.to_string(),
                                title: title.clone(),
                                reason: reason.to_string(),
                            });
                            (
                                json!({"schema_version": 1, "status": "success", "data": {
                                    "cancelled": true,
                                    "lane_id": lane_id,
                                    "title": title,
                                    "note": "The delegated scope is now yours. Partial workspace changes were preserved; inspect and validate them before continuing."
                                }}),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    },
                    _ => (
                        tool_error(
                            "cancel_delegated_task requires non-empty `lane_id` and `reason`.",
                        ),
                        MetaControl::Continue,
                    ),
                }
            }
            "update_plan" => match parse_plan(arguments) {
                Ok(steps) => {
                    let done = steps.iter().filter(|s| s.status == PlanStatus::Done).count();
                    let total = steps.len();
                    let explanation = arguments
                        .get("explanation")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    state.plan = steps.clone();
                    state.events.push(HarnessEvent::PlanUpdated { steps, explanation });
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"done": done, "total": total}}),
                        MetaControl::Continue,
                    )
                }
                Err(message) => (tool_error(&message), MetaControl::Continue),
            },
            "set_session_title" => {
                let title = arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim);
                let Some(title) = title else {
                    return (
                        tool_error("set_session_title requires a `title` string."),
                        MetaControl::Continue,
                    );
                };
                state.title = (!title.is_empty()).then(|| title.to_string());
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "renamed": true,
                        "title": state.title,
                    }}),
                    MetaControl::Continue,
                )
            }
            "present_file" => {
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(path) = path else {
                    return (
                        tool_error("present_file requires a `path`."),
                        MetaControl::Continue,
                    );
                };
                let resolved = if std::path::Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    self.context.workspace_root().join(path)
                };
                // Only real files get presented — a hallucinated or not-yet-written
                // path fails loudly so the agent writes the file first.
                if !resolved.is_file() {
                    return (
                        tool_error(format!(
                            "present_file: `{path}` does not exist — write the file before presenting it."
                        )),
                        MetaControl::Continue,
                    );
                }
                let caption = arguments
                    .get("caption")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let shown = resolved.display().to_string();
                state.events.push(HarnessEvent::FilePresented {
                    path: shown.clone(),
                    caption,
                });
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "presented": shown,
                        "note": "shown to the user as an openable file card; continue your turn as usual",
                    }}),
                    MetaControl::Continue,
                )
            }
            "complete_goal" => {
                let summary = arguments
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match state.goal.as_mut() {
                    Some(goal) if goal.status == GoalStatus::Active => {
                        goal.status = GoalStatus::Complete;
                        let text = goal.text.clone();
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "goal_completed".to_string(),
                            reasoning: if summary.is_empty() {
                                text
                            } else {
                                summary.clone()
                            },
                        });
                        (
                            json!({"schema_version": 1, "status": "success", "data": {"completed": true}}),
                            MetaControl::EndTurn {
                                kind: TurnEndKind::Complete,
                                final_text: Some(if summary.is_empty() {
                                    "Goal complete.".to_string()
                                } else {
                                    summary
                                }),
                            },
                        )
                    }
                    _ => (
                        tool_error("complete_goal: there is no active goal to complete."),
                        MetaControl::Continue,
                    ),
                }
            }
            "ask_user" => match parse_ask_user(arguments) {
                Ok(rendered) => {
                    let prompt_text = first_question_text(&rendered)
                        .or_else(|| {
                            rendered
                                .get("context")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "Waiting for your input.".to_string());
                    state.events.push(HarnessEvent::UserQuestion {
                        questions: rendered.clone(),
                    });
                    state.pending_question = Some(rendered);
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"asked": true}}),
                        MetaControl::EndTurn {
                            kind: TurnEndKind::Ask,
                            final_text: Some(prompt_text),
                        },
                    )
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "delegate_task" => match parse_delegate_brief(arguments) {
                Ok(brief) => {
                    // Follow-up to an existing lane: resume it with the new brief,
                    // context intact.
                    if let Some(lane_id) = brief.lane_id.as_deref() {
                        return match lanes.follow_up(lane_id, &brief.description) {
                            Ok(title) => {
                                state.events.push(HarnessEvent::LaneSpawned {
                                    id: lane_id.to_string(),
                                    title: title.clone(),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "continued": true,
                                            "lane_id": lane_id,
                                            "title": title,
                                            "note": "Lane resumed with its prior context; its report will arrive as a [lane_report] message.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        };
                    }
                    match lanes.spawn(
                        &brief.title,
                        &brief.description,
                        brief.read_only,
                        brief.agent.clone(),
                        brief.profile.clone(),
                    ) {
                        Ok(id) => {
                            state.events.push(HarnessEvent::LaneSpawned {
                                id: id.clone(),
                                title: brief.title.clone(),
                            });
                            let mut data = json!({
                                "delegated": true,
                                "lane_id": id,
                                "title": brief.title,
                                "access": if brief.read_only { "read_only" } else { "full" },
                                "note": "Lane runs in the background; its report will arrive as a [lane_report] message. Follow up later by re-calling delegate_task with this lane_id.",
                            });
                            if let Some(ref agent) = brief.agent {
                                data["agent"] = json!(agent);
                            }
                            if let Some(ref profile) = brief.profile {
                                data["profile"] = json!(profile);
                            }
                            (
                                json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": data,
                                }),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    }
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "monitor" => {
                let action = arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("add");
                match action {
                    "add" => {
                        let Some(path) = arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            return (
                                tool_error("monitor add requires a `path`."),
                                MetaControl::Continue,
                            );
                        };
                        let label = arguments
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(path);
                        let filter = arguments
                            .get("filter")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        match watches.add(path, label, filter) {
                            Ok(record) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_added".to_string(),
                                    reasoning: format!(
                                        "watching \"{}\" ({})",
                                        record.label, record.path
                                    ),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "watching": true,
                                            "watch_id": record.id,
                                            "label": record.label,
                                            "path": record.path,
                                            "note": "Tailing from the current end of file. Appended text arrives as a [file_watch] message (debounced per burst); ending your turn is how you wait for it.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "remove" => {
                        let key = arguments
                            .get("watch_id")
                            .or_else(|| arguments.get("path"))
                            .or_else(|| arguments.get("label"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let Some(key) = key else {
                            return (
                                tool_error(
                                    "monitor remove needs a `watch_id`, `path`, or `label`.",
                                ),
                                MetaControl::Continue,
                            );
                        };
                        match watches.remove(key) {
                            Ok(label) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_removed".to_string(),
                                    reasoning: format!("stopped watching \"{label}\""),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": { "removed": true, "label": label }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "list" => (
                        json!({
                            "schema_version": 1,
                            "status": "success",
                            "data": {
                                "watches": watches.records().iter().map(|r| json!({
                                    "watch_id": r.id,
                                    "label": r.label,
                                    "path": r.path,
                                    "filter": r.filter,
                                })).collect::<Vec<_>>(),
                            }
                        }),
                        MetaControl::Continue,
                    ),
                    other => (
                        tool_error(format!(
                            "monitor action must be add | remove | list, got `{other}`."
                        )),
                        MetaControl::Continue,
                    ),
                }
            }
            other => (
                tool_error(format!("`{other}` is not a recognized meta tool.")),
                MetaControl::Continue,
            ),
        }
    }

    pub(super) fn definitions_for(
        &self,
        conversation_mode: bool,
        goal_active: bool,
    ) -> Vec<crate::llm::NativeToolDefinition> {
        let mut definitions = self.tools.definitions();
        if conversation_mode {
            // User-facing: meta tools (note/ask_user/delegate); no terminate tool —
            // a plain reply ends the turn. `complete_goal` is added only while a goal runs.
            definitions.extend(
                meta::conversation_meta_definitions_for(goal_active, self.config.allow_lane_control)
                    .into_iter()
                    .filter(|d| !self.config.hidden_meta.contains(&d.name.as_str())),
            );
        } else {
            // Headless (lanes / one-shot run): an explicit terminate_loop carries a
            // structured summary back to the caller.
            definitions.push(meta::terminate_loop_tool());
        }
        definitions
    }
}

/// Validate an `update_plan` call: 1–12 non-empty steps, at most one in progress.
pub(super) fn parse_plan(arguments: &Value) -> Result<Vec<PlanStep>, String> {
    let steps: Vec<PlanStep> = arguments
        .get("steps")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("update_plan `steps` must be a list of {{step, status}}: {e}"))?
        .unwrap_or_default();
    if steps.is_empty() || steps.len() > 12 {
        return Err("update_plan needs between 1 and 12 steps.".to_string());
    }
    if steps.iter().any(|s| s.step.trim().is_empty()) {
        return Err("every plan step needs text.".to_string());
    }
    if steps.iter().filter(|s| s.status == PlanStatus::InProgress).count() > 1 {
        return Err("mark at most one step `in_progress`.".to_string());
    }
    Ok(steps
        .into_iter()
        .map(|s| PlanStep {
            step: s.step.trim().to_string(),
            status: s.status,
        })
        .collect())
}
