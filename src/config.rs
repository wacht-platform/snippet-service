use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-workspace state path: `~/.snippet/workspaces/{name}-{hash}/state.json`.
/// Single source of truth, used by the per-launch config and the serve daemon.
pub fn state_path_for_workspace(workspace: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace.hash(&mut hasher);
    let hash = hasher.finish();
    let name = workspace.file_name().and_then(|n| n.to_str()).unwrap_or("workspace");
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(format!(".snippet/workspaces/{name}-{hash:x}/state.json"))
}

/// Root holding every workspace's session state.
pub fn workspaces_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".snippet").join("workspaces")
}

/// Restrict a file to owner-only (0600) on Unix; no-op elsewhere.
pub fn set_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

use crate::llm::AgentModel;
use crate::openai::{OpenAiCompatibleModel, OpenAiCompatibleConfig};
use crate::anthropic::{AnthropicModel, AnthropicConfig};
use crate::gemini::{GeminiModel, GeminiConfig};
use crate::tools::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetConfig {
    // Workspace + state are per-launch (derived from the cwd), never read from or
    // written to the global config — so snippet operates on whatever folder you
    // launch it in. `skip` keeps them out of config.toml entirely.
    #[serde(skip)]
    pub workspace: PathBuf,
    #[serde(skip)]
    pub state_path: PathBuf,
    #[serde(default)]
    pub resume_on_start: bool,
    /// Start runs in manual approval mode — bash and file edits wait for y/n.
    #[serde(default)]
    pub manual_approval: bool,
    /// Name of the active profile in `setups`. The active profile's config is
    /// mirrored into `model` for the runtime.
    #[serde(default, alias = "active_profile", alias = "active_setup", skip_serializing_if = "Option::is_none")]
    pub active_setup: Option<String>,
    /// Exa API key for the `web_search` / `web_read` tools. When set, web search is
    /// enabled; absent, the tools aren't offered to the model. Declared before the
    /// `model` table so it serializes as a top-level key (TOML requires scalars
    /// before any table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exa_api_key: Option<String>,
    /// TUI color theme name (e.g. "midnight", "light", "high-contrast", "ember").
    /// Unset = default. Declared before `model` so it stays a top-level key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Per-workspace memory: the agent accumulates durable facts/playbooks per
    /// folder under `<ws>/.snippet/memory/`, loaded into context each session and
    /// refreshed by a reflection pass during compaction. These are scalars, so
    /// they're declared before the `setups`/`model` tables (TOML ordering rule).
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    #[serde(default = "default_memory_index_budget_chars")]
    pub memory_index_budget_chars: usize,
    #[serde(default = "default_memory_entry_budget_chars")]
    pub memory_entry_budget_chars: usize,
    #[serde(default = "default_memory_max_entries")]
    pub memory_max_entries: usize,
    #[serde(default = "default_memory_reflect_on_compaction")]
    pub memory_reflect_on_compaction: bool,
    /// Saved provider profiles, keyed by name. Multiple can be configured; one is
    /// active (`active_setup`). Declared after the scalar keys so the emitted TOML
    /// stays valid (tables must follow top-level scalars).
    #[serde(default, alias = "profiles", alias = "setups", skip_serializing_if = "Option::is_none")]
    pub setups: Option<BTreeMap<String, ModelConfig>>,
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
    /// Override the User-Agent for OpenAI-compatible endpoints. Leave unset to
    /// use the default coding-agent UA (needed for Kimi For Coding and similar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Whether the model accepts image inputs. OFF by default (safe): when off,
    /// images read by `read_image` are passed as a text placeholder instead of
    /// inlined bytes, so text-only models don't 400. Set true for multimodal models.
    #[serde(default)]
    pub supports_images: bool,
    /// Reasoning effort: "low" | "medium" | "high" | "off" (or unset). Mapped per
    /// provider (OpenAI reasoning_effort, Gemini thinkingConfig, Anthropic thinking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Model context window in tokens, for the status-bar usage gauge and
    /// compaction thresholds.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Start compaction when the largest observed prompt reaches this percentage
    /// of the configured context window.
    #[serde(default = "default_compact_at_pct")]
    pub compact_at_pct: u8,
    #[serde(default = "default_cache_prompt")]
    pub cache_prompt: bool,
}

impl Default for SnippetConfig {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            state_path: default_state_path(),
            resume_on_start: false,
            manual_approval: false,
            active_setup: None,
            setups: None,
            model: ModelConfig::default(),
            exa_api_key: None,
            theme: None,
            memory_enabled: default_memory_enabled(),
            memory_index_budget_chars: default_memory_index_budget_chars(),
            memory_entry_budget_chars: default_memory_entry_budget_chars(),
            memory_max_entries: default_memory_max_entries(),
            memory_reflect_on_compaction: default_memory_reflect_on_compaction(),
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
                set_private(path);
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
            "openai-compatible" | "openai" | "anthropic" | "gemini" | "openrouter" | "chatgpt" => {}
            other => {
                return Err(ToolError::msg(format!(
                    "unsupported model.provider `{}`; expected one of `openai-compatible`, `openai`, `anthropic`, `gemini`, `openrouter`, `chatgpt`",
                    other
                )));
            }
        }
        config.resolve_relative_paths(path);
        Ok(config)
    }

    /// Pin the workspace to the current working directory (where snippet was
    /// launched) and derive a per-workspace state location, so each folder gets
    /// its own scoped conversation history and snippet never operates on a stale
    /// pinned path.
    fn resolve_relative_paths(&mut self, _config_path: &Path) {
        if let Ok(cwd) = std::env::current_dir() {
            self.workspace = cwd;
        } else if self.workspace.as_os_str().is_empty() {
            self.workspace = PathBuf::from(".");
        }
        self.state_path = state_path_for_workspace(&self.workspace);
    }

    /// A copy of this config pinned to a different workspace folder — keeps the
    /// provider/model/keys but recomputes the workspace + state path. The serve
    /// daemon uses this to open a session in any folder the user picks.
    pub fn for_workspace(&self, workspace: PathBuf) -> SnippetConfig {
        let mut c = self.clone();
        c.state_path = state_path_for_workspace(&workspace);
        c.workspace = workspace;
        c
    }
}

impl SnippetConfig {
    /// Profile names in stable (sorted) order.
    pub fn profile_names(&self) -> Vec<String> {
        self.setups
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Migrate a lone `[model]` into a named profile if none exist, so the rest of
    /// the UI can treat every config as a profile.
    pub fn ensure_setups(&mut self) {
        if self.setups.as_ref().map(|m| m.is_empty()).unwrap_or(true) {
            let key = if self.model.provider.is_empty() {
                "default".to_string()
            } else {
                self.model.provider.clone()
            };
            let mut map = BTreeMap::new();
            map.insert(key.clone(), self.model.clone());
            self.setups = Some(map);
            self.active_setup = Some(key);
        }
    }

    /// A unique profile key derived from a provider name (`provider`, `provider-2`, …).
    pub fn unique_profile_key(&self, provider: &str) -> String {
        let exists = |k: &str| self.setups.as_ref().map(|m| m.contains_key(k)).unwrap_or(false);
        if !exists(provider) {
            return provider.to_string();
        }
        let mut n = 2;
        loop {
            let k = format!("{provider}-{n}");
            if !exists(&k) {
                return k;
            }
            n += 1;
        }
    }

    /// Insert or replace a profile; mirror it into `model` when it's the active one.
    pub fn upsert_profile(&mut self, name: &str, cfg: ModelConfig) {
        let map = self.setups.get_or_insert_with(BTreeMap::new);
        map.insert(name.to_string(), cfg.clone());
        if self.active_setup.as_deref() == Some(name) || self.active_setup.is_none() {
            self.active_setup = Some(name.to_string());
            self.model = cfg;
        }
    }

    /// Activate a profile, mirroring its config into `model`. False if not found.
    pub fn activate(&mut self, name: &str) -> bool {
        if let Some(cfg) = self.setups.as_ref().and_then(|m| m.get(name)).cloned() {
            self.active_setup = Some(name.to_string());
            self.model = cfg;
            true
        } else {
            false
        }
    }

    /// Remove a profile; if it was active, fall back to the first remaining one.
    pub fn remove_profile(&mut self, name: &str) {
        if let Some(map) = self.setups.as_mut() {
            map.remove(name);
        }
        if self.active_setup.as_deref() == Some(name) {
            let next = self.setups.as_ref().and_then(|m| m.keys().next().cloned());
            match next {
                Some(k) => {
                    self.activate(&k);
                }
                None => self.active_setup = None,
            }
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
                let mut model = self.model.clone();
                if model.is_empty() {
                    // Latest stable Flash model (confirmed against ai.google.dev/models).
                    model = "gemini-3.5-flash".to_string();
                }
                Box::new(GeminiModel::new(GeminiConfig {
                    api_key: self.api_key.clone(),
                    model,
                    temperature: self.temperature,
                    max_retries: self.max_retries,
                    initial_retry_ms: self.initial_retry_ms,
                    max_retry_ms: self.max_retry_ms,
                    supports_images: true, // Gemini models are multimodal
                    reasoning_effort: self.reasoning_effort.clone(),
                }))
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
                    supports_images: self.supports_images,
                    reasoning_effort: self.reasoning_effort.clone(),
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
            "chatgpt" => {
                let mut model = self.model.clone();
                if model.is_empty() {
                    model = "gpt-5.1-codex".to_string();
                }
                Box::new(crate::chatgpt::ChatGptModel::new(crate::chatgpt::ChatGptConfig {
                    model,
                    reasoning_effort: self.reasoning_effort.clone(),
                    supports_images: true, // ChatGPT/Codex models are multimodal
                    max_retries: self.max_retries,
                    initial_retry_ms: self.initial_retry_ms,
                    max_retry_ms: self.max_retry_ms,
                }))
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
            user_agent: value.user_agent,
            supports_images: value.supports_images,
            reasoning_effort: value.reasoning_effort,
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

fn default_compact_at_pct() -> u8 {
    90
}

fn default_cache_prompt() -> bool {
    true
}

fn default_memory_enabled() -> bool {
    true
}

fn default_memory_index_budget_chars() -> usize {
    5_000
}

fn default_memory_entry_budget_chars() -> usize {
    12_000
}

fn default_memory_max_entries() -> usize {
    128
}

fn default_memory_reflect_on_compaction() -> bool {
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
            user_agent: None,
            supports_images: false,
            reasoning_effort: None,
            context_window: default_context_window(),
            compact_at_pct: default_compact_at_pct(),
            cache_prompt: default_cache_prompt(),
        }
    }
}


/// Write `config` to `path` with a round-trip parse guard (refuse to write a config
/// that won't load back), then chmod it 0600. Shared by the TUI-style save path and
/// the serve daemon's config endpoints.
pub fn save_config(config: &SnippetConfig, path: &Path) -> Result<(), String> {
    let toml_str = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    toml::from_str::<SnippetConfig>(&toml_str)
        .map_err(|e| format!("refusing to write config that won't round-trip: {e}"))?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, toml_str).map_err(|e| format!("write {}: {e}", path.display()))?;
    set_private(path);
    Ok(())
}
