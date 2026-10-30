use super::*;

use std::collections::VecDeque;

use serde_json::{Value, json};

use super::ambient_router::Part;
use super::ambient_watch::Verdict;
use super::call_detect::{Class, Detector, Signal, WINDOW_MS};
use crate::recording_policy::{Decision, REFUSAL_COOLDOWN_MS, RecordingPolicy};

pub(crate) const PROMPT_MS: i64 = 120_000;
pub(crate) const END_QUIET_MS: i64 = 120_000;
const MEMORABLE: f64 = 0.65;
const CLEAR_TO_ASSISTANT: f64 = 0.85;
const RING: usize = 40;
const MAX_LINES: usize = 4000;
const RESOLVED: usize = 32;
const TRANSCRIPT_CHARS: usize = 60_000;
const MAX_PENDING_MS: i64 = 1_200_000;
const REPROMPT_MS: i64 = 180_000;
const HEAD_LINES: usize = 12;
pub(crate) const TITLE: &str = "Take notes on this conversation?";
pub(crate) const BODY: &str = "Sounds like you're talking with someone. Snippet can keep notes.";

#[derive(Clone, Debug)]
pub(crate) struct Heard {
    pub at_ms: i64,
    pub ids: Vec<String>,
    pub text: String,
    pub memorable: bool,
    pub speaker: Option<String>,
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
    pub last_prompt: i64,
    pub lines: Vec<Heard>,
}

#[derive(Debug)]
pub(crate) enum Effect {
    Prompt { call_id: String, expires_at: i64 },
    State { call_id: String, state: &'static str },
    Begin(Call),
    Auto { call_id: String, label: String },
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
    policy: RecordingPolicy,
    refused_at: Option<i64>,
}

impl CallBook {
    pub fn set_policy(&mut self, policy: RecordingPolicy) {
        self.policy = policy;
    }

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
                if call.phase == Phase::Pending {
                    let cap = call.detected_at + MAX_PENDING_MS;
                    call.expires_at = call.expires_at.max(now + PROMPT_MS).min(cap);
                    if now - call.last_prompt >= REPROMPT_MS {
                        call.expires_at = call.expires_at.max((now + REPROMPT_MS + 30_000).min(cap));
                        call.last_prompt = now;
                        fx.push(Effect::Prompt { call_id: call.id.clone(), expires_at: call.expires_at });
                    }
                }
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
            let head = lines.iter().take(HEAD_LINES).map(|h| h.text.as_str()).collect::<Vec<_>>().join(" ");
            let cooled = self.refused_at.is_some_and(|t| now - t < REFUSAL_COOLDOWN_MS);
            let decision = self.policy.decide(now, crate::recording_policy::local_at(now), &head, cooled);
            if decision == Decision::Drop {
                fx.push(Effect::Scrub { device: device.to_string(), ids: lines.iter().flat_map(|l| l.ids.iter().cloned()).collect() });
                return fx;
            }
            let mut call = Call {
                id: uuid::Uuid::new_v4().to_string(),
                device: device.to_string(),
                phase: Phase::Pending,
                detected_at: now,
                expires_at: now + PROMPT_MS,
                started_at: first,
                last_call_like: now,
                last_prompt: now,
                lines,
            };
            fx.push(Effect::Quiet(true));
            if let Decision::Auto { label } = decision {
                call.phase = Phase::Tracking;
                fx.push(Effect::Begin(call.clone()));
                fx.push(Effect::Auto { call_id: call.id.clone(), label });
            } else {
                fx.push(Effect::Prompt { call_id: call.id.clone(), expires_at: call.expires_at });
            }
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
                self.refused_at = Some(now);
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
                self.refused_at = Some(now);
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

/// Human conversation that is not for the assistant: a meeting, other-party talk or out-of-context speech.
fn heard_by_others(v: &Verdict, delivered: bool, wake: bool) -> bool {
    if delivered || wake || matches!(v.kind.as_str(), "media" | "background" | "noise") || v.played >= 0.8 || v.echo >= 0.7 {
        return false;
    }
    v.intent == "meeting" || v.other >= 0.5 || (v.kind == "conversation" && v.out_of_context >= 0.5)
}

/// The next room-busy timestamp (seconds): refreshed by others' talk, cleared by a direct address.
fn room_after(last: i64, v: &Verdict, delivered: bool, wake: bool, now_s: i64) -> i64 {
    if wake || delivered || clearly_for_assistant(v, wake) {
        0
    } else if heard_by_others(v, delivered, wake) {
        now_s
    } else {
        last
    }
}

pub(super) async fn observe(d: &Shared, device: &str, parts: &[Part], v: &Verdict, delivered: bool, wake: bool) {
    let Some(first) = parts.first() else { return };
    crate::ambient::set_last_others(room_after(crate::ambient::last_others(), v, delivered, wake, chrono::Utc::now().timestamp()));
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
    let label = speaker_label(speaker);
    let signal = Signal { speaker: Some(speaker), ..signal };
    let ids: Vec<String> = parts.iter().map(|p| p.id.clone()).collect();
    let heard = Heard { at_ms: signal.at_ms, ids, text, memorable: !delivered && v.kept() && v.memorable >= MEMORABLE, speaker: label };
    let policy = d.config.lock().unwrap().recording.clone();
    let fx = {
        let mut book = book();
        book.set_policy(policy);
        book.observe(device, &signal, heard, now_ms())
    };
    if !fx.is_empty() {
        ensure_ticker(d);
    }
    apply(d, fx).await;
}

fn speaker_label(s: super::speakers::Speaker) -> Option<String> {
    use super::speakers::Speaker;
    match s {
        Speaker::You => Some("You".into()),
        Speaker::Voice(n) => Some(format!("Speaker {n}")),
        Speaker::Unknown => None,
    }
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
            Effect::Auto { call_id, label } => {
                eprintln!("[call] {} tracking automatically under {label}", short(&call_id));
                crate::session::emit_device_event(auto_event(&call_id, &label));
                super::agent_routes::record_agent_message(d, crate::mission_control::SESSION_ID, "voice classifier", &auto_note(&label), false).await;
            }
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

pub(crate) fn auto_note(label: &str) -> String {
    format!("[recording] started automatically under {label}")
}

pub(crate) fn auto_event(call_id: &str, label: &str) -> Value {
    json!({
        "kind": "call_state",
        "source_key": format!("call_state:{call_id}:tracking"),
        "payload": {"type": "call_state", "call_id": call_id, "state": "tracking", "auto": true, "label": label},
    })
}

pub(crate) fn prompt_event(call_id: &str, expires_at: i64) -> Value {
    json!({
        "kind": "call_prompt",
        "source_key": format!("call_prompt:{call_id}"),
        "ttl_secs": ((expires_at - now_ms()) / 1000).clamp(1, MAX_PENDING_MS / 1000),
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
            rusqlite::params![call_id, line.at_ms, line.speaker.as_deref().unwrap_or(""), line.text, ids],
        )?;
        Ok(())
    });
}

fn set_row_state(d: &Shared, call_id: &str, state: &str) {
    set_state(&d.store, call_id, state);
}

fn set_state(store: &crate::store::Store, call_id: &str, state: &str) {
    store.write(|c| {
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

pub(crate) fn scrub_row(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO ambient_events_fts(ambient_events_fts, rowid, text)
         SELECT 'delete', rowid, text FROM ambient_events WHERE id = ?1 AND kept = 1 AND text != ''",
        [id],
    )?;
    c.execute("UPDATE ambient_events SET text = '', kept = 0 WHERE id = ?1", [id])?;
    Ok(())
}

pub(crate) fn transcript(lines: &[Heard]) -> String {
    transcript_to(lines, Some(TRANSCRIPT_CHARS))
}

fn transcript_to(lines: &[Heard], cap: Option<usize>) -> String {
    let mut out = String::new();
    for line in lines {
        let at = chrono::DateTime::from_timestamp_millis(line.at_ms).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
        let who = line.speaker.as_deref().map(|s| format!("{s}: ")).unwrap_or_default();
        let row = format!("[{at}] {who}{}\n", line.text.trim());
        if cap.is_some_and(|cap| out.len() + row.len() > cap) {
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
    deliver_ended(d, &call.id, &call.device, call.started_at, ended_at, &call.lines).await;
    book().summarised(&call.id);
}

/// Saves the recording, hands it to Mission Control and marks the call summarised.
async fn deliver_ended(d: &Shared, id: &str, device: &str, started_at: i64, ended_at: i64, lines: &[Heard]) {
    let saved = crate::recordings::write(&crate::recordings::dir(), id, device, started_at, ended_at, lines.len(), &transcript_to(lines, None));
    let path = match saved {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[call] {} recording not saved: {e}", short(id));
            None
        }
    };
    let msg = ended_message(id, device, started_at, ended_at, lines, path.as_deref());
    super::autonomy::deliver_to_mission_control(d, msg).await;
    set_row_state(d, id, "summarised");
    crate::session::emit_device_event(state_event(id, "summarised"));
}

fn ended_message(id: &str, device: &str, started_at: i64, ended_at: i64, lines: &[Heard], path: Option<&std::path::Path>) -> String {
    let iso = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()).unwrap_or_default();
    let saved = match path {
        Some(p) => format!(" The full recording is saved at {}; when another agent needs this conversation, pass it that path (workers can read files), not the transcript.", p.display()),
        None => " The recording could not be saved to a file.".into(),
    };
    let attr = path.map(|p| format!(" path=\"{}\"", super::ambient_watch::escape(&p.to_string_lossy()))).unwrap_or_default();
    format!(
        "<call_ended id=\"{}\" device=\"{}\" started=\"{}\" ended=\"{}\" lines=\"{}\"{attr}>The user asked Snippet to take notes on this conversation and it has ended.{saved} Speaker labels, where present, are best-effort voice matching (You is the user); otherwise infer who said what only where the content makes it clear, and never invent names. Reply in this chat in text only, without speaking: a short summary, the decisions made, and the action items with owners and dates where they were stated. Keep commitments the user made the way you keep other remembered notes. If nothing of substance was said, say so in one line.\n<transcript>\n{}</transcript></call_ended>",
        super::ambient_watch::escape(id),
        super::ambient_watch::escape(device),
        iso(started_at),
        iso(ended_at),
        lines.len(),
        super::ambient_watch::escape(&transcript(lines)),
    )
}

struct Unsummarised {
    id: String,
    device: String,
    started_at: i64,
    ended_at: i64,
    lines: Vec<Heard>,
}

/// Calls the user opted into that never reached a summary (the daemon stopped first).
fn unsummarised(store: &crate::store::Store) -> Vec<Unsummarised> {
    store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT id, device, started_at, ended_at FROM ambient_calls WHERE state IN ('tracking', 'ended') ORDER BY started_at")?;
            let calls = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::new();
            for (id, device, started_at, ended_at) in calls {
                let mut stmt = c.prepare("SELECT at_ms, text, event_ids, speaker FROM ambient_call_lines WHERE call_id = ?1 AND text != '' ORDER BY at_ms, rowid")?;
                let lines = stmt
                    .query_map([&id], |r| {
                        let ids: String = r.get(2)?;
                        Ok(Heard { at_ms: r.get(0)?, ids: serde_json::from_str(&ids).unwrap_or_default(), text: r.get(1)?, memorable: false, speaker: Some(r.get::<_, String>(3)?).filter(|s| !s.is_empty()) })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let ended_at = ended_at.or_else(|| lines.last().map(|l| l.at_ms)).unwrap_or(started_at);
                out.push(Unsummarised { id, device, started_at, ended_at, lines });
            }
            Ok(out)
        })
        .unwrap_or_default()
}

/// Removes a recording: the file, its lines, its call row and any event text behind them. Redacting blanks the text and keeps the metadata.
pub(crate) fn delete_recording(store: &crate::store::Store, dir: &std::path::Path, id: &str, redact: bool) -> Result<Value, String> {
    if book().active().is_some_and(|c| c.id == id) {
        return Err("that conversation is still being recorded; stop it first".into());
    }
    let file = crate::recordings::find(dir, id);
    let (known, event_ids) = store
        .with_connection(|c| {
            let known: i64 = c.query_row("SELECT (SELECT COUNT(*) FROM ambient_calls WHERE id = ?1) + (SELECT COUNT(*) FROM ambient_call_lines WHERE call_id = ?1)", [id], |r| r.get(0))?;
            let mut stmt = c.prepare("SELECT event_ids FROM ambient_call_lines WHERE call_id = ?1")?;
            let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            let ids: Vec<String> = rows.iter().flat_map(|j| serde_json::from_str::<Vec<String>>(j).unwrap_or_default()).collect();
            Ok((known, ids))
        })
        .map_err(|e| e.to_string())?;
    if known == 0 && file.is_none() {
        return Err("no recording with that id".into());
    }
    store
        .with_connection(|c| {
            for e in &event_ids {
                scrub_row(c, e)?;
            }
            if redact {
                c.execute("UPDATE ambient_call_lines SET text = '' WHERE call_id = ?1", [id])?;
            } else {
                c.execute("DELETE FROM ambient_call_lines WHERE call_id = ?1", [id])?;
                c.execute("DELETE FROM ambient_calls WHERE id = ?1", [id])?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    if let Some(path) = &file {
        if redact { crate::recordings::redact(path) } else { std::fs::remove_file(path) }.map_err(|e| e.to_string())?;
    }
    Ok(json!({"ok": true, "mode": if redact { "redact" } else { "delete" }, "file": file.is_some(), "lines": event_ids.len()}))
}

/// Deletes recordings (files, rows, lines) that started before the cutoff; returns how many calls and files went.
pub(crate) fn sweep(store: &crate::store::Store, dir: &std::path::Path, cutoff_ms: i64) -> (usize, usize) {
    let active = book().active().map(|c| c.id.clone()).unwrap_or_default();
    let old: Vec<String> = store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT id FROM ambient_calls WHERE started_at < ?1 AND id != ?2")?;
            let ids = stmt.query_map(rusqlite::params![cutoff_ms, active], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .unwrap_or_default();
    store.write(|c| {
        for id in &old {
            c.execute("DELETE FROM ambient_call_lines WHERE call_id = ?1", [id])?;
            c.execute("DELETE FROM ambient_calls WHERE id = ?1", [id])?;
        }
        Ok(())
    });
    let files: Vec<_> = crate::recordings::older_than(dir, cutoff_ms);
    let removed = files.iter().filter(|f| std::fs::remove_file(f).is_ok()).count();
    (old.len(), removed)
}

/// Hourly: apply the retention setting.
pub(crate) fn sweep_loop(d: Shared) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let days = d.config.lock().unwrap().recording.retention_days;
            if days > 0 {
                let cutoff = now_ms() - days as i64 * 86_400_000;
                let (calls, files) = sweep(&d.store, &crate::recordings::dir(), cutoff);
                if calls + files > 0 {
                    eprintln!("[call] retention removed {calls} call(s) and {files} recording file(s) older than {days} days");
                }
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

/// On startup: summarise tracked calls the previous run never finished.
pub(crate) fn recover(d: Shared) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        for call in unsummarised(&d.store) {
            eprintln!("[call] {} recovering after restart", short(&call.id));
            if call.lines.is_empty() {
                set_row_state(&d, &call.id, "summarised");
                continue;
            }
            deliver_ended(&d, &call.id, &call.device, call.started_at, call.ended_at, &call.lines).await;
        }
    });
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
        Heard { at_ms, ids: vec![format!("e{n}")], text: format!("line {n}"), memorable: false, speaker: None }
    }

    /// Feeds a back-and-forth until it prompts; returns the effects and the clock.
    fn converse(book: &mut CallBook, from: i64, lines: usize) -> (Vec<Effect>, i64) {
        converse_on(book, "mac", from, lines)
    }

    fn converse_on(book: &mut CallBook, device: &str, from: i64, lines: usize) -> (Vec<Effect>, i64) {
        let mut fx = Vec::new();
        let mut now = from;
        for n in 0..lines {
            now = from + n as i64 * 8_000;
            fx.extend(book.observe(device, &signal(now, if n % 2 == 0 { "meeting" } else { "note" }), heard(now, n), now));
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
        assert!(expires - call.detected_at >= PROMPT_MS, "continuing speech only extends");
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
    fn transcript_has_no_speaker_labels() {
        let lines = vec![heard(T0, 0), heard(T0 + 1000, 1), heard(T0 + 2000, 3)];
        let t = transcript(&lines);
        assert!(t.contains("] line 0") && t.contains("] line 1") && !t.contains("You") && !t.contains("Voice"), "{t}");
    }

    fn verdict(intent: &str, addressed: f64, other: f64) -> Verdict {
        Verdict {
            kind: "conversation".into(),
            intent: intent.into(),
            addressed,
            matches: 0.0,
            suggest: 0.0,
            memorable: 0.0,
            echo: 0.0,
            other,
            played: 0.0,
            out_of_context: 0.0,
            judge: "test",
            voice: None,
        }
    }

    #[test]
    fn asking_snippet_mid_call_still_gets_through() {
        assert!(clearly_for_assistant(&verdict("for_assistant", 0.2, 0.9), true));
        assert!(clearly_for_assistant(&verdict("for_assistant", 0.9, 0.1), false));
        assert!(!clearly_for_assistant(&verdict("for_assistant", 0.7, 0.1), false));
        assert!(!clearly_for_assistant(&verdict("meeting", 0.9, 0.8), false));
    }

    fn notes(fx: &[Effect]) -> Vec<String> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Note(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn held_notes_wait_out_the_window_and_die_with_a_declined_call() {
        let mut book = CallBook::default();
        let solo = Heard { memorable: true, ..heard(T0, 0) };
        let fx = book.observe("mac", &signal(T0, "note"), solo, T0);
        assert!(notes(&fx).is_empty());
        assert!(notes(&book.tick(T0 + 30_000)).is_empty());
        assert_eq!(notes(&book.tick(T0 + WINDOW_MS)), ["line 0"]);

        let start = T0 + 500_000;
        let mut fx = Vec::new();
        for n in 0..6 {
            let at = start + n as i64 * 8_000;
            let h = Heard { memorable: true, ..heard(at, n) };
            fx.extend(book.observe("mac", &signal(at, if n % 2 == 0 { "meeting" } else { "note" }), h, at));
        }
        let id = prompted(&fx).unwrap();
        let mut fx = Vec::new();
        book.ignore(&id, start + 50_000, &mut fx).unwrap();
        fx.extend(book.tick(start + 400_000));
        assert!(notes(&fx).is_empty(), "nothing said on a declined call becomes a memory");
    }

    fn call_store() -> (tempfile::TempDir, crate::store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("calls.db")).unwrap();
        (dir, store)
    }

    fn seed(store: &crate::store::Store, id: &str, state: &str, lines: &[&str]) {
        store.write(|c| {
            c.execute("INSERT INTO ambient_calls (id, device, detected_at, started_at, state) VALUES (?1, 'mac', 1, ?2, ?3)", rusqlite::params![id, T0, state])?;
            for (n, text) in lines.iter().enumerate() {
                c.execute(
                    "INSERT INTO ambient_call_lines (call_id, at_ms, speaker, text, event_ids) VALUES (?1, ?2, '', ?3, '[\"e\"]')",
                    rusqlite::params![id, T0 + n as i64 * 1000, text],
                )?;
            }
            Ok(())
        });
    }

    #[test]
    fn recovery_rebuilds_tracking_calls_and_skips_pending_and_declined() {
        let (_dir, store) = call_store();
        seed(&store, "t1", "tracking", &["first", "", "second"]);
        seed(&store, "e1", "ended", &["only"]);
        seed(&store, "p1", "pending", &["secret"]);
        seed(&store, "d1", "declined", &["secret"]);
        seed(&store, "s1", "summarised", &["done"]);
        let found = unsummarised(&store);
        let ids: Vec<&str> = found.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["t1", "e1"]);
        let t = &found[0];
        assert_eq!(t.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(t.ended_at, T0 + 2000);
        set_state(&store, "t1", "summarised");
        set_state(&store, "e1", "summarised");
        assert!(unsummarised(&store).is_empty());
    }

    #[test]
    fn ended_message_carries_the_path_and_the_saved_file_the_whole_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<Heard> = (0..6000).map(|n| Heard { text: format!("line number {n} with some padding words"), ..heard(T0 + n as i64 * 1000, n) }).collect();
        let full = transcript_to(&lines, None);
        assert!(transcript(&lines).contains("too long") && !full.contains("too long"));
        let path = crate::recordings::write(tmp.path(), "abcdef123456", "mac", T0, T0 + 9000, lines.len(), &full).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("- lines: 6000") && body.contains("line number 5999 "));
        let msg = ended_message("abcdef123456", "mac", T0, T0 + 9000, &lines, Some(&path));
        assert!(msg.contains(&format!("path=\"{}\"", path.display())) && msg.contains("pass it that path"));
        assert!(msg.contains("too long"));
    }

    #[test]
    fn out_of_context_talk_flips_the_room_flag_and_a_direct_address_clears_it() {
        let mut v = verdict("note", 0.1, 0.0);
        assert_eq!(room_after(0, &v, false, false, 500), 0);
        v.out_of_context = 0.6;
        assert_eq!(room_after(0, &v, false, false, 500), 500);
        assert_eq!(room_after(0, &verdict("meeting", 0.1, 0.0), false, false, 600), 600);
        assert_eq!(room_after(0, &verdict("note", 0.1, 0.7), false, false, 700), 700);
        assert_eq!(room_after(500, &v, false, true, 510), 0);
        assert_eq!(room_after(500, &verdict("for_assistant", 0.9, 0.1), true, false, 510), 0);
        let media = Verdict { kind: "media".into(), ..verdict("meeting", 0.0, 0.0) };
        assert_eq!(room_after(0, &media, false, false, 500), 0);
    }

    fn auto_policy() -> RecordingPolicy {
        let mut p = RecordingPolicy::default();
        let when = crate::recording_policy::When { keywords: vec!["line".into()], ..Default::default() };
        p.set_rule("Standup", "auto", when, "user: always record standup", T0).unwrap();
        p
    }

    fn started_auto(fx: &[Effect]) -> Option<String> {
        fx.iter().find_map(|e| match e {
            Effect::Auto { label, .. } => Some(label.clone()),
            _ => None,
        })
    }

    #[test]
    fn auto_rule_starts_tracking_without_a_prompt_and_notifies() {
        let mut book = CallBook::default();
        book.set_policy(auto_policy());
        let (fx, now) = converse(&mut book, T0, 6);
        assert_eq!(started_auto(&fx).as_deref(), Some("Standup"));
        assert!(prompted(&fx).is_none());
        assert!(fx.iter().any(|e| matches!(e, Effect::Begin(c) if c.phase == Phase::Tracking)));
        assert!(fx.iter().any(|e| matches!(e, Effect::Quiet(true))));
        assert!(!fx.iter().any(|e| matches!(e, Effect::State { .. })));
        assert_eq!(book.active().unwrap().phase, Phase::Tracking);
        let id = book.active().unwrap().id.clone();
        let ev = auto_event(&id, "Standup");
        assert_eq!(ev["payload"], json!({"type":"call_state","call_id":id,"state":"tracking","auto":true,"label":"Standup"}));
        assert_eq!(auto_note("Standup"), "[recording] started automatically under Standup");
        assert!(book.stop(&id, now + 1000, &mut Vec::new()).is_ok());
    }

    #[test]
    fn never_and_snooze_drop_silently_and_beat_auto() {
        for tweak in [0, 1] {
            let mut p = auto_policy();
            if tweak == 0 {
                let when = crate::recording_policy::When { keywords: vec!["line".into()], ..Default::default() };
                p.set_rule("Private", "never", when, "user: never record this", T0).unwrap();
            } else {
                p.snooze_until_ms = Some(T0 + 3_600_000);
            }
            let mut book = CallBook::default();
            book.set_policy(p);
            let (fx, _) = converse(&mut book, T0, 6);
            assert!(book.active().is_none());
            assert!(prompted(&fx).is_none() && started_auto(&fx).is_none() && !persisted(&fx));
            assert!(!fx.iter().any(|e| matches!(e, Effect::Quiet(_) | Effect::State { .. })));
            assert!(fx.iter().any(|e| matches!(e, Effect::Scrub { ids, .. } if ids.len() >= 4)), "held lines are discarded");
        }
    }

    #[test]
    fn a_decline_or_stop_blocks_auto_for_ten_minutes() {
        let mut book = CallBook::default();
        let (fx, now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        book.ignore(&id, now + 1000, &mut Vec::new()).unwrap();
        book.set_policy(auto_policy());
        let (fx, _) = converse_on(&mut book, "phone", now + 60_000, 6);
        assert!(started_auto(&fx).is_none() && prompted(&fx).is_some(), "declined: ask, never auto");
        let id = book.active().unwrap().id.clone();
        book.ignore(&id, now + 120_000, &mut Vec::new()).unwrap();
        let later = now + 1000 + REFUSAL_COOLDOWN_MS + 1_000_000;
        let (fx, _) = converse_on(&mut book, "tablet", later, 6);
        assert_eq!(started_auto(&fx).as_deref(), Some("Standup"));
    }

    #[test]
    fn pending_prompt_extends_to_the_cap_and_refreshes_every_three_minutes() {
        let mut book = CallBook::default();
        let (fx, mut now) = converse(&mut book, T0, 6);
        let id = prompted(&fx).unwrap();
        let detected = book.active().unwrap().detected_at;
        let mut prompts = Vec::new();
        let expired_at = loop {
            now += 8_000;
            let n = ((now - T0) / 8_000) as usize;
            let fx = book.observe("mac", &signal(now, if n % 2 == 0 { "meeting" } else { "note" }), heard(now, n), now);
            if states(&fx).contains(&"expired") {
                break now;
            }
            for e in &fx {
                if let Effect::Prompt { call_id, expires_at } = e {
                    assert_eq!(call_id, &id);
                    prompts.push((now, *expires_at));
                }
            }
            assert!(book.active().unwrap().expires_at <= detected + MAX_PENDING_MS);
        };
        assert!(expired_at >= detected + MAX_PENDING_MS, "never lapses before the cap while speech continues");
        assert!(prompts.len() >= 5 && prompts.windows(2).all(|w| w[1].0 - w[0].0 >= REPROMPT_MS && w[1].0 - w[0].0 < REPROMPT_MS + 10_000), "{prompts:?}");
        assert!(prompts.iter().all(|(_, e)| *e <= detected + MAX_PENDING_MS));
    }

    #[test]
    fn speaker_labels_render_in_transcripts_and_survive_recovery() {
        let lines = vec![
            Heard { speaker: Some("You".into()), ..heard(T0, 0) },
            Heard { speaker: Some("Speaker 1".into()), ..heard(T0 + 1000, 1) },
            heard(T0 + 2000, 2),
        ];
        let t = transcript(&lines);
        assert!(t.contains("] You: line 0") && t.contains("] Speaker 1: line 1") && t.contains("] line 2"), "{t}");
        let (_dir, store) = call_store();
        store.write(|c| {
            c.execute("INSERT INTO ambient_calls (id, device, detected_at, started_at, state) VALUES ('c', 'mac', 1, ?1, 'tracking')", [T0])?;
            for (n, who) in ["You", "", "Speaker 2"].iter().enumerate() {
                c.execute(
                    "INSERT INTO ambient_call_lines (call_id, at_ms, speaker, text, event_ids) VALUES ('c', ?1, ?2, 'x', '[]')",
                    rusqlite::params![T0 + n as i64, who],
                )?;
            }
            Ok(())
        });
        let got: Vec<Option<String>> = unsummarised(&store)[0].lines.iter().map(|l| l.speaker.clone()).collect();
        assert_eq!(got, [Some("You".into()), None, Some("Speaker 2".into())]);
        assert_eq!(speaker_label(super::super::speakers::Speaker::Voice(3)).as_deref(), Some("Speaker 3"));
        assert_eq!(speaker_label(super::super::speakers::Speaker::Unknown), None);
    }

    #[test]
    fn retention_sweep_takes_only_old_calls_and_files() {
        let (dir, store) = call_store();
        let rec = dir.path().join("rec");
        let day = 86_400_000;
        seed(&store, "old", "summarised", &["a"]);
        store.write(|c| c.execute("UPDATE ambient_calls SET started_at = ?1 WHERE id = 'old'", [T0 - 70 * day]));
        seed(&store, "new", "summarised", &["b"]);
        crate::recordings::write(&rec, "old", "mac", T0 - 70 * day, T0 - 70 * day + 1, 1, "x\n").unwrap();
        crate::recordings::write(&rec, "new", "mac", T0, T0 + 1, 1, "y\n").unwrap();
        assert_eq!(sweep(&store, &rec, T0 - 60 * day), (1, 1));
        let left: i64 = store.with_connection(|c| c.query_row("SELECT COUNT(*) FROM ambient_calls", [], |r| r.get(0))).unwrap();
        assert_eq!(left, 1);
        assert_eq!(crate::recordings::list(&rec, 10).len(), 1);
        assert_eq!(sweep(&store, &rec, T0 - 60 * day), (0, 0));
    }

    #[test]
    fn delete_removes_everything_and_redact_keeps_the_header() {
        let (dir, store) = call_store();
        let rec = dir.path().join("rec");
        for id in ["gone", "blank"] {
            seed(&store, id, "summarised", &["secret words"]);
            crate::recordings::write(&rec, id, "mac", T0, T0 + 1, 1, "[10:00:00] secret words\n").unwrap();
        }
        store.write(|c| {
            c.execute("INSERT INTO ambient_events (id, device, started_at, status, text, created_at, kept) VALUES ('e', 'mac', '2026-10-29T10:00:00Z', 'done', 'secret words', 0, 1)", [])?;
            c.execute("INSERT INTO ambient_events_fts(rowid, text) SELECT rowid, text FROM ambient_events WHERE id = 'e'", [])?;
            c.execute("UPDATE ambient_call_lines SET event_ids = '[\"e\"]'", [])
        });
        let count = |sql: &str| -> i64 { store.with_connection(|c| c.query_row(sql, [], |r| r.get(0))).unwrap() };
        assert!(delete_recording(&store, &rec, "nope", false).is_err());
        delete_recording(&store, &rec, "gone", false).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM ambient_calls WHERE id = 'gone'") + count("SELECT COUNT(*) FROM ambient_call_lines WHERE call_id = 'gone'"), 0);
        assert!(crate::recordings::find(&rec, "gone").is_none());
        assert_eq!(count("SELECT COUNT(*) FROM ambient_events WHERE text = 'secret words'"), 0, "event text scrubbed");
        delete_recording(&store, &rec, "blank", true).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM ambient_calls WHERE id = 'blank'"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM ambient_call_lines WHERE call_id = 'blank' AND text = ''"), 1);
        let body = std::fs::read_to_string(crate::recordings::find(&rec, "blank").unwrap()).unwrap();
        assert!(body.contains("- id: blank") && body.contains("[redacted]") && !body.contains("secret"), "{body}");
        assert_eq!(crate::recordings::list(&rec, 10).len(), 1);
    }
}
