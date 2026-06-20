use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition,
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
}

pub struct OpenAiCompatibleModel {
    config: OpenAiCompatibleConfig,
    client: reqwest::Client,
}

impl OpenAiCompatibleModel {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl AgentModel for OpenAiCompatibleModel {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> Result<ModelOutput, ToolError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = self.build_chat_request(messages, tools, force_tool);
        let bytes = self.send_with_retries(&url, &body).await?;

        let response: ChatResponse = serde_json::from_slice(&bytes)?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::msg("model response had no choices"))?;
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
        })
    }
}

impl OpenAiCompatibleModel {
    fn build_chat_request(
        &self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> ChatRequest {
        ChatRequest {
            model: self.config.model.clone(),
            messages: messages
                .iter()
                .enumerate()
                .map(|(index, message)| chat_message_from_harness(index, message))
                .collect(),
            tools: tools.iter().map(chat_tool_from_definition).collect(),
            temperature: self.config.temperature,
            // `required` needs tools to choose from, else providers reject it.
            tool_choice: (force_tool && !tools.is_empty()).then_some("required"),
            // Opt into usage accounting on OpenRouter so cache-read tokens surface.
            usage: self
                .config
                .base_url
                .contains("openrouter")
                .then_some(UsageRequest { include: true }),
        }
    }

    async fn send_with_retries(&self, url: &str, body: &ChatRequest) -> Result<Vec<u8>, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();

        for attempt in 1..=max_attempts {
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
            "model request failed after {} attempt(s): {}",
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

fn chat_message_from_harness(index: usize, message: &HarnessMessage) -> ChatMessage {
    match message {
        HarnessMessage::System { content } if index == 0 => ChatMessage::text("system", content),
        HarnessMessage::System { content } => ChatMessage::text(
            "user",
            &format!("[runtime_signal]\n{content}\n[/runtime_signal]"),
        ),
        HarnessMessage::User { content } => ChatMessage::text("user", content),
        HarnessMessage::Assistant { content, tool_calls } => {
            let out_calls = tool_calls
                .iter()
                .map(|call| OutToolCall {
                    id: call.id.clone(),
                    call_type: "function",
                    function: OutToolCallFunction {
                        name: call.name.clone(),
                        arguments: serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
                .collect();
            ChatMessage::assistant(content, out_calls)
        }
        HarnessMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
        } => {
            let body =
                serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string());
            if tool_call_id.is_empty() {
                // Legacy state written before native function calling — render as
                // a text block so old sessions still load.
                ChatMessage::text(
                    "user",
                    &format!("[tool_result]\ntool = \"{tool_name}\"\noutput = {body}\n[/tool_result]"),
                )
            } else {
                ChatMessage::tool(tool_call_id, &body)
            }
        }
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
    // OpenRouter only reports detailed token accounting (incl.
    // prompt_tokens_details.cached_tokens) when usage accounting is opted into.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageRequest>,
}

#[derive(Debug, Serialize)]
struct UsageRequest {
    include: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    // Assistant messages that carry only tool calls send `content: null`, so it
    // must be omittable.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OutToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn assistant(content: &str, tool_calls: Vec<OutToolCall>) -> Self {
        // An assistant turn that is purely tool calls sends null content.
        let content = if content.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(content.to_string())
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
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
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

#[derive(Debug, Clone)]
pub struct GithubCopilotConfig {
    pub github_token: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
}

pub struct GithubCopilotModel {
    config: GithubCopilotConfig,
    client: reqwest::Client,
}

impl GithubCopilotModel {
    pub fn new(config: GithubCopilotConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    async fn get_copilot_token(&self) -> Result<String, ToolError> {
        let response = self
            .client
            .get("https://api.github.com/copilot_internal/v2/token")
            .header(reqwest::header::USER_AGENT, "GitHubCopilotChat/0.26.7")
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.config.github_token.trim()))
            .send()
            .await
            .map_err(|e| ToolError::msg(format!("failed to request copilot token: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ToolError::msg(format!("GitHub token exchange failed: HTTP {status} - {body}")));
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }

        let res: TokenResponse = response.json().await.map_err(|e| {
            ToolError::msg(format!("failed to parse copilot token JSON: {e}"))
        })?;

        Ok(res.token)
    }

    fn build_chat_request(
        &self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> ChatRequest {
        ChatRequest {
            model: self.config.model.clone(),
            messages: messages
                .iter()
                .enumerate()
                .map(|(index, message)| chat_message_from_harness(index, message))
                .collect(),
            tools: tools.iter().map(chat_tool_from_definition).collect(),
            temperature: self.config.temperature,
            // `required` needs tools to choose from, else providers reject it.
            tool_choice: (force_tool && !tools.is_empty()).then_some("required"),
            usage: None,
        }
    }

    async fn send_with_retries(&self, url: &str, body: &ChatRequest) -> Result<Vec<u8>, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let mut last_error = String::new();

        let session_token = self.get_copilot_token().await?;

        for attempt in 1..=max_attempts {
            let response = self
                .client
                .post(url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", session_token))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header("editor-version", "vscode/1.99.0")
                .header("editor-plugin-version", "copilot-chat/0.26.7")
                .header("user-agent", "GitHubCopilotChat/0.26.7")
                .header("copilot-integration-id", "vscode-chat")
                .header("x-github-api-version", "2025-04-01")
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
            "model request failed after {} attempt(s): {}",
            max_attempts, last_error
        )))
    }
}

#[async_trait]
impl AgentModel for GithubCopilotModel {
    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
    ) -> Result<ModelOutput, ToolError> {
        let url = "https://api.githubcopilot.com/chat/completions";
        let body = self.build_chat_request(messages, tools, force_tool);
        let bytes = self.send_with_retries(url, &body).await?;

        let response: ChatResponse = serde_json::from_slice(&bytes)?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::msg("model response had no choices"))?;
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
        })
    }
}
