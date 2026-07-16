//! Live model catalog — query a provider's own models API so editors can offer
//! real model IDs and (where the provider reports them) real capabilities,
//! instead of a hardcoded list that goes stale.
//!
//! Coverage is uneven by design, because the providers are uneven (verified
//! against live docs, July 2026):
//! - Anthropic: full — `GET /v1/models` returns a `capabilities` tree with per-
//!   level effort flags (low/medium/high/xhigh/max), thinking types, image
//!   input, and `max_input_tokens`.
//! - OpenRouter: partial — `supported_parameters` says whether `reasoning` /
//!   `reasoning_effort` are accepted (not which levels) + `context_length`.
//! - OpenAI-compatible: IDs only — `/models` carries no capability metadata.
//! - Gemini: ListModels has `thinking: bool` + token limits, no levels.
//! - ChatGPT (Codex subscription): no catalog endpoint; returns an empty list.
//!
//! Anything unknown stays `None` — the runtime effort auto-degrade in the
//! harness is the universal fallback for what discovery can't tell us.

use serde::Serialize;
use serde_json::Value;

use crate::config::ModelConfig;

#[derive(Debug, Clone, Serialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Input context window in tokens, when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Effort tiers the provider explicitly supports, ladder order. Only
    /// Anthropic reports this; `None` means "unknown", not "unsupported".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    /// Whether the model supports reasoning/thinking at all; `None` = unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
}

fn catalog_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// Fetch the provider's model list for a profile. Returns a normalized list
/// (possibly empty for providers with no catalog endpoint) or a human-readable
/// error. The API key never leaves this process.
pub async fn fetch_models(cfg: &ModelConfig) -> Result<Vec<CatalogModel>, String> {
    match cfg.provider.as_str() {
        "anthropic" | "anthropic-compatible" => fetch_anthropic(cfg).await,
        "openai" | "openai-compatible" => fetch_openai_compatible(cfg).await,
        // The openrouter provider pins its base URL in build_model; mirror that
        // here so a fresh editor draft (empty base_url) still resolves.
        "openrouter" => {
            let mut c = cfg.clone();
            c.base_url = "https://openrouter.ai/api/v1".to_string();
            fetch_openai_compatible(&c).await
        }
        "gemini" => fetch_gemini(cfg).await,
        // Codex subscription backend has no models endpoint.
        "chatgpt" => Ok(Vec::new()),
        other => Err(format!("no model catalog for provider `{other}`")),
    }
}

/// `…/v1/messages` (from the shared URL builder) → `…/v1/models`, so catalog
/// requests honour the same base-URL quirks the chat path already handles.
fn anthropic_models_url(base_url: &str) -> String {
    let messages = crate::anthropic::anthropic_messages_url(base_url);
    format!("{}models?limit=1000", messages.trim_end_matches("messages"))
}

async fn fetch_anthropic(cfg: &ModelConfig) -> Result<Vec<CatalogModel>, String> {
    let url = anthropic_models_url(&cfg.base_url);
    let response = catalog_client()
        .get(&url)
        .header("x-api-key", cfg.api_key.trim())
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["data"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let caps = &m["capabilities"];
            // Ladder order; a level counts only when explicitly supported.
            let efforts: Vec<String> = ["low", "medium", "high", "xhigh", "max"]
                .iter()
                .filter(|l| caps["effort"][**l]["supported"] == Value::Bool(true))
                .map(|l| l.to_string())
                .collect();
            let effort_supported = caps["effort"]["supported"].as_bool();
            let thinking = caps["thinking"]["supported"].as_bool();
            let reasoning = match (effort_supported, thinking) {
                (None, None) => None,
                (e, t) => Some(e.unwrap_or(false) || t.unwrap_or(false)),
            };
            Some(CatalogModel {
                id,
                display_name: m["display_name"].as_str().map(str::to_string),
                context_window: m["max_input_tokens"].as_u64().filter(|n| *n > 0),
                efforts: effort_supported.map(|_| efforts),
                reasoning,
                supports_images: caps["image_input"]["supported"].as_bool(),
            })
        })
        .collect())
}

/// `…/chat/completions` (from the shared URL builder) → `…/models`. Works for
/// OpenAI, OpenRouter, NVIDIA NIM, DeepSeek, and most compatible gateways.
fn openai_models_url(base_url: &str) -> String {
    let chat = crate::openai::openai_chat_url(base_url);
    format!("{}models", chat.trim_end_matches("chat/completions"))
}

async fn fetch_openai_compatible(cfg: &ModelConfig) -> Result<Vec<CatalogModel>, String> {
    let url = openai_models_url(&cfg.base_url);
    let is_openrouter = cfg.base_url.to_ascii_lowercase().contains("openrouter");
    let mut req = catalog_client().get(&url);
    if !cfg.api_key.trim().is_empty() {
        req = req.bearer_auth(cfg.api_key.trim());
    }
    let response = req.send().await.map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["data"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            // OpenRouter enriches the standard shape with capability hints;
            // plain OpenAI-compatible endpoints return bare IDs.
            let (reasoning, context_window) = if is_openrouter {
                let params = m["supported_parameters"].as_array();
                let reasoning = params.map(|p| {
                    p.iter().any(|x| {
                        matches!(x.as_str(), Some("reasoning") | Some("reasoning_effort"))
                    })
                });
                (reasoning, m["context_length"].as_u64())
            } else {
                (None, None)
            };
            Some(CatalogModel {
                id,
                display_name: m["name"].as_str().map(str::to_string),
                context_window,
                efforts: None,
                reasoning,
                supports_images: None,
            })
        })
        .collect())
}

async fn fetch_gemini(cfg: &ModelConfig) -> Result<Vec<CatalogModel>, String> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000&key={}",
        cfg.api_key.trim()
    );
    let response = catalog_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["models"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            // "models/gemini-3.5-flash" → "gemini-3.5-flash"
            let id = m["name"].as_str()?.trim_start_matches("models/").to_string();
            // Only generateContent-capable models are usable as a chat model.
            let methods = m["supportedGenerationMethods"].as_array();
            if let Some(methods) = methods {
                if !methods.iter().any(|x| x.as_str() == Some("generateContent")) {
                    return None;
                }
            }
            Some(CatalogModel {
                id,
                display_name: m["displayName"].as_str().map(str::to_string),
                context_window: m["inputTokenLimit"].as_u64(),
                efforts: None,
                reasoning: m["thinking"].as_bool(),
                supports_images: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod catalog_url_tests {
    use super::*;

    #[test]
    fn anthropic_models_urls() {
        assert_eq!(
            anthropic_models_url(""),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            anthropic_models_url("https://gw.example.com/v1"),
            "https://gw.example.com/v1/models?limit=1000"
        );
        assert_eq!(
            anthropic_models_url("https://gw.example.com"),
            "https://gw.example.com/v1/models?limit=1000"
        );
    }

    #[test]
    fn openai_models_urls() {
        assert_eq!(
            openai_models_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            openai_models_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/models"
        );
    }
}
