//! ChatGPT-subscription model adapter — talks to the Codex Responses backend at
//! `chatgpt.com/backend-api/codex/responses` using OAuth tokens from
//! [`crate::chatgpt_auth`]. The Responses API has its own request/response shape
//! (item array `input`, flat function tools, SSE event stream), distinct from the
//! chat-completions adapter in `openai.rs`.

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::chatgpt_auth::{self, ChatGptTokens};
use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamBuffer,
    StreamHandle, TokenUsage,
};
use crate::openai::{is_retryable_status, is_retryable_transport_error, retry_after_delay, retry_delay};
use crate::tools::ToolError;

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Parse the Codex rate-limit usage headers (`x-codex-{primary,secondary}-*`) that
/// ride on every /codex/responses response.
fn parse_codex_rate_limits(
    headers: &reqwest::header::HeaderMap,
) -> Option<crate::llm::RateLimitSnapshot> {
    use crate::llm::{RateLimitSnapshot, RateLimitWindow};
    let f64_at = |name: String| {
        headers.get(&name).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<f64>().ok())
    };
    let i64_at = |name: String| {
        headers.get(&name).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<i64>().ok())
    };
    let window = |p: &str| {
        let used = f64_at(format!("x-codex-{p}-used-percent")).unwrap_or(0.0);
        let mins = i64_at(format!("x-codex-{p}-window-minutes")).unwrap_or(0);
        let reset = i64_at(format!("x-codex-{p}-reset-at"));
        if used == 0.0 && mins == 0 && reset.is_none() {
            return None;
        }
        Some(RateLimitWindow {
            used_percent: used,
            window_minutes: mins,
            resets_at: reset.unwrap_or(0),
        })
    };
    let (primary, secondary) = (window("primary"), window("secondary"));
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(RateLimitSnapshot { primary, secondary })
}

#[derive(Debug, Clone)]
pub struct ChatGptConfig {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub supports_images: bool,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
}

pub struct ChatGptModel {
    config: ChatGptConfig,
    client: reqwest::Client,
    tokens: Option<ChatGptTokens>,
    /// Stable per-process id used for the session/cache-key headers.
    session_id: String,
}

impl ChatGptModel {
    pub fn new(config: ChatGptConfig) -> Self {
        Self {
            config,
            client: crate::llm::model_http_client(None),
            tokens: chatgpt_auth::load_blocking(),
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Refresh the access token if it's missing or near expiry, persisting it.
    async fn ensure_fresh(&mut self) -> Result<&ChatGptTokens, ToolError> {
        let stale = self.tokens.as_ref().map(ChatGptTokens::is_stale).unwrap_or(false);
        if stale {
            if let Some(prior) = self.tokens.clone() {
                match chatgpt_auth::refresh(&prior).await {
                    Ok(fresh) => self.tokens = Some(fresh),
                    // A failed refresh isn't necessarily fatal — the existing token
                    // may still have a few seconds; let the request try and 401 if not.
                    Err(_) => {}
                }
            }
        }
        self.tokens.as_ref().ok_or_else(|| {
            ToolError::model_request(
                "not signed in to ChatGPT — open the model picker and choose “Sign in with ChatGPT”."
                    .to_string(),
                false,
            )
        })
    }
}

#[async_trait]
impl AgentModel for ChatGptModel {
    fn is_configured(&self) -> bool {
        self.tokens.is_some()
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
        // Confirm we have a (fresh) token before building the request.
        self.ensure_fresh().await?;

        let body = build_responses_request(&self.config, messages, tools, force_tool);
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut fatal = false;
        let mut refreshed = false;
        let mut attempts = 0u32;

        let mut attempt = 0u32;
        while attempt < max_attempts {
            attempt += 1;
            attempts = attempt;
            if let Some(sink) = sink.as_ref() {
                StreamBuffer::clear(sink);
            }
            let tokens = self
                .tokens
                .clone()
                .ok_or_else(|| ToolError::model_request("ChatGPT sign-in missing".to_string(), false))?;

            let request = self
                .client
                .post(RESPONSES_URL)
                .header("authorization", format!("Bearer {}", tokens.access_token))
                .header("chatgpt-account-id", &tokens.account_id)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "codex_cli_rs")
                .header("session_id", &self.session_id)
                .header("conversation_id", &self.session_id)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(CONTENT_TYPE, "application/json")
                .json(&body);

            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        // Rate-limit usage is on the response headers (read before the
                        // SSE body consumes the response).
                        let rate_limit = parse_codex_rate_limits(response.headers());
                        match parse_responses_sse(response, sink.as_ref()).await {
                            Ok(mut output) => {
                                output.rate_limit = rate_limit;
                                return Ok(output);
                            }
                            // A mid-stream break lands here AFTER a 200. Retry it like
                            // a failed send (the sink is cleared at the top of each
                            // attempt, so re-streaming is clean) instead of aborting.
                            Err(e) => {
                                if !e.retryable() {
                                    return Err(e);
                                }
                                last_error = e.to_string();
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
                                continue;
                            }
                        }
                    }
                    // Expired/invalid token → refresh once, then retry without
                    // consuming a backoff attempt.
                    if status == StatusCode::UNAUTHORIZED && !refreshed {
                        refreshed = true;
                        if let Some(prior) = self.tokens.clone() {
                            match chatgpt_auth::refresh(&prior).await {
                                Ok(fresh) => {
                                    self.tokens = Some(fresh);
                                    // Genuinely free retry: on the last attempt a
                                    // plain `continue` exhausted the loop and the
                                    // fresh token was never used.
                                    attempt -= 1;
                                    continue;
                                }
                                Err(e) => {
                                    return Err(ToolError::model_request(
                                        format!("ChatGPT token refresh failed: {e} — sign in again."),
                                        false,
                                    ));
                                }
                            }
                        }
                    }
                    let retry_after = retry_after_delay(response.headers().get(RETRY_AFTER));
                    let text = response.text().await.unwrap_or_default();
                    last_error = crate::llm::humanize_http_error(status, &text);
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

        if let Some(sink) = sink.as_ref() {
            StreamBuffer::clear(sink);
        }
        Err(ToolError::model_request(
            crate::llm::final_model_error(&last_error, attempts),
            !fatal,
        ))
    }
}

/// Normalize a reasoning-effort string for the Codex backend (it rejects
/// "minimal"; "off"/empty disables reasoning entirely).
fn normalize_effort(effort: Option<&str>) -> Option<String> {
    // Values pass through to the Responses API verbatim (low/medium/high, plus
    // `xhigh` on gpt-5.1-codex-max and later). `off`/empty omit the reasoning
    // block; `minimal` isn't accepted by Codex, so it degrades to `low`.
    let e = effort?.trim();
    if e.is_empty() || e.eq_ignore_ascii_case("off") {
        return None;
    }
    Some(if e.eq_ignore_ascii_case("minimal") {
        "low".to_string()
    } else {
        e.to_lowercase()
    })
}

fn build_responses_request(
    config: &ChatGptConfig,
    messages: &[HarnessMessage],
    tools: &[NativeToolDefinition],
    force_tool: bool,
) -> Value {
    // The first System message is the base instructions; everything else maps to
    // the `input` item array.
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    // Drop function_call_outputs whose function_call was compacted away (the API 400s).
    let mut seen_calls: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (index, message) in messages.iter().enumerate() {
        match message {
            HarnessMessage::System { content } if index == 0 => {
                instructions = content.clone();
            }
            HarnessMessage::System { content } => {
                input.push(message_item(
                    "user",
                    &format!("[runtime_signal]\n{content}\n[/runtime_signal]"),
                ));
            }
            HarnessMessage::User { content } => {
                input.push(message_item("user", content));
            }
            HarnessMessage::Assistant { content, tool_calls } => {
                if !content.is_empty() {
                    input.push(message_item("assistant", content));
                }
                for call in tool_calls {
                    let args = serde_json::to_string(&crate::llm::arguments_as_object(&call.arguments))
                        .unwrap_or_else(|_| "{}".to_string());
                    if !call.id.is_empty() {
                        seen_calls.insert(call.id.clone());
                    }
                    input.push(json!({
                        "type": "function_call",
                        "name": call.name,
                        "arguments": args,
                        "call_id": call.id,
                    }));
                }
            }
            HarnessMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
            } => {
                let (cleaned, image) = crate::llm::split_inlined_image(content);
                let output = serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
                if tool_call_id.is_empty() {
                    // Legacy state with no native call id — render as user text.
                    input.push(message_item(
                        "user",
                        &format!("[tool_result]\ntool = \"{tool_name}\"\noutput = {output}\n[/tool_result]"),
                    ));
                } else if !seen_calls.contains(tool_call_id) {
                    // Orphaned result (call was compacted away) — send as plain text.
                    input.push(message_item(
                        "user",
                        &format!("[tool_result {tool_name}]\n{output}\n[/tool_result]"),
                    ));
                } else {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": output,
                    }));
                    if let Some((mime, base64)) = image {
                        if config.supports_images {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{
                                    "type": "input_image",
                                    "image_url": format!("data:{mime};base64,{base64}"),
                                }],
                            }));
                        }
                    }
                }
            }
            HarnessMessage::Summary { kind, content } => {
                input.push(message_item(
                    "user",
                    &format!("[summary:{kind}]\n{content}\n[/summary]"),
                ));
            }
        }
    }

    // Every function_call must have a matching function_call_output, or the API
    // 400s. Synthesize a placeholder for any call whose result was never recorded
    // (failed/cut tool calls).
    let output_ids: std::collections::HashSet<String> = input
        .iter()
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|v| v.get("call_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut reconciled: Vec<Value> = Vec::with_capacity(input.len());
    for item in input {
        let missing = item.get("type").and_then(Value::as_str) == Some("function_call")
            && item
                .get("call_id")
                .and_then(Value::as_str)
                .map(|id| !id.is_empty() && !output_ids.contains(id))
                .unwrap_or(false);
        let call_id = item.get("call_id").and_then(Value::as_str).map(str::to_string);
        reconciled.push(item);
        if missing {
            reconciled.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "[no result recorded]",
            }));
        }
    }
    let input = reconciled;

    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "strict": false,
                "parameters": t.input_schema,
            })
        })
        .collect();

    let mut body = json!({
        "model": config.model,
        "input": input,
        "tool_choice": if force_tool && !tools_json.is_empty() { "required" } else { "auto" },
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
    });
    let obj = body.as_object_mut().expect("object");
    if !instructions.is_empty() {
        obj.insert("instructions".to_string(), json!(instructions));
    }
    if !tools_json.is_empty() {
        obj.insert("tools".to_string(), json!(tools_json));
    }
    if let Some(effort) = normalize_effort(config.reasoning_effort.as_deref()) {
        obj.insert("reasoning".to_string(), json!({ "effort": effort, "summary": "auto" }));
        obj.insert("include".to_string(), json!(["reasoning.encrypted_content"]));
    }
    body
}

fn message_item(role: &str, text: &str) -> Value {
    // The Responses API requires assistant content to be `output_text`; user /
    // developer content must be `input_text`. Sending input_text on an assistant
    // message 400s ("'input_text' invalid, expected 'output_text'").
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// Parse the Codex Responses SSE stream into a `ModelOutput`. Assistant text
/// deltas stream to `sink`; tool calls arrive whole in `response.output_item.done`
/// items; usage comes from `response.completed`.
async fn parse_responses_sse(
    response: reqwest::Response,
    sink: Option<&StreamHandle>,
) -> Result<ModelOutput, ToolError> {
    let mut text = String::new();
    let mut calls: Vec<GeneratedToolCall> = Vec::new();
    let mut usage: Option<TokenUsage> = None;
    let mut failure: Option<String> = None;
    let mut incomplete = false;

    crate::sse::for_each_event(response, |data| {
        if data == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = chunk.get("delta").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                    text.push_str(delta);
                    if let Some(sink) = sink {
                        StreamBuffer::append(sink, delta);
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let (Some(sink), Some(delta)) =
                    (sink, chunk.get("delta").and_then(Value::as_str).filter(|d| !d.is_empty()))
                {
                    StreamBuffer::append_thinking(sink, delta);
                }
            }
            "response.output_item.done" => {
                let item = chunk.get("item");
                if item.and_then(|i| i.get("type")).and_then(Value::as_str) == Some("function_call") {
                    let item = item.expect("checked");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    if !name.is_empty() {
                        let args_str = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                        let arguments = serde_json::from_str::<Value>(args_str)
                            .unwrap_or_else(|_| Value::String(args_str.to_string()));
                        let id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        calls.push(GeneratedToolCall {
                            tool_name: name.to_string(),
                            arguments,
                            id,
                            ..Default::default()
                        });
                    }
                }
            }
            "response.completed" => {
                if let Some(u) = chunk.pointer("/response/usage").filter(|u| u.is_object()) {
                    usage = Some(TokenUsage {
                        prompt_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        completion_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                        total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
                        cache_read_tokens: u
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        ..Default::default()
                    });
                }
            }
            "response.failed" | "response.incomplete" => {
                if chunk.get("type").and_then(Value::as_str) == Some("response.incomplete") {
                    incomplete = true;
                }
                failure = chunk
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("response failed".to_string()));
            }
            _ => {}
        }
    })
    .await
    // A break while consuming the SSE body (dropped connection, body-decode
    // error) is transient — mark it retryable so the request loop re-runs it
    // instead of aborting (this silently killed memory reflection mid-compaction).
    .map_err(|e| ToolError::model_request(format!("chatgpt stream error: {e}"), true))?;

    if let Some(err) = failure {
        if text.is_empty() && calls.is_empty() {
            let fatal = err.contains("context_length")
                || err.contains("insufficient_quota")
                || err.contains("usage_not_included");
            return Err(ToolError::model_request(format!("chatgpt: {err}"), !fatal));
        }
    }

    Ok(ModelOutput {
        calls,
        content_text: (!text.is_empty()).then_some(text),
        usage,
        // An incomplete response with partial output is a token-cap truncation —
        // report it as such so the harness continues the turn instead of
        // presenting the fragment as the final answer.
        finish_reason: incomplete.then(|| "length".to_string()),
        rate_limit: None,
    })
}
