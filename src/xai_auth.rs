//! xAI (Grok/X subscription) OAuth — RFC 8628 device-code flow. A SuperGrok or X
//! Premium account authorizes a device code, and the resulting access token is used
//! as a plain Bearer key against the standard xAI API (`https://api.x.ai/v1`).
//!
//! Tokens are stored at `~/.snippet/xai_auth.json` and refreshed before expiry.
//! Ported from the pi harness's `auth/oauth/xai.ts`.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_S: i64 = 3600;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XaiTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

impl XaiTokens {
    pub fn is_stale(&self) -> bool {
        now_ms() >= self.expires_at_ms
    }
}

pub fn tokens_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".snippet")
        .join("xai_auth.json")
}

pub fn load_blocking() -> Option<XaiTokens> {
    std::fs::read_to_string(tokens_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save_blocking(tokens: &XaiTokens) -> Result<(), String> {
    let path = tokens_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn logout_blocking() -> Result<(), String> {
    match std::fs::remove_file(tokens_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn is_signed_in() -> bool {
    load_blocking().is_some()
}

#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub interval_s: u64,
    pub expires_in_s: i64,
}

fn tokens_from_response(body: &Value, prior_refresh: Option<&str>) -> Result<XaiTokens, String> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("token response had no access_token")?
        .to_string();
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| prior_refresh.map(str::to_string))
        .ok_or("token response had no refresh_token")?;
    let lifetime = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_S);
    Ok(XaiTokens {
        access_token: access,
        refresh_token: refresh,
        expires_at_ms: now_ms() + lifetime * 1000 - REFRESH_SKEW_MS,
    })
}

async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<(bool, Value), String> {
    let resp = client()
        .post(url)
        .header("accept", "application/json")
        .form(fields)
        .send()
        .await
        .map_err(|e| format!("xAI OAuth request failed: {e}"))?;
    let ok = resp.status().is_success();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((ok, body))
}

fn failure(action: &str, body: &Value) -> String {
    let err = body.get("error").and_then(Value::as_str).unwrap_or("");
    let desc = body.get("error_description").and_then(Value::as_str).unwrap_or("");
    let detail = [err, desc].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(": ");
    if detail.is_empty() {
        format!("xAI OAuth {action} failed")
    } else {
        format!("xAI OAuth {action} failed: {detail}")
    }
}

/// Step 1: request a device code the user authorizes at x.ai.
pub async fn begin_device_code_login() -> Result<DeviceCodeInfo, String> {
    let (ok, body) = post_form(
        DEVICE_CODE_URL,
        &[("client_id", CLIENT_ID), ("scope", SCOPE), ("referrer", "snippet")],
    )
    .await?;
    if !ok {
        return Err(failure("device authorization", &body));
    }
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(DeviceCodeInfo {
        user_code: s("user_code").ok_or("no user_code")?,
        verification_uri: s("verification_uri_complete")
            .or_else(|| s("verification_uri"))
            .ok_or("no verification_uri")?,
        device_code: s("device_code").ok_or("no device_code")?,
        interval_s: body.get("interval").and_then(Value::as_u64).filter(|n| *n > 0).unwrap_or(5),
        expires_in_s: body.get("expires_in").and_then(Value::as_i64).unwrap_or(600),
    })
}

/// Step 2: poll the token endpoint until the user approves (or it expires).
pub async fn poll_for_tokens(device: DeviceCodeInfo) -> Result<XaiTokens, String> {
    let deadline = now_ms() + device.expires_in_s * 1000;
    let mut interval = device.interval_s.max(1);
    // Device flows require waiting before the first poll.
    tokio::time::sleep(Duration::from_secs(interval)).await;
    loop {
        if now_ms() > deadline {
            return Err("xAI device code expired — run login again".to_string());
        }
        let (ok, body) = post_form(
            TOKEN_URL,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", &device.device_code),
            ],
        )
        .await?;
        if ok {
            return tokens_from_response(&body, None);
        }
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") | Some("authorization_denied") => {
                return Err("xAI device authorization was denied".to_string());
            }
            Some("expired_token") => return Err("xAI device code expired".to_string()),
            _ => return Err(failure("device token polling", &body)),
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

pub async fn refresh(prior: &XaiTokens) -> Result<XaiTokens, String> {
    let (ok, body) = post_form(
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &prior.refresh_token),
        ],
    )
    .await?;
    if !ok {
        return Err(failure("token refresh", &body));
    }
    tokens_from_response(&body, Some(&prior.refresh_token))
}

/// A valid access token for a request: loads the stored token, refreshes it when
/// stale (persisting the new one), and returns the bearer value. Errors when not
/// signed in or the refresh fails (the caller surfaces "sign in again").
pub async fn access_token() -> Result<String, String> {
    let tokens = load_blocking().ok_or("not signed in to xAI — run `snippet xai login`")?;
    if !tokens.is_stale() {
        return Ok(tokens.access_token);
    }
    let fresh = refresh(&tokens).await?;
    save_blocking(&fresh)?;
    Ok(fresh.access_token)
}
