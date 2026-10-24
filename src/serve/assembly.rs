use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const SEND_MIN: usize = 3200;
const FINISH_WAIT: Duration = Duration::from_millis(1800);
const EMPTY_WAIT: Duration = Duration::from_millis(1100);

pub(super) struct Transcript {
    pub text: String,
    pub words: Vec<crate::stt::Word>,
}

enum Cmd {
    Audio(Vec<u8>),
    Force,
    Finish,
    Abort,
}

pub(super) struct Handle {
    cmd: mpsc::UnboundedSender<Cmd>,
    result: Option<oneshot::Receiver<Result<Transcript, String>>>,
    buf: Vec<u8>,
}

impl Handle {
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.cmd.send(Cmd::Audio(std::mem::take(&mut self.buf)));
        }
    }

    pub fn push(&mut self, pcm: &[u8]) {
        self.buf.extend_from_slice(pcm);
        if self.buf.len() >= SEND_MIN {
            self.flush();
        }
    }

    pub fn force(&mut self) {
        self.flush();
        let _ = self.cmd.send(Cmd::Force);
    }

    pub async fn finish(mut self) -> Result<Transcript, String> {
        self.flush();
        let _ = self.cmd.send(Cmd::Finish);
        let rx = self.result.take().ok_or("already finished")?;
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("session ended".into()),
            Err(_) => Err("timed out".into()),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.result.is_some() {
            let _ = self.cmd.send(Cmd::Abort);
        }
    }
}

pub(super) fn start(key: String, terms: Vec<String>) -> Handle {
    let (cmd, rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = done_tx.send(run(key, terms, rx).await);
    });
    Handle { cmd, result: Some(done_rx), buf: Vec::new() }
}

struct Turn {
    text: String,
    words: Vec<crate::stt::Word>,
    ended: bool,
}

fn parse_turn(v: &serde_json::Value) -> Option<(i64, Turn)> {
    let order = v.get("turn_order").and_then(|x| x.as_i64())?;
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
    Some((order, Turn { text, words, ended }))
}

async fn run(key: String, terms: Vec<String>, mut cmd: mpsc::UnboundedReceiver<Cmd>) -> Result<Transcript, String> {
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
    let mut turns: BTreeMap<i64, Turn> = BTreeMap::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut open = true;
    loop {
        tokio::select! {
            c = cmd.recv(), if open => match c {
                Some(Cmd::Audio(b)) => {
                    if tx.send(Message::Binary(b.into())).await.is_err() {
                        return Err("AssemblyAI connection dropped".into());
                    }
                }
                Some(Cmd::Force) => {
                    let _ = tx.send(Message::Text("{\"type\":\"ForceEndpoint\"}".into())).await;
                }
                Some(Cmd::Abort) => {
                    let _ = tx.send(Message::Text("{\"type\":\"Terminate\"}".into())).await;
                    return Err("aborted".into());
                }
                Some(Cmd::Finish) | None => {
                    open = false;
                    let _ = tx.send(Message::Text("{\"type\":\"ForceEndpoint\"}".into())).await;
                    let wait = if turns.is_empty() { EMPTY_WAIT } else { FINISH_WAIT };
                    deadline = Some(tokio::time::Instant::now() + wait);
                }
            },
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    match v.get("type").and_then(|x| x.as_str()) {
                        Some("Turn") => {
                            if let Some((order, turn)) = parse_turn(&v) {
                                let keep = turns.get(&order).is_some_and(|old| old.ended && !turn.ended);
                                if !keep {
                                    turns.insert(order, turn);
                                }
                                if !open && !turns.is_empty() && turns.values().all(|t| t.ended) {
                                    break;
                                }
                            }
                        }
                        Some("Termination") => break,
                        Some(other) if other.eq_ignore_ascii_case("error") => {
                            return Err(format!("AssemblyAI error: {t}"));
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("AssemblyAI stream error: {e}")),
            },
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => break,
        }
    }
    let _ = tx.send(Message::Text("{\"type\":\"Terminate\"}".into())).await;
    let mut text = String::new();
    let mut words: Vec<crate::stt::Word> = Vec::new();
    for turn in turns.values() {
        if turn.text.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&turn.text);
        words.extend(turn.words.iter().cloned());
    }
    Ok(Transcript { text, words })
}
