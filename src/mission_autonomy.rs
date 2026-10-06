use std::path::PathBuf;

use chrono::{Local, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::Store;

pub const MIN_ROUND_MINUTES: u32 = 10;
const MAX_REMEMBERED_QUESTIONS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutonomySettings {
    #[serde(default)]
    pub on: bool,
    #[serde(default = "default_round_minutes")]
    pub round_minutes: u32,
    #[serde(default)]
    pub quiet_start: Option<String>,
    #[serde(default)]
    pub quiet_end: Option<String>,
    #[serde(default)]
    pub utc_offset_minutes: Option<i32>,
}

fn default_round_minutes() -> u32 {
    30
}

impl Default for AutonomySettings {
    fn default() -> Self {
        Self {
            on: false,
            round_minutes: default_round_minutes(),
            quiet_start: Some("22:00".into()),
            quiet_end: Some("08:00".into()),
            utc_offset_minutes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Followup {
    pub id: String,
    pub due_at: i64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeldPing {
    pub title: String,
    pub message: String,
    pub kind: String,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AutonomyState {
    #[serde(default)]
    pub settings: AutonomySettings,
    #[serde(default)]
    pub last_round_at: i64,
    #[serde(default)]
    pub last_round_summary: Option<String>,
    #[serde(default)]
    pub last_digest: String,
    #[serde(default)]
    pub followups: Vec<Followup>,
    #[serde(default)]
    pub held_pings: Vec<HeldPing>,
    #[serde(default)]
    pub asked_questions: Vec<String>,
    #[serde(default)]
    pub pings_sent: u32,
    #[serde(default)]
    pub answers: Vec<QueuedAnswer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedAnswer {
    pub session_id: String,
    pub answer: String,
}

pub fn queue_answer(session_id: &str, answer: &str) -> Result<(), String> {
    update(|state| {
        state.answers.push(QueuedAnswer {
            session_id: session_id.to_string(),
            answer: answer.to_string(),
        })
    })
}

pub fn take_answers() -> Vec<QueuedAnswer> {
    if load().answers.is_empty() {
        return Vec::new();
    }
    update(|state| std::mem::take(&mut state.answers)).unwrap_or_default()
}

fn store() -> Result<Store, String> {
    Store::open_cached(crate::store::default_db_path()).map_err(|e| e.to_string())
}

pub fn load() -> AutonomyState {
    store()
        .ok()
        .and_then(|s| s.load_autonomy_json().ok().flatten())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(state: &AutonomyState) -> Result<(), String> {
    let raw = serde_json::to_string(state).map_err(|e| e.to_string())?;
    store()?.save_autonomy_json(&raw).map_err(|e| e.to_string())
}

pub fn update<T>(f: impl FnOnce(&mut AutonomyState) -> T) -> Result<T, String> {
    let mut state = load();
    let out = f(&mut state);
    save(&state)?;
    Ok(out)
}

pub fn is_on() -> bool {
    load().settings.on
}

pub fn brief_path() -> PathBuf {
    crate::config::snippet_home().join("mission-control").join("brief.md")
}

pub fn read_brief() -> String {
    std::fs::read_to_string(brief_path()).unwrap_or_default()
}

pub fn write_brief(content: &str) -> Result<(), String> {
    let path = brief_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, content).map_err(|e| e.to_string())
}

fn parse_clock(value: Option<&str>) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(value?.trim(), "%H:%M").ok()
}

pub fn valid_clock(value: &str) -> bool {
    parse_clock(Some(value)).is_some()
}

pub fn user_now(settings: &AutonomySettings) -> chrono::DateTime<chrono::FixedOffset> {
    let offset = settings
        .utc_offset_minutes
        .and_then(|m| chrono::FixedOffset::east_opt(m * 60))
        .unwrap_or_else(|| *Local::now().offset());
    chrono::Utc::now().with_timezone(&offset)
}

pub fn in_quiet_hours(settings: &AutonomySettings) -> bool {
    let (Some(start), Some(end)) = (
        parse_clock(settings.quiet_start.as_deref()),
        parse_clock(settings.quiet_end.as_deref()),
    ) else {
        return false;
    };
    let now = user_now(settings).time();
    let now = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).unwrap_or(now);
    if start == end {
        false
    } else if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn emit(title: &str, message: &str, kind: &str) {
    crate::session::emit_device_event(json!({
        "kind": "ping",
        "session": crate::mission_control::SESSION_ID,
        "title": title,
        "message": message,
        "ping_kind": kind,
        "urgent": kind == "urgent",
    }));
}

pub fn ping(title: &str, message: &str, kind: &str) -> Result<&'static str, String> {
    let now = chrono::Utc::now().timestamp();
    update(|state| {
        if kind != "urgent" && in_quiet_hours(&state.settings) {
            state.held_pings.push(HeldPing {
                title: title.to_string(),
                message: message.to_string(),
                kind: kind.to_string(),
                at: now,
            });
            "held"
        } else {
            state.pings_sent += 1;
            emit(title, message, kind);
            "sent"
        }
    })
}

pub fn release_held_pings() {
    let state = load();
    if state.held_pings.is_empty() || in_quiet_hours(&state.settings) {
        return;
    }
    let held = update(|state| std::mem::take(&mut state.held_pings)).unwrap_or_default();
    match held.len() {
        0 => {}
        1 => emit(&held[0].title, &held[0].message, &held[0].kind),
        n => {
            let lines: Vec<String> = held.iter().map(|p| format!("• {}", p.title)).collect();
            emit(&format!("{n} updates from overnight"), &lines.join("\n"), "update");
        }
    }
}

pub fn schedule_followup(due_at: i64, note: &str) -> Result<Followup, String> {
    let followup = Followup {
        id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
        due_at,
        note: note.to_string(),
    };
    let saved = followup.clone();
    update(move |state| {
        state.followups.push(followup);
        state.followups.sort_by_key(|f| f.due_at);
    })?;
    Ok(saved)
}

pub fn take_due_followups(now: i64) -> Vec<Followup> {
    let state = load();
    if !state.followups.iter().any(|f| f.due_at <= now) {
        return Vec::new();
    }
    update(|state| {
        let (due, later): (Vec<Followup>, Vec<Followup>) =
            std::mem::take(&mut state.followups).into_iter().partition(|f| f.due_at <= now);
        state.followups = later;
        due
    })
    .unwrap_or_default()
}

pub fn remember_question(key: &str) -> bool {
    update(|state| {
        if state.asked_questions.iter().any(|k| k == key) {
            return false;
        }
        state.asked_questions.push(key.to_string());
        if state.asked_questions.len() > MAX_REMEMBERED_QUESTIONS {
            let excess = state.asked_questions.len() - MAX_REMEMBERED_QUESTIONS;
            state.asked_questions.drain(..excess);
        }
        true
    })
    .unwrap_or(false)
}

pub fn mode_line() -> String {
    format!(
        "{} You have no shell (no bash): you coordinate through sessions, tasks and agents, and use read_file for a quick look at a specific file.",
        mode_status()
    )
}

fn mode_status() -> String {
    let state = load();
    let settings = &state.settings;
    if !settings.on {
        return "Autonomous mode is off. The user is driving: answer them in your replies, use ask_user for a question, and act on what they ask and on the reports that arrive.".to_string();
    }
    let quiet = match (&settings.quiet_start, &settings.quiet_end) {
        (Some(start), Some(end)) => format!(
            ", quiet hours {start}–{end} in the user's time (UTC{}){}",
            user_now(settings).format("%:z"),
            if in_quiet_hours(settings) { ", in effect" } else { "" }
        ),
        _ => String::new(),
    };
    format!(
        "Autonomous mode is on: rounds every {} minutes{quiet}. The user may be away and doesn't watch this chat: reach them with ping_user, never ask_user (it would block you and stop your rounds until they answer). When they message you directly they are here, so answer in your reply as usual.",
        settings.round_minutes
    )
}

pub fn next_round_at(state: &AutonomyState) -> Option<i64> {
    state
        .settings
        .on
        .then(|| state.last_round_at + i64::from(state.settings.round_minutes.max(MIN_ROUND_MINUTES)) * 60)
}
