use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamBuffer,
    StreamHandle, TokenUsage,
};
use crate::tools::ToolError;

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    /// API root (no trailing `/v1/messages`). `https://api.anthropic.com` for the
    /// real API; a custom host for an Anthropic-Messages-compatible gateway.
    pub base_url: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
    pub cache_prompt: bool,
    pub supports_images: bool,
    pub reasoning_effort: Option<String>,
}

pub struct AnthropicModel {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicModel {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            client: crate::llm::model_http_client(None),
        }
    }
}

#[async_trait]
impl AgentModel for AnthropicModel {
    fn is_configured(&self) -> bool {
        !self.config.api_key.trim().is_empty()
    }

    fn swap_reasoning_effort(&mut self, effort: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.config.reasoning_effort, effort)
    }

    fn supports_images(&self) -> bool {
        self.config.supports_images
    }

    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        let url = &anthropic_messages_url(&self.config.base_url);
        let url = url.as_str();
        if let Some(sink) = sink {
            return self.generate_streaming(url, messages, tools, force_tool, &sink).await;
        }
        let body = self.build_anthropic_request(messages, tools, force_tool, false);
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
                        ..Default::default()
                    });
                }
                AnthropicResponseContent::Thinking {}
                | AnthropicResponseContent::RedactedThinking {} => {}
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
            rate_limit: None,
        })
    }
}

impl AnthropicModel {
    fn build_anthropic_request(
        &self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        stream: bool,
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
            // Stamp the breakpoint on the last DURABLE content block, not the
            // final one: the final block is the harness's per-turn live-context
            // text, regenerated every turn — a breakpoint there caches a prefix
            // no later request can ever match, so the message history would get
            // ZERO cache hits (only system+tools would hit). The block before it
            // is stable history; entries cached there are exact prefixes of the
            // next turn's request and hit via Anthropic's breakpoint lookback.
            let stamp = |block: &mut AnthropicContent| match block {
                AnthropicContent::Text { cache_control, .. }
                | AnthropicContent::ToolResult { cache_control, .. } => {
                    *cache_control = Some(json!({"type": "ephemeral"}));
                    true
                }
                // A bare tool_use block is never the last durable block (its
                // results follow it); skip rather than stamping it.
                AnthropicContent::ToolUse { .. } => false,
            };
            let mut placed = false;
            // The live context may share the final message with preceding blocks
            // (user-role merging) — prefer that message's second-to-last block.
            if let Some(last_msg) = anthropic_messages.last_mut() {
                let n = last_msg.content.len();
                if n >= 2 {
                    placed = stamp(&mut last_msg.content[n - 2]);
                }
            }
            if !placed {
                let m = anthropic_messages.len();
                if m >= 2 {
                    if let Some(block) = anthropic_messages[m - 2].content.last_mut() {
                        placed = stamp(block);
                    }
                }
            }
            if !placed {
                // Single-message requests (summarizer / reflection workers): the
                // only block is the target — their long shared prefix still
                // benefits across those workers' turns.
                if let Some(block) =
                    anthropic_messages.last_mut().and_then(|m| m.content.last_mut())
                {
                    stamp(block);
                }
            }
        }

        let tool_choice = if force_tool && !tools.is_empty() {
            Some(json!({"type": "any"}))
        } else {
            None
        };

        // Extended thinking: when reasoning is requested, enable it with a budget
        // and bump max_tokens above the budget. Anthropic requires temperature to be
        // unset (defaults to 1) when thinking is on, and forbids forced tool_choice
        // with thinking — so drop both in that case.
        //
        // Thinking is only enabled on TOOL-LESS requests: with tools, Anthropic
        // requires the assistant's `thinking` block (with its signature) to be
        // replayed ahead of `tool_use` on the follow-up request, and this adapter
        // doesn't capture/persist thinking blocks — every post-tool-call request
        // would 400 (fatal) and agentic use breaks entirely.
        let thinking_budget = if tools.is_empty() {
            anthropic_thinking_budget(self.config.reasoning_effort.as_deref())
        } else {
            None
        };
        let (thinking, max_tokens, temperature, tool_choice) = match thinking_budget {
            Some(budget) => (
                Some(json!({ "type": "enabled", "budget_tokens": budget })),
                budget + 4096,
                None,
                None,
            ),
            None => (None, 4096, self.config.temperature, tool_choice),
        };

        AnthropicRequest {
            model: self.config.model.clone(),
            messages: anthropic_messages,
            max_tokens,
            system,
            tools: anthropic_tools,
            temperature,
            tool_choice,
            thinking,
            stream: stream.then_some(true),
        }
    }

    /// Streaming counterpart of `generate`: opens an SSE response and assembles
    /// the same `ModelOutput`, pushing visible text deltas into `sink` as they
    /// arrive. Retries mirror `send_with_retries`; each attempt clears the sink so
    /// a retry doesn't double-render already-streamed text.
    async fn generate_streaming(
        &self,
        url: &str,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: &StreamHandle,
    ) -> Result<ModelOutput, ToolError> {
        let body = self.build_anthropic_request(messages, tools, force_tool, true);
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut fatal = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
            attempts = attempt;
            StreamBuffer::clear(sink);
            let response = self
                .client
                .post(url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", "2023-06-01")
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return parse_anthropic_sse(response, sink).await;
                    }
                    let retry_after = retry_after_delay(response.headers().get(RETRY_AFTER));
                    let response_body = response.text().await.unwrap_or_default();
                    last_error = crate::llm::humanize_http_error(status, &response_body);
                    if !is_retryable_status(status) {
                        fatal = true;
                        break;
                    }
                    if attempt == max_attempts {
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
                    last_error = crate::llm::humanize_transport_error(&error);
                    if !is_retryable_transport_error(&error) {
                        fatal = true;
                        break;
                    }
                    if attempt == max_attempts {
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

        StreamBuffer::clear(sink);
        Err(ToolError::model_request(
            crate::llm::final_model_error(&last_error, attempts),
            !fatal,
        ))
    }

    async fn send_with_retries(&self, url: &str, body: &AnthropicRequest) -> Result<Vec<u8>, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut fatal = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
            attempts = attempt;
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
                    last_error = crate::llm::humanize_http_error(status, &response_body);
                    if !is_retryable_status(status) {
                        fatal = true;
                        break;
                    }
                    if attempt == max_attempts {
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
                    last_error = crate::llm::humanize_transport_error(&error);
                    if !is_retryable_transport_error(&error) {
                        fatal = true;
                        break;
                    }
                    if attempt == max_attempts {
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

        Err(ToolError::model_request(
            crate::llm::final_model_error(&last_error, attempts),
            !fatal,
        ))
    }
}

/// Consume Anthropic's `message_stream` SSE and assemble a `ModelOutput`. Text
/// deltas are pushed to `sink` live; tool_use blocks accumulate their streamed
/// `input_json_delta` fragments and are parsed when the response ends.
async fn parse_anthropic_sse(
    response: reqwest::Response,
    sink: &StreamHandle,
) -> Result<ModelOutput, ToolError> {
    // Tool-use blocks indexed by their content-block position; text is gathered
    // separately. `partial` holds the streamed argument JSON until block stop.
    struct PendingTool {
        id: String,
        name: String,
        partial: String,
    }
    let mut text = String::new();
    let mut tools: std::collections::BTreeMap<u64, PendingTool> = std::collections::BTreeMap::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_creation = 0u64;
    let mut stream_error: Option<String> = None;

    crate::sse::for_each_event(response, |data| {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.pointer("/message/usage") {
                    input_tokens = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
            }
            Some("content_block_start") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = event.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    tools.insert(
                        index,
                        PendingTool {
                            id: block
                                .and_then(|b| b.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: block
                                .and_then(|b| b.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            partial: String::new(),
                        },
                    );
                }
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = event.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(chunk) = delta.and_then(|d| d.get("text")).and_then(Value::as_str)
                        {
                            text.push_str(chunk);
                            StreamBuffer::append(sink, chunk);
                        }
                    }
                    // Extended-thinking tokens (present only when thinking is
                    // enabled) — shown dimmed, separate from the answer.
                    Some("thinking_delta") => {
                        if let Some(chunk) =
                            delta.and_then(|d| d.get("thinking")).and_then(Value::as_str)
                        {
                            StreamBuffer::append_thinking(sink, chunk);
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(tool), Some(chunk)) = (
                            tools.get_mut(&index),
                            delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(Value::as_str),
                        ) {
                            tool.partial.push_str(chunk);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
                    stop_reason = Some(reason.to_string());
                }
                if let Some(out) = event.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    output_tokens = out;
                }
                // Some Anthropic-COMPATIBLE gateways (e.g. OpenCode zen) put the
                // FINAL, authoritative input/cache counts in message_delta, and
                // report only a preliminary input_tokens in message_start. Native
                // Anthropic omits these here, so this is a no-op there — but when
                // present they're the accurate totals, so prefer them.
                if let Some(inp) = event.pointer("/usage/input_tokens").and_then(Value::as_u64) {
                    input_tokens = inp;
                }
                if let Some(cr) = event.pointer("/usage/cache_read_input_tokens").and_then(Value::as_u64) {
                    cache_read = cr;
                }
                if let Some(cc) = event.pointer("/usage/cache_creation_input_tokens").and_then(Value::as_u64) {
                    cache_creation = cc;
                }
            }
            // Mid-stream failure (e.g. overloaded_error): the stream ends here.
            // Surface it — silently returning the partial accumulation made a
            // truncated fragment look like a finished reply.
            Some("error") => {
                let msg = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown mid-stream error");
                let kind = event
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                stream_error = Some(format!("{kind}: {msg}"));
            }
            _ => {}
        }
    })
    .await
    .map_err(|error| ToolError::msg(format!("anthropic stream error: {error}")))?;

    if let Some(err) = stream_error {
        return Err(ToolError::model_request(
            format!("anthropic mid-stream error: {err}"),
            true,
        ));
    }

    let calls = tools
        .into_values()
        .map(|tool| GeneratedToolCall {
            tool_name: tool.name,
            arguments: serde_json::from_str(&tool.partial).unwrap_or_else(|_| json!({})),
            id: Some(tool.id),
            ..Default::default()
        })
        .collect();

    Ok(ModelOutput {
        calls,
        content_text: (!text.is_empty()).then_some(text),
        usage: Some(TokenUsage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        }),
        finish_reason: stop_reason,
        rate_limit: None,
    })
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
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

/// Map a unified reasoning-effort level to an Anthropic extended-thinking budget
/// (tokens). `off`/unset/unknown → no extended thinking.
/// Build the Messages endpoint from a configured base URL, tolerating whatever
/// shape the user pasted (compatible gateways present the base URL inconsistently):
///   ""                              → https://api.anthropic.com/v1/messages
///   https://host                    → https://host/v1/messages
///   https://host/v1  (incl. /v1/)   → https://host/v1/messages   (no double /v1)
///   https://host/v1/messages        → used as-is
pub(crate) fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return "https://api.anthropic.com/v1/messages".to_string();
    }
    if base.ends_with("/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn anthropic_thinking_budget(effort: Option<&str>) -> Option<u32> {
    match effort?.to_ascii_lowercase().as_str() {
        "low" => Some(2048),
        "medium" => Some(8192),
        "high" => Some(16384),
        // Tiers above `high` for models that reason harder when asked. Kept well
        // under Claude's max thinking budget; `max_tokens` is bumped past it below.
        "xhigh" => Some(32768),
        "max" => Some(63999),
        _ => None,
    }
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
        // String for text-only results, or an array of blocks (text + image) when
        // a read_image result carries an inlined image.
        content: Value,
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
    // Extended-thinking blocks (tool-less requests can enable thinking); the
    // buffered path has nowhere to show them, but parsing must not fail on them.
    // Fields intentionally ignored.
    Thinking {},
    RedactedThinking {},
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
                            // tool_use input must be a JSON object — coerce so a
                            // salvaged/non-object args value isn't rejected.
                            input: crate::llm::arguments_as_object(&call.arguments),
                        },
                    );
                }
            }
            HarnessMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
            } => {
                // Pull any inlined image out so it becomes a real image block, not
                // a giant base64 string in the text.
                let (cleaned, image) = crate::llm::split_inlined_image(content);
                let body =
                    serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
                if tool_call_id.is_empty() {
                    // Legacy state (pre native function calling) — render as text.
                    let text = format!(
                        "[tool_result]\ntool = \"{tool_name}\"\noutput = {body}\n[/tool_result]"
                    );
                    push_block(&mut prepared, "user", text_block(text));
                } else {
                    let content_value = match image {
                        Some((mime, base64)) => json!([
                            {"type": "text", "text": body},
                            {"type": "image", "source": {
                                "type": "base64", "media_type": mime, "data": base64
                            }},
                        ]),
                        None => Value::String(body),
                    };
                    push_block(
                        &mut prepared,
                        "user",
                        AnthropicContent::ToolResult {
                            tool_use_id: tool_call_id.clone(),
                            content: content_value,
                            cache_control: None,
                        },
                    );
                }
            }
            HarnessMessage::Summary { kind, content } => {
                let text = format!("[summary:{kind}]\n{content}\n[/summary]");
                push_block(&mut prepared, "user", text_block(text));
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

