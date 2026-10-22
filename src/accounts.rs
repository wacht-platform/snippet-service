use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::llm::RateLimitSnapshot;

pub const MAIN: &str = "main";
pub const PROVIDERS: [&str; 3] = ["chatgpt", "xai", "claude-code"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pool {
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    labels: HashMap<String, String>,
    #[serde(default)]
    limited: HashMap<String, i64>,
    #[serde(default)]
    usage: HashMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub limited_until: Option<i64>,
    pub usage: Option<RateLimitSnapshot>,
}

static LOCK: Mutex<()> = Mutex::new(());

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn dir(provider: &str) -> PathBuf {
    crate::config::snippet_home().join("accounts").join(provider)
}

fn pool_path(provider: &str) -> PathBuf {
    dir(provider).join("pool.json")
}

fn load(provider: &str) -> Pool {
    std::fs::read_to_string(pool_path(provider))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store(provider: &str, pool: &Pool) {
    let path = pool_path(provider);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string_pretty(pool) {
        let _ = std::fs::write(path, body);
    }
}

fn edit<T>(provider: &str, f: impl FnOnce(&mut Pool) -> T) -> T {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load(provider);
    let out = f(&mut pool);
    store(provider, &pool);
    out
}

pub fn is_pooled(provider: &str) -> bool {
    PROVIDERS.contains(&provider)
}

pub fn credential_path(provider: &str, id: &str) -> Option<PathBuf> {
    match (provider, id) {
        ("chatgpt", MAIN) => Some(crate::chatgpt_auth::tokens_path()),
        ("xai", MAIN) => Some(crate::xai_auth::tokens_path()),
        ("claude-code", MAIN) => None,
        _ => Some(dir(provider).join(format!("{id}.json"))),
    }
}

fn exists(provider: &str, id: &str) -> bool {
    match credential_path(provider, id) {
        Some(path) => path.is_file(),
        None => true,
    }
}

fn ids_in(provider: &str, pool: &Pool) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    if exists(provider, MAIN) {
        found.push(MAIN.to_string());
    }
    if let Ok(entries) = std::fs::read_dir(dir(provider)) {
        let mut extra: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
            .filter(|n| n != "pool" && n != MAIN)
            .collect();
        extra.sort();
        found.extend(extra);
    }
    let mut ordered: Vec<String> = pool.order.iter().filter(|id| found.contains(id)).cloned().collect();
    for id in found {
        if !ordered.contains(&id) {
            ordered.push(id);
        }
    }
    ordered
}

pub fn ids(provider: &str) -> Vec<String> {
    ids_in(provider, &load(provider))
}

fn limited_in(pool: &Pool, id: &str) -> Option<i64> {
    pool.limited.get(id).copied().filter(|until| *until > now())
}

pub fn active(provider: &str) -> Option<String> {
    let pool = load(provider);
    let ids = ids_in(provider, &pool);
    ids.iter()
        .find(|id| limited_in(&pool, id).is_none())
        .or_else(|| ids.iter().min_by_key(|id| limited_in(&pool, id).unwrap_or(0)))
        .cloned()
}

pub fn exhausted_until(snapshot: &RateLimitSnapshot) -> Option<i64> {
    [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter(|w| w.used_percent >= 99.5 && w.resets_at > now())
        .map(|w| w.resets_at)
        .max()
}

pub fn record_usage(provider: &str, id: &str, snapshot: &RateLimitSnapshot) {
    edit(provider, |pool| {
        pool.usage.insert(id.to_string(), snapshot.clone());
        if let Some(until) = exhausted_until(snapshot) {
            let slot = pool.limited.entry(id.to_string()).or_insert(until);
            *slot = (*slot).max(until);
        }
    });
}

pub fn usage(provider: &str, id: &str) -> Option<RateLimitSnapshot> {
    load(provider).usage.get(id).cloned()
}

pub fn reset_for(provider: &str, id: &str, hint: Option<i64>) -> i64 {
    if let Some(at) = hint.filter(|at| *at > now()) {
        return at;
    }
    let snapshot = usage(provider, id);
    if let Some(at) = snapshot.as_ref().and_then(exhausted_until) {
        return at;
    }
    crate::routing::reset_from(snapshot.as_ref())
}

pub fn rotate(provider: &str, from: &str, until: i64) -> Option<String> {
    let next = edit(provider, |pool| {
        let until = until.max(now() + 60);
        let slot = pool.limited.entry(from.to_string()).or_insert(until);
        *slot = (*slot).max(until);
        let ids = ids_in(provider, pool);
        ids.into_iter().find(|id| id != from && limited_in(pool, id).is_none())
    });
    if let Some(next) = &next {
        eprintln!("accounts: {provider} {from} is limited; switching to {next}");
    }
    next
}

pub fn label(provider: &str, id: &str) -> String {
    let pool = load(provider);
    if let Some(label) = pool.labels.get(id).filter(|l| !l.trim().is_empty()) {
        return label.clone();
    }
    let email = credential_path(provider, id)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(str::to_string))
        .filter(|e| !e.is_empty());
    if let Some(email) = email {
        return email;
    }
    let position = ids_in(provider, &pool).iter().position(|x| x == id).map(|i| i + 1).unwrap_or(1);
    format!("Account {position}")
}

pub fn list(provider: &str) -> Vec<AccountView> {
    let current = active(provider);
    let pool = load(provider);
    ids_in(provider, &pool)
        .into_iter()
        .map(|id| AccountView {
            label: label(provider, &id),
            active: current.as_deref() == Some(id.as_str()),
            limited_until: limited_in(&pool, &id),
            usage: pool.usage.get(&id).cloned(),
            id,
        })
        .collect()
}

pub fn new_id(provider: &str) -> String {
    if !exists(provider, MAIN) && provider != "claude-code" {
        return MAIN.to_string();
    }
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

pub fn write_credential(provider: &str, id: &str, body: &str) -> Result<(), String> {
    let path = credential_path(provider, id).ok_or("this account has no stored credential")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn read_credential(provider: &str, id: &str) -> Option<String> {
    std::fs::read_to_string(credential_path(provider, id)?).ok()
}

pub fn remove(provider: &str, id: &str) -> Result<(), String> {
    if let Some(path) = credential_path(provider, id) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    } else {
        return Err("the signed-in Claude Code account is managed by `claude` itself".to_string());
    }
    edit(provider, |pool| {
        pool.order.retain(|x| x != id);
        pool.labels.remove(id);
        pool.limited.remove(id);
        pool.usage.remove(id);
    });
    Ok(())
}

pub fn set_order(provider: &str, order: Vec<String>) {
    edit(provider, |pool| pool.order = order);
}

pub fn set_label(provider: &str, id: &str, label: &str) {
    edit(provider, |pool| {
        if label.trim().is_empty() {
            pool.labels.remove(id);
        } else {
            pool.labels.insert(id.to_string(), label.trim().to_string());
        }
    });
}

pub fn clear_limit(provider: &str, id: &str) {
    edit(provider, |pool| {
        pool.limited.remove(id);
    });
}

pub fn reset_hint(body: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    if let Some(at) = error.get("resets_at").and_then(|v| v.as_i64()) {
        return Some(at);
    }
    error
        .get("resets_in_seconds")
        .and_then(|v| v.as_i64())
        .map(|secs| now() + secs)
}

fn error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    ["type", "code"]
        .iter()
        .find_map(|k| error.get(*k).and_then(|v| v.as_str()).map(str::to_string))
}

pub fn is_limit_response(
    status: reqwest::StatusCode,
    body: &str,
    snapshot: Option<&RateLimitSnapshot>,
    retry_after: Option<std::time::Duration>,
) -> bool {
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return false;
    }
    if reset_hint(body).is_some() || snapshot.and_then(exhausted_until).is_some() {
        return true;
    }
    if let Some(code) = error_code(body) {
        return matches!(
            code.as_str(),
            "usage_limit_reached" | "usage_not_included" | "insufficient_quota" | "quota_exceeded" | "resource_exhausted"
        );
    }
    retry_after.is_none_or(|delay| delay.as_secs() > 300)
}
