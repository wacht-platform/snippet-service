//! xAI (Grok) Responses adapter. SuperGrok/X Premium OAuth talks to
//! `https://api.x.ai/v1/responses` so built-in server tools such as `x_search`
//! can run on xAI. Snippet client tools stay mixed in as function tools; the
//! harness loop is unchanged.

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamBuffer,
    StreamHandle, TokenUsage,
};
use crate::openai::{
    OpenAiCompatibleConfig, is_retryable_status, is_retryable_transport_error, retry_after_delay,
    retry_delay,
};
use crate::tools::ToolError;

const RESPONSES_PATH: &str = "/responses";

#[derive(Debug, Clone)]
pub struct XaiConfig {
    pub inner: OpenAiCompatibleConfig,
    /// Attach xAI's built-in `{ "type": "x_search" }` server tool.
    pub x_search: bool,
}

pub struct XaiModel {
    config: XaiConfig,
    client: reqwest::Client,
}

impl XaiModel {
    pub fn new(config: XaiConfig) -> Self {
        let user_agent = config
            .inner
            .user_agent
            .clone()
            .unwrap_or_else(|| "snippet/xai".to_string());
        let client = crate::llm::model_http_client(Some(&user_agent));
        Self { config, client }
    }

    fn responses_url(&self) -> String {
        let base = self.config.inner.base_url.trim().trim_end_matches('/');
        if base.ends_with(RESPONSES_PATH) {
            base.to_string()
        } else {
            format!("{base}{RESPONSES_PATH}")
        }
    }

    async fn bearer_token(&mut self) -> Result<String, ToolError> {
        if self.config.inner.oauth_xai {
            crate::xai_auth::access_token()
                .await
                .map_err(|e| ToolError::model_request(e, false))
        } else {
            let key = self.config.inner.api_key.trim();
            if key.is_empty() {
                Err(ToolError::model_request(
                    "xAI API key missing — sign in with SuperGrok or set a key.".to_string(),
                    false,
                ))
            } else {
                Ok(key.to_string())
            }
        }
    }
}

#[async_trait]
impl AgentModel for XaiModel {
    fn is_configured(&self) -> bool {
        if self.config.inner.oauth_xai {
            crate::xai_auth::is_signed_in()
        } else {
            !self.config.inner.api_key.trim().is_empty()
        }
    }

    fn swap_reasoning_effort(&mut self, effort: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.config.inner.reasoning_effort, effort)
    }

    fn supports_images(&self) -> bool {
        self.config.inner.supports_images
    }

    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        let body = build_responses_request(&self.config, messages, tools, force_tool);
        let url = self.responses_url();
        let max_attempts = self.config.inner.max_retries.saturating_add(1).max(1);
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
            let token = self.bearer_token().await?;
            let stream = sink.is_some() || self.config.inner.stream;
            let mut payload = body.clone();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("stream".into(), json!(stream));
            }

            let mut request = self
                .client
                .post(&url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(CONTENT_TYPE, "application/json")
                .json(&payload);
            if stream {
                request = request.header(reqwest::header::ACCEPT, "text/event-stream");
            }

            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let parsed = if stream {
                            parse_responses_sse(response, sink.as_ref()).await
                        } else {
                            parse_responses_json(response, sink.as_ref()).await
                        };
                        match parsed {
                            Ok(output) => return Ok(output),
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
                                    self.config.inner.initial_retry_ms,
                                    self.config.inner.max_retry_ms,
                                ))
                                .await;
                                continue;
                            }
                        }
                    }
                    if status == StatusCode::UNAUTHORIZED
                        && self.config.inner.oauth_xai
                        && !refreshed
                    {
                        refreshed = true;
                        if let Some(prior) = crate::xai_auth::load_blocking() {
                            match crate::xai_auth::refresh(&prior).await {
                                Ok(fresh) => {
                                    let _ = crate::xai_auth::save_blocking(&fresh);
                                    attempt = attempt.saturating_sub(1);
                                    continue;
                                }
                                Err(e) => {
                                    return Err(ToolError::model_request(
                                        format!("xAI token refresh failed: {e} — sign in again."),
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
                        self.config.inner.initial_retry_ms,
                        self.config.inner.max_retry_ms,
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
                        self.config.inner.initial_retry_ms,
                        self.config.inner.max_retry_ms,
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

fn normalize_effort(effort: Option<&str>) -> Option<String> {
    let e = effort?.trim();
    if e.is_empty() || e.eq_ignore_ascii_case("off") {
        return None;
    }
    Some(e.to_lowercase())
}

fn build_responses_request(
    config: &XaiConfig,
    messages: &[HarnessMessage],
    tools: &[NativeToolDefinition],
    force_tool: bool,
) -> Value {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    let mut seen_calls: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (index, message) in messages.iter().enumerate() {
        match message {
            HarnessMessage::System { content } if index == 0 => {
                instructions = content.clone();
                if config.x_search {
                    instructions.push_str(
                        "\n\nFor current discussion on X/Twitter, the built-in X search runs automatically; use `web_search` for the open web; use browser tools only for logged-in sites.",
                    );
                }
            }
            HarnessMessage::System { content } => {
                input.push(message_item(
                    "user",
                    &format!("[steering]\n{content}\n[/steering]"),
                ));
            }
            HarnessMessage::User { content } => {
                input.push(message_item("user", content));
            }
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                if !content.is_empty() {
                    input.push(message_item("assistant", content));
                }
                for call in tool_calls {
                    let args =
                        serde_json::to_string(&crate::llm::arguments_as_object(&call.arguments))
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
                let output =
                    serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| cleaned.to_string());
                if tool_call_id.is_empty() {
                    input.push(message_item(
                        "user",
                        &format!("[tool_result]\ntool = \"{tool_name}\"\noutput = {output}\n[/tool_result]"),
                    ));
                } else if !seen_calls.contains(tool_call_id) {
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
                        if config.inner.supports_images {
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
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_string);
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

    let mut tools_json: Vec<Value> = Vec::new();
    if config.x_search {
        tools_json.push(json!({ "type": "x_search" }));
    }
    for t in tools {
        tools_json.push(json!({
            "type": "function",
            "name": t.name,
            "description": t.description,
            "strict": false,
            "parameters": t.input_schema,
        }));
    }

    // Never force x_search; required tool choice only applies to client tools.
    let tool_choice = if force_tool && tools.iter().any(|t| !t.name.is_empty()) {
        json!("required")
    } else {
        json!("auto")
    };

    let mut body = json!({
        "model": config.inner.model,
        "input": input,
        "tool_choice": tool_choice,
        "parallel_tool_calls": true,
        "store": false,
        "stream": false,
    });
    let obj = body.as_object_mut().expect("object");
    if !instructions.is_empty() {
        obj.insert("instructions".to_string(), json!(instructions));
    }
    if !tools_json.is_empty() {
        obj.insert("tools".to_string(), json!(tools_json));
    }
    if let Some(effort) = normalize_effort(config.inner.reasoning_effort.as_deref()) {
        obj.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    body
}

fn message_item(role: &str, text: &str) -> Value {
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

fn is_client_function_item(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") | Some("function") => true,
        _ => false,
    }
}

fn collect_client_call(item: &Value, calls: &mut Vec<GeneratedToolCall>) {
    if !is_client_function_item(item) {
        return;
    }
    let name = item
        .get("name")
        .or_else(|| item.pointer("/function/name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.is_empty() || name == "x_search" || name == "web_search" || name == "code_interpreter" {
        return;
    }
    let args_str = item
        .get("arguments")
        .or_else(|| item.pointer("/function/arguments"))
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let arguments = serde_json::from_str::<Value>(args_str)
        .unwrap_or_else(|_| Value::String(args_str.to_string()));
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    calls.push(GeneratedToolCall {
        tool_name: name.to_string(),
        arguments,
        id,
        ..Default::default()
    });
}

fn append_citations(text: &mut String, value: &Value) {
    let mut urls: Vec<String> = Vec::new();
    if let Some(list) = value.get("citations").and_then(Value::as_array) {
        for item in list {
            if let Some(url) = item
                .as_str()
                .or_else(|| item.get("url").and_then(Value::as_str))
            {
                if !url.is_empty() && !urls.iter().any(|u| u == url) {
                    urls.push(url.to_string());
                }
            }
        }
    }
    if urls.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\nSources:");
    for url in urls {
        text.push_str("\n- ");
        text.push_str(&url);
    }
}

fn usage_from_value(u: &Value) -> TokenUsage {
    TokenUsage {
        prompt_tokens: u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion_tokens: u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_read_tokens: u
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Default::default()
    }
}

async fn parse_responses_json(
    response: reqwest::Response,
    sink: Option<&StreamHandle>,
) -> Result<ModelOutput, ToolError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ToolError::model_request(format!("xAI response body: {e}"), true))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        ToolError::model_request(format!("could not parse xAI responses JSON: {e}"), false)
    })?;
    parse_responses_value(&value, sink)
}

fn parse_responses_value(
    value: &Value,
    sink: Option<&StreamHandle>,
) -> Result<ModelOutput, ToolError> {
    if let Some(err) = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Err(ToolError::model_request(format!("xAI: {err}"), false));
    }

    let mut text = String::new();
    let mut calls: Vec<GeneratedToolCall> = Vec::new();
    let output = value
        .get("output")
        .or_else(|| value.pointer("/response/output"))
        .and_then(Value::as_array);
    if let Some(items) = output {
        for item in items {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "message" => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(chunk) = part
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                text.push_str(chunk);
                            }
                        }
                    }
                }
                "output_text" => {
                    if let Some(chunk) = item.get("text").and_then(Value::as_str) {
                        text.push_str(chunk);
                    }
                }
                "function_call" | "function" => collect_client_call(item, &mut calls),
                "x_search" | "web_search" | "code_interpreter" | "reasoning" => {}
                _ => collect_client_call(item, &mut calls),
            }
        }
    }
    if text.is_empty() {
        if let Some(chunk) = value
            .get("output_text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            text.push_str(chunk);
        }
    }
    append_citations(&mut text, value);
    if let Some(sink) = sink {
        if !text.is_empty() {
            StreamBuffer::append(sink, &text);
        }
    }
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/response/usage"))
        .filter(|u| u.is_object())
        .map(usage_from_value);
    Ok(ModelOutput {
        calls,
        content_text: (!text.is_empty()).then_some(text),
        usage,
        finish_reason: value
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| *s == "incomplete")
            .map(|_| "length".to_string()),
        rate_limit: None,
    })
}

async fn parse_responses_sse(
    response: reqwest::Response,
    sink: Option<&StreamHandle>,
) -> Result<ModelOutput, ToolError> {
    let mut text = String::new();
    let mut calls: Vec<GeneratedToolCall> = Vec::new();
    let mut usage: Option<TokenUsage> = None;
    let mut failure: Option<String> = None;
    let mut incomplete = false;
    let mut citations: Option<Value> = None;

    crate::sse::for_each_event(response, |data| {
        if data == "[DONE]" {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = chunk
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    text.push_str(delta);
                    if let Some(sink) = sink {
                        StreamBuffer::append(sink, delta);
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let (Some(sink), Some(delta)) = (
                    sink,
                    chunk
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty()),
                ) {
                    StreamBuffer::append_thinking(sink, delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = chunk.get("item") {
                    collect_client_call(item, &mut calls);
                }
            }
            "response.completed" => {
                if let Some(u) = chunk.pointer("/response/usage").filter(|u| u.is_object()) {
                    usage = Some(usage_from_value(u));
                }
                if let Some(c) = chunk.pointer("/response/citations") {
                    citations = Some(c.clone());
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
    .map_err(|e| ToolError::model_request(format!("xAI stream error: {e}"), true))?;

    if let Some(c) = citations.as_ref() {
        let mut wrapper = json!({});
        if let Some(obj) = wrapper.as_object_mut() {
            obj.insert("citations".into(), c.clone());
        }
        append_citations(&mut text, &wrapper);
    }

    if let Some(err) = failure {
        if text.is_empty() && calls.is_empty() {
            return Err(ToolError::model_request(format!("xAI: {err}"), false));
        }
    }

    Ok(ModelOutput {
        calls,
        content_text: (!text.is_empty()).then_some(text),
        usage,
        finish_reason: incomplete.then(|| "length".to_string()),
        rate_limit: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::NativeToolDefinition;

    fn sample_tools() -> Vec<NativeToolDefinition> {
        vec![NativeToolDefinition {
            name: "bash".into(),
            description: "run a command".into(),
            input_schema: json!({"type": "object"}),
        }]
    }

    fn cfg(x_search: bool) -> XaiConfig {
        XaiConfig {
            inner: OpenAiCompatibleConfig {
                api_key: String::new(),
                base_url: "https://api.x.ai/v1".into(),
                model: "grok-4.6".into(),
                temperature: None,
                max_retries: 2,
                initial_retry_ms: 1,
                max_retry_ms: 1,
                user_agent: None,
                supports_images: true,
                reasoning_effort: Some("high".into()),
                stream: false,
                session_id: None,
                oauth_xai: true,
            },
            x_search,
        }
    }

    #[test]
    fn request_includes_x_search_and_function_tools() {
        let body = build_responses_request(
            &cfg(true),
            &[HarnessMessage::User {
                content: "what's on X?".into(),
            }],
            &sample_tools(),
            false,
        );
        assert_eq!(body["store"], json!(false));
        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["type"], "x_search");
        assert_eq!(tools[1]["type"], "function");
        assert_eq!(tools[1]["name"], "bash");
    }

    #[test]
    fn request_omits_x_search_when_disabled() {
        let body = build_responses_request(
            &cfg(false),
            &[HarnessMessage::User {
                content: "hi".into(),
            }],
            &sample_tools(),
            false,
        );
        let tools = body["tools"].as_array().expect("tools");
        assert!(tools.iter().all(|t| t["type"] != "x_search"));
        assert_eq!(tools[0]["name"], "bash");
    }

    #[test]
    fn parser_maps_only_client_function_calls() {
        let value = json!({
            "output": [
                {"type": "x_search", "name": "x_search"},
                {
                    "type": "function_call",
                    "name": "bash",
                    "call_id": "call_1",
                    "arguments": "{\"command\":\"ls\"}"
                },
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
            ],
            "citations": ["https://x.com/status/1"]
        });
        let out = parse_responses_value(&value, None).expect("parse");
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].tool_name, "bash");
        assert_eq!(out.calls[0].id.as_deref(), Some("call_1"));
        let text = out.content_text.unwrap();
        assert!(text.contains("done"));
        assert!(text.contains("https://x.com/status/1"));
    }

    #[test]
    fn parser_ignores_server_tool_names() {
        let value = json!({
            "output": [{
                "type": "function_call",
                "name": "x_search",
                "arguments": "{}"
            }]
        });
        let out = parse_responses_value(&value, None).expect("parse");
        assert!(out.calls.is_empty());
    }
}
