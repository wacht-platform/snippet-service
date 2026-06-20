use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolError;

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
    /// "required"`) to break a text-only stall.
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> Result<ModelOutput, ToolError>;
}

#[async_trait]
impl<T: ?Sized + AgentModel + Send> AgentModel for Box<T> {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> Result<ModelOutput, ToolError> {
        (**self).generate(messages, tools, force_tool).await
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
