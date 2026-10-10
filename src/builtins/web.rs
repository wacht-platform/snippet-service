use super::*;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
pub struct WebSearchTool {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_web_results")]
    num_results: usize,
}

fn default_web_results() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web (via Exa) for anything beyond the local workspace — current events, library/API docs, error messages, release notes, best practices. Returns ranked results with title, URL, publish date, and a text snippet from each page. Use a focused natural-language query.".to_string(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Natural-language search query."
                    },
                    "num_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 5,
                        "description": "How many results to return (1-10)."
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WebSearchArgs = expect_object("web_search", arguments)?;
        let num = args.num_results.clamp(1, 10);
        let body = json!({
            "query": args.query,
            "numResults": num,
            "type": "auto",
            "contents": { "text": { "maxCharacters": 1200 } },
        });

        let response = reqwest::Client::new()
            .post("https://api.exa.ai/search")
            .header("x-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| ToolError::msg(format!("exa request failed: {error}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ToolError::msg(format!("reading exa response failed: {error}")))?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Ok(ToolResult::error(format!(
                "exa search failed: HTTP {status}: {detail}"
            )));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::msg(format!("invalid exa response: {error}")))?;

        let results: Vec<Value> = parsed
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let snippet: String = item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(1000)
                            .collect();
                        json!({
                            "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                            "published_date": item.get("publishedDate").and_then(Value::as_str),
                            "snippet": snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ToolResult::success(json!({
            "query": args.query,
            "count": results.len(),
            "results": results,
        })))
    }
}

pub struct WebReadTool {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
struct WebReadArgs {
    url: String,
    #[serde(default = "default_read_chars")]
    max_characters: usize,
}

fn default_read_chars() -> usize {
    8000
}

#[async_trait]
impl Tool for WebReadTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "web_read".to_string(),
            description: "Fetch and read the full text of a specific web page by URL (via Exa) — use after web_search to read a result in depth, or to read any known URL (docs page, issue, article). Returns the page's extracted text.".to_string(),
            input_schema: object_schema(
                json!({
                    "url": {
                        "type": "string",
                        "description": "The full URL of the page to read."
                    },
                    "max_characters": {
                        "type": "integer",
                        "minimum": 500,
                        "maximum": 10000,
                        "default": 8000,
                        "description": "Maximum characters of page text to return (Exa caps this at 10000)."
                    }
                }),
                &["url"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WebReadArgs = expect_object("web_read", arguments)?;
        let max_chars = args.max_characters.clamp(500, 10_000);
        let body = json!({
            "urls": [args.url],
            "text": { "maxCharacters": max_chars },
            // 0 = fetch fresh: the documented way to crawl a URL not already in
            // Exa's index (replaces the deprecated `livecrawl`).
            "maxAgeHours": 0,
        });

        let response = reqwest::Client::new()
            .post("https://api.exa.ai/contents")
            .header("x-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| ToolError::msg(format!("exa request failed: {error}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ToolError::msg(format!("reading exa response failed: {error}")))?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Ok(ToolResult::error(format!(
                "exa read failed: HTTP {status}: {detail}"
            )));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::msg(format!("invalid exa response: {error}")))?;

        let Some(result) = parsed
            .get("results")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
        else {
            // Exa reports per-URL failures in `statuses[].error` rather than results.
            let reason = parsed
                .get("statuses")
                .and_then(Value::as_array)
                .and_then(|s| s.first())
                .and_then(|s| s.get("error"))
                .and_then(Value::as_str)
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            return Ok(ToolResult::error(format!(
                "no content returned for `{}` — the page may be unreachable or blocked.{reason}",
                args.url
            )));
        };

        let text: String = result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(max_chars)
            .collect();

        Ok(ToolResult::success(json!({
            "url": result.get("url").and_then(Value::as_str).unwrap_or(&args.url),
            "title": result.get("title").and_then(Value::as_str).unwrap_or(""),
            "published_date": result.get("publishedDate").and_then(Value::as_str),
            "text": text,
        })))
    }
}

