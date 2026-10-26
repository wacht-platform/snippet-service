use std::collections::HashMap;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use super::{unauthorized, Shared};
use crate::tts_stream::PhoneFrame;

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

struct Open {
    meta: serde_json::Value,
    pcm: Vec<u8>,
}

async fn run(socket: WebSocket, d: Shared, device: String, token: Option<String>) {
    let (mut tx, mut rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<PhoneFrame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let msg = match frame {
                PhoneFrame::Text(text) => Message::Text(text.into()),
                PhoneFrame::Binary(bytes) => Message::Binary(bytes.into()),
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    let mut phone: Option<u64> = None;
    let mut open: HashMap<u32, Open> = HashMap::new();
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Binary(b) => {
                if b.len() < 4 {
                    continue;
                }
                let h = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if super::listener::audio(&device, h, &b[4..]) {
                    continue;
                }
                if let Some(seg) = open.get_mut(&h)
                    && seg.pcm.len() + b.len() - 4 <= MAX_SEGMENT_PCM
                {
                    seg.pcm.extend_from_slice(&b[4..]);
                }
            }
            Message::Text(t) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                let h = v.get("h").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                match v.get("t").and_then(|x| x.as_str()) {
                    Some("start") => {
                        if open.len() >= MAX_OPEN {
                            let _ = out_tx.send(PhoneFrame::Text(serde_json::json!({"t":"err","h":h,"code":429}).to_string()));
                            continue;
                        }
                        if let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) {
                            super::ambient_routes::announce(&d, &device, seq, false);
                        }
                        open.insert(h, Open { meta: v, pcm: Vec::new() });
                    }
                    Some("open") => {
                        let started = v.get("started_at").and_then(|x| x.as_str()).map(str::to_string);
                        let reply = match super::listener::open(&d, &device, h, started.as_deref()).await {
                            Ok(()) => serde_json::json!({"t":"opened","h":h}),
                            Err(code) => serde_json::json!({"t":"err","h":h,"code":code}),
                        };
                        let _ = out_tx.send(PhoneFrame::Text(reply.to_string()));
                    }
                    Some("close") => {
                        super::listener::close(&device, Some(h));
                    }
                    Some(kind @ ("media" | "play")) => {
                        let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
                        super::listener::flag(&device, kind, on);
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
                            let _ = out_tx.send(PhoneFrame::Text(serde_json::json!({"t":"err","h":h,"code":404}).to_string()));
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
                            let _ = out.send(PhoneFrame::Text(serde_json::json!({"t":"done","h":h,"id":id,"code":code,"status":status}).to_string()));
                        });
                    }
                    Some("caps") => {
                        if !device.is_empty() && v.get("tts_stream").and_then(|x| x.as_i64()).unwrap_or(0) > 0 {
                            if let Some(old) = phone.take() {
                                crate::tts_stream::unregister_phone(&device, old);
                            }
                            phone = Some(crate::tts_stream::register_phone(&device, out_tx.clone()));
                        }
                    }
                    Some("tts_pos") => {
                        let turn = v.get("turn").and_then(|x| x.as_i64()).unwrap_or(0);
                        let ms = v.get("ms").and_then(|x| x.as_i64()).unwrap_or(0);
                        let done = v.get("done").and_then(|x| x.as_bool()).unwrap_or(false);
                        if !device.is_empty() {
                            crate::tts_stream::report_pos(&device, turn, ms, done);
                        }
                    }
                    Some("ping") => {
                        let _ = out_tx.send(PhoneFrame::Text(serde_json::json!({"t":"pong"}).to_string()));
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    if let Some(id) = phone {
        crate::tts_stream::unregister_phone(&device, id);
    }
    if !device.is_empty() {
        super::listener::close(&device, None);
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
    let response = super::ambient_routes::ingest(State(d), Query(q), Bytes::from(super::listener::wav(&seg.pcm))).await;
    let code = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16).await.unwrap_or_default();
    let status = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str().map(str::to_string)))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).to_string());
    (code, status)
}
