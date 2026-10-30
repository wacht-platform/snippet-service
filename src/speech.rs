use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

struct FluxProvisional {
    turn: Arc<crate::tts_stream::StreamTurn>,
    token: u64,
}

static FLUX_STREAMING: Mutex<Option<FluxProvisional>> = Mutex::new(None);
static STREAM_INTERRUPTED: Mutex<Option<String>> = Mutex::new(None);
static DIRECT: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);
static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn normalise(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn spoken_directly(text: &str) -> bool {
    DIRECT.lock().unwrap().as_ref().is_some_and(|(at, t)| at.elapsed().as_secs() < 20 && *t == normalise(text))
}

pub fn pending() -> bool {
    crate::tts_stream::busy()
}

pub fn speakable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn cut_view(text: &str) -> Option<(String, &'static str)> {
    let wanted = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (status, heard): (String, Option<String>) = crate::ambient::store()
        .ok()
        .and_then(|s| {
            s.with_connection(|c| {
                c.query_row(
                    "SELECT status, heard_text FROM ambient_turns WHERE role = 'assistant' AND TRIM(text) = ?1 ORDER BY id DESC LIMIT 1",
                    [&wanted],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
            })
            .ok()
        })?;
    if status != "interrupted" && status != "cancelled" {
        return None;
    }
    let heard = heard.unwrap_or_default().trim().to_string();
    let note = if heard.is_empty() {
        "the user interrupted before any of this was spoken"
    } else {
        "the user cut this off here; the rest was never spoken"
    };
    Some((heard, note))
}

pub fn has_interruption() -> bool {
    STREAM_INTERRUPTED.lock().unwrap().is_some() || crate::tts_stream::is_reconciling()
}

pub fn take_interruption() -> Option<String> {
    STREAM_INTERRUPTED.lock().unwrap().take()
}

pub(crate) fn note_stream_interruption(text: String) {
    *STREAM_INTERRUPTED.lock().unwrap() = Some(text);
    crate::ambient::emit_state("interrupted");
}

fn emit(value: Value) {
    crate::session::emit_device_event(value);
}

async fn prepare() -> Result<(String, crate::ambient::Conversation), String> {
    let Some((device, _)) = crate::ambient::lease_holder() else {
        return Err("no device is listening, so there is nobody to speak to".into());
    };
    static TTL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if crate::ambient::conversation().is_none() || TTL.load(Ordering::Relaxed) == 0 {
        if !crate::ambient::effective_active().await {
            crate::ambient::set_mode("active", "speaking to you").await?;
        }
        let ttl = crate::config::SnippetConfig::load(crate::config::default_config_path())
            .await
            .map(|c| c.ambient.conversation_timeout_secs)
            .unwrap_or(90);
        TTL.store(ttl.max(1), Ordering::Relaxed);
    }
    let conversation = crate::ambient::open_conversation("mission-control", TTL.load(Ordering::Relaxed));
    Ok((device, conversation))
}

fn finalise_text(turn: i64, text: &str) {
    if let Ok(store) = crate::ambient::store() {
        store.write(|c| {
            c.execute("UPDATE ambient_turns SET text = ?2 WHERE id = ?1", rusqlite::params![turn, text])?;
            Ok(())
        });
    }
}

pub fn asks_aloud(text: &str, expecting: Option<&str>) -> bool {
    text.contains('?') && expecting.is_some_and(|k| matches!(k, "answer" | "confirmation" | "choice" | "dictation"))
}

enum Reuse {
    Done,
    Rest(String),
    Unrelated,
}

fn streamed(turn: i64) -> Value {
    json!({"spoken": true, "turn": turn, "queued": true, "streamed": true})
}

fn reuse_flux_turn(p: &FluxProvisional, text: &str) -> Reuse {
    let sent = p.turn.text();
    if normalise(text).starts_with(&normalise(&sent)) {
        let rest = match text.strip_prefix(sent.trim_start()) {
            Some(r) => r.to_string(),
            None => {
                let skip = sent.split_whitespace().count();
                format!(" {}", text.split_whitespace().skip(skip).collect::<Vec<_>>().join(" "))
            }
        };
        if p.turn.is_stopped() || rest.trim().is_empty() || p.turn.push(&rest) {
            p.turn.flush();
            finalise_text(p.turn.turn, text);
            return Reuse::Done;
        }
        return Reuse::Rest(rest.trim().to_string());
    }
    let said: Vec<&str> = sent.split_whitespace().collect();
    let want: Vec<&str> = text.split_whitespace().collect();
    let common = said.iter().zip(&want).take_while(|(a, b)| a.eq_ignore_ascii_case(b)).count();
    if p.turn.is_stopped() || said.is_empty() || common < said.len().min(3) {
        return Reuse::Unrelated;
    }
    let extra = want.iter().skip(said.len()).copied().collect::<Vec<_>>().join(" ");
    if !extra.is_empty() {
        p.turn.push(&format!(" {extra}"));
    }
    p.turn.flush();
    finalise_text(p.turn.turn, text);
    Reuse::Done
}

pub async fn speak(text: &str, expecting: Option<&str>, about: Option<&str>, replace: bool) -> Result<Value, String> {
    let clean = speakable(text);
    let text = clean.trim();
    if text.is_empty() {
        return Err("nothing to say (bracketed notes are never spoken)".into());
    }
    if crate::ambient::call_quiet() {
        return Err("the user is on a call: speech is off until it ends, so reply in text".into());
    }
    let (device, conversation) = prepare().await?;
    if let Some(kind) = expecting.filter(|k| !k.is_empty() && *k != "none") {
        crate::ambient::set_expectation(kind, about.unwrap_or(""), 10);
    }
    let mut say = text.to_string();
    let flux = {
        let mut guard = FLUX_STREAMING.lock().unwrap();
        let p = guard.take();
        if p.is_none() {
            *DIRECT.lock().unwrap() = Some((std::time::Instant::now(), normalise(text)));
        }
        p
    };
    if let Some(p) = flux {
        match reuse_flux_turn(&p, text) {
            Reuse::Done => return Ok(streamed(p.turn.turn)),
            Reuse::Rest(rest) => say = rest,
            Reuse::Unrelated => {}
        }
        p.turn.flush();
    }
    let text = say.as_str();
    if let Some(t) = crate::tts_stream::find_recent(&device, text, 8) {
        t.flush();
        finalise_text(t.turn, text);
        return Ok(streamed(t.turn));
    }
    *DIRECT.lock().unwrap() = Some((std::time::Instant::now(), normalise(text)));
    if replace {
        cancel_all("replaced");
    }
    let session = crate::tts_stream::ready(&device)
        .await
        .ok_or_else(|| "speech stream is not ready for the listening device".to_string())?;
    let t = crate::tts_stream::start_turn(&session, &device, &conversation.id)?;
    finalise_text(t.turn, text);
    t.push(text);
    t.flush();
    Ok(streamed(t.turn))
}

pub fn is_audible(device: &str) -> bool {
    crate::tts_stream::audible(device)
}

fn open_turns(device: &str) -> Vec<i64> {
    let Ok(store) = crate::ambient::store() else {
        return Vec::new();
    };
    store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM ambient_turns WHERE device = ?1 AND role = 'assistant' AND created_at >= ?2
                   AND status NOT IN ('played', 'missed', 'interrupted', 'cancelled', 'failed') ORDER BY id",
            )?;
            let since = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)) as i64 - 300;
            let rows = stmt.query_map(rusqlite::params![device, since], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
}

pub fn interrupt(device: &str) -> bool {
    let open = open_turns(device);
    if open.is_empty() && !crate::tts_stream::busy_for(device) {
        return false;
    }
    cancel_all("interrupted");
    if let Ok(store) = crate::ambient::store() {
        for turn in &open {
            store.write(|c| {
                c.execute(
                    "UPDATE ambient_turns SET status = 'interrupted' WHERE id = ?1 AND status NOT IN ('played', 'missed', 'interrupted', 'cancelled')",
                    [turn],
                )?;
                Ok(())
            });
        }
    }
    emit(json!({"kind": "ambient_speech_interrupt", "device": device, "turns": open}));
    crate::ambient::emit_state("interrupted");
    true
}

pub fn cancel_all(reason: &str) {
    crate::tts_stream::stop(None, reason, None);
}

fn read_string(b: &[char], mut i: usize) -> (String, usize, bool) {
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(i) else {
            return (out, i, false);
        };
        match c {
            '"' => return (out, i + 1, true),
            '\\' => {
                let Some(&n) = b.get(i + 1) else {
                    return (out, i, false);
                };
                match n {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let hex = |at: usize| -> Option<u32> {
                            let s: String = b.get(at..at + 4)?.iter().collect();
                            u32::from_str_radix(&s, 16).ok()
                        };
                        let Some(high) = hex(i + 2) else {
                            return (out, i, false);
                        };
                        if (0xD800..0xDC00).contains(&high) {
                            if b.get(i + 6) == Some(&'\\') && b.get(i + 7) == Some(&'u') {
                                let Some(low) = hex(i + 8) else {
                                    return (out, i, false);
                                };
                                let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                i += 12;
                                continue;
                            }
                            return (out, i, false);
                        }
                        out.push(char::from_u32(high).unwrap_or('\u{fffd}'));
                        i += 6;
                        continue;
                    }
                    other => out.push(other),
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
}

fn extract_text(raw: &str) -> Option<(String, bool)> {
    let b: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let skip = |i: &mut usize| {
        while *i < b.len() && b[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip(&mut i);
    if b.get(i) != Some(&'{') {
        return None;
    }
    i += 1;
    loop {
        skip(&mut i);
        if i >= b.len() {
            return None;
        }
        if b[i] == ',' {
            i += 1;
            skip(&mut i);
        }
        if b.get(i) == Some(&'}') {
            return None;
        }
        if b.get(i) != Some(&'"') {
            return None;
        }
        let (key, next, closed) = read_string(&b, i + 1);
        if !closed {
            return None;
        }
        i = next;
        skip(&mut i);
        if b.get(i) != Some(&':') {
            return None;
        }
        i += 1;
        skip(&mut i);
        if key == "text" {
            if b.get(i) != Some(&'"') {
                return None;
            }
            let (val, _, done) = read_string(&b, i + 1);
            return Some((val, done));
        }
        match b.get(i)? {
            '"' => {
                let (_, next, done) = read_string(&b, i + 1);
                if !done {
                    return None;
                }
                i = next;
            }
            '{' | '[' => {
                let (open, close) = if b[i] == '{' { ('{', '}') } else { ('[', ']') };
                let mut depth = 0i32;
                while i < b.len() {
                    if b[i] == '"' {
                        let (_, next, done) = read_string(&b, i + 1);
                        if !done {
                            return None;
                        }
                        i = next;
                        continue;
                    }
                    if b[i] == open {
                        depth += 1;
                    } else if b[i] == close {
                        depth -= 1;
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            _ => {
                while i < b.len() && !matches!(b[i], ',' | '}') {
                    i += 1;
                }
                if i >= b.len() {
                    return None;
                }
            }
        }
    }
}

enum Route {
    Pending,
    Flux(Arc<crate::tts_stream::StreamTurn>, u64),
    Off,
}

fn fresh_route() -> Route {
    if crate::tts_stream::enabled() && !crate::ambient::call_quiet() { Route::Pending } else { Route::Off }
}

fn direct_claims(text: &str, done: bool) -> Option<Route> {
    let guard = DIRECT.lock().unwrap();
    let (_, said) = guard.as_ref().filter(|(at, _)| at.elapsed().as_secs() < 20)?;
    let n = normalise(text);
    if !said.starts_with(&n) {
        return None;
    }
    Some(if done || n.len() >= 24 || *said == n { Route::Off } else { Route::Pending })
}

async fn open_flux(text: &str, done: bool) -> Route {
    if let Some(r) = direct_claims(text, done) {
        return r;
    }
    let Ok((device, conversation)) = prepare().await else {
        return Route::Off;
    };
    let Some(session) = crate::tts_stream::ready(&device).await else {
        return Route::Off;
    };
    let mut guard = FLUX_STREAMING.lock().unwrap();
    if let Some(r) = direct_claims(text, done) {
        return r;
    }
    match crate::tts_stream::start_turn(&session, &device, &conversation.id) {
        Ok(turn) => {
            let token = TOKEN.fetch_add(1, Ordering::SeqCst);
            *guard = Some(FluxProvisional { turn: turn.clone(), token });
            Route::Flux(turn, token)
        }
        Err(e) => {
            eprintln!("[ambient-flux] could not start a streamed turn: {e}");
            Route::Off
        }
    }
}

fn drop_flux_provisional(token: u64) {
    let mut guard = FLUX_STREAMING.lock().unwrap();
    if guard.as_ref().is_some_and(|p| p.token == token) {
        if let Some(p) = guard.take() {
            p.turn.flush();
        }
    }
}

fn push_suffix(turn: &Arc<crate::tts_stream::StreamTurn>, text: &str, pushed: &mut usize) {
    let total = text.chars().count();
    if total > *pushed {
        turn.push(&text.chars().skip(*pushed).collect::<String>());
        *pushed = total;
    }
}

pub async fn consume_tool_stream(mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::llm::ToolArgEvent>) {
    use crate::llm::ToolArgEvent;
    let mut call: Option<String> = None;
    let mut raw = String::new();
    let mut route = Route::Off;
    let mut pushed = 0usize;
    while let Some(event) = rx.recv().await {
        match event {
            ToolArgEvent::Start { call_id, name } => {
                if let Route::Flux(turn, token) = &route {
                    drop_flux_provisional(*token);
                    turn.flush();
                }
                if name == "speak" || name.ends_with("__speak") {
                    crate::ambient::mark_last("speak_call");
                    call = Some(call_id);
                    raw.clear();
                    pushed = 0;
                    route = fresh_route();
                } else {
                    call = None;
                    route = Route::Off;
                }
            }
            ToolArgEvent::Delta { call_id, chunk } => {
                if call.as_deref() != Some(call_id.as_str()) {
                    continue;
                }
                raw.push_str(&chunk);
                let Some((text, done)) = extract_text(&raw) else { continue };
                let text = speakable(&text);
                if matches!(route, Route::Pending) && !text.trim().is_empty() {
                    route = open_flux(&text, done).await;
                }
                if let Route::Flux(turn, _) = &route {
                    push_suffix(turn, &text, &mut pushed);
                }
            }
            ToolArgEvent::End { call_id } => {
                if call.as_deref() != Some(call_id.as_str()) {
                    continue;
                }
                let text = extract_text(&raw).map(|(t, _)| speakable(&t));
                if matches!(route, Route::Pending) {
                    route = match text.as_deref().filter(|t| !t.trim().is_empty() && !spoken_directly(t)) {
                        Some(t) => open_flux(t, true).await,
                        None => Route::Off,
                    };
                }
                if let Route::Flux(turn, token) = &route {
                    if let Some(t) = &text {
                        push_suffix(turn, t, &mut pushed);
                        finalise_text(turn.turn, t.trim());
                    }
                    turn.flush();
                    let token = *token;
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        drop_flux_provisional(token);
                    });
                }
                route = Route::Off;
                call = None;
                raw.clear();
                pushed = 0;
            }
            ToolArgEvent::Reset => {
                if let Route::Flux(turn, token) = &route {
                    drop_flux_provisional(*token);
                    turn.flush();
                }
                route = Route::Off;
                call = None;
                raw.clear();
                pushed = 0;
            }
        }
    }
    if let Route::Flux(_, token) = route {
        drop_flux_provisional(token);
    }
}

pub fn mission_control_stream() -> crate::llm::StreamHandle {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(consume_tool_stream(rx));
    let mut b = crate::llm::StreamBuffer::default();
    b.set_tool_tap(tx);
    Arc::new(Mutex::new(b))
}

#[cfg(test)]
mod tests {
    use super::speakable;

    #[test]
    fn bracketed_notes_are_never_spoken() {
        assert_eq!(speakable("[the user interrupted before any of this was spoken]").trim(), "");
        assert_eq!(speakable("Got it [cut off by the user; the rest was never spoken]").trim(), "Got it");
        assert_eq!(speakable("Hello [unfinished"), "Hello ");
        assert_eq!(speakable("Plain reply."), "Plain reply.");
    }
}
