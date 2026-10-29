use super::*;


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
    if held {
        super::listener::warm(&d, &req.device);
    }
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
    assemblyai_api_key: Option<String>,
    deepgram_api_key: Option<String>,
    deepgram_host: Option<String>,
    tts_voice: Option<String>,
    tts_speed: Option<u32>,
    stt: Option<String>,
}

const DEEPGRAM_HOSTS: &[(&str, &str)] = &[
    ("api.deepgram.com", "Global (US)"),
    ("api.in.deepgram.com", "India"),
    ("api.eu.deepgram.com", "Europe"),
    ("api.au.deepgram.com", "Australia"),
];

const FLUX_VOICES: &[(&str, &str, &str)] = &[
    ("hannah", "American", "Female"), ("kit", "British", "Male"), ("alexis", "American", "Female"),
    ("cliff", "American", "Male"), ("sienna", "American", "Female"), ("cole", "American", "Male"),
    ("brooke", "American", "Female"), ("colin", "British", "Male"), ("gemma", "British", "Female"),
    ("haley", "American", "Female"), ("heather", "American", "Female"), ("miles", "American", "Male"),
    ("sean", "British", "Male"), ("bree", "American", "Female"), ("brittany", "American", "Female"),
    ("bruce", "American", "Male"), ("conor", "British", "Male"), ("donovan", "American", "Male"),
    ("drew", "American", "Male"), ("elise", "American", "Female"), ("jack", "British", "Male"),
    ("kai", "Singaporean", "Male"), ("kelsey", "American", "Female"), ("maeve", "Irish", "Female"),
    ("marcelo", "Filipino", "Male"), ("marcus", "American", "Male"), ("meena", "Indian", "Female"),
    ("meghan", "American", "Female"), ("naveen", "Indian", "Male"), ("paige", "American", "Female"),
    ("priya", "Indian", "Female"), ("rufus", "British", "Male"), ("sharon", "Australian", "Female"),
    ("tanner", "British", "Male"), ("wade", "American", "Male"), ("wes", "American", "Male"),
];

fn valid_voice(v: &str) -> bool {
    v.strip_prefix("flux-")
        .and_then(|rest| rest.strip_suffix("-en"))
        .is_some_and(|name| FLUX_VOICES.iter().any(|(n, _, _)| *n == name))
}

fn settings_view(d: &Daemon) -> serde_json::Value {
    let (ambient, watcher_key_set, c_assembly_key_set, c_deepgram_key_set) = {
        let c = d.config.lock().unwrap();
        (
            c.ambient.clone(),
            c.typesafe_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            has_key(&c.assemblyai_api_key),
            has_key(&c.deepgram_api_key),
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
    let listening = lease_holder();
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
        "assemblyai_key_set": c_assembly_key_set,
        "deepgram_key_set": c_deepgram_key_set,
        "conversation_timeout_secs": ambient.conversation_timeout_secs,
        "deepgram_host": ambient.deepgram_host,
        "stt": ambient.stt,
        "deepgram_hosts": DEEPGRAM_HOSTS.iter().map(|(h, l)| serde_json::json!({"host": h, "label": l})).collect::<Vec<_>>(),
        "tts_voice": if ambient.tts_voice.starts_with("flux-") { ambient.tts_voice.clone() } else { "flux-priya-en".to_string() },
        "tts_speed": ambient.tts_speed,
        "tts_voices": FLUX_VOICES.iter().map(|(n, a, g)| serde_json::json!({"model": format!("flux-{n}-en"), "accent": a, "gender": g})).collect::<Vec<_>>(),
        "wake_phrases": ambient.wake_phrases,
        "watcher_key_set": watcher_key_set,
        "conversation": crate::ambient::conversation().map(|c| serde_json::json!({
            "id": c.id,
            "opened_by": c.opened_by,
            "seconds_left": c.seconds_left(),
        })),
        "listening_id": listening.as_ref().map(|(id, _)| id),
        "listening_device": listening.as_ref().map(|(_, label)| label),
        "voiceprints": ambient.voiceprints.iter().map(|vp| serde_json::json!({
            "id": vp.id,
            "label": vp.label,
            "created_at": vp.created_at,
        })).collect::<Vec<_>>(),
    })
}

pub(crate) async fn get_settings(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    Json(settings_view(&d)).into_response()
}

fn patched_ambient(mut ambient: crate::config::AmbientConfig, req: &SettingsReq) -> (crate::config::AmbientConfig, Option<String>) {
    if let Some(v) = req.enabled {
        ambient.enabled = v;
        if !v {
            lease_clear();
        }
    }
    if let Some(v) = req.mode.clone() {
        if v == "passive" {
            crate::ambient::end_conversation();
        }
        ambient.mode = v;
    }
    if let Some(v) = req.watcher.clone() {
        ambient.watcher = v;
    }
    if let Some(v) = req.watcher_chat_context {
        ambient.watcher_chat_context = v;
    }
    if let Some(v) = req.conversation_timeout_secs {
        ambient.conversation_timeout_secs = v;
    }
    if let Some(v) = req.wake_phrases.clone() {
        ambient.wake_phrases = v;
    }
    if let Some(v) = req.stt.clone() {
        ambient.stt = v;
    }
    let mut invalid = None;
    if let Some(v) = req.deepgram_host.clone() {
        let v = v.trim().to_ascii_lowercase();
        if DEEPGRAM_HOSTS.iter().any(|(h, _)| *h == v) {
            ambient.deepgram_host = v;
        } else {
            invalid = Some(format!("Unknown Deepgram region {v}"));
        }
    }
    if let Some(v) = req.tts_voice.clone() {
        let v = v.trim().to_ascii_lowercase();
        if valid_voice(&v) {
            ambient.tts_voice = v;
        } else {
            invalid = Some(format!("Unknown voice {v}"));
        }
    }
    if let Some(v) = req.tts_speed {
        ambient.tts_speed = v.clamp(80, 150);
    }
    (ambient, invalid)
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
        let (ambient, invalid) = patched_ambient(c.ambient.clone(), &req);
        let next_key = |value: Option<String>, current: &Option<String>| match value {
            Some(key) => Some(key.trim().to_string()).filter(|k| !k.is_empty()),
            None => current.clone(),
        };
        let assemblyai = next_key(req.assemblyai_api_key, &c.assemblyai_api_key);
        let deepgram = next_key(req.deepgram_api_key, &c.deepgram_api_key);
        match invalid.map_or_else(|| ambient.normalized(), Err) {
            Err(e) => Err((StatusCode::BAD_REQUEST, e)),
            Ok(ambient) if ambient.enabled && !has_key(&deepgram) => Err((
                StatusCode::BAD_REQUEST,
                "Add the Deepgram key in Settings → Voice before turning ambient listening on".into(),
            )),
            Ok(ambient) if ambient.enabled && ambient.stt == "assemblyai" && !has_key(&assemblyai) => Err((
                StatusCode::BAD_REQUEST,
                "Add the AssemblyAI key in Settings → Voice, or switch speech recognition to Deepgram Flux".into(),
            )),
            Ok(ambient) => {
                let restart = ambient.stt != c.ambient.stt
                    || ambient.deepgram_host != c.ambient.deepgram_host
                    || assemblyai != c.assemblyai_api_key
                    || deepgram != c.deepgram_api_key;
                if restart {
                    super::listener::restart_all();
                }
                c.ambient = ambient;
                c.assemblyai_api_key = assemblyai;
                c.deepgram_api_key = deepgram;
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
            crate::tts::apply_config(&d.config.lock().unwrap());
            Json(settings_view(&d)).into_response()
        }
        Err((code, e)) => (code, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct Segment {
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

pub(super) async fn store_segment(d: &Shared, q: Segment, wav: &[u8]) -> Result<String, (u16, String)> {
    d.reload_config().await;
    if !d.config.lock().unwrap().ambient.enabled {
        return Err((403, "ambient listening is turned off on the server".into()));
    }
    if q.device.trim().is_empty() {
        return Err((400, "device is required".into()));
    }
    if !q.backlog && !lease_touch(&q.device, None, false) {
        return Err((409, "another device is listening".into()));
    }
    if !valid_id(&q.id) {
        return Err((400, "id must be 1-64 letters, digits or dashes".into()));
    }
    if wav.is_empty() {
        return Err((400, "empty audio".into()));
    }
    let status_of = |e: Option<serde_json::Value>| e.and_then(|v| v["status"].as_str().map(str::to_string));
    if let Some(status) = status_of(fetch(d, &q.id)) {
        return Ok(status);
    }
    let started = if q.started_at.is_empty() { chrono::Utc::now().to_rfc3339() } else { q.started_at.clone() };
    let path = audio_path(&q.id);
    let stored = async {
        tokio::fs::create_dir_all(audio_dir()).await?;
        tokio::fs::write(&path, wav).await
    };
    if let Err(e) = stored.await {
        return Err((500, format!("could not store audio: {e}")));
    }
    let inserted = d.store.with_connection(|c| {
        c.execute(
            "INSERT OR IGNORE INTO ambient_events
                 (id, device, started_at, duration_ms, status, created_at, seq, continued, routed)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8)",
            rusqlite::params![q.id, q.device, started, q.duration_ms, chrono::Utc::now().timestamp(), q.seq, q.continued as i64, !q.backlog as i64],
        )
    });
    match inserted {
        Ok(1) => spawn_transcription(d.clone(), q.id.clone()),
        Ok(_) => {}
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err((500, format!("could not record event: {e}")));
        }
    }
    Ok(status_of(fetch(d, &q.id)).unwrap_or_else(|| "pending".into()))
}

pub(super) fn mark(id: &str, stage: &str) {
    crate::ambient::mark(id, stage);
}

pub(super) async fn prune_audio(keep: usize) {
    let Ok(mut dir) = tokio::fs::read_dir(audio_dir()).await else { return };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        if let Ok(modified) = entry.metadata().await.and_then(|m| m.modified()) {
            files.push((modified, entry.path()));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

pub(super) fn gate() -> std::sync::Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_TRANSCRIPTIONS))).clone()
}

fn spawn_transcription(d: Shared, id: String) {
    tokio::spawn(async move {
        let path = audio_path(&id);
        let permit = gate().acquire_owned().await.ok();
        let result = super::transcribe::transcribe_ambient(&d, &path).await;
        drop(permit);
        mark(&id, "transcribed");
        let (status, text, error) = match result {
            Ok(s) if s.text.is_empty() => ("empty", String::new(), None),
            Ok(s) => ("done", s.text, None),
            Err(e) => ("failed", String::new(), Some(e)),
        };
        d.store.write(|c| {
            c.execute(
                "UPDATE ambient_events SET status = ?2, text = ?3, error = ?4,
                     kept = CASE WHEN ?2 = 'done' THEN 0 ELSE 1 END WHERE id = ?1",
                rusqlite::params![id, status, text, error],
            )?;
            Ok(())
        });
        if status != "failed" {
            prune_audio(40).await;
        }
        let row: Option<(String, Option<i64>, String, i64, i64, i64)> = d
            .store
            .with_connection(|c| {
                c.query_row(
                    "SELECT device, seq, started_at, duration_ms, continued, routed
                     FROM ambient_events WHERE id = ?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
            })
            .ok();
        let Some((device, seq, started_at, duration_ms, continued, routed)) = row else {
            return;
        };
        let seq = seq.unwrap_or(-1);
        if status != "done" {
            super::ambient_router::submit(&d, &device, super::ambient_router::Msg::Skip(seq));
            return;
        }
        if assistant_was_speaking(&device, &started_at, duration_ms) && echo_of_reply(&text, &device, 0.8) {
            d.store.write(|c| {
                c.execute("UPDATE ambient_events SET status = 'echo', kept = 0 WHERE id = ?1", [&id])?;
                Ok(())
            });
            super::ambient_router::submit(&d, &device, super::ambient_router::Msg::Skip(seq));
            return;
        }
        if routed == 0 {
            d.store.write(|c| {
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
        let vps = d.config.lock().unwrap().ambient.voiceprints.clone();
        let user_voice = if vps.is_empty() {
            None
        } else {
            tokio::fs::read(audio_path(&id))
                .await
                .ok()
                .and_then(|bytes| crate::voiceprint::extract_from_pcm_or_wav(&bytes).ok())
                .and_then(|vec| crate::voiceprint::score_vector(&vec, &vps))
        };
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
                continued: continued != 0,
                user_voice,
                voice_score: None,
                revises: None,
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

const ANSWER_LEAD: &[&str] = &["yeah", "yes", "yep", "yup", "no", "nope", "nah", "okay", "ok", "sure", "right", "exactly", "correct", "haan", "han"];

fn longest_run(heard: &[String], said: &[String]) -> usize {
    let mut best = 0;
    let mut row = vec![0usize; said.len() + 1];
    for h in heard {
        let mut next = vec![0usize; said.len() + 1];
        for (j, w) in said.iter().enumerate() {
            if h == w {
                next[j + 1] = row[j] + 1;
                best = best.max(next[j + 1]);
            }
        }
        row = next;
    }
    best
}

pub(super) fn echo_match(text: &str, replies: &[String], ratio: f64) -> bool {
    let heard = spoken_words(text);
    let mut content: Vec<&String> = heard.iter().filter(|w| !ECHO_STOP.contains(&w.as_str())).collect();
    content.sort();
    content.dedup();
    if content.len() < 2 {
        return false;
    }
    replies.iter().any(|reply| {
        let said = spoken_words(reply);
        let answers = ANSWER_LEAD.contains(&heard[0].as_str()) && !said.windows(2).any(|w| w[0] == heard[0] && w[1] == heard[1]);
        let shared = content.iter().filter(|w| said.contains(**w)).count();
        !answers && shared as f64 / content.len() as f64 >= ratio && longest_run(&heard, &said) as f64 >= 0.6 * heard.len() as f64
    })
}

pub(super) fn echo_of_reply(text: &str, device: &str, ratio: f64) -> bool {
    echo_match(text, &recent_replies(device), ratio)
}

pub(super) fn recent_replies(device: &str) -> Vec<String> {
    let mut out = crate::tts_stream::live_texts(device);
    let since = chrono::Utc::now().timestamp() - 180;
    let db_replies: Vec<String> = crate::ambient::store()
        .ok()
        .and_then(|s| {
            s.with_connection(|c| {
                let mut stmt = c.prepare(
                    "SELECT text FROM ambient_turns WHERE role = 'assistant' AND device = ?1 AND created_at >= ?2 AND TRIM(text) != ''
                     ORDER BY id DESC LIMIT 4",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![device, since], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .ok()
        })
        .unwrap_or_default();
    for r in db_replies {
        if !r.trim().is_empty() && !out.iter().any(|existing| existing == r.trim()) {
            out.push(r);
        }
    }
    out
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

pub(super) async fn record_heard(d: &Shared, device: &str, e: super::listener::Event) {
    let started_at = chrono::DateTime::from_timestamp_millis((e.started * 1000.0) as i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();
    let now = chrono::Utc::now().timestamp();
    d.store.write(|c| {
        c.execute(
            "INSERT OR IGNORE INTO ambient_events
                 (id, device, started_at, duration_ms, status, text, created_at, seq, continued, during_playback, routed, media, kept)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 1, ?10, ?11)",
            rusqlite::params![
                e.id,
                device,
                started_at,
                e.duration_ms,
                e.status,
                e.text,
                now,
                e.seq,
                e.during_playback as i64,
                e.media as i64,
                (e.status == "empty") as i64
            ],
        )
    });
    mark(&e.id, "transcribed");
    if e.status != "done" {
        announce(d, device, e.seq, true);
    } else {
        super::ambient_router::submit(
            d,
            device,
            super::ambient_router::Msg::Done(super::ambient_router::Part {
                id: e.id.clone(),
                seq: e.seq,
                text: e.text,
                started: e.started,
                ended: e.started + e.duration_ms as f64 / 1000.0,
                started_at,
                continued: false,
                user_voice: e.user_voice,
                voice_score: Some(std::sync::Arc::new(tokio::sync::Mutex::new(Some(e.voice_score)))),
                revises: e.revises,
            }),
        );
    }
    if !e.pcm.is_empty() && tokio::fs::create_dir_all(audio_dir()).await.is_ok() {
        let _ = tokio::fs::write(audio_path(&e.id), super::listener::wav(&e.pcm)).await;
    }
    prune_audio(40).await;
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

#[derive(Deserialize)]
pub(crate) struct EnrollVoiceprintReq {
    #[serde(default)]
    audio_base64: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    from_recent: bool,
}

#[derive(Deserialize)]
pub(crate) struct DeleteVoiceprintQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

pub(crate) async fn enroll_voiceprint(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<EnrollVoiceprintReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let audio_bytes = if req.from_recent {
        let Ok(mut dir) = tokio::fs::read_dir(audio_dir()).await else {
            return (StatusCode::BAD_REQUEST, "No recent ambient voice recordings found").into_response();
        };
        let mut latest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        while let Ok(Some(entry)) = dir.next_entry().await {
            if let Ok(modified) = entry.metadata().await.and_then(|m| m.modified()) {
                if latest.as_ref().is_none_or(|(t, _)| modified > *t) {
                    latest = Some((modified, entry.path()));
                }
            }
        }
        let Some((_, path)) = latest else {
            return (StatusCode::BAD_REQUEST, "No recent ambient voice recordings found").into_response();
        };
        match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("Failed to read recording: {e}")).into_response(),
        }
    } else {
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(req.audio_base64.trim()) {
            Ok(b) if !b.is_empty() => b,
            _ => return (StatusCode::BAD_REQUEST, "audio_base64 is required").into_response(),
        }
    };

    let vec = match crate::voiceprint::extract_from_pcm_or_wav(&audio_bytes) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let save_res = {
        let mut c = d.config.lock().unwrap();
        let next_num = c.ambient.voiceprints.len() + 1;
        let label = if req.label.trim().is_empty() {
            format!("Sample {next_num}")
        } else {
            req.label.trim().to_string()
        };
        let audio_path = crate::voiceprint::store_enrolment_audio(&audio_bytes).ok();
        c.ambient.voiceprints.push(crate::voiceprint::VoiceprintSample {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            created_at: chrono::Utc::now().timestamp(),
            model: crate::voiceprint::NEURAL_MODEL.into(),
            audio_path,
            vector: vec,
        });
        while c.ambient.voiceprints.len() > 8 {
            c.ambient.voiceprints.remove(0);
        }
        crate::config::save_config(&c, &d.config_path)
    };
    if let Err(e) = save_res {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(settings_view(&d)).into_response()
}

pub(crate) async fn delete_voiceprint(
    State(d): State<Shared>,
    Query(q): Query<DeleteVoiceprintQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let save_res = {
        let mut c = d.config.lock().unwrap();
        if let Some(id) = q.id.as_deref().filter(|s| !s.trim().is_empty()) {
            c.ambient.voiceprints.retain(|vp| vp.id != id);
        } else {
            c.ambient.voiceprints.clear();
        }
        crate::config::save_config(&c, &d.config_path)
    };
    if let Err(e) = save_res {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(settings_view(&d)).into_response()
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
    d.store.write(|c| c.execute("UPDATE ambient_events SET routed = 0 WHERE status = 'pending' AND created_at < ?1", [stale]));
    for id in pending {
        if audio_path(&id).exists() {
            spawn_transcription(d.clone(), id);
        } else {
            d.store.write(|c| {
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

fn has_key(k: &Option<String>) -> bool {
    k.as_deref().is_some_and(|k| !k.trim().is_empty())
}

pub(crate) fn warm_voice(d: Shared) {
    tokio::spawn(async move {
        crate::tts::clean_old_replies().await;
        crate::tts::apply_config(&d.config.lock().unwrap());
    });
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
