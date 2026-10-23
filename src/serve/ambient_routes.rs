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
    voice: Option<String>,
    typesafe_api_key: Option<String>,
    conversation_timeout_secs: Option<u32>,
    wake_phrases: Option<Vec<String>>,
}

fn settings_view(d: &Daemon) -> serde_json::Value {
    let (ambient, key_set, watcher_key_set) = {
        let c = d.config.lock().unwrap();
        (
            c.ambient.clone(),
            c.assemblyai_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            c.typesafe_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
        )
    };
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
        "key_set": key_set,
        "devices": devices,
        "effective_mode": crate::ambient::effective_mode_name(ambient.enabled && ambient.mode == "active"),
        "watcher": ambient.watcher,
        "watcher_chat_context": ambient.watcher_chat_context,
        "voice": ambient.voice,
        "voices": crate::tts::VOICES.iter().map(|(id, name)| serde_json::json!({"id": id, "name": name})).collect::<Vec<_>>(),
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
        if let Some(v) = req.voice {
            ambient.voice = v;
        }
        if let Some(v) = req.conversation_timeout_secs {
            ambient.conversation_timeout_secs = v;
        }
        if let Some(v) = req.wake_phrases {
            ambient.wake_phrases = v;
        }
        match ambient.normalized() {
            Err(e) => Err((StatusCode::BAD_REQUEST, e)),
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
            if ambient.enabled && ambient.voice != "system" {
                let voice = ambient.voice.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::tts::prepare(&voice).await {
                        eprintln!("[ambient] could not prepare voice {voice}: {e}");
                    }
                });
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
    if !lease_touch(&q.device, None, false) {
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
            "INSERT OR IGNORE INTO ambient_events (id, device, started_at, duration_ms, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            rusqlite::params![q.id, q.device, started, q.duration_ms, now],
        )
    });
    if let Err(e) = inserted {
        let _ = tokio::fs::remove_file(&path).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("could not record event: {e}")).into_response();
    }
    spawn_transcription(d.clone(), q.id.clone());
    Json(fetch(&d, &q.id).unwrap_or_else(|| serde_json::json!({"id": q.id, "status": "pending"})))
        .into_response()
}

fn spawn_transcription(d: Shared, id: String) {
    tokio::spawn(async move {
        let path = audio_path(&id);
        static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
        let gate = GATE
            .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_TRANSCRIPTIONS)))
            .clone();
        let permit = gate.acquire_owned().await.ok();
        let result = super::transcribe::transcribe_ambient(&d, &path).await;
        drop(permit);
        let (status, text, error) = match result {
            Ok(text) if text.is_empty() => ("empty", String::new(), None),
            Ok(text) => ("done", text, None),
            Err(e) => ("failed", String::new(), Some(e)),
        };
        let _ = d.store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_events SET status = ?2, text = ?3, error = ?4,
                     kept = CASE WHEN ?2 = 'done' THEN 0 ELSE 1 END WHERE id = ?1",
                rusqlite::params![id, status, text, error],
            )?;
            Ok(())
        });
        if status != "failed" {
            let _ = tokio::fs::remove_file(&path).await;
        }
        if status != "done" {
            return;
        }
        let (verdict, delivered) = super::ambient_watch::process(&d, &id, &text).await;
        let kept = verdict.kept();
        let _ = d.store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_events SET verdict = ?2, addressed = ?3, judge = ?4, kept = ?5, intent = ?6
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    verdict.kind,
                    verdict.addressed,
                    verdict.judge,
                    kept as i64,
                    verdict.intent
                ],
            )?;
            if kept {
                c.execute(
                    "INSERT INTO ambient_events_fts(rowid, text)
                     SELECT rowid, text FROM ambient_events WHERE id = ?1",
                    [&id],
                )?;
            }
            Ok(())
        });
        super::ambient_watch::act(&d, &id, &text, &verdict, delivered).await;
    });
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
    match crate::ambient::turns_after(q.after, &q.device) {
        Ok(turns) => Json(serde_json::json!({ "turns": turns })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct PlayedReq {
    id: i64,
}

pub(crate) async fn played(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<PlayedReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    crate::ambient::mark_played(req.id);
    Json(serde_json::json!({"ok": true})).into_response()
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

pub(crate) fn warm_voice(d: Shared) {
    tokio::spawn(async move {
        crate::tts::clean_old_replies().await;
        let (enabled, voice) = {
            let c = d.config.lock().unwrap();
            (c.ambient.enabled, c.ambient.voice.clone())
        };
        if enabled && voice != "system" {
            if let Err(e) = crate::tts::prepare(&voice).await {
                eprintln!("[ambient] could not prepare voice {voice}: {e}");
            }
        }
    });
}
