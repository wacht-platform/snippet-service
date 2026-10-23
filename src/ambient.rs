use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::store::Store;

const LEASE_TTL: Duration = Duration::from_secs(45);

static LEASE: Mutex<Option<(String, String, Instant)>> = Mutex::new(None);
static CONVERSATION: Mutex<Option<Conversation>> = Mutex::new(None);

#[derive(Clone)]
pub struct Conversation {
    pub id: String,
    pub opened_by: String,
    expires: Instant,
}

impl Conversation {
    pub fn seconds_left(&self) -> u64 {
        self.expires.saturating_duration_since(Instant::now()).as_secs()
    }
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
    end_conversation();
}

pub fn lease_clear() {
    *LEASE.lock().unwrap() = None;
    end_conversation();
}

pub fn conversation() -> Option<Conversation> {
    let mut conversation = CONVERSATION.lock().unwrap();
    if conversation.as_ref().is_some_and(|c| c.expires <= Instant::now()) {
        *conversation = None;
    }
    conversation.clone()
}

pub fn open_conversation(by: &str, ttl_secs: u32) -> Conversation {
    let ttl = Duration::from_secs(u64::from(ttl_secs));
    let mut conversation = CONVERSATION.lock().unwrap();
    let live = conversation.as_ref().is_some_and(|c| c.expires > Instant::now());
    if !live {
        *conversation = Some(Conversation {
            id: format!("c{}", chrono::Utc::now().timestamp_millis()),
            opened_by: by.to_string(),
            expires: Instant::now() + ttl,
        });
    }
    let c = conversation.as_mut().unwrap();
    c.expires = Instant::now() + ttl;
    c.clone()
}

pub fn end_conversation() {
    *CONVERSATION.lock().unwrap() = None;
}

fn store() -> Result<Store, String> {
    Store::open_cached(crate::store::default_db_path()).map_err(|e| e.to_string())
}

pub fn record_turn(conversation_id: &str, role: &str, text: &str, device: &str) -> Result<i64, String> {
    store()?
        .with_connection(|c| {
            c.execute(
                "INSERT INTO ambient_turns (conversation_id, role, text, device, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![conversation_id, role, text, device, chrono::Utc::now().timestamp()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .map_err(|e| e.to_string())
}

pub fn turns_after(after: i64, device: &str) -> Result<Vec<Value>, String> {
    store()?
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, text, created_at FROM ambient_turns
                 WHERE id > ?1 AND role = 'assistant' AND device = ?2 AND played_at IS NULL
                   AND created_at >= ?3
                 ORDER BY id LIMIT 50",
            )?;
            let cutoff = chrono::Utc::now().timestamp() - 120;
            let rows = stmt
                .query_map(rusqlite::params![after, device, cutoff], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "conversation_id": r.get::<_, String>(1)?,
                        "text": r.get::<_, String>(2)?,
                        "created_at": r.get::<_, i64>(3)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .map_err(|e| e.to_string())
}

pub fn mark_played(id: i64) {
    if let Ok(store) = store() {
        let _ = store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET played_at = ?2 WHERE id = ?1",
                rusqlite::params![id, chrono::Utc::now().timestamp()],
            )?;
            Ok(())
        });
    }
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

pub async fn speak(text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to say".into());
    }
    let Some((device, _)) = lease_holder() else {
        return Err("no device is listening, so there is nobody to speak to".into());
    };
    if !effective_active().await {
        return Err("ambient is in passive mode and no conversation is open; use ping_user instead".into());
    }
    let ttl = crate::config::SnippetConfig::load(crate::config::default_config_path())
        .await
        .map(|c| c.ambient.conversation_timeout_secs)
        .unwrap_or(90);
    let conversation = open_conversation("mission-control", ttl);
    let id = record_turn(&conversation.id, "assistant", text, &device)?;
    crate::session::emit_device_event(json!({
        "kind": "ambient_speech",
        "id": id,
        "conversation_id": conversation.id,
        "device": device,
        "text": text,
    }));
    Ok(json!({"spoken": true, "turn": id}))
}

pub fn prompt_line() -> String {
    match (lease_holder(), conversation()) {
        (Some((_, label)), Some(c)) => format!(
            " A live spoken conversation is open with the user on {label} (about {}s left). Their words arrive as <ambient_turn> messages; answer every one by calling speak with one to three plain sentences (it is read aloud, so no markdown or lists). Anything you only write in your reply is not heard.",
            c.seconds_left()
        ),
        _ => String::new(),
    }
}

pub fn search(query: &str, limit: usize) -> Result<Vec<Value>, String> {
    let limit = limit.clamp(1, 50) as i64;
    let terms = query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    store()?
        .with_connection(|c| {
            let map = |r: &rusqlite::Row| {
                Ok(json!({
                    "at": r.get::<_, String>(0)?,
                    "device": r.get::<_, String>(1)?,
                    "text": r.get::<_, String>(2)?,
                }))
            };
            if terms.is_empty() {
                let mut stmt = c.prepare(
                    "SELECT started_at, device, text FROM ambient_events
                     WHERE status = 'done' AND kept = 1 ORDER BY started_at DESC LIMIT ?1",
                )?;
                return stmt.query_map([limit], map)?.collect::<Result<Vec<_>, _>>();
            }
            let mut stmt = c.prepare(
                "SELECT e.started_at, e.device, e.text FROM ambient_events e
                 JOIN ambient_events_fts f ON f.rowid = e.rowid
                 WHERE ambient_events_fts MATCH ?1 AND e.kept = 1
                 ORDER BY e.started_at DESC LIMIT ?2",
            )?;
            stmt.query_map(rusqlite::params![terms, limit], map)?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| e.to_string())
}
