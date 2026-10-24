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
    pub words: Vec<crate::stt::Word>,
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
    let offset = session.flushed.load(Ordering::SeqCst) as f32 / 32_000.0;
    Ticket { inner: Some(Inner { session, start, offset, sent: 0, buf: Vec::new() }) }
}

struct Inner {
    session: Arc<Session>,
    start: u64,
    offset: f32,
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
        self.sent += bytes.len() as u64;
        self.session.flushed.fetch_add(bytes.len() as u64, Ordering::SeqCst);
        let _ = self.session.cmd.send(Cmd::Audio(bytes));
    }

    fn owns(&self, index: Option<u64>, turn: &Turn, end: f32) -> bool {
        match (turn.words.first(), turn.words.last()) {
            (Some(first), Some(last)) => {
                let mid = (first.start + last.end) / 2.0;
                mid >= self.offset && mid < end
            }
            _ => index.is_some_and(|i| i >= self.start),
        }
    }

    async fn finish(mut self) -> Result<Transcript, String> {
        self.flush();
        let end = self.offset + self.sent as f32 / 32_000.0;
        let _ = self.session.cmd.send(Cmd::Force);
        let deadline = tokio::time::Instant::now() + FINISH_WAIT;
        let empty_deadline = tokio::time::Instant::now() + EMPTY_WAIT;
        loop {
            let notified = self.session.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (have, open, closed, any_partial) = {
                let s = self.session.state.lock().unwrap();
                (s.finals.iter().any(|(i, t)| self.owns(Some(*i), t, end)), s.partial.as_ref().is_some_and(|p| self.owns(None, p, end)), s.closed.clone(), s.partial.as_ref().is_some_and(|p| self.owns(None, p, end)))
            };
            if have && !open {
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
        let mut words: Vec<crate::stt::Word> = Vec::new();
        let mut parts: Vec<&Turn> = s.finals.iter().filter(|(i, t)| self.owns(Some(*i), t, end)).map(|(_, t)| t).collect();
        if let Some(p) = s.partial.as_ref().filter(|p| self.owns(None, p, end)) {
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
            words.extend(turn.words.iter().map(|w| crate::stt::Word {
                text: w.text.clone(),
                start: (w.start - self.offset).max(0.0),
                end: (w.end - self.offset).max(0.0),
            }));
        }
        Ok(Transcript { text, words })
    }
}

impl Ticket {
    pub fn push(&mut self, pcm: &[u8]) {
        if let Some(inner) = self.inner.as_mut() {
            inner.buf.extend_from_slice(pcm);
            if inner.buf.len() >= SEND_MIN {
                inner.flush();
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

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            tokio::spawn(async move {
                let _ = inner.finish().await;
            });
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
