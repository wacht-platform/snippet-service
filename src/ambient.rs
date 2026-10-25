use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::store::Store;

const LEASE_TTL: Duration = Duration::from_secs(45);

static LAST_USER_TURN: AtomicI64 = AtomicI64::new(0);
static PINNED_ACTIVE: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SPOKE_GEN: AtomicU64 = AtomicU64::new(0);
static EXPECT: Mutex<Option<Expectation>> = Mutex::new(None);

static LEASE: Mutex<Option<(String, String, Instant)>> = Mutex::new(None);
static CONVERSATION: Mutex<Option<Conversation>> = Mutex::new(None);

#[derive(Clone)]
pub struct Conversation {
    pub id: String,
    pub opened_by: String,
    expires: Instant,
    hold_until: Option<Instant>,
}

#[derive(Clone)]
pub struct Expectation {
    pub kind: String,
    pub about: String,
    until: Instant,
}

impl Conversation {
    pub fn seconds_left(&self) -> u64 {
        self.expires.saturating_duration_since(Instant::now()).as_secs()
    }
}

pub fn note_user_turn() {
    LAST_USER_TURN.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    *EXPECT.lock().unwrap() = None;
    hold(120);
}

pub fn note_pinned(active: bool) {
    if PINNED_ACTIVE.swap(active, Ordering::Relaxed) != active {
        emit_state("pin changed");
    }
}

pub fn note_enabled(on: bool) {
    if ENABLED.swap(on, Ordering::Relaxed) != on {
        emit_state(if on { "listening started" } else { "listening stopped" });
    }
}

pub fn run_generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub fn note_spoke() {
    SPOKE_GEN.store(GENERATION.load(Ordering::Relaxed), Ordering::Relaxed);
}

pub fn spoke_since(generation: u64) -> bool {
    SPOKE_GEN.load(Ordering::Relaxed) >= generation
}

pub fn cancel_all_speech(reason: &str) {
    crate::speech::cancel_all(reason);
}

pub fn hold(secs: u64) {
    if let Some(c) = CONVERSATION.lock().unwrap().as_mut() {
        c.hold_until = Some(Instant::now() + Duration::from_secs(secs));
    }
}

pub fn playback_finished(ttl_secs: u32) {
    if let Some(c) = CONVERSATION.lock().unwrap().as_mut() {
        c.hold_until = None;
        c.expires = Instant::now() + Duration::from_secs(u64::from(ttl_secs));
    }
}

pub fn set_expectation(kind: &str, about: &str, minutes: u64) {
    *EXPECT.lock().unwrap() = Some(Expectation {
        kind: kind.to_string(),
        about: about.to_string(),
        until: Instant::now() + Duration::from_secs(minutes.clamp(1, 30) * 60),
    });
}

pub fn expectation() -> Option<Expectation> {
    let mut e = EXPECT.lock().unwrap();
    if e.as_ref().is_some_and(|x| x.until <= Instant::now()) {
        *e = None;
    }
    e.clone()
}

pub fn is_pinned() -> bool {
    PINNED_ACTIVE.load(Ordering::Relaxed)
}

pub fn state_name() -> &'static str {
    if !ENABLED.load(Ordering::Relaxed) {
        "off"
    } else if conversation().is_some() {
        "active"
    } else if PINNED_ACTIVE.load(Ordering::Relaxed) {
        "pinned"
    } else {
        "passive"
    }
}

pub fn sub_state() -> &'static str {
    if crate::speech::pending() {
        "speaking"
    } else if CONVERSATION.lock().unwrap().as_ref().is_some_and(|c| c.hold_until.is_some_and(|h| h > Instant::now())) {
        "thinking"
    } else {
        "listening"
    }
}

pub fn state_json(reason: &str) -> Value {
    let device = lease_holder().map(|(d, _)| d).unwrap_or_default();
    json!({
        "kind": "ambient_state",
        "mode": state_name(),
        "sub": sub_state(),
        "reason": reason,
        "device": device,
        "expecting": expectation().map(|e| json!({"kind": e.kind, "about": e.about})),
    })
}

type ModeHook = Box<dyn Fn(&str, &str) + Send + Sync>;
static MODE_HOOK: std::sync::OnceLock<ModeHook> = std::sync::OnceLock::new();
static LAST_MODE: Mutex<String> = Mutex::new(String::new());

pub fn set_mode_hook(hook: ModeHook) {
    let _ = MODE_HOOK.set(hook);
}

pub fn emit_state(reason: &str) {
    crate::session::emit_device_event(state_json(reason));
    let mode = effective_mode_name(is_pinned());
    let changed = {
        let mut last = LAST_MODE.lock().unwrap();
        let changed = !last.is_empty() && *last != mode;
        *last = mode.to_string();
        changed
    };
    if changed && !reason.starts_with("mission_control:") {
        if let Some(hook) = MODE_HOOK.get() {
            hook(mode, reason);
        }
    }
}

pub fn effective_mode_name(pinned_active: bool) -> &'static str {
    if pinned_active || conversation().is_some() { "active" } else { "passive" }
}

pub fn lease_holder() -> Option<(String, String)> {
    let mut lease = LEASE.lock().unwrap();
    if lease.as_ref().is_some_and(|(_, _, at)| at.elapsed() > LEASE_TTL) {
        *lease = None;
    }
    lease.as_ref().map(|(device, label, _)| (device.clone(), label.clone()))
}

pub fn lease_touch(device: &str, label: Option<&str>, claim: bool) -> bool {
    let mut lease = LEASE.lock().unwrap();
    let live = lease.as_ref().is_some_and(|(_, _, at)| at.elapsed() <= LEASE_TTL);
    let other = live && lease.as_ref().is_some_and(|(held, _, _)| held != device);
    if other && !claim {
        return false;
    }
    if other {
        crate::speech::cancel_all("device changed");
    }
    let kept = lease
        .as_ref()
        .filter(|(held, _, _)| held == device)
        .map(|(_, l, _)| l.clone());
    let label = label
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .or(kept)
        .unwrap_or_else(|| device.to_string());
    *lease = Some((device.to_string(), label, Instant::now()));
    true
}

pub fn lease_release(device: &str) {
    let mut lease = LEASE.lock().unwrap();
    if lease.as_ref().is_some_and(|(held, _, _)| held == device) {
        *lease = None;
    }
    drop(lease);
    crate::speech::cancel_all("device released");
    end_conversation();
}

pub fn lease_clear() {
    *LEASE.lock().unwrap() = None;
    crate::speech::cancel_all("listening ended");
    end_conversation();
}

pub fn conversation() -> Option<Conversation> {
    let mut conversation = CONVERSATION.lock().unwrap();
    let now = Instant::now();
    if conversation.as_ref().is_some_and(|c| c.expires <= now && c.hold_until.is_none_or(|h| h <= now)) {
        *conversation = None;
        drop(conversation);
        emit_state("conversation ended");
        return None;
    }
    conversation.clone()
}

pub fn open_conversation(by: &str, ttl_secs: u32) -> Conversation {
    let ttl = Duration::from_secs(u64::from(ttl_secs));
    let mut conversation = CONVERSATION.lock().unwrap();
    let live = conversation.as_ref().is_some_and(|c| c.expires > Instant::now() || c.hold_until.is_some_and(|h| h > Instant::now()));
    if !live {
        *conversation = Some(Conversation {
            id: format!("c{}", chrono::Utc::now().timestamp_millis()),
            opened_by: by.to_string(),
            expires: Instant::now() + ttl,
            hold_until: None,
        });
    }
    let c = conversation.as_mut().unwrap();
    c.expires = Instant::now() + ttl;
    let out = c.clone();
    drop(conversation);
    if !live {
        emit_state("conversation opened");
    }
    out
}

pub fn end_conversation() {
    let had = CONVERSATION.lock().unwrap().take().is_some();
    if had {
        emit_state("conversation ended");
    }
}

pub fn mark(id: &str, stage: &str) {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    let mut map = T.get_or_init(Default::default).lock().unwrap();
    let now = std::time::Instant::now();
    if stage == "end" {
        if map.len() > 64 {
            map.clear();
        }
        map.insert(id.to_string(), now);
    }
    if stage == "deliver" {
        *last().lock().unwrap() = id.to_string();
    }
    let ms = map.get(id).map(|t| now.duration_since(*t).as_millis()).unwrap_or(0);
    eprintln!("[ambient-t] {} {stage} +{ms}ms", &id[..id.len().min(8)]);
}

fn last() -> &'static std::sync::Mutex<String> {
    static L: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();
    L.get_or_init(Default::default)
}

pub fn mark_last(stage: &str) {
    let id = last().lock().unwrap().clone();
    if !id.is_empty() {
        mark(&id, stage);
    }
}

pub(crate) fn store() -> Result<Store, String> {
    Store::open_cached(crate::store::default_db_path()).map_err(|e| e.to_string())
}

pub fn record_turn(conversation_id: &str, role: &str, text: &str, device: &str) -> Result<i64, String> {
    record_turn_status(conversation_id, role, text, device, "played")
}

pub fn record_turn_status(conversation_id: &str, role: &str, text: &str, device: &str, status: &str) -> Result<i64, String> {
    store()?
        .with_connection(|c| {
            c.execute(
                "INSERT INTO ambient_turns (conversation_id, role, text, device, created_at, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![conversation_id, role, text, device, chrono::Utc::now().timestamp(), status],
            )?;
            Ok(c.last_insert_rowid())
        })
        .map_err(|e| e.to_string())
}

pub async fn effective_active() -> bool {
    if conversation().is_some() {
        return true;
    }
    crate::config::SnippetConfig::load(crate::config::default_config_path())
        .await
        .map(|c| c.ambient.enabled && c.ambient.mode == "active")
        .unwrap_or(false)
}

pub fn prompt_line() -> String {
    let holder = lease_holder();
    let Some((_, label)) = &holder else {
        return String::new();
    };
    let expecting = expectation()
        .map(|e| format!(" You told the voice classifier to expect: {} about {}.", e.kind, e.about))
        .unwrap_or_default();
    match conversation() {
        Some(c) => format!(
            " Ambient mode is ACTIVE: a live spoken conversation is open with the user on {label} (about {}s left). Nearly everything they say arrives as <ambient_turn mode=\"active\"> messages; answer by calling speak with one to three plain sentences (it is read aloud, so no markdown or lists), or say nothing when a line is not for you. The user hears silence until your first speak call, so make speak your FIRST action on every spoken turn: if the answer needs tools, say a short acknowledgement or the answer you already know first (for example \"Checking now.\"), then run the tools, then speak the result. Never run a tool before the first speak. If speaking is no longer needed, call ambient_mode with passive.{expecting}",
            c.seconds_left()
        ),
        None if PINNED_ACTIVE.load(Ordering::Relaxed) => {
            let last = LAST_USER_TURN.load(Ordering::Relaxed);
            let idle = (chrono::Utc::now().timestamp() - last) / 60;
            if last == 0 || idle >= 5 {
                " Ambient listening is pinned to active but the user has not spoken to you for a while; if they are just typing, call ambient_mode with passive so it stops listening for you.".to_string()
            } else {
                String::new()
            }
        }
        None => format!(
            " Ambient mode is PASSIVE on {label}: speech addressed to you arrives as <ambient_turn mode=\"passive\"> messages; you have no speak tool in this mode, so reply in the chat (the user gets a notification for each reply) and end with one plain line. If a spoken back and forth would help (a question, hands busy), call ambient_mode with active and then speak.{expecting}"
        ),
    }
}

pub fn search(query: &str, limit: usize, include_hidden: bool) -> Result<Vec<Value>, String> {
    let limit = limit.clamp(1, 50) as i64;
    let words: Vec<String> = query.split_whitespace().map(|w| w.replace('"', "")).collect();
    store()?
        .with_connection(|c| {
            let map = |r: &rusqlite::Row| {
                Ok(json!({
                    "at": r.get::<_, String>(0)?,
                    "device": r.get::<_, String>(1)?,
                    "text": r.get::<_, String>(2)?,
                    "judged_as": r.get::<_, Option<String>>(3)?,
                }))
            };
            if include_hidden {
                let mut sql = String::from(
                    "SELECT started_at, device, text, verdict FROM ambient_events
                     WHERE status = 'done' AND text != ''",
                );
                let mut params: Vec<String> = Vec::new();
                for w in &words {
                    params.push(format!("%{}%", w.replace('%', "").replace('_', "")));
                    sql.push_str(&format!(" AND text LIKE ?{} COLLATE NOCASE", params.len()));
                }
                sql.push_str(&format!(" ORDER BY started_at DESC LIMIT {limit}"));
                let mut stmt = c.prepare(&sql)?;
                return stmt
                    .query_map(rusqlite::params_from_iter(params.iter()), map)?
                    .collect::<Result<Vec<_>, _>>();
            }
            if words.is_empty() {
                let mut stmt = c.prepare(
                    "SELECT started_at, device, text, verdict FROM ambient_events
                     WHERE status = 'done' AND kept = 1 ORDER BY started_at DESC LIMIT ?1",
                )?;
                return stmt.query_map([limit], map)?.collect::<Result<Vec<_>, _>>();
            }
            let terms = words.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" ");
            let mut stmt = c.prepare(
                "SELECT e.started_at, e.device, e.text, e.verdict FROM ambient_events e
                 JOIN ambient_events_fts f ON f.rowid = e.rowid
                 WHERE ambient_events_fts MATCH ?1 AND e.kept = 1
                 ORDER BY e.started_at DESC LIMIT ?2",
            )?;
            stmt.query_map(rusqlite::params![terms, limit], map)?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| e.to_string())
}

pub async fn set_mode(mode: &str, reason: &str) -> Result<Value, String> {
    let reason = format!("mission_control: {}", reason.trim());
    match mode {
        "active" => {
            if lease_holder().is_none() {
                return Err("no device is listening, so there is nobody to talk to".into());
            }
            let ttl = crate::config::SnippetConfig::load(crate::config::default_config_path())
                .await
                .map(|c| c.ambient.conversation_timeout_secs)
                .unwrap_or(90);
            open_conversation("mission-control", ttl);
            emit_state(&reason);
        }
        "passive" => {
            let path = crate::config::default_config_path();
            let mut config = crate::config::SnippetConfig::load(&path).await.map_err(|e| e.to_string())?;
            let was_pinned = config.ambient.mode == "active";
            if was_pinned {
                config.ambient.mode = "passive".into();
                crate::config::save_config(&config, &path)?;
            }
            PINNED_ACTIVE.store(false, Ordering::Relaxed);
            crate::speech::cancel_all("passive");
            end_conversation();
            emit_state(&reason);
        }
        _ => return Err("mode must be passive or active".into()),
    }
    Ok(json!({"mode": state_name()}))
}
