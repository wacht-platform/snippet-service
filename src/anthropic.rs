use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition,
};
use crate::tools::ToolError;

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
    pub cache_prompt: bool,
}

pub struct AnthropicModel {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl AgentModel for AnthropicModel {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> Result<ModelOutput, ToolError> {
        let url = "https://api.anthropic.com/v1/messages";
        let body = self.build_anthropic_request(messages, tools, force_tool);
        let bytes = self.send_with_retries(url, &body).await?;

        let response: AnthropicResponse = serde_json::from_slice(&bytes)?;
        let mut content_text = None;
        let mut calls = Vec::new();

        for block in response.content {
            match block {
                AnthropicResponseContent::Text { text } => {
                    content_text = Some(text);
                }
                AnthropicResponseContent::ToolUse { id, name, input } => {
                    calls.push(GeneratedToolCall {
                        tool_name: name,
                        arguments: input,
                        id: Some(id),
                    });
                }
            }
        }

        Ok(ModelOutput {
            calls,
            content_text,
            usage: response.usage.map(|u| crate::llm::TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.input_tokens.saturating_add(u.output_tokens),
                cache_read_tokens: u.cache_read_input_tokens,
                cache_creation_tokens: u.cache_creation_input_tokens,
            }),
            // Anthropic signals a token-cap cut-off as `max_tokens`; normalize so
            // `ModelOutput::is_truncated` catches it like the OpenAI `length`.
            finish_reason: response.stop_reason,
        })
    }
}

impl AnthropicModel {
    fn build_anthropic_request(
        &self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> AnthropicRequest {
        let (system_prompt, mut anthropic_messages) = prepare_messages(messages);
        let system = system_prompt.map(|prompt| vec![AnthropicSystemContent {
            content_type: "text",
            text: prompt,
            cache_control: if self.config.cache_prompt { Some(json!({"type": "ephemeral"})) } else { None },
        }]);

        let mut anthropic_tools: Vec<AnthropicTool> = tools.iter().map(anthropic_tool_from_definition).collect();
        if self.config.cache_prompt {
            if let Some(last_tool) = anthropic_tools.last_mut() {
                last_tool.cache_control = Some(json!({"type": "ephemeral"}));
            }
        }

        if self.config.cache_prompt {
            if let Some(last_msg) = anthropic_messages.last_mut() {
                if let Some(last_content) = last_msg.content.last_mut() {
                    match last_content {
                        AnthropicContent::Text { cache_control, .. }
                        | AnthropicContent::ToolResult { cache_control, .. } => {
                            *cache_control = Some(json!({"type": "ephemeral"}));
                        }
                        // A bare tool_use block as the final content is rare; skip
                        // the cache breakpoint rather than stamping it.
                        AnthropicContent::ToolUse { .. } => {}
                    }
                }
            }
        }

        let tool_choice = if force_tool && !tools.is_empty() {
            Some(json!({"type": "any"}))
        } else {
            None
        };

        AnthropicRequest {
            model: self.config.model.clone(),
            messages: anthropic_messages,
            max_tokens: 4096,
            system,
            tools: anthropic_tools,
            temperature: self.config.temperature,
            tool_choice,
        }
    }

    async fn send_with_retries(&self, url: &str, body: &AnthropicRequest) -> Result<Vec<u8>, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();

        for attempt in 1..=max_attempts {
            let response = self
                .client
                .post(url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", "2023-06-01")
                .header(CONTENT_TYPE, "application/json")
                .json(body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after_delay(response.headers().get(RETRY_AFTER));
                    let bytes = response.bytes().await.map_err(|error| {
                        ToolError::msg(format!("failed reading anthropic response body: {error}"))
                    })?;

                    if status.is_success() {
                        return Ok(bytes.to_vec());
                    }

                    let response_body = String::from_utf8_lossy(&bytes).to_string();
                    last_error = format!("HTTP {status}: {response_body}");
                    if !is_retryable_status(status) || attempt == max_attempts {
                        break;
                    }

                    sleep(retry_delay(
                        attempt,
                        retry_after,
                        self.config.initial_retry_ms,
                        self.config.max_retry_ms,
                    ))
                    .await;
                }
                Err(error) => {
                    last_error = error.to_string();
                    if !is_retryable_transport_error(&error) || attempt == max_attempts {
                        break;
                    }
                    sleep(retry_delay(
                        attempt,
                        None,
                        self.config.initial_retry_ms,
                        self.config.max_retry_ms,
                    ))
                    .await;
                }
            }
        }

        Err(ToolError::msg(format!(
            "anthropic model request failed after {} attempt(s): {}",
            max_attempts, last_error
        )))
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status.is_server_error()
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retry_after_delay(value: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let raw = value?.to_str().ok()?;
    raw.parse::<u64>().ok().map(Duration::from_secs)
}

fn retry_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    initial_ms: u64,
    max_ms: u64,
) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(Duration::from_millis(max_ms.max(1)));
    }
    let multiplier = 2u64.saturating_pow(attempt.saturating_sub(1).min(10));
    Duration::from_millis(
        initial_ms
            .max(1)
            .saturating_mul(multiplier)
            .min(max_ms.max(1)),
    )
}

#[derive(Debug, Serialize)]
struct AnthropicSystemContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<Value>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemContent>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContent>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<Value>,
}

fn anthropic_tool_from_definition(definition: &NativeToolDefinition) -> AnthropicTool {
    AnthropicTool {
        name: definition.name.clone(),
        description: definition.description.clone(),
        input_schema: definition.input_schema.clone(),
        cache_control: None,
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseContent>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContent {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn prepare_messages(harness_msgs: &[HarnessMessage]) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_prompt = None;
    let mut prepared = Vec::new();

    for (index, msg) in harness_msgs.iter().enumerate() {
        match msg {
            HarnessMessage::System { content } if index == 0 => {
                system_prompt = Some(content.clone());
            }
            HarnessMessage::System { content } => {
                let text = format!("[runtime_signal]\n{content}\n[/runtime_signal]");
                push_block(&mut prepared, "user", text_block(text));
            }
            HarnessMessage::User { content } => {
                push_block(&mut prepared, "user", text_block(content.clone()));
            }
            HarnessMessage::Assistant { content, tool_calls } => {
                // Assistant turn: optional text, then a tool_use block per call.
                if !content.is_empty() {
                    push_block(&mut prepared, "assistant", text_block(content.clone()));
                }
                for call in tool_calls {
                    push_block(
                        &mut prepared,
                        "assistant",
                        AnthropicContent::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.arguments.clone(),
                        },
                    );
                }
            }
            HarnessMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
            } => {
                let body =
                    serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string());
                if tool_call_id.is_empty() {
                    // Legacy state (pre native function calling) — render as text.
                    let text = format!(
                        "[tool_result]\ntool = \"{tool_name}\"\noutput = {body}\n[/tool_result]"
                    );
                    push_block(&mut prepared, "user", text_block(text));
                } else {
                    push_block(
                        &mut prepared,
                        "user",
                        AnthropicContent::ToolResult {
                            tool_use_id: tool_call_id.clone(),
                            content: body,
                            cache_control: None,
                        },
                    );
                }
            }
        }
    }

    (system_prompt, prepared)
}

fn text_block(text: String) -> AnthropicContent {
    AnthropicContent::Text {
        text,
        cache_control: None,
    }
}

/// Append a content block, merging into the previous message when it shares the
/// role so tool_use / tool_result blocks pair correctly across one assistant /
/// user turn.
fn push_block(prepared: &mut Vec<AnthropicMessage>, role: &str, block: AnthropicContent) {
    if let Some(last) = prepared.last_mut() {
        if last.role == role {
            last.content.push(block);
            return;
        }
    }
    prepared.push(AnthropicMessage {
        role: role.to_string(),
        content: vec![block],
    });
}
