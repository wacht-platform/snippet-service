use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::store::Store;

const LEASE_TTL: Duration = Duration::from_secs(45);

static LAST_USER_TURN: AtomicI64 = AtomicI64::new(0);
static PINNED_ACTIVE: AtomicBool = AtomicBool::new(false);

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

pub fn note_user_turn() {
    LAST_USER_TURN.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
}

pub fn note_pinned(active: bool) {
    PINNED_ACTIVE.store(active, Ordering::Relaxed);
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
                    let id = r.get::<_, i64>(0)?;
                    Ok(json!({
                        "audio": crate::tts::audio_dir().join(format!("{id}.wav")).exists(),
                        "id": id,
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
    let ambient = crate::config::SnippetConfig::load(crate::config::default_config_path())
        .await
        .map(|c| c.ambient)
        .unwrap_or_default();
    let conversation = open_conversation("mission-control", ambient.conversation_timeout_secs);
    let id = record_turn(&conversation.id, "assistant", text, &device)?;
    let mut audio = false;
    if ambient.voice != "system" {
        match crate::tts::synthesize(&ambient.voice, text).await {
            Ok(bytes) => {
                let dir = crate::tts::audio_dir();
                if tokio::fs::create_dir_all(&dir).await.is_ok()
                    && tokio::fs::write(dir.join(format!("{id}.wav")), bytes).await.is_ok()
                {
                    audio = true;
                }
            }
            Err(e) => eprintln!("[ambient] server voice failed: {e}"),
        }
    }
    crate::session::emit_device_event(json!({
        "kind": "ambient_speech",
        "id": id,
        "conversation_id": conversation.id,
        "device": device,
        "text": text,
        "audio": audio,
    }));
    Ok(json!({"spoken": true, "turn": id, "voice": if audio { ambient.voice.as_str() } else { "phone" }}))
}

pub fn prompt_line() -> String {
    let holder = lease_holder();
    match (&holder, conversation()) {
        (Some((_, label)), Some(c)) => format!(
            " A live spoken conversation is open with the user on {label} (about {}s left). Their words arrive as <ambient_turn> messages; answer every one by calling speak with one to three plain sentences (it is read aloud, so no markdown or lists). Anything you only write in your reply is not heard.",
            c.seconds_left()
        ),
        (Some(_), None) if PINNED_ACTIVE.load(Ordering::Relaxed) => {
            let last = LAST_USER_TURN.load(Ordering::Relaxed);
            let idle = (chrono::Utc::now().timestamp() - last) / 60;
            if last == 0 || idle >= 5 {
                " Ambient listening is pinned to active but the user has not spoken to you for a while; if they are just typing, call ambient_passive so it stops listening for you.".to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
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

pub async fn set_passive() -> Result<(), String> {
    let path = crate::config::default_config_path();
    let mut config = crate::config::SnippetConfig::load(&path)
        .await
        .map_err(|e| e.to_string())?;
    config.ambient.mode = "passive".into();
    crate::config::save_config(&config, &path)?;
    note_pinned(false);
    end_conversation();
    Ok(())
}
