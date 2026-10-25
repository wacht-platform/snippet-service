use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const CHUNK: usize = 2560;
const BYTES_PER_SEC: f32 = 32_000.0;
const CATCH_UP: f32 = 0.1;
const TAIL: f32 = 0.4;
const GAP_FILL: f32 = 2.0;
const MAX_AHEAD: f32 = 1.0;
const EDGE: f32 = 0.05;
const FORCE_AFTER: Duration = Duration::from_millis(300);
const GIVE_UP: Duration = Duration::from_millis(3000);
const GIVE_UP_MAX: Duration = Duration::from_secs(10);
const LIVE_WAIT: Duration = Duration::from_millis(1500);
const LIVE_MAX: Duration = Duration::from_millis(3500);
const DECODE_RATE: f32 = 1.3;
const ASSEMBLY_WAIT: Duration = Duration::from_secs(15);
const IDLE_CLOSE: Duration = Duration::from_secs(300);
const PING_EVERY: Duration = Duration::from_secs(20);
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const MAX_KEYTERMS: usize = 100;
const KEEP_WORDS: usize = 600;

static NEXT_SEG: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, PartialEq)]
pub(super) struct Params {
    key: String,
    eot: f32,
    eager: Option<f32>,
    keyterms: Vec<String>,
}

pub(super) fn params(c: &crate::config::SnippetConfig) -> Option<Params> {
    let key = c.deepgram_api_key.as_deref().map(str::trim).filter(|k| !k.is_empty())?.to_string();
    let eot = c.ambient.flux_eot_threshold.clamp(0.5, 1.0);
    let eager = Some(c.ambient.flux_eager_eot_threshold).filter(|e| *e >= 0.3).map(|e| e.min(0.9).min(eot));
    let mut keyterms: Vec<String> = Vec::new();
    for t in c.ambient.wake_phrases.iter().chain(c.ambient.flux_keyterms.iter()) {
        let t = t.trim();
        if !t.is_empty() && !keyterms.iter().any(|k| k.eq_ignore_ascii_case(t)) {
            keyterms.push(t.to_string());
        }
    }
    keyterms.truncate(MAX_KEYTERMS);
    Some(Params { key, eot, eager, keyterms })
}


struct Outcome {
    text: String,
    words: Vec<crate::stt::Word>,
    seconds: f32,
    eager: Option<i64>,
    eot: Option<i64>,
    trigger: String,
    timing: String,
}

enum Cmd {
    Begin(u64, Instant),
    Audio(u64, Vec<u8>),
    End { seg: u64, at: Instant, live: bool, reply: oneshot::Sender<Outcome> },
    Abort(u64),
    Configure(String),
    Close,
}

struct Session {
    cmd: mpsc::UnboundedSender<Cmd>,
    closed: Mutex<bool>,
    params: Mutex<Params>,
}

enum Failure {
    Connect(String),
    Stream(String),
}

#[derive(Default)]
struct Health {
    fails: u32,
    until: Option<Instant>,
}

fn hub() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static HUB: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    HUB.get_or_init(Default::default)
}

fn health() -> &'static Mutex<HashMap<String, Health>> {
    static H: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

fn connect_failed(device: &str, reason: &str) {
    let mut map = health().lock().unwrap();
    let h = map.entry(device.to_string()).or_default();
    h.fails += 1;
    let wait = BACKOFF_BASE.saturating_mul(1 << (h.fails - 1).min(6)).min(BACKOFF_MAX);
    h.until = Some(Instant::now() + wait);
    if h.fails == 1 || h.fails.is_power_of_two() {
        eprintln!("[ambient-flux-stt] {device} connect failed ({} in a row), retrying after {}s: {reason}", h.fails, wait.as_secs());
    }
}

fn connected(device: &str) {
    health().lock().unwrap().remove(device);
}

fn configure_message(p: &Params) -> String {
    let mut thresholds = serde_json::json!({ "eot_threshold": p.eot });
    if let Some(e) = p.eager {
        thresholds["eager_eot_threshold"] = serde_json::json!(e);
    }
    serde_json::json!({ "type": "Configure", "thresholds": thresholds, "keyterms": p.keyterms }).to_string()
}

fn session(device: &str, p: Params) -> Option<Arc<Session>> {
    if health().lock().unwrap().get(device).and_then(|h| h.until).is_some_and(|u| Instant::now() < u) {
        return None;
    }
    let mut map = hub().lock().unwrap();
    if let Some(existing) = map.get(device).filter(|s| !*s.closed.lock().unwrap()) {
        let mut current = existing.params.lock().unwrap();
        if current.key != p.key || current.eager.is_some() != p.eager.is_some() {
            let _ = existing.cmd.send(Cmd::Close);
        } else {
            if *current != p {
                let _ = existing.cmd.send(Cmd::Configure(configure_message(&p)));
                *current = p.clone();
            }
            return Some(existing.clone());
        }
    }
    let (cmd, rx) = mpsc::unbounded_channel();
    let session = Arc::new(Session { cmd, closed: Mutex::new(false), params: Mutex::new(p.clone()) });
    map.insert(device.to_string(), session.clone());
    let (task, name) = (session.clone(), device.to_string());
    tokio::spawn(async move {
        let result = run(&name, p, rx).await;
        *task.closed.lock().unwrap() = true;
        match result {
            Err(Failure::Connect(e)) => connect_failed(&name, &e),
            Err(Failure::Stream(e)) => eprintln!("[ambient-flux-stt] {name} connection lost, reconnecting on next segment: {e}"),
            Ok(()) => {}
        }
        let mut map = hub().lock().unwrap();
        if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &task)) {
            map.remove(&name);
        }
    });
    Some(session)
}

pub(super) fn prewarm(device: &str, p: Params) {
    let _ = session(device, p);
}

pub(super) fn begin(device: &str, p: Params) -> Option<Ticket> {
    let session = session(device, p)?;
    let seg = NEXT_SEG.fetch_add(1, Ordering::Relaxed);
    session.cmd.send(Cmd::Begin(seg, Instant::now())).ok()?;
    Some(Ticket { session: Some(session), seg, buf: Vec::new() })
}

pub(super) struct Ticket {
    session: Option<Arc<Session>>,
    seg: u64,
    buf: Vec<u8>,
}

fn short(id: &str) -> String {
    id[..id.len().min(8)].to_string()
}

fn log_outcome(id: &str, o: &Outcome, tag: &str) {
    let ms = |v: Option<i64>| v.map(|m| format!("{m:+}")).unwrap_or_else(|| "-".into());
    eprintln!("[ambient-flux-stt] {} eager_eot {} eot {} ({}{tag}) text={:?}", short(id), ms(o.eager), ms(o.eot), o.trigger, o.text);
    eprintln!("[ambient-flux-stt] {} timing: {}", short(id), o.timing);
    let n = words(&o.text).len();
    if n <= 1 && o.seconds >= 1.5 {
        eprintln!("[ambient-flux-stt] {} short: {n} word(s) for {:.1}s of audio", short(id), o.seconds);
    }
}

impl Ticket {
    pub fn push(&mut self, pcm: &[u8]) {
        let Some(session) = self.session.as_ref() else { return };
        self.buf.extend_from_slice(pcm);
        while self.buf.len() >= CHUNK {
            let rest = self.buf.split_off(CHUNK);
            let _ = session.cmd.send(Cmd::Audio(self.seg, std::mem::replace(&mut self.buf, rest)));
        }
    }

    fn close(&mut self, at: Instant, live: bool) -> Option<oneshot::Receiver<Outcome>> {
        let session = self.session.take()?;
        if !self.buf.is_empty() {
            let _ = session.cmd.send(Cmd::Audio(self.seg, std::mem::take(&mut self.buf)));
        }
        let (reply, rx) = oneshot::channel();
        session.cmd.send(Cmd::End { seg: self.seg, at, live, reply }).ok()?;
        Some(rx)
    }

    pub fn end(mut self, id: String, at: Instant, assembly: Option<oneshot::Receiver<Option<String>>>) {
        let Some(rx) = self.close(at, false) else { return };
        tokio::spawn(async move {
            let Ok(flux) = rx.await else { return };
            log_outcome(&id, &flux, "");
            let reference = match assembly {
                Some(rx) => tokio::time::timeout(ASSEMBLY_WAIT, rx).await.ok().and_then(|r| r.ok()).flatten(),
                None => None,
            };
            let s = short(&id);
            match reference {
                Some(asm) => {
                    let (dist, len) = word_distance(&asm, &flux.text);
                    let wer = dist as f32 / len.max(1) as f32;
                    let mine = words(&flux.text).len();
                    let flag = if len >= 3 && (mine as f32) < 0.6 * len as f32 { " SHORT" } else { "" };
                    eprintln!("[ambient-flux-stt] {s} vs assembly: wer={wer:.2} ({dist}/{len}){flag} assembly={asm:?} flux={:?}", flux.text);
                }
                None => eprintln!("[ambient-flux-stt] {s} vs assembly: no assembly transcript flux={:?}", flux.text),
            }
        });
    }

    pub async fn finish(mut self, id: &str, at: Instant) -> Result<(String, Vec<crate::stt::Word>), String> {
        let rx = self.close(at, true).ok_or_else(|| "Flux session closed".to_string())?;
        match tokio::time::timeout_at(tokio::time::Instant::from_std(at + LIVE_MAX + Duration::from_millis(300)), rx).await {
            Ok(Ok(o)) => {
                log_outcome(id, &o, ", live");
                if o.trigger == "gave-up" {
                    return Err("Flux was still decoding at the deadline".into());
                }
                if o.text.trim().is_empty() {
                    return Err("Flux heard no words".into());
                }
                Ok((o.text, o.words))
            }
            Ok(Err(_)) => Err("Flux session dropped".into()),
            Err(_) => Err(format!("Flux gave no transcript within {}ms", LIVE_MAX.as_millis())),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session.cmd.send(Cmd::Abort(self.seg));
        }
    }
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

fn word_distance(reference: &str, hypothesis: &str) -> (usize, usize) {
    let (r, h) = (words(reference), words(hypothesis));
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    (prev[h.len()], r.len())
}


fn turn_words(v: &serde_json::Value) -> Vec<crate::stt::Word> {
    let list: Vec<&serde_json::Value> = v.get("words").and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    let from = v.get("audio_window_start").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
    let to = v.get("audio_window_end").and_then(|x| x.as_f64()).unwrap_or(from as f64) as f32;
    let step = (to - from) / list.len().max(1) as f32;
    list.iter()
        .enumerate()
        .filter_map(|(i, w)| {
            let text = w.get("word").and_then(|x| x.as_str())?.trim().to_string();
            let start = w.get("start").and_then(|x| x.as_f64()).map(|s| s as f32).unwrap_or(from + step * i as f32);
            let end = w.get("end").and_then(|x| x.as_f64()).map(|s| s as f32).unwrap_or(start + step);
            Some(crate::stt::Word { text, start, end })
        })
        .filter(|w| !w.text.is_empty())
        .collect()
}

struct Seg {
    id: u64,
    start: f32,
    end: Option<f32>,
    ended: Option<(Instant, oneshot::Sender<Outcome>)>,
    backlog: f32,
    caught: Option<Instant>,
    forced: bool,
    eager: Option<Instant>,
    eot: Option<Instant>,
    trigger: String,
    live: bool,
    begun: Instant,
    first_chunk: Option<Instant>,
    last_chunk: Option<Instant>,
    chunks: u32,
    max_gap: Duration,
    heard: Option<Instant>,
}

impl Seg {
    fn covers(&self, w: &crate::stt::Word) -> bool {
        let mid = (w.start + w.end) / 2.0;
        mid >= self.start - EDGE && self.end.is_none_or(|e| mid < e + EDGE)
    }
}

struct Stream {
    sent: u64,
    processed: f32,
    in_turn: bool,
    eot_window: f32,
    last_end: f32,
    finals: Vec<crate::stt::Word>,
    partial: Vec<crate::stt::Word>,
    segs: Vec<Seg>,
}

impl Stream {
    fn clock(&self) -> f32 {
        self.sent as f32 / BYTES_PER_SEC
    }

    fn open(&self) -> bool {
        self.segs.iter().any(|s| s.end.is_none())
    }

    fn on_turn(&mut self, v: &serde_json::Value) {
        if let Some(end) = v.get("audio_window_end").and_then(|x| x.as_f64()) {
            self.processed = self.processed.max(end as f32);
        }
        let event = v.get("event").and_then(|x| x.as_str()).unwrap_or("");
        let words = turn_words(v);
        if std::env::var("FLUX_DEBUG_TMP").is_ok() && (event != "Update" || !words.is_empty()) {
            eprintln!("DBG {event} processed={:.2} clock={:.2} segs={:?} words={:?}", self.processed, self.clock(), self.segs.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(), words.iter().map(|w| (w.text.as_str(), w.start, w.end)).collect::<Vec<_>>());
        }
        let now = Instant::now();
        let touched: Vec<usize> = (0..self.segs.len()).filter(|&i| words.iter().any(|w| self.segs[i].covers(w))).collect();
        if !words.is_empty() {
            for &i in &touched {
                self.segs[i].heard.get_or_insert(now);
            }
        }
        match event {
            "StartOfTurn" => self.in_turn = true,
            "EagerEndOfTurn" => touched.iter().for_each(|&i| self.segs[i].eager = Some(now)),
            "TurnResumed" => touched.iter().for_each(|&i| self.segs[i].eager = None),
            "EndOfTurn" => {
                self.in_turn = false;
                self.eot_window = self.processed;
                let trigger = v.get("trigger").and_then(|x| x.as_str()).unwrap_or("model").to_string();
                for &i in &touched {
                    self.segs[i].eot = Some(now);
                    self.segs[i].trigger = trigger.clone();
                }
            }
            _ => {}
        }
        if event == "EndOfTurn" {
            self.finals.extend(words);
            self.partial.clear();
            let over = self.finals.len().saturating_sub(KEEP_WORDS);
            self.finals.drain(..over);
        } else {
            self.partial = words;
        }
    }

    fn complete(&mut self, i: usize, trigger: Option<&str>) {
        let mut seg = self.segs.remove(i);
        let Some((at, reply)) = seg.ended.take() else { return };
        let picked: Vec<&crate::stt::Word> = self.finals.iter().chain(self.partial.iter()).filter(|w| seg.covers(w)).collect();
        let text = picked.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let words = picked
            .iter()
            .map(|w| crate::stt::Word { text: w.text.clone(), start: (w.start - seg.start).max(0.0), end: (w.end - seg.start).max(0.0) })
            .collect();
        let rel = |t: Option<Instant>| {
            t.map(|t| if t >= at { t.duration_since(at).as_millis() as i64 } else { -(at.duration_since(t).as_millis() as i64) })
        };
        let ms = |v: Option<i64>| v.map(|m| format!("{m:+}ms")).unwrap_or_else(|| "never".into());
        let span = seg.first_chunk.zip(seg.last_chunk).map(|(a, b)| b.duration_since(a).as_secs_f32()).unwrap_or(0.0);
        let timing = format!(
            "first chunk {}ms after begin, {} chunks, max gap {}ms, {:.1}s audio over {:.1}s, backlog {:.1}s at end, first words {} vs end",
            seg.first_chunk.map(|t| t.duration_since(seg.begun).as_millis() as i64).unwrap_or(-1),
            seg.chunks,
            seg.max_gap.as_millis(),
            seg.end.unwrap_or(seg.start) - seg.start,
            span,
            seg.backlog,
            ms(rel(seg.heard)),
        );
        let _ = reply.send(Outcome {
            text,
            words,
            seconds: seg.end.unwrap_or(seg.start) - seg.start,
            eager: rel(seg.eager),
            eot: rel(seg.eot),
            trigger: trigger.map(str::to_string).unwrap_or(seg.trigger),
            timing,
        });
    }

    fn settle(&mut self) -> bool {
        let now = Instant::now();
        let mut force = false;
        let mut i = 0;
        while i < self.segs.len() {
            let seg = &self.segs[i];
            let (Some(end), Some((at, _))) = (seg.end, seg.ended.as_ref()) else {
                i += 1;
                continue;
            };
            let catch_up = Duration::from_secs_f32(seg.backlog / DECODE_RATE);
            let limit = if seg.live { (LIVE_WAIT + catch_up).min(LIVE_MAX) } else { (GIVE_UP + catch_up).min(GIVE_UP_MAX) };
            if at.elapsed() >= limit {
                self.complete(i, Some("gave-up"));
                continue;
            }
            if self.processed < end + CATCH_UP {
                i += 1;
                continue;
            }
            let caught = *self.segs[i].caught.get_or_insert(now);
            let newer = self.segs.iter().any(|s| s.id != self.segs[i].id && s.start >= end);
            if !self.in_turn && (self.eot_window >= end || self.processed >= end + TAIL) {
                self.complete(i, None);
                continue;
            }
            if newer {
                self.complete(i, Some("split"));
                continue;
            }
            if self.in_turn && !self.segs[i].forced && caught.elapsed() >= FORCE_AFTER {
                self.segs[i].forced = true;
                force = true;
            }
            i += 1;
        }
        force
    }

    fn wants_silence(&self) -> bool {
        let pending = self.segs.iter().any(|s| s.ended.is_some()) || self.clock() < self.last_end + GAP_FILL;
        !self.open() && pending && self.clock() - self.processed < MAX_AHEAD
    }
}

async fn run(device: &str, p: Params, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<(), Failure> {
    let mut url = format!(
        "wss://api.deepgram.com/v2/listen?model=flux-general-en&encoding=linear16&sample_rate=16000&eot_threshold={}",
        p.eot
    );
    if let Some(e) = p.eager {
        url.push_str(&format!("&eager_eot_threshold={e}"));
    }
    for t in &p.keyterms {
        url.push_str("&keyterm=");
        url.push_str(&urlencoding::encode(t));
    }
    let mut request = url.into_client_request().map_err(|e| Failure::Connect(e.to_string()))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {}", p.key).parse().map_err(|_| Failure::Connect("invalid Deepgram key".into()))?,
    );
    let began = Instant::now();
    let (ws, _) = tokio::time::timeout(Duration::from_secs(6), tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| Failure::Connect("Flux STT connect timed out".into()))?
        .map_err(|e| Failure::Connect(format!("Flux STT connect failed: {e}")))?;
    connected(device);
    eprintln!("[ambient-flux-stt] {device} connected in {}ms", began.elapsed().as_millis());
    let (mut tx, mut rx) = ws.split();
    let lost = |e: tokio_tungstenite::tungstenite::Error| Failure::Stream(format!("Flux STT send failed: {e}"));
    let mut st = Stream { sent: 0, processed: 0.0, in_turn: false, eot_window: 0.0, last_end: 0.0, finals: Vec::new(), partial: Vec::new(), segs: Vec::new() };
    let mut last_audio = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    let silence = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            c = cmd.recv() => match c {
                Some(Cmd::Begin(id, begun)) => {
                    let start = st.clock();
                    st.segs.push(Seg {
                        id,
                        start,
                        end: None,
                        ended: None,
                        backlog: 0.0,
                        caught: None,
                        forced: false,
                        eager: None,
                        eot: None,
                        trigger: "none".into(),
                        live: false,
                        begun,
                        first_chunk: None,
                        last_chunk: None,
                        chunks: 0,
                        max_gap: Duration::ZERO,
                        heard: None,
                    });
                }
                Some(Cmd::Audio(id, b)) => {
                    last_audio = Instant::now();
                    if let Some(s) = st.segs.iter_mut().find(|s| s.id == id) {
                        if let Some(prev) = s.last_chunk {
                            s.max_gap = s.max_gap.max(last_audio.duration_since(prev));
                        }
                        s.first_chunk.get_or_insert(last_audio);
                        s.last_chunk = Some(last_audio);
                        s.chunks += 1;
                    }
                    st.sent += b.len() as u64;
                    tx.send(Message::Binary(b.into())).await.map_err(lost)?;
                }
                Some(Cmd::End { seg, at, live, reply }) => {
                    let (clock, processed) = (st.clock(), st.processed);
                    if let Some(s) = st.segs.iter_mut().find(|s| s.id == seg) {
                        s.end = Some(clock);
                        s.backlog = (clock - processed).max(0.0);
                        s.ended = Some((at, reply));
                        s.live = live;
                    }
                    st.last_end = clock;
                }
                Some(Cmd::Abort(seg)) => st.segs.retain(|s| s.id != seg),
                Some(Cmd::Configure(m)) => tx.send(Message::Text(m.into())).await.map_err(lost)?,
                Some(Cmd::Close) | None => break,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    match v.get("type").and_then(|x| x.as_str()) {
                        Some("TurnInfo") => st.on_turn(&v),
                        Some("ConfigureSuccess") => eprintln!("[ambient-flux-stt] {device} reconfigured"),
                        Some("ConfigureFailure") => eprintln!("[ambient-flux-stt] {device} configure rejected: {t}"),
                        Some("Error") => return Err(Failure::Stream(format!("Flux STT error: {t}"))),
                        _ => {}
                    }
                    if st.settle() {
                        tx.send(Message::Text("{\"type\":\"ForceEndTurn\"}".into())).await.map_err(lost)?;
                    }
                }
                Some(Ok(Message::Close(frame))) => return Err(Failure::Stream(format!("Flux STT closed the connection: {frame:?}"))),
                None => return Err(Failure::Stream("Flux STT connection ended".into())),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(Failure::Stream(format!("Flux STT stream error: {e}"))),
            },
            _ = ping.tick() => tx.send(Message::Ping(Vec::new().into())).await.map_err(lost)?,
            _ = tick.tick() => {
                if st.segs.is_empty() && last_audio.elapsed() >= IDLE_CLOSE {
                    break;
                }
                if st.settle() {
                    tx.send(Message::Text("{\"type\":\"ForceEndTurn\"}".into())).await.map_err(lost)?;
                }
                // Flux decodes about 1.3x real time; padding while it lags only grows the backlog.
                if st.wants_silence() {
                    last_audio = Instant::now();
                    st.sent += silence.len() as u64;
                    tx.send(Message::Binary(silence.clone().into())).await.map_err(lost)?;
                }
            }
        }
    }
    let _ = tx.send(Message::Text("{\"type\":\"CloseStream\"}".into())).await;
    Ok(())
}
