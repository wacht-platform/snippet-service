//! Durable recurring jobs that poke Mission Control or any conversation session.
//!
//! **Detection:** jobs live in the device SQLite database, alongside every other
//! durable record. The serve tick loop is the only reader. It claims due jobs and
//! delivers `LoopInput::SetGoal` so the target session drives that piece of work
//! to `complete_goal`. Optional `plan_path` is read from disk at fire time. If
//! the target is mid-turn, has a running lane, or already has an active goal or
//! a paused one of its own, the fire is queued (one deep). A paused goal left by
//! an earlier scheduled run does not block: the next fire replaces it. Queued jobs are retried as soon as that
//! goal completes (fast poll while anything is queued) — missed intervals are not
//! backfilled.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::Store;

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Default root for recurring jobs. Now only a KEY: the jobs themselves live in
/// the device database, and this path resolves to it.
pub fn default_root() -> PathBuf {
    crate::config::snippet_home().join("recurring")
}

/// The store these jobs live in. Any root inside `~/.snippet` resolves to the one
/// device database; a test root gets its own file.
fn store_for(root: &Path) -> Result<Store, String> {
    Store::open_cached(crate::app_store::app_db_path(root)).map_err(|e| e.to_string())
}

/// Shortest repeating interval (5 minutes).
pub const MIN_INTERVAL_SECS: u64 = 300;

/// How often a job fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Fire every `every_secs` seconds (minimum [`MIN_INTERVAL_SECS`]).
    Interval { every_secs: u64 },
    /// Fire once a day at `hour`:`minute` in the user's time (0–23, 0–59).
    Daily { hour: u8, minute: u8 },
    /// Fire at `hour`:`minute` on the weekdays in `days` (bit 0 = Monday).
    Weekly { days: u8, hour: u8, minute: u8 },
    /// One-off fire at unix second `at`. After it fires the job is disabled
    /// (kept so the UI can show it as done).
    Once { at: u64 },
}

impl Schedule {
    /// Parse `every 5m|15m|1h|1d|300s`, `daily HH:MM`, `at HH:MM` (next
    /// occurrence), or `in <duration>` (e.g. `in 30m`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let lower = raw.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("every ") {
            let every_secs = parse_duration(rest.trim())?;
            if every_secs < MIN_INTERVAL_SECS {
                return Err("interval must be at least 5 minutes".into());
            }
            return Ok(Self::Interval { every_secs });
        }
        if let Some(rest) = lower.strip_prefix("daily ") {
            let (hour, minute) = parse_hhmm(rest.trim())?;
            return Ok(Self::Daily { hour, minute });
        }
        if let Some(rest) = lower.strip_prefix("at ") {
            let (hour, minute) = parse_hhmm(rest.trim())?;
            let now = epoch_secs();
            return Ok(Self::Once {
                at: next_time_after(now, hour, minute, ALL_DAYS, user_offset()),
            });
        }
        if let Some((days, time)) = lower.split_once(' ')
            && let Some(days) = parse_days(days)
        {
            let (hour, minute) = parse_hhmm(time.trim())?;
            return Ok(Self::Weekly { days, hour, minute });
        }
        if let Some(rest) = lower.strip_prefix("in ") {
            let secs = parse_duration(rest.trim())?;
            return Ok(Self::Once {
                at: epoch_secs().saturating_add(secs),
            });
        }
        Err("schedule must be `every 5m|1h|1d`, `daily HH:MM`, `weekdays HH:MM`, `mon,wed HH:MM`, `at HH:MM`, or `in 30m`".into())
    }

    pub fn display(&self) -> String {
        match self {
            Self::Interval { every_secs } => {
                if *every_secs % 86400 == 0 {
                    format!("every {}d", every_secs / 86400)
                } else if *every_secs % 3600 == 0 {
                    format!("every {}h", every_secs / 3600)
                } else if *every_secs % 60 == 0 {
                    format!("every {}m", every_secs / 60)
                } else {
                    format!("every {every_secs}s")
                }
            }
            Self::Daily { hour, minute } => format!("daily {hour:02}:{minute:02}"),
            Self::Weekly { days, hour, minute } => format!("{} {hour:02}:{minute:02}", display_days(*days)),
            Self::Once { at } => format!("once {}", fmt_hhmm(*at)),
        }
    }

    pub fn interval_secs(&self) -> Option<u64> {
        match self {
            Self::Interval { every_secs } => Some(*every_secs),
            _ => None,
        }
    }

    /// Next fire time after `from` (unix seconds). `Once` never re-arms: after
    /// its moment passes it stays due-until-fired (the tick disables it right
    /// after delivering), so return `at` unchanged.
    pub fn next_after(&self, from: u64) -> u64 {
        self.next_after_in(from, user_offset())
    }

    /// Wall-clock schedules are read in `offset` (the user's, not the server's).
    pub fn next_after_in(&self, from: u64, offset: chrono::FixedOffset) -> u64 {
        match self {
            Self::Interval { every_secs } => from.saturating_add(*every_secs),
            Self::Daily { hour, minute } => next_time_after(from, *hour, *minute, ALL_DAYS, offset),
            Self::Weekly { days, hour, minute } => next_time_after(from, *hour, *minute, *days, offset),
            Self::Once { at } => *at,
        }
    }
}

const ALL_DAYS: u8 = 0x7f;
const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn parse_days(raw: &str) -> Option<u8> {
    match raw {
        "weekdays" => return Some(0x1f),
        "weekends" => return Some(0x60),
        _ => {}
    }
    let mut mask = 0u8;
    for part in raw.split(',') {
        let i = DAY_NAMES.iter().position(|d| part.trim().starts_with(d) && part.trim().len() >= 3)?;
        mask |= 1 << i;
    }
    (mask != 0).then_some(mask)
}

fn display_days(mask: u8) -> String {
    match mask {
        0x1f => "weekdays".into(),
        0x60 => "weekends".into(),
        ALL_DAYS => "daily".into(),
        _ => DAY_NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, d)| *d)
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// The user's offset from Mission Control's settings, else the server's.
pub fn user_offset() -> chrono::FixedOffset {
    crate::mission_autonomy::offset_or_local(crate::mission_autonomy::user_offset_minutes())
}

/// HH:MM for a unix timestamp in the user's time (used in the `once` display).
fn fmt_hhmm(at: u64) -> String {
    use chrono::{TimeZone, Timelike};
    let at_i = i64::try_from(at).unwrap_or(i64::MAX);
    match user_offset().timestamp_opt(at_i, 0).single() {
        Some(dt) => format!("{:02}:{:02}", dt.hour(), dt.minute()),
        None => "??".into(),
    }
}

fn parse_duration(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("missing duration".into());
    }
    let (num, unit) = raw.split_at(raw.len().saturating_sub(1));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration `{raw}`"))?;
    if n == 0 {
        return Err("duration must be > 0".into());
    }
    match unit {
        "s" => Ok(n),
        "m" => Ok(n.saturating_mul(60)),
        "h" => Ok(n.saturating_mul(3600)),
        "d" => Ok(n.saturating_mul(86400)),
        _ => Err(format!("unknown duration unit in `{raw}` (use s/m/h/d)")),
    }
}

fn parse_hhmm(raw: &str) -> Result<(u8, u8), String> {
    let mut parts = raw.split(':');
    let hour: u8 = parts
        .next()
        .ok_or_else(|| "daily needs HH:MM".to_string())?
        .parse()
        .map_err(|_| "daily hour must be 0–23".to_string())?;
    let minute: u8 = parts
        .next()
        .ok_or_else(|| "daily needs HH:MM".to_string())?
        .parse()
        .map_err(|_| "daily minute must be 0–59".to_string())?;
    if parts.next().is_some() {
        return Err("daily needs HH:MM".into());
    }
    if hour > 23 {
        return Err("daily hour must be 0–23".into());
    }
    if minute > 59 {
        return Err("daily minute must be 0–59".into());
    }
    Ok((hour, minute))
}

/// Next `hour`:`minute` after `from` on a day in `days`, read in `offset`.
fn next_time_after(from: u64, hour: u8, minute: u8, days: u8, offset: chrono::FixedOffset) -> u64 {
    use chrono::{Datelike, NaiveTime, TimeZone};
    let from_i = i64::try_from(from).unwrap_or(i64::MAX);
    let (Some(now), Some(tod)) = (
        offset.timestamp_opt(from_i, 0).single(),
        NaiveTime::from_hms_opt(hour as u32, minute as u32, 0),
    ) else {
        return from.saturating_add(86400);
    };
    let mut day = now.date_naive();
    for _ in 0..8 {
        if days & (1 << day.weekday().num_days_from_monday()) != 0
            && let Some(dt) = offset.from_local_datetime(&day.and_time(tod)).single()
            && dt.timestamp() > from_i
        {
            return dt.timestamp() as u64;
        }
        day = day.succ_opt().unwrap_or(day);
    }
    from.saturating_add(86400)
}

/// A durable recurring poke.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringJob {
    pub id: String,
    pub title: String,
    /// Target conversation: `mission-control` or any session id.
    pub session_id: String,
    pub prompt: String,
    /// Optional markdown/plan file, read at fire time (relative to the session
    /// workspace, or absolute). Empty/None = prompt only.
    #[serde(default)]
    pub plan_path: Option<String>,
    pub schedule: Schedule,
    /// How the fire lands in the session: an autonomous [`Delivery::Goal`]
    /// (default) or a plain [`Delivery::Message`] chat turn.
    #[serde(default)]
    pub delivery: Delivery,
    pub enabled: bool,
    pub next_run_at: u64,
    #[serde(default)]
    pub last_run_at: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// One-deep queued fire: true when a due run was skipped because the
    /// session was busy. Next idle tick delivers it, then advances the
    /// schedule from *now* (no backlog of missed intervals).
    #[serde(default)]
    pub queued: bool,
    pub created_at: u64,
    pub updated_at: u64,
    /// Set when this job is an agent's wake schedule.
    #[serde(default)]
    pub wake: Option<crate::agent_wakes::WakePolicy>,
    #[serde(default)]
    pub last_result: Option<String>,
}

/// One fire of a job, kept for audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecurringRun {
    pub id: i64,
    pub job_id: String,
    pub owner: Option<String>,
    pub session_id: String,
    pub fired_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub summary: Option<String>,
    pub reported_to: Option<String>,
}

pub fn store_at(root: &Path) -> Result<Store, String> {
    store_for(root)
}

/// How a fire reaches the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Set an autonomous goal the agent drives to `complete_goal`.
    #[default]
    Goal,
    /// Send a plain user message (scheduled one-off messages).
    Message,
}

impl RecurringJob {
    /// Body text shared by both deliveries: title, prompt, and the plan file
    /// read now so the markdown/plan can change between runs. Title and prompt
    /// that restate each other render once (the longer variant) — firing
    /// "say hi / say hi to me" as two lines reads like a glitch.
    fn render_body(&self, workspace: Option<&Path>) -> Result<String, String> {
        let title = self.title.trim();
        let prompt = self.prompt.trim();
        let (title, prompt) = if !title.is_empty() && !prompt.is_empty() {
            let (t, p) = (normalize_fire_text(title), normalize_fire_text(prompt));
            if !t.is_empty() && p.contains(&t) {
                // Prompt restates the title — the prompt says it all.
                ("", prompt)
            } else if !p.is_empty() && t.contains(&p) {
                // Title restates the prompt — the title says it all.
                (title, "")
            } else {
                (title, prompt)
            }
        } else {
            (title, prompt)
        };
        let mut body = String::new();
        if !title.is_empty() {
            body.push_str(title);
            body.push_str("\n\n");
        }
        if !prompt.is_empty() {
            body.push_str(prompt);
            body.push('\n');
        }
        if let Some(path) = self
            .plan_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let contents = read_plan(path, workspace)?;
            if !prompt.is_empty() || !title.is_empty() {
                body.push('\n');
            }
            body.push_str("From `");
            body.push_str(path);
            body.push_str("`:\n\n");
            body.push_str(&contents);
            if !contents.ends_with('\n') {
                body.push('\n');
            }
        }
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err("goal text is empty".into());
        }
        Ok(body)
    }

    /// Goal text for this fire: a formatted scheduled-run header (job,
    /// schedule, fire time), the instruction, and a closing directive telling
    /// the agent to report what happened via `complete_goal` — so the chat
    /// shows what was scheduled, what was asked, and how the run went, not a
    /// bare prompt dump.
    pub fn render_goal(&self, workspace: Option<&Path>) -> Result<String, String> {
        let body = self.render_body(workspace)?;
        let job_label = if self.title.trim().is_empty() {
            "untitled job"
        } else {
            self.title.trim()
        };
        let mut out = String::new();
        out.push_str(&format!("**Scheduled run — {job_label}**\n\n"));
        out.push_str(&format!("- Schedule: {}\n", self.schedule.display()));
        out.push_str(&format!("- Fired: {}\n", fmt_local_time(epoch_secs())));
        out.push_str("\n---\n\n");
        out.push_str(&body);
        if let Some(wake) = &self.wake {
            out.push_str(&crate::agent_wakes::goal_addendum(wake, self.last_result.as_deref(), self.last_run_at));
        }
        let playbook = self.playbook_topic();
        out.push_str(&format!("\n\n---\n\nThis job runs again, and each run should go better than the last. Before you start, read its playbook in project memory (`playbooks/{playbook}`) if it exists, and follow it. Before you finish, update that playbook: what worked and should stay the approach, what went wrong and what to do instead next time (situation → approach → why), and any entry this run proved wrong. Keep it short: a proven rule replaces the run notes that led to it."));
        out.push_str("\n\nWhen the work is done (or blocked), call `complete_goal` with a short summary saying **what this run was**, **what you did**, **how it went**, and **what the next run will do better** — that summary is surfaced to the user as the run's outcome.");
        Ok(out)
    }

    pub fn playbook_topic(&self) -> String {
        let mut slug = String::new();
        for c in self.title.trim().chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }
        let slug = slug.trim_end_matches('-');
        if slug.is_empty() { self.id.clone() } else { slug.to_string() }
    }

}

/// "YYYY-MM-DD HH:MM" in the user's time for a unix timestamp (fire header).
pub fn fmt_local_time(at: u64) -> String {
    use chrono::TimeZone;
    let at_i = i64::try_from(at).unwrap_or(i64::MAX);
    match user_offset().timestamp_opt(at_i, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "unknown time".into(),
    }
}

/// Lowercase and collapse every non-alphanumeric run to single spaces — the
/// comparison basis for "title and prompt restate each other".
fn normalize_fire_text(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

const PLAN_MAX_BYTES: usize = 64 * 1024;

fn read_plan(path: &str, workspace: Option<&Path>) -> Result<String, String> {
    let given = PathBuf::from(path);
    let resolved = if given.is_absolute() {
        given
    } else {
        let ws = workspace
            .ok_or_else(|| "plan path is relative but this session has no workspace".to_string())?;
        let joined = ws.join(&given);
        let canon_ws = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
        match joined.canonicalize() {
            Ok(canon) => {
                if !canon.starts_with(&canon_ws) {
                    return Err("plan path must stay inside the session workspace".into());
                }
                canon
            }
            Err(_) => {
                return Err(format!("plan file not found: {}", joined.display()));
            }
        }
    };
    let bytes =
        fs::read(&resolved).map_err(|e| format!("read plan {}: {e}", resolved.display()))?;
    if bytes.len() > PLAN_MAX_BYTES {
        return Err("plan file is larger than 64 KiB".into());
    }
    String::from_utf8(bytes).map_err(|_| "plan file is not UTF-8".into())
}

pub fn list_jobs(root: &Path) -> Result<Vec<RecurringJob>, String> {
    let mut jobs = store_for(root)?.list_recurring_jobs().map_err(|e| e.to_string())?;
    jobs.sort_by(|a, b| a.next_run_at.cmp(&b.next_run_at).then(a.id.cmp(&b.id)));
    Ok(jobs)
}

fn normalize_plan_path(raw: Option<&str>) -> Option<String> {
    let p = raw?.trim();
    if p.is_empty() {
        None
    } else {
        Some(p.to_string())
    }
}

pub fn create_job(
    root: &Path,
    title: &str,
    session_id: &str,
    prompt: &str,
    schedule: Schedule,
    plan_path: Option<&str>,
) -> Result<RecurringJob, String> {
    create_job_with(
        root,
        title,
        session_id,
        prompt,
        schedule,
        plan_path,
        Delivery::Goal,
    )
}

/// Same as [`create_job`] with an explicit delivery and immediate first run:
/// `next_run_at` starts at `now` so the tick fires it on its next pass (the
/// "first run immediately" behaviour for both goals and scheduled messages).
pub fn create_job_with(
    root: &Path,
    title: &str,
    session_id: &str,
    prompt: &str,
    schedule: Schedule,
    plan_path: Option<&str>,
    delivery: Delivery,
) -> Result<RecurringJob, String> {
    let title = title.trim();
    let session_id = session_id.trim();
    let prompt = prompt.trim();
    let plan_path = normalize_plan_path(plan_path);
    if title.is_empty() {
        return Err("title is required".into());
    }
    if session_id.is_empty() {
        return Err("session_id is required".into());
    }
    if prompt.is_empty() && plan_path.is_none() {
        return Err("prompt or plan_path is required".into());
    }
    let now = epoch_secs();
    let job = RecurringJob {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        plan_path,
        delivery,
        // First run fires on the next tick (≤15s away) instead of waiting a
        // full interval / until tomorrow.
        next_run_at: now,
        schedule,
        enabled: true,
        last_run_at: None,
        last_error: None,
        queued: false,
        created_at: now,
        updated_at: now,
        wake: None,
        last_result: None,
    };
    store_for(root)?
        .upsert_recurring_job(&job)
        .map_err(|e| e.to_string())?;
    notify_changed(&job.id);
    Ok(job)
}

pub fn update_job(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut RecurringJob),
) -> Result<RecurringJob, String> {
    let store = store_for(root)?;
    let mut job = store
        .get_recurring_job(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no recurring job `{id}`"))?;
    f(&mut job);
    job.updated_at = epoch_secs();
    store
        .upsert_recurring_job(&job)
        .map_err(|e| e.to_string())?;
    notify_changed(&job.id);
    Ok(job)
}

fn notify_changed(id: &str) {
    crate::session::emit_device_event(serde_json::json!({ "kind": "recurring", "id": id }));
}

pub fn delete_job(root: &Path, id: &str) -> Result<(), String> {
    if store_for(root)?
        .delete_recurring_job(id)
        .map_err(|e| e.to_string())?
    {
        notify_changed(id);
        Ok(())
    } else {
        Err(format!("no recurring job `{id}`"))
    }
}

pub fn set_enabled(root: &Path, id: &str, enabled: bool) -> Result<RecurringJob, String> {
    update_job(root, id, |job| {
        job.enabled = enabled;
        if enabled && job.next_run_at < epoch_secs() {
            job.next_run_at = job.schedule.next_after(epoch_secs());
        }
        if !enabled {
            job.queued = false;
        }
    })
}

/// Claim due jobs: enabled, `next_run_at <= now` or `queued`. Does not mutate
/// `next_run_at` — the tick loop marks success / queue / error after delivery.
pub fn due_jobs(root: &Path, now: u64) -> Result<Vec<RecurringJob>, String> {
    Ok(list_jobs(root)?
        .into_iter()
        .filter(|j| j.enabled && (j.queued || j.next_run_at <= now))
        .collect())
}

/// True when any enabled job is waiting on a busy session (goal already running).
pub fn has_queued(root: &Path) -> bool {
    list_jobs(root)
        .map(|jobs| jobs.iter().any(|j| j.enabled && j.queued))
        .unwrap_or(false)
}

/// After a successful delivery: clear queue, stamp last_run, advance schedule
/// from `now` (skip missed intervals). A `Once` schedule is disabled instead —
/// it has fired its one shot and stays visible in the UI as done.
pub fn mark_fired(root: &Path, id: &str, now: u64) -> Result<RecurringJob, String> {
    update_job(root, id, |job| {
        job.queued = false;
        job.last_run_at = Some(now);
        job.last_error = None;
        if matches!(job.schedule, Schedule::Once { .. }) {
            job.enabled = false;
        } else {
            job.next_run_at = job.schedule.next_after(now);
        }
    })
}

/// Session was busy: keep one queued fire, leave `next_run_at` as-is so it
/// stays due. A second due interval while already queued is a no-op.
pub fn mark_queued(root: &Path, id: &str) -> Result<RecurringJob, String> {
    update_job(root, id, |job| {
        job.queued = true;
        job.last_error = None;
    })
}

pub fn mark_error(root: &Path, id: &str, error: &str) -> Result<RecurringJob, String> {
    let now = epoch_secs();
    update_job(root, id, |job| {
        job.last_error = Some(error.to_string());
        // Still advance so a persistent failure doesn't tight-loop.
        job.queued = false;
        job.next_run_at = job.schedule.next_after(now);
    })
}

/// Whether a harness status should defer a scheduled poke.
pub fn session_is_busy(status: &str) -> bool {
    matches!(status, "running" | "waiting_for_input")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn parse_interval_and_daily() {
        assert_eq!(
            Schedule::parse("every 15m").unwrap(),
            Schedule::Interval { every_secs: 900 }
        );
        assert_eq!(
            Schedule::parse("every 1h").unwrap(),
            Schedule::Interval { every_secs: 3600 }
        );
        assert_eq!(
            Schedule::parse("every 1d").unwrap(),
            Schedule::Interval { every_secs: 86400 }
        );
        assert_eq!(
            Schedule::parse("daily 09:30").unwrap(),
            Schedule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert!(Schedule::parse("every 30s").is_err());
        assert!(Schedule::parse("every 4m").is_err());
        assert_eq!(
            Schedule::parse("every 5m").unwrap(),
            Schedule::Interval { every_secs: 300 }
        );
        assert_eq!(
            Schedule::parse("every 300s").unwrap(),
            Schedule::Interval { every_secs: 300 }
        );
        assert!(Schedule::parse("cron * * *").is_err());
        assert!(Schedule::parse("daily 25:00").is_err());
    }

    #[test]
    fn interval_next_after_adds() {
        let s = Schedule::Interval { every_secs: 300 };
        assert_eq!(s.next_after(1000), 1300);
    }

    #[test]
    fn once_schedule_parses_and_never_rearms() {
        let now = epoch_secs();
        match Schedule::parse("in 30m").unwrap() {
            Schedule::Once { at } => {
                assert!(at >= now + 1800 && at <= now + 1800 + 2);
            }
            other => panic!("expected Once, got {other:?}"),
        }
        match Schedule::parse("at 09:00").unwrap() {
            Schedule::Once { at } => {
                // Next occurrence of 09:00 — today or tomorrow, but in the future.
                assert!(at > now);
            }
            other => panic!("expected Once, got {other:?}"),
        }
        assert!(Schedule::parse("at 25:00").is_err());
        assert!(Schedule::parse("in 0s").is_err());
        // Once never re-arms.
        let s = Schedule::Once { at: 5000 };
        assert_eq!(s.next_after(9999), 5000);
    }

    #[test]
    fn create_job_first_run_is_immediate() {
        let root = tempfile::tempdir().unwrap();
        let before = epoch_secs();
        let job = create_job(
            root.path(),
            "Nightly review",
            "mission-control",
            "Review the inbox",
            Schedule::Interval { every_secs: 3600 },
            None,
        )
        .unwrap();
        // Due on the next tick, not a full interval out.
        assert!(job.next_run_at <= before + 2);
        // And the schedule still advances from fire time afterwards.
        let fired = mark_fired(root.path(), &job.id, epoch_secs()).unwrap();
        assert!(fired.next_run_at >= before + 3600);
    }

    #[test]
    fn once_job_disables_after_firing() {
        let root = tempfile::tempdir().unwrap();
        let job = create_job(
            root.path(),
            "One-off",
            "mission-control",
            "Say hi at noon",
            Schedule::Once { at: epoch_secs() },
            None,
        )
        .unwrap();
        assert!(job.enabled);
        let fired = mark_fired(root.path(), &job.id, epoch_secs()).unwrap();
        assert!(!fired.enabled, "Once should disable after firing");
        // Due-filter ignores it now.
        assert!(
            due_jobs(root.path(), epoch_secs())
                .unwrap()
                .iter()
                .all(|j| j.id != job.id)
        );
    }

    #[test]
    fn message_delivery_defaults_to_goal() {
        let raw = serde_json::to_string(&RecurringJob {
            id: "x".into(),
            title: "t".into(),
            session_id: "mission-control".into(),
            prompt: "p".into(),
            plan_path: None,
            schedule: Schedule::Interval { every_secs: 3600 },
            delivery: Delivery::Goal,
            enabled: true,
            next_run_at: 0,
            last_run_at: None,
            last_error: None,
            queued: false,
            created_at: 0,
            updated_at: 0,
            wake: None,
            last_result: None,
        })
        .unwrap();
        // Old on-disk jobs (no delivery field) must parse as Goal.
        let without_delivery = raw.replace(r#","delivery":"goal""#, "");
        let job: RecurringJob = serde_json::from_str(&without_delivery).unwrap();
        assert_eq!(job.delivery, Delivery::Goal);
    }

    #[test]
    fn create_list_update_delete() {
        let root = tmp();
        let job = create_job(
            root.path(),
            "Check CI",
            "mission-control",
            "summarize CI",
            Schedule::parse("every 1h").unwrap(),
            None,
        )
        .unwrap();
        assert!(job.enabled);
        assert_eq!(job.session_id, "mission-control");
        assert!(!job.queued);
        // First run is immediate: due on the next tick, not a full interval out.
        assert!(job.next_run_at <= job.created_at);

        let listed = list_jobs(root.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        set_enabled(root.path(), &job.id, false).unwrap();
        assert!(!list_jobs(root.path()).unwrap()[0].enabled);

        delete_job(root.path(), &job.id).unwrap();
        assert!(list_jobs(root.path()).unwrap().is_empty());
    }

    #[test]
    fn due_includes_queued_even_if_next_run_in_future() {
        let root = tmp();
        let job = create_job(
            root.path(),
            "T",
            "s1",
            "do it",
            Schedule::Interval { every_secs: 3600 },
            None,
        )
        .unwrap();
        mark_queued(root.path(), &job.id).unwrap();
        let due = due_jobs(root.path(), 0).unwrap();
        assert_eq!(due.len(), 1);
        assert!(due[0].queued);
    }

    #[test]
    fn mark_fired_clears_queue_and_skips_backlog() {
        let root = tmp();
        let job = create_job(
            root.path(),
            "T",
            "s1",
            "do it",
            Schedule::Interval { every_secs: 300 },
            None,
        )
        .unwrap();
        // Pretend it was due an hour ago and queued.
        update_job(root.path(), &job.id, |j| {
            j.next_run_at = 1000;
            j.queued = true;
        })
        .unwrap();
        let now = 1000 + 3600;
        let fired = mark_fired(root.path(), &job.id, now).unwrap();
        assert!(!fired.queued);
        assert_eq!(fired.last_run_at, Some(now));
        assert_eq!(fired.next_run_at, now + 300);
    }

    #[test]
    fn render_goal_is_work_to_complete() {
        let job = RecurringJob {
            id: "abc".into(),
            title: "Ping".into(),
            session_id: "mission-control".into(),
            prompt: "say hello".into(),
            plan_path: None,
            schedule: Schedule::Interval { every_secs: 300 },
            delivery: Delivery::Goal,
            enabled: true,
            next_run_at: 0,
            last_run_at: None,
            last_error: None,
            queued: false,
            created_at: 0,
            updated_at: 0,
            wake: None,
            last_result: None,
        };
        let msg = job.render_goal(None).unwrap();
        // Formatted header, then the instruction, then the outcome directive.
        assert!(msg.starts_with("**Scheduled run — Ping**"));
        assert!(msg.contains("say hello"));
        assert!(msg.contains("Schedule: every 5m"));
        assert!(msg.contains("Fired: "));
        assert!(msg.contains("complete_goal"));
        assert!(msg.contains("how it went"));
        assert!(!msg.contains("[recurring_job]"));
    }

    #[test]
    fn render_goal_includes_plan_file() {
        let dir = tmp();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# Do the thing\n\n- step 1\n").unwrap();
        let job = RecurringJob {
            id: "abc".into(),
            title: "Nightly".into(),
            session_id: "s1".into(),
            prompt: "follow the plan".into(),
            plan_path: Some(plan.display().to_string()),
            schedule: Schedule::Interval { every_secs: 300 },
            delivery: Delivery::Goal,
            enabled: true,
            next_run_at: 0,
            last_run_at: None,
            last_error: None,
            queued: false,
            created_at: 0,
            updated_at: 0,
            wake: None,
            last_result: None,
        };
        let msg = job.render_goal(None).unwrap();
        assert!(msg.contains("follow the plan"));
        assert!(msg.contains("# Do the thing"));
        assert!(msg.contains("From `"));
    }

    #[test]
    fn create_accepts_plan_without_prompt() {
        let root = tmp();
        let job = create_job(
            root.path(),
            "From file",
            "s1",
            "",
            Schedule::Interval { every_secs: 300 },
            Some("notes/plan.md"),
        )
        .unwrap();
        assert_eq!(job.plan_path.as_deref(), Some("notes/plan.md"));
        assert!(job.prompt.is_empty());
    }

    #[test]
    fn busy_statuses() {
        assert!(session_is_busy("running"));
        assert!(session_is_busy("waiting_for_input"));
        assert!(!session_is_busy("idle"));
        assert!(!session_is_busy("completed"));
    }

    #[test]
    fn weekly_schedules_parse_and_display() {
        assert_eq!(Schedule::parse("weekdays 09:00").unwrap(), Schedule::Weekly { days: 0x1f, hour: 9, minute: 0 });
        assert_eq!(Schedule::parse("Mon,Thu 18:30").unwrap(), Schedule::Weekly { days: 0b1001, hour: 18, minute: 30 });
        assert_eq!(Schedule::parse("weekends 10:00").unwrap().display(), "weekends 10:00");
        assert_eq!(Schedule::parse("mon,wed 07:05").unwrap().display(), "mon,wed 07:05");
        assert!(Schedule::parse("mon,xyz 09:00").is_err());
        assert!(Schedule::parse("weekdays 25:00").is_err());
    }

    #[test]
    fn wall_clock_schedules_follow_the_given_offset() {
        let ist = chrono::FixedOffset::east_opt(330 * 60).unwrap();
        let utc = chrono::FixedOffset::east_opt(0).unwrap();
        // Sat 31 Oct 2026 13:00 UTC = 18:30 IST.
        let from = 1_793_451_600;
        let daily = Schedule::Daily { hour: 9, minute: 0 };
        assert_eq!(daily.next_after_in(from, ist), 1_793_503_800);
        assert_eq!(daily.next_after_in(from, utc), 1_793_523_600);
        let weekdays = Schedule::Weekly { days: 0x1f, hour: 9, minute: 0 };
        assert_eq!(weekdays.next_after_in(from, ist), 1_793_503_800 + 86_400);
        let sat = Schedule::Weekly { days: 1 << 5, hour: 20, minute: 0 };
        assert_eq!(sat.next_after_in(from, ist), from + 5400);
        assert_eq!(sat.next_after_in(from + 5400, ist), from + 5400 + 7 * 86_400);
    }

}
