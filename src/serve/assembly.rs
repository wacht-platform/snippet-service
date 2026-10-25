use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SEND_MIN: usize = 3200;
const MIN_SEND: usize = 1760;
const MAX_SEND: usize = 24_000;
const FINISH_WAIT: Duration = Duration::from_millis(1800);
const EMPTY_WAIT: Duration = Duration::from_millis(1100);
const IDLE_CLOSE: Duration = Duration::from_secs(300);
const KEEP_FINALS: usize = 64;

pub(super) struct Transcript {
    pub text: String,
}

enum Cmd {
    Audio(Vec<u8>),
    Force,
}

#[derive(Clone)]
struct Turn {
    text: String,
    words: Vec<crate::stt::Word>,
}

#[derive(Default)]
struct State {
    finals: VecDeque<(u64, Turn)>,
    next: u64,
    partial: Option<Turn>,
    closed: Option<String>,
}

pub(super) struct Session {
    cmd: mpsc::UnboundedSender<Cmd>,
    state: Mutex<State>,
    notify: Notify,
    flushed: AtomicU64,
}

fn hub() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static HUB: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    HUB.get_or_init(Default::default)
}

pub(super) fn begin(device: &str, key: String, terms: Vec<String>) -> Ticket {
    let session = {
        let mut map = hub().lock().unwrap();
        match map.get(device).filter(|s| s.state.lock().unwrap().closed.is_none()) {
            Some(existing) => existing.clone(),
            None => {
                let (cmd, rx) = mpsc::unbounded_channel();
                let session = Arc::new(Session { cmd, state: Mutex::new(State::default()), notify: Notify::new(), flushed: AtomicU64::new(0) });
                map.insert(device.to_string(), session.clone());
                let (task, name) = (session.clone(), device.to_string());
                tokio::spawn(async move {
                    let reason = run(&task, key, terms, rx).await.err().unwrap_or_else(|| "closed".into());
                    task.state.lock().unwrap().closed = Some(reason);
                    task.notify.notify_waiters();
                    let mut map = hub().lock().unwrap();
                    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &task)) {
                        map.remove(&name);
                    }
                });
                session
            }
        }
    };
    let start = session.state.lock().unwrap().next;
    let init_byte = session.flushed.load(Ordering::SeqCst);
    Ticket {
        inner: Some(Inner {
            session,
            start,
            init_byte,
            start_byte: None,
            end_byte: init_byte,
            voiced_byte: init_byte,
            sent: 0,
            buf: Vec::new(),
        }),
    }
}

struct Inner {
    session: Arc<Session>,
    start: u64,
    init_byte: u64,
    start_byte: Option<u64>,
    end_byte: u64,
    voiced_byte: u64,
    sent: u64,
    buf: Vec<u8>,
}

pub(super) struct Ticket {
    inner: Option<Inner>,
}

impl Inner {
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let mut bytes = std::mem::take(&mut self.buf);
            while bytes.len() > MAX_SEND {
                let rest = bytes.split_off(MAX_SEND);
                self.send(std::mem::replace(&mut bytes, rest));
            }
            if bytes.len() < MIN_SEND {
                bytes.resize(MIN_SEND, 0);
            }
            self.send(bytes);
        }
    }

    fn send(&mut self, bytes: Vec<u8>) {
        let n = bytes.len() as u64;
        let prev = self.session.flushed.fetch_add(n, Ordering::SeqCst);
        if self.start_byte.is_none() {
            self.start_byte = Some(prev);
        }
        self.end_byte = prev + n;
        self.sent += n;
        let _ = self.session.cmd.send(Cmd::Audio(bytes));
    }

    fn window(&self) -> (f32, f32, f32) {
        let s = self.start_byte.unwrap_or(self.init_byte) as f32 / 32_000.0;
        let e = self.end_byte as f32 / 32_000.0;
        let v = (self.voiced_byte.max(self.start_byte.unwrap_or(self.init_byte)) as f32 / 32_000.0).min(e);
        (s, e, v)
    }

    fn owns(&self, index: Option<u64>, turn: &Turn, start_sec: f32, end_sec: f32) -> bool {
        if let Some(i) = index
            && i < self.start
        {
            return false;
        }
        match (turn.words.first(), turn.words.last()) {
            (Some(first), Some(last)) => last.end > start_sec - 0.15 && first.start < end_sec + 0.35,
            _ => index.is_none_or(|i| i >= self.start),
        }
    }

    async fn finish(mut self) -> Result<Transcript, String> {
        self.flush();
        let (start_sec, audio_end_sec, voiced_end_sec) = self.window();
        // Feed 200ms of trailing silence with ForceEndpoint so AssemblyAI flushes its lookahead buffer immediately.
        self.send(vec![0u8; 6400]);
        let end_sec = audio_end_sec + 0.20;
        let finals_at_end = self.session.state.lock().unwrap().finals.len();
        let _ = self.session.cmd.send(Cmd::Force);
        let deadline = tokio::time::Instant::now() + FINISH_WAIT;
        let empty_deadline = tokio::time::Instant::now() + EMPTY_WAIT;
        loop {
            let notified = self.session.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (have, covered, open, closed, any_partial) = {
                let s = self.session.state.lock().unwrap();
                let owned: Vec<&Turn> = s
                    .finals
                    .iter()
                    .filter(|(i, t)| self.owns(Some(*i), t, start_sec, end_sec))
                    .map(|(_, t)| t)
                    .collect();
                let have = !owned.is_empty();
                let last_word_end = owned
                    .iter()
                    .rev()
                    .find_map(|t| t.words.last().map(|w| w.end))
                    .unwrap_or(0.0);
                let new_final = s.finals.len() > finals_at_end;
                let covered = have && (new_final || last_word_end + 0.65 >= voiced_end_sec);
                let open = s
                    .partial
                    .as_ref()
                    .is_some_and(|p| self.owns(None, p, start_sec, end_sec));
                (have, covered, open, s.closed.clone(), open)
            };
            if covered && !open {
                break;
            }
            if let Some(reason) = closed {
                if !have && !any_partial {
                    return Err(reason);
                }
                break;
            }
            let limit = if have || any_partial { deadline } else { empty_deadline };
            if tokio::time::timeout_at(limit, notified).await.is_err() {
                break;
            }
        }
        let s = self.session.state.lock().unwrap();
        let mut text = String::new();
        let mut parts: Vec<&Turn> = s
            .finals
            .iter()
            .filter(|(i, t)| self.owns(Some(*i), t, start_sec, end_sec))
            .map(|(_, t)| t)
            .collect();
        if let Some(p) = s
            .partial
            .as_ref()
            .filter(|p| self.owns(None, p, start_sec, end_sec))
        {
            parts.push(p);
        }
        for turn in parts {
            if turn.text.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&turn.text);
        }
        Ok(Transcript { text })
    }
}

impl Ticket {
    pub fn push(&mut self, pcm: &[u8], voiced: bool) {
        if let Some(inner) = self.inner.as_mut() {
            inner.buf.extend_from_slice(pcm);
            if inner.buf.len() >= SEND_MIN {
                inner.flush();
            }
            if voiced {
                inner.voiced_byte = inner.end_byte + inner.buf.len() as u64;
            }
        }
    }

    pub fn force(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.flush();
            let _ = inner.session.cmd.send(Cmd::Force);
        }
    }

    pub async fn finish(mut self) -> Result<Transcript, String> {
        match self.inner.take() {
            Some(inner) => inner.finish().await,
            None => Err("already finished".into()),
        }
    }
}

fn parse_turn(v: &serde_json::Value) -> Option<(bool, Turn)> {
    v.get("turn_order").and_then(|x| x.as_i64())?;
    let text = v.get("transcript").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let ended = v.get("end_of_turn").and_then(|x| x.as_bool()).unwrap_or(false);
    let words = v
        .get("words")
        .and_then(|x| x.as_array())
        .map(|ws| {
            ws.iter()
                .filter_map(|w| {
                    Some(crate::stt::Word {
                        text: w.get("text")?.as_str()?.to_string(),
                        start: w.get("start")?.as_f64()? as f32 / 1000.0,
                        end: w.get("end")?.as_f64()? as f32 / 1000.0,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some((ended, Turn { text, words }))
}

async fn run(session: &Arc<Session>, key: String, terms: Vec<String>, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<(), String> {
    let keyterms = serde_json::to_string(&terms).map_err(|e| e.to_string())?;
    let url = format!(
        "wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&encoding=pcm_s16le&speech_model=universal-3-6-pro&mode=balanced&language_codes={}&keyterms_prompt={}",
        urlencoding::encode("[\"en\"]"),
        urlencoding::encode(&keyterms)
    );
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert("Authorization", key.trim().parse().map_err(|_| "invalid AssemblyAI key".to_string())?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(6), tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "AssemblyAI connect timed out".to_string())?
        .map_err(|e| format!("AssemblyAI connect failed: {e}"))?;
    let (mut tx, mut rx) = ws.split();
    let mut last_audio = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            c = cmd.recv() => match c {
                Some(Cmd::Audio(b)) => {
                    last_audio = Instant::now();
                    tx.send(Message::Binary(b.into())).await.map_err(|_| "AssemblyAI connection dropped".to_string())?;
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
                        Some("Turn") => {
                            if let Some((ended, turn)) = parse_turn(&v) {
                                let mut s = session.state.lock().unwrap();
                                if ended {
                                    let idx = s.next;
                                    s.next += 1;
                                    s.finals.push_back((idx, turn));
                                    while s.finals.len() > KEEP_FINALS {
                                        s.finals.pop_front();
                                    }
                                    s.partial = None;
                                } else {
                                    s.partial = Some(turn);
                                }
                                drop(s);
                                session.notify.notify_waiters();
                            }
                        }
                        Some("Termination") => return Err("AssemblyAI ended the session".into()),
                        Some(other) if other.eq_ignore_ascii_case("error") => return Err(format!("AssemblyAI error: {t}")),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Err("AssemblyAI closed the connection".into()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("AssemblyAI stream error: {e}")),
            },
            _ = tick.tick() => {
                let _ = tx.send(Message::Ping(Default::default())).await;
                if last_audio.elapsed() >= IDLE_CLOSE {
                    let _ = tx.send(Message::Text("{\"type\":\"Terminate\"}".into())).await;
                    return Ok(());
                }
            }
        }
    }
    let _ = tx.send(Message::Text("{\"type\":\"Terminate\"}".into())).await;
    Ok(())
}

pub(super) async fn transcribe_file(key: &str, path: &std::path::Path) -> Result<String, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().map_err(|e| e.to_string())?;
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("could not read audio: {e}"))?;
    let up: serde_json::Value = client
        .post("https://api.assemblyai.com/v2/upload")
        .header("authorization", key)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("AssemblyAI upload failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("AssemblyAI upload failed: {e}"))?;
    let url = up.get("upload_url").and_then(|u| u.as_str()).ok_or_else(|| format!("AssemblyAI upload rejected: {up}"))?;
    let made: serde_json::Value = client
        .post("https://api.assemblyai.com/v2/transcript")
        .header("authorization", key)
        .json(&serde_json::json!({ "audio_url": url, "speech_models": ["universal-3-pro", "universal-2"], "language_detection": true }))
        .send()
        .await
        .map_err(|e| format!("AssemblyAI request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("AssemblyAI request failed: {e}"))?;
    let id = made.get("id").and_then(|u| u.as_str()).ok_or_else(|| format!("AssemblyAI rejected the audio: {made}"))?.to_string();
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        tokio::time::sleep(Duration::from_millis(900)).await;
        let got: serde_json::Value = client
            .get(format!("https://api.assemblyai.com/v2/transcript/{id}"))
            .header("authorization", key)
            .send()
            .await
            .map_err(|e| format!("AssemblyAI poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("AssemblyAI poll failed: {e}"))?;
        match got.get("status").and_then(|x| x.as_str()) {
            Some("completed") => return Ok(got.get("text").and_then(|x| x.as_str()).unwrap_or("").trim().to_string()),
            Some("error") => return Err(format!("AssemblyAI could not transcribe: {}", got.get("error").and_then(|x| x.as_str()).unwrap_or("unknown error"))),
            _ => {}
        }
        if Instant::now() > deadline {
            return Err("AssemblyAI took too long to transcribe the audio".into());
        }
    }
}

impl Ticket {
    pub(super) fn heard(&self) -> Option<Heard> {
        self.inner.as_ref().map(|i| Heard { session: i.session.clone(), start: i.start })
    }
}

pub(super) struct Heard {
    session: Arc<Session>,
    start: u64,
}

impl Heard {
    pub(super) fn text(&self) -> String {
        let st = self.session.state.lock().unwrap();
        let mut out: Vec<String> = st.finals.iter().filter(|(i, _)| *i >= self.start).map(|(_, t)| t.text.clone()).collect();
        if let Some(p) = &st.partial {
            out.push(p.text.clone());
        }
        out.join(" ").trim().to_string()
    }
}
