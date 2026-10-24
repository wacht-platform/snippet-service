use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

struct Live {
    queue: Mutex<VecDeque<String>>,
    closed: AtomicBool,
    notify: tokio::sync::Notify,
    text: Mutex<String>,
}

impl Live {
    fn new() -> Arc<Self> {
        Arc::new(Live {
            queue: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
            text: Mutex::new(String::new()),
        })
    }

    fn push(&self, chunk: String) {
        let mut text = self.text.lock().unwrap();
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&chunk);
        drop(text);
        self.queue.lock().unwrap().push_back(chunk);
        self.notify.notify_one();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    async fn next(&self, cancel: &AtomicBool) -> Option<String> {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(chunk) = self.queue.lock().unwrap().pop_front() {
                return Some(chunk);
            }
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    self.close();
                }
            }
        }
    }
}

struct Job {
    turn: i64,
    device: String,
    conversation: String,
    live: Arc<Live>,
    cancel: Arc<AtomicBool>,
}

struct Provisional {
    live: Arc<Live>,
    turn: i64,
    token: u64,
}

static STREAMING: Mutex<Option<Provisional>> = Mutex::new(None);
static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
static CURRENT: Mutex<Option<(i64, Arc<AtomicBool>, String)>> = Mutex::new(None);
static WORKER: AtomicBool = AtomicBool::new(false);
static INTERRUPTED: Mutex<Option<String>> = Mutex::new(None);

pub fn chunk_path(turn: i64, seq: i64) -> std::path::PathBuf {
    crate::tts::audio_dir().join(turn.to_string()).join(format!("{seq}.pcm"))
}

pub fn pending() -> bool {
    CURRENT.lock().unwrap().is_some() || !QUEUE.lock().unwrap().is_empty()
}

pub fn cut_view(text: &str) -> Option<String> {
    let wanted = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (status, heard): (String, Option<String>) = crate::ambient::store()
        .ok()
        .and_then(|s| {
            s.with_connection(|c| {
                c.query_row(
                    "SELECT status, heard_text FROM ambient_turns WHERE role = 'assistant' AND TRIM(text) = ?1 ORDER BY id DESC LIMIT 1",
                    [&wanted],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
            })
            .ok()
        })?;
    if status != "interrupted" && status != "cancelled" {
        return None;
    }
    let heard = heard.unwrap_or_default();
    Some(if heard.trim().is_empty() {
        "[the user interrupted before any of this was spoken]".to_string()
    } else {
        format!("{} [cut off by the user; the rest was never spoken]", heard.trim())
    })
}

pub fn take_interruption() -> Option<String> {
    INTERRUPTED.lock().unwrap().take()
}

fn emit(value: Value) {
    crate::session::emit_device_event(value);
}

fn set_status(turn: i64, status: &str) {
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute("UPDATE ambient_turns SET status = ?2 WHERE id = ?1", rusqlite::params![turn, status])?;
            Ok(())
        });
    }
}

async fn prepare() -> Result<(String, crate::ambient::Conversation), String> {
    let Some((device, _)) = crate::ambient::lease_holder() else {
        return Err("no device is listening, so there is nobody to speak to".into());
    };
    if !crate::ambient::effective_active().await {
        return Err("ambient is passive: you cannot speak in this mode. Reply in the chat (the user gets a notification), or call ambient_mode with active first if a spoken back and forth is needed".into());
    }
    let ttl = crate::config::SnippetConfig::load(crate::config::default_config_path())
        .await
        .map(|c| c.ambient.conversation_timeout_secs)
        .unwrap_or(90);
    let conversation = crate::ambient::open_conversation("mission-control", ttl);
    Ok((device, conversation))
}

fn create_job(device: &str, conversation: &str) -> Result<(Arc<Live>, i64, Arc<AtomicBool>), String> {
    let turn = crate::ambient::record_turn_status(conversation, "assistant", "", device, "queued")?;
    crate::ambient::note_spoke();
    crate::ambient::hold(180);
    let live = Live::new();
    let cancel = Arc::new(AtomicBool::new(false));
    QUEUE.lock().unwrap().push_back(Job {
        turn,
        device: device.to_string(),
        conversation: conversation.to_string(),
        live: live.clone(),
        cancel: cancel.clone(),
    });
    start_worker();
    Ok((live, turn, cancel))
}

fn finalise_text(turn: i64, text: &str) {
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute("UPDATE ambient_turns SET text = ?2 WHERE id = ?1", rusqlite::params![turn, text])?;
            Ok(())
        });
    }
}

pub async fn speak(text: &str, expecting: Option<&str>, about: Option<&str>, replace: bool) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to say".into());
    }
    let (device, conversation) = prepare().await?;
    if let Some(kind) = expecting.filter(|k| !k.is_empty() && *k != "none") {
        crate::ambient::set_expectation(kind, about.unwrap_or(""), 10);
    }
    let provisional = STREAMING.lock().unwrap().take();
    if let Some(p) = provisional {
        let spoken = p.live.text.lock().unwrap().clone();
        let words_spoken = spoken.split_whitespace().count();
        let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        if !p.live.closed.load(Ordering::SeqCst) && norm(text).starts_with(&norm(&spoken)) {
            let rest = text.split_whitespace().skip(words_spoken).collect::<Vec<_>>().join(" ");
            for chunk in crate::tts::chunk_text(&rest) {
                p.live.push(chunk);
            }
            p.live.close();
            finalise_text(p.turn, text);
            return Ok(json!({"spoken": true, "turn": p.turn, "queued": true, "streamed": true}));
        }
        p.live.close();
    }
    let chunks = crate::tts::chunk_text(text);
    if chunks.is_empty() {
        return Err("nothing to say".into());
    }
    if replace {
        cancel_all("replaced");
    }
    let (live, turn, _) = create_job(&device, &conversation.id)?;
    finalise_text(turn, text);
    let count = chunks.len();
    for chunk in chunks {
        live.push(chunk);
    }
    live.close();
    Ok(json!({"spoken": true, "turn": turn, "queued": true, "chunks": count}))
}

fn start_worker() {
    if WORKER.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            let job = QUEUE.lock().unwrap().pop_front();
            let Some(job) = job else {
                WORKER.store(false, Ordering::SeqCst);
                if QUEUE.lock().unwrap().is_empty() || WORKER.swap(true, Ordering::SeqCst) {
                    break;
                }
                continue;
            };
            run_job(job).await;
        }
    });
}

async fn run_job(job: Job) {
    *CURRENT.lock().unwrap() = Some((job.turn, job.cancel.clone(), job.device.clone()));
    crate::ambient::mark_last("speech_start");
    crate::ambient::emit_state("speaking");
    emit(json!({
        "kind": "ambient_speech_start",
        "turn": job.turn,
        "device": job.device,
        "conversation_id": job.conversation,
        "format": "pcm_s16le_24000",
    }));
    let dir = crate::tts::audio_dir().join(job.turn.to_string());
    let _ = tokio::fs::create_dir_all(&dir).await;
    let mut made: i64 = 0;
    let mut failed = false;
    while let Some(text) = job.live.next(&job.cancel).await {
        let seq = made;
        match crate::tts::synthesize_pcm(crate::models::TTS_VOICE, &text).await {
            Ok((bytes, ms)) => {
                if tokio::fs::write(dir.join(format!("{seq}.pcm")), bytes).await.is_err() {
                    failed = true;
                    break;
                }
                if let Ok(store) = crate::ambient::store() {
                    let _ = store.with_connection(|c| {
                        c.execute(
                            "INSERT OR REPLACE INTO ambient_chunks (turn_id, seq, ms, text) VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![job.turn, seq, ms, text],
                        )?;
                        Ok(())
                    });
                }
                made += 1;
                if made == 1 {
                    crate::ambient::mark_last("first_chunk");
                }
                emit(json!({
                    "kind": "ambient_speech_chunk",
                    "turn": job.turn,
                    "seq": seq,
                    "ms": ms,
                    "text": text,
                    "device": job.device,
                }));
            }
            Err(e) => {
                eprintln!("[ambient] speech synthesis failed: {e}");
                failed = true;
                break;
            }
        }
    }
    let cancelled = job.cancel.load(Ordering::SeqCst);
    if let Ok(store) = crate::ambient::store() {
        let status = if failed { "failed" } else if cancelled { "cancelled" } else { "synthesised" };
        let _ = store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET ended = 1, chunk_count = ?2,
                     status = CASE WHEN status IN ('interrupted', 'played', 'missed') THEN status ELSE ?3 END
                 WHERE id = ?1",
                rusqlite::params![job.turn, made, status],
            )?;
            Ok(())
        });
    }
    emit(json!({"kind": "ambient_speech_end", "turn": job.turn, "count": made, "device": job.device}));
    *CURRENT.lock().unwrap() = None;
    crate::ambient::emit_state("speech synthesised");
}

fn heard(turn: i64, through: i64, played_ms: i64) -> String {
    let Ok(store) = crate::ambient::store() else {
        return String::new();
    };
    store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT seq, ms, text FROM ambient_chunks WHERE turn_id = ?1 ORDER BY seq")?;
            let rows = stmt
                .query_map([turn], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out: Vec<String> = Vec::new();
            for (seq, ms, text) in rows {
                if seq <= through {
                    out.push(text);
                } else if seq == through + 1 && played_ms > 0 && ms > 0 {
                    let words: Vec<&str> = text.split_whitespace().collect();
                    let take = ((words.len() as f64) * (played_ms as f64 / ms as f64)).floor() as usize;
                    if take > 0 {
                        out.push(words[..take.min(words.len())].join(" "));
                    }
                }
            }
            Ok(out.join(" "))
        })
        .unwrap_or_default()
}

fn audible_turns(device: &str) -> Vec<i64> {
    let now = chrono::Utc::now().timestamp();
    crate::ambient::store()
        .ok()
        .and_then(|s| {
            s.with_connection(|c| {
                let mut stmt = c.prepare(
                    "SELECT t.id FROM ambient_turns t
                     WHERE t.role = 'assistant' AND t.device = ?1 AND t.status IN ('queued', 'synthesised')
                       AND t.created_at + COALESCE((SELECT SUM(ms) FROM ambient_chunks WHERE turn_id = t.id), 0) / 1000 + 3 >= ?2
                     ORDER BY t.id",
                )?;
                let rows = stmt.query_map(rusqlite::params![device, now], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .ok()
        })
        .unwrap_or_default()
}

pub fn is_audible(device: &str) -> bool {
    !audible_turns(device).is_empty() || CURRENT.lock().unwrap().is_some()
}

pub fn interrupt(device: &str) -> bool {
    let turns = audible_turns(device);
    if turns.is_empty() && CURRENT.lock().unwrap().is_none() {
        return false;
    }
    eprintln!("[ambient-speech] interrupt device={device} turns={turns:?}");
    cancel_all("interrupted");
    let mut heard_text = String::new();
    for turn in &turns {
        let through: i64 = crate::ambient::store()
            .ok()
            .and_then(|s| s.with_connection(|c| c.query_row("SELECT played_through FROM ambient_turns WHERE id = ?1", [turn], |r| r.get(0))).ok())
            .unwrap_or(-1);
        let text = heard(*turn, through, 0);
        if let Ok(store) = crate::ambient::store() {
            let _ = store.with_connection(|c| {
                c.execute(
                    "UPDATE ambient_turns SET status = 'interrupted', heard_text = ?2 WHERE id = ?1 AND status NOT IN ('played', 'missed')",
                    rusqlite::params![turn, text],
                )?;
                Ok(())
            });
        }
        emit(json!({"kind": "ambient_speech_cancel", "turn": turn, "after_seq": through, "reason": "interrupted", "device": device}));
        if !text.is_empty() {
            heard_text = text;
        }
    }
    *INTERRUPTED.lock().unwrap() = Some(heard_text);
    crate::ambient::emit_state("interrupted");
    true
}

pub fn cancel_all(reason: &str) {
    eprintln!("[ambient-speech] cancel_all reason={reason}");
    let queued: Vec<Job> = QUEUE.lock().unwrap().drain(..).collect();
    for job in &queued {
        set_status(job.turn, "cancelled");
        emit(json!({"kind": "ambient_speech_cancel", "turn": job.turn, "after_seq": -1, "reason": reason, "device": job.device}));
    }
    let current = CURRENT.lock().unwrap().clone();
    if let Some((turn, flag, device)) = current {
        flag.store(true, Ordering::SeqCst);
        let through: i64 = crate::ambient::store()
            .ok()
            .and_then(|s| s.with_connection(|c| c.query_row("SELECT played_through FROM ambient_turns WHERE id = ?1", [turn], |r| r.get(0))).ok())
            .unwrap_or(-1);
        let text = heard(turn, through, 0);
        if let Ok(store) = crate::ambient::store() {
            let _ = store.with_connection(|c| {
                c.execute(
                    "UPDATE ambient_turns SET status = 'cancelled', heard_text = ?2 WHERE id = ?1 AND status NOT IN ('played', 'missed')",
                    rusqlite::params![turn, text],
                )?;
                Ok(())
            });
        }
        emit(json!({"kind": "ambient_speech_cancel", "turn": turn, "after_seq": through, "reason": reason, "device": device}));
    }
}

pub fn played(turn: i64, seq: Option<i64>, complete: bool, ttl_secs: u32) {
    if let Ok(store) = crate::ambient::store() {
        let now = chrono::Utc::now().timestamp();
        let _ = store.with_connection(|c| {
            if let Some(seq) = seq {
                c.execute(
                    "UPDATE ambient_turns SET played_through = MAX(played_through, ?2) WHERE id = ?1",
                    rusqlite::params![turn, seq],
                )?;
            }
            if complete {
                c.execute(
                    "UPDATE ambient_turns SET status = 'played', played_at = ?2,
                         heard_text = text WHERE id = ?1 AND status IN ('queued', 'synthesised', 'missed')",
                    rusqlite::params![turn, now],
                )?;
            }
            Ok(())
        });
    }
    if complete {
        crate::ambient::playback_finished(ttl_secs);
        crate::ambient::emit_state("playback finished");
    }
}

pub fn barge(turn: i64, seq: i64, played_ms: i64) {
    eprintln!("[ambient-speech] barge turn={turn} seq={seq} played_ms={played_ms}");
    let text = heard(turn, seq, played_ms);
    let queued: Vec<Job> = QUEUE.lock().unwrap().drain(..).collect();
    for job in &queued {
        set_status(job.turn, "cancelled");
        emit(json!({"kind": "ambient_speech_cancel", "turn": job.turn, "after_seq": -1, "reason": "interrupted", "device": job.device}));
    }
    let current = CURRENT.lock().unwrap().clone();
    if let Some((cur, flag, device)) = current {
        if cur >= turn {
            flag.store(true, Ordering::SeqCst);
            if cur != turn {
                set_status(cur, "cancelled");
                emit(json!({"kind": "ambient_speech_cancel", "turn": cur, "after_seq": -1, "reason": "interrupted", "device": device}));
            }
        }
    }
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET status = 'interrupted', heard_text = ?2,
                     played_through = MAX(played_through, ?3) WHERE id = ?1",
                rusqlite::params![turn, text, seq],
            )?;
            Ok(())
        });
    }
    *INTERRUPTED.lock().unwrap() = Some(text);
    crate::ambient::emit_state("interrupted");
}

pub fn turns_after(after: i64, device: &str) -> Result<Vec<Value>, String> {
    let store = crate::ambient::store()?;
    let cutoff = chrono::Utc::now().timestamp() - 60;
    store
        .with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET status = 'missed' WHERE role = 'assistant' AND device = ?1
                   AND status IN ('queued', 'synthesised')
                   AND created_at + COALESCE((SELECT SUM(ms) FROM ambient_chunks WHERE turn_id = ambient_turns.id), 0) / 1000 < ?2",
                rusqlite::params![device, cutoff],
            )?;
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, text, created_at, status, ended, chunk_count FROM ambient_turns
                 WHERE id > ?1 AND role = 'assistant' AND device = ?2 AND status IN ('queued', 'synthesised')
                 ORDER BY id LIMIT 20",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![after, device], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, i64>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::new();
            for (id, conversation, text, created, status, ended, count) in rows {
                let mut cs = c.prepare("SELECT seq, ms, text FROM ambient_chunks WHERE turn_id = ?1 ORDER BY seq")?;
                let chunks = cs
                    .query_map([id], |r| Ok(json!({"seq": r.get::<_, i64>(0)?, "ms": r.get::<_, i64>(1)?, "text": r.get::<_, String>(2)?})))?
                    .collect::<Result<Vec<_>, _>>()?;
                out.push(json!({
                    "id": id,
                    "conversation_id": conversation,
                    "text": text,
                    "status": status,
                    "ended": ended,
                    "count": count,
                    "chunks": chunks,
                    "created_at": created,
                }));
            }
            Ok(out)
        })
        .map_err(|e| e.to_string())
}

fn read_string(b: &[char], mut i: usize) -> (String, usize, bool) {
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(i) else {
            return (out, i, false);
        };
        match c {
            '"' => return (out, i + 1, true),
            '\\' => {
                let Some(&n) = b.get(i + 1) else {
                    return (out, i, false);
                };
                match n {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let hex = |at: usize| -> Option<u32> {
                            let s: String = b.get(at..at + 4)?.iter().collect();
                            u32::from_str_radix(&s, 16).ok()
                        };
                        let Some(high) = hex(i + 2) else {
                            return (out, i, false);
                        };
                        if (0xD800..0xDC00).contains(&high) {
                            if b.get(i + 6) == Some(&'\\') && b.get(i + 7) == Some(&'u') {
                                let Some(low) = hex(i + 8) else {
                                    return (out, i, false);
                                };
                                let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                i += 12;
                                continue;
                            }
                            return (out, i, false);
                        }
                        out.push(char::from_u32(high).unwrap_or('\u{fffd}'));
                        i += 6;
                        continue;
                    }
                    other => out.push(other),
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
}

fn extract_text(raw: &str) -> Option<(String, bool)> {
    let b: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let skip = |i: &mut usize| {
        while *i < b.len() && b[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip(&mut i);
    if b.get(i)? != &'{' {
        return None;
    }
    i += 1;
    loop {
        skip(&mut i);
        while b.get(i) == Some(&',') {
            i += 1;
            skip(&mut i);
        }
        if b.get(i)? != &'"' {
            return None;
        }
        let (key, next, complete) = read_string(&b, i + 1);
        if !complete {
            return None;
        }
        i = next;
        skip(&mut i);
        if b.get(i)? != &':' {
            return None;
        }
        i += 1;
        skip(&mut i);
        if key == "text" {
            if b.get(i)? != &'"' {
                return None;
            }
            let (value, _, done) = read_string(&b, i + 1);
            return Some((value, done));
        }
        match *b.get(i)? {
            '"' => {
                let (_, next, complete) = read_string(&b, i + 1);
                if !complete {
                    return None;
                }
                i = next;
            }
            '{' | '[' => {
                let mut depth = 0i32;
                loop {
                    let c = *b.get(i)?;
                    if c == '"' {
                        let (_, next, complete) = read_string(&b, i + 1);
                        if !complete {
                            return None;
                        }
                        i = next;
                        continue;
                    }
                    if c == '{' || c == '[' {
                        depth += 1;
                    } else if c == '}' || c == ']' {
                        depth -= 1;
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            _ => {
                while i < b.len() && !matches!(b[i], ',' | '}') {
                    i += 1;
                }
                if i >= b.len() {
                    return None;
                }
            }
        }
    }
}

async fn feed(live: &mut Option<(Arc<Live>, i64, u64)>, disabled: &mut bool, chunk: String) {
    if *disabled {
        return;
    }
    if live.is_none() {
        match prepare().await {
            Ok((device, conversation)) => match create_job(&device, &conversation.id) {
                Ok((l, turn, _)) => {
                    let token = TOKEN.fetch_add(1, Ordering::SeqCst);
                    *STREAMING.lock().unwrap() = Some(Provisional { live: l.clone(), turn, token });
                    *live = Some((l, turn, token));
                }
                Err(_) => {
                    *disabled = true;
                    return;
                }
            },
            Err(_) => {
                *disabled = true;
                return;
            }
        }
    }
    if let Some((l, _, _)) = live {
        l.push(chunk);
    }
}

fn drop_provisional(token: u64) {
    let mut guard = STREAMING.lock().unwrap();
    if guard.as_ref().is_some_and(|p| p.token == token) {
        if let Some(p) = guard.take() {
            p.live.close();
        }
    }
}

pub async fn consume_tool_stream(mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::llm::ToolArgEvent>) {
    use crate::llm::ToolArgEvent;
    let mut call: Option<String> = None;
    let mut raw = String::new();
    let mut sent = 0usize;
    let mut live: Option<(Arc<Live>, i64, u64)> = None;
    let mut disabled = false;
    while let Some(event) = rx.recv().await {
        match event {
            ToolArgEvent::Start { call_id, name } => {
                if name == "speak" || name.ends_with("__speak") {
                    crate::ambient::mark_last("speak_call");
                    call = Some(call_id);
                    raw.clear();
                    sent = 0;
                    live = None;
                    disabled = false;
                } else {
                    call = None;
                }
            }
            ToolArgEvent::Delta { call_id, chunk } => {
                if call.as_deref() != Some(call_id.as_str()) {
                    continue;
                }
                raw.push_str(&chunk);
                if let Some((text, _)) = extract_text(&raw) {
                    let chunks = crate::tts::chunk_text(&text);
                    let ready = chunks.len().saturating_sub(1);
                    while sent < ready {
                        feed(&mut live, &mut disabled, chunks[sent].clone()).await;
                        sent += 1;
                    }
                }
            }
            ToolArgEvent::End { call_id } => {
                if call.as_deref() != Some(call_id.as_str()) {
                    continue;
                }
                if let Some((text, _)) = extract_text(&raw) {
                    let chunks = crate::tts::chunk_text(&text);
                    while sent < chunks.len() {
                        feed(&mut live, &mut disabled, chunks[sent].clone()).await;
                        sent += 1;
                    }
                }
                if let Some((_, _, token)) = live.take() {
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        drop_provisional(token);
                    });
                }
                call = None;
            }
            ToolArgEvent::Reset => {
                if let Some((_, _, token)) = live.take() {
                    drop_provisional(token);
                }
                call = None;
                raw.clear();
                sent = 0;
            }
        }
    }
}

pub fn mission_control_stream() -> crate::llm::StreamHandle {
    let (tap, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(consume_tool_stream(rx));
    let mut buffer = crate::llm::StreamBuffer::default();
    buffer.set_tool_tap(tap);
    Arc::new(Mutex::new(buffer))
}
