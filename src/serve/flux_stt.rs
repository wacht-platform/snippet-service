use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const CHUNK: usize = 2560;
const SETTLE: Duration = Duration::from_millis(700);
const FORCE_AFTER: Duration = Duration::from_millis(1500);
const GIVE_UP: Duration = Duration::from_millis(3000);
const ASSEMBLY_WAIT: Duration = Duration::from_secs(15);
const IDLE_CLOSE: Duration = Duration::from_secs(300);
const BACKOFF_BASE: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(600);

struct Outcome {
    text: String,
    eager: Option<i64>,
    eot: Option<i64>,
    trigger: String,
}

enum Cmd {
    Begin,
    Audio(Vec<u8>),
    End { at: Instant, reply: oneshot::Sender<Outcome> },
    Abort,
}

struct Session {
    cmd: mpsc::UnboundedSender<Cmd>,
    closed: Mutex<bool>,
}

#[derive(Default)]
struct Health {
    fails: u32,
    until: Option<Instant>,
    logged: bool,
}

fn hub() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static HUB: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    HUB.get_or_init(Default::default)
}

fn health() -> &'static Mutex<HashMap<String, Health>> {
    static H: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

fn failed(device: &str, reason: &str) {
    let mut map = health().lock().unwrap();
    let h = map.entry(device.to_string()).or_default();
    h.fails += 1;
    let wait = BACKOFF_BASE.saturating_mul(1 << (h.fails - 1).min(6)).min(BACKOFF_MAX);
    h.until = Some(Instant::now() + wait);
    if !h.logged {
        h.logged = true;
        eprintln!("[ambient-flux-stt] {device} shadow disabled for {}s: {reason}", wait.as_secs());
    }
}

fn healthy(device: &str) {
    health().lock().unwrap().remove(device);
}

pub(super) fn begin(device: &str, key: String, terms: Vec<String>) -> Option<Ticket> {
    if health().lock().unwrap().get(device).and_then(|h| h.until).is_some_and(|u| Instant::now() < u) {
        return None;
    }
    let session = {
        let mut map = hub().lock().unwrap();
        match map.get(device).filter(|s| !*s.closed.lock().unwrap()) {
            Some(existing) => existing.clone(),
            None => {
                let (cmd, rx) = mpsc::unbounded_channel();
                let session = Arc::new(Session { cmd, closed: Mutex::new(false) });
                map.insert(device.to_string(), session.clone());
                let (task, name) = (session.clone(), device.to_string());
                tokio::spawn(async move {
                    let result = run(&name, key, terms, rx).await;
                    *task.closed.lock().unwrap() = true;
                    if let Err(e) = result {
                        failed(&name, &e);
                    }
                    let mut map = hub().lock().unwrap();
                    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &task)) {
                        map.remove(&name);
                    }
                });
                session
            }
        }
    };
    let _ = session.cmd.send(Cmd::Begin);
    Some(Ticket { session: Some(session), buf: Vec::new() })
}

pub(super) struct Ticket {
    session: Option<Arc<Session>>,
    buf: Vec<u8>,
}

impl Ticket {
    pub fn push(&mut self, pcm: &[u8]) {
        let Some(session) = self.session.as_ref() else { return };
        self.buf.extend_from_slice(pcm);
        while self.buf.len() >= CHUNK {
            let rest = self.buf.split_off(CHUNK);
            let _ = session.cmd.send(Cmd::Audio(std::mem::replace(&mut self.buf, rest)));
        }
    }

    pub fn end(mut self, id: String, at: Instant, assembly: Option<oneshot::Receiver<Option<String>>>) {
        let Some(session) = self.session.take() else { return };
        if !self.buf.is_empty() {
            let _ = session.cmd.send(Cmd::Audio(std::mem::take(&mut self.buf)));
        }
        let (reply, rx) = oneshot::channel();
        if session.cmd.send(Cmd::End { at, reply }).is_err() {
            return;
        }
        let short = id[..id.len().min(8)].to_string();
        tokio::spawn(async move {
            let Ok(flux) = rx.await else { return };
            let ms = |v: Option<i64>| v.map(|m| format!("{m:+}")).unwrap_or_else(|| "-".into());
            eprintln!(
                "[ambient-flux-stt] {short} eager_eot {} eot {} ({}) text={:?}",
                ms(flux.eager),
                ms(flux.eot),
                flux.trigger,
                flux.text
            );
            let reference = match assembly {
                Some(rx) => tokio::time::timeout(ASSEMBLY_WAIT, rx).await.ok().and_then(|r| r.ok()).flatten(),
                None => None,
            };
            match reference {
                Some(asm) => {
                    let (dist, len) = word_distance(&asm, &flux.text);
                    let wer = dist as f32 / len.max(1) as f32;
                    eprintln!("[ambient-flux-stt] {short} vs assembly: wer={wer:.2} ({dist}/{len}) assembly={asm:?} flux={:?}", flux.text);
                }
                None => eprintln!("[ambient-flux-stt] {short} vs assembly: no assembly transcript flux={:?}", flux.text),
            }
        });
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session.cmd.send(Cmd::Abort);
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

struct Seg {
    turns: Vec<String>,
    partial: String,
    eager: Option<Instant>,
    eot: Option<Instant>,
    trigger: String,
    ended: Option<(Instant, oneshot::Sender<Outcome>)>,
    eot_after_end: bool,
    forced: bool,
}

impl Seg {
    fn new() -> Self {
        Seg { turns: Vec::new(), partial: String::new(), eager: None, eot: None, trigger: "none".into(), ended: None, eot_after_end: false, forced: false }
    }

    fn finish(mut self, trigger: Option<&str>) -> bool {
        let Some((at, reply)) = self.ended.take() else { return false };
        let mut text = self.turns.join(" ");
        if !self.partial.trim().is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(self.partial.trim());
        }
        let rel = |t: Option<Instant>| {
            t.map(|t| if t >= at { t.duration_since(at).as_millis() as i64 } else { -(at.duration_since(t).as_millis() as i64) })
        };
        let _ = reply.send(Outcome {
            text,
            eager: rel(self.eager),
            eot: rel(self.eot),
            trigger: trigger.map(str::to_string).unwrap_or(self.trigger),
        });
        true
    }
}

async fn run(device: &str, key: String, terms: Vec<String>, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<(), String> {
    let mut url = "wss://api.deepgram.com/v2/listen?model=flux-general-en&encoding=linear16&sample_rate=16000&eot_threshold=0.7&eager_eot_threshold=0.5".to_string();
    for t in &terms {
        url.push_str("&keyterm=");
        url.push_str(&urlencoding::encode(t));
    }
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request
        .headers_mut()
        .insert("Authorization", format!("Token {}", key.trim()).parse().map_err(|_| "invalid Deepgram key".to_string())?);
    let began = Instant::now();
    let (ws, _) = tokio::time::timeout(Duration::from_secs(6), tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "Flux STT connect timed out".to_string())?
        .map_err(|e| format!("Flux STT connect failed: {e}"))?;
    eprintln!("[ambient-flux-stt] {device} connected in {}ms", began.elapsed().as_millis());
    let (mut tx, mut rx) = ws.split();
    let mut seg: Option<Seg> = None;
    let mut in_turn = false;
    let mut last_audio = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let silence = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            c = cmd.recv() => match c {
                Some(Cmd::Begin) => {
                    if let Some(prev) = seg.take() {
                        prev.finish(Some("cut"));
                    }
                    seg = Some(Seg::new());
                }
                Some(Cmd::Audio(b)) => {
                    last_audio = Instant::now();
                    tx.send(Message::Binary(b.into())).await.map_err(|e| format!("Flux STT send failed: {e}"))?;
                }
                Some(Cmd::End { at, reply }) => {
                    if let Some(s) = seg.as_mut() {
                        s.ended = Some((at, reply));
                    }
                }
                Some(Cmd::Abort) => {
                    if seg.as_ref().is_some_and(|s| s.ended.is_none()) {
                        seg = None;
                    }
                }
                None => break,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    match v.get("type").and_then(|x| x.as_str()) {
                        Some("TurnInfo") => {
                            let text = v.get("transcript").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                            let now = Instant::now();
                            let event = v.get("event").and_then(|x| x.as_str()).unwrap_or("");
                            match event {
                                "StartOfTurn" => in_turn = true,
                                "EndOfTurn" => in_turn = false,
                                _ => {}
                            }
                            if let Some(s) = seg.as_mut() {
                                match event {
                                    "Update" | "StartOfTurn" => s.partial = text,
                                    "EagerEndOfTurn" => {
                                        s.eager = Some(now);
                                        s.partial = text;
                                    }
                                    "TurnResumed" => {
                                        s.eager = None;
                                        s.partial = text;
                                    }
                                    "EndOfTurn" => {
                                        if !text.is_empty() {
                                            s.turns.push(text);
                                        }
                                        s.partial.clear();
                                        s.eot = Some(now);
                                        s.trigger = v.get("trigger").and_then(|x| x.as_str()).unwrap_or("model").to_string();
                                        s.eot_after_end = s.ended.is_some();
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some("Error") => return Err(format!("Flux STT error: {t}")),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    return if seg.is_some() { Err("Flux STT closed the connection".into()) } else { Ok(()) };
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("Flux STT stream error: {e}")),
            },
            _ = tick.tick() => {
                let Some(since) = seg.as_ref().and_then(|s| s.ended.as_ref()).map(|(at, _)| at.elapsed()) else {
                    if seg.is_none() && last_audio.elapsed() >= IDLE_CLOSE {
                        let _ = tx.send(Message::Text("{\"type\":\"CloseStream\"}".into())).await;
                        return Ok(());
                    }
                    continue;
                };
                let s = seg.as_mut().unwrap();
                if !in_turn && (s.eot_after_end || since >= SETTLE) || since >= GIVE_UP {
                    if seg.take().is_some_and(|s| s.finish(if since >= GIVE_UP { Some("gave-up") } else { None })) {
                        healthy(device);
                    }
                    continue;
                }
                // The phone uploads only speech, so Flux needs trailing silence to see the turn end.
                last_audio = Instant::now();
                tx.send(Message::Binary(silence.clone().into())).await.map_err(|e| format!("Flux STT send failed: {e}"))?;
                if in_turn && since >= FORCE_AFTER && !s.forced {
                    s.forced = true;
                    tx.send(Message::Text("{\"type\":\"ForceEndTurn\"}".into())).await.map_err(|e| format!("Flux STT send failed: {e}"))?;
                }
            }
        }
    }
    let _ = tx.send(Message::Text("{\"type\":\"CloseStream\"}".into())).await;
    Ok(())
}
