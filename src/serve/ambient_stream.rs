use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use super::{unauthorized, Shared};

const MAX_SEGMENT_PCM: usize = 16_000 * 2 * 120;
const MAX_OPEN: usize = 4;

#[derive(Deserialize)]
pub(crate) struct StreamQuery {
    token: Option<String>,
    #[serde(default)]
    device: String,
}

pub(crate) async fn stream(ws: WebSocketUpgrade, State(d): State<Shared>, Query(q): Query<StreamQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let device = q.device.trim().to_string();
    let token = q.token.clone();
    ws.max_message_size(1 << 20)
        .on_upgrade(move |socket| run(socket, d, device, token))
}

const QUIET_FLOOR: f32 = 200.0;
const QUIET_BYTES: usize = 9_600;

struct Open {
    meta: serde_json::Value,
    pcm: Vec<u8>,
    peak: f32,
    quiet: usize,
    voiced: usize,
    checks: u8,
    spec: Option<(usize, tokio::task::JoinHandle<Result<super::transcribe::Spoken, String>>)>,
}

fn rms(bytes: &[u8]) -> f32 {
    let n = bytes.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = bytes
        .chunks_exact(2)
        .map(|b| {
            let v = i16::from_le_bytes([b[0], b[1]]) as f64;
            v * v
        })
        .sum();
    (sum / n as f64).sqrt() as f32
}

fn quiet(level: f32, peak: f32) -> bool {
    level < QUIET_FLOOR.max(peak * 0.08)
}

fn wav(pcm: &[u8]) -> Vec<u8> {
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

async fn run(socket: WebSocket, d: Shared, device: String, token: Option<String>) {
    let (mut tx, mut rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let mut open: HashMap<u32, Open> = HashMap::new();
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Binary(b) => {
                if b.len() < 4 {
                    continue;
                }
                let h = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if let Some(seg) = open.get_mut(&h) {
                    if seg.pcm.len() + b.len() - 4 <= MAX_SEGMENT_PCM {
                        let body = &b[4..];
                        seg.pcm.extend_from_slice(body);
                        if body.len() <= 2048 {
                            let level = rms(body);
                            seg.peak = seg.peak.max(level);
                            if quiet(level, seg.peak) {
                                seg.quiet += body.len();
                            } else {
                                seg.quiet = 0;
                                seg.voiced += body.len();
                                let due = (seg.checks == 0 && seg.voiced >= 12_800) || (seg.checks == 1 && seg.voiced >= 38_400);
                                if due && !device.is_empty() && crate::speech::is_audible(&device) {
                                    seg.checks += 1;
                                    let first = seg.checks == 1;
                                    let samples: Vec<f32> = seg.pcm.chunks_exact(2).take(16_000 * 3).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
                                    let (d, dev) = (d.clone(), device.clone());
                                    tokio::spawn(async move {
                                        let sim = tokio::task::spawn_blocking(move || crate::diarize::user_similarity(&d.store, &samples)).await;
                                        let go = match sim {
                                            Ok(Ok(Some(s))) => s >= if first { 0.28 } else { 0.22 },
                                            Ok(Ok(None)) => true,
                                            _ => false,
                                        };
                                        eprintln!("[ambient-speech] interrupt check {dev} go={go}");
                                        if go {
                                            crate::speech::interrupt(&dev);
                                        }
                                    });
                                }
                                if let Some((_, h)) = seg.spec.take() {
                                    h.abort();
                                }
                            }
                            if seg.quiet >= QUIET_BYTES && seg.spec.is_none() && seg.pcm.len() >= 16_000 {
                                let samples: Vec<f32> =
                                    seg.pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
                                let len = seg.pcm.len();
                                let d = d.clone();
                                let handle = tokio::spawn(async move {
                                    let permit = super::ambient_routes::gate().acquire_owned().await.ok();
                                    let r = super::transcribe::transcribe_samples(&d, samples).await;
                                    drop(permit);
                                    r
                                });
                                seg.spec = Some((len, handle));
                            }
                        }
                    }
                }
            }
            Message::Text(t) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                let h = v.get("h").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                match v.get("t").and_then(|x| x.as_str()) {
                    Some("start") => {
                        if open.len() >= MAX_OPEN {
                            let _ = out_tx.send(serde_json::json!({"t":"err","h":h,"code":429}).to_string());
                            continue;
                        }
                        if let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) {
                            super::ambient_routes::announce(&d, &device, seq, false);
                        }
                        open.insert(h, Open { meta: v, pcm: Vec::new(), peak: 0.0, quiet: 0, voiced: 0, checks: 0, spec: None });
                    }
                    Some("skip") => {
                        if let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) {
                            super::ambient_routes::announce(&d, &device, seq, true);
                        }
                    }
                    Some("abort") => {
                        open.remove(&h);
                    }
                    Some("end") => {
                        let Some(mut seg) = open.remove(&h) else {
                            let _ = out_tx.send(serde_json::json!({"t":"err","h":h,"code":404}).to_string());
                            continue;
                        };
                        if let Some(ms) = v.get("duration_ms") {
                            seg.meta["duration_ms"] = ms.clone();
                        }
                        for key in ["media", "during_playback"] {
                            if let Some(m) = v.get(key) {
                                seg.meta[key] = m.clone();
                            }
                        }
                        if let Some(n) = v.get("bytes").and_then(|x| x.as_u64()) {
                            seg.pcm.truncate((n as usize) & !1);
                        }
                        if let Some((len, handle)) = seg.spec.take() {
                            let tail_ok = len <= seg.pcm.len()
                                && seg.pcm[len..]
                                    .chunks(1024)
                                    .all(|c| quiet(rms(c), seg.peak));
                            match (tail_ok, v.get("id").and_then(|x| x.as_str())) {
                                (true, Some(id)) => {
                                    super::ambient_routes::precomputed().lock().unwrap().insert(id.to_string(), handle);
                                }
                                _ => handle.abort(),
                            }
                        }
                        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                            super::ambient_routes::mark(id, "end");
                        }
                        let d = d.clone();
                        let out = out_tx.clone();
                        let device = device.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            let (code, status) = finish(d, device, token, seg).await;
                            let id = v.get("id").cloned().unwrap_or_default();
                            let _ = out.send(serde_json::json!({"t":"done","h":h,"id":id,"code":code,"status":status}).to_string());
                        });
                    }
                    Some("stat") => {
                        eprintln!("[ambient-stat] {device} {v}");
                    }
                    Some("ping") => {
                        let _ = out_tx.send(serde_json::json!({"t":"pong"}).to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    drop(out_tx);
    let _ = writer.await;
}

async fn finish(d: Shared, device: String, token: Option<String>, seg: Open) -> (u16, String) {
    if seg.pcm.is_empty() {
        return (400, "empty audio".into());
    }
    let mut meta = seg.meta;
    meta["device"] = serde_json::Value::String(device);
    meta["token"] = token.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null);
    if meta.get("id").is_none() {
        return (400, "id required".into());
    }
    let q: super::ambient_routes::IngestQuery = match serde_json::from_value(meta) {
        Ok(q) => q,
        Err(e) => return (400, e.to_string()),
    };
    let response = super::ambient_routes::ingest(State(d), Query(q), Bytes::from(wav(&seg.pcm))).await;
    let code = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16).await.unwrap_or_default();
    let status = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str().map(str::to_string)))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
    (code, status)
}
