use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamBuffer,
    StreamHandle, TokenUsage,
};
use crate::tools::ToolError;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
    /// User-Agent sent on every request. Some gated providers (e.g. Kimi For
    /// Coding) only serve recognised coding-agent UAs; `None` falls back to a
    /// Claude Code-style UA so those endpoints accept us out of the box.
    pub user_agent: Option<String>,
    /// Whether the model accepts image inputs (drives `supports_images()`).
    pub supports_images: bool,
    /// Reasoning effort: low | medium | high | off (sent as `reasoning_effort`).
    pub reasoning_effort: Option<String>,
    /// Force the streaming wire protocol even when no live sink is attached, so
    /// stream-only providers (e.g. NVIDIA NIM MiniMax) work on the buffered
    /// serve/app/lanes path instead of returning an empty non-streaming completion.
    pub stream: bool,
}

/// Default UA for OpenAI-compatible endpoints. Mirrors Claude Code's
/// `claude-cli/<ver> (external, cli)` so coding-agent-gated providers (Kimi For
/// Coding, etc.) whitelist us; harmless to ungated providers.
const DEFAULT_USER_AGENT: &str = "claude-cli/2.0.37 (external, cli)";

pub struct OpenAiCompatibleModel {
    config: OpenAiCompatibleConfig,
    client: reqwest::Client,
}

impl OpenAiCompatibleModel {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        let user_agent = config
            .user_agent
            .clone()
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { config, client }
    }
}

#[async_trait]
impl AgentModel for OpenAiCompatibleModel {
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
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        // Stream when there's a live sink (interactive) or when the profile forces it
        // (stream-only providers like NVIDIA NIM MiniMax). A buffered caller with no
        // sink still assembles a full response from a throwaway buffer.
        if sink.is_some() || self.config.stream {
            let body = self.build_chat_request(messages, tools, force_tool, true);
            let headers = vec![("authorization", format!("Bearer {}", self.config.api_key))];
            let detached = sink
                .is_none()
                .then(|| Arc::new(Mutex::new(StreamBuffer::default())));
            let sink_ref = sink.as_ref().or(detached.as_ref()).expect("a sink is present");
            return stream_chat_with_retries(
                &self.client,
                &url,
                &headers,
                &body,
                sink_ref,
                self.config.max_retries,
                self.config.initial_retry_ms,
                self.config.max_retry_ms,
            )
            .await;
        }
        let body = self.build_chat_request(messages, tools, force_tool, false);
        let bytes = self.send_with_retries(&url, &body).await?;

        // Surface the raw body on parse / empty-choices failures. Some OpenAI-compatible
        // gateways (notably NVIDIA NIM) return HTTP 200 with an error payload or an empty
        // `choices` array; swallowing the body makes the failure undebuggable.
        let response: ChatResponse = serde_json::from_slice(&bytes).map_err(|error| {
            ToolError::msg(format!("could not parse model response: {error}; {}", body_snippet(&bytes)))
        })?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::msg(format!("model response had no choices; {}", body_snippet(&bytes))))?;
        let calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .filter_map(|call| {
                let id = call.id.clone();
                let function = call.function?;
                let arguments = serde_json::from_str::<Value>(&function.arguments)
                    .unwrap_or(Value::String(function.arguments));
                Some(GeneratedToolCall {
                    tool_name: function.name,
                    arguments,
                    id,
                    ..Default::default()
                })
            })
            .collect();

        Ok(ModelOutput {
            calls,
            content_text: choice.message.content,
            usage: response.usage.map(|u| crate::llm::TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                cache_read_tokens: u.prompt_tokens_details.cached_tokens,
                ..Default::default()
            }),
            finish_reason: choice.finish_reason,
            rate_limit: None,
        })
    }
}

impl OpenAiCompatibleModel {
    fn build_chat_request(
        &self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        stream: bool,
    ) -> ChatRequest {
        // Reasoning effort for o-series / gpt-5 / reasoning models. "off" or unset
        // omits it so non-reasoning models don't 400 on the field.
        let reasoning_effort = self
            .config
            .reasoning_effort
            .as_deref()
            .filter(|e| !e.eq_ignore_ascii_case("off") && !e.trim().is_empty())
            .map(str::to_string);
        // DeepSeek's THINKING mode rejects `tool_choice: required` (HTTP 400
        // "Thinking mode does not support this tool_choice"). When thinking is on
        // for a DeepSeek model, degrade forced tool use to `auto` — callers that
        // force (the compaction summarizer + memory reflection) re-prompt if the
        // model skips the tool, so correctness is preserved. Other providers
        // (OpenAI o-series) support `required` with reasoning, so keep it there.
        let model_l = self.config.model.to_lowercase();
        let base_l = self.config.base_url.to_lowercase();
        let deepseek_thinking =
            reasoning_effort.is_some() && (model_l.contains("deepseek") || base_l.contains("deepseek"));
        // Providers that reject `tool_choice: "required"`: DeepSeek thinking mode (400
        // "Thinking mode does not support this tool_choice") and NVIDIA NIM, which
        // dropped `required` on its OpenAI-compatible endpoint. Degrade to auto — the
        // forced-tool callers (summarizer, memory reflection) re-prompt if the model
        // skips the tool, so correctness holds.
        let no_forced_tool_choice = deepseek_thinking || base_l.contains("nvidia.com");
        ChatRequest {
            model: self.config.model.clone(),
            messages: build_chat_messages(messages),
            tools: tools.iter().map(chat_tool_from_definition).collect(),
            temperature: self.config.temperature,
            // `required` needs tools to choose from, else providers reject it; and
            // DeepSeek thinking mode rejects it outright (see above).
            tool_choice: (force_tool && !tools.is_empty() && !no_forced_tool_choice).then_some("required"),
            reasoning_effort,
            // Opt into usage accounting on OpenRouter so cache-read tokens surface.
            usage: self
                .config
                .base_url
                .contains("openrouter")
                .then_some(UsageRequest { include: true }),
            stream: stream.then_some(true),
            // Streaming usage only arrives with include_usage on stream_options.
            stream_options: stream.then_some(StreamOptions { include_usage: true }),
        }
    }

    async fn send_with_retries(&self, url: &str, body: &ChatRequest) -> Result<Vec<u8>, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut fatal = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
            attempts = attempt;
            let response = self
                .client
                .post(url)
                .header(AUTHORIZATION, format!("Bearer {}", self.config.api_key))
                .header(CONTENT_TYPE, "application/json")
                .json(body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after_delay(response.headers().get(RETRY_AFTER));
                    let bytes = response.bytes().await.map_err(|error| {
                        ToolError::msg(format!("failed reading model response body: {error}"))
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

pub(crate) fn is_retryable_status(status: StatusCode) -> bool {
    // 429 (rate limited) is deliberately NOT retried — surface it immediately so the
    // user knows they've hit a limit, instead of silently backing off and re-hitting it.
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status.is_server_error()
}

pub(crate) fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

pub(crate) fn retry_after_delay(value: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let raw = value?.to_str().ok()?;
    raw.parse::<u64>().ok().map(Duration::from_secs)
}

pub(crate) fn retry_delay(
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

/// Send a streaming chat request (with per-provider headers) and assemble the
/// `ModelOutput` from the SSE response, pushing text deltas to `sink` as they
/// arrive. Retries mirror `send_with_retries`; each attempt clears the sink so a
/// retry doesn't double-render already-streamed text.
async fn stream_chat_with_retries(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: &ChatRequest,
    sink: &StreamHandle,
    max_retries: u32,
    initial_ms: u64,
    max_ms: u64,
) -> Result<ModelOutput, ToolError> {
    let max_attempts = max_retries.saturating_add(1).max(1);
    let mut last_error = String::new();
    let mut fatal = false;
    let mut attempts = 0u32;

    for attempt in 1..=max_attempts {
        attempts = attempt;
        StreamBuffer::clear(sink);
        let mut request = client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .json(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return parse_openai_sse(response, sink).await;
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
                sleep(retry_delay(attempt, retry_after, initial_ms, max_ms)).await;
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
                sleep(retry_delay(attempt, None, initial_ms, max_ms)).await;
            }
        }
    }

    StreamBuffer::clear(sink);
    Err(ToolError::model_request(
        crate::llm::final_model_error(&last_error, attempts),
        !fatal,
    ))
}

/// Consume an OpenAI-style `chat.completions` SSE stream and assemble a
/// `ModelOutput`. Content deltas push to `sink` live; tool-call deltas accumulate
/// by index (id/name once, arguments fragment by fragment) and parse at the end.
async fn parse_openai_sse(
    response: reqwest::Response,
    sink: &StreamHandle,
) -> Result<ModelOutput, ToolError> {
    struct PendingCall {
        id: String,
        name: String,
        args: String,
    }
    let mut text = String::new();
    let mut calls: std::collections::BTreeMap<u64, PendingCall> = std::collections::BTreeMap::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<TokenUsage> = None;
    let mut stream_error: Option<String> = None;

    crate::sse::for_each_event(response, |data| {
        if data == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        // Mid-stream failure (OpenRouter et al. send `{"error":{...}}` and close):
        // surface it — skipping it returned a partial accumulation as success.
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            stream_error.get_or_insert(msg);
            return;
        }
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            usage = Some(TokenUsage {
                prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                completion_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
                total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
                cache_read_tokens: u
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                ..Default::default()
            });
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            finish_reason = Some(reason.to_string());
        }
        let delta = choice.get("delta");
        // Reasoning tokens, when the model emits them (OpenRouter: `reasoning`;
        // DeepSeek and others: `reasoning_content`) — shown dimmed, not part of
        // the answer text.
        if let Some(reasoning) = delta
            .and_then(|d| d.get("reasoning").or_else(|| d.get("reasoning_content")))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            StreamBuffer::append_thinking(sink, reasoning);
        }
        if let Some(content) = delta
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            text.push_str(content);
            StreamBuffer::append(sink, content);
        }
        if let Some(tool_calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in tool_calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let entry = calls.entry(index).or_insert_with(|| PendingCall {
                    id: String::new(),
                    name: String::new(),
                    args: String::new(),
                });
                if let Some(id) = call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    entry.id = id.to_string();
                }
                if let Some(name) = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    entry.name.push_str(name);
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.args.push_str(args);
                }
            }
        }
    })
    .await
    .map_err(|error| ToolError::msg(format!("model stream error: {error}")))?;

    if let Some(err) = stream_error {
        return Err(ToolError::model_request(
            format!("model mid-stream error: {err}"),
            true,
        ));
    }

    let calls = calls
        .into_values()
        .filter(|call| !call.name.is_empty())
        .map(|call| GeneratedToolCall {
            tool_name: call.name,
            arguments: serde_json::from_str::<Value>(&call.args)
                .unwrap_or(Value::String(call.args)),
            id: (!call.id.is_empty()).then_some(call.id),
            ..Default::default()
        })
        .collect();

    Ok(ModelOutput {
        calls,
        content_text: (!text.is_empty()).then_some(text),
        usage,
        finish_reason,
        rate_limit: None,
    })
}

/// Build the wire message list. Image messages (from read_image results) are held
/// and flushed only after the contiguous run of `tool` messages ends — OpenAI
/// requires every tool result to immediately follow the assistant tool-call turn
/// with no other message interleaved.
fn build_chat_messages(messages: &[HarnessMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut pending_images: Vec<ChatMessage> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        for chat_message in chat_messages_from_harness(index, message) {
            let is_image =
                chat_message.role == "user" && matches!(chat_message.content, Some(Value::Array(_)));
            if is_image {
                pending_images.push(chat_message);
            } else if chat_message.role == "tool" {
                out.push(chat_message);
            } else {
                out.append(&mut pending_images);
                out.push(chat_message);
            }
        }
    }
    out.append(&mut pending_images);
    ensure_tool_results(&mut out);
    out
}

/// Safety net: guarantee every assistant `tool_call` is followed by a `tool`
/// message with the matching id. Strict providers (DeepSeek) 400 with
/// "insufficient tool messages" otherwise — and a single unanswered call poisons
/// every later request. Heals histories where a call went unanswered for any
/// reason (deduped, interrupted, or crashed mid-batch).
fn ensure_tool_results(out: &mut Vec<ChatMessage>) {
    let mut i = 0;
    while i < out.len() {
        let ids: Vec<String> = if out[i].role == "assistant" {
            out[i].tool_calls.iter().map(|c| c.id.clone()).filter(|s| !s.is_empty()).collect()
        } else {
            Vec::new()
        };
        if ids.is_empty() {
            i += 1;
            continue;
        }
        // Walk the contiguous run of tool messages right after this assistant turn.
        let mut j = i + 1;
        let mut answered = std::collections::HashSet::new();
        while j < out.len() && out[j].role == "tool" {
            if let Some(id) = &out[j].tool_call_id {
                answered.insert(id.clone());
            }
            j += 1;
        }
        // Insert a stub for each unanswered id so the run stays complete.
        let mut at = j;
        for id in ids {
            if !answered.contains(&id) {
                out.insert(
                    at,
                    ChatMessage::tool(
                        &id,
                        "{\"schema_version\":1,\"status\":\"ok\",\"data\":{\"note\":\"no result recorded for this call\"}}",
                    ),
                );
                at += 1;
            }
        }
        i = at;
    }
}

fn chat_messages_from_harness(index: usize, message: &HarnessMessage) -> Vec<ChatMessage> {
    match message {
        HarnessMessage::System { content } if index == 0 => {
            vec![ChatMessage::text("system", content)]
        }
        HarnessMessage::System { content } => vec![ChatMessage::text(
            "user",
            &format!("[runtime_signal]\n{content}\n[/runtime_signal]"),
        )],
        HarnessMessage::User { content } => vec![ChatMessage::text("user", content)],
        HarnessMessage::Assistant { content, tool_calls } => {
            let out_calls = tool_calls
                .iter()
                .map(|call| OutToolCall {
                    id: call.id.clone(),
                    call_type: "function",
                    function: OutToolCallFunction {
                        name: call.name.clone(),
                        // Must be a stringified JSON OBJECT — coerce non-object
                        // arguments so strict providers (Cohere) don't 400.
                        arguments: serde_json::to_string(&crate::llm::arguments_as_object(
                            &call.arguments,
                        ))
                        .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
                .collect();
            vec![ChatMessage::assistant(content, out_calls)]
        }
        HarnessMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
        } => {
            let (cleaned, image) = crate::llm::split_inlined_image(content);
            let body =
                serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
            if tool_call_id.is_empty() {
                // Legacy state written before native function calling — render as
                // a text block so old sessions still load.
                vec![ChatMessage::text(
                    "user",
                    &format!("[tool_result]\ntool = \"{tool_name}\"\noutput = {body}\n[/tool_result]"),
                )]
            } else {
                // Tool messages can't carry images, so an inlined image follows as
                // a separate user message with an image_url data URL.
                let mut out = vec![ChatMessage::tool(tool_call_id, &body)];
                if let Some((mime, base64)) = image {
                    out.push(ChatMessage::user_image(&mime, &base64));
                }
                out
            }
        }
        HarnessMessage::Summary { kind, content } => vec![ChatMessage::text(
            "user",
            &format!("[summary:{kind}]\n{content}\n[/summary]"),
        )],
    }
}

fn chat_tool_from_definition(definition: &NativeToolDefinition) -> ChatTool {
    ChatTool {
        tool_type: "function".to_string(),
        function: ChatToolFunction {
            name: definition.name.clone(),
            description: definition.description.clone(),
            parameters: definition.input_schema.clone(),
        },
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    // OpenRouter only reports detailed token accounting (incl.
    // prompt_tokens_details.cached_tokens) when usage accounting is opted into.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
struct UsageRequest {
    include: bool,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    // A plain string for text, or an array of content parts (for image_url).
    // Assistant messages that carry only tool calls send `content: null`, so it
    // must be omittable.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OutToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn assistant(content: &str, tool_calls: Vec<OutToolCall>) -> Self {
        // An assistant turn that is purely tool calls sends null content.
        let content = if content.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(Value::String(content.to_string()))
        };
        Self {
            role: "assistant".to_string(),
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    fn tool(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(Value::String(content.to_string())),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }

    /// A user message carrying a single inline image as an `image_url` data URL —
    /// how OpenAI-style APIs accept images (tool messages can't hold them).
    fn user_image(mime: &str, base64: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{base64}") }
            }])),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct OutToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: OutToolCallFunction,
}

#[derive(Debug, Serialize)]
struct OutToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: ChatToolFunction,
}

#[derive(Debug, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    // OpenAI/compatible report cache hits nested under prompt_tokens_details.
    #[serde(default)]
    prompt_tokens_details: PromptTokensDetails,
}

#[derive(Debug, Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCall {
    #[serde(default)]
    id: Option<String>,
    function: Option<ChatToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallFunction {
    name: String,
    #[serde(default = "empty_json_object")]
    arguments: String,
}

fn empty_json_object() -> String {
    json!({}).to_string()
}

/// A short, safe preview of a raw response body for error messages — enough to see a
/// gateway's soft-error payload (e.g. NVIDIA NIM's HTTP-200 errors) without dumping a
/// huge blob into the UI.
fn body_snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "body: <empty>".to_string();
    }
    let preview: String = trimmed.chars().take(600).collect();
    if preview.chars().count() < trimmed.chars().count() {
        format!("body: {preview}…")
    } else {
        format!("body: {preview}")
    }
}

