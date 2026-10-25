use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::Shared;

const BPS: f64 = 32_000.0;
const RING: usize = 32_000 * 180;
const SEND_AT: u64 = 1_760;
const MIN_SEND: u64 = 1_760;
const MAX_SEND: u64 = 24_000;
const REPLAY: u64 = 32_000 * 10;
const PAD: u64 = 9_600;
const IDLE_CLOSE: Duration = Duration::from_secs(60);
const URGENT: &[&str] = &["stop", "wait", "hold", "pause", "no", "nope", "sorry", "actually", "hang", "snippet", "chief", "enough", "shut", "quiet"];
const SAMPLE: Duration = Duration::from_millis(250);

enum Cmd {
    Audio(Vec<u8>),
    Config(String),
    Force,
}

struct Conn {
    id: u64,
    tx: Option<mpsc::UnboundedSender<Cmd>>,
    base: u64,
    sent: u64,
    active: bool,
}

struct Live {
    conn: u64,
    turn: i64,
    seq: i64,
    interrupted: bool,
}

#[derive(Default)]
struct Inner {
    ring: VecDeque<u8>,
    ring_start: u64,
    g: u64,
    anchors: VecDeque<(u64, f64)>,
    playing: VecDeque<(u64, bool)>,
    media: VecDeque<(u64, bool)>,
    play_flag: bool,
    media_flag: bool,
    audible: bool,
    sampled: Option<Instant>,
    conn: Option<Conn>,
    retry_at: Option<Instant>,
    failures: u32,
    pending_from: u64,
    last_final: u64,
    live: Option<Live>,
    stream: Option<u32>,
}

pub(super) struct Listener {
    device: String,
    d: Shared,
    inner: Mutex<Inner>,
}

fn hub() -> &'static Mutex<HashMap<String, Arc<Listener>>> {
    static HUB: OnceLock<Mutex<HashMap<String, Arc<Listener>>>> = OnceLock::new();
    HUB.get_or_init(Default::default)
}

fn get(d: &Shared, device: &str) -> Arc<Listener> {
    hub()
        .lock()
        .unwrap()
        .entry(device.to_string())
        .or_insert_with(|| Arc::new(Listener { device: device.to_string(), d: d.clone(), inner: Mutex::new(Inner::default()) }))
        .clone()
}

fn next_seq() -> i64 {
    static SEQ: AtomicI64 = AtomicI64::new(0);
    let now = chrono::Utc::now().timestamp_millis();
    let mut prev = SEQ.load(Ordering::SeqCst);
    loop {
        let next = (prev + 1).max(now);
        match SEQ.compare_exchange(prev, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn credentials(d: &Shared) -> Option<(String, Vec<String>)> {
    let c = d.config.lock().unwrap();
    let key = c.assemblyai_api_key.clone().filter(|k| !k.trim().is_empty())?;
    let mut terms = c.ambient.wake_phrases.clone();
    for extra in ["Snippet", "Mission Control"] {
        if !terms.iter().any(|t| t.eq_ignore_ascii_case(extra)) {
            terms.push(extra.to_string());
        }
    }
    Some((key, terms))
}

fn active_mode() -> bool {
    crate::ambient::conversation().is_some() || crate::ambient::expectation().is_some()
}

fn silences(active: bool) -> (u32, u32) {
    if active { (160, 900) } else { (260, 1600) }
}

fn at(marks: &VecDeque<(u64, bool)>, g: u64) -> bool {
    marks.iter().rev().find(|(m, _)| *m <= g).map(|(_, v)| *v).unwrap_or(false)
}

fn trim(marks: &mut VecDeque<(u64, bool)>, from: u64) {
    while marks.len() > 1 && marks[1].0 <= from {
        marks.pop_front();
    }
}

fn set(marks: &mut VecDeque<(u64, bool)>, g: u64, on: bool) {
    if marks.back().map(|(_, v)| *v) != Some(on) {
        marks.push_back((g, on));
    }
}

pub(super) fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

pub(super) async fn open(d: &Shared, device: &str, h: u32, started_at: Option<&str>) -> Result<(), u16> {
    d.reload_config().await;
    if !d.config.lock().unwrap().ambient.enabled {
        return Err(403);
    }
    if device.is_empty() {
        return Err(400);
    }
    if !crate::ambient::lease_touch(device, None, false) {
        return Err(409);
    }
    if credentials(d).is_none() {
        return Err(503);
    }
    let wall = started_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
    let l = get(d, device);
    l.finish_stream(false);
    let mut s = l.inner.lock().unwrap();
    let g = s.g;
    s.anchors.push_back((g, wall));
    while s.anchors.len() > 64 {
        s.anchors.pop_front();
    }
    s.stream = Some(h);
    drop(s);
    l.ensure();
    Ok(())
}

pub(super) fn audio(device: &str, h: u32, pcm: &[u8]) -> bool {
    let Some(l) = hub().lock().unwrap().get(device).cloned() else { return false };
    {
        let mut s = l.inner.lock().unwrap();
        if s.stream != Some(h) {
            return false;
        }
        s.ring.extend(pcm.iter().copied());
        s.g += pcm.len() as u64;
        if s.ring.len() > RING {
            let n = s.ring.len() - RING;
            s.ring.drain(..n);
            s.ring_start += n as u64;
            let from = s.ring_start;
            trim(&mut s.playing, from);
            trim(&mut s.media, from);
        }
    }
    l.sample();
    l.ensure();
    l.pump(false);
    true
}

pub(super) fn close(device: &str, h: Option<u32>) {
    let Some(l) = hub().lock().unwrap().get(device).cloned() else { return };
    if h.is_none() || l.inner.lock().unwrap().stream == h {
        l.finish_stream(true);
    }
}

pub(super) fn flag(device: &str, kind: &str, on: bool) {
    let Some(l) = hub().lock().unwrap().get(device).cloned() else { return };
    let mut s = l.inner.lock().unwrap();
    let g = s.g;
    match kind {
        "media" => {
            s.media_flag = on;
            set(&mut s.media, g, on);
        }
        _ => {
            s.play_flag = on;
            let v = on || s.audible;
            set(&mut s.playing, g, v);
        }
    }
}

impl Listener {
    fn finish_stream(&self, force: bool) {
        let mut s = self.inner.lock().unwrap();
        if s.stream.take().is_none() {
            return;
        }
        let unsent = s.conn.as_ref().map(|c| s.g - c.sent).unwrap_or(0);
        if unsent > 0 && unsent < MIN_SEND {
            let pad = (MIN_SEND - unsent) as usize;
            s.ring.extend(std::iter::repeat_n(0u8, pad));
            s.g += pad as u64;
        }
        drop(s);
        self.pump(true);
        if force {
            if let Some(tx) = self.inner.lock().unwrap().conn.as_ref().and_then(|c| c.tx.clone()) {
                let _ = tx.send(Cmd::Force);
            }
        }
    }

    fn sample(&self) {
        let due = {
            let s = self.inner.lock().unwrap();
            s.sampled.is_none_or(|t| t.elapsed() >= SAMPLE)
        };
        if !due {
            return;
        }
        let audible = crate::speech::is_audible(&self.device);
        let active = active_mode();
        let mut s = self.inner.lock().unwrap();
        s.sampled = Some(Instant::now());
        s.audible = audible;
        let (g, v) = (s.g, audible || s.play_flag);
        set(&mut s.playing, g, v);
        if let Some(c) = s.conn.as_mut()
            && c.active != active
            && let Some(tx) = c.tx.as_ref()
        {
            c.active = active;
            let (min, max) = silences(active);
            let _ = tx.send(Cmd::Config(
                serde_json::json!({"type":"UpdateConfiguration","min_turn_silence":min,"max_turn_silence":max}).to_string(),
            ));
        }
    }

    fn ensure(self: &Arc<Self>) {
        static IDS: AtomicU64 = AtomicU64::new(1);
        let mut s = self.inner.lock().unwrap();
        if s.conn.is_some() || s.stream.is_none() || s.retry_at.is_some_and(|t| t > Instant::now()) {
            return;
        }
        let Some((key, terms)) = credentials(&self.d) else { return };
        let id = IDS.fetch_add(1, Ordering::SeqCst);
        let base = s.pending_from.max(s.ring_start).min(s.g);
        let active = active_mode();
        s.conn = Some(Conn { id, tx: None, base, sent: base, active });
        drop(s);
        let me = self.clone();
        tokio::spawn(async move {
            let (tx, rx) = mpsc::unbounded_channel();
            let ws = connect(&key, &terms, active).await;
            let ws = match ws {
                Ok(ws) => {
                    {
                        let mut s = me.inner.lock().unwrap();
                        s.failures = 0;
                        s.retry_at = None;
                        if let Some(c) = s.conn.as_mut().filter(|c| c.id == id) {
                            c.tx = Some(tx);
                        }
                    }
                    me.pump(false);
                    ws
                }
                Err(e) => {
                    eprintln!("[listener] {} connect failed: {e}", me.device);
                    me.dropped(id, false);
                    return;
                }
            };
            let idle = run(&me, id, ws, rx).await;
            if let Err(e) = &idle {
                eprintln!("[listener] {} stream ended: {e}", me.device);
            }
            me.dropped(id, idle.is_ok());
        });
    }

    fn dropped(self: &Arc<Self>, id: u64, idle: bool) {
        let mut s = self.inner.lock().unwrap();
        let Some(c) = s.conn.take_if(|c| c.id == id) else { return };
        if idle {
            s.pending_from = c.sent;
        } else {
            s.failures += 1;
            let wait = Duration::from_millis((500u64 << s.failures.min(6)).min(30_000));
            s.retry_at = Some(Instant::now() + wait);
            s.pending_from = s.last_final.max(c.base).max(s.g.saturating_sub(REPLAY));
        }
        let stale = s.live.take();
        drop(s);
        if let Some(live) = stale {
            super::ambient_routes::announce(&self.d, &self.device, live.seq, true);
        }
    }

    fn pump(&self, all: bool) {
        let mut s = self.inner.lock().unwrap();
        let (g, ring_start) = (s.g, s.ring_start);
        let Some(c) = s.conn.as_ref() else { return };
        let Some(tx) = c.tx.clone() else { return };
        let mut sent = c.sent.max(ring_start);
        let mut chunks = Vec::new();
        while g - sent >= SEND_AT || (all && g > sent) {
            let n = (g - sent).min(MAX_SEND);
            let from = (sent - ring_start) as usize;
            chunks.push(s.ring.range(from..from + n as usize).copied().collect::<Vec<u8>>());
            sent += n;
        }
        if let Some(c) = s.conn.as_mut() {
            c.sent = sent;
        }
        drop(s);
        for chunk in chunks {
            let _ = tx.send(Cmd::Audio(chunk));
        }
    }

    fn on_turn(&self, conn: u64, v: &serde_json::Value) {
        let Some(order) = v.get("turn_order").and_then(|x| x.as_i64()) else { return };
        let ended = v.get("end_of_turn").and_then(|x| x.as_bool()).unwrap_or(false);
        let transcript = v.get("transcript").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        let mut s = self.inner.lock().unwrap();
        let Some(base) = s.conn.as_ref().filter(|c| c.id == conn).map(|c| c.base) else { return };
        let words: Vec<Heard> = v
            .get("words")
            .and_then(|x| x.as_array())
            .map(|ws| {
                ws.iter()
                    .filter_map(|w| {
                        let start = base + (w.get("start")?.as_f64()? / 1000.0 * BPS) as u64 & !1;
                        let end = base + (w.get("end")?.as_f64()? / 1000.0 * BPS) as u64 & !1;
                        let text = w.get("text")?.as_str()?.to_string();
                        let mid = (start + end) / 2;
                        Some(Heard { key: norm(&text), text, start, end, playing: at(&s.playing, mid), media: at(&s.media, mid) })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fresh = s.live.as_ref().is_none_or(|l| l.conn != conn || l.turn != order);
        if fresh {
            if ended && words.is_empty() && transcript.is_empty() {
                return;
            }
            s.live = Some(Live { conn, turn: order, seq: next_seq(), interrupted: false });
        }
        let live = s.live.as_mut().unwrap();
        let seq = live.seq;
        let replies = super::ambient_routes::recent_replies(&self.device);
        let mask = echo_mask(&words, &replies);
        let real = words.iter().zip(&mask).filter(|(_, e)| !**e).count();
        let vocab: std::collections::HashSet<String> = replies.iter().flat_map(|r| r.split_whitespace().map(norm)).collect();
        let novel = words.iter().zip(&mask).filter(|(w, e)| !**e && !vocab.contains(&w.key)).count();
        let audible = s.audible || s.play_flag;
        let live = s.live.as_mut().unwrap();
        let urgent = words.iter().zip(&mask).any(|(w, e)| !*e && URGENT.contains(&w.key.as_str()));
        let barge = !live.interrupted && ((novel >= 1 && real >= 2) || (urgent && real >= 1) || (ended && novel >= 1)) && {
            let busy = crate::ambient::conversation().is_some() && super::autonomy::mission_control_busy();
            audible || busy
        };
        if barge {
            live.interrupted = true;
        }
        if !ended {
            drop(s);
            if fresh {
                super::ambient_routes::announce(&self.d, &self.device, seq, false);
            }
            if barge {
                self.barge_in(&words, &mask);
            }
            return;
        }
        s.live = None;
        if let Some(last) = words.last() {
            s.last_final = s.last_final.max(last.end);
        }
        let tail_ms = s.g.saturating_sub(words.last().map(|w| w.end).unwrap_or(s.g)) / 32;
        let (first_g, last_g) = match (words.first(), words.last()) {
            (Some(f), Some(l)) => (f.start, l.end),
            _ => (s.g, s.g),
        };
        let opened = s.anchors.iter().rev().find(|(a, _)| *a <= last_g).map(|(a, _)| *a).unwrap_or(0);
        let first_g = first_g.max(opened).min(last_g);
        let wall = s.anchors.iter().rev().find(|(a, _)| *a <= first_g).or(s.anchors.front()).map(|(a, w)| w + (first_g as f64 - *a as f64) / BPS);
        let from = first_g.saturating_sub(PAD).max(s.ring_start);
        let to = (last_g + PAD).min(s.g);
        let pcm: Vec<u8> = if to > from {
            s.ring.range((from - s.ring_start) as usize..(to - s.ring_start) as usize).copied().collect()
        } else {
            Vec::new()
        };
        drop(s);
        if barge {
            self.barge_in(&words, &mask);
        }
        let echo_all = !words.is_empty() && real == 0;
        let media = !words.is_empty() && words.iter().filter(|w| w.media).count() * 2 > words.len();
        let during_playback = words.iter().any(|w| w.playing);
        let text = if words.is_empty() || real == words.len() {
            if transcript.is_empty() { words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ") } else { transcript.clone() }
        } else {
            words.iter().zip(&mask).filter(|(_, e)| !**e).map(|(w, _)| w.text.as_str()).collect::<Vec<_>>().join(" ")
        };
        let (status, text) = if echo_all {
            ("echo", transcript.clone())
        } else if media {
            ("media", text)
        } else {
            let spoken = super::transcribe::finish_text(&text).text;
            if spoken.is_empty() { ("empty", String::new()) } else { ("done", spoken) }
        };
        if real < words.len() {
            eprintln!("[listener] {} echo filtered {}/{} words: {transcript:?} -> {text:?}", self.device, words.len() - real, words.len());
        }
        let started = wall.unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
        let duration_ms = ((last_g.saturating_sub(first_g)) as f64 / BPS * 1000.0) as i64;
        let id = uuid::Uuid::new_v4().to_string();
        crate::ambient::mark(&id, "end");
        eprintln!("[listener] {} final after {tail_ms}ms of trailing audio", self.device);
        let event = Event {
            id,
            seq,
            status,
            text,
            started,
            duration_ms,
            during_playback,
            media,
            pcm,
        };
        let (d, device) = (self.d.clone(), self.device.clone());
        tokio::spawn(async move { super::ambient_routes::record_heard(&d, &device, event).await });
    }

    fn barge_in(&self, words: &[Heard], mask: &[bool]) {
        let heard: Vec<&str> = words.iter().zip(mask).filter(|(_, e)| !**e).map(|(w, _)| w.text.as_str()).collect();
        let lag = words.last().map(|w| self.inner.lock().unwrap().g.saturating_sub(w.end) / 32).unwrap_or(0);
        eprintln!("[ambient-speech] barge-in {} heard={:?} audio_lag={lag}ms", self.device, heard.join(" "));
        let (d, device) = (self.d.clone(), self.device.clone());
        tokio::spawn(async move {
            let spoke = crate::speech::interrupt(&device);
            let busy = crate::ambient::conversation().is_some() && super::autonomy::mission_control_busy();
            if spoke || busy {
                super::autonomy::interrupt_mission_control(&d).await;
            }
        });
    }
}

pub(super) struct Event {
    pub id: String,
    pub seq: i64,
    pub status: &'static str,
    pub text: String,
    pub started: f64,
    pub duration_ms: i64,
    pub during_playback: bool,
    pub media: bool,
    pub pcm: Vec<u8>,
}

struct Heard {
    text: String,
    key: String,
    start: u64,
    end: u64,
    playing: bool,
    media: bool,
}

fn norm(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = (row[j] + 1).min(row[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
            prev = cur;
        }
    }
    row[b.len()]
}

fn similar(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (a.chars().count(), b.chars().count());
    if la.min(lb) < 3 {
        return false;
    }
    if (a.starts_with(b) || b.starts_with(a)) && la.min(lb) >= 3 {
        return true;
    }
    lev(a, b) <= (la.max(lb) / 4).max(1)
}

fn echo_mask(words: &[Heard], replies: &[String]) -> Vec<bool> {
    let mut mask = vec![false; words.len()];
    if !words.iter().any(|w| w.playing) {
        return mask;
    }
    for reply in replies {
        let said: Vec<String> = reply.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
        let mut i = 0;
        while i < words.len() {
            let mut best: Option<(usize, usize)> = None;
            for j in 0..said.len() {
                let (mut k, mut skipped, mut matched, mut last_match) = (0usize, 0usize, 0usize, 0usize);
                while i + k < words.len() && j + k + skipped < said.len() {
                    if similar(&words[i + k].key, &said[j + k + skipped]) {
                        matched += 1;
                        last_match = k + 1;
                        k += 1;
                    } else if skipped < 2 && j + k + skipped + 1 < said.len() && similar(&words[i + k].key, &said[j + k + skipped + 1]) {
                        skipped += 1;
                    } else if k + 1 < words.len() - i && matched >= 2 && words[i + k].playing {
                        k += 1;
                        if k - last_match > 1 {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                if last_match > best.map(|b| b.1).unwrap_or(0) {
                    best = Some((matched, last_match));
                }
            }
            match best {
                Some((matched, len)) if matched >= 2 || (matched == 1 && len == 1 && words.len() == 1 && words[i].playing) => {
                    let playing = words[i..i + len].iter().filter(|w| w.playing).count();
                    if playing * 2 >= len {
                        mask[i..i + len].iter_mut().for_each(|m| *m = true);
                    }
                    i += len.max(1);
                }
                _ => i += 1,
            }
        }
    }
    for gap in [1usize, 2] {
        for i in 1..words.len().saturating_sub(gap) {
            let end = i + gap;
            if end < words.len() && mask[i - 1] && mask[end] && (i..end).all(|k| !mask[k] && words[k].playing) {
                (i..end).for_each(|k| mask[k] = true);
            }
        }
    }
    mask
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(key: &str, terms: &[String], active: bool) -> Result<Ws, String> {
    let keyterms = serde_json::to_string(terms).map_err(|e| e.to_string())?;
    let (min, max) = silences(active);
    let url = format!(
        "wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro&mode=balanced&min_turn_silence={min}&max_turn_silence={max}&interruption_delay=80&language_codes={}&keyterms_prompt={}",
        urlencoding::encode("[\"en\"]"),
        urlencoding::encode(&keyterms)
    );
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert("Authorization", key.trim().parse().map_err(|_| "invalid AssemblyAI key".to_string())?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(6), tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "AssemblyAI connect timed out".to_string())?
        .map_err(|e| format!("AssemblyAI connect failed: {e}"))?;
    Ok(ws)
}

async fn run(l: &Arc<Listener>, id: u64, ws: Ws, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<(), String> {
    let (mut tx, mut rx) = ws.split();
    let mut last_audio = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut pinged = Instant::now();
    loop {
        tokio::select! {
            c = cmd.recv() => match c {
                Some(Cmd::Audio(b)) => {
                    last_audio = Instant::now();
                    tx.send(Message::Binary(b.into())).await.map_err(|_| "AssemblyAI connection dropped".to_string())?;
                }
                Some(Cmd::Config(text)) => {
                    let _ = tx.send(Message::Text(text.into())).await;
                }
                Some(Cmd::Force) => {
                    let _ = tx.send(Message::Text("{\"type\":\"ForceEndpoint\"}".into())).await;
                }
                None => break,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    match v.get("type").and_then(|x| x.as_str()) {
                        Some("Turn") => l.on_turn(id, &v),
                        Some("Termination") => return Err("AssemblyAI ended the session".into()),
                        Some(other) if other.eq_ignore_ascii_case("error") => return Err(format!("AssemblyAI error: {t}")),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(f))) => return Err(format!("AssemblyAI closed the connection: {f:?}")),
                None => return Err("AssemblyAI closed the connection".into()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("AssemblyAI stream error: {e}")),
            },
            _ = tick.tick() => {
                if pinged.elapsed() >= Duration::from_secs(10) {
                    pinged = Instant::now();
                    let _ = tx.send(Message::Ping(Default::default())).await;
                }
                let open = l.inner.lock().unwrap().stream.is_some();
                if !open && last_audio.elapsed() >= IDLE_CLOSE {
                    break;
                }
            }
        }
    }
    l.inner.lock().unwrap().conn.as_mut().filter(|c| c.id == id).map(|c| c.tx = None);
    let _ = tx.send(Message::Text("{\"type\":\"Terminate\"}".into())).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(deadline, rx.next()).await {
        if let Message::Text(t) = m
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
        {
            match v.get("type").and_then(|x| x.as_str()) {
                Some("Turn") => l.on_turn(id, &v),
                Some("Termination") => break,
                _ => {}
            }
        }
    }
    Ok(())
}
