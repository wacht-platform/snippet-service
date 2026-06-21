//! Native Gemini adapter with explicit prompt caching.
//!
//! Gemini used to run through the OpenAI-compatibility shim, which gives only
//! implicit (automatic, uncontrolled) caching. This adapter speaks Gemini's
//! native `generateContent` API and manages **explicit** caches via the
//! `cachedContents` endpoint: it caches the stable prefix (system instruction +
//! tools + history up to the volatile tail), reuses it across turns while the
//! prefix matches, recreates it as history grows, and deletes superseded caches.
//! Ported from wacht's `agent-engine/src/llm/gemini`, trimmed to snippet's shape.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamHandle,
    TokenUsage,
};
use crate::tools::ToolError;

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// How long an explicit cache lives before Gemini evicts it.
const CACHE_TTL_SECS: i64 = 1800;
/// Minimum estimated prefix tokens below which explicit caching doesn't pay off.
const CACHE_MIN_TOKENS: usize = 4096;
const CHARS_PER_TOKEN: usize = 4;
/// Trailing `contents` kept OUT of the cache — the live-context message changes
/// every turn, so caching it would invalidate the signature each time.
const LIVE_TAIL: usize = 1;
/// Google's documented skip sentinel for replayed functionCall parts on Gemini 3+
/// (base64 of "skip_thought_signature_validator") — 2.5 doesn't require it.
const SKIP_THOUGHT_SIGNATURE: &str = "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=";

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
}

/// Live explicit-cache handle, held across the session (the model is reused).
#[derive(Clone)]
struct CacheState {
    cache_name: String,
    prefix_signature: String,
    cached_contents_signature: String,
    cached_content_count: usize,
    expire_at: DateTime<Utc>,
}

struct CachePlan {
    /// Payload for `POST /cachedContents` (the full cacheable prefix).
    full_cache_payload: Value,
    /// The generate request to actually send (reduced to a `cachedContent` ref +
    /// delta when the prior cache is reusable).
    send_payload: Value,
    prefix_signature: String,
    cached_contents_signature: String,
    cached_content_count: usize,
}

pub struct GeminiModel {
    config: GeminiConfig,
    client: reqwest::Client,
    cache: Option<CacheState>,
}

impl GeminiModel {
    pub fn new(config: GeminiConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            cache: None,
        }
    }
}

#[async_trait]
impl AgentModel for GeminiModel {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        _sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        // Gemini is buffered (non-streaming) — the sink is unused here.
        let (system, contents) = build_contents(messages, &self.config.model);

        let mut body = serde_json::Map::new();
        body.insert(
            "system_instruction".to_string(),
            json!({ "parts": [{ "text": system }] }),
        );
        body.insert("contents".to_string(), Value::Array(contents));
        body.insert("safetySettings".to_string(), safety_settings());
        if !tools.is_empty() {
            let declarations: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    })
                })
                .collect();
            body.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": declarations }]),
            );
            let mode = if force_tool { "ANY" } else { "AUTO" };
            body.insert(
                "toolConfig".to_string(),
                json!({ "functionCallingConfig": { "mode": mode } }),
            );
        }
        if let Some(temperature) = self.config.temperature {
            body.insert(
                "generationConfig".to_string(),
                json!({ "temperature": temperature }),
            );
        }
        let body = Value::Object(body);

        // Plan explicit caching (reduces the send payload to a cache ref + delta
        // when reusable). Always on; `build_cache_plan` returns None when the
        // prefix is below the worthwhile-token minimum.
        let plan = self.build_cache_plan(&body);
        let send_body = plan
            .as_ref()
            .map(|plan| plan.send_payload.clone())
            .unwrap_or(body);

        let url = format!("{}/models/{}:generateContent", API_BASE, self.config.model);
        let parsed = self.post_generate(&url, &send_body).await?;

        // Refresh the cache for next turn (create/keep cachedContents), best-effort.
        if let Some(plan) = plan {
            self.refresh_cache(&plan).await;
        }

        Ok(map_response(parsed, &self.config.model))
    }
}

impl GeminiModel {
    /// Build an explicit-cache plan from the request body, and (when the prior
    /// cache is still a valid prefix) rewrite the send payload to reference it.
    fn build_cache_plan(&self, body: &Value) -> Option<CachePlan> {
        let mut send = body.clone();
        let obj = send.as_object_mut()?;
        if obj.contains_key("cachedContent") {
            return None;
        }
        let system_instruction = obj.get("system_instruction").cloned();
        let tools = obj.get("tools").cloned();
        let tool_config = obj.get("toolConfig").cloned();
        let contents = obj.get("contents").and_then(Value::as_array).cloned()?;
        if contents.is_empty() {
            return None;
        }

        let cacheable_count = contents.len().saturating_sub(LIVE_TAIL.min(contents.len()));
        let cacheable = contents[..cacheable_count].to_vec();
        let tail = contents[cacheable_count..].to_vec();
        if cacheable.is_empty() {
            return None;
        }

        let stable = json!({
            "systemInstruction": system_instruction,
            "tools": tools,
            "toolConfig": tool_config,
        });
        let prefix_signature = short_hash(serde_json::to_string(&stable).ok()?.as_bytes());
        let cached_contents_signature =
            short_hash(serde_json::to_string(&cacheable).ok()?.as_bytes());

        let mut cache_payload = serde_json::Map::new();
        cache_payload.insert(
            "model".to_string(),
            json!(format!("models/{}", self.config.model)),
        );
        cache_payload.insert("ttl".to_string(), json!(format!("{CACHE_TTL_SECS}s")));
        if let Some(system_instruction) = system_instruction {
            cache_payload.insert("systemInstruction".to_string(), system_instruction);
        }
        cache_payload.insert("contents".to_string(), Value::Array(cacheable.clone()));
        if let Some(tools) = tools {
            cache_payload.insert("tools".to_string(), tools);
        }
        if let Some(tool_config) = tool_config {
            cache_payload.insert("toolConfig".to_string(), tool_config);
        }
        let full_cache_payload = Value::Object(cache_payload);

        if estimate_tokens(&full_cache_payload) < CACHE_MIN_TOKENS {
            return None;
        }

        // If the prior cache is still a valid prefix, send only the delta contents
        // plus the live tail, referencing the cache by name.
        if let Some(prior) = self.cache.as_ref() {
            if self.can_reuse(prior, &prefix_signature, &cacheable) {
                let mut delta = cacheable[prior.cached_content_count..].to_vec();
                delta.extend(tail);
                obj.remove("system_instruction");
                obj.remove("tools");
                obj.remove("toolConfig");
                obj.insert("cachedContent".to_string(), json!(prior.cache_name));
                obj.insert("contents".to_string(), Value::Array(delta));
            }
        }

        Some(CachePlan {
            full_cache_payload,
            send_payload: send,
            prefix_signature,
            cached_contents_signature,
            cached_content_count: cacheable.len(),
        })
    }

    /// Is the prior cache a still-valid prefix of the current cacheable contents?
    fn can_reuse(&self, prior: &CacheState, prefix_signature: &str, cacheable: &[Value]) -> bool {
        if prior.prefix_signature != prefix_signature
            || prior.expire_at <= Utc::now() + chrono::Duration::seconds(5)
            || cacheable.len() < prior.cached_content_count
            || prior.cached_content_count == 0
        {
            return false;
        }
        let cached_prefix = &cacheable[..prior.cached_content_count];
        let signature = serde_json::to_string(cached_prefix)
            .map(|s| short_hash(s.as_bytes()))
            .unwrap_or_default();
        signature == prior.cached_contents_signature
    }

    /// Create (or keep) the explicit cache covering the current prefix, updating
    /// `self.cache` and deleting any superseded cache. Best-effort: a failure just
    /// means the next turn re-sends the prefix uncached.
    async fn refresh_cache(&mut self, plan: &CachePlan) {
        if let Some(prior) = self.cache.as_ref() {
            let unchanged = prior.prefix_signature == plan.prefix_signature
                && prior.cached_contents_signature == plan.cached_contents_signature
                && prior.cached_content_count == plan.cached_content_count
                && prior.expire_at > Utc::now() + chrono::Duration::seconds(5);
            if unchanged {
                return;
            }
        }

        let url = format!("{API_BASE}/cachedContents");
        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.config.api_key)
            .header(CONTENT_TYPE, "application/json")
            .json(&plan.full_cache_payload)
            .timeout(Duration::from_secs(60))
            .send()
            .await;
        let Ok(response) = response else { return };
        if !response.status().is_success() {
            return;
        }
        let Ok(text) = response.text().await else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<CachedContentResponse>(&text) else {
            return;
        };
        let expire_at = parsed
            .expire_time
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(CACHE_TTL_SECS));

        let superseded = self.cache.as_ref().map(|c| c.cache_name.clone());
        self.cache = Some(CacheState {
            cache_name: parsed.name.clone(),
            prefix_signature: plan.prefix_signature.clone(),
            cached_contents_signature: plan.cached_contents_signature.clone(),
            cached_content_count: plan.cached_content_count,
            expire_at,
        });
        if let Some(old) = superseded {
            if old != parsed.name {
                self.delete_cache(&old).await;
            }
        }
    }

    async fn delete_cache(&self, cache_name: &str) {
        if cache_name.is_empty() {
            return;
        }
        let _ = self
            .client
            .delete(format!("{API_BASE}/{cache_name}"))
            .header("x-goog-api-key", &self.config.api_key)
            .timeout(Duration::from_secs(30))
            .send()
            .await;
    }

    async fn post_generate(&self, url: &str, body: &Value) -> Result<GeminiResponse, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();
        let mut empty_retries = 0u32;

        for attempt in 1..=max_attempts {
            let response = self
                .client
                .post(url)
                .header("x-goog-api-key", &self.config.api_key)
                .header(CONTENT_TYPE, "application/json")
                .json(body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    let bytes = match response.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            last_error = error.to_string();
                            if attempt < max_attempts {
                                sleep(self.delay(attempt)).await;
                            }
                            continue;
                        }
                    };
                    if !status.is_success() {
                        let detail = String::from_utf8_lossy(&bytes);
                        last_error =
                            format!("HTTP {status}: {}", detail.chars().take(400).collect::<String>());
                        if is_retryable_status(status) && attempt < max_attempts {
                            sleep(self.delay(attempt)).await;
                            continue;
                        }
                        break;
                    }
                    let parsed: GeminiResponse = serde_json::from_slice(&bytes)
                        .map_err(|error| ToolError::msg(format!("invalid gemini response: {error}")))?;
                    // Gemini flash sometimes returns an empty turn (no parts) even
                    // with tools available — transient. Retry a few times before
                    // handing the empty response back.
                    if response_empty(&parsed) && empty_retries < 3 && attempt < max_attempts {
                        empty_retries += 1;
                        sleep(self.delay(attempt)).await;
                        continue;
                    }
                    return Ok(parsed);
                }
                Err(error) => {
                    last_error = error.to_string();
                    let retryable = error.is_timeout() || error.is_connect() || error.is_request();
                    if retryable && attempt < max_attempts {
                        sleep(self.delay(attempt)).await;
                        continue;
                    }
                    break;
                }
            }
        }

        Err(ToolError::msg(format!(
            "gemini request failed after {max_attempts} attempt(s): {last_error}"
        )))
    }

    fn delay(&self, attempt: u32) -> Duration {
        let multiplier = 2u64.saturating_pow(attempt.saturating_sub(1).min(10));
        Duration::from_millis(
            self.config
                .initial_retry_ms
                .max(1)
                .saturating_mul(multiplier)
                .min(self.config.max_retry_ms.max(1)),
        )
    }
}

fn build_contents(messages: &[HarnessMessage], model: &str) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut contents: Vec<Value> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            HarnessMessage::System { content } if index == 0 => system = content.clone(),
            HarnessMessage::System { content } => push_content(
                &mut contents,
                "user",
                vec![json!({ "text": format!("[runtime_signal]\n{content}\n[/runtime_signal]") })],
            ),
            HarnessMessage::User { content } => {
                push_content(&mut contents, "user", vec![json!({ "text": content })])
            }
            HarnessMessage::Assistant { content, tool_calls } => {
                let mut parts: Vec<Value> = Vec::new();
                if !content.is_empty() {
                    parts.push(json!({ "text": content }));
                }
                for call in tool_calls {
                    let mut part = json!({
                        "functionCall": {
                            "name": call.name,
                            "args": crate::llm::arguments_as_object(&call.arguments),
                        }
                    });
                    // Replay the REAL thought signature when it came from this same
                    // model (signatures are model-specific). Otherwise Gemini 3+
                    // rejects a replayed functionCall without one, so attach the
                    // skip sentinel; Gemini 2.5 needs neither.
                    let own_signature = call
                        .signature
                        .as_deref()
                        .filter(|sig| !sig.is_empty() && call.origin_model.as_deref() == Some(model));
                    if let Some(signature) = own_signature {
                        part["thoughtSignature"] = json!(signature);
                    } else if !is_gemini_2_5(model) {
                        part["thoughtSignature"] = json!(SKIP_THOUGHT_SIGNATURE);
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    push_content(&mut contents, "model", parts);
                }
            }
            HarnessMessage::ToolResult { tool_name, content, .. } => {
                let (cleaned, image) = crate::llm::split_inlined_image(content);
                let response = if cleaned.is_object() {
                    cleaned
                } else {
                    json!({ "result": cleaned })
                };
                let mut parts = vec![json!({
                    "functionResponse": { "name": tool_name, "response": response }
                })];
                if let Some((mime, base64)) = image {
                    parts.push(json!({ "inline_data": { "mime_type": mime, "data": base64 } }));
                }
                push_content(&mut contents, "user", parts);
            }
        }
    }
    (system, contents)
}

/// Append parts, merging into the previous content when it shares the role (so
/// parallel tool calls / results land in one turn, as Gemini expects).
fn push_content(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                existing.append(&mut parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

fn map_response(response: GeminiResponse, model: &str) -> ModelOutput {
    let mut content_text = String::new();
    let mut calls = Vec::new();
    for candidate in &response.candidates {
        for part in &candidate.content.parts {
            if let Some(text) = &part.text {
                content_text.push_str(text);
            }
            if let Some(call) = &part.function_call {
                calls.push(GeneratedToolCall {
                    tool_name: call.name.clone(),
                    arguments: call.args.clone(),
                    // Gemini has no call ids; synthesize one so the harness can
                    // pair the tool_result (functionResponse matches by name).
                    id: Some(uuid::Uuid::new_v4().to_string()),
                    // Capture the thought signature + the model it came from so it
                    // can be replayed next turn (Gemini 3 reasoning continuity).
                    signature: part.thought_signature.clone(),
                    origin_model: Some(model.to_string()),
                });
            }
        }
    }
    let finish_reason = response
        .candidates
        .first()
        .and_then(|candidate| candidate.finish_reason.clone());
    let usage = response.usage_metadata.map(|u| TokenUsage {
        prompt_tokens: u.prompt_token_count as u64,
        completion_tokens: u.candidates_token_count as u64,
        total_tokens: u.total_token_count as u64,
        cache_read_tokens: u.cached_content_token_count.unwrap_or(0) as u64,
        cache_creation_tokens: 0,
    });
    ModelOutput {
        calls,
        content_text: (!content_text.is_empty()).then_some(content_text),
        usage,
        finish_reason,
    }
}

fn response_empty(response: &GeminiResponse) -> bool {
    let has_call = response
        .candidates
        .iter()
        .flat_map(|candidate| candidate.content.parts.iter())
        .any(|part| part.function_call.is_some());
    let has_text = response
        .candidates
        .iter()
        .flat_map(|candidate| candidate.content.parts.iter())
        .any(|part| part.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false));
    !has_call && !has_text
}

fn safety_settings() -> Value {
    json!([
        { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE" },
    ])
}

fn is_gemini_2_5(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("2.5") || model.contains("2-5")
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status.is_server_error()
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn estimate_tokens(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|s| s.chars().count())
        .unwrap_or(0)
        .div_ceil(CHARS_PER_TOKEN)
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: CandidateContent,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Debug, Deserialize)]
struct CandidatePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "functionCall", default)]
    function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "thoughtSignature", default)]
    thought_signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Deserialize)]
struct UsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
    #[serde(rename = "totalTokenCount", default)]
    total_token_count: u32,
}

#[derive(Debug, Deserialize)]
struct CachedContentResponse {
    name: String,
    #[serde(rename = "expireTime", default)]
    expire_time: Option<String>,
}
