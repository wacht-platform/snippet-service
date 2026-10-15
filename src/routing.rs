use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::SnippetConfig;
use crate::llm::RateLimitSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Memory,
    Mission,
    Inbox,
    Lanes,
}

impl Route {
    pub const ALL: [Route; 4] = [Route::Memory, Route::Mission, Route::Inbox, Route::Lanes];

    pub fn key(self) -> &'static str {
        match self {
            Route::Memory => "memory",
            Route::Mission => "mission",
            Route::Inbox => "inbox",
            Route::Lanes => "lanes",
        }
    }

    pub fn parse(key: &str) -> Option<Route> {
        Route::ALL.into_iter().find(|r| r.key() == key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mission: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbox: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<String>,
}

impl RoutingConfig {
    pub fn is_empty(&self) -> bool {
        Route::ALL.iter().all(|r| self.get(*r).is_empty())
    }

    pub fn get(&self, route: Route) -> &[String] {
        match route {
            Route::Memory => &self.memory,
            Route::Mission => &self.mission,
            Route::Inbox => &self.inbox,
            Route::Lanes => &self.lanes,
        }
    }

    pub fn set(&mut self, route: Route, profiles: Vec<String>) {
        let slot = match route {
            Route::Memory => &mut self.memory,
            Route::Mission => &mut self.mission,
            Route::Inbox => &mut self.inbox,
            Route::Lanes => &mut self.lanes,
        };
        *slot = profiles;
    }
}

pub fn configured(cfg: &SnippetConfig, route: Route) -> Vec<String> {
    let mut names: Vec<String> = cfg.routing.get(route).to_vec();
    if names.is_empty() && route == Route::Lanes {
        names.extend(cfg.delegate_setup.clone());
    }
    let exists = |n: &String| cfg.setups.as_ref().is_some_and(|s| s.contains_key(n));
    let mut seen = Vec::new();
    for n in names {
        if exists(&n) && !seen.contains(&n) {
            seen.push(n);
        }
    }
    seen
}

pub fn candidates(cfg: &SnippetConfig, route: Route) -> Vec<String> {
    configured(cfg, route).into_iter().filter(|n| limited_until(n).is_none()).collect()
}

pub fn next_after(cfg: &SnippetConfig, route: Route, current: &str) -> Option<String> {
    let all = configured(cfg, route);
    let start = all.iter().position(|n| n == current).map(|i| i + 1).unwrap_or(0);
    all.into_iter()
        .skip(start)
        .find(|n| n != current && limited_until(n).is_none())
}

pub fn alternatives(cfg: &SnippetConfig, current: &str) -> Vec<String> {
    cfg.setups
        .as_ref()
        .map(|s| s.keys().filter(|n| n.as_str() != current && limited_until(n).is_none()).cloned().collect())
        .unwrap_or_default()
}

fn limited() -> &'static Mutex<HashMap<String, i64>> {
    static LIMITED: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    LIMITED.get_or_init(Default::default)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn mark_limited(profile: &str, until: i64) {
    if profile.is_empty() {
        return;
    }
    let until = until.max(now() + 60);
    let mut map = limited().lock().unwrap();
    let slot = map.entry(profile.to_string()).or_insert(until);
    *slot = (*slot).max(until);
}

pub fn limited_until(profile: &str) -> Option<i64> {
    let mut map = limited().lock().unwrap();
    match map.get(profile).copied() {
        Some(until) if until > now() => Some(until),
        Some(_) => {
            map.remove(profile);
            None
        }
        None => None,
    }
}

pub fn limited_now() -> HashMap<String, i64> {
    let now = now();
    let mut map = limited().lock().unwrap();
    map.retain(|_, until| *until > now);
    map.clone()
}

pub fn limit_in(result: &Result<crate::harness::HarnessOutcome, crate::tools::ToolError>) -> Option<String> {
    let message = match result {
        Err(e) => e.to_string(),
        Ok(outcome) => match outcome.events.last() {
            Some(crate::harness::HarnessEvent::ModelError { message }) => message.clone(),
            _ => return None,
        },
    };
    is_limit_error(&message).then_some(message)
}

pub fn reset_label(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "soon".to_string())
}

pub fn is_limit_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    if m.contains("context") && !m.contains("rate") && !m.contains("usage") {
        return false;
    }
    m.starts_with("rate limited")
        || m.contains("rate limit")
        || m.contains("rate_limit")
        || m.contains("usage limit")
        || m.contains("hit your limit")
        || m.contains("limit reached")
        || m.contains("too many requests")
        || m.contains("resource_exhausted")
        || m.contains("resource exhausted")
        || m.contains("quota")
        || m.contains(" 429")
        || m.starts_with("429")
}

pub fn reset_from(snapshot: Option<&RateLimitSnapshot>) -> i64 {
    let fallback = now() + 3600;
    let Some(snap) = snapshot else {
        return fallback;
    };
    let windows: Vec<_> = [snap.primary.as_ref(), snap.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter(|w| w.resets_at > now())
        .collect();
    windows
        .iter()
        .filter(|w| w.used_percent >= 95.0)
        .map(|w| w.resets_at)
        .max()
        .or_else(|| windows.iter().map(|w| w.resets_at).min())
        .unwrap_or(fallback)
}

#[derive(Debug, Clone)]
pub struct LimitHit {
    pub session_id: String,
    pub message: String,
    pub resets_at: i64,
}

fn sender() -> &'static OnceLock<mpsc::UnboundedSender<LimitHit>> {
    static TX: OnceLock<mpsc::UnboundedSender<LimitHit>> = OnceLock::new();
    &TX
}

pub fn subscribe() -> Option<mpsc::UnboundedReceiver<LimitHit>> {
    let (tx, rx) = mpsc::unbounded_channel();
    sender().set(tx).ok().map(|_| rx)
}

pub fn report(hit: LimitHit) {
    if let Some(tx) = sender().get() {
        let _ = tx.send(hit);
    }
}
