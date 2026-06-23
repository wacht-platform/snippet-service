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
            client: reqwest::Client::new(),
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
        let body_preview = serde_json::to_string(&body)
            .unwrap_or_else(|_| "<unserializable body>".to_string())
            .chars()
            .take(2000)
            .collect::<String>();
        let content_type = "application/json";
        {
            let log_path = std::path::PathBuf::from("/tmp/snippet/.snippet/debug.log");
            if let Some(parent) = log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                use std::io::Write;
                let _ = writeln!(file, "chatgpt_request url={RESPONSES_URL} content_type={content_type} body={body_preview}");
            }
        }
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut fatal = false;
        let mut refreshed = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
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
                        return parse_responses_sse(response, sink.as_ref()).await;
                    }
                    // Expired/invalid token → refresh once, then retry without
                    // consuming a backoff attempt.
                    if status == StatusCode::UNAUTHORIZED && !refreshed {
                        refreshed = true;
                        if let Some(prior) = self.tokens.clone() {
                            match chatgpt_auth::refresh(&prior).await {
                                Ok(fresh) => {
                                    self.tokens = Some(fresh);
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
                    {
                        let log_path = std::path::PathBuf::from("/tmp/snippet/.snippet/debug.log");
                        if let Some(parent) = log_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                            use std::io::Write;
                            let _ = writeln!(file, "chatgpt_response status={status} body={text}");
                        }
                    }
                    last_error = format!("HTTP {status}: {text}");
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
                    last_error = error.to_string();
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
            format!("chatgpt request failed after {attempts} attempt(s): {last_error}"),
            !fatal,
        ))
    }
}

/// Normalize a reasoning-effort string for the Codex backend (it rejects
/// "minimal"; "off"/empty disables reasoning entirely).
fn normalize_effort(effort: Option<&str>) -> Option<String> {
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
    // The Responses API rejects a `function_call_output` whose `call_id` has no
    // preceding `function_call` (e.g. after compaction summarized the assistant
    // turn that made the call). Track emitted call ids and drop orphaned outputs.
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
                    // Orphaned result (its call was compacted away) — keep the content
                    // as plain text so it isn't lost, but not as a function_call_output
                    // the API would reject.
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
    .map_err(|e| ToolError::msg(format!("chatgpt stream error: {e}")))?;

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
        finish_reason: None,
    })
}
