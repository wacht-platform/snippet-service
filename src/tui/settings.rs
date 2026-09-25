use std::path::PathBuf;
use std::time::Duration;

use crate::config::SnippetConfig;
use crate::harness::LoopInput;
use super::app::*;
use super::*;

/// Providers offered by the login form, in display order.
pub(crate) const LOGIN_PROVIDERS: &[&str] = &[
    "openai",
    "chatgpt",
    "xai",
    "anthropic",
    "gemini",
    "openrouter",
    "opencode-zen",
    "opencode-go",
    "openai-compatible",
    "anthropic-compatible",
];



/// Providers that talk to a user-supplied endpoint, so the login form shows the
/// Base URL field (and can fetch models keyless).
pub(crate) fn provider_needs_base_url(provider: &str) -> bool {
    provider == "openai-compatible" || provider == "anthropic-compatible"
}

/// Single source of truth for a provider's default base URL and model.
pub(crate) fn provider_defaults(provider: &str) -> (String, String) {
    match provider {
        "openai" => (
            "https://api.openai.com/v1".to_string(),
            "gpt-5.5".to_string(),
        ),
        // ChatGPT-subscription (OAuth) — no base URL / API key; model is a Codex slug.
        "chatgpt" => (String::new(), "gpt-5.1-codex".to_string()),
        "anthropic" => (String::new(), "claude-opus-4-8".to_string()),
        // xAI (Grok/X subscription) — OAuth via `snippet xai login`; no base URL/key.
        "xai" => (String::new(), "grok-4".to_string()),
        // Anthropic-Messages-compatible gateway — user supplies base_url + model.
        "anthropic-compatible" => (String::new(), String::new()),
        // Native Gemini adapter — no base URL (it has its own endpoint), like Anthropic.
        "gemini" => (String::new(), "gemini-3.5-flash".to_string()),
        "openrouter" => (
            "https://openrouter.ai/api/v1".to_string(),
            "anthropic/claude-opus-4-8".to_string(),
        ),
        "opencode-zen" => (
            "https://opencode.ai/zen/v1".to_string(),
            "deepseek-v4-flash".to_string(),
        ),
        "opencode-go" => (
            "https://opencode.ai/zen/go/v1".to_string(),
            "kimi-k2.6".to_string(),
        ),
        // openai-compatible: no sensible model default — the user picks from the
        // endpoint's fetched list or types one.
        _ => ("http://localhost:11434/v1".to_string(), String::new()),
    }
}

pub(crate) fn provider_context_defaults(provider: &str) -> (u64, u8) {
    match provider {
        "openai" | "chatgpt" | "anthropic" | "gemini" | "xai" => (250_000, 90),
        "opencode-zen" | "opencode-go" => (250_000, 90),
        "openai-compatible" | "anthropic-compatible" => (130_000, 90),
        // Keep openrouter aligned with the hosted-provider defaults unless the
        // user overrides it per profile.
        "openrouter" => (250_000, 90),
        _ => (130_000, 90),
    }
}

/// The model candidates shown inline in the login form. The list is filtered by
/// the current model text, with prefix matches ranked first and the active value
/// pinned into the results so arrowing through suggestions stays stable.
pub(crate) fn login_model_rows(app: &App) -> Vec<String> {
    let all: Vec<String> = match &app.form_fetched_models {
        Some(fetched) => fetched.clone(),
        None => get_provider_models(&app.form_provider)
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let query = app.form_model.trim().to_ascii_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for model in all {
        let lower = model.to_ascii_lowercase();
        let matches = query.is_empty() || lower.starts_with(&query) || lower.contains(&query);
        if !matches || !seen.insert(model.clone()) {
            continue;
        }
        if !query.is_empty() && lower.starts_with(&query) {
            prefix.push(model);
        } else {
            contains.push(model);
        }
    }

    let current = app.form_model.trim();
    if !current.is_empty() && seen.insert(current.to_string()) {
        prefix.insert(0, current.to_string());
    }

    prefix.extend(contains);
    prefix.truncate(6);
    prefix
}


impl App {
    pub(crate) fn close_login(&mut self, restore: bool) {
        if restore {
            if let Some(orig) = self.original_config.take() {
                self.options.config = orig;
            }
        } else {
            self.original_config = None;
        }
        self.login_active = false;
        // When opened from the profiles screen, return there (refreshed) rather than
        // dropping to the transcript.
        if self.return_to_profiles {
            self.return_to_profiles = false;
            self.editing_profile = None;
            self.open_profiles();
        }
        // When closing login during an active session, the conversation is preserved.
    }


    /// Tab order of the login form fields (Base URL only for openai-compatible).
    pub(crate) fn login_focus_order(&self) -> Vec<SettingsField> {
        // Subscription providers sign in via OAuth — no API key / base URL fields.
        if self.form_provider == "chatgpt" || self.form_provider == "xai" {
            let mut order = vec![
                SettingsField::Provider,
                SettingsField::Model,
                SettingsField::Reasoning,
                SettingsField::ContextWindow,
                SettingsField::Compaction,
            ];
            if self.form_provider == "xai" {
                order.push(SettingsField::XSearch);
            }
            return order;
        }
        let mut order = vec![SettingsField::Provider, SettingsField::ApiKey];
        if provider_needs_base_url(&self.form_provider) {
            order.push(SettingsField::BaseUrl);
        }
        order.push(SettingsField::Model);
        order.push(SettingsField::Reasoning);
        order.push(SettingsField::ContextWindow);
        order.push(SettingsField::Compaction);
        order
    }

    /// Move focus between login fields. Lazily fetches the model list the first
    /// time focus lands on the Model field with a key present.
    pub(crate) fn login_move_focus(&mut self, forward: bool) {
        let order = self.login_focus_order();
        let cur = order
            .iter()
            .position(|f| *f == self.form_focus)
            .unwrap_or(0);
        let next = if forward {
            (cur + 1) % order.len()
        } else if cur == 0 {
            order.len() - 1
        } else {
            cur - 1
        };
        self.form_focus = order[next];

        // Keyless endpoints are legitimate for openai-compatible (local Ollama,
        // LM Studio…): fetch on a base_url alone there; other providers need a key.
        let can_fetch = !self.form_api_key.trim().is_empty()
            || (provider_needs_base_url(&self.form_provider) && !self.form_base_url.trim().is_empty())
            // xAI has no key/base URL — fetch once signed in via the subscription.
            || (self.form_provider == "xai" && crate::xai_auth::is_signed_in());
        if self.form_focus == SettingsField::Model
            && self.form_fetched_models.is_none()
            && can_fetch
        {
            self.trigger_models_fetch();
        }
    }

    /// `←`/`→` on the focused field: cycle provider or model.
    pub(crate) fn login_adjust(&mut self, forward: bool) {
        match self.form_focus {
            SettingsField::Provider => self.change_provider(forward),
            SettingsField::Model => self.login_cycle_model(forward),
            SettingsField::Reasoning => self.login_cycle_reasoning(forward),
            SettingsField::Compaction => self.login_cycle_compaction_pct(forward),
            SettingsField::XSearch => self.form_x_search = !self.form_x_search,
            _ => {}
        }
    }

    pub(crate) fn login_cycle_reasoning(&mut self, forward: bool) {
        pub(crate) const OPTIONS: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
        let current = self
            .form_reasoning_effort
            .as_deref()
            .unwrap_or("medium")
            .to_ascii_lowercase();
        let idx = OPTIONS.iter().position(|v| *v == current).unwrap_or(2);
        let next = if forward {
            (idx + 1) % OPTIONS.len()
        } else if idx == 0 {
            OPTIONS.len() - 1
        } else {
            idx - 1
        };
        self.form_reasoning_effort = Some(OPTIONS[next].to_string());
    }

    pub(crate) fn login_cycle_compaction_pct(&mut self, forward: bool) {
        let current = self
            .form_compact_at_pct
            .trim()
            .parse::<u8>()
            .ok()
            .unwrap_or(85)
            .clamp(50, 95);
        let next = if forward {
            current.saturating_add(5).min(95)
        } else {
            current.saturating_sub(5).max(50)
        };
        self.form_compact_at_pct = next.to_string();
    }

    /// Cycle the chosen model through the full candidate list (fetched or
    /// fallback), independent of any typed text.
    pub(crate) fn login_cycle_model(&mut self, forward: bool) {
        let all: Vec<String> = match &self.form_fetched_models {
            Some(fetched) => fetched.clone(),
            None => get_provider_models(&self.form_provider)
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        if all.is_empty() {
            return;
        }
        let next = match all.iter().position(|m| *m == self.form_model) {
            Some(i) if forward => (i + 1) % all.len(),
            Some(0) => all.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.form_model = all[next].clone();
        self.model_picker_index = 0;
    }

    /// Type a character into the focused text field.
    pub(crate) fn login_edit_char(&mut self, c: char) {
        match self.form_focus {
            SettingsField::ApiKey => self.form_api_key.push(c),
            SettingsField::BaseUrl => self.form_base_url.push(c),
            SettingsField::Model => {
                self.form_model.push(c);
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => {
                if c.is_ascii_digit() {
                    self.form_context_window.push(c);
                }
            }
            SettingsField::Compaction => {
                if c.is_ascii_digit() {
                    self.form_compact_at_pct.push(c);
                }
            }
            _ => {}
        }
    }

    /// Paste into the focused login-form field. Fields are single-line, so pasted
    /// newlines and edge whitespace are stripped (a pasted key/model/URL often has
    /// a trailing newline).
    pub(crate) fn login_paste(&mut self, text: &str) {
        let cleaned = text.replace(['\n', '\r'], "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            return;
        }
        match self.form_focus {
            SettingsField::ApiKey => self.form_api_key.push_str(cleaned),
            SettingsField::BaseUrl => self.form_base_url.push_str(cleaned),
            SettingsField::Model => {
                self.form_model.push_str(cleaned);
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => self.form_context_window.push_str(
                &cleaned
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
            SettingsField::Compaction => self.form_compact_at_pct.push_str(
                &cleaned
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>(),
            ),
            _ => {}
        }
    }

    pub(crate) fn login_backspace(&mut self) {
        match self.form_focus {
            SettingsField::ApiKey => {
                self.form_api_key.pop();
            }
            SettingsField::BaseUrl => {
                self.form_base_url.pop();
            }
            SettingsField::Model => {
                self.form_model.pop();
                self.model_picker_index = 0;
            }
            SettingsField::ContextWindow => {
                self.form_context_window.pop();
            }
            SettingsField::Compaction => {
                self.form_compact_at_pct.pop();
            }
            _ => {}
        }
    }

    /// Validate the form and connect: persist the config and close the form.
    pub(crate) fn login_connect(&mut self) {
        // ChatGPT-subscription has no API key — it signs in via OAuth (or reuses an
        // existing sign-in) instead of validating a key.
        if self.form_provider == "chatgpt" {
            self.start_chatgpt_login(crate::chatgpt_auth::ChatGptLoginMethod::Browser);
            return;
        }
        // xAI subscription — device-code sign-in, no API key.
        if self.form_provider == "xai" {
            self.start_xai_login();
            return;
        }
        if self.form_api_key.trim().is_empty() {
            self.form_focus = SettingsField::ApiKey;
            self.status = "An API key is required to connect.".to_string();
            return;
        }
        if self.form_model.trim().is_empty() {
            self.form_focus = SettingsField::Model;
            self.status = "Pick or type a model to connect.".to_string();
            return;
        }
        let context_window = self
            .form_context_window
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|v| *v >= 8_000)
            .unwrap_or_else(|| provider_context_defaults(&self.form_provider).0);
        let compact_at_pct = self
            .form_compact_at_pct
            .trim()
            .parse::<u8>()
            .ok()
            .unwrap_or_else(|| provider_context_defaults(&self.form_provider).1)
            .clamp(50, 95);
        match self.save_settings_to_file_with_limits(context_window, compact_at_pct) {
            Ok(_) => {
                self.close_login(false);
                // The resident loop builds its model once at spawn, so a live
                // (idle) loop won't pick up the change until it's restarted; it
                // resumes the persisted conversation, so nothing is lost.
                let resumed = self.restart_loop_for_config();
                self.status = format!(
                    "✓ Connected — {} · {}{}",
                    self.options.config.model.provider,
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Begin a ChatGPT sign-in flow. Reuses an existing sign-in if there
    /// is one; otherwise starts either the browser OAuth flow or the device-code flow.
    pub(crate) fn start_chatgpt_login(&mut self, method: crate::chatgpt_auth::ChatGptLoginMethod) {
        if self.chatgpt_login_handle.is_some() {
            self.status = match method {
                crate::chatgpt_auth::ChatGptLoginMethod::Browser => {
                    "Sign-in already in progress — finish it in your browser.".to_string()
                }
                crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode => {
                    "Sign-in already in progress — finish the device-code flow first.".to_string()
                }
            };
            return;
        }
        if crate::chatgpt_auth::is_signed_in() {
            self.finish_chatgpt_login(None);
            return;
        }
        self.chatgpt_device_code = None;
        match method {
            crate::chatgpt_auth::ChatGptLoginMethod::Browser => {
                self.status = "Opening browser — finish signing in to ChatGPT…".to_string();
                self.chatgpt_login_handle = Some(tokio::spawn(async move {
                    crate::chatgpt_auth::login(crate::chatgpt_auth::ChatGptLoginMethod::Browser)
                        .await
                }));
            }
            crate::chatgpt_auth::ChatGptLoginMethod::DeviceCode => {
                // Fetch the device code off-thread (NEVER block_on inside the async
                // runtime — that panics). tick() surfaces the code and starts polling.
                self.status = "Starting device-code sign-in…".to_string();
                self.chatgpt_device_begin_handle = Some(tokio::spawn(async move {
                    crate::chatgpt_auth::begin_device_code_login().await
                }));
            }
        }
    }

    /// Persist the chatgpt provider + model once a sign-in is in place and connect.
    pub(crate) fn finish_chatgpt_login(&mut self, email: Option<String>) {
        if self.form_model.trim().is_empty() {
            self.form_model = "gpt-5.1-codex".to_string();
        }
        self.form_api_key = String::new();
        self.form_base_url = String::new();
        self.chatgpt_device_code = None;
        match self.save_settings_to_file() {
            Ok(_) => {
                self.close_login(false);
                let resumed = self.restart_loop_for_config();
                let who = email.map(|e| format!(" as {e}")).unwrap_or_default();
                self.status = format!(
                    "✓ Signed in to ChatGPT{who} — {}{}",
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Begin xAI (Grok/X subscription) sign-in — device-code only. Reuses an
    /// existing sign-in; otherwise fetches a code (shown by `tick`) and polls.
    pub(crate) fn start_xai_login(&mut self) {
        if self.xai_login_handle.is_some() || self.xai_device_begin_handle.is_some() {
            self.status =
                "xAI sign-in already in progress — finish the device-code flow first.".to_string();
            return;
        }
        if crate::xai_auth::is_signed_in() {
            self.finish_xai_login();
            return;
        }
        self.xai_device_code = None;
        self.status = "Starting xAI device-code sign-in…".to_string();
        self.xai_device_begin_handle = Some(tokio::spawn(async move {
            crate::xai_auth::begin_device_code_login().await
        }));
    }

    /// Persist the xai provider + model once signed in and connect.
    pub(crate) fn finish_xai_login(&mut self) {
        if self.form_model.trim().is_empty() {
            self.form_model = "grok-4".to_string();
        }
        self.form_api_key = String::new();
        self.form_base_url = String::new();
        self.xai_device_code = None;
        match self.save_settings_to_file() {
            Ok(_) => {
                self.close_login(false);
                let resumed = self.restart_loop_for_config();
                self.status = format!(
                    "✓ Signed in to xAI — {}{}",
                    self.options.config.model.model,
                    if resumed { " · resumed" } else { "" },
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub(crate) fn logout_xai(&mut self) {
        match crate::xai_auth::logout_blocking() {
            Ok(()) => {
                self.xai_device_code = None;
                self.status = "Signed out of xAI".to_string();
            }
            Err(e) => self.status = format!("xAI sign-out failed: {e}"),
        }
    }

    pub(crate) fn logout_chatgpt(&mut self) {
        match crate::chatgpt_auth::logout_blocking() {
            Ok(()) => {
                self.chatgpt_device_code = None;
                if self.form_provider == "chatgpt" {
                    self.form_api_key.clear();
                    self.form_base_url.clear();
                }
                if self.options.config.model.provider == "chatgpt" {
                    let name = self.options.config.active_setup.clone();
                    if let Some(name) = name {
                        if let Some(setups) = self.options.config.setups.as_mut() {
                            if let Some(cfg) = setups.get_mut(&name) {
                                cfg.api_key.clear();
                            }
                        }
                    }
                    let _ = self.save_config_file();
                }
                self.status = "Signed out of ChatGPT.".to_string();
            }
            Err(error) => {
                self.error = Some(format!("ChatGPT logout failed: {error}"));
            }
        }
    }

    /// Apply a config change to the resident loop by restarting it. A live loop is
    /// aborted (it's idle, waiting for input — guards ensure it isn't mid-turn) and
    /// respawned with `resume`, continuing the conversation with the new model.
    /// Returns `true` if a loop was actually restarted. No-op when none is running —
    /// the next `spawn_loop` already uses the new config.
    pub(crate) fn restart_loop_for_config(&mut self) -> bool {
        if !self.agent_alive() {
            return false;
        }
        if let Some(handle) = self.agent.take() {
            handle.abort();
        }
        self.input_tx = None;
        // In sidecar mode the daemon rebuilds the model via /session/model or
        // config watch; drop our attach and re-open so we pick up the new loop.
        self.sidecar_attach = None;
        self.pending_sidecar_attach = None;
        self.spawn_loop(None, true);
        true
    }

    pub(crate) fn init_settings_form(&mut self) {
        self.form_provider = self.options.config.model.provider.clone();
        self.form_api_key = self.options.config.model.api_key.clone();
        self.form_model = self.options.config.model.model.clone();
        self.form_model_query = String::new();
        self.form_base_url = self.options.config.model.base_url.clone();
        self.form_reasoning_effort = self
            .options
            .config
            .model
            .reasoning_effort
            .clone()
            .or(Some("medium".to_string()));
        self.form_context_window = self.options.config.model.context_window.to_string();
        self.form_compact_at_pct = self.options.config.model.compact_at_pct.to_string();
        self.form_x_search = self.options.config.model.x_search;
        self.form_focus = SettingsField::Provider;
        self.form_fetched_models = None;
        self.models_fetch_status = String::new();
    }

    pub(crate) fn trigger_models_fetch(&mut self) {
        if let Some(ref handle) = self.models_fetch_handle {
            handle.abort();
        }
        self.form_fetched_models = None;
        self.models_fetch_status = "Fetching available models from provider...".to_string();

        let provider = self.form_provider.clone();
        let api_key = self.form_api_key.clone();
        let base_url = self.form_base_url.clone();

        self.models_fetch_handle = Some(tokio::spawn(async move {
            fetch_models_from_provider(provider, api_key, base_url).await
        }));
    }

    pub(crate) fn save_config_file(&self) -> Result<(), String> {
        let toml_str = toml::to_string_pretty(&self.options.config)
            .map_err(|e| format!("failed to serialize config: {e}"))?;

        // Never overwrite the config with something we can't read back — guards
        // against a serialization ordering bug silently corrupting the user's file.
        toml::from_str::<crate::config::SnippetConfig>(&toml_str)
            .map_err(|e| format!("refusing to write config that won't round-trip: {e}"))?;

        std::fs::write(&self.options.config_path, toml_str)
            .map_err(|e| format!("failed to write config: {e}"))?;
        crate::config::set_private(&self.options.config_path);
        Ok(())
    }

    pub(crate) fn save_settings_to_file(&mut self) -> Result<(), String> {
        let context_window = provider_context_defaults(&self.form_provider).0;
        let compact_at_pct = provider_context_defaults(&self.form_provider).1;
        self.save_settings_to_file_with_limits(context_window, compact_at_pct)
    }

    pub(crate) fn save_settings_to_file_with_limits(
        &mut self,
        context_window: u64,
        compact_at_pct: u8,
    ) -> Result<(), String> {
        // Start from the profile being edited (preserving its other fields like
        // temperature/reasoning) or the active config when adding a new one.
        let mut model_config = self
            .editing_profile
            .as_ref()
            .and_then(|n| {
                self.options
                    .config
                    .setups
                    .as_ref()
                    .and_then(|m| m.get(n))
                    .cloned()
            })
            .unwrap_or_else(|| self.options.config.model.clone());

        model_config.provider = self.form_provider.clone();
        model_config.api_key = self.form_api_key.clone();
        model_config.model = self.form_model.clone();
        model_config.base_url = self.form_base_url.clone();
        model_config.reasoning_effort = self
            .form_reasoning_effort
            .clone()
            .filter(|v| !v.trim().is_empty());
        model_config.x_search = self.form_provider == "xai" && self.form_x_search;

        model_config.context_window = context_window;
        model_config.compact_at_pct = compact_at_pct;

        // Write into the named profile (editing the same key, or a fresh unique one)
        // and make it active.
        let key = self
            .editing_profile
            .clone()
            .unwrap_or_else(|| self.options.config.unique_profile_key(&self.form_provider));
        self.options.config.upsert_profile(&key, model_config);
        self.options.config.activate(&key);
        self.editing_profile = Some(key);

        self.save_config_file()?;

        self.status = String::new();
        Ok(())
    }

    pub(crate) fn change_provider(&mut self, next: bool) {
        let current_idx = LOGIN_PROVIDERS
            .iter()
            .position(|p| *p == self.form_provider)
            .unwrap_or(0);
        let next_idx = if next {
            (current_idx + 1) % LOGIN_PROVIDERS.len()
        } else if current_idx == 0 {
            LOGIN_PROVIDERS.len() - 1
        } else {
            current_idx - 1
        };
        self.form_provider = LOGIN_PROVIDERS[next_idx].to_string();

        let (base_url, model) = provider_defaults(&self.form_provider);
        self.form_base_url = base_url;
        self.form_model = model;
        let (context_window, compact_at_pct) = provider_context_defaults(&self.form_provider);
        self.form_context_window = context_window.to_string();
        self.form_compact_at_pct = compact_at_pct.to_string();
        // Keep the user's current reasoning preference if present; otherwise default to medium.
        if self.form_reasoning_effort.is_none() {
            self.form_reasoning_effort = Some("medium".to_string());
        }
        self.form_model_query = String::new();
        // The previous provider's model list no longer applies.
        self.form_fetched_models = None;
    }


}

pub(crate) fn get_provider_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &[
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3",
            "gpt-5.3-codex",
            "gpt-4o",
        ],
        "anthropic" => &[
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "claude-fable-5",
        ],
        "gemini" => &[
            "gemini-3.5-flash",
            "gemini-3.1-pro",
            "gemini-3.1-flash-lite",
            "gemini-3-pro",
        ],
        "openrouter" => &[
            "anthropic/claude-opus-4-8",
            "anthropic/claude-sonnet-4-6",
            "google/gemini-3.1-pro",
            "deepseek/deepseek-v4-pro",
            "qwen/qwen3-coder",
            "openai/gpt-5.5",
        ],
        "opencode-zen" => &[
            "gpt-5.5",
            "claude-opus-4-8",
            "gemini-3.5-flash",
            "grok-4.6",
            "deepseek-v4-flash",
        ],
        "opencode-go" => &[
            "kimi-k2.6",
            "grok-4.6",
            "glm-5.3",
            "qwen3.8-flash",
            "deepseek-v4-flash",
        ],
        // openai-compatible points at an arbitrary endpoint — there are no
        // sensible static suggestions; the model list is fetched from its
        // /models endpoint (or typed by the user).
        // ChatGPT-subscription Codex backend (OAuth, no key). The 5.6 family
        // requires the codex client-identity headers (see chatgpt.rs).
        "chatgpt" => &[
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.1-codex",
            "gpt-5.4-mini",
        ],
        "openai-compatible" => &[],
        _ => &[],
    }
}

pub(crate) async fn fetch_models_from_provider(
    provider: String,
    api_key: String,
    base_url: String,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    match provider.as_str() {
        "openai" => {
            let url = "https://api.openai.com/v1/models";
            let res = client
                .get(url)
                .bearer_auth(api_key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "anthropic" | "anthropic-compatible" => {
            let raw = if provider == "anthropic-compatible" && !base_url.trim().is_empty() {
                base_url.trim().trim_end_matches('/').to_string()
            } else {
                "https://api.anthropic.com".to_string()
            };
            // Tolerate a base that already includes /v1 (matches the messages URL logic).
            let url = if raw.ends_with("/v1") {
                format!("{raw}/models")
            } else {
                format!("{raw}/v1/models")
            };
            let res = client
                .get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "gemini" => {
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models?key={}",
                api_key
            );
            let res = client.get(&url).send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("models").and_then(|m| m.as_array()) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        let stripped = name.strip_prefix("models/").unwrap_or(name);
                        models.push(stripped.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "openrouter" => {
            let url = "https://openrouter.ai/api/v1/models";
            let mut req = client.get(url);
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key);
            }
            let res = req.send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "xai" => {
            let token = crate::xai_auth::access_token()
                .await
                .map_err(|e| format!("sign in to xAI first: {e}"))?;
            let res = client
                .get("https://api.x.ai/v1/models")
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        "opencode-zen" | "opencode-go" | "openai-compatible" => {
            let mut url = if provider == "opencode-zen" {
                "https://opencode.ai/zen/v1".to_string()
            } else if provider == "opencode-go" {
                "https://opencode.ai/zen/go/v1".to_string()
            } else {
                base_url
            };
            if !url.ends_with("/models") {
                if url.ends_with('/') {
                    url.push_str("models");
                } else {
                    url.push_str("/models");
                }
            }
            let mut req = client.get(&url);
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key);
            }
            let res = req.send().await.map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("HTTP status {}", res.status()));
            }
            let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
            let mut models = Vec::new();
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    }
                }
            } else if let Some(arr) = data.get("models").and_then(|m| m.as_array()) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        models.push(name.to_string());
                    }
                }
            } else if let Some(arr) = data.as_array() {
                for item in arr {
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        models.push(id.to_string());
                    } else if let Some(s) = item.as_str() {
                        models.push(s.to_string());
                    }
                }
            }
            models.sort();
            if models.is_empty() {
                return Err("No models found".to_string());
            }
            Ok(models)
        }
        _ => Err(format!("Unsupported provider: {}", provider)),
    }
}

