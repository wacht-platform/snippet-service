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
pub const META_TOOL_NAMES: [&str; 4] = ["note", "ask_user", "delegate_task", "terminate_loop"];

pub fn is_meta_tool(name: &str) -> bool {
    META_TOOL_NAMES.contains(&name)
}

/// Tool definitions advertised to the conversation agent on top of the coding
/// tools. `terminate_loop` is intentionally absent here — it already ships as a
/// coding tool definition; the loop reinterprets it for conversation turn-control.
/// There is no separate "reply"/notify tool: the agent talks to the user with text
/// beside `terminate_loop`, and progress shows as text beside working tool calls.
pub fn conversation_meta_definitions() -> Vec<NativeToolDefinition> {
    vec![note_tool(), ask_user_tool(), delegate_task_tool()]
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
        description: "Hand a scoped, self-contained unit of work to a background lane. The lane \
            runs a fresh coding agent to completion in parallel and reports a summary back to you \
            when done — you keep working in the meantime and may delegate several lanes at once. \
            The brief must name BOTH the scope to inspect/act on AND the concrete deliverable \
            expected; a vague brief produces vague work. Do not delegate trivial one-step actions \
            you can do yourself."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short label for the lane (a few words)."
                },
                "description": {
                    "type": "string",
                    "description": "The brief. State what to inspect/do, what to ignore, and the concrete output expected (e.g. a file to write or a finding to report). Minimum ~80 chars."
                }
            },
            "required": ["title", "description"],
            "additionalProperties": false,
        }),
    }
}

// --- Validation (infra-free port of wacht's delegation / ask_user gates) ---

const MIN_DELEGATE_DESCRIPTION_CHARS: usize = 40;

pub struct DelegateBrief {
    pub title: String,
    pub description: String,
}

/// Validate a `delegate_task` payload. Mirrors wacht's `validate_delegate_description`:
/// the brief must state both a scope boundary and an expected deliverable, so the
/// lane can't drift. Returns a user-correctable message on failure (fed back to the
/// model as a tool error so it self-corrects next turn).
pub fn parse_delegate_brief(arguments: &Value) -> Result<DelegateBrief, String> {
    let title = arguments
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if title.is_empty() {
        return Err("delegate_task requires a non-empty `title`.".to_string());
    }

    let normalized = arguments
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() < MIN_DELEGATE_DESCRIPTION_CHARS {
        return Err(format!(
            "delegate_task needs a brief describing what the lane should do and what it should \
             produce (at least {MIN_DELEGATE_DESCRIPTION_CHARS} characters — a sentence or two)."
        ));
    }

    Ok(DelegateBrief {
        title,
        description: normalized,
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
