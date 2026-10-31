//! Provider prompt-cache keep-warm for idle agents.
//!
//! An idle session's cached prefix expires with the provider's TTL. Where a
//! one-hour cache can be written (Anthropic with `cache_ttl = "1h"`), a ping
//! a little under the hour re-reads the prefix and so extends it, until the
//! session has been idle `max_idle_hours`. The ping adds nothing to the
//! transcript and its usage is tagged `keep_warm` in the ledger.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::agent_wakes::{OwnerKind, Setter};
use crate::llm::{NativeToolDefinition, TokenUsage};
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub const PING: &str = "[keep-warm] Reply with one word.";
pub const NO_HOUR_CACHE: &str = "this provider has no one-hour prompt cache a request can extend";
pub const CACHE_TTL_SECS: i64 = 3600;
/// Past this, the one-hour entry is about to lapse and a ping would rewrite it in full.
const EXPIRY_MARGIN_SECS: i64 = 120;
pub const DEFAULT_INTERVAL_MINUTES: u32 = 55;
pub const MIN_INTERVAL_MINUTES: u32 = 30;
pub const MAX_INTERVAL_MINUTES: u32 = 57;
pub const DEFAULT_MAX_IDLE_HOURS: u32 = 6;
pub const MAX_IDLE_HOURS_CEILING: u32 = 24;

/// Why a provider can't be kept warm, by profile provider name. `None` defers to
/// the adapter's own answer (Anthropic decides by its cache TTL).
pub fn reason_for(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" | "anthropic-compatible" => None,
        "claude-code" | "antigravity" => {
            Some("runs through a CLI driver that owns its own prompt cache")
        }
        "gemini" => Some("Gemini context caches here are explicit with a 30-minute TTL, not a one-hour prompt cache"),
        "openai" | "chatgpt" | "xai" | "grok" | "openrouter" | "openai-compatible" => Some(
            "automatic prefix caching: the provider sets retention and offers no one-hour TTL to request",
        ),
        _ => Some(NO_HOUR_CACHE),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepWarm {
    pub owner: String,
    pub owner_kind: OwnerKind,
    pub session_id: String,
    pub enabled: bool,
    pub interval_minutes: u32,
    pub max_idle_hours: u32,
    pub set_by: Setter,
    pub updated_at: i64,
    #[serde(default)]
    pub last_warm_at: Option<i64>,
    #[serde(default)]
    pub last_cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub last_cache_creation_tokens: Option<u64>,
    #[serde(default)]
    pub skipped_reason: Option<String>,
    #[serde(default)]
    pub skipped_at: Option<i64>,
    /// Set while the session's model can't hold a one-hour cache; outranks
    /// the timing reasons in `skipped_reason`.
    #[serde(default)]
    pub provider_reason: Option<String>,
    #[serde(default)]
    pub warms: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct KeepWarmSpec {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub interval_minutes: Option<u32>,
    #[serde(default)]
    pub max_idle_hours: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Warm,
    Wait,
    Skip(String),
}

/// What to do for one entry now. `last_turn` is the session's last real model
/// call; `quiet` and `busy` are the owner's quiet hours and a running turn.
pub fn decide(entry: &KeepWarm, now: i64, last_turn: Option<i64>, quiet: bool, busy: bool) -> Decision {
    if !entry.enabled {
        return Decision::Skip("disabled".into());
    }
    let Some(last_turn) = last_turn else {
        return Decision::Skip("no model call in this session yet".into());
    };
    let idle = now - last_turn;
    if idle >= entry.max_idle_hours as i64 * 3600 {
        return Decision::Skip(format!("idle over {}h; stopped until the next turn", entry.max_idle_hours));
    }
    if quiet {
        return Decision::Skip("quiet hours".into());
    }
    let touched = last_turn
        .max(entry.last_warm_at.unwrap_or(0))
        .max(entry.skipped_at.unwrap_or(0));
    let since = now - touched;
    if since < entry.interval_minutes as i64 * 60 {
        return Decision::Wait;
    }
    let cache_touched = last_turn.max(entry.last_warm_at.unwrap_or(0));
    if now - cache_touched >= CACHE_TTL_SECS - EXPIRY_MARGIN_SECS {
        return Decision::Skip("cache already lapsed; the next turn rebuilds it".into());
    }
    if busy {
        return Decision::Wait;
    }
    Decision::Warm
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Warmed(TokenUsage),
    /// The model can't hold a one-hour cache; no request was sent.
    Unsupported(String),
    Skipped(String),
    Failed(String),
}

static PENDING: LazyLock<Mutex<HashMap<String, oneshot::Sender<Outcome>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Register a ping the session loop will answer through [`complete`].
pub fn register(id: &str) -> oneshot::Receiver<Outcome> {
    let (tx, rx) = oneshot::channel();
    if let Ok(mut pending) = PENDING.lock() {
        pending.insert(id.to_string(), tx);
    }
    rx
}

pub fn complete(id: &str, outcome: Outcome) {
    let tx = PENDING.lock().ok().and_then(|mut p| p.remove(id));
    if let Some(tx) = tx {
        let _ = tx.send(outcome);
    }
}

/// Apply a ping's outcome to the entry.
pub fn record_outcome(entry: &mut KeepWarm, outcome: &Outcome, now: i64) {
    match outcome {
        Outcome::Warmed(usage) => {
            entry.last_warm_at = Some(now);
            entry.last_cache_read_tokens = Some(usage.cache_read_tokens);
            entry.last_cache_creation_tokens = Some(usage.cache_creation_tokens);
            entry.skipped_reason = None;
            entry.skipped_at = None;
            entry.provider_reason = None;
            entry.warms += 1;
        }
        Outcome::Unsupported(reason) => {
            entry.provider_reason = Some(reason.clone());
            entry.skipped_reason = Some(reason.clone());
            entry.skipped_at = Some(now);
        }
        Outcome::Skipped(reason) => {
            entry.skipped_reason = Some(reason.clone());
            entry.skipped_at = Some(now);
        }
        Outcome::Failed(error) => {
            entry.skipped_reason = Some(format!("ping failed: {error}"));
        }
    }
}

pub fn list(root: &Path) -> Result<Vec<KeepWarm>, String> {
    let store = crate::recurring::store_at(root)?;
    let rows = store.keep_warm_entries().map_err(|e| e.to_string())?;
    Ok(rows.iter().filter_map(|raw| serde_json::from_str(raw).ok()).collect())
}

pub fn get(root: &Path, owner: &str) -> Result<Option<KeepWarm>, String> {
    Ok(list(root)?.into_iter().find(|e| e.owner == owner.trim()))
}

pub fn put(root: &Path, entry: &KeepWarm) -> Result<(), String> {
    let raw = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    crate::recurring::store_at(root)?
        .put_keep_warm(&entry.owner, &raw, entry.updated_at)
        .map_err(|e| e.to_string())
}

pub fn clear(root: &Path, owner: &str, setter: Setter) -> Result<bool, String> {
    if setter == Setter::MissionControl
        && get(root, owner)?.is_some_and(|e| e.set_by == Setter::User)
    {
        return Err("the user set this keep-warm; ask them to change it".into());
    }
    crate::recurring::store_at(root)?
        .delete_keep_warm(owner.trim())
        .map_err(|e| e.to_string())
}

pub fn set(
    root: &Path,
    owner: &str,
    kind: OwnerKind,
    spec: KeepWarmSpec,
    setter: Setter,
    now: i64,
) -> Result<KeepWarm, String> {
    let owner = owner.trim();
    let current = get(root, owner)?;
    if setter == Setter::MissionControl && current.as_ref().is_some_and(|e| e.set_by == Setter::User) {
        return Err("the user set this keep-warm; ask them to change it".into());
    }
    let interval = spec
        .interval_minutes
        .or(current.as_ref().map(|e| e.interval_minutes))
        .unwrap_or(DEFAULT_INTERVAL_MINUTES);
    if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&interval) {
        return Err(format!(
            "interval_minutes must be {MIN_INTERVAL_MINUTES}-{MAX_INTERVAL_MINUTES}: under the one-hour cache TTL"
        ));
    }
    let max_idle = spec
        .max_idle_hours
        .or(current.as_ref().map(|e| e.max_idle_hours))
        .unwrap_or(DEFAULT_MAX_IDLE_HOURS);
    if !(1..=MAX_IDLE_HOURS_CEILING).contains(&max_idle) {
        return Err(format!("max_idle_hours must be 1-{MAX_IDLE_HOURS_CEILING}"));
    }
    let mut entry = current.unwrap_or_else(|| KeepWarm {
        owner: owner.to_string(),
        owner_kind: kind,
        session_id: crate::agent_wakes::session_for(owner, kind),
        enabled: true,
        interval_minutes: interval,
        max_idle_hours: max_idle,
        set_by: setter,
        updated_at: now,
        last_warm_at: None,
        last_cache_read_tokens: None,
        last_cache_creation_tokens: None,
        skipped_reason: None,
        skipped_at: None,
        provider_reason: None,
        warms: 0,
    });
    entry.enabled = spec.enabled.unwrap_or(entry.enabled);
    entry.interval_minutes = interval;
    entry.max_idle_hours = max_idle;
    entry.set_by = setter;
    entry.updated_at = now;
    entry.skipped_at = None;
    entry.provider_reason = None;
    entry.skipped_reason = None;
    put(root, &entry)?;
    Ok(entry)
}

/// Note a timing skip, or clear a stale one on a wait. Returns whether the
/// entry changed, so the tick writes only then.
pub fn note_decision(entry: &mut KeepWarm, decision: &Decision) -> bool {
    let shown = match decision {
        Decision::Skip(reason) => Some(entry.provider_reason.clone().unwrap_or_else(|| reason.clone())),
        Decision::Wait if entry.skipped_at.is_none() => entry.provider_reason.clone(),
        _ => return false,
    };
    if entry.skipped_reason == shown {
        return false;
    }
    entry.skipped_reason = shown;
    true
}

/// Owner's quiet hours: its wake schedule's when set, else the user's.
pub fn quiet_now(root: &Path, owner: &str, now: i64) -> bool {
    let at = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default();
    if let Some(wake) = crate::agent_wakes::get(root, owner).ok().flatten().and_then(|j| j.wake)
        && wake.quiet_start.is_some()
    {
        return crate::mission_autonomy::quiet_at(
            wake.quiet_start.as_deref(),
            wake.quiet_end.as_deref(),
            crate::agent_wakes::policy_offset(&wake),
            at,
        );
    }
    let settings = crate::mission_autonomy::load().settings;
    crate::mission_autonomy::quiet_at(
        settings.quiet_start.as_deref(),
        settings.quiet_end.as_deref(),
        crate::mission_autonomy::offset_or_local(settings.utc_offset_minutes),
        at,
    )
}

pub fn view(entry: &KeepWarm) -> Value {
    let mut value = serde_json::to_value(entry).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.remove("skipped_at");
        object.insert(
            "last_warm_at_local".into(),
            entry
                .last_warm_at
                .map(|t| crate::recurring::fmt_local_time(t as u64))
                .into(),
        );
    }
    value
}

pub fn limits() -> Value {
    json!({
        "default_interval_minutes": DEFAULT_INTERVAL_MINUTES,
        "min_interval_minutes": MIN_INTERVAL_MINUTES,
        "max_interval_minutes": MAX_INTERVAL_MINUTES,
        "default_max_idle_hours": DEFAULT_MAX_IDLE_HOURS,
        "max_idle_hours_ceiling": MAX_IDLE_HOURS_CEILING,
    })
}

#[derive(Deserialize)]
struct ToolArgs {
    action: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(flatten)]
    spec: KeepWarmSpec,
}

pub struct KeepWarmTool;

#[async_trait]
impl Tool for KeepWarmTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "keep_warm".into(),
            description: "Show, set or clear prompt-cache keep-warm: while you are idle, a minimal request just under the hour keeps the provider cache alive, up to max_idle_hours, outside quiet hours. Only providers with a one-hour cache are warmed; show says why otherwise. Mission Control may pass agent_id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["show", "set", "clear"]},
                    "agent_id": {"type": "string", "description": "Mission Control only."},
                    "enabled": {"type": "boolean"},
                    "interval_minutes": {"type": "integer", "minimum": MIN_INTERVAL_MINUTES, "maximum": MAX_INTERVAL_MINUTES},
                    "max_idle_hours": {"type": "integer", "minimum": 1, "maximum": MAX_IDLE_HOURS_CEILING}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ToolArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let named = args.agent_id.as_deref().map(str::trim).filter(|a| !a.is_empty());
        let (owner, setter) = match (ctx.is_mission_control(), named) {
            (true, Some(other)) => (other.to_string(), Setter::MissionControl),
            (true, None) => (crate::mission_control::SESSION_ID.to_string(), Setter::Owner),
            (false, Some(_)) => return Err(ToolError::msg("only Mission Control can manage another agent's keep-warm")),
            (false, None) => (
                ctx.agent_id()
                    .filter(|a| *a != crate::coordination::SNIPPET_AGENT_ID)
                    .or(ctx.durable_session_id())
                    .ok_or_else(|| ToolError::msg("this session has no durable identity"))?
                    .to_string(),
                Setter::Owner,
            ),
        };
        let root = crate::recurring::default_root();
        match args.action.as_str() {
            "show" => Ok(ToolResult::success(match get(&root, &owner).map_err(ToolError::msg)? {
                Some(entry) => view(&entry),
                None => json!({"owner": owner, "keep_warm": null}),
            })),
            "clear" => {
                let cleared = clear(&root, &owner, setter).map_err(ToolError::msg)?;
                Ok(ToolResult::success(json!({"owner": owner, "cleared": cleared})))
            }
            "set" => {
                let store = crate::recurring::store_at(&root).map_err(ToolError::msg)?;
                let kind = crate::agent_wakes::resolve_owner(&store, &owner).map_err(ToolError::msg)?;
                let now = chrono::Utc::now().timestamp();
                let entry = set(&root, &owner, kind, args.spec, setter, now).map_err(ToolError::msg)?;
                Ok(ToolResult::success(view(&entry)))
            }
            other => Err(ToolError::msg(format!("unknown action `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_793_451_600;

    fn entry() -> KeepWarm {
        KeepWarm {
            owner: "researcher".into(),
            owner_kind: OwnerKind::Agent,
            session_id: "inbox-researcher".into(),
            enabled: true,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            max_idle_hours: DEFAULT_MAX_IDLE_HOURS,
            set_by: Setter::User,
            updated_at: NOW,
            last_warm_at: None,
            last_cache_read_tokens: None,
            last_cache_creation_tokens: None,
            skipped_reason: None,
            skipped_at: None,
            provider_reason: None,
            warms: 0,
        }
    }

    #[test]
    fn warms_just_before_the_hour_and_not_before() {
        let e = entry();
        assert_eq!(decide(&e, NOW, Some(NOW - 30 * 60), false, false), Decision::Wait);
        assert_eq!(decide(&e, NOW, Some(NOW - 55 * 60), false, false), Decision::Warm);
        assert_eq!(decide(&e, NOW, Some(NOW - 55 * 60), false, true), Decision::Wait);
    }

    #[test]
    fn a_warm_resets_the_clock_until_max_idle() {
        let mut e = entry();
        e.last_warm_at = Some(NOW - 10 * 60);
        assert_eq!(decide(&e, NOW, Some(NOW - 3 * 3600), false, false), Decision::Wait);
        e.last_warm_at = Some(NOW - 56 * 60);
        assert_eq!(decide(&e, NOW, Some(NOW - 3 * 3600), false, false), Decision::Warm);
        assert!(matches!(
            decide(&e, NOW, Some(NOW - 6 * 3600), false, false),
            Decision::Skip(r) if r.contains("idle over 6h")
        ));
    }

    #[test]
    fn lapsed_caches_quiet_hours_and_disabled_entries_are_skipped() {
        let mut e = entry();
        assert!(matches!(decide(&e, NOW, Some(NOW - 70 * 60), false, false), Decision::Skip(r) if r.contains("lapsed")));
        assert_eq!(decide(&e, NOW, Some(NOW - 55 * 60), true, false), Decision::Skip("quiet hours".into()));
        assert!(matches!(decide(&e, NOW, None, false, false), Decision::Skip(_)));
        e.enabled = false;
        assert_eq!(decide(&e, NOW, Some(NOW - 55 * 60), false, false), Decision::Skip("disabled".into()));
    }

    #[test]
    fn an_unsupported_skip_is_not_retried_every_tick() {
        let mut e = entry();
        record_outcome(&mut e, &Outcome::Skipped("no hour cache".into()), NOW - 60);
        assert_eq!(decide(&e, NOW, Some(NOW - 56 * 60), false, false), Decision::Wait);
        assert_eq!(e.skipped_reason.as_deref(), Some("no hour cache"));
    }

    #[test]
    fn a_provider_reason_outranks_timing_reasons_until_a_warm() {
        let mut e = entry();
        record_outcome(&mut e, &Outcome::Unsupported("5-minute cache".into()), NOW);
        assert!(!note_decision(&mut e, &Decision::Skip("cache already lapsed".into())));
        assert_eq!(e.skipped_reason.as_deref(), Some("5-minute cache"));
        record_outcome(&mut e, &Outcome::Warmed(TokenUsage::default()), NOW);
        assert!(e.provider_reason.is_none());
        assert!(note_decision(&mut e, &Decision::Skip("quiet hours".into())));
        assert!(note_decision(&mut e, &Decision::Wait));
        assert!(e.skipped_reason.is_none());
    }

    #[test]
    fn a_warm_records_the_cache_read() {
        let mut e = entry();
        let usage = TokenUsage { cache_read_tokens: 41_200, cache_creation_tokens: 90, ..TokenUsage::default() };
        record_outcome(&mut e, &Outcome::Warmed(usage), NOW);
        assert_eq!(e.last_warm_at, Some(NOW));
        assert_eq!(e.last_cache_read_tokens, Some(41_200));
        assert_eq!(e.warms, 1);
        assert!(e.skipped_reason.is_none());
    }

    #[test]
    fn providers_without_an_hour_cache_say_why() {
        assert!(reason_for("anthropic").is_none());
        assert!(reason_for("claude-code").unwrap().contains("CLI"));
        assert!(reason_for("gemini").unwrap().contains("30-minute"));
        assert!(reason_for("chatgpt").unwrap().contains("automatic"));
        assert_eq!(reason_for("mercury"), Some(NO_HOUR_CACHE));
    }

    #[tokio::test]
    async fn the_session_loop_answers_a_registered_ping() {
        let rx = register("kw-test");
        complete("kw-test", Outcome::Skipped("x".into()));
        assert!(matches!(rx.await, Ok(Outcome::Skipped(r)) if r == "x"));
    }

    #[test]
    fn set_validates_and_guards_user_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let e = set(root, "researcher", OwnerKind::Agent, KeepWarmSpec::default(), Setter::User, NOW).unwrap();
        assert_eq!((e.interval_minutes, e.max_idle_hours, e.enabled), (55, 6, true));
        assert_eq!(e.session_id, "inbox-researcher");
        let bad = KeepWarmSpec { interval_minutes: Some(60), ..KeepWarmSpec::default() };
        assert!(set(root, "researcher", OwnerKind::Agent, bad, Setter::User, NOW).is_err());
        assert!(
            set(root, "researcher", OwnerKind::Agent, KeepWarmSpec::default(), Setter::MissionControl, NOW).is_err()
        );
        assert!(clear(root, "researcher", Setter::MissionControl).is_err());
        assert!(clear(root, "researcher", Setter::User).unwrap());
        assert!(get(root, "researcher").unwrap().is_none());
    }
}
