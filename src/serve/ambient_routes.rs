use super::*;

use axum::body::Bytes;

const MAX_SEGMENT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct SettingsReq {
    enabled: Option<bool>,
    mode: Option<String>,
}

fn settings_view(d: &Daemon) -> serde_json::Value {
    let (ambient, key_set) = {
        let c = d.config.lock().unwrap();
        (
            c.ambient.clone(),
            c.assemblyai_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
        )
    };
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
        }
        if let Some(v) = req.mode {
            ambient.mode = v;
        }
        match ambient.normalized() {
            Err(e) => Err((StatusCode::BAD_REQUEST, e)),
            Ok(ambient) => {
                c.ambient = ambient;
                crate::config::save_config(&c, &d.config_path)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
            }
        }
    };
    match result {
        Ok(()) => Json(settings_view(&d)).into_response(),
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
    }))
}

const COLUMNS: &str = "e.id, e.device, e.started_at, e.duration_ms, e.status, e.text, e.error";

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
        let result = super::transcribe::transcribe_ambient(&d, &path).await;
        let (status, text, error) = match result {
            Ok(text) if text.is_empty() => ("empty", String::new(), None),
            Ok(text) => ("done", text, None),
            Err(e) => ("failed", String::new(), Some(e)),
        };
        let _ = d.store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_events SET status = ?2, text = ?3, error = ?4 WHERE id = ?1",
                rusqlite::params![id, status, text, error],
            )?;
            if status == "done" {
                c.execute(
                    "INSERT INTO ambient_events_fts(rowid, text)
                     SELECT rowid, text FROM ambient_events WHERE id = ?1",
                    [&id],
                )?;
            }
            Ok(())
        });
        if status != "failed" {
            let _ = tokio::fs::remove_file(&path).await;
        }
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
