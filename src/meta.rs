//! Conversation-agent meta tools.
//!
//! These are advertised to the top-level conversation agent and intercepted by
//! the harness loop *before* the generic [`crate::tools::ToolRegistry`], because
//! they steer the loop itself (end a turn, pause for input, spawn a lane) rather
//! than just producing a value. Delegated lanes never see them.

use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;

/// Names the harness loop must intercept instead of dispatching to the registry.
pub const META_TOOL_NAMES: [&str; 8] = [
    "update_plan",
    "ask_user",
    "delegate_task",
    "cancel_delegated_task",
    "complete_goal",
    "monitor",
    "present_file",
    "set_session_title",
];

pub fn is_meta_tool(name: &str) -> bool {
    META_TOOL_NAMES.contains(&name)
}

/// Tool definitions advertised to the conversation agent on top of the coding
/// tools. Ordinarily there is no terminate/complete tool — the agent finishes a
/// turn by replying with no tool calls. `complete_goal` is the one exception, and
/// it's only offered while an autonomous `/goal` is active (so it can end it).
pub fn conversation_meta_definitions(goal_active: bool) -> Vec<NativeToolDefinition> {
    conversation_meta_definitions_for(goal_active, true)
}

pub fn conversation_meta_definitions_for(
    goal_active: bool,
    allow_lane_control: bool,
) -> Vec<NativeToolDefinition> {
    let mut tools = vec![
        update_plan_tool(),
        ask_user_tool(),
        monitor_tool(),
        present_file_tool(),
        set_session_title_tool(),
    ];
    if allow_lane_control {
        tools.push(delegate_task_tool());
        tools.push(cancel_delegated_task_tool());
    }
    if goal_active {
        tools.push(complete_goal_tool());
    }
    tools
}

fn cancel_delegated_task_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "cancel_delegated_task".to_string(),
        description: "Stop one RUNNING delegated task and reclaim its scope before doing that work yourself. Use only when the user redirects, the work is now unnecessary, or you must deliberately take over. Give the lane_id and a concise reason. The lane's partial workspace changes are preserved; validate them before relying on them.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "lane_id": {"type": "string", "description": "ID of the running delegated task to stop."},
                "reason": {"type": "string", "description": "Why the parent is reclaiming this delegated scope."}
            },
            "required": ["lane_id", "reason"],
            "additionalProperties": false,
        }),
    }
}
fn set_session_title_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "set_session_title".to_string(),
        description: "Change the title of the current session. Use a short, descriptive title that reflects the user's request or the work underway. This is a harmless conversation action and does not require approval.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "The new session title. Use an empty string to clear the custom title."
                }
            },
            "required": ["title"],
            "additionalProperties": false
        }),
    }
}

fn present_file_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "present_file".to_string(),
        description: "Present a file to the user: it appears in the conversation as an openable \
            file card (in both the TUI and the app). Use it when a deliverable IS a file — a \
            report you wrote, a generated artifact, a diff, an image — instead of pasting its \
            contents into chat. The file must already exist (write it first). Present only the \
            file(s) that are the deliverable, not every file you touched. Presenting does not \
            end your turn — you still deliver your answer text as usual."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to present — workspace-relative or absolute."},
                "caption": {"type": "string", "description": "Optional one-line caption shown with the file."}
            },
            "required": ["path"]
        }),
    }
}

fn monitor_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "monitor".to_string(),
        description: "Watch a file and be woken with the text appended to it: how you wait on output you don't control (a build or test log, a long process's log). Register the watch, then end your turn; each matching append arrives as a [file_watch] message. Always set a `filter` regex for the lines you need (failures and completion), since every wake costs a model turn; best is a sentinel you append yourself, e.g. `<cmd>; echo \"__DONE__ exit=$?\" >> build.log` with filter `__DONE__`. Remove the watch when it has served its purpose. Actions: add (default), remove, list."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "remove", "list"],
                    "description": "add registers a watch (default); remove deletes one (by watch_id, path, or label); list shows active watches."
                },
                "path": {
                    "type": "string",
                    "description": "File to watch (absolute, or relative to the workspace). May not exist yet — the watch fires once it's created and written."
                },
                "label": {
                    "type": "string",
                    "description": "A short, specific subject YOU choose for this watch — e.g. 'watch the build log'. This is how it's shown to the user; always refer to it by this subject."
                },
                "filter": {
                    "type": "string",
                    "description": "Regex: wake only when the appended text matches, ideally your own completion sentinel."
                },
                "watch_id": {
                    "type": "string",
                    "description": "For action:\"remove\" — the id from the add result or the [file_watch] follow_up_id."
                }
            },
            "required": []
        }),
    }
}

fn complete_goal_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "complete_goal".to_string(),
        description: "Finish the current /goal once every part of it is done and verified; the loop then \
            stops nudging you to continue. Pass a short `summary` of what you accomplished, which \
            the user sees. A pause, a question or remaining work isn't a finish: keep going, or say \
            what you need."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "A short summary of what the goal accomplished (shown to the user)."
                }
            },
            "required": ["summary"],
            "additionalProperties": false,
        }),
    }
}

/// Explicit completion tool for HEADLESS runs (delegated lanes, one-shot
/// `run()`) — not advertised to the user-facing conversation agent. A lane ends
/// by calling this with a deliberate `summary` (the structured handoff folded
/// back into the parent), so its report is never just "whatever the last prose
/// happened to be". Replying with no tool calls also ends a run as a fallback.
pub fn terminate_loop_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "terminate_loop".to_string(),
        description: "End your run and hand back a summary of what you did and found. Call this \
            once the task is finished: `summary` is a tight account of the outcome, key decisions, \
            and any blockers — it is what the caller reads. Finish your actual work first, then \
            call `terminate_loop`."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Tight account of the outcome, key decisions, and any blockers — what the caller reads."
                }
            },
            "required": ["summary"],
            "additionalProperties": false,
        }),
    }
}

fn update_plan_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "update_plan".to_string(),
        description: "Keep a short, visible plan for work with several distinct steps; the user \
            sees it as a checklist. Send the whole list every time: short concrete steps, each \
            `pending`, `in_progress` or `done`, with exactly one `in_progress` while you work. Mark \
            a step done as soon as it is, and reshape the list when you learn something that \
            changes the work (say why in `explanation`). Skip it for small tasks. Updating the \
            plan is not progress by itself: after updating it, do the work."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 12,
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": { "type": "string", "description": "A short, concrete step." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                },
                "explanation": {
                    "type": "string",
                    "description": "Optional: why the plan changed."
                }
            },
            "required": ["steps"],
            "additionalProperties": false,
        }),
    }
}

fn ask_user_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "ask_user".to_string(),
        description: "Ask the user something: a clarification, a choice, a confirmation or a missing fact. A question asked here pauses the turn and reaches the user properly, which a plain-text question doesn't; ask once context, tools or a sensible default can't settle it. It ends your turn until they answer. Pick each question's `answer_kind.kind` by the shape of the answer: free_text, single_choice or multi_choice (with `choices`: a short `label`, a one-line `description`, `recommended: true` on your pick), yes_no, or confirm (a gate before an irreversible action). Give each question a one- or two-word `header` when you ask several."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more questions presented together.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Optional; only to tell several questions apart."},
                            "text": {"type": "string", "description": "Question text shown to the user."},
                            "header": {"type": "string", "description": "One or two words naming the question when several are asked together."},
                            "answer_kind": {
                                "type": "object",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["free_text", "single_choice", "multi_choice", "yes_no", "confirm"],
                                        "description": "The answer's shape."
                                    },
                                    "choices": {
                                        "type": "array",
                                        "description": "Required for single_choice and multi_choice, most likely first.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "value": {"type": "string"},
                                                "label": {"type": "string", "description": "A few words."},
                                                "description": {"type": "string", "description": "One line on what choosing this means or costs."},
                                                "recommended": {"type": "boolean", "description": "The option you would pick; shown first and preselected. At most one for single_choice."}
                                            },
                                            "required": ["value", "label"]
                                        }
                                    },
                                    "confirm_label": {"type": "string", "description": "confirm: label for the confirm action."},
                                    "cancel_label": {"type": "string", "description": "confirm: label for the cancel action."}
                                },
                                "required": ["kind"]
                            }
                        },
                        "required": ["text", "answer_kind"]
                    }
                },
                "context": {
                    "type": "string",
                    "description": "Optional one-paragraph explanation of why you're asking, shown above the questions."
                }
            },
            "required": ["questions"],
            "additionalProperties": false,
        }),
    }
}

fn delegate_task_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "delegate_task".to_string(),
        description: "Hand a self-contained piece of work to a background lane: a fresh sub-agent that runs in parallel and reports back with file:line evidence. Use it to fan out independent areas or a long investigation while keeping your own context lean. The brief names the scope and the concrete deliverable. Carry on with your own share of the work meanwhile; when only waiting is left, end your turn, and each report wakes you. Your final answer waits for the lanes it depends on. Pass `lane_id` to talk to a lane you started: a running lane gets your description as a message (an answer to its question, a change of plan), and a finished one picks up again as a follow-up with its context intact. Set access `read_only` for investigation and review (its file-editing tools are removed)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "A 2–5 word label for the lane, shown to the user (e.g. 'audit auth flow'). Required for a new lane."
                },
                "description": {
                    "type": "string",
                    "description": "The brief. State what to inspect/do, what to ignore, and the concrete output expected (e.g. a file to write or a finding to report). Minimum ~80 chars. With lane_id: your message to that lane, any length."
                },
                "lane_id": {
                    "type": "string",
                    "description": "Talk to this lane instead of starting a new one: a running lane gets the description as a message; a finished one continues with it as a follow-up, context intact."
                },
                "access": {
                    "type": "string",
                    "enum": ["full", "read_only"],
                    "description": "read_only removes the lane's file-editing tools (investigation/review lanes). Default full."
                },
                "agent": {
                    "type": "string",
                    "description": "Optional specialized agent identity or role name for this lane (e.g. 'reviewer', 'researcher', 'security')."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional inference profile; omit to stay on your model and its prompt cache."
                }
            },
            "required": ["description"],
            "additionalProperties": false,
        }),
    }
}

// --- Validation ---

const MIN_DELEGATE_DESCRIPTION_CHARS: usize = 40;

pub struct DelegateBrief {
    pub title: String,
    pub description: String,
    /// Continue this existing lane with `description` as a follow-up.
    pub lane_id: Option<String>,
    /// Strip the lane's file-mutation tools (investigation lanes).
    pub read_only: bool,
    /// Specialized agent identity or role name.
    pub agent: Option<String>,
    /// Optional inference profile name.
    pub profile: Option<String>,
}

/// Validate a `delegate_task` payload: the brief must state both a scope
/// boundary and an expected deliverable, so the lane can't drift.
/// Returns a user-correctable message on failure (fed back to the
/// model as a tool error so it self-corrects next turn).
pub fn parse_delegate_brief(arguments: &Value) -> Result<DelegateBrief, String> {
    let lane_id = arguments
        .get("lane_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let agent = arguments
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let profile = arguments
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let read_only = match arguments
        .get("access")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        None | Some("") | Some("full") => false,
        Some("read_only") => true,
        Some(other) => {
            return Err(format!(
                "delegate_task `access` must be `full` or `read_only`, not `{other}`."
            ));
        }
    };
    let title = arguments
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    // A follow-up reuses the existing lane's title; a new lane needs one.
    if title.is_empty() && lane_id.is_none() {
        return Err(
            "delegate_task requires a non-empty `title` (or a `lane_id` to follow up).".to_string(),
        );
    }

    let description = arguments
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    // Length-check the collapsed form but DELIVER the original text: collapsing
    // all whitespace destroyed the brief's structure (lists, code blocks,
    // paragraphs) before the lane ever saw it.
    let collapsed_len = description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .count();
    if lane_id.is_some() && description.is_empty() {
        return Err("delegate_task with a lane_id needs a description: the message for that lane.".to_string());
    }
    if lane_id.is_none() && collapsed_len < MIN_DELEGATE_DESCRIPTION_CHARS {
        return Err(format!(
            "delegate_task needs a brief describing what the lane should do and what it should \
             produce (at least {MIN_DELEGATE_DESCRIPTION_CHARS} characters — a sentence or two)."
        ));
    }

    Ok(DelegateBrief {
        title,
        description,
        lane_id,
        read_only,
        agent,
        profile,
    })
}

/// Validate an `ask_user` payload: at least one question, unique ids, non-empty
/// text, and `single_choice` carries choices. Returns the rendered question set
/// (as JSON) on success.
pub fn parse_ask_user(arguments: &Value) -> Result<Value, String> {
    let questions = arguments
        .get("questions")
        .and_then(Value::as_array)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| "ask_user requires a non-empty `questions` array.".to_string())?;

    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(questions.len());
    for (i, question) in questions.iter().enumerate() {
        // `id` is OPTIONAL — it only exists to key answers when several questions
        // are asked together. When the model omits it (the common single-question
        // case), default to the index so it never has to invent ceremony ids.
        let id = question
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("q{i}"));
        if !seen.insert(id.clone()) {
            return Err(format!("ask_user: duplicate question id `{id}`."));
        }
        if question
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            return Err(format!("ask_user: question `{id}` needs non-empty `text`."));
        }
        let kind = question
            .get("answer_kind")
            .and_then(|k| k.get("kind"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("ask_user: question `{id}` needs `answer_kind.kind`."))?;
        const KINDS: [&str; 5] = ["free_text", "single_choice", "multi_choice", "yes_no", "confirm"];
        if !KINDS.contains(&kind) {
            return Err(format!(
                "ask_user: question `{id}` has unknown answer kind `{kind}`; use one of {}.",
                KINDS.join(", ")
            ));
        }
        if kind == "single_choice" || kind == "multi_choice" {
            let has_choices = question
                .get("answer_kind")
                .and_then(|k| k.get("choices"))
                .and_then(Value::as_array)
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if !has_choices {
                return Err(format!(
                    "ask_user: question `{id}` is {kind} and requires non-empty `choices`."
                ));
            }
        }
        // Emit with the effective id filled in, so clients always have one to key by.
        let mut q = question.clone();
        if let Some(obj) = q.as_object_mut() {
            obj.insert("id".to_string(), Value::String(id));
            // A header is a tab label: keep it short whatever the model sent.
            if let Some(h) = obj.get("header").and_then(Value::as_str) {
                let h: String = h.trim().chars().take(16).collect();
                if h.is_empty() {
                    obj.remove("header");
                } else {
                    obj.insert("header".to_string(), Value::String(h));
                }
            }
        }
        out.push(q);
    }

    Ok(json!({
        "questions": out,
        "context": arguments.get("context").cloned().unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_user_accepts_multi_choice_and_trims_headers() {
        let parsed = parse_ask_user(&json!({
            "questions": [
                {"text": "Which targets?", "header": "   Build targets for release   ",
                 "answer_kind": {"kind": "multi_choice", "choices": [
                     {"value": "android", "label": "Android", "recommended": true},
                     {"value": "macos", "label": "macOS"}]}},
                {"text": "Ship now?", "header": "  ", "answer_kind": {"kind": "yes_no"}}
            ]
        }))
        .expect("valid");
        let qs = parsed["questions"].as_array().unwrap();
        assert_eq!(qs[0]["header"], "Build targets fo");
        assert!(qs[1].get("header").is_none());
    }

    #[test]
    fn ask_user_rejects_multi_choice_without_choices_and_unknown_kinds() {
        let err = parse_ask_user(&json!({"questions": [
            {"text": "Pick", "answer_kind": {"kind": "multi_choice"}}]}))
        .unwrap_err();
        assert!(err.contains("multi_choice"));
        let err = parse_ask_user(&json!({"questions": [
            {"text": "Pick", "answer_kind": {"kind": "slider"}}]}))
        .unwrap_err();
        assert!(err.contains("unknown answer kind"));
    }

    #[test]
    fn test_parse_delegate_brief_with_agent() {
        let payload = json!({
            "title": "security audit",
            "description": "Inspect all authentication endpoints and verify timing-safe token comparison is applied.",
            "access": "read_only",
            "agent": "security"
        });
        let brief = parse_delegate_brief(&payload).expect("should parse");
        assert_eq!(brief.title, "security audit");
        assert_eq!(brief.read_only, true);
        assert_eq!(brief.agent.as_deref(), Some("security"));
        assert!(brief.lane_id.is_none());
    }

    #[test]
    fn test_parse_delegate_brief_without_agent() {
        let payload = json!({
            "title": "refactor handlers",
            "description": "Refactor route handlers to use the shared error type and return structured responses."
        });
        let brief = parse_delegate_brief(&payload).expect("should parse");
        assert_eq!(brief.title, "refactor handlers");
        assert_eq!(brief.read_only, false);
        assert_eq!(brief.agent, None);
        assert_eq!(brief.profile, None);
    }

    #[test]
    fn test_parse_delegate_brief_with_profile() {
        let payload = json!({
            "title": "explore dependencies",
            "description": "Examine Cargo.toml and lockfile to map dependency tree versions and vulnerabilities.",
            "profile": "claude-haiku"
        });
        let brief = parse_delegate_brief(&payload).expect("should parse");
        assert_eq!(brief.profile.as_deref(), Some("claude-haiku"));
    }
}
