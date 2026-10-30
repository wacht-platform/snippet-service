//! Standing rules for recording conversations. Mission Control edits them through tools; the call detector enforces them.

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

pub const MAX_WINDOW_MS: i64 = 12 * 3_600_000;
pub const MAX_SNOOZE_MINUTES: i64 = 720;
pub const MAX_RETENTION_DAYS: u32 = 3650;
pub const REFUSAL_COOLDOWN_MS: i64 = 600_000;

fn default_retention() -> u32 {
    60
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecordingPolicy {
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_until_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<Window>,
}

impl Default for RecordingPolicy {
    fn default() -> Self {
        RecordingPolicy { retention_days: default_retention(), snooze_until_ms: None, rules: Vec::new(), windows: Vec::new() }
    }
}

impl RecordingPolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct When {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub weekdays: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hhmm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_hhmm: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    pub label: String,
    pub mode: String,
    #[serde(default)]
    pub when: When,
    #[serde(default)]
    pub basis: String,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub id: String,
    pub label: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub mode: String,
    #[serde(default)]
    pub basis: String,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Drop,
    Auto { label: String },
    Ask,
}

fn minutes(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn local_at(ms: i64) -> NaiveDateTime {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.naive_local()).unwrap_or_default()
}

impl When {
    pub fn matches(&self, text_lower: &str, local: NaiveDateTime) -> bool {
        if !self.keywords.is_empty() && !self.keywords.iter().any(|k| text_lower.contains(&k.to_lowercase())) {
            return false;
        }
        if !self.weekdays.is_empty() && !self.weekdays.contains(&(local.weekday().num_days_from_sunday() as u8)) {
            return false;
        }
        if let (Some(from), Some(to)) = (self.from_hhmm.as_deref().and_then(minutes), self.to_hhmm.as_deref().and_then(minutes)) {
            let now = local.hour() * 60 + local.minute();
            let inside = if from <= to { now >= from && now < to } else { now >= from || now < to };
            if !inside {
                return false;
            }
        }
        true
    }
}

impl RecordingPolicy {
    pub fn prune(&mut self, now_ms: i64) -> bool {
        let before = self.windows.len();
        self.windows.retain(|w| w.to_ms > now_ms);
        if self.snooze_until_ms.is_some_and(|t| t <= now_ms) {
            self.snooze_until_ms = None;
            return true;
        }
        before != self.windows.len()
    }

    pub fn snoozed(&self, now_ms: i64) -> bool {
        self.snooze_until_ms.is_some_and(|t| t > now_ms)
    }

    /// Snooze or never beats auto beats ask. `cooled` suppresses auto after the user's own refusal.
    pub fn decide(&self, now_ms: i64, local: NaiveDateTime, heard: &str, cooled: bool) -> Decision {
        if self.snoozed(now_ms) {
            return Decision::Drop;
        }
        let text = heard.to_lowercase();
        let live = |w: &&Window| w.from_ms <= now_ms && now_ms < w.to_ms;
        let hit = |r: &&Rule| r.when.matches(&text, local);
        if self.windows.iter().filter(live).any(|w| w.mode == "never") || self.rules.iter().filter(hit).any(|r| r.mode == "never") {
            return Decision::Drop;
        }
        if cooled {
            return Decision::Ask;
        }
        if let Some(w) = self.windows.iter().filter(live).find(|w| w.mode == "auto") {
            return Decision::Auto { label: w.label.clone() };
        }
        match self.rules.iter().filter(hit).find(|r| r.mode == "auto") {
            Some(r) => Decision::Auto { label: r.label.clone() },
            None => Decision::Ask,
        }
    }

    pub fn add_window(&mut self, label: &str, from_ms: i64, to_ms: i64, mode: &str, basis: &str, now_ms: i64) -> Result<String, String> {
        mode_ok(mode)?;
        need_basis(basis)?;
        let label = need_label(label)?;
        if to_ms <= from_ms {
            return Err("the window must end after it starts".into());
        }
        if to_ms - from_ms > MAX_WINDOW_MS {
            return Err("a window can span at most 12 hours".into());
        }
        if to_ms <= now_ms {
            return Err("the window is already over".into());
        }
        self.prune(now_ms);
        let id = uuid::Uuid::new_v4().to_string();
        self.windows.push(Window { id: id.clone(), label, from_ms, to_ms, mode: mode.into(), basis: basis.trim().into(), created_at: now_ms });
        Ok(id)
    }

    pub fn snooze(&mut self, minutes: i64, now_ms: i64) -> Result<i64, String> {
        if !(1..=MAX_SNOOZE_MINUTES).contains(&minutes) {
            return Err("snooze must be between 1 minute and 12 hours".into());
        }
        let until = now_ms + minutes * 60_000;
        self.snooze_until_ms = Some(until);
        Ok(until)
    }

    pub fn set_rule(&mut self, label: &str, mode: &str, when: When, basis: &str, now_ms: i64) -> Result<String, String> {
        if !matches!(mode, "auto" | "ask" | "never") {
            return Err("mode must be auto, ask or never".into());
        }
        if mode != "ask" {
            need_basis(basis)?;
        }
        let label = need_label(label)?;
        let when = clean_when(when)?;
        self.rules.retain(|r| !r.label.eq_ignore_ascii_case(&label));
        let id = uuid::Uuid::new_v4().to_string();
        self.rules.push(Rule { id: id.clone(), label, mode: mode.into(), when, basis: basis.trim().into(), created_at: now_ms });
        Ok(id)
    }

    pub fn remove_rule(&mut self, id: &str) -> bool {
        let n = self.rules.len();
        self.rules.retain(|r| r.id != id);
        n != self.rules.len()
    }

    pub fn remove_window(&mut self, id: &str) -> bool {
        let n = self.windows.len();
        self.windows.retain(|w| w.id != id);
        n != self.windows.len()
    }
}

fn mode_ok(mode: &str) -> Result<(), String> {
    if matches!(mode, "auto" | "never") { Ok(()) } else { Err("mode must be auto or never".into()) }
}

fn need_basis(basis: &str) -> Result<(), String> {
    if basis.trim().is_empty() {
        Err("basis is required: quote what the user said or the standing approval this rests on".into())
    } else {
        Ok(())
    }
}

fn need_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() { Err("label is required".into()) } else { Ok(label.chars().take(80).collect()) }
}

fn clean_when(mut when: When) -> Result<When, String> {
    when.keywords = when.keywords.iter().map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).collect();
    if when.weekdays.iter().any(|d| *d > 6) {
        return Err("weekdays are 0 (Sunday) to 6 (Saturday)".into());
    }
    when.weekdays.sort_unstable();
    when.weekdays.dedup();
    match (&when.from_hhmm, &when.to_hhmm) {
        (None, None) => {}
        (Some(a), Some(b)) if minutes(a).is_some() && minutes(b).is_some() => {}
        _ => return Err("from_hhmm and to_hhmm must both be given as HH:MM".into()),
    }
    Ok(when)
}

/// Parses RFC 3339, or `YYYY-MM-DD HH:MM` / `YYYY-MM-DDTHH:MM` as local time.
pub fn parse_when(text: &str) -> Result<i64, String> {
    use chrono::TimeZone;
    let t = text.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Ok(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, fmt) {
            if let Some(dt) = chrono::Local.from_local_datetime(&n).single() {
                return Ok(dt.timestamp_millis());
            }
        }
    }
    Err(format!("could not read the time \"{t}\": use RFC 3339 or YYYY-MM-DD HH:MM"))
}

pub fn clamp_retention(days: u32) -> u32 {
    days.min(MAX_RETENTION_DAYS)
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Load the config, apply `f`, save it. Used by the Mission Control tools.
pub async fn update<T>(f: impl FnOnce(&mut RecordingPolicy, i64) -> Result<T, String>) -> Result<(T, RecordingPolicy), String> {
    let path = crate::config::default_config_path();
    let mut config = crate::config::SnippetConfig::load(&path).await.map_err(|e| e.to_string())?;
    let now = now_ms();
    config.recording.prune(now);
    let out = f(&mut config.recording, now)?;
    crate::config::save_config(&config, &path)?;
    Ok((out, config.recording))
}

pub async fn current() -> RecordingPolicy {
    let mut p = crate::config::SnippetConfig::load(crate::config::default_config_path()).await.map(|c| c.recording).unwrap_or_default();
    p.prune(now_ms());
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    const NOW: i64 = 1_800_000_000_000;

    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn rule(mode: &str, when: When) -> Rule {
        Rule { id: "r".into(), label: format!("{mode} rule"), mode: mode.into(), when, basis: "b".into(), created_at: 0 }
    }

    fn kw(k: &str) -> When {
        When { keywords: vec![k.into()], ..When::default() }
    }

    #[test]
    fn snooze_and_never_beat_auto_beat_ask() {
        let mut p = RecordingPolicy::default();
        assert_eq!(p.decide(NOW, at(2, 10, 0), "standup", false), Decision::Ask);
        p.rules.push(rule("auto", kw("standup")));
        assert_eq!(p.decide(NOW, at(2, 10, 0), "Daily STANDUP time", false), Decision::Auto { label: "auto rule".into() });
        assert_eq!(p.decide(NOW, at(2, 10, 0), "standup", true), Decision::Ask);
        p.rules.push(rule("never", kw("standup")));
        assert_eq!(p.decide(NOW, at(2, 10, 0), "standup", false), Decision::Drop);
        p.rules.pop();
        p.windows.push(Window { id: "w".into(), label: "retro".into(), from_ms: NOW - 1, to_ms: NOW + 1000, mode: "auto".into(), basis: "b".into(), created_at: 0 });
        assert_eq!(p.decide(NOW, at(2, 10, 0), "unrelated", false), Decision::Auto { label: "retro".into() });
        assert_eq!(p.decide(NOW + 1000, at(2, 10, 0), "unrelated", false), Decision::Ask);
        p.windows[0].mode = "never".into();
        assert_eq!(p.decide(NOW, at(2, 10, 0), "standup", false), Decision::Drop);
        p.windows[0].mode = "auto".into();
        p.snooze_until_ms = Some(NOW + 1);
        assert_eq!(p.decide(NOW, at(2, 10, 0), "standup", false), Decision::Drop);
        assert!(!p.snoozed(NOW + 1));
    }

    #[test]
    fn keywords_weekdays_and_times_including_midnight_wrap() {
        let when = When { keywords: vec!["Board".into()], weekdays: vec![1], from_hhmm: Some("09:00".into()), to_hhmm: Some("10:30".into()) };
        let monday = at(2, 9, 30);
        assert!(when.matches("board review", monday));
        assert!(!when.matches("lunch", monday));
        assert!(!when.matches("board", at(3, 9, 30)));
        assert!(!when.matches("board", at(2, 10, 30)));
        let night = When { from_hhmm: Some("22:00".into()), to_hhmm: Some("02:00".into()), ..When::default() };
        assert!(night.matches("", at(2, 23, 0)) && night.matches("", at(3, 1, 59)));
        assert!(!night.matches("", at(2, 12, 0)) && !night.matches("", at(3, 2, 0)));
        assert!(When::default().matches("anything", monday));
    }

    #[test]
    fn windows_expire_and_are_capped_at_twelve_hours() {
        let mut p = RecordingPolicy::default();
        assert!(p.add_window("x", NOW, NOW + MAX_WINDOW_MS + 1, "auto", "b", NOW).is_err());
        assert!(p.add_window("x", NOW, NOW, "auto", "b", NOW).is_err());
        assert!(p.add_window("x", NOW - 2000, NOW - 1000, "auto", "b", NOW).is_err());
        p.add_window("x", NOW, NOW + MAX_WINDOW_MS, "auto", "b", NOW).unwrap();
        assert!(!p.prune(NOW + MAX_WINDOW_MS - 1));
        assert!(p.prune(NOW + MAX_WINDOW_MS));
        assert!(p.windows.is_empty());
    }

    #[test]
    fn basis_is_required_for_auto_and_never_entries() {
        let mut p = RecordingPolicy::default();
        assert!(p.add_window("x", NOW, NOW + 1000, "auto", "  ", NOW).is_err());
        assert!(p.add_window("x", NOW, NOW + 1000, "never", "", NOW).is_err());
        assert!(p.set_rule("r", "auto", When::default(), "", NOW).is_err());
        assert!(p.set_rule("r", "never", When::default(), " ", NOW).is_err());
        assert!(p.set_rule("r", "ask", When::default(), "", NOW).is_ok());
        let id = p.set_rule("r", "auto", kw(" Standup "), "user: always record standup", NOW).unwrap();
        assert_eq!(p.rules.len(), 1, "same label replaces");
        assert_eq!(p.rules[0].when.keywords, ["standup"]);
        assert!(p.set_rule("bad", "auto", When { weekdays: vec![7], ..When::default() }, "b", NOW).is_err());
        assert!(p.set_rule("bad", "auto", When { from_hhmm: Some("09:00".into()), ..When::default() }, "b", NOW).is_err());
        assert!(p.remove_rule(&id) && !p.remove_rule(&id));
    }

    #[test]
    fn snooze_is_bounded_and_policy_round_trips_through_toml() {
        let mut p = RecordingPolicy::default();
        assert!(p.snooze(0, NOW).is_err() && p.snooze(721, NOW).is_err());
        assert_eq!(p.snooze(60, NOW).unwrap(), NOW + 3_600_000);
        p.set_rule("r", "auto", When { keywords: vec!["a".into()], weekdays: vec![1, 2], from_hhmm: Some("09:00".into()), to_hhmm: Some("10:00".into()) }, "b", NOW).unwrap();
        p.add_window("w", NOW, NOW + 1000, "never", "b", NOW).unwrap();
        let mut c = crate::config::SnippetConfig::default();
        c.recording = p.clone();
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<crate::config::SnippetConfig>(&text).unwrap().recording, p);
        assert!(!toml::to_string_pretty(&crate::config::SnippetConfig::default()).unwrap().contains("recording"));
        assert_eq!(clamp_retention(99_999), 3650);
        assert!(parse_when("2026-11-02 09:30").is_ok() && parse_when("2026-11-02T09:30:00+00:00").is_ok() && parse_when("tomorrow").is_err());
    }
}
