use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::ToolError;

/// Shared HTTP client for every model provider. Sets a connect timeout and — the
/// important part — a READ (idle) timeout: it fires only when NO bytes arrive for
/// the window, so a hung provider ("stopped responding" mid-request) errors out
/// and the agent recovers instead of wedging forever at `Running`. A read timeout
/// (not a total timeout) never cuts off a long-but-active generation, since each
/// received chunk resets it.
pub fn model_http_client(user_agent: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(180));
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua);
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Turn a failed model HTTP response into a short, human-readable explanation
/// instead of dumping the raw status line and the provider's JSON error body at
/// the user. We keep a compact hint — the status code, plus the provider's own
/// message when we can pull it cleanly out of the body — but never the whole
/// payload. Shared by every model adapter so errors read the same everywhere.
pub(crate) fn humanize_http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = status.as_u16();
    let headline = match code {
        429 => "Rate limited — the provider is throttling requests. Wait a moment and try again.",
        401 | 403 => "Authentication failed — check the API key (or sign in again) for this provider.",
        402 => "Payment required — the provider reports you're out of credits or quota.",
        400 | 422 => "The provider rejected the request as invalid.",
        404 => "Not found — the model name or endpoint may be wrong.",
        408 => "The provider timed out handling the request.",
        413 => "The request was too large for the provider.",
        500..=599 => "The provider is having server trouble. Try again shortly.",
        _ => "The model request failed.",
    };
    match extract_provider_message(body) {
        Some(msg) => format!("{headline} ({msg}; HTTP {code})"),
        None => format!("{headline} (HTTP {code})"),
    }
}

/// Best-effort pull of a concise message out of a provider error body. Handles
/// the common shapes — `{"error":{"message":…,"type":…}}`, `{"message":…}`,
/// `{"error":"…"}` — and truncates, so we never re-introduce a wall of JSON.
fn extract_provider_message(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let pick = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.chars().take(160).collect::<String>())
    };
    let v: Value = serde_json::from_str(body).ok()?;
    if let Some(err) = v.get("error") {
        if let Some(m) = err.get("message").and_then(Value::as_str) {
            return pick(m);
        }
        if let Some(t) = err.get("type").and_then(Value::as_str) {
            return pick(t);
        }
        if let Some(s) = err.as_str() {
            return pick(s);
        }
    }
    v.get("message").and_then(Value::as_str).and_then(pick)
}

/// Friendly text for a transport-level failure (no HTTP response at all).
pub(crate) fn humanize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "The request to the provider timed out.".to_string()
    } else if error.is_connect() {
        "Couldn't connect to the provider — check the network or the endpoint URL.".to_string()
    } else {
        "Couldn't reach the provider (network error).".to_string()
    }
}

/// Compose the final user-facing model error. `last_error` is already a friendly
/// sentence; we only tack on the retry count when we actually retried, so a
/// first-try failure (e.g. a rate limit surfaced immediately) reads cleanly.
pub(crate) fn final_model_error(last_error: &str, attempts: u32) -> String {
    let last_error = last_error.trim();
    let msg = if last_error.is_empty() {
        "The model request failed."
    } else {
        last_error
    };
    if attempts > 1 {
        format!("{msg} (after {attempts} attempts)")
    } else {
        msg.to_string()
    }
}

/// True when a model error means the configured reasoning effort / thinking
/// setting itself was rejected — an invalid-request (400/422) response whose
/// provider message names the reasoning knob. No provider except Anthropic
/// exposes which effort tiers a model supports, so this is the reliable
/// cross-provider signal: try the configured tier, and step down on rejection.
pub fn is_effort_rejection(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    let invalid = m.contains("http 400") || m.contains("http 422");
    invalid
        && (m.contains("reasoning_effort")
            || m.contains("reasoning.effort")
            || m.contains("reasoning effort")
            || m.contains("thinking")
            || m.contains("budget_tokens"))
}

/// One step down the effort ladder. `None` = the ladder is exhausted; the
/// caller drops the reasoning knob entirely (provider default / no thinking).
pub fn degrade_effort(current: &str) -> Option<&'static str> {
    match current.to_ascii_lowercase().as_str() {
        "max" => Some("xhigh"),
        "xhigh" => Some("high"),
        "high" => Some("medium"),
        "medium" => Some("low"),
        _ => None,
    }
}

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

    /// Clear text + thinking — fresh per turn/step so thinking never accumulates.
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
    /// Gemini `thoughtSignature` for this call (opaque reasoning handle). Replayed
    /// on the next turn so Gemini 3 keeps its chain-of-thought across tool calls.
    #[serde(default)]
    pub signature: Option<String>,
    /// Model the call (and its `signature`) originated on. Signatures are
    /// model-specific, so we only replay one when the current model matches.
    #[serde(default)]
    pub origin_model: Option<String>,
}

/// One native tool call recorded on an assistant turn, paired with a
/// `ToolResult` message carrying the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Gemini thought signature + originating model, persisted so it can be
    /// replayed on later turns (see `GeneratedToolCall`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_model: Option<String>,
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
    /// ChatGPT-subscription rate-limit usage, when the provider reports it.
    #[serde(default)]
    pub rate_limit: Option<RateLimitSnapshot>,
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

/// ChatGPT-subscription rate-limit usage (parsed from Codex response headers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RateLimitSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: i64,
    /// Unix epoch seconds when the window resets (0 if unknown).
    pub resets_at: i64,
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

    /// Whether this model accepts image inputs. Text-only models return false, so
    /// the harness sends a text placeholder instead of inlining image bytes —
    /// avoiding a multimodal 400 and un-sticking an already-poisoned conversation.
    fn supports_images(&self) -> bool {
        false
    }

    /// Whether the model has usable credentials (API key / OAuth tokens). When
    /// false the harness refuses the API call and asks the user to configure one.
    fn is_configured(&self) -> bool {
        true
    }

    /// Temporarily swap the model's reasoning effort, returning the prior value so
    /// the caller can restore it. Compaction/summary/reflection run this down to a
    /// minimal effort: those are mechanical tool-loops (~24 sequential calls), and
    /// full chain-of-thought on a reasoning model just burns wall-clock. Default
    /// no-op for models without a reasoning knob.
    fn swap_reasoning_effort(&mut self, _effort: Option<String>) -> Option<String> {
        None
    }
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

    fn supports_images(&self) -> bool {
        (**self).supports_images()
    }

    fn is_configured(&self) -> bool {
        (**self).is_configured()
    }

    fn swap_reasoning_effort(&mut self, effort: Option<String>) -> Option<String> {
        (**self).swap_reasoning_effort(effort)
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
    Summary {
        kind: String,
        content: String,
    },
    System {
        content: String,
    },
}
