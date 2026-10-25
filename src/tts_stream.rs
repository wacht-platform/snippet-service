use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const CONNECT_WAIT: Duration = Duration::from_millis(1500);
const CONNECT_LIMIT: Duration = Duration::from_secs(6);
const IDLE_CLOSE: Duration = Duration::from_secs(300);
const PING_EVERY: Duration = Duration::from_secs(20);
const POS_WAIT: Duration = Duration::from_millis(400);
const INTERRUPT_WAIT: Duration = Duration::from_millis(1500);
const POS_FRESH: Duration = Duration::from_secs(1);
const LINGER: Duration = Duration::from_secs(4);
const DEFAULT_VOICE: &str = "flux-priya-en";

pub enum PhoneFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, PartialEq)]
struct Config {
    key: String,
    voice: String,
    ttl: u32,
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static RECONCILING: AtomicUsize = AtomicUsize::new(0);
static LIVE: Mutex<Vec<Arc<StreamTurn>>> = Mutex::new(Vec::new());

pub fn apply_config(c: &crate::config::SnippetConfig) {
    let key = c.deepgram_api_key.clone().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    let voice = if c.ambient.tts_voice.starts_with("flux-") { c.ambient.tts_voice.clone() } else { DEFAULT_VOICE.to_string() };
    let next = key.map(|key| Config { key, voice, ttl: c.ambient.conversation_timeout_secs });
    let mut current = CONFIG.lock().unwrap();
    if current.as_ref().map(|c| (&c.key, &c.voice)) != next.as_ref().map(|c| (&c.key, &c.voice)) {
        let stale: Vec<Arc<Session>> = hub().lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in stale {
            if s.st.lock().unwrap().queue.is_empty() {
                let _ = s.cmd.send(Cmd::Close);
            }
        }
    }
    *current = next;
}

pub fn enabled() -> bool {
    CONFIG.lock().unwrap().is_some()
}

fn config() -> Option<Config> {
    CONFIG.lock().unwrap().clone()
}

fn phones() -> &'static Mutex<HashMap<String, (u64, mpsc::UnboundedSender<PhoneFrame>)>> {
    static PHONES: OnceLock<Mutex<HashMap<String, (u64, mpsc::UnboundedSender<PhoneFrame>)>>> = OnceLock::new();
    PHONES.get_or_init(Default::default)
}

pub fn register_phone(device: &str, tx: mpsc::UnboundedSender<PhoneFrame>) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    phones().lock().unwrap().insert(device.to_string(), (id, tx));
    id
}

pub fn unregister_phone(device: &str, id: u64) {
    let mut map = phones().lock().unwrap();
    if map.get(device).is_some_and(|(i, _)| *i == id) {
        map.remove(device);
    }
}

pub fn has_phone(device: &str) -> bool {
    phones().lock().unwrap().get(device).is_some_and(|(_, tx)| !tx.is_closed())
}

fn send_phone(device: &str, frame: PhoneFrame) -> bool {
    phones().lock().unwrap().get(device).is_some_and(|(_, tx)| tx.send(frame).is_ok())
}

#[derive(Clone, Copy)]
struct Pos {
    turn: i64,
    ms: i64,
    done: bool,
    at: Instant,
}

fn positions() -> &'static Mutex<HashMap<String, Pos>> {
    static POS: OnceLock<Mutex<HashMap<String, Pos>>> = OnceLock::new();
    POS.get_or_init(Default::default)
}

pub fn report_pos(device: &str, turn: i64, ms: i64, done: bool) {
    let now = Instant::now();
    positions().lock().unwrap().insert(device.to_string(), Pos { turn, ms, done, at: now });
    let found = LIVE.lock().unwrap().iter().find(|t| t.turn == turn).cloned();
    let Some(t) = found else { return };
    let finished = {
        let mut i = t.inner.lock().unwrap();
        i.heard = Some((ms, now));
        done && !i.stopped && i.audio_ms.is_some()
    };
    if done {
        let generated = t.inner.lock().unwrap().audio_ms.unwrap_or(-1);
        let flag = if generated > 0 && ms + 400 < generated { " CUT-SHORT" } else { "" };
        eprintln!("[ambient-flux] turn {turn} phone played {ms}ms of {generated}ms generated{flag}");
    }
    if finished {
        LIVE.lock().unwrap().retain(|x| !Arc::ptr_eq(x, &t));
        mark_played(&t);
        let ttl = config().map(|c| c.ttl).unwrap_or(90);
        crate::ambient::playback_finished(ttl);
        crate::ambient::emit_state("playback finished");
    }
}

fn mark_played(t: &StreamTurn) {
    if let Ok(store) = crate::ambient::store() {
        let now = chrono::Utc::now().timestamp();
        let _ = store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET status = 'played', played_at = ?2, heard_text = text WHERE id = ?1 AND status = 'speaking'",
                rusqlite::params![t.turn, now],
            )?;
            Ok(())
        });
    }
}

fn set_status(turn: i64, status: &str) {
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute("UPDATE ambient_turns SET status = ?2 WHERE id = ?1 AND status = 'speaking'", rusqlite::params![turn, status])?;
            Ok(())
        });
    }
}

fn emit(value: Value) {
    crate::session::emit_device_event(value);
}

#[derive(Default)]
struct TurnInner {
    text: String,
    queued: bool,
    flushed: bool,
    stopped: bool,
    started: bool,
    start_offset: i64,
    bytes: u64,
    audio_ms: Option<i64>,
    ended_at: Option<Instant>,
    heard: Option<(i64, Instant)>,
    first_push: Option<Instant>,
    file: Option<std::fs::File>,
}

pub struct StreamTurn {
    pub turn: i64,
    device: String,
    session: Arc<Session>,
    inner: Mutex<TurnInner>,
}

impl StreamTurn {
    pub fn push(self: &Arc<Self>, delta: &str) -> bool {
        if delta.is_empty() {
            return true;
        }
        let mut i = self.inner.lock().unwrap();
        if i.flushed || i.stopped {
            return false;
        }
        if !i.queued {
            let mut st = self.session.st.lock().unwrap();
            if st.closed || st.retired {
                return false;
            }
            st.queue.push_back(self.clone());
            i.queued = true;
            i.first_push = Some(Instant::now());
        }
        i.text.push_str(delta);
        self.session.st.lock().unwrap().last_used = Some(Instant::now());
        self.session.send(json!({"type": "Speak", "text": delta}))
    }

    pub fn flush(self: &Arc<Self>) {
        let mut i = self.inner.lock().unwrap();
        if i.flushed || i.stopped {
            return;
        }
        i.flushed = true;
        if i.queued {
            drop(i);
            self.session.send(json!({"type": "Flush"}));
            return;
        }
        i.stopped = true;
        drop(i);
        LIVE.lock().unwrap().retain(|x| !Arc::ptr_eq(x, self));
        set_status(self.turn, "cancelled");
        emit(json!({"kind": "ambient_speech_end", "turn": self.turn, "count": 0, "device": self.device, "stream": true}));
    }

    pub fn text(&self) -> String {
        self.inner.lock().unwrap().text.clone()
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.lock().unwrap().stopped
    }
}

enum Cmd {
    Send(String),
    Close,
}

#[derive(Default)]
struct SessionState {
    connected: bool,
    closed: bool,
    retired: bool,
    queue: VecDeque<Arc<StreamTurn>>,
    completed_ms: i64,
    interrupted: Option<oneshot::Sender<Value>>,
    last_used: Option<Instant>,
}

pub struct Session {
    device: String,
    voice: String,
    cmd: mpsc::UnboundedSender<Cmd>,
    st: Mutex<SessionState>,
    changed: Notify,
}

impl Session {
    fn send(&self, v: Value) -> bool {
        self.cmd.send(Cmd::Send(v.to_string())).is_ok()
    }
}

fn hub() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static HUB: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    HUB.get_or_init(Default::default)
}

fn session(device: &str) -> Option<Arc<Session>> {
    let cfg = config()?;
    let mut map = hub().lock().unwrap();
    if let Some(s) = map.get(device) {
        let st = s.st.lock().unwrap();
        if !st.closed && !st.retired && s.voice == cfg.voice {
            drop(st);
            return Some(s.clone());
        }
    }
    let (cmd, rx) = mpsc::unbounded_channel();
    let s = Arc::new(Session {
        device: device.to_string(),
        voice: cfg.voice.clone(),
        cmd,
        st: Mutex::new(SessionState::default()),
        changed: Notify::new(),
    });
    map.insert(device.to_string(), s.clone());
    let task = s.clone();
    tokio::spawn(async move {
        let outcome = run(&task, cfg.key, rx).await;
        if let Err(e) = &outcome {
            eprintln!("[ambient-flux] {} session ended: {e}", task.device);
        }
        let orphans: Vec<Arc<StreamTurn>> = {
            let mut st = task.st.lock().unwrap();
            st.closed = true;
            st.interrupted = None;
            st.queue.drain(..).collect()
        };
        task.changed.notify_waiters();
        for t in orphans {
            let ms = (t.inner.lock().unwrap().bytes / 48) as i64;
            finish_turn(&t, ms);
        }
        let mut map = hub().lock().unwrap();
        if map.get(&task.device).is_some_and(|x| Arc::ptr_eq(x, &task)) {
            map.remove(&task.device);
        }
    });
    Some(s)
}

pub fn prewarm(device: &str) {
    if enabled() && has_phone(device) {
        let _ = session(device);
    }
}

pub async fn ready(device: &str) -> Option<Arc<Session>> {
    if !enabled() {
        return None;
    }
    if !has_phone(device) {
        eprintln!("[ambient-flux] {device} has no duplex socket, using chunk speech");
        return None;
    }
    let s = session(device)?;
    let deadline = tokio::time::Instant::now() + CONNECT_WAIT;
    loop {
        let notified = s.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let st = s.st.lock().unwrap();
            if st.connected && !st.closed && !st.retired {
                return Some(s.clone());
            }
            if st.closed {
                eprintln!("[ambient-flux] {device} connect failed, using chunk speech");
                return None;
            }
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(deadline) => {
                eprintln!("[ambient-flux] {device} not connected within {}ms, using chunk speech", CONNECT_WAIT.as_millis());
                return None;
            }
        }
    }
}

pub fn start_turn(session: &Arc<Session>, device: &str, conversation: &str) -> Result<Arc<StreamTurn>, String> {
    let turn = crate::ambient::record_turn_status(conversation, "assistant", "", device, "speaking")?;
    crate::ambient::note_spoke();
    crate::ambient::hold(180);
    let dir = crate::tts::audio_dir().join(turn.to_string());
    let file = std::fs::create_dir_all(&dir).and_then(|_| std::fs::File::create(dir.join("stream.pcm"))).ok();
    let t = Arc::new(StreamTurn {
        turn,
        device: device.to_string(),
        session: session.clone(),
        inner: Mutex::new(TurnInner { file, ..Default::default() }),
    });
    LIVE.lock().unwrap().push(t.clone());
    crate::ambient::mark_last("speech_start");
    emit(json!({
        "kind": "ambient_speech_start",
        "turn": turn,
        "device": device,
        "conversation_id": conversation,
        "format": "pcm_s16le_24000",
        "stream": true,
    }));
    crate::ambient::emit_state("speaking");
    Ok(t)
}

async fn run(session: &Arc<Session>, key: String, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<(), String> {
    let url = format!(
        "wss://api.deepgram.com/v2/speak?model={}&encoding=linear16&sample_rate=24000",
        urlencoding::encode(&session.voice)
    );
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request
        .headers_mut()
        .insert("Authorization", format!("Token {key}").parse().map_err(|_| "invalid Deepgram key".to_string())?);
    let began = Instant::now();
    let (ws, _) = tokio::time::timeout(CONNECT_LIMIT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "Flux connect timed out".to_string())?
        .map_err(|e| format!("Flux connect failed: {e}"))?;
    eprintln!("[ambient-flux] {} connected in {}ms", session.device, began.elapsed().as_millis());
    {
        let mut st = session.st.lock().unwrap();
        st.connected = true;
        st.last_used = Some(Instant::now());
    }
    session.changed.notify_waiters();
    let (mut tx, mut rx) = ws.split();
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                tx.send(Message::Ping(Vec::new().into())).await.map_err(|e| format!("Flux ping failed: {e}"))?;
            }
            c = cmd.recv() => match c {
                Some(Cmd::Send(text)) => {
                    tx.send(Message::Text(text.into())).await.map_err(|e| format!("Flux send failed: {e}"))?;
                }
                Some(Cmd::Close) | None => break,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) => on_audio(session, &b),
                Some(Ok(Message::Text(t))) => on_event(session, &t),
                Some(Ok(Message::Close(frame))) => return Err(format!("Flux closed the connection: {frame:?}")),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("Flux stream error: {e}")),
                None => return Err("Flux connection ended".into()),
            },
            _ = tick.tick() => {
                let st = session.st.lock().unwrap();
                if st.queue.is_empty() && st.last_used.is_some_and(|at| at.elapsed() >= IDLE_CLOSE) {
                    drop(st);
                    break;
                }
            }
        }
    }
    let _ = tx.send(Message::Text("{\"type\":\"Close\"}".into())).await;
    let _ = tx.close().await;
    Ok(())
}

fn front(session: &Session) -> Option<Arc<StreamTurn>> {
    session.st.lock().unwrap().queue.front().cloned()
}

fn begin(t: &StreamTurn, i: &mut TurnInner) {
    if i.started {
        return;
    }
    i.started = true;
    i.start_offset = t.session.st.lock().unwrap().completed_ms;
    send_phone(&t.device, PhoneFrame::Text(json!({"t": "tts_start", "turn": t.turn}).to_string()));
}

fn on_audio(session: &Session, pcm: &[u8]) {
    if session.st.lock().unwrap().retired {
        return;
    }
    let Some(t) = front(session) else { return };
    let mut i = t.inner.lock().unwrap();
    if i.stopped {
        return;
    }
    begin(&t, &mut i);
    if i.bytes == 0 {
        crate::ambient::mark_last("first_chunk");
        if let Some(at) = i.first_push {
            eprintln!("[ambient-flux] turn {} first audio {}ms after first text", t.turn, at.elapsed().as_millis());
        }
    }
    i.bytes += pcm.len() as u64;
    if let Some(f) = i.file.as_mut() {
        let _ = f.write_all(pcm);
    }
    drop(i);
    let mut frame = Vec::with_capacity(4 + pcm.len());
    frame.extend_from_slice(&(t.turn as u32).to_le_bytes());
    frame.extend_from_slice(pcm);
    send_phone(&t.device, PhoneFrame::Binary(frame));
}

fn on_event(session: &Session, raw: &str) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return };
    match v.get("type").and_then(|x| x.as_str()) {
        Some("SpeechStarted") => {
            if session.st.lock().unwrap().retired {
                return;
            }
            if let Some(t) = front(session) {
                let mut i = t.inner.lock().unwrap();
                if !i.stopped {
                    begin(&t, &mut i);
                }
            }
        }
        Some("SpeechMetadata") => {
            let ms = v.get("audio_duration_ms").and_then(|x| x.as_i64()).unwrap_or(0);
            let done = {
                let mut st = session.st.lock().unwrap();
                st.completed_ms += ms;
                st.queue.pop_front()
            };
            if let Some(t) = done {
                finish_turn(&t, ms);
            }
        }
        Some("SpeechInterrupted") => {
            if let Some(tx) = session.st.lock().unwrap().interrupted.take() {
                let _ = tx.send(v);
            }
        }
        Some("Warning") => {
            eprintln!("[ambient-flux] {} {}", session.device, raw.chars().take(300).collect::<String>());
        }
        Some("Error") => {
            eprintln!("[ambient-flux] {} {}", session.device, raw.chars().take(300).collect::<String>());
            session.st.lock().unwrap().retired = true;
            let _ = session.cmd.send(Cmd::Close);
        }
        _ => {}
    }
}

fn finish_turn(t: &StreamTurn, ms: i64) {
    let (stopped, text) = {
        let mut i = t.inner.lock().unwrap();
        if i.audio_ms.is_some() {
            return;
        }
        i.audio_ms = Some(ms);
        i.ended_at = Some(Instant::now());
        i.file = None;
        (i.stopped, i.text.clone())
    };
    {
        let chars = text.trim().chars().count().max(1) as i64;
        let per = ms / chars;
        let flag = if chars >= 20 && per < 35 { " SHORT-AUDIO" } else { "" };
        eprintln!("[ambient-flux] turn {} generated {ms}ms for {chars} chars ({per}ms/char){flag}", t.turn);
    }
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute(
                "INSERT OR REPLACE INTO ambient_chunks (turn_id, seq, ms, text) VALUES (?1, 0, ?2, ?3)",
                rusqlite::params![t.turn, ms, text.trim()],
            )?;
            c.execute(
                "UPDATE ambient_turns SET ended = 1, chunk_count = 1, text = CASE WHEN TRIM(text) = '' THEN ?2 ELSE text END WHERE id = ?1",
                rusqlite::params![t.turn, text.trim()],
            )?;
            Ok(())
        });
    }
    if stopped {
        return;
    }
    send_phone(&t.device, PhoneFrame::Text(json!({"t": "tts_end", "turn": t.turn, "ms": ms}).to_string()));
    emit(json!({"kind": "ambient_speech_end", "turn": t.turn, "count": 1, "ms": ms, "device": t.device, "stream": true}));
    crate::ambient::emit_state("speech synthesised");
}

fn prune() {
    let stale: Vec<Arc<StreamTurn>> = {
        let mut live = LIVE.lock().unwrap();
        let (gone, keep): (Vec<_>, Vec<_>) = live.drain(..).partition(|t| {
            let i = t.inner.lock().unwrap();
            let Some(ended) = i.ended_at else { return false };
            let last = i.heard.map(|(_, at)| at.max(ended)).unwrap_or(ended);
            let ms = i.audio_ms.unwrap_or(0).max(0) as u64;
            let waited = if i.heard.is_some() { LINGER } else { LINGER + Duration::from_millis(ms) };
            last.elapsed() > waited || !has_phone(&t.device)
        });
        *live = keep;
        gone
    };
    for t in stale {
        if t.inner.lock().unwrap().heard.is_some() {
            mark_played(&t);
        } else {
            set_status(t.turn, "missed");
        }
    }
}

pub fn busy() -> bool {
    prune();
    !LIVE.lock().unwrap().is_empty()
}

pub fn busy_for(device: &str) -> bool {
    prune();
    LIVE.lock().unwrap().iter().any(|t| t.device == device)
}

pub fn audible(device: &str) -> bool {
    prune();
    let turns: Vec<Arc<StreamTurn>> = LIVE.lock().unwrap().iter().filter(|t| t.device == device).cloned().collect();
    if turns.is_empty() {
        return false;
    }
    if turns.iter().any(|t| {
        let i = t.inner.lock().unwrap();
        !i.stopped && (i.ended_at.is_none() || i.ended_at.is_some_and(|at| at.elapsed() < POS_FRESH))
    }) {
        return true;
    }
    positions()
        .lock()
        .unwrap()
        .get(device)
        .is_some_and(|p| !p.done && p.at.elapsed() < POS_FRESH && turns.iter().any(|t| t.turn == p.turn))
}

fn estimate(text: &str, played_ms: i64, total_ms: i64) -> String {
    let text = text.trim();
    if played_ms <= 0 || total_ms <= 0 || text.is_empty() {
        return String::new();
    }
    if played_ms >= total_ms {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut cut = (chars.len() as f64 * played_ms as f64 / total_ms as f64).floor() as usize;
    while cut > 0 && cut < chars.len() && !chars[cut].is_whitespace() {
        cut -= 1;
    }
    chars[..cut].iter().collect::<String>().trim().to_string()
}

fn heard_estimate(t: &StreamTurn, played: Option<i64>) -> String {
    let i = t.inner.lock().unwrap();
    let played = played.or(i.heard.map(|(ms, _)| ms)).unwrap_or(0);
    let total = i.audio_ms.unwrap_or((i.bytes / 48) as i64);
    estimate(&i.text, played, total)
}

fn write_heard(turn: i64, status: &str, heard: &str) {
    if let Ok(store) = crate::ambient::store() {
        let _ = store.with_connection(|c| {
            c.execute(
                "UPDATE ambient_turns SET status = ?2, heard_text = ?3 WHERE id = ?1 AND status IN ('speaking', 'interrupted', 'cancelled')",
                rusqlite::params![turn, status, heard],
            )?;
            Ok(())
        });
    }
}

pub fn stop(device: Option<&str>, reason: &str, hint: Option<(i64, i64)>) -> Vec<i64> {
    prune();
    let turns: Vec<Arc<StreamTurn>> = {
        let mut live = LIVE.lock().unwrap();
        let (hit, keep): (Vec<_>, Vec<_>) = live.drain(..).partition(|t| device.is_none_or(|d| t.device == d));
        *live = keep;
        hit
    };
    if turns.is_empty() {
        return Vec::new();
    }
    let stopped_at = Instant::now();
    let status = if reason == "interrupted" { "interrupted" } else { "cancelled" };
    let mut devices: Vec<String> = turns.iter().map(|t| t.device.clone()).collect();
    devices.sort();
    devices.dedup();
    for d in &devices {
        send_phone(d, PhoneFrame::Text(json!({"t": "tts_stop", "turn": 0}).to_string()));
    }
    for t in &turns {
        let played = hint.filter(|(turn, _)| *turn == t.turn).map(|(_, ms)| ms);
        let guess = heard_estimate(t, played);
        let mut i = t.inner.lock().unwrap();
        i.stopped = true;
        i.flushed = true;
        drop(i);
        write_heard(t.turn, status, &guess);
        emit(json!({"kind": "ambient_speech_cancel", "turn": t.turn, "after_seq": -1, "reason": reason, "device": t.device, "stream": true}));
    }
    let mut sessions: Vec<Arc<Session>> = Vec::new();
    for t in &turns {
        if !sessions.iter().any(|s| Arc::ptr_eq(s, &t.session)) {
            sessions.push(t.session.clone());
        }
    }
    let mut active: Vec<(Arc<Session>, Arc<StreamTurn>)> = Vec::new();
    for s in sessions {
        let generating = {
            let mut st = s.st.lock().unwrap();
            let front = st.queue.front().cloned();
            if front.is_some() {
                st.retired = true;
            }
            front
        };
        if let Some(f) = generating {
            let mut map = hub().lock().unwrap();
            if map.get(&s.device).is_some_and(|x| Arc::ptr_eq(x, &s)) {
                map.remove(&s.device);
            }
            drop(map);
            active.push((s, f));
        }
    }
    for (s, _) in &active {
        if has_phone(&s.device) {
            let _ = session(&s.device);
        }
    }
    let ids: Vec<i64> = turns.iter().map(|t| t.turn).collect();
    eprintln!("[ambient-flux] stop reason={reason} turns={ids:?} generating={}", active.len());
    RECONCILING.fetch_add(1, Ordering::SeqCst);
    let reason = reason.to_string();
    tokio::spawn(async move {
        reconcile(turns, active, devices, stopped_at, hint, status, reason == "interrupted").await;
        RECONCILING.fetch_sub(1, Ordering::SeqCst);
    });
    ids
}

async fn reconcile(
    turns: Vec<Arc<StreamTurn>>,
    active: Vec<(Arc<Session>, Arc<StreamTurn>)>,
    devices: Vec<String>,
    stopped_at: Instant,
    hint: Option<(i64, i64)>,
    status: &'static str,
    interrupted: bool,
) {
    let waiting: Vec<&String> = devices.iter().filter(|d| has_phone(d)).collect();
    let deadline = Instant::now() + POS_WAIT;
    while !waiting.is_empty() && Instant::now() < deadline {
        let fresh = {
            let pos = positions().lock().unwrap();
            waiting.iter().all(|d| pos.get(*d).is_some_and(|p| p.at >= stopped_at))
        };
        if fresh {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut heard_by_turn: HashMap<i64, i64> = HashMap::new();
    let mut reached: HashMap<String, i64> = HashMap::new();
    {
        let pos = positions().lock().unwrap();
        for d in &devices {
            if let Some(p) = pos.get(d).filter(|p| turns.iter().any(|t| t.turn == p.turn)) {
                heard_by_turn.insert(p.turn, p.ms);
                reached.insert(d.clone(), p.turn);
            }
        }
    }
    if let Some((turn, ms)) = hint {
        heard_by_turn.entry(turn).or_insert(ms);
    }
    let mut spoken: HashMap<i64, String> = HashMap::new();
    for (s, current) in &active {
        let at = turns
            .iter()
            .filter(|t| t.device == s.device && Arc::ptr_eq(&t.session, s))
            .find_map(|t| heard_by_turn.get(&t.turn).map(|ms| (t, *ms)))
            .map(|(t, ms)| t.inner.lock().unwrap().start_offset + ms)
            .unwrap_or(0);
        let (tx, rx) = oneshot::channel();
        s.st.lock().unwrap().interrupted = Some(tx);
        let sent = Instant::now();
        s.send(json!({"type": "Interrupt", "playback_offset": {"type": "time_ms", "value": at}}));
        match tokio::time::timeout(INTERRUPT_WAIT, rx).await {
            Ok(Ok(v)) => {
                eprintln!(
                    "[ambient-flux] interrupt turn={} offset={at} played={} after {}ms",
                    current.turn,
                    v.get("audio_played_ms").and_then(|x| x.as_i64()).unwrap_or(-1),
                    sent.elapsed().as_millis()
                );
                if heard_by_turn.contains_key(&current.turn) {
                    if let Some(text) = v.get("text_spoken").and_then(|x| x.as_str()) {
                        spoken.insert(current.turn, text.trim().to_string());
                    }
                }
            }
            _ => eprintln!("[ambient-flux] no SpeechInterrupted for turn {}", current.turn),
        }
        let _ = s.cmd.send(Cmd::Close);
    }
    let mut last = String::new();
    for t in &turns {
        let heard = match (spoken.remove(&t.turn), heard_by_turn.get(&t.turn)) {
            (Some(text), _) => text,
            (None, Some(ms)) => heard_estimate(t, Some(*ms)),
            (None, None) if reached.get(&t.device).is_some_and(|r| *r > t.turn) => t.text().trim().to_string(),
            (None, None) => String::new(),
        };
        write_heard(t.turn, status, &heard);
        if !heard.is_empty() {
            last = heard;
        }
    }
    if interrupted {
        crate::speech::note_stream_interruption(last);
    }
}

pub fn is_reconciling() -> bool {
    RECONCILING.load(Ordering::SeqCst) > 0
}

pub async fn settle(max: Duration) {
    let deadline = Instant::now() + max;
    while RECONCILING.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}
