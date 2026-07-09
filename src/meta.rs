//! Conversation-agent meta tools.
//!
//! These are advertised to the top-level conversation agent and intercepted by
//! the harness loop *before* the generic [`crate::tools::ToolRegistry`], because
//! they steer the loop itself (end a turn, pause for input, spawn a lane) rather
//! than just producing a value. Delegated lanes never see them.
//!
//! Ported in shape from wacht's `executor/agent_loop/meta_tools` — the
//! platform-coupled bits (DB writes, NATS, board items) are dropped; the
//! infra-free validation discipline is kept.

use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;

/// Names the harness loop must intercept instead of dispatching to the registry.
pub const META_TOOL_NAMES: [&str; 5] =
    ["note", "ask_user", "delegate_task", "complete_goal", "monitor"];

pub fn is_meta_tool(name: &str) -> bool {
    META_TOOL_NAMES.contains(&name)
}

/// Tool definitions advertised to the conversation agent on top of the coding
/// tools. Ordinarily there is no terminate/complete tool — the agent finishes a
/// turn by replying with no tool calls. `complete_goal` is the one exception, and
/// it's only offered while an autonomous `/goal` is active (so it can end it).
pub fn conversation_meta_definitions(goal_active: bool) -> Vec<NativeToolDefinition> {
    let mut tools = vec![note_tool(), ask_user_tool(), delegate_task_tool(), monitor_tool()];
    if goal_active {
        tools.push(complete_goal_tool());
    }
    tools
}

fn monitor_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "monitor".to_string(),
        description: "Watch a file and be WOKEN with whatever text gets appended to it — the way \
            to wait on output you don't control (a build log, test output, a long process's log, \
            a file another program writes). Register the watch, then END YOUR TURN: going idle is \
            how you wait; each append arrives later as a [file_watch] message carrying the new \
            text. Do NOT poll the file with read_file in a loop. An optional `filter` regex makes \
            the wake fire only when the appended text matches (e.g. \"error|FAILED|passed\") — \
            non-matching output is consumed silently. Appends are debounced, so one burst = one \
            wake. Remove the watch when it has served its purpose. Actions: add (default) | \
            remove | list."
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
                    "description": "Optional regex; wake only when the appended chunk matches it. Omit to wake on every append."
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
        description: "End the current autonomous /goal. Call this ONLY when the goal is genuinely \
            100% COMPLETE — every part done and verified — so the loop stops re-prompting you to \
            continue. Pass a short `summary` of what you accomplished (shown to the user). Do NOT \
            call it to pause, to ask a question, or while any work remains."
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

fn note_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "note".to_string(),
        description: "Write a private note to yourself, recorded in history so you can read it \
            back on a later turn. Use it to plan a multi-step sequence, record an observation from \
            a tool result, or anchor a decision. Notes do NOT execute work, are NOT shown to the \
            user as an answer, and do NOT end the turn. After a note, act on the next turn — do not \
            take notes repeatedly without making progress."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "entry": {
                    "type": "string",
                    "description": "The note content. Specific and grounded in what you just observed."
                }
            },
            "required": ["entry"],
            "additionalProperties": false,
        }),
    }
}

fn ask_user_tool() -> NativeToolDefinition {
    NativeToolDefinition {
        name: "ask_user".to_string(),
        description: "The only channel for asking the user anything (clarification, choice, \
            confirmation, missing fact). Never end a turn with a question in plain text — use this \
            tool. Last resort: prefer resolving via other tools, context, or a sensible default. \
            Ends the turn and pauses until answered; one pending question set at a time. Each \
            question needs an `id`, `text`, and `answer_kind.kind` chosen by the SHAPE of the \
            answer: free_text (open-ended), single_choice (one of a known set; provide `choices`), \
            yes_no (literal yes/no), or confirm (irreversible action gate)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more questions presented together. IDs must be unique within the set.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Stable id; the answer references it."},
                            "text": {"type": "string", "description": "Question text shown to the user."},
                            "answer_kind": {
                                "type": "object",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["free_text", "single_choice", "yes_no", "confirm"],
                                        "description": "Discriminator selecting the answer shape."
                                    },
                                    "choices": {
                                        "type": "array",
                                        "description": "single_choice: REQUIRED options, ordered by likelihood. Each has a `value` and a `label`.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "value": {"type": "string"},
                                                "label": {"type": "string"},
                                                "description": {"type": "string"}
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
                        "required": ["id", "text", "answer_kind"]
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
        description: "Hand a scoped, self-contained unit of work to a background lane — a fresh \
            coding sub-agent that runs to completion in PARALLEL and reports back (its findings \
            cited with exact file:line). Delegating makes you an ORCHESTRATOR: spawn SEVERAL lanes \
            to cover breadth and keep YOUR OWN context lean (the lanes hold the detail; you keep the \
            conclusions). REACH FOR THIS when the work splits into independent areas (fan them out \
            instead of grinding serially), or a self-contained investigation/build will take many \
            steps. The brief must name BOTH the scope to inspect/act on AND the concrete deliverable \
            expected; a vague brief produces vague work. \
            Ending your turn IS how you WAIT for lanes — you go idle and each report wakes you (no \
            polling). Keep the user posted with a short progress note; just don't present your \
            COMPLETE/final answer while lanes you need are still running (your [delegated_lanes] \
            context lists them). Fold each report in and synthesize once they're in — progressively \
            or all at once. Only skip delegation for trivial one-step actions you can just do yourself. \
            CONTINUING: pass `lane_id` (from an earlier delegation) to send a FOLLOW-UP to a finished \
            lane — it resumes with its full context intact, so use it for 'now also check X' or \
            'apply the fix you proposed' instead of re-briefing a fresh lane from scratch. \
            SCOPING: set access='read_only' for pure investigation/search/review lanes (their \
            file-editing tools are removed) — prefer it whenever the lane shouldn't change anything; \
            several read-only lanes can safely fan out in parallel while you keep editing."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "A short, specific label YOU choose for this lane (2–5 words) that says what it's doing — e.g. 'audit auth flow', 'extract CLI modules', 'review error handling'. Shown to the user and in your [delegated_lanes] context, so make it descriptive, not generic ('investigate', 'task 1'). Required for a new lane; ignored when lane_id is set."
                },
                "description": {
                    "type": "string",
                    "description": "The brief. State what to inspect/do, what to ignore, and the concrete output expected (e.g. a file to write or a finding to report). Minimum ~80 chars. With lane_id: the follow-up instruction."
                },
                "lane_id": {
                    "type": "string",
                    "description": "Continue this FINISHED lane with the description as a follow-up (context intact) instead of starting a new lane."
                },
                "access": {
                    "type": "string",
                    "enum": ["full", "read_only"],
                    "description": "read_only removes the lane's file-editing tools (investigation/review lanes). Default full."
                }
            },
            "required": ["description"],
            "additionalProperties": false,
        }),
    }
}

// --- Validation (infra-free port of wacht's delegation / ask_user gates) ---

const MIN_DELEGATE_DESCRIPTION_CHARS: usize = 40;

pub struct DelegateBrief {
    pub title: String,
    pub description: String,
    /// Continue this existing lane with `description` as a follow-up.
    pub lane_id: Option<String>,
    /// Strip the lane's file-mutation tools (investigation lanes).
    pub read_only: bool,
}

/// Validate a `delegate_task` payload. Mirrors wacht's `validate_delegate_description`:
/// the brief must state both a scope boundary and an expected deliverable, so the
/// lane can't drift. Returns a user-correctable message on failure (fed back to the
/// model as a tool error so it self-corrects next turn).
pub fn parse_delegate_brief(arguments: &Value) -> Result<DelegateBrief, String> {
    let lane_id = arguments
        .get("lane_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let read_only = match arguments.get("access").and_then(Value::as_str).map(str::trim) {
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
        return Err("delegate_task requires a non-empty `title` (or a `lane_id` to follow up).".to_string());
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
    if collapsed_len < MIN_DELEGATE_DESCRIPTION_CHARS {
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
    for question in questions {
        let id = question
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "ask_user: each question needs a non-empty `id`.".to_string())?;
        if !seen.insert(id.to_string()) {
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
        if kind == "single_choice" {
            let has_choices = question
                .get("answer_kind")
                .and_then(|k| k.get("choices"))
                .and_then(Value::as_array)
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if !has_choices {
                return Err(format!(
                    "ask_user: question `{id}` is single_choice and requires non-empty `choices`."
                ));
            }
        }
    }

    Ok(json!({
        "questions": questions,
        "context": arguments.get("context").cloned().unwrap_or(Value::Null),
    }))
}
