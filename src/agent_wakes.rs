//! Wake schedules for any agent or session, built on recurring jobs.
//!
//! A wake schedule is a [`RecurringJob`] carrying a [`WakePolicy`]: the job
//! supplies the schedule, the brief (its prompt), the target session and
//! next/last run, and the serve tick fires it like any other job. The policy
//! adds who owns it, quiet hours, a daily run cap and where the result is
//! reported. One schedule per owner.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::recurring::{self, Delivery, RecurringJob, Schedule};
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

/// Shortest interval an agent or Mission Control may set.
pub const MIN_AGENT_INTERVAL_SECS: u64 = 15 * 60;
pub const DEFAULT_MAX_RUNS_PER_DAY: u32 = 48;
pub const MAX_RUNS_PER_DAY_CEILING: u32 = 288;
/// Schedules Mission Control may hold on other agents at once.
pub const MAX_SET_BY_MISSION_CONTROL: usize = 12;
pub const NO_CHANGE: &str = "NO_CHANGE";
const BRIEF_MAX_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setter {
    User,
    MissionControl,
    #[serde(rename = "self")]
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportTo {
    #[default]
    MissionControl,
    User,
    TaskRoom,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportPolicy {
    #[serde(default)]
    pub to: ReportTo,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default = "yes")]
    pub only_on_change: bool,
}

fn yes() -> bool {
    true
}

impl Default for ReportPolicy {
    fn default() -> Self {
        Self { to: ReportTo::MissionControl, task_id: None, only_on_change: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakePolicy {
    pub owner: String,
    pub owner_kind: OwnerKind,
    pub set_by: Setter,
    #[serde(default)]
    pub quiet_start: Option<String>,
    #[serde(default)]
    pub quiet_end: Option<String>,
    /// None follows the user's offset from Mission Control's settings.
    #[serde(default)]
    pub utc_offset_minutes: Option<i32>,
    #[serde(default)]
    pub report: ReportPolicy,
    #[serde(default = "default_cap")]
    pub max_runs_per_day: u32,
}

fn default_cap() -> u32 {
    DEFAULT_MAX_RUNS_PER_DAY
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Agent,
    Session,
    MissionControl,
}

/// What the user, an agent or Mission Control asks for. Absent fields keep
/// the current value on an update; an empty quiet clock clears quiet hours.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WakeSpec {
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub quiet_start: Option<String>,
    #[serde(default)]
    pub quiet_end: Option<String>,
    #[serde(default)]
    pub utc_offset_minutes: Option<i32>,
    #[serde(default)]
    pub report: Option<ReportPolicy>,
    #[serde(default)]
    pub max_runs_per_day: Option<u32>,
}

pub fn session_for(owner: &str, kind: OwnerKind) -> String {
    match kind {
        OwnerKind::Agent => crate::session::inbox_session_id(owner),
        OwnerKind::MissionControl => crate::mission_control::SESSION_ID.to_string(),
        OwnerKind::Session => owner.to_string(),
    }
}

/// An agent id, Mission Control, or an existing session id.
pub fn resolve_owner(store: &crate::store::Store, owner: &str) -> Result<OwnerKind, String> {
    let owner = owner.trim();
    if owner.is_empty() {
        return Err("owner is required".into());
    }
    if owner == crate::mission_control::SESSION_ID {
        return Ok(OwnerKind::MissionControl);
    }
    if store.get_agent(owner).map_err(|e| e.to_string())?.is_some() {
        return Ok(OwnerKind::Agent);
    }
    if store.has_conversation(owner).map_err(|e| e.to_string())? {
        return Ok(OwnerKind::Session);
    }
    Err(format!("no agent or session `{owner}`"))
}

pub fn list(root: &Path) -> Result<Vec<RecurringJob>, String> {
    Ok(recurring::list_jobs(root)?.into_iter().filter(|j| j.wake.is_some()).collect())
}

pub fn get(root: &Path, owner: &str) -> Result<Option<RecurringJob>, String> {
    Ok(list(root)?.into_iter().find(|j| j.wake.as_ref().is_some_and(|w| w.owner == owner.trim())))
}

pub fn clear(root: &Path, owner: &str) -> Result<bool, String> {
    match get(root, owner)? {
        Some(job) => recurring::delete_job(root, &job.id).map(|_| true),
        None => Ok(false),
    }
}

fn clean_clock(raw: Option<String>, current: Option<String>) -> Result<Option<String>, String> {
    match raw {
        None => Ok(current),
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) if crate::mission_autonomy::valid_clock(&v) => Ok(Some(v.trim().to_string())),
        Some(v) => Err(format!("quiet hours need HH:MM, got `{v}`")),
    }
}

pub fn validate_schedule(schedule: &Schedule, setter: Setter) -> Result<(), String> {
    let min = if setter == Setter::User { recurring::MIN_INTERVAL_SECS } else { MIN_AGENT_INTERVAL_SECS };
    match schedule.interval_secs() {
        Some(secs) if secs < min => Err(format!("interval must be at least {} minutes", min / 60)),
        _ => Ok(()),
    }
}

/// Create or update the owner's schedule.
pub fn set(
    root: &Path,
    owner: &str,
    kind: OwnerKind,
    spec: WakeSpec,
    setter: Setter,
    now: u64,
) -> Result<RecurringJob, String> {
    let owner = owner.trim().to_string();
    let existing = get(root, &owner)?;
    if let (Some(job), Setter::MissionControl) = (&existing, setter)
        && job.wake.as_ref().is_some_and(|w| w.set_by == Setter::User)
    {
        return Err("the user set this schedule; ask them to change it".into());
    }
    if setter == Setter::MissionControl && existing.is_none() {
        let held = list(root)?
            .iter()
            .filter(|j| j.wake.as_ref().is_some_and(|w| w.set_by == Setter::MissionControl))
            .count();
        if held >= MAX_SET_BY_MISSION_CONTROL {
            return Err(format!("Mission Control already holds {held} wake schedules; clear one first"));
        }
    }
    let schedule = match spec.schedule.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => Some(Schedule::parse(raw)?),
        None => None,
    };
    if let Some(s) = &schedule {
        validate_schedule(s, setter)?;
    }
    let brief = spec.brief.map(|b| b.trim().to_string());
    if brief.as_ref().is_some_and(|b| b.chars().count() > BRIEF_MAX_CHARS) {
        return Err(format!("brief is limited to {BRIEF_MAX_CHARS} characters"));
    }
    let current = existing.as_ref().and_then(|j| j.wake.clone());
    let report = spec.report.or_else(|| current.as_ref().map(|w| w.report.clone())).unwrap_or_default();
    if report.to == ReportTo::TaskRoom && report.task_id.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return Err("report to task_room needs task_id".into());
    }
    let cap = spec
        .max_runs_per_day
        .or(current.as_ref().map(|w| w.max_runs_per_day))
        .unwrap_or(DEFAULT_MAX_RUNS_PER_DAY);
    let ceiling = if setter == Setter::User { MAX_RUNS_PER_DAY_CEILING } else { DEFAULT_MAX_RUNS_PER_DAY };
    if cap == 0 || cap > ceiling {
        return Err(format!("max_runs_per_day must be 1 to {ceiling}"));
    }
    if let Some(m) = spec.utc_offset_minutes
        && !(-14 * 60..=14 * 60).contains(&m)
    {
        return Err("utc_offset_minutes must be within ±14 hours".into());
    }
    let policy = WakePolicy {
        owner: owner.clone(),
        owner_kind: kind,
        set_by: setter,
        quiet_start: clean_clock(spec.quiet_start, current.as_ref().and_then(|w| w.quiet_start.clone()))?,
        quiet_end: clean_clock(spec.quiet_end, current.as_ref().and_then(|w| w.quiet_end.clone()))?,
        utc_offset_minutes: spec.utc_offset_minutes.or(current.as_ref().and_then(|w| w.utc_offset_minutes)),
        report,
        max_runs_per_day: cap,
    };
    if policy.quiet_start.is_some() != policy.quiet_end.is_some() {
        return Err("quiet hours need both quiet_start and quiet_end".into());
    }
    let offset = policy_offset(&policy);
    match existing {
        Some(job) => recurring::update_job(root, &job.id, |job| {
            let was_enabled = job.enabled;
            if let Some(s) = schedule {
                if s != job.schedule {
                    job.next_run_at = s.next_after_in(now, offset);
                }
                job.schedule = s;
            }
            if let Some(b) = brief {
                job.prompt = b;
            }
            if let Some(t) = spec.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
                job.title = t;
            }
            if let Some(enabled) = spec.enabled {
                job.enabled = enabled;
                if enabled && !was_enabled && job.next_run_at <= now {
                    job.next_run_at = job.schedule.next_after_in(now, offset);
                }
                if !enabled {
                    job.queued = false;
                }
            }
            job.session_id = session_for(&owner, kind);
            job.wake = Some(policy);
        }),
        None => {
            let schedule = schedule.ok_or("schedule is required, e.g. `every 30m`, `daily 09:00`, `weekdays 09:00`")?;
            let brief = brief.filter(|b| !b.is_empty()).ok_or("brief is required: what to do on each wake")?;
            let title = spec
                .title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("Wake: {owner}"));
            let job = recurring::create_job_with(
                root,
                &title,
                &session_for(&owner, kind),
                &brief,
                schedule.clone(),
                None,
                Delivery::Goal,
            )?;
            recurring::update_job(root, &job.id, |job| {
                job.next_run_at = schedule.next_after_in(now, offset);
                job.enabled = spec.enabled.unwrap_or(true);
                job.wake = Some(policy);
            })
        }
    }
}

pub fn policy_offset(policy: &WakePolicy) -> chrono::FixedOffset {
    crate::mission_autonomy::offset_or_local(
        policy.utc_offset_minutes.or_else(crate::mission_autonomy::user_offset_minutes),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Fire,
    Defer { until: u64, reason: &'static str },
}

/// Whether a due wake may fire now: quiet hours and the daily cap defer it.
pub fn gate(policy: &WakePolicy, offset: chrono::FixedOffset, now: u64, runs_last_day: u32, oldest_in_day: Option<u64>) -> Gate {
    let at = chrono::DateTime::from_timestamp(now as i64, 0).unwrap_or_default();
    if let Some(end) = crate::mission_autonomy::quiet_ends_after(
        policy.quiet_start.as_deref(),
        policy.quiet_end.as_deref(),
        offset,
        at,
    ) {
        return Gate::Defer { until: end.timestamp() as u64, reason: "quiet hours" };
    }
    if runs_last_day >= policy.max_runs_per_day {
        let until = oldest_in_day.map_or(now + 3600, |t| t + 86_400).max(now + 60);
        return Gate::Defer { until, reason: "daily run cap" };
    }
    Gate::Fire
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub fn is_no_change(summary: &str, last_result: Option<&str>) -> bool {
    let s = summary.trim();
    s.starts_with(NO_CHANGE) || last_result.is_some_and(|last| normalise(last) == normalise(s))
}

/// Where a finished run's result goes; None keeps it to the run log.
pub fn route(policy: &ReportPolicy, status: &str, summary: &str, last_result: Option<&str>) -> Option<ReportTo> {
    if policy.to == ReportTo::None {
        return None;
    }
    if status == "completed" && policy.only_on_change && is_no_change(summary, last_result) {
        return None;
    }
    Some(policy.to)
}

pub fn goal_addendum(policy: &WakePolicy, last_result: Option<&str>, last_run_at: Option<u64>) -> String {
    let dest = match policy.report.to {
        ReportTo::MissionControl => "Mission Control".to_string(),
        ReportTo::User => "the user as a notification".to_string(),
        ReportTo::TaskRoom => format!("task room {}", policy.report.task_id.as_deref().unwrap_or("?")),
        ReportTo::None => "the run log only".to_string(),
    };
    let mut out = format!("\n\n---\n\nThis is your scheduled wake. Your complete_goal summary goes to {dest}");
    if policy.report.only_on_change {
        out.push_str(&format!(", unless nothing changed: then start it with {NO_CHANGE}"));
    }
    out.push('.');
    if let (Some(last), Some(at)) = (last_result.filter(|l| !l.trim().is_empty()), last_run_at) {
        let when = chrono::DateTime::from_timestamp(at as i64, 0)
            .map(|t| t.with_timezone(&policy_offset(policy)).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        out.push_str(&format!("\nLast run ({when}): {}", last.trim()));
    }
    out
}

pub fn view(job: &RecurringJob, runs_last_day: u32, last_status: Option<&str>) -> Value {
    let wake = job.wake.as_ref();
    json!({
        "owner": wake.map(|w| w.owner.as_str()),
        "owner_kind": wake.map(|w| w.owner_kind),
        "job_id": job.id,
        "session_id": job.session_id,
        "title": job.title,
        "brief": job.prompt,
        "schedule": job.schedule,
        "schedule_text": job.schedule.display(),
        "enabled": job.enabled,
        "quiet_hours": wake.and_then(|w| match (&w.quiet_start, &w.quiet_end) {
            (Some(start), Some(end)) => Some(json!({"start": start, "end": end})),
            _ => None,
        }),
        "utc_offset_minutes": wake.and_then(|w| w.utc_offset_minutes),
        "report": wake.map(|w| &w.report),
        "set_by": wake.map(|w| w.set_by),
        "max_runs_per_day": wake.map(|w| w.max_runs_per_day),
        "runs_last_day": runs_last_day,
        "next_run_at": job.enabled.then_some(job.next_run_at),
        "last_run_at": job.last_run_at,
        "last_status": last_status,
        "last_result": job.last_result,
        "last_error": job.last_error,
        "queued": job.queued,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })
}

pub fn view_with_runs(root: &Path, job: &RecurringJob, now: u64) -> Value {
    let store = recurring::store_at(root).ok();
    let runs = store
        .as_ref()
        .and_then(|s| s.count_recurring_runs_since(&job.id, now as i64 - 86_400).ok())
        .unwrap_or(0);
    let last = store
        .as_ref()
        .and_then(|s| s.list_recurring_runs(Some(&job.id), None, 1).ok())
        .and_then(|r| r.into_iter().next());
    view(job, runs, last.as_ref().map(|r| r.status.as_str()))
}

pub fn limits() -> Value {
    json!({
        "min_interval_secs": {"user": recurring::MIN_INTERVAL_SECS, "agent": MIN_AGENT_INTERVAL_SECS},
        "default_max_runs_per_day": DEFAULT_MAX_RUNS_PER_DAY,
        "max_runs_per_day": {"user": MAX_RUNS_PER_DAY_CEILING, "agent": DEFAULT_MAX_RUNS_PER_DAY},
        "max_set_by_mission_control": MAX_SET_BY_MISSION_CONTROL,
    })
}

#[derive(Deserialize)]
struct WakeToolArgs {
    action: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    brief: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    quiet_start: Option<String>,
    #[serde(default)]
    quiet_end: Option<String>,
    #[serde(default)]
    report_to: Option<ReportTo>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    only_on_change: Option<bool>,
}

pub struct WakeScheduleTool;

#[async_trait]
impl Tool for WakeScheduleTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "wake_schedule".into(),
            description: "Show, set or clear a recurring wake: on each one you get the brief as a goal and your complete_goal summary is reported (to Mission Control by default, only when something changed). Schedule: `every 30m` (15m minimum), `daily 09:00`, `weekdays 09:00`, `mon,thu 18:00`, in the user's time. Mission Control may pass agent_id to manage another agent's.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["show", "set", "clear"]},
                    "agent_id": {"type": "string", "description": "Mission Control only: the agent or session to manage."},
                    "schedule": {"type": "string"},
                    "brief": {"type": "string", "description": "What to do on each wake."},
                    "enabled": {"type": "boolean"},
                    "quiet_start": {"type": "string", "description": "HH:MM; empty clears."},
                    "quiet_end": {"type": "string", "description": "HH:MM; empty clears."},
                    "report_to": {"type": "string", "enum": ["mission_control", "user", "task_room", "none"]},
                    "task_id": {"type": "string", "description": "For report_to task_room."},
                    "only_on_change": {"type": "boolean", "description": "Default true."}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WakeToolArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let named = args.agent_id.as_deref().map(str::trim).filter(|a| !a.is_empty());
        let (owner, setter) = match (ctx.is_mission_control(), named) {
            (true, Some(other)) => (other.to_string(), Setter::MissionControl),
            (true, None) => return Err(ToolError::msg("pass agent_id; your own wakes are automated wakes and schedule_wake")),
            (false, Some(_)) => return Err(ToolError::msg("only Mission Control can set another agent's wake")),
            (false, None) => {
                let own = ctx
                    .agent_id()
                    .filter(|a| *a != crate::coordination::SNIPPET_AGENT_ID)
                    .or(ctx.durable_session_id())
                    .ok_or_else(|| ToolError::msg("this session has no durable identity to wake"))?;
                (own.to_string(), Setter::Owner)
            }
        };
        let root = recurring::default_root();
        let now = chrono::Utc::now().timestamp() as u64;
        match args.action.as_str() {
            "show" => Ok(ToolResult::success(match get(&root, &owner).map_err(ToolError::msg)? {
                Some(job) => view_with_runs(&root, &job, now),
                None => json!({"owner": owner, "schedule": null}),
            })),
            "clear" => {
                if setter == Setter::MissionControl
                    && get(&root, &owner)
                        .map_err(ToolError::msg)?
                        .and_then(|j| j.wake)
                        .is_some_and(|w| w.set_by == Setter::User)
                {
                    return Err(ToolError::msg("the user set this schedule; ask them to change it"));
                }
                let cleared = clear(&root, &owner).map_err(ToolError::msg)?;
                Ok(ToolResult::success(json!({"owner": owner, "cleared": cleared})))
            }
            "set" => {
                let store = recurring::store_at(&root).map_err(ToolError::msg)?;
                let kind = resolve_owner(&store, &owner).map_err(ToolError::msg)?;
                let report = if args.report_to.is_some() || args.task_id.is_some() || args.only_on_change.is_some() {
                    let current = get(&root, &owner).ok().flatten().and_then(|j| j.wake).map(|w| w.report).unwrap_or_default();
                    Some(ReportPolicy {
                        to: args.report_to.unwrap_or(current.to),
                        task_id: args.task_id.or(current.task_id),
                        only_on_change: args.only_on_change.unwrap_or(current.only_on_change),
                    })
                } else {
                    None
                };
                let spec = WakeSpec {
                    schedule: args.schedule,
                    brief: args.brief,
                    enabled: args.enabled,
                    quiet_start: args.quiet_start,
                    quiet_end: args.quiet_end,
                    report,
                    ..WakeSpec::default()
                };
                let job = set(&root, &owner, kind, spec, setter, now).map_err(ToolError::msg)?;
                Ok(ToolResult::success(view_with_runs(&root, &job, now)))
            }
            other => Err(ToolError::msg(format!("unknown action `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_793_451_600; // 2026-10-31 13:00 UTC, 18:30 at +05:30

    fn root() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn spec(schedule: &str) -> WakeSpec {
        WakeSpec {
            schedule: Some(schedule.into()),
            brief: Some("Check the X mentions and summarise anything new".into()),
            utc_offset_minutes: Some(330),
            ..WakeSpec::default()
        }
    }

    fn ist() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(330 * 60).unwrap()
    }

    #[test]
    fn set_creates_one_schedule_per_owner_and_persists() {
        let r = root();
        let job = set(r.path(), "jev", OwnerKind::Agent, spec("every 30m"), Setter::Owner, T0).unwrap();
        assert_eq!(job.session_id, "inbox-jev");
        assert_eq!(job.next_run_at, T0 + 1800);
        assert!(job.enabled);
        let wake = job.wake.clone().unwrap();
        assert_eq!(wake.report, ReportPolicy::default());
        assert!(wake.report.only_on_change);

        let again = set(
            r.path(),
            "jev",
            OwnerKind::Agent,
            WakeSpec { brief: Some("New brief".into()), ..WakeSpec::default() },
            Setter::Owner,
            T0 + 60,
        )
        .unwrap();
        assert_eq!(again.id, job.id);
        assert_eq!(again.prompt, "New brief");
        assert_eq!(again.next_run_at, T0 + 1800, "unchanged schedule keeps its next run");
        assert_eq!(list(r.path()).unwrap().len(), 1);

        let reread = get(r.path(), "jev").unwrap().unwrap();
        assert_eq!(reread.wake, again.wake);
        assert!(clear(r.path(), "jev").unwrap());
        assert!(get(r.path(), "jev").unwrap().is_none());
        assert!(!clear(r.path(), "jev").unwrap());
    }

    #[test]
    fn plain_recurring_jobs_are_not_wake_schedules() {
        let r = root();
        recurring::create_job(r.path(), "sync", "s1", "do it", Schedule::Interval { every_secs: 3600 }, None).unwrap();
        assert!(list(r.path()).unwrap().is_empty());
    }

    #[test]
    fn minimum_interval_depends_on_who_sets_it() {
        let r = root();
        let err = set(r.path(), "jev", OwnerKind::Agent, spec("every 10m"), Setter::Owner, T0).unwrap_err();
        assert!(err.contains("15 minutes"), "{err}");
        let err = set(r.path(), "jev", OwnerKind::Agent, spec("every 10m"), Setter::MissionControl, T0).unwrap_err();
        assert!(err.contains("15 minutes"), "{err}");
        assert!(set(r.path(), "jev", OwnerKind::Agent, spec("every 10m"), Setter::User, T0).is_ok());
    }

    #[test]
    fn mission_control_is_capped_and_cannot_override_the_user() {
        let r = root();
        for i in 0..MAX_SET_BY_MISSION_CONTROL {
            set(r.path(), &format!("a{i}"), OwnerKind::Agent, spec("every 1h"), Setter::MissionControl, T0).unwrap();
        }
        let err = set(r.path(), "one-more", OwnerKind::Agent, spec("every 1h"), Setter::MissionControl, T0).unwrap_err();
        assert!(err.contains("clear one first"), "{err}");
        assert!(set(r.path(), "a0", OwnerKind::Agent, spec("every 2h"), Setter::MissionControl, T0).is_ok());

        set(r.path(), "mine", OwnerKind::Agent, spec("every 1h"), Setter::User, T0).unwrap();
        let err = set(r.path(), "mine", OwnerKind::Agent, spec("every 2h"), Setter::MissionControl, T0).unwrap_err();
        assert!(err.contains("user set"), "{err}");
    }

    #[test]
    fn run_cap_has_a_lower_ceiling_for_agents() {
        let r = root();
        let mut s = spec("every 1h");
        s.max_runs_per_day = Some(100);
        assert!(set(r.path(), "jev", OwnerKind::Agent, s.clone(), Setter::Owner, T0).is_err());
        assert_eq!(set(r.path(), "jev", OwnerKind::Agent, s, Setter::User, T0).unwrap().wake.unwrap().max_runs_per_day, 100);
    }

    #[test]
    fn validation_errors() {
        let r = root();
        let mut s = spec("every 1h");
        s.quiet_start = Some("22:00".into());
        assert!(set(r.path(), "x", OwnerKind::Agent, s.clone(), Setter::User, T0).unwrap_err().contains("both"));
        s.quiet_end = Some("8am".into());
        assert!(set(r.path(), "x", OwnerKind::Agent, s, Setter::User, T0).unwrap_err().contains("HH:MM"));
        let mut s = spec("every 1h");
        s.report = Some(ReportPolicy { to: ReportTo::TaskRoom, task_id: None, only_on_change: true });
        assert!(set(r.path(), "x", OwnerKind::Agent, s, Setter::User, T0).unwrap_err().contains("task_id"));
        let s = WakeSpec { schedule: Some("every 1h".into()), ..WakeSpec::default() };
        assert!(set(r.path(), "x", OwnerKind::Agent, s, Setter::User, T0).unwrap_err().contains("brief"));
    }

    #[test]
    fn daily_schedule_uses_the_owners_timezone() {
        let r = root();
        let job = set(r.path(), "jev", OwnerKind::Agent, spec("daily 09:00"), Setter::User, T0).unwrap();
        // 09:00 at +05:30 on 1 Nov is 03:30 UTC.
        assert_eq!(job.next_run_at, 1_793_503_800);
    }

    fn policy(quiet: Option<(&str, &str)>, cap: u32) -> WakePolicy {
        WakePolicy {
            owner: "jev".into(),
            owner_kind: OwnerKind::Agent,
            set_by: Setter::User,
            quiet_start: quiet.map(|q| q.0.into()),
            quiet_end: quiet.map(|q| q.1.into()),
            utc_offset_minutes: Some(330),
            report: ReportPolicy::default(),
            max_runs_per_day: cap,
        }
    }

    #[test]
    fn gate_defers_through_quiet_hours_in_the_owners_timezone() {
        let p = policy(Some(("22:00", "08:00")), 48);
        // 13:00 UTC is 18:30 IST: not quiet.
        assert_eq!(gate(&p, ist(), T0, 0, None), Gate::Fire);
        // 20:00 UTC is 01:30 IST: quiet until 08:00 IST = 02:30 UTC next day.
        let night = T0 + 7 * 3600;
        assert_eq!(
            gate(&p, ist(), night, 0, None),
            Gate::Defer { until: T0 + 13 * 3600 + 1800, reason: "quiet hours" }
        );
        // The same UTC instant is not quiet for a UTC owner (20:00 < 22:00).
        assert_eq!(gate(&p, chrono::FixedOffset::east_opt(0).unwrap(), night, 0, None), Gate::Fire);
    }

    #[test]
    fn gate_defers_at_the_daily_cap_until_the_oldest_run_ages_out() {
        let p = policy(None, 3);
        assert_eq!(gate(&p, ist(), T0, 2, Some(T0 - 3600)), Gate::Fire);
        assert_eq!(
            gate(&p, ist(), T0, 3, Some(T0 - 3600)),
            Gate::Defer { until: T0 - 3600 + 86_400, reason: "daily run cap" }
        );
    }

    #[test]
    fn report_routing_respects_only_on_change() {
        let p = ReportPolicy::default();
        assert_eq!(route(&p, "completed", "Two new mentions", Some("One mention")), Some(ReportTo::MissionControl));
        assert_eq!(route(&p, "completed", "NO_CHANGE: still quiet", None), None);
        assert_eq!(route(&p, "completed", "one  mention", Some("One mention")), None);
        assert_eq!(route(&p, "paused", "NO_CHANGE", None), Some(ReportTo::MissionControl));
        let always = ReportPolicy { only_on_change: false, ..ReportPolicy::default() };
        assert_eq!(route(&always, "completed", "NO_CHANGE", None), Some(ReportTo::MissionControl));
        let silent = ReportPolicy { to: ReportTo::None, ..ReportPolicy::default() };
        assert_eq!(route(&silent, "failed", "boom", None), None);
    }

    #[test]
    fn goal_text_carries_the_report_destination_and_last_result() {
        let r = root();
        let mut job = set(r.path(), "jev", OwnerKind::Agent, spec("every 1h"), Setter::User, T0).unwrap();
        job.last_result = Some("Three mentions, one needs a reply".into());
        job.last_run_at = Some(T0);
        let goal = job.render_goal(None).unwrap();
        assert!(goal.contains("Check the X mentions"));
        assert!(goal.contains("goes to Mission Control"));
        assert!(goal.contains(NO_CHANGE));
        assert!(goal.contains("Last run (2026-10-31 18:30): Three mentions"), "{goal}");
    }

    #[test]
    fn api_view_shape() {
        let r = root();
        let mut s = spec("weekdays 09:00");
        s.quiet_start = Some("22:00".into());
        s.quiet_end = Some("08:00".into());
        let job = set(r.path(), "jev", OwnerKind::Agent, s, Setter::User, T0).unwrap();
        let v = view(&job, 2, Some("completed"));
        for key in [
            "owner", "owner_kind", "job_id", "session_id", "title", "brief", "schedule", "schedule_text", "enabled",
            "quiet_hours", "utc_offset_minutes", "report", "set_by", "max_runs_per_day", "runs_last_day",
            "next_run_at", "last_run_at", "last_status", "last_result", "last_error", "queued", "created_at",
            "updated_at",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["owner_kind"], "agent");
        assert_eq!(v["set_by"], "user");
        assert_eq!(v["schedule"], json!({"kind": "weekly", "days": 31, "hour": 9, "minute": 0}));
        assert_eq!(v["schedule_text"], "weekdays 09:00");
        assert_eq!(v["quiet_hours"], json!({"start": "22:00", "end": "08:00"}));
        assert_eq!(v["report"], json!({"to": "mission_control", "task_id": null, "only_on_change": true}));
        assert_eq!(v["runs_last_day"], 2);
        assert_eq!(limits()["min_interval_secs"]["agent"], 900);
    }

    #[test]
    fn run_log_records_counts_and_interrupts() {
        let r = root();
        let job = set(r.path(), "jev", OwnerKind::Agent, spec("every 1h"), Setter::User, T0).unwrap();
        let store = recurring::store_at(r.path()).unwrap();
        let a = store.insert_recurring_run(&job.id, Some("jev"), &job.session_id, T0 as i64 - 90_000).unwrap();
        let b = store.insert_recurring_run(&job.id, Some("jev"), &job.session_id, T0 as i64 - 60).unwrap();
        store.finish_recurring_run(a, "completed", Some("ok"), Some("mission_control"), T0 as i64 - 89_000).unwrap();
        assert_eq!(store.count_recurring_runs_since(&job.id, T0 as i64 - 86_400).unwrap(), 1);
        assert_eq!(store.interrupt_running_recurring_runs(T0 as i64).unwrap(), 1);
        let runs = store.list_recurring_runs(None, Some("jev"), 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, b);
        assert_eq!(runs[0].status, "interrupted");
        assert_eq!(runs[1].reported_to.as_deref(), Some("mission_control"));
        let v = view_with_runs(r.path(), &job, T0);
        assert_eq!(v["last_status"], "interrupted");
    }

    #[test]
    fn old_recurring_table_gains_wake_columns() {
        let dir = root();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE recurring_jobs (id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, session_id TEXT NOT NULL,
                 prompt TEXT NOT NULL, plan_path TEXT, schedule_json TEXT NOT NULL, delivery TEXT NOT NULL,
                 enabled INTEGER NOT NULL, next_run_at INTEGER NOT NULL, last_run_at INTEGER, last_error TEXT,
                 queued INTEGER NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 INSERT INTO recurring_jobs VALUES ('j1','t','s','p',NULL,'{\"kind\":\"interval\",\"every_secs\":3600}','goal',1,0,NULL,NULL,0,0,0);",
            )
            .unwrap();
        }
        let store = crate::store::Store::open(&path).unwrap();
        let jobs = store.list_recurring_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].wake.is_none() && jobs[0].last_result.is_none());
    }
}
