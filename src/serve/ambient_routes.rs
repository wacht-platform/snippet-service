use super::*;

use axum::body::Bytes;

const MAX_SEGMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSCRIPTIONS: usize = 4;
use crate::ambient::{lease_clear, lease_holder, lease_release, lease_touch};

#[derive(Deserialize)]
pub(crate) struct LeaseReq {
    device: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    claim: bool,
    #[serde(default)]
    release: bool,
}

pub(crate) async fn lease(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<LeaseReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.device.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "device is required").into_response();
    }
    if req.release {
        lease_release(&req.device);
        return Json(serde_json::json!({"held": false, "holder": serde_json::Value::Null})).into_response();
    }
    let held = lease_touch(&req.device, Some(&req.label), req.claim);
    let holder = lease_holder();
    Json(serde_json::json!({
        "held": held,
        "holder": holder.as_ref().map(|(_, l)| l),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct SettingsReq {
    enabled: Option<bool>,
    mode: Option<String>,
    watcher: Option<String>,
    watcher_chat_context: Option<bool>,
    typesafe_api_key: Option<String>,
    conversation_timeout_secs: Option<u32>,
    wake_phrases: Option<Vec<String>>,
}

fn settings_view(d: &Daemon) -> serde_json::Value {
    let (ambient, watcher_key_set) = {
        let c = d.config.lock().unwrap();
        (
            c.ambient.clone(),
            c.typesafe_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
        )
    };
    crate::ambient::note_enabled(ambient.enabled);
    crate::ambient::note_pinned(ambient.enabled && ambient.mode == "active");
    let since = (chrono::Utc::now() - chrono::Duration::minutes(10)).timestamp();
    let devices: Vec<serde_json::Value> = d
        .store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT device, MAX(created_at) FROM ambient_events
                 WHERE created_at >= ?1 GROUP BY device ORDER BY MAX(created_at) DESC",
            )?;
            let rows = stmt
                .query_map([since], |r| {
                    Ok(serde_json::json!({
                        "device": r.get::<_, String>(0)?,
                        "last_seen": r.get::<_, i64>(1)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    serde_json::json!({
        "enabled": ambient.enabled,
        "mode": ambient.mode,
        "devices": devices,
        "effective_mode": crate::ambient::effective_mode_name(ambient.enabled && ambient.mode == "active"),
        "state": crate::ambient::state_name(),
        "sub": crate::ambient::sub_state(),
        "expecting": crate::ambient::expectation().map(|e| serde_json::json!({"kind": e.kind, "about": e.about})),
        "watcher": if ambient.watcher == "jev" { "classifier" } else { ambient.watcher.as_str() },
        "watcher_chat_context": ambient.watcher_chat_context,
        "models_ready": crate::models::is_ready(crate::models::STT_MODEL) && crate::models::is_ready(crate::models::TTS_VOICE) && crate::models::is_ready(crate::models::SPEAKER_MODEL),
        "conversation_timeout_secs": ambient.conversation_timeout_secs,
        "wake_phrases": ambient.wake_phrases,
        "watcher_key_set": watcher_key_set,
        "conversation": crate::ambient::conversation().map(|c| serde_json::json!({
            "id": c.id,
            "opened_by": c.opened_by,
            "seconds_left": c.seconds_left(),
        })),
        "listening_id": lease_holder().map(|(d, _)| d),
        "listening_device": lease_holder().map(|(_, l)| l),
    })
}

pub(crate) async fn get_settings(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    Json(settings_view(&d)).into_response()
}

pub(crate) async fn set_settings(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SettingsReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let result = {
        let mut c = d.config.lock().unwrap();
        let mut ambient = c.ambient.clone();
        if let Some(v) = req.enabled {
            ambient.enabled = v;
            if !v {
                lease_clear();
            }
        }
        if let Some(v) = req.mode {
            if v == "passive" && ambient.mode != "passive" {
                crate::ambient::end_conversation();
            }
            ambient.mode = v;
        }
        if let Some(v) = req.watcher {
            ambient.watcher = v;
        }
        if let Some(v) = req.watcher_chat_context {
            ambient.watcher_chat_context = v;
        }
        if let Some(v) = req.conversation_timeout_secs {
            ambient.conversation_timeout_secs = v;
        }
        if let Some(v) = req.wake_phrases {
            ambient.wake_phrases = v;
        }
        match ambient.normalized() {
            Err(e) => Err((StatusCode::BAD_REQUEST, e)),
            Ok(ambient)
                if ambient.enabled
                    && !(crate::models::is_ready(crate::models::STT_MODEL)
                        && crate::models::is_ready(crate::models::TTS_VOICE)
                        && crate::models::is_ready(crate::models::SPEAKER_MODEL)) =>
            {
                Err((
                    StatusCode::BAD_REQUEST,
                    "Download a speech model, a reply voice and a speaker model in Settings, Voice, before turning ambient listening on".into(),
                ))
            }
            Ok(ambient) => {
                c.ambient = ambient;
                if let Some(key) = req.typesafe_api_key {
                    let key = key.trim().to_string();
                    c.typesafe_api_key = (!key.is_empty()).then_some(key);
                }
                crate::config::save_config(&c, &d.config_path)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
            }
        }
    };
    match result {
        Ok(()) => {
            let ambient = d.config.lock().unwrap().ambient.clone();
            if ambient.enabled {
                warm_models(crate::models::STT_MODEL.to_string(), crate::models::TTS_VOICE.to_string());
            }
            Json(settings_view(&d)).into_response()
        }
        Err((code, e)) => (code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct IngestQuery {
    token: Option<String>,
    id: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    started_at: String,
    #[serde(default)]
    duration_ms: i64,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    continued: bool,
    #[serde(default)]
    during_playback: bool,
    #[serde(default)]
    media: bool,
    #[serde(default)]
    backlog: bool,
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    token: Option<String>,
    from: Option<String>,
    to: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    all: Option<bool>,
}

fn audio_dir() -> PathBuf {
    crate::config::snippet_home().join("ambient").join("audio")
}

fn audio_path(id: &str) -> PathBuf {
    audio_dir().join(format!("{id}.wav"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn event_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "device": row.get::<_, String>(1)?,
        "started_at": row.get::<_, String>(2)?,
        "duration_ms": row.get::<_, i64>(3)?,
        "status": row.get::<_, String>(4)?,
        "text": row.get::<_, String>(5)?,
        "error": row.get::<_, Option<String>>(6)?,
        "verdict": row.get::<_, Option<String>>(7)?,
        "addressed": row.get::<_, Option<f64>>(8)?,
        "kept": row.get::<_, i64>(9)? != 0,
        "intent": row.get::<_, Option<String>>(10)?,
    }))
}

const COLUMNS: &str = "e.id, e.device, e.started_at, e.duration_ms, e.status, e.text, e.error, e.verdict, e.addressed, e.kept, e.intent";

fn fetch(d: &Daemon, id: &str) -> Option<serde_json::Value> {
    d.store
        .with_connection(|c| {
            c.query_row(
                &format!("SELECT {COLUMNS} FROM ambient_events e WHERE e.id = ?1"),
                [id],
                event_json,
            )
        })
        .ok()
}

pub(crate) async fn ingest(
    State(d): State<Shared>,
    Query(q): Query<IngestQuery>,
    body: Bytes,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    d.reload_config().await;
    if !d.config.lock().unwrap().ambient.enabled {
        return (StatusCode::FORBIDDEN, "ambient listening is turned off on the server").into_response();
    }
    if q.device.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "device is required").into_response();
    }
    if !q.backlog && !lease_touch(&q.device, None, false) {
        return (StatusCode::CONFLICT, "another device is listening").into_response();
    }
    if !valid_id(&q.id) {
        return (StatusCode::BAD_REQUEST, "id must be 1-64 letters, digits or dashes").into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty audio").into_response();
    }
    if let Some(existing) = fetch(&d, &q.id) {
        return Json(existing).into_response();
    }
    let started = if q.started_at.is_empty() {
        chrono::Utc::now().to_rfc3339()
    } else {
        q.started_at.clone()
    };
    if let Err(e) = tokio::fs::create_dir_all(audio_dir()).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not store audio: {e}")).into_response();
    }
    let path = audio_path(&q.id);
    if let Err(e) = tokio::fs::write(&path, &body).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not store audio: {e}")).into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let inserted = d.store.with_connection(|c| {
        c.execute(
            "INSERT OR IGNORE INTO ambient_events
                 (id, device, started_at, duration_ms, status, created_at, seq, continued, during_playback, routed, media)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                q.id,
                q.device,
                started,
                q.duration_ms,
                now,
                q.seq,
                q.continued as i64,
                q.during_playback as i64,
                !q.backlog as i64,
                q.media as i64
            ],
        )
    });
    let fresh = matches!(inserted, Ok(1));
    if let Err(e) = inserted {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not record event: {e}")).into_response();
    }
    if fresh {
        spawn_transcription(d.clone(), q.id.clone());
    }
    Json(fetch(&d, &q.id).unwrap_or_else(|| serde_json::json!({"id": q.id, "status": "pending"})))
        .into_response()
}

type Early = tokio::task::JoinHandle<Result<super::transcribe::Spoken, String>>;

pub(super) fn precomputed() -> &'static std::sync::Mutex<std::collections::HashMap<String, Early>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Early>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

pub(super) fn gate() -> std::sync::Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_TRANSCRIPTIONS))).clone()
}

fn spawn_transcription(d: Shared, id: String) {
    tokio::spawn(async move {
        let path = audio_path(&id);
        let early = precomputed().lock().unwrap().remove(&id);
        let result = match early {
            Some(handle) => match handle.await {
                Ok(r) => r,
                Err(e) => Err(e.to_string()),
            },
            None => {
                let permit = gate().acquire_owned().await.ok();
                let r = super::transcribe::transcribe_ambient(&d, &path).await;
                drop(permit);
                r
            }
        };
        let speaker_is_you = result.as_ref().is_ok_and(|s| s.summary == "you");
        let (status, text, error, speakers, summary) = match result {
            Ok(s) if s.text.is_empty() => ("empty", String::new(), None, None, None),
            Ok(s) => ("done", s.text, None, Some(s.speakers), Some(s.summary)),
            Err(e) => ("failed", String::new(), Some(e), None, None),
        };
        let _ = d.store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_events SET status = ?2, text = ?3, error = ?4,
                     kept = CASE WHEN ?2 = 'done' THEN 0 ELSE 1 END,
                     speakers = ?5, speaker_summary = ?6 WHERE id = ?1",
                rusqlite::params![id, status, text, error, speakers, summary],
            )?;
            Ok(())
        });
        if status != "failed" {
            let _ = tokio::fs::remove_file(&path).await;
        }
        let row: Option<(String, Option<i64>, String, i64, i64, i64, i64)> = d
            .store
            .with_connection(|c| {
                c.query_row(
                    "SELECT device, seq, started_at, duration_ms, continued, during_playback, routed
                     FROM ambient_events WHERE id = ?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
            })
            .ok();
        let Some((device, seq, started_at, duration_ms, continued, during_playback, routed)) = row else {
            return;
        };
        let seq = seq.unwrap_or(-1);
        if status != "done" {
            super::ambient_router::submit(&d, &device, super::ambient_router::Msg::Skip(seq));
            return;
        }
        let spoke_then = during_playback != 0 || assistant_was_speaking(&device, &started_at, duration_ms);
        if spoke_then && echo_of_reply(&text, &device, if during_playback != 0 { 0.6 } else { 0.8 }) {
            let _ = d.store.with_connection(|c| {
                c.execute("UPDATE ambient_events SET status = 'echo', kept = 0 WHERE id = ?1", [&id])?;
                Ok(())
            });
            super::ambient_router::submit(&d, &device, super::ambient_router::Msg::Skip(seq));
            return;
        }
        let media: i64 = d
            .store
            .with_connection(|c| c.query_row("SELECT media FROM ambient_events WHERE id = ?1", [&id], |r| r.get(0)))
            .unwrap_or(0);
        if media != 0 && !speaker_is_you {
            let _ = d.store.with_connection(|c| {
                c.execute("UPDATE ambient_events SET status = 'media', kept = 0 WHERE id = ?1", [&id])?;
                Ok(())
            });
            super::ambient_router::submit(&d, &device, super::ambient_router::Msg::Skip(seq));
            return;
        }
        if routed == 0 {
            let _ = d.store.with_connection(|c| {
                c.execute("UPDATE ambient_events SET kept = 1 WHERE id = ?1", [&id])?;
                c.execute(
                    "INSERT INTO ambient_events_fts(rowid, text) SELECT rowid, text FROM ambient_events WHERE id = ?1",
                    [&id],
                )?;
                Ok(())
            });
            return;
        }
        let started = chrono::DateTime::parse_from_rfc3339(&started_at)
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
        let lines = speakers
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok())
            .unwrap_or_default();
        super::ambient_router::submit(
            &d,
            &device,
            super::ambient_router::Msg::Done(super::ambient_router::Part {
                id: id.clone(),
                seq,
                text: text.clone(),
                started,
                ended: started + duration_ms as f64 / 1000.0,
                started_at,
                summary: summary.unwrap_or("unknown").to_string(),
                speakers: lines,
                continued: continued != 0,
            }),
        );
    });
}

const ECHO_STOP: &[&str] = &[
    "a", "an", "the", "i", "you", "me", "my", "it", "is", "are", "was", "to", "of", "and", "or", "so", "in", "on", "that", "this",
    "do", "did", "does", "be", "can", "just", "yes", "no", "ok", "okay", "right", "well", "uh", "um", "oh", "we", "he", "she",
    "they", "what", "how", "if", "for", "with", "at", "as", "but", "not", "have", "has", "had", "will", "would", "could", "your",
    "its", "there", "here", "now", "then",
];

fn spoken_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn echo_of_reply(text: &str, device: &str, ratio: f64) -> bool {
    let heard = spoken_words(text);
    let mut content: Vec<&String> = heard.iter().filter(|w| !ECHO_STOP.contains(&w.as_str())).collect();
    content.sort();
    content.dedup();
    if content.len() < 2 {
        return false;
    }
    let since = chrono::Utc::now().timestamp() - 180;
    let replies: Vec<String> = crate::ambient::store()
        .ok()
        .and_then(|s| {
            s.with_connection(|c| {
                let mut stmt = c.prepare(
                    "SELECT text FROM ambient_turns WHERE role = 'assistant' AND device = ?1 AND created_at >= ?2
                     ORDER BY id DESC LIMIT 3",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![device, since], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .ok()
        })
        .unwrap_or_default();
    let n = if heard.len() >= 5 { 3 } else { 2 };
    replies.iter().any(|reply| {
        let said = spoken_words(reply);
        let shared = content.iter().filter(|w| said.contains(**w)).count();
        shared as f64 / content.len() as f64 >= ratio && heard.windows(n).any(|h| said.windows(n).any(|r| r == h))
    })
}

#[derive(Deserialize)]
pub(crate) struct ActivityReq {
    device: String,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    skip: bool,
}

pub(crate) async fn activity(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ActivityReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if lease_holder().is_some_and(|(h, _)| h == req.device) {
        match (req.skip, req.seq) {
            (true, Some(seq)) if seq > 0 => super::ambient_router::submit(&d, &req.device, super::ambient_router::Msg::Skip(seq)),
            _ => super::ambient_router::submit(&d, &req.device, super::ambient_router::Msg::Activity(req.seq)),
        }
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

pub(super) fn announce(d: &Shared, device: &str, seq: i64, skip: bool) {
    if !lease_holder().is_some_and(|(h, _)| h == device) {
        return;
    }
    if skip && seq > 0 {
        super::ambient_router::submit(d, device, super::ambient_router::Msg::Skip(seq));
    } else {
        super::ambient_router::submit(d, device, super::ambient_router::Msg::Activity(Some(seq)));
    }
}

pub(crate) fn resume(d: Shared) {
    let pending: Vec<String> = d
        .store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT id FROM ambient_events WHERE status = 'pending'")?;
            let ids = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .unwrap_or_default();
    let stale = chrono::Utc::now().timestamp() - 60;
    let _ = d.store.with_connection(|c| c.execute("UPDATE ambient_events SET routed = 0 WHERE status = 'pending' AND created_at < ?1", [stale]));
    for id in pending {
        if audio_path(&id).exists() {
            spawn_transcription(d.clone(), id);
        } else {
            let _ = d.store.with_connection(|c| {
                c.execute(
                    "UPDATE ambient_events SET status = 'failed', error = 'audio file missing' WHERE id = ?1",
                    [&id],
                )
            });
        }
    }
}

pub(crate) async fn list(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500) as i64;
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        s.split_whitespace()
            .map(|w| format!("\"{}\"", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let from = q.from.clone().unwrap_or_default();
    let to = q.to.clone().unwrap_or_default();
    let rows = d.store.with_connection(|c| {
        let mut sql = format!("SELECT {COLUMNS} FROM ambient_events e");
        if search.is_some() {
            sql.push_str(" JOIN ambient_events_fts f ON f.rowid = e.rowid");
        }
        sql.push_str(" WHERE (?1 = '' OR e.started_at >= ?1) AND (?2 = '' OR e.started_at <= ?2)");
        if search.is_some() {
            sql.push_str(" AND ambient_events_fts MATCH ?4");
        }
        if !q.all.unwrap_or(false) {
            sql.push_str(" AND e.kept = 1");
        }
        sql.push_str(" ORDER BY e.started_at DESC LIMIT ?3");
        let mut stmt = c.prepare(&sql)?;
        let rows = match &search {
            Some(s) => stmt
                .query_map(rusqlite::params![from, to, limit, s], event_json)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(rusqlite::params![from, to, limit], event_json)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    });
    match rows {
        Ok(events) => Json(serde_json::json!({ "events": events })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

pub(crate) async fn stats(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let since = (chrono::Utc::now() - chrono::Duration::hours(24)).timestamp();
    let rows = d.store.with_connection(|c| {
        let mut stmt = c.prepare(
            "SELECT status, COUNT(*), COALESCE(SUM(duration_ms), 0) FROM ambient_events
             WHERE created_at >= ?1 GROUP BY status",
        )?;
        let rows = stmt
            .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    });
    match rows {
        Ok(rows) => {
            let mut by_status = serde_json::Map::new();
            let mut total_ms = 0i64;
            let mut total = 0i64;
            for (status, count, ms) in rows {
                by_status.insert(status, serde_json::json!(count));
                total += count;
                total_ms += ms;
            }
            Json(serde_json::json!({
                "segments": total,
                "audio_seconds": total_ms / 1000,
                "by_status": by_status,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

pub(crate) fn body_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(MAX_SEGMENT_BYTES)
}

#[derive(Deserialize)]
pub(crate) struct TurnsQuery {
    token: Option<String>,
    #[serde(default)]
    after: i64,
    #[serde(default)]
    device: String,
}

pub(crate) async fn turns(State(d): State<Shared>, Query(q): Query<TurnsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match crate::speech::turns_after(q.after, &q.device) {
        Ok(turns) => Json(serde_json::json!({ "turns": turns })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct PlayedReq {
    id: i64,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    complete: Option<bool>,
}

pub(crate) async fn played(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<PlayedReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let ttl = d.config.lock().unwrap().ambient.conversation_timeout_secs;
    crate::speech::played(req.id, req.seq, req.complete.unwrap_or(req.seq.is_none()), ttl);
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
pub(crate) struct BargeReq {
    turn: i64,
    #[serde(default = "no_chunk")]
    seq: i64,
    #[serde(default)]
    played_ms: i64,
}

fn no_chunk() -> i64 {
    -1
}

pub(crate) async fn barge(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<BargeReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    crate::speech::barge(req.turn, req.seq, req.played_ms);
    Json(serde_json::json!({"ok": true})).into_response()
}

pub(crate) async fn turn_chunk(
    State(d): State<Shared>,
    axum::extract::Path((id, seq)): axum::extract::Path<(i64, i64)>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match tokio::fs::read(crate::speech::chunk_path(id, seq)).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "audio/L16;rate=24000;channels=1")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "chunk not ready").into_response(),
    }
}

pub(crate) async fn end_conversation(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    crate::ambient::end_conversation();
    Json(settings_view(&d)).into_response()
}

pub(crate) async fn turn_audio(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match tokio::fs::read(crate::tts::audio_dir().join(format!("{id}.wav"))).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "audio/wav")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no audio for this turn").into_response(),
    }
}

fn warm_models(stt: String, voice: String) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::stt::warm(&stt) {
            eprintln!("[ambient] could not load speech model {stt}: {e}");
        }
        if let Err(e) = crate::tts::warm(&voice) {
            eprintln!("[ambient] could not load voice {voice}: {e}");
        }
        if let Err(e) = crate::diarize::warm() {
            eprintln!("[ambient] could not load speaker model: {e}");
        }
    });
}

pub(crate) fn warm_voice(d: Shared) {
    let reaper = d.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            if reaper.config.lock().unwrap().ambient.enabled {
                continue;
            }
            let _ = tokio::task::spawn_blocking(|| {
                let idle = std::time::Duration::from_secs(600);
                crate::stt::unload_idle(idle);
                crate::tts::unload_idle(idle);
                crate::diarize::unload_idle(idle);
            })
            .await;
        }
    });
    tokio::spawn(async move {
        crate::tts::clean_old_replies().await;
        let stt = crate::models::STT_MODEL.to_string();
        let voice = crate::models::TTS_VOICE.to_string();
        let enabled = d.config.lock().unwrap().ambient.enabled;
        if enabled && crate::models::is_ready(&stt) && crate::models::is_ready(&voice) && crate::models::is_ready(crate::models::SPEAKER_MODEL) {
            warm_models(stt, voice);
        }
    });
}

#[derive(Deserialize)]
pub(crate) struct ModelReq {
    id: String,
}

pub(crate) async fn models(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    Json(serde_json::json!({ "models": crate::models::list() })).into_response()
}

pub(crate) async fn model_download(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ModelReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::models::start_download(&req.id) {
        Ok(()) => Json(serde_json::json!({ "models": crate::models::list() })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub(crate) async fn model_delete(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ModelReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    {
        let c = d.config.lock().unwrap();
        if c.ambient.enabled && (crate::models::STT_MODEL == req.id || crate::models::TTS_VOICE == req.id || crate::models::SPEAKER_MODEL == req.id) {
            return (StatusCode::CONFLICT, "turn ambient listening off before removing the model it uses").into_response();
        }
    }
    if let Err(e) = crate::models::delete(&req.id).await {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    Json(serde_json::json!({ "models": crate::models::list() })).into_response()
}

pub(crate) async fn speakers_list(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let list: Vec<serde_json::Value> = crate::diarize::speakers(&d.store)
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "label": crate::diarize::display_name(s),
                "is_user": s.is_user,
                "lines": s.lines,
                "last_seen": s.last_seen,
            })
        })
        .collect();
    Json(serde_json::json!({
        "models_ready": crate::models::is_ready(crate::models::SPEAKER_MODEL),
        "enrolled": crate::diarize::enrolled(&d.store),
        "speakers": list,
    }))
    .into_response()
}

pub(crate) async fn speakers_enroll(State(d): State<Shared>, Query(a): Query<Auth>, body: Bytes) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if !crate::models::is_ready(crate::models::SPEAKER_MODEL) {
        return (StatusCode::BAD_REQUEST, "Download the speaker model first").into_response();
    }
    let path = std::env::temp_dir().join(format!("snippet-enrol-{}", uuid::Uuid::new_v4()));
    if let Err(e) = tokio::fs::write(&path, &body).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let worker = d.clone();
    let file = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let samples = crate::diarize::read_audio_16k(&file)?;
        crate::diarize::enroll(&worker.store, &samples)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let _ = tokio::fs::remove_file(&path).await;
    match result {
        Ok(seconds) => Json(serde_json::json!({"enrolled": true, "seconds": seconds})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub(crate) async fn speakers_unenroll(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::diarize::unenroll(&d.store) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(crate) async fn speakers_clear(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::diarize::clear(&d.store) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct SpeakerLabelReq {
    id: i64,
    label: String,
}

#[derive(Deserialize)]
pub(crate) struct SpeakerClaimReq {
    id: i64,
}

pub(crate) async fn speakers_claim(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SpeakerClaimReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::diarize::claim(&d.store, req.id, true) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) if e == "no such speaker" => (StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(crate) async fn speakers_label(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<SpeakerLabelReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match crate::diarize::rename(&d.store, req.id, &req.label) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn assistant_was_speaking(device: &str, started_at: &str, duration_ms: i64) -> bool {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return false;
    };
    let start = t.timestamp();
    let end = start + duration_ms / 1000;
    crate::ambient::store()
        .ok()
        .and_then(|st| {
            st.with_connection(|c| {
                let mut stmt = c.prepare(
                    "SELECT t.created_at, COALESCE((SELECT SUM(ms) FROM ambient_chunks WHERE turn_id = t.id), 0)
                     FROM ambient_turns t WHERE t.role = 'assistant' AND t.device = ?1 AND t.created_at >= ?2
                     ORDER BY t.id DESC LIMIT 4",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![device, start - 90], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .ok()
        })
        .is_some_and(|rows| rows.iter().any(|(at, ms)| *at <= end && at + ms / 1000 + 4 >= start))
}
