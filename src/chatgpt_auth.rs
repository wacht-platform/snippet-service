//! "Sign in with ChatGPT" — the Codex CLI OAuth flow that lets a user authorize
//! their ChatGPT subscription (Plus/Pro/Team) so snippet can call models through
//! `chatgpt.com/backend-api/codex/responses` instead of a per-token API key.
//!
//! Flow (verified against openai/codex + the opencode reference plugin):
//!   1. PKCE: 64 random bytes → base64url verifier; challenge = b64url(SHA256(verifier)).
//!   2. Open the browser to auth.openai.com/oauth/authorize.
//!   3. A local server on 127.0.0.1:1455 catches the /auth/callback redirect.
//!   4. Exchange the code at auth.openai.com/oauth/token for access/refresh/id tokens.
//!   5. The ChatGPT-Account-Id comes from the id_token JWT claims.
//! Tokens are stored at ~/.snippet/chatgpt_auth.json and refreshed before expiry.

use std::path::PathBuf;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const FALLBACK_CALLBACK_PORT: u16 = 1457;
const DEVICE_AUTH_API_BASE_URL: &str = "https://auth.openai.com/api/accounts";
const DEVICE_AUTH_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_AUTH_CALLBACK_URI: &str = "https://auth.openai.com/deviceauth/callback";
const SCOPE: &str = "openid profile email offline_access";
const ORIGINATOR: &str = "codex_cli_rs";

/// Persisted ChatGPT-subscription credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub id_token: String,
    /// Unix seconds when `access_token` expires.
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl ChatGptTokens {
    /// True when the access token is expired or within 60s of expiry.
    pub fn is_stale(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.expires_at - now <= 60
    }
}

pub fn tokens_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".snippet").join("chatgpt_auth.json")
}

/// Read stored tokens synchronously (used at model construction, which is sync).
pub fn load_blocking() -> Option<ChatGptTokens> {
    let raw = std::fs::read_to_string(tokens_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_blocking(tokens: &ChatGptTokens) -> Result<(), String> {
    let path = tokens_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn logout_blocking() -> Result<(), String> {
    let path = tokens_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// Whether a sign-in already exists.
pub fn is_signed_in() -> bool {
    load_blocking().is_some()
}

fn pkce() -> (String, String) {
    // 64 random bytes from four v4 UUIDs (getrandom-backed) → base64url verifier.
    let mut bytes = Vec::with_capacity(64);
    for _ in 0..4 {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    let verifier = URL_SAFE_NO_PAD.encode(&bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn build_authorize_url(challenge: &str, state: &str, redirect_uri: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator={}",
        pct_encode(CLIENT_ID),
        pct_encode(redirect_uri),
        pct_encode(SCOPE),
        pct_encode(challenge),
        pct_encode(state),
        pct_encode(ORIGINATOR),
    )
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

/// The ChatGPT-Account-Id lives in the id_token JWT claims at
/// `["https://api.openai.com/auth"]["chatgpt_account_id"]`.
fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("email")
        .or_else(|| claims.pointer("/https:~1~1api.openai.com~1profile/email"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatGptLoginMethod {
    Browser,
    DeviceCode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeInfo {
    pub verification_url: String,
    pub user_code: String,
    /// Carried so the poll phase reuses the SAME code that was displayed.
    pub device_auth_id: String,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn tokens_from_response(resp: TokenResponse, prior: Option<&ChatGptTokens>) -> Result<ChatGptTokens, String> {
    let access_token = resp
        .access_token
        .or_else(|| prior.map(|p| p.access_token.clone()))
        .ok_or("token response had no access_token")?;
    let refresh_token = resp
        .refresh_token
        .or_else(|| prior.map(|p| p.refresh_token.clone()))
        .ok_or("token response had no refresh_token")?;
    let id_token = resp
        .id_token
        .or_else(|| prior.map(|p| p.id_token.clone()))
        .unwrap_or_default();
    let account_id = account_id_from_id_token(&id_token)
        .or_else(|| prior.map(|p| p.account_id.clone()))
        .ok_or("could not read chatgpt_account_id from id_token")?;
    let email = email_from_id_token(&id_token).or_else(|| prior.and_then(|p| p.email.clone()));
    let expires_at = chrono::Utc::now().timestamp() + resp.expires_in.unwrap_or(3600);
    Ok(ChatGptTokens {
        access_token,
        refresh_token,
        account_id,
        id_token,
        expires_at,
        email,
    })
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<ChatGptTokens, String> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange failed: HTTP {status}: {body}"));
    }
    let parsed: TokenResponse = resp.json().await.map_err(|e| format!("parse token JSON: {e}"))?;
    tokens_from_response(parsed, None)
}

/// Refresh an expired access token using the stored refresh token.
pub async fn refresh(prior: &ChatGptTokens) -> Result<ChatGptTokens, String> {
    let client = reqwest::Client::new();
    let body = json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": prior.refresh_token,
    });
    let resp = client
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("refresh request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("token refresh failed: HTTP {status}: {text}"));
    }
    let parsed: TokenResponse = resp.json().await.map_err(|e| format!("parse refresh JSON: {e}"))?;
    let refreshed = tokens_from_response(parsed, Some(prior))?;
    save_blocking(&refreshed)?;
    Ok(refreshed)
}

/// Wait for the OAuth redirect on the local callback server, returning the code.
async fn wait_for_callback(state: &str) -> Result<(String, String), String> {
    let listener = match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(listener) => (listener, CALLBACK_PORT),
        Err(primary) => TcpListener::bind(("127.0.0.1", FALLBACK_CALLBACK_PORT))
            .await
            .map(|listener| (listener, FALLBACK_CALLBACK_PORT))
            .map_err(|fallback| {
                format!(
                    "could not bind 127.0.0.1:{CALLBACK_PORT} ({primary}); fallback 127.0.0.1:{FALLBACK_CALLBACK_PORT} also failed: {fallback}"
                )
            })?,
    };
    let (listener, port) = listener;

    let deadline = Duration::from_secs(300);
    loop {
        let (mut stream, _) = timeout(deadline, listener.accept())
            .await
            .map_err(|_| "sign-in timed out (no callback within 5 min)".to_string())?
            .map_err(|e| format!("callback accept failed: {e}"))?;

        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");

        if !target.starts_with("/auth/callback") {
            // Ignore stray requests (favicon, etc.) and keep listening.
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        }

        let query = target.split('?').nth(1).unwrap_or("");
        let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                params.insert(pct_decode(k), pct_decode(v));
            }
        }

        let body = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Signed in to snippet</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #0b1020;
      --panel: rgba(17, 24, 39, 0.92);
      --border: rgba(148, 163, 184, 0.18);
      --text: #e5eefb;
      --muted: #9fb0c8;
      --accent: #7dd3fc;
      --accent-2: #a78bfa;
      --success: #34d399;
      --shadow: 0 24px 80px rgba(0, 0, 0, 0.45);
    }
    * { box-sizing: border-box; }
    html, body { height: 100%; }
    body {
      margin: 0;
      min-height: 100vh;
      display: grid;
      place-items: center;
      padding: 24px;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      color: var(--text);
      background:
        radial-gradient(circle at top, rgba(125, 211, 252, 0.18), transparent 34%),
        radial-gradient(circle at bottom right, rgba(167, 139, 250, 0.16), transparent 28%),
        linear-gradient(180deg, #0f172a 0%, var(--bg) 100%);
    }
    .card {
      width: min(560px, 100%);
      background: var(--panel);
      border: 1px solid var(--border);
      border-radius: 24px;
      box-shadow: var(--shadow);
      padding: 32px;
      backdrop-filter: blur(14px);
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 10px;
      padding: 8px 12px;
      border-radius: 999px;
      background: rgba(52, 211, 153, 0.12);
      border: 1px solid rgba(52, 211, 153, 0.24);
      color: #c7f9e8;
      font-size: 14px;
      font-weight: 600;
      letter-spacing: 0.01em;
    }
    .dot {
      width: 10px;
      height: 10px;
      border-radius: 999px;
      background: var(--success);
      box-shadow: 0 0 0 6px rgba(52, 211, 153, 0.12);
    }
    h1 {
      margin: 20px 0 10px;
      font-size: clamp(32px, 5vw, 42px);
      line-height: 1.05;
      letter-spacing: -0.03em;
    }
    p {
      margin: 0;
      color: var(--muted);
      font-size: 16px;
      line-height: 1.65;
    }
    .panel {
      margin-top: 24px;
      padding: 18px 20px;
      border-radius: 18px;
      background: rgba(15, 23, 42, 0.7);
      border: 1px solid rgba(125, 211, 252, 0.14);
    }
    .panel strong {
      display: block;
      margin-bottom: 6px;
      color: var(--text);
      font-size: 15px;
    }
    .brand {
      margin-top: 22px;
      color: #6b7a90;
      font-size: 13px;
      text-transform: uppercase;
      letter-spacing: 0.14em;
    }
  </style>
</head>
<body>
  <main class="card">
    <div class="badge"><span class="dot"></span> ChatGPT sign-in complete</div>
    <h1>You’re signed in to snippet.</h1>
    <p>The terminal has your session now. You can close this tab and go back to your editor.</p>
    <section class="panel">
      <strong>What happens next</strong>
      <p>Return to snippet and continue using the ChatGPT-backed Codex models from the app.</p>
    </section>
    <div class="brand">snippet</div>
  </main>
</body>
</html>
"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;

        if let Some(err) = params.get("error") {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            return Err(format!("authorization denied: {err} {desc}"));
        }
        if params.get("state").map(String::as_str) != Some(state) {
            return Err("state mismatch in callback (possible CSRF) — try again".to_string());
        }
        let code = params
            .get("code")
            .cloned()
            .ok_or_else(|| "callback had no authorization code".to_string())?;
        return Ok((code, format!("http://localhost:{port}/auth/callback")));
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeUserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

fn parse_interval(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(5),
        serde_json::Value::String(s) => s.trim().parse::<u64>().unwrap_or(5),
        _ => 5,
    }
}

async fn request_device_code() -> Result<(String, String, u64), String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{DEVICE_AUTH_API_BASE_URL}/deviceauth/usercode"))
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("device code request failed: HTTP {status}: {body}"));
    }
    let parsed: DeviceCodeUserCodeResponse = resp
        .json()
        .await
        .map_err(|e| format!("parse device code JSON: {e}"))?;
    Ok((
        parsed.device_auth_id,
        parsed.user_code,
        parse_interval(&parsed.interval).max(1),
    ))
}

async fn poll_device_code(device_auth_id: &str, user_code: &str, interval: u64) -> Result<DeviceCodeTokenResponse, String> {
    let client = reqwest::Client::new();
    let started = Instant::now();
    loop {
        let resp = client
            .post(format!("{DEVICE_AUTH_API_BASE_URL}/deviceauth/token"))
            .json(&json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .map_err(|e| format!("device code poll failed: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .json()
                .await
                .map_err(|e| format!("parse device token JSON: {e}"));
        }
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
            if started.elapsed() >= Duration::from_secs(15 * 60) {
                return Err("device code sign-in timed out after 15 minutes".to_string());
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            continue;
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("device code poll failed: HTTP {status}: {body}"));
    }
}

pub async fn begin_device_code_login() -> Result<DeviceCodeInfo, String> {
    let (device_auth_id, user_code, interval) = request_device_code().await?;
    Ok(DeviceCodeInfo {
        verification_url: DEVICE_AUTH_VERIFICATION_URL.to_string(),
        user_code,
        device_auth_id,
        interval,
    })
}

/// Poll for and complete a device-code sign-in begun by `begin_device_code_login`,
/// reusing the same code that was shown to the user.
pub async fn complete_device_code_login(info: DeviceCodeInfo) -> Result<ChatGptTokens, String> {
    let code = poll_device_code(&info.device_auth_id, &info.user_code, info.interval).await?;
    let client = reqwest::Client::new();
    let tokens = exchange_code(
        &client,
        &code.authorization_code,
        &code.code_verifier,
        DEVICE_AUTH_CALLBACK_URI,
    )
    .await?;
    save_blocking(&tokens)?;
    Ok(tokens)
}

async fn login_browser() -> Result<ChatGptTokens, String> {
    let (verifier, challenge) = pkce();
    let state = uuid::Uuid::new_v4().simple().to_string();

    let callback = tokio::spawn({
        let state = state.clone();
        async move { wait_for_callback(&state).await }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let redirect_uri = REDIRECT_URI;
    let url = build_authorize_url(&challenge, &state, redirect_uri);
    open_browser(&url);

    let (code, actual_redirect_uri) = callback
        .await
        .map_err(|e| format!("callback task failed: {e}"))??;

    let client = reqwest::Client::new();
    let tokens = exchange_code(&client, &code, &verifier, &actual_redirect_uri).await?;
    save_blocking(&tokens)?;
    Ok(tokens)
}

async fn login_device_code() -> Result<ChatGptTokens, String> {
    let info = begin_device_code_login().await?;
    complete_device_code_login(info).await
}

pub async fn login(method: ChatGptLoginMethod) -> Result<ChatGptTokens, String> {
    match method {
        ChatGptLoginMethod::Browser => login_browser().await,
        ChatGptLoginMethod::DeviceCode => login_device_code().await,
    }
}
