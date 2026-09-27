use super::*;

#[derive(Deserialize)]
pub(crate) struct ProfileReq {
    pub(crate) name: Option<String>,
    pub(crate) provider: String,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) supports_images: Option<bool>,
    #[serde(default)]
    pub(crate) context_window: Option<u64>,
    #[serde(default)]
    pub(crate) stream: Option<bool>,
    #[serde(default)]
    pub(crate) x_search: Option<bool>,
    #[serde(default)]
    pub(crate) set_active: bool,
}


// PUT /config/profile — add/update an API-key provider profile; persists to disk.
// An omitted/blank api_key keeps any existing key (so editing doesn't wipe it).
pub(crate) async fn put_profile(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ProfileReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.provider.trim().is_empty() || req.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "provider and model are required").into_response();
    }
    // Reject providers `SnippetConfig::load` won't accept — persisting one works
    // in-memory but bricks the next daemon/TUI startup on the config re-parse.
    if !crate::config::provider_supported(&req.provider) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported provider `{}`; expected one of {}",
                req.provider,
                crate::config::SUPPORTED_PROVIDERS.join(", ")
            ),
        )
            .into_response();
    }
    d.reload_config().await; // modify the current on-disk config, not a stale copy
    let result = {
        let mut c = d.config.lock().unwrap();
        let name = req
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| c.unique_profile_key(&req.provider));
        // Start from the existing profile so an edit only changes what the
        // request states — rebuilding from defaults silently wiped hand-tuned
        // fields (user_agent, temperature, retries, cache_prompt, …).
        let mut mc = c
            .setups
            .as_ref()
            .and_then(|m| m.get(&name))
            .cloned()
            .unwrap_or_default();
        mc.provider = req.provider.clone();
        mc.model = req.model.clone();
        if let Some(url) = req.base_url.clone().filter(|s| !s.trim().is_empty()) {
            mc.base_url = url;
        } else if mc.base_url.trim().is_empty() {
            mc.base_url = InferenceProfileConfig::default().base_url;
        }
        // An omitted/blank api_key keeps the existing one (editing doesn't wipe it).
        if let Some(key) = req.api_key.clone().filter(|s| !s.is_empty()) {
            mc.api_key = key;
        }
        // For the optional fields: an explicit value wins; omitted keeps current.
        if let Some(effort) = req.reasoning_effort.clone() {
            mc.reasoning_effort = Some(effort).filter(|s| !s.is_empty());
        }
        if let Some(images) = req.supports_images {
            mc.supports_images = images;
        }
        if let Some(ctx) = req.context_window.filter(|&n| n > 0) {
            mc.context_window = ctx;
        }
        if let Some(stream) = req.stream {
            mc.stream = stream;
        }
        if let Some(x_search) = req.x_search {
            mc.x_search = x_search;
        }
        c.upsert_profile(&name, mc);
        if req.set_active {
            c.activate(&name);
        }
        save_config(&c, &d.config_path).map(|_| name)
    };
    match result {
        Ok(name) => {
            notify_models(None);
            Json(serde_json::json!({ "name": name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ActiveReq {
    name: String,
}

// POST /config/active — set the global active profile (default for new sessions).
pub(crate) async fn set_active(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ActiveReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await; // don't clobber TUI-side profile edits
    let result = {
        let mut c = d.config.lock().unwrap();
        if !c.activate(&req.name) {
            return (StatusCode::NOT_FOUND, "no such profile").into_response();
        }
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => {
            notify_models(None);
            Json(serde_json::json!({ "active": req.name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct VaultSetReq {
    name: String,
    value: String,
}

// GET /vault — secret NAMES only; values never leave the daemon.
// POST /xai/login — begin the xAI device-code flow and poll for approval in the
// background (saving the token on success). Returns the code + URL for the app to
// show; the app then polls /xai/status until signed_in flips true.
pub(crate) async fn xai_login(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::xai_auth::begin_device_code_login().await {
        Ok(device) => {
            let poll = device.clone();
            tokio::spawn(async move {
                if let Ok(tokens) = crate::xai_auth::poll_for_tokens(poll).await {
                    let _ = crate::xai_auth::save_blocking(&tokens);
                }
            });
            Json(serde_json::json!({
                "user_code": device.user_code,
                "verification_uri": device.verification_uri,
                "expires_in": device.expires_in_s,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// GET /xai/status — whether an xAI subscription token is stored.
pub(crate) async fn xai_status(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "signed_in": crate::xai_auth::is_signed_in() })).into_response()
}

// POST /xai/logout — drop the stored xAI token.
pub(crate) async fn xai_logout(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::xai_auth::logout_blocking() {
        Ok(()) => Json(serde_json::json!({ "signed_in": false })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /chatgpt/login — begin the ChatGPT device-code flow; poll + save in the
// background. Returns the code + URL for the app to show.
pub(crate) async fn chatgpt_login(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::chatgpt_auth::begin_device_code_login().await {
        Ok(device) => {
            let user_code = device.user_code.clone();
            let url = device.verification_url.clone();
            tokio::spawn(async move {
                if let Ok(tokens) = crate::chatgpt_auth::complete_device_code_login(device).await {
                    let _ = crate::chatgpt_auth::save_blocking(&tokens);
                }
            });
            Json(serde_json::json!({
                "user_code": user_code,
                "verification_uri": url,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// GET /chatgpt/status — whether a ChatGPT subscription token is stored.
pub(crate) async fn chatgpt_status(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "signed_in": crate::chatgpt_auth::is_signed_in() })).into_response()
}

// POST /chatgpt/logout — drop the stored ChatGPT token.
pub(crate) async fn chatgpt_logout(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::chatgpt_auth::logout_blocking() {
        Ok(()) => Json(serde_json::json!({ "signed_in": false })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(crate) async fn vault_list(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "names": crate::vault::Vault::load().names() })).into_response()
}

// PUT /vault — store a secret (from the app's vault screen; TLS/tunnel carries it).
pub(crate) async fn vault_set(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<VaultSetReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let mut vault = crate::vault::Vault::load();
    match vault.set(&req.name, &req.value) {
        Ok(()) => Json(serde_json::json!({ "stored": req.name })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct VaultNameQ {
    name: String,
    token: Option<String>,
}

// DELETE /vault?name= — remove a secret.
pub(crate) async fn vault_delete(State(d): State<Shared>, Query(q): Query<VaultNameQ>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let mut vault = crate::vault::Vault::load();
    match vault.remove(&q.name) {
        Ok(true) => Json(serde_json::json!({ "removed": q.name })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such secret").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ProviderModelsReq {
    /// Existing profile to list models for; its stored key/base URL are used.
    #[serde(default)]
    name: Option<String>,
    /// Ad-hoc lookup for a profile being created in an editor (not yet saved).
    /// `api_key` falls back to the named profile's stored key when empty.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

// POST /provider/models — query the provider's own models API (key stays
// server-side) and return a normalized catalog: real model IDs plus whatever
// capabilities the provider reports (effort tiers on Anthropic, reasoning
// support on OpenRouter, context windows where available).
pub(crate) async fn provider_models(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ProviderModelsReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let mut cfg = {
        let c = d.config.lock().unwrap();
        let stored = req
            .name
            .as_deref()
            .and_then(|n| c.setups.as_ref().and_then(|m| m.get(n)).cloned());
        match stored {
            Some(m) => m,
            None if req.provider.is_some() => crate::config::InferenceProfileConfig {
                provider: req.provider.clone().unwrap_or_default(),
                ..Default::default()
            },
            None => return (StatusCode::NOT_FOUND, "no such profile").into_response(),
        }
    };
    // Editor-supplied overrides win over the stored profile's values.
    if let Some(p) = req.provider {
        cfg.provider = p;
    }
    if let Some(b) = req.base_url {
        if !b.trim().is_empty() {
            cfg.base_url = b;
        }
    }
    if let Some(k) = req.api_key {
        if !k.trim().is_empty() {
            cfg.api_key = k;
        }
    }
    match crate::catalog::fetch_models(&cfg).await {
        Ok(models) => Json(serde_json::json!({ "models": models })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct DelegateReq {
    /// Profile for delegated lanes. Empty/null clears it (delegation → active model).
    #[serde(default)]
    name: Option<String>,
}

// POST /config/delegate — set (or clear) the profile that delegated lanes run on.
pub(crate) async fn set_delegate(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<DelegateReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await; // don't clobber TUI-side profile edits
    let name = req.name.filter(|n| !n.trim().is_empty());
    let result = {
        let mut c = d.config.lock().unwrap();
        if let Some(n) = name.as_deref() {
            if !c.setups.as_ref().is_some_and(|m| m.contains_key(n)) {
                return (StatusCode::NOT_FOUND, "no such profile").into_response();
            }
        }
        c.delegate_setup = name.clone();
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => {
            notify_models(None);
            Json(serde_json::json!({ "delegate": name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct DeleteProfileQuery {
    token: Option<String>,
    name: String,
}

// DELETE /config/profile?name= — remove a profile (active falls back to first left).
pub(crate) async fn delete_profile(State(d): State<Shared>, Query(q): Query<DeleteProfileQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    d.reload_config().await; // start from current disk state so we don't resurrect TUI-deleted profiles
    let result = {
        let mut c = d.config.lock().unwrap();
        c.remove_profile(&q.name);
        save_config(&c, &d.config_path)
    };
    match result {
        Ok(_) => {
            notify_models(None);
            Json(serde_json::json!({ "removed": q.name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct SessionModelReq {
    session: String,
    profile: String,
}

// POST /session/model {session, profile} — pin one conversation to a profile.
// Rebuilds its loop on the chosen model, resuming from disk, and persists the
// choice so it survives a daemon restart.
pub(crate) async fn set_session_model(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SessionModelReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    // One implementation, shared with dispatch: both need the same
    // resolve → abort → restart-on-new-model sequence, and a second copy would
    // be free to drift on the role-aware details. The DIFFERENCE is persistence:
    // this route is the user pinning the chat's model, so `true`.
    match d
        .run_session_on_profile(&req.session, &req.profile, true)
        .await
    {
        Ok(()) => {
            notify_models(Some(&req.session));
            Json(serde_json::json!({ "session": req.session, "profile": req.profile }))
                .into_response()
        }
        Err(error) => (StatusCode::NOT_FOUND, error).into_response(),
    }
}



fn notify_models(session: Option<&str>) {
    crate::session::emit_device_event(match session {
        Some(id) => serde_json::json!({ "kind": "models", "session": id }),
        None => serde_json::json!({ "kind": "models" }),
    });
}
