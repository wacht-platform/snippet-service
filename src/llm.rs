use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolError;

/// Live text the model is currently streaming, shared between the model (writer)
/// and the TUI (reader). The model appends visible text deltas as they arrive;
/// the UI renders the in-progress text and clears it once the turn commits to
/// durable state. Only the interactive conversation sets a sink — lanes and
/// one-shot runs stream nothing and keep the buffered path.
#[derive(Default)]
pub struct StreamBuffer {
    /// The visible answer the model is streaming.
    pub text: String,
    /// Reasoning/thinking tokens, when the provider returns them — shown dimmed,
    /// separate from the answer. Best-effort: empty for models that don't emit it.
    pub thinking: String,
}

pub type StreamHandle = Arc<Mutex<StreamBuffer>>;

impl StreamBuffer {
    pub fn append(handle: &StreamHandle, delta: &str) {
        if let Ok(mut buf) = handle.lock() {
            buf.text.push_str(delta);
        }
    }

    pub fn append_thinking(handle: &StreamHandle, delta: &str) {
        if let Ok(mut buf) = handle.lock() {
            buf.thinking.push_str(delta);
        }
    }

    pub fn clear(handle: &StreamHandle) {
        if let Ok(mut buf) = handle.lock() {
            buf.text.clear();
            buf.thinking.clear();
        }
    }

    pub fn snapshot(handle: &StreamHandle) -> String {
        handle.lock().map(|buf| buf.text.clone()).unwrap_or_default()
    }

    pub fn snapshot_thinking(handle: &StreamHandle) -> String {
        handle.lock().map(|buf| buf.thinking.clone()).unwrap_or_default()
    }
}

/// Split an inlined image out of a `read_image` tool-result value. The harness
/// stamps base64 image bytes onto `data.image_base64` (with `data.mime`) before a
/// turn; providers call this to (a) get the cleaned value for the text part —
/// without the huge base64 blob — and (b) the `(mime, base64)` to emit as a real
/// image block. Returns `None` when there's no inlined image.
pub fn split_inlined_image(content: &Value) -> (Value, Option<(String, String)>) {
    let base64 = content
        .pointer("/data/image_base64")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(base64) = base64 else {
        return (content.clone(), None);
    };
    let mime = content
        .pointer("/data/mime")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    let mut cleaned = content.clone();
    if let Some(data) = cleaned.get_mut("data").and_then(Value::as_object_mut) {
        data.remove("image_base64");
    }
    (cleaned, Some((mime, base64)))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct GeneratedToolCall {
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Provider-assigned call id (OpenAI `tool_calls[].id` / Anthropic `tool_use`
    /// block id). `None` for calls salvaged from text — the harness synthesizes
    /// an id so the native tool_call/tool_result pairing stays valid.
    #[serde(default)]
    pub id: Option<String>,
}

/// One native tool call recorded on an assistant turn, paired with a
/// `ToolResult` message carrying the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Coerce tool-call arguments to a JSON object for the wire. Providers require
/// tool_call arguments to be an object (OpenAI: a stringified object; Anthropic:
/// an object); a salvaged or unparseable call can leave `arguments` as a bare
/// string or null, which strict providers (e.g. Cohere) reject with a 400. Pass a
/// string that itself parses to an object through; otherwise fall back to `{}`.
pub fn arguments_as_object(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else if let Value::String(s) = value {
        serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelOutput {
    #[serde(default)]
    pub calls: Vec<GeneratedToolCall>,
    #[serde(default)]
    pub content_text: Option<String>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// Provider finish/stop reason for the turn (e.g. `length`, `max_tokens`,
    /// `stop`, `tool_calls`). Used to detect a response cut off at the token cap.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ModelOutput {
    /// Whether the turn was cut off at the output-token limit (and so its text
    /// must not be treated as a finished reply).
    pub fn is_truncated(&self) -> bool {
        self.finish_reason
            .as_deref()
            .map(|r| {
                let r = r.to_ascii_lowercase();
                r == "length" || r == "max_tokens"
            })
            .unwrap_or(false)
    }
}

/// Token usage reported by the model for a single request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

#[async_trait]
pub trait AgentModel: Send + Sync {
    /// `force_tool` requires at least one tool call this turn (`tool_choice:
    /// "required"`) to break a text-only stall. When `sink` is `Some`, the model
    /// streams visible text deltas into it as they arrive (the full `ModelOutput`
    /// is still returned the same way); `None` keeps the buffered path.
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError>;
}

#[async_trait]
impl<T: ?Sized + AgentModel + Send> AgentModel for Box<T> {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        (**self).generate(messages, tools, force_tool, sink).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum HarnessMessage {
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: String,
        /// Native tool calls the assistant made this turn. Each is answered by a
        /// following `ToolResult` with the matching `tool_call_id`.
        #[serde(default)]
        tool_calls: Vec<ToolCallRecord>,
    },
    ToolResult {
        /// Id of the assistant `tool_call` this result answers. Empty on states
        /// written before native function calling (rendered as a text block).
        #[serde(default)]
        tool_call_id: String,
        tool_name: String,
        content: Value,
    },
    System {
        content: String,
    },
}
