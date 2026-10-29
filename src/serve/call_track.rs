use super::*;

use std::collections::VecDeque;

use serde_json::{Value, json};

use super::ambient_router::Part;
use super::ambient_watch::Verdict;
use super::call_detect::{Class, Detector, Signal, WINDOW_MS};
use super::speakers::Speaker;

pub(crate) const PROMPT_MS: i64 = 120_000;
pub(crate) const END_QUIET_MS: i64 = 120_000;
const MEMORABLE: f64 = 0.65;
const CLEAR_TO_ASSISTANT: f64 = 0.85;
const RING: usize = 40;
const MAX_LINES: usize = 4000;
const RESOLVED: usize = 32;
const TRANSCRIPT_CHARS: usize = 60_000;
pub(crate) const TITLE: &str = "Take notes on this conversation?";
pub(crate) const BODY: &str = "Sounds like you're talking with someone. Snippet can keep notes.";

#[derive(Clone, Debug)]
pub(crate) struct Heard {
    pub at_ms: i64,
    pub ids: Vec<String>,
    pub text: String,
    pub speaker: Speaker,
    pub memorable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Pending,
    Tracking,
}

#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub id: String,
    pub device: String,
    pub phase: Phase,
    pub detected_at: i64,
    pub expires_at: i64,
    pub started_at: i64,
    pub last_call_like: i64,
    pub lines: Vec<Heard>,
}

#[derive(Debug)]
pub(crate) enum Effect {
    Prompt { call_id: String, expires_at: i64 },
    State { call_id: String, state: &'static str },
    Begin(Call),
    Append { call_id: String, line: Heard },
    Scrub { device: String, ids: Vec<String> },
    Note(String),
    Summarise { call: Call, ended_at: i64 },
    Quiet(bool),
}

pub(crate) type Refusal = (StatusCode, &'static str);

/// Pure call state: detection, the pre-approval ring and the pending/tracking lifecycle.
#[derive(Default)]
pub(crate) struct CallBook {
    detectors: HashMap<String, Detector>,
    recent: HashMap<String, VecDeque<Heard>>,
    active: Option<Call>,
    resolved: VecDeque<(String, &'static str)>,
    held: VecDeque<(String, i64, Heard)>,
}

impl CallBook {
    pub fn observe(&mut self, device: &str, signal: &Signal, heard: Heard, now: i64) -> Vec<Effect> {
        let mut fx = self.tick(now);
        let seen = self.detectors.entry(device.to_string()).or_default().observe(signal);
        let keep = !matches!(seen.class, Class::Assistant | Class::Played) && signal.words > 0;
        let on_call = self.holds(device);
        if heard.memorable && !(on_call && keep) {
            self.held.push_back((device.to_string(), now, heard.clone()));
        }
        if let Some(call) = self.active.as_mut().filter(|c| c.device == device) {
            if seen.class.call_like() {
                call.last_call_like = now;
            }
            if keep && call.lines.len() < MAX_LINES {
                if call.phase == Phase::Tracking {
                    fx.push(Effect::Append { call_id: call.id.clone(), line: heard.clone() });
                }
                call.lines.push(heard);
            }
            return fx;
        }
        let ring = self.recent.entry(device.to_string()).or_default();
        if keep {
            ring.push_back(heard);
        }
        while ring.len() > RING || ring.front().is_some_and(|h| signal.at_ms - h.at_ms > 2 * WINDOW_MS) {
            ring.pop_front();
        }
        if let (Some(first), None) = (seen.trigger, &self.active) {
            let lines: Vec<Heard> = ring.drain(..).filter(|h| h.at_ms >= first).collect();
            self.held.retain(|(d, _, h)| d != device || h.at_ms < first);
            let call = Call {
                id: uuid::Uuid::new_v4().to_string(),
                device: device.to_string(),
                phase: Phase::Pending,
                detected_at: now,
                expires_at: now + PROMPT_MS,
                started_at: first,
                last_call_like: now,
                lines,
            };
            fx.push(Effect::Quiet(true));
            fx.push(Effect::Prompt { call_id: call.id.clone(), expires_at: call.expires_at });
            self.active = Some(call);
        }
        fx
    }

    pub fn tick(&mut self, now: i64) -> Vec<Effect> {
        let mut fx = Vec::new();
        // Notes wait out a detection window so a call found later can still hold them back.
        let busy = self.active.as_ref().map(|c| c.device.clone());
        while let Some(i) = self.held.iter().position(|(d, at, _)| now - at >= WINDOW_MS && busy.as_deref() != Some(d)) {
            let (_, _, h) = self.held.remove(i).unwrap();
            fx.push(Effect::Note(h.text));
        }
        let Some(call) = &self.active else { return fx };
        match call.phase {
            Phase::Pending if now >= call.expires_at => self.drop_call("expired", &mut fx),
            Phase::Tracking if now - call.last_call_like >= END_QUIET_MS => self.end(now, &mut fx),
            _ => {}
        }
        fx
    }

    pub fn track(&mut self, id: &str, now: i64, fx: &mut Vec<Effect>) -> Result<&'static str, Refusal> {
        fx.extend(self.tick(now));
        match self.active.as_mut() {
            Some(c) if c.id == id && c.phase == Phase::Pending => {
                c.phase = Phase::Tracking;
                c.last_call_like = now;
                fx.push(Effect::Begin(c.clone()));
                fx.push(Effect::State { call_id: c.id.clone(), state: "tracking" });
                Ok("tracking")
            }
            _ => Err(self.refusal(id)),
        }
    }

    pub fn ignore(&mut self, id: &str, now: i64, fx: &mut Vec<Effect>) -> Result<&'static str, Refusal> {
        fx.extend(self.tick(now));
        match &self.active {
            Some(c) if c.id == id && c.phase == Phase::Pending => {
                self.drop_call("declined", fx);
                Ok("declined")
            }
            _ => Err(self.refusal(id)),
        }
    }

    pub fn stop(&mut self, id: &str, now: i64, fx: &mut Vec<Effect>) -> Result<&'static str, Refusal> {
        fx.extend(self.tick(now));
        match &self.active {
            Some(c) if c.id == id && c.phase == Phase::Tracking => {
                self.end(now, fx);
                Ok("ended")
            }
            _ => Err(self.refusal(id)),
        }
    }

    pub fn summarised(&mut self, id: &str) {
        if let Some(entry) = self.resolved.iter_mut().find(|(c, _)| c == id) {
            entry.1 = "summarised";
        }
    }

    pub fn active(&self) -> Option<&Call> {
        self.active.as_ref()
    }

    pub fn holds(&self, device: &str) -> bool {
        self.active.as_ref().is_some_and(|c| c.device == device)
    }

    fn refusal(&self, id: &str) -> Refusal {
        if let Some(c) = self.active.as_ref().filter(|c| c.id == id) {
            return (StatusCode::CONFLICT, if c.phase == Phase::Pending { "pending" } else { "tracking" });
        }
        match self.resolved.iter().find(|(c, _)| c == id) {
            Some((_, state)) => (StatusCode::CONFLICT, state),
            None => (StatusCode::NOT_FOUND, "unknown"),
        }
    }

    fn resolve(&mut self, id: String, state: &'static str) {
        self.resolved.push_back((id, state));
        while self.resolved.len() > RESOLVED {
            self.resolved.pop_front();
        }
    }

    fn drop_call(&mut self, state: &'static str, fx: &mut Vec<Effect>) {
        let Some(call) = self.active.take() else { return };
        fx.push(Effect::Scrub { device: call.device.clone(), ids: call.lines.iter().flat_map(|l| l.ids.iter().cloned()).collect() });
        fx.push(Effect::State { call_id: call.id.clone(), state });
        fx.push(Effect::Quiet(false));
        self.resolve(call.id, state);
    }

    fn end(&mut self, now: i64, fx: &mut Vec<Effect>) {
        let Some(call) = self.active.take() else { return };
        fx.push(Effect::State { call_id: call.id.clone(), state: "ended" });
        fx.push(Effect::Quiet(false));
        self.resolve(call.id.clone(), "ended");
        fx.push(Effect::Summarise { call, ended_at: now });
    }
}

fn book() -> std::sync::MutexGuard<'static, CallBook> {
    static BOOK: std::sync::OnceLock<std::sync::Mutex<CallBook>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A call is pending or tracked on this device: hold everything that isn't clearly for the assistant.
pub(super) fn holds(device: &str) -> bool {
    book().holds(device)
}

pub(super) fn clearly_for_assistant(v: &Verdict, wake: bool) -> bool {
    wake || (v.kind == "conversation" && v.addressed >= CLEAR_TO_ASSISTANT && v.veto() < 0.5 && v.out_of_context < 0.5)
}

pub(super) async fn observe(d: &Shared, device: &str, parts: &[Part], v: &Verdict, delivered: bool, wake: bool) {
    let Some(first) = parts.first() else { return };
    let text = parts.iter().map(|p| p.text.trim()).collect::<Vec<_>>().join(" ");
    let signal = Signal {
        at_ms: (first.started * 1000.0) as i64,
        kind: v.kind.clone(),
        intent: v.intent.clone(),
        addressed: v.addressed,
        played: v.played,
        echo: v.echo,
        delivered,
        wake,
        media: false,
        during_playback: crate::tts_stream::busy_for(device),
        words: super::call_detect::meaningful_words(&text),
        user_voice: v.user_voice(),
        other: v.other,
        dur_ms: ((parts.last().map_or(first.ended, |p| p.ended) - first.started) * 1000.0) as i64,
        speaker: None,
    };
    let played = v.kind == "media" || v.played >= 0.8;
    let speaker = super::speakers::assign(device, &first.id, signal.at_ms, v.voice.as_deref(), played);
    let signal = Signal { speaker: Some(speaker), ..signal };
    let ids: Vec<String> = parts.iter().map(|p| p.id.clone()).collect();
    let label = speaker.label();
    for id in &ids {
        d.store.write(|c| c.execute("UPDATE ambient_events SET speaker = ?2 WHERE id = ?1", rusqlite::params![id, label]).map(|_| ()));
    }
    let heard = Heard { at_ms: signal.at_ms, ids, text, speaker, memorable: !delivered && v.kept() && v.memorable >= MEMORABLE };
    let fx = book().observe(device, &signal, heard, now_ms());
    if !fx.is_empty() {
        ensure_ticker(d);
    }
    apply(d, fx).await;
}

fn ensure_ticker(d: &Shared) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        let d = d.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let fx = book().tick(now_ms());
                apply(&d, fx).await;
            }
        });
    });
}

async fn apply(d: &Shared, fx: Vec<Effect>) {
    for effect in fx {
        match effect {
            Effect::Prompt { call_id, expires_at } => {
                crate::ambient::end_conversation();
                eprintln!("[call] {} detected, asking", short(&call_id));
                crate::session::emit_device_event(prompt_event(&call_id, expires_at));
            }
            Effect::State { call_id, state } => {
                eprintln!("[call] {} {state}", short(&call_id));
                set_row_state(d, &call_id, state);
                crate::session::emit_device_event(state_event(&call_id, state));
            }
            Effect::Quiet(on) => crate::ambient::set_call_quiet(on),
            Effect::Begin(call) => persist_begin(d, &call),
            Effect::Append { call_id, line } => persist_line(d, &call_id, &line),
            Effect::Scrub { device, ids } => {
                super::ambient_router::forget_out_of_context(&device);
                scrub(d, &ids).await
            }
            Effect::Note(text) => {
                super::agent_routes::record_agent_message(d, crate::mission_control::SESSION_ID, "voice classifier", &text, false).await;
            }
            Effect::Summarise { call, ended_at } => {
                let d = d.clone();
                tokio::spawn(async move { summarise(&d, call, ended_at).await });
            }
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

pub(crate) fn prompt_event(call_id: &str, expires_at: i64) -> Value {
    json!({
        "kind": "call_prompt",
        "source_key": format!("call_prompt:{call_id}"),
        "ttl_secs": PROMPT_MS / 1000,
        "payload": {"type": "call_prompt", "call_id": call_id, "expires_at": expires_at, "title": TITLE, "body": BODY},
    })
}

pub(crate) fn state_event(call_id: &str, state: &str) -> Value {
    json!({
        "kind": "call_state",
        "source_key": format!("call_state:{call_id}:{state}"),
        "payload": {"type": "call_state", "call_id": call_id, "state": state},
    })
}

fn persist_begin(d: &Shared, call: &Call) {
    d.store.write(|c| {
        c.execute(
            "INSERT OR IGNORE INTO ambient_calls (id, device, detected_at, started_at, state) VALUES (?1, ?2, ?3, ?4, 'tracking')",
            rusqlite::params![call.id, call.device, call.detected_at, call.started_at],
        )?;
        Ok(())
    });
    for line in &call.lines {
        persist_line(d, &call.id, line);
    }
}

fn persist_line(d: &Shared, call_id: &str, line: &Heard) {
    let ids = serde_json::to_string(&line.ids).unwrap_or_else(|_| "[]".into());
    d.store.write(|c| {
        c.execute(
            "INSERT INTO ambient_call_lines (call_id, at_ms, speaker, text, event_ids) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![call_id, line.at_ms, line.speaker.label(), line.text, ids],
        )?;
        Ok(())
    });
}

fn set_row_state(d: &Shared, call_id: &str, state: &str) {
    d.store.write(|c| {
        c.execute(
            "UPDATE ambient_calls SET state = ?2,
                 ended_at = CASE WHEN ?2 IN ('ended', 'summarised') THEN COALESCE(ended_at, ?3) ELSE ended_at END
             WHERE id = ?1",
            rusqlite::params![call_id, state, now_ms()],
        )?;
        Ok(())
    });
}

/// A declined or expired call keeps nothing: the lines' text, search index and audio go.
async fn scrub(d: &Shared, ids: &[String]) {
    for id in ids {
        d.store.write(|c| scrub_row(c, id));
        let _ = tokio::fs::remove_file(super::ambient_routes::audio_path(id)).await;
    }
}

fn scrub_row(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO ambient_events_fts(ambient_events_fts, rowid, text)
         SELECT 'delete', rowid, text FROM ambient_events WHERE id = ?1 AND kept = 1 AND text != ''",
        [id],
    )?;
    c.execute("UPDATE ambient_events SET text = '', kept = 0 WHERE id = ?1", [id])?;
    Ok(())
}

pub(crate) fn transcript(lines: &[Heard]) -> String {
    let mut out = String::new();
    for line in lines {
        let at = chrono::DateTime::from_timestamp_millis(line.at_ms).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
        let row = format!("[{at}] {}: {}\n", line.speaker.label(), line.text.trim());
        if out.len() + row.len() > TRANSCRIPT_CHARS {
            out.push_str("[transcript cut here: too long]\n");
            break;
        }
        out.push_str(&row);
    }
    out
}

async fn summarise(d: &Shared, call: Call, ended_at: i64) {
    for line in call.lines.iter().filter(|l| l.memorable) {
        let body = format!("Said on a call: {}", line.text.trim());
        super::agent_routes::record_agent_message(d, crate::mission_control::SESSION_ID, "voice classifier", &body, false).await;
    }
    let iso = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()).unwrap_or_default();
    let msg = format!(
        "<call_ended id=\"{}\" device=\"{}\" started=\"{}\" ended=\"{}\" lines=\"{}\">The user asked Snippet to track this call and it has ended. Lines are labelled You (the user's voice), Voice 1, Voice 2 and so on (other people, grouped by voice) or unknown (too short to tell). Reply in this chat in text only, without speaking: a short summary, the decisions made, and the action items with owners and dates where they were stated. Keep commitments the user made the way you keep other remembered notes. If nothing of substance was said, say so in one line.\n<transcript>\n{}</transcript></call_ended>",
        super::ambient_watch::escape(&call.id),
        super::ambient_watch::escape(&call.device),
        iso(call.started_at),
        iso(ended_at),
        call.lines.len(),
        super::ambient_watch::escape(&transcript(&call.lines)),
    );
    super::autonomy::deliver_to_mission_control(d, msg).await;
    book().summarised(&call.id);
    set_row_state(d, &call.id, "summarised");
    crate::session::emit_device_event(state_event(&call.id, "summarised"));
}

pub(crate) fn status_json(call: Option<&Call>) -> Value {
    match call {
        None => json!({"state": "idle"}),
        Some(c) => json!({
            "call_id": c.id,
            "state": if c.phase == Phase::Pending { "pending" } else { "tracking" },
            "device": c.device,
            "detected_at": c.detected_at,
            "expires_at": (c.phase == Phase::Pending).then_some(c.expires_at),
            "started_at": c.started_at,
            "lines": c.lines.len(),
        }),
    }
}

pub(crate) async fn status(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let fx = book().tick(now_ms());
    apply(&d, fx).await;
    Json(status_json(book().active())).into_response()
}

async fn answer(d: Shared, token: Option<String>, id: String, act: fn(&mut CallBook, &str, i64, &mut Vec<Effect>) -> Result<&'static str, Refusal>) -> Response {
    if !d.authed(&token) {
        return unauthorized();
    }
    let mut fx = Vec::new();
    let result = act(&mut book(), &id, now_ms(), &mut fx);
    ensure_ticker(&d);
    match result {
        Ok(state) => {
            apply(&d, fx).await;
            Json(json!({"state": state})).into_response()
        }
        Err((code, state)) => {
            apply(&d, fx).await;
            (code, Json(json!({"state": state}))).into_response()
        }
    }
}

pub(crate) async fn track(State(d): State<Shared>, axum::extract::Path(id): axum::extract::Path<String>, Query(a): Query<Auth>) -> Response {
    answer(d, a.token, id, CallBook::track).await
}

pub(crate) async fn ignore(State(d): State<Shared>, axum::extract::Path(id): axum::extract::Path<String>, Query(a): Query<Auth>) -> Response {
    answer(d, a.token, id, CallBook::ignore).await
}

pub(crate) async fn stop(State(d): State<Shared>, axum::extract::Path(id): axum::extract::Path<String>, Query(a): Query<Auth>) -> Response {
    answer(d, a.token, id, CallBook::stop).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000;

    fn signal(at_ms: i64, intent: &str) -> Signal {
        Signal { at_ms, kind: "conversation".into(), intent: intent.into(), addressed: 0.2, words: 4, ..Signal::default() }
    }

    fn heard(at_ms: i64, n: usize) -> Heard {
        Heard { at_ms, ids: vec![format!("e{n}")], text: format!("line {n}"), speaker: if n % 2 == 0 { Speaker::You } else { Speaker::Voice(1) }, memorable: false }
    }

    /// Feeds a back-and-forth until it prompts; returns the effects and the clock.
    fn converse(book: &mut CallBook, from: i64, lines: usize) -> (Vec<Effect>, i64) {
        let mut fx = Vec::new();
        let mut now = from;
        for n in 0..lines {
            now = from + n as i64 * 8_000;
            fx.extend(book.observe("mac", &signal(now, if n % 2 == 0 { "meeting" } else { "note" }), heard(now, n), now));
        }
        (fx, now)
    }

    fn prompted(fx: &[Effect]) -> Option<String> {
        fx.iter().find_map(|e| match e {
            Effect::Prompt { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
    }

    fn persisted(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Begin(_) | Effect::Append { .. }))
    }

    fn states(fx: &[Effect]) -> Vec<&'static str> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::State { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn track_records_from_the_first_line() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).expect("prompted");
        assert!(!persisted(&fx), "nothing is written before approval");
        assert_eq!(book.active().unwrap().started_at, T0);
        let mut fx = Vec::new();
        assert_eq!(book.track(&id, now + 1000, &mut fx), Ok("tracking"));
        let Some(Effect::Begin(call)) = fx.iter().find(|e| matches!(e, Effect::Begin(_))) else { panic!("no begin") };
        assert_eq!(call.lines.len(), 6);
        assert_eq!(call.lines[0].speaker.label(), "You");
        assert_eq!(call.lines[1].speaker.label(), "Voice 1");
        assert_eq!(states(&fx), ["tracking"]);
        let more = book.observe("mac", &signal(now + 9000, "meeting"), heard(now + 9000, 7), now + 9000);
        assert!(matches!(more.as_slice(), [Effect::Append { .. }]));
        let mut again = Vec::new();
        assert_eq!(book.track(&id, now + 9500, &mut again), Err((StatusCode::CONFLICT, "tracking")));
        assert_eq!(book.ignore(&id, now + 9500, &mut again), Err((StatusCode::CONFLICT, "tracking")));
    }

    #[test]
    fn ignore_scrubs_and_never_reprompts_the_same_call() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        let more = book.observe("mac", &signal(now + 5000, "meeting"), heard(now + 5000, 9), now + 5000);
        assert!(!persisted(&more));
        let mut fx = Vec::new();
        assert_eq!(book.ignore(&id, now + 6000, &mut fx), Ok("declined"));
        let Some(Effect::Scrub { ids, .. }) = fx.iter().find(|e| matches!(e, Effect::Scrub { .. })) else { panic!("no scrub") };
        assert_eq!(ids.len(), 7);
        assert!(!persisted(&fx));
        assert_eq!(states(&fx), ["declined"]);
        assert!(book.active().is_none());
        let (fx, now) = converse(&mut book, now + 10_000, 30);
        assert!(prompted(&fx).is_none(), "the same conversation must not prompt again");
        assert!(!persisted(&fx));
        let mut late = Vec::new();
        assert_eq!(book.track(&id, now, &mut late), Err((StatusCode::CONFLICT, "declined")));
        let (fx, _) = converse(&mut book, now + 200_000, 6);
        assert!(prompted(&fx).is_some(), "a new call after a quiet spell prompts again");
    }

    #[test]
    fn unanswered_prompt_expires_after_120_seconds() {
        let mut book = CallBook::default();
        let (fx, _) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        let call = book.active().unwrap();
        let expires = call.expires_at;
        assert_eq!(expires - call.detected_at, PROMPT_MS);
        assert!(book.tick(expires - 1).is_empty());
        let fx = book.tick(expires);
        assert_eq!(states(&fx), ["expired"]);
        assert!(fx.iter().any(|e| matches!(e, Effect::Scrub { ids, .. } if ids.len() == 6)));
        assert!(!persisted(&fx));
        let mut late = Vec::new();
        assert_eq!(book.track(&id, expires + 1, &mut late), Err((StatusCode::CONFLICT, "expired")));
        assert_eq!(book.track("nope", expires + 1, &mut late), Err((StatusCode::NOT_FOUND, "unknown")));
    }

    #[test]
    fn answering_after_expiry_reports_expired() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        let mut fx = Vec::new();
        assert_eq!(book.track(&id, now + PROMPT_MS + 1, &mut fx), Err((StatusCode::CONFLICT, "expired")));
        assert_eq!(states(&fx), ["expired"]);
    }

    #[test]
    fn tracking_ends_on_stop_or_after_quiet() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        book.track(&id, now, &mut Vec::new()).unwrap();
        let mut fx = Vec::new();
        assert_eq!(book.stop(&id, now + 1000, &mut fx), Ok("ended"));
        assert_eq!(states(&fx), ["ended"]);
        assert!(fx.iter().any(|e| matches!(e, Effect::Summarise { call, .. } if call.lines.len() == 6)));
        assert_eq!(book.stop(&id, now + 2000, &mut Vec::new()), Err((StatusCode::CONFLICT, "ended")));
        book.summarised(&id);
        assert_eq!(book.ignore(&id, now + 2000, &mut Vec::new()), Err((StatusCode::CONFLICT, "summarised")));

        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        book.track(&id, now, &mut Vec::new()).unwrap();
        assert_eq!(book.stop("other", now, &mut Vec::new()), Err((StatusCode::NOT_FOUND, "unknown")));
        assert!(book.tick(now + END_QUIET_MS - 1).is_empty());
        let fx = book.tick(now + END_QUIET_MS);
        assert_eq!(states(&fx), ["ended"]);
        assert!(fx.iter().any(|e| matches!(e, Effect::Summarise { .. })));
    }

    #[test]
    fn stop_on_a_pending_call_is_a_conflict() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        assert_eq!(book.stop(&id, now, &mut Vec::new()), Err((StatusCode::CONFLICT, "pending")));
    }

    #[test]
    fn played_audio_never_prompts() {
        let mut book = CallBook::default();
        for n in 0..40 {
            let at = T0 + n * 6000;
            let tv = Signal { kind: "media".into(), ..signal(at, "meeting") };
            assert!(book.observe("mac", &tv, heard(at, n as usize), at).is_empty());
        }
        assert!(book.active().is_none());
    }

    #[test]
    fn other_device_and_assistant_lines_stay_out_of_the_call() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        prompted(&fx).unwrap();
        assert!(book.holds("mac") && !book.holds("phone"));
        let to_agent = Signal { delivered: true, ..signal(now + 1000, "for_assistant") };
        book.observe("mac", &to_agent, heard(now + 1000, 50), now + 1000);
        book.observe("phone", &signal(now + 1000, "meeting"), heard(now + 1000, 51), now + 1000);
        assert_eq!(book.active().unwrap().lines.len(), 6);
    }

    #[test]
    fn events_match_the_app_contract() {
        let p = prompt_event("c1", 42);
        assert_eq!(p["payload"], json!({"type":"call_prompt","call_id":"c1","expires_at":42,"title":TITLE,"body":BODY}));
        assert_eq!(state_event("c1", "declined")["payload"], json!({"type":"call_state","call_id":"c1","state":"declined"}));
        assert_eq!(status_json(None), json!({"state":"idle"}));
    }

    #[test]
    fn scrub_removes_text_and_search_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("calls.db")).unwrap();
        let found = |store: &crate::store::Store| -> i64 {
            store
                .with_connection(|c| {
                    c.query_row(
                        "SELECT COUNT(*) FROM ambient_events_fts WHERE ambient_events_fts MATCH 'quarterly'",
                        [],
                        |r| r.get(0),
                    )
                })
                .unwrap()
        };
        store
            .with_connection(|c| {
                c.execute(
                    "INSERT INTO ambient_events (id, device, started_at, status, text, created_at, kept)
                     VALUES ('e1', 'mac', '2026-10-29T10:00:00Z', 'done', 'the quarterly numbers are late', 0, 1)",
                    [],
                )?;
                c.execute("INSERT INTO ambient_events_fts(rowid, text) SELECT rowid, text FROM ambient_events WHERE id = 'e1'", [])
            })
            .unwrap();
        assert_eq!(found(&store), 1);
        store.with_connection(|c| scrub_row(c, "e1")).unwrap();
        assert_eq!(found(&store), 0);
        let (text, kept): (String, i64) = store
            .with_connection(|c| c.query_row("SELECT text, kept FROM ambient_events WHERE id = 'e1'", [], |r| Ok((r.get(0)?, r.get(1)?))))
            .unwrap();
        assert_eq!((text.as_str(), kept), ("", 0));
        store
            .with_connection(|c| c.execute("INSERT INTO ambient_events_fts(ambient_events_fts) VALUES ('integrity-check')", []))
            .unwrap();
    }

    #[test]
    fn transcript_labels_speakers() {
        let lines = vec![heard(T0, 0), heard(T0 + 1000, 1), Heard { speaker: Speaker::Unknown, ..heard(T0 + 2000, 3) }];
        let t = transcript(&lines);
        assert!(t.contains("You: line 0") && t.contains("Voice 1: line 1") && t.contains("unknown: line 3"), "{t}");
    }
}
