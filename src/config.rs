use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::llm::AgentModel;
use crate::openai::{OpenAiCompatibleModel, OpenAiCompatibleConfig};
use crate::anthropic::{AnthropicModel, AnthropicConfig};
use crate::tools::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetConfig {
    #[serde(default = "default_workspace")]
    pub workspace: PathBuf,
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,
    #[serde(default)]
    pub resume_on_start: bool,
    #[serde(default, skip_serializing, alias = "active_profile", alias = "active_setup")]
    pub active_setup: Option<String>,
    #[serde(default, skip_serializing, alias = "profiles", alias = "setups")]
    pub setups: Option<std::collections::HashMap<String, ModelConfig>>,
    #[serde(default)]
    pub model: ModelConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_initial_retry_ms")]
    pub initial_retry_ms: u64,
    #[serde(default = "default_max_retry_ms")]
    pub max_retry_ms: u64,
    /// Model context window in tokens, for the status-bar usage gauge.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default = "default_cache_prompt")]
    pub cache_prompt: bool,
}

impl Default for SnippetConfig {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            state_path: default_state_path(),
            resume_on_start: false,
            active_setup: None,
            setups: None,
            model: ModelConfig::default(),
        }
    }
}

impl SnippetConfig {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        let path = path.as_ref();
        
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let raw = match tokio::fs::read_to_string(path).await {
            Ok(content) => content,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                let default_config = Self::default();
                let toml_str = toml::to_string_pretty(&default_config).map_err(|err| {
                    ToolError::msg(format!("failed to serialize default config: {err}"))
                })?;
                tokio::fs::write(path, toml_str).await.map_err(|err| {
                    ToolError::msg(format!("failed to write default config to `{}`: {err}", path.display()))
                })?;
                let mut config = default_config;
                config.resolve_relative_paths(path);
                return Ok(config);
            }
            Err(error) => {
                return Err(ToolError::msg(format!(
                    "failed to read config `{}`: {error}",
                    path.display()
                )));
            }
        };

        let mut config: Self = toml::from_str(&raw).map_err(|error| {
            ToolError::msg(format!(
                "failed to parse config `{}` as TOML: {error}",
                path.display()
            ))
        })?;

        if let Some(ref setups) = config.setups {
            if let Some(ref active) = config.active_setup {
                if let Some(active_model) = setups.get(active) {
                    config.model = active_model.clone();
                }
            } else if let Some(default_model) = setups.get("default") {
                config.model = default_model.clone();
            }
        }

        match config.model.provider.as_str() {
            "openai-compatible" | "openai" | "anthropic" | "gemini" | "openrouter" => {}
            other => {
                return Err(ToolError::msg(format!(
                    "unsupported model.provider `{}`; expected one of `openai-compatible`, `openai`, `anthropic`, `gemini`, `openrouter`",
                    other
                )));
            }
        }
        config.resolve_relative_paths(path);
        Ok(config)
    }

    fn resolve_relative_paths(&mut self, _config_path: &Path) {
        if self.workspace.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                self.workspace = cwd.join(&self.workspace);
            }
        }
        if self.state_path.is_relative() {
            let workspace_hash = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                self.workspace.hash(&mut hasher);
                hasher.finish()
            };
            let workspace_name = self.workspace.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace");
            
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            
            self.state_path = home.join(format!(
                ".snippet/workspaces/{}-{:x}/state.json",
                workspace_name,
                workspace_hash
            ));
        }
    }
}

impl ModelConfig {
    pub fn build_model(&self) -> Box<dyn AgentModel> {
        match self.provider.as_str() {
            "openai" => {
                let mut config: OpenAiCompatibleConfig = self.clone().into();
                if config.base_url == "https://api.openai.com/v1" || config.base_url == "https://inference.signalstac.xyz/v1" {
                    config.base_url = "https://api.openai.com/v1".to_string();
                }
                Box::new(OpenAiCompatibleModel::new(config))
            }

            "gemini" => {
                let mut config: OpenAiCompatibleConfig = self.clone().into();
                config.base_url = "https://generativelanguage.googleapis.com/v1beta/openai/".to_string();
                Box::new(OpenAiCompatibleModel::new(config))
            }
            "anthropic" => {
                let config = AnthropicConfig {
                    api_key: self.api_key.clone(),
                    model: self.model.clone(),
                    temperature: self.temperature,
                    max_retries: self.max_retries,
                    initial_retry_ms: self.initial_retry_ms,
                    max_retry_ms: self.max_retry_ms,
                    cache_prompt: self.cache_prompt,
                };
                Box::new(AnthropicModel::new(config))
            }
            "openrouter" => {
                let mut config: OpenAiCompatibleConfig = self.clone().into();
                config.base_url = "https://openrouter.ai/api/v1".to_string();
                if config.model.is_empty() {
                    config.model = "google/gemini-2.5-pro".to_string();
                }
                Box::new(OpenAiCompatibleModel::new(config))
            }
            _ => {
                Box::new(OpenAiCompatibleModel::new(self.clone().into()))
            }
        }
    }
}

impl From<ModelConfig> for OpenAiCompatibleConfig {
    fn from(value: ModelConfig) -> Self {
        Self {
            api_key: value.api_key,
            base_url: value.base_url,
            model: value.model,
            temperature: value.temperature,
            max_retries: value.max_retries,
            initial_retry_ms: value.initial_retry_ms,
            max_retry_ms: value.max_retry_ms,
        }
    }
}

fn default_workspace() -> PathBuf {
    ".".into()
}

fn default_state_path() -> PathBuf {
    ".snippet/state.json".into()
}

fn default_provider() -> String {
    "openai-compatible".to_string()
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_max_retries() -> u32 {
    4
}

fn default_initial_retry_ms() -> u64 {
    750
}

fn default_max_retry_ms() -> u64 {
    8_000
}

fn default_context_window() -> u64 {
    128_000
}

fn default_cache_prompt() -> bool {
    true
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            api_key: String::new(),
            base_url: default_base_url(),
            model: "gpt-4o".to_string(),
            temperature: None,
            max_retries: default_max_retries(),
            initial_retry_ms: default_initial_retry_ms(),
            max_retry_ms: default_max_retry_ms(),
            context_window: default_context_window(),
            cache_prompt: default_cache_prompt(),
        }
    }
}
