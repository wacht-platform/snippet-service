use super::*;

#[derive(Deserialize)]
pub(crate) struct ShellsQuery {
    pub(crate) token: Option<String>,
}


// A shell belongs to the machine: switching or closing a session must not kill
// it. `/attach` cannot serve this because it calls `ensure_live`, so a socket
// with no live session gets a 404. This route carries the same `wire: term`
// frames over a socket that requires no session.
//
// Lifecycle is still explicit: the client creates and closes shells through the
// same `open`/`new`/`close` ops, so a dropped socket never destroys a pty.
pub(crate) async fn shells_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(q): Query<ShellsQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let terms = d.shells.clone();
    ws.on_upgrade(move |socket| handle_shells_ws(socket, terms))
}

async fn handle_shells_ws(socket: WebSocket, terms: Arc<crate::term::SessionTerms>) {
    use base64::Engine;
    let (mut sender, mut receiver) = socket.split();
    let term_client = terms.subscribe();
    let push_terms = terms.clone();
    let mut term_seq: u64 = 0;

    // Reconnect support: report what already exists, so a client that
    // reattaches rebuilds its strip instead of showing nothing while the ptys
    // keep running.
    let existing: Vec<serde_json::Value> = terms
        .list()
        .into_iter()
        .map(|(id, alive)| serde_json::json!({ "id": id, "alive": alive }))
        .collect();
    let hello = serde_json::json!({ "wire": "term", "op": "list", "shells": existing });
    if let Ok(json) = serde_json::to_string(&hello) {
        if sender.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    let push = tokio::spawn(async move {
        loop {
            // Drain the PTY so idle shells still fire BEL / OSC.
            let _ = push_terms.take_snapshots();
            for (id, chunk, cols, rows, alive) in push_terms.poll_client(&term_client) {
                if chunk.is_empty() && alive {
                    continue;
                }
                term_seq = term_seq.wrapping_add(1);
                let frame = serde_json::json!({
                    "wire": "term",
                    "op": "out",
                    "id": id,
                    "seq": term_seq,
                    "data": base64::engine::general_purpose::STANDARD.encode(&chunk),
                    "cols": cols,
                    "rows": rows,
                    "alive": alive,
                });
                if let Ok(json) = serde_json::to_string(&frame) {
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(t.as_str()) {
                    if val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        apply_term_client(&terms, &val);
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

// WS /attach?session= — stream this session's HarnessState + receive LoopInput.
pub(crate) async fn attach_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(q): Query<AttachQuery>,
) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    match d.ensure_live(&q.session).await {
        Some((_, state_path, stream)) => {
            let terms = {
                let sessions = d.sessions.lock().await;
                sessions.get(&q.session).map(|s| s.terms.clone())
            };
            let daemon = d.clone();
            let session = q.session.clone();
            let compact = q
                .compact
                .as_deref()
                .is_some_and(|v| v != "0" && v != "false");
            ws.on_upgrade(move |socket| {
                handle_ws(socket, daemon, session, state_path, stream, terms, compact)
            })
        }
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn handle_ws(
    socket: WebSocket,
    daemon: Shared,
    session: String,
    state_path: PathBuf,
    stream: crate::llm::StreamHandle,
    terms: Option<std::sync::Arc<crate::term::SessionTerms>>,
    compact: bool,
) {
    let (mut sender, mut receiver) = socket.split();
    let (history_tx, history_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Option<usize>)>();
    let history_request_tx = history_tx.clone();
    let (event_request_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();

    let push_daemon = daemon.clone();
    let push_session = session.clone();
    let push_state_path = state_path.clone();
    let push_stream = stream.clone();
    let push_terms = terms.clone();
    let push = tokio::spawn(async move {
        let daemon = push_daemon;
        let session = push_session;
        let state_path = push_state_path;
        let stream = push_stream;
        let terms = push_terms;
        let mut history_rx = history_rx;
        let term_client = terms.as_ref().map(|t| t.subscribe());
        let mut last_state_fingerprint = None;
        let mut last_events: Vec<crate::harness::HarnessEvent> = Vec::new();
        let mut last_event_offset = 0usize;
        let mut last_lanes_json: Option<String> = None;
        let mut last_stream_fp: u64 = 0;
        let mut last_queue_revision = 0;
        let mut attach_revision: u64 = 0;
        let mut term_seq: u64 = 0;
        loop {
            let queue_revision = daemon.queue_revision.load(Ordering::Acquire);
            if let Some(fingerprint) = session_state_fingerprint(&daemon.store, &state_path) {
                if Some(fingerprint) != last_state_fingerprint
                    || queue_revision != last_queue_revision
                {
                    let resume = !last_events.is_empty();
                    let from = if resume { last_event_offset } else { 0 };
                    let loaded = read_session_state_tail(&state_path, from).and_then(|(state, _)| {
                        let continuous = resume
                            && state.events.len() >= last_events.len()
                            && state.events[..last_events.len()] == last_events[..];
                        if resume && !continuous {
                            read_session_state_tail(&state_path, 0).map(|(s, _)| (s, false))
                        } else {
                            Some((state, continuous))
                        }
                    });
                    if let Some((mut state, continuous)) = loaded {
                        let hidden = {
                            let mut overlays = daemon.queue_hidden.lock().unwrap();
                            let entries = overlays.entry(session.clone()).or_default();
                            entries
                                .retain(|id| state.queued_inputs.iter().any(|item| &item.id == id));
                            entries.clone()
                        };
                        if !hidden.is_empty() {
                            state
                                .queued_inputs
                                .retain(|item| !hidden.contains(&item.id));
                        }
                        let events = std::mem::take(&mut state.events);
                        if let Ok(mut v) = serde_json::to_value(&state) {
                            if let Some(o) = v.as_object_mut() {
                                o.remove("messages");
                                // Rate limits are PROVIDER-scoped. Only ChatGPT sessions get
                                // the account-wide overlay; every other provider gets NO
                                // rate_limit — including scrubbing a stale snapshot persisted
                                // before a model switch (it showed ChatGPT's monthly limits
                                // on an anthropic-compatible chat).
                                if daemon.session_provider(&session).await == "chatgpt" {
                                    if let Some(g) = crate::chatgpt::read_global_usage() {
                                        if let Ok(gv) = serde_json::to_value(&g) {
                                            o.insert("rate_limit".into(), gv);
                                        }
                                    }
                                } else {
                                    o.remove("rate_limit");
                                }
                            }
                            let lanes_json = v
                                .get("lanes")
                                .map(|l| l.to_string())
                                .unwrap_or_else(|| "[]".to_string());
                            attach_revision = attach_revision.wrapping_add(1);
                            if let Some(o) = v.as_object_mut() {
                                o.insert("revision".into(), serde_json::json!(attach_revision));
                                if continuous {
                                    let tail = &events[last_events.len()..];
                                    o.insert("wire".into(), serde_json::json!("delta"));
                                    o.insert("new_events".into(), wire_events(tail, compact));
                                    o.insert("event_count".into(), serde_json::json!(events.len()));
                                    if last_lanes_json.as_deref() == Some(lanes_json.as_str()) {
                                        o.remove("lanes");
                                    } else if !o.contains_key("lanes") {
                                        o.insert("lanes".into(), serde_json::json!([]));
                                    }
                                    last_events = events;
                                } else {
                                    let (start, _, _) = session_event_page(&events, None, None);
                                    last_event_offset = start;
                                    o.insert("wire".into(), serde_json::json!("snapshot"));
                                    o.insert("events".into(), wire_events(&events[start..], compact));
                                    o.insert("event_offset".into(), serde_json::json!(start));
                                    last_events = events[start..].to_vec();
                                }
                            }
                            last_lanes_json = Some(lanes_json);
                            last_queue_revision = queue_revision;
                            if let Ok(json) = serde_json::to_string(&v) {
                                if sender.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                                last_state_fingerprint = Some(fingerprint);
                            }
                        }
                    }
                }
            }
            {
                use std::hash::{Hash, Hasher};
                let snap = crate::llm::StreamBuffer::snapshot(&stream);
                let think = crate::llm::StreamBuffer::snapshot_thinking(&stream);
                let visible = stream.try_lock().map(|b| b.text_visible).unwrap_or(false);
                let mut h = std::collections::hash_map::DefaultHasher::new();
                snap.hash(&mut h);
                think.hash(&mut h);
                visible.hash(&mut h);
                let fp = h.finish();
                if fp != last_stream_fp {
                    last_stream_fp = fp;
                    let frame = serde_json::json!({
                        "wire": "stream",
                        "text": snap,
                        "thinking": think,
                        "text_visible": visible,
                    });
                    if let Ok(json) = serde_json::to_string(&frame) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            if let Some(terms) = terms.as_ref() {
                use base64::Engine;
                // Incremental PTY bytes only. Replaying raw scrollback into a
                // sized vt100 (tmux/ghostty-style: emulator owns the grid;
                // clients get live bytes or a cell snapshot, never CSI history)
                // is what glued `ls` onto the fish prompt.
                let _ = terms.take_snapshots();
                let frames = match term_client.as_ref() {
                    Some(c) => terms.poll_client(c),
                    None => terms.poll_all(),
                };
                for (id, chunk, cols, rows, alive) in frames {
                    if chunk.is_empty() && alive {
                        continue;
                    }
                    term_seq = term_seq.wrapping_add(1);
                    let frame = serde_json::json!({
                        "wire": "term",
                        "op": "out",
                        "id": id,
                        "seq": term_seq,
                        "data": base64::engine::general_purpose::STANDARD.encode(&chunk),
                        "cols": cols,
                        "rows": rows,
                        "alive": alive,
                    });
                    if let Ok(json) = serde_json::to_string(&frame) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            if let Ok(index) = event_rx.try_recv() {
                if let Some((state, _)) = read_session_state_tail(&state_path, index) {
                    let frame = serde_json::json!({
                        "wire": "event",
                        "index": index,
                        "event": state.events.first(),
                    });
                    if sender
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            if let Ok((before, limit)) = history_rx.try_recv() {
                if let Some((state, _)) = read_session_state_tail(&state_path, 0) {
                    let (start, end, has_older) =
                        session_event_page(&state.events, Some(before), limit);
                    let frame = serde_json::json!({
                        "wire": "history",
                        "events": wire_events(&state.events[start..end], compact),
                        "start": start,
                        "end": end,
                        "has_older": has_older,
                    });
                    if sender
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Each idempotent client input may carry a nonce. On reconnect the mobile
    // client resends with the same nonce so the server drops duplicates.
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                // Parse as raw Value first to extract the nonce without
                // changing the LoopInput serde format.
                let dominated = serde_json::from_str::<serde_json::Value>(t.as_str());
                if let Ok(val) = dominated {
                    if val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        if let Some(terms) = terms.as_ref() {
                            apply_term_client(terms, &val);
                        }
                        continue;
                    }
                    if val.get("kind").and_then(|k| k.as_str()) == Some("history") {
                        let before =
                            val.get("before").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let limit = val.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
                        let _ = history_request_tx.send((before, limit));
                        continue;
                    }
                    if val.get("kind").and_then(|k| k.as_str()) == Some("event") {
                        if let Some(index) = val.get("index").and_then(|v| v.as_u64()) {
                            let _ = event_request_tx.send(index as usize);
                        }
                        continue;
                    }
                    if let Some(nonce) = val.get("nonce").and_then(|n| n.as_str()) {
                        if let Some(kind) = val.get("kind").and_then(|k| k.as_str()) {
                            // User messages and approval/question answers are both
                            // retried across reconnects and must be idempotent.
                            let idempotent = matches!(
                                kind,
                                "user_message"
                                    | "answer"
                                    | "approve"
                                    | "approve_all"
                                    | "deny"
                                    | "queue"
                                    | "unqueue"
                                    | "steer_queued"
                                    | "drop_queued"
                            );
                            if idempotent && !daemon.accept_nonce(&session, nonce) {
                                continue; // duplicate — drop silently
                            }
                        }
                    }
                }
                if let Ok(input) = serde_json::from_str::<LoopInput>(t.as_str()) {
                    daemon.deliver(&session, input).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

fn apply_term_client(terms: &crate::term::SessionTerms, val: &serde_json::Value) {
    use base64::Engine;
    let op = val.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
    let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let id = val
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    match op {
        "open" | "new" => {
            // Honor the client's pane id. Allocating a different one left the
            // TUI painting an empty pane while output landed on an unseen id.
            let id = if op == "new" && id.is_empty() {
                terms.alloc_id()
            } else {
                id
            };
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
                // Do not snapshot raw scrollback — replaying it into vt100
                // at the current size scrambles fish/zsh/bash prompts.
            }
        }
        "resize" => {
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
            }
        }
        "in" => {
            // Typing must spawn the pane if `open`/`new` never landed
            // (blank Ctrl-N tab). Dropping `in` on a missing id is why
            // keys look captured but never paint.
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                if let Some(data) = val.get("data").and_then(|v| v.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        term.write(&bytes);
                    }
                }
            }
        }
        "close" => terms.close(&id),
        _ => {}
    }
}

// WS /events — device-wide firehose. Emits a compact event on status changes
// (including running) so the session list can update live, even for chats the
// app isn't painting. `notify` is true when the event should also raise an OS
// banner; the app still receives every frame for UI.
pub(crate) async fn events_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(a): Query<Auth>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    ws.on_upgrade(move |socket| handle_events_ws(socket, d))
}

fn allow_device_event(daemon: &Daemon, event: &serde_json::Value) -> bool {
    let settings = mc::load_settings(&daemon.mission_control_root);
    if settings.notification_policy == "none" {
        return false;
    }
    if settings.notification_policy != "mission_control_only" {
        return true;
    }
    let session = event.get("session").and_then(|v| v.as_str()).unwrap_or("");
    settings.mission_control_session_id.as_deref() == Some(session)
        || session == mc::SESSION_ID
}

async fn handle_events_ws(socket: WebSocket, daemon: Shared) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = subscribe_device_events();
    let push = tokio::spawn(async move {
        // Idle PTYs still need a pump so BEL / OSC fire when nobody is attached.
        // 5s is plenty — the firehose itself is push, not this tick.
        let mut harvest = tokio::time::interval(Duration::from_secs(5));
        harvest.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Ok(e) => {
                            let mut e = e;
                            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                            // Always push to the UI. OS banners stay policy-gated
                            // and never fire just because a chat started running.
                            let notify = kind != "running"
                                && kind != "models"
                                && kind != "coordination_event"
                                && allow_device_event(&daemon, &e);
                            if let Some(obj) = e.as_object_mut() {
                                obj.insert("notify".into(), serde_json::Value::Bool(notify));
                            }
                            if sender
                                .send(Message::Text(e.to_string().into()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
                _ = harvest.tick() => {
                    let live = daemon.sessions.lock().await;
                    for sess in live.values() {
                        sess.terms.pump();
                    }
                }
            }
        }
    });
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Close(_) = msg {
            break;
        }
    }
    push.abort();
}


