//! Goals: the user's whole outcome as data. Tasks link to a goal by `goal_id`;
//! a goal closes only when none of its tasks is open and every acceptance
//! criterion carries evidence.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::tasks::{TASK_COLUMNS, task_from_row};
use super::{Store, StoreError, Task, TaskStatus, VerifyMethod};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Open,
    Closed,
    Cancelled,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(raw.trim().to_string())).ok()
    }
}

fn human() -> VerifyMethod {
    VerifyMethod::Human
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalCriterion {
    pub criterion: String,
    #[serde(default = "human")]
    pub method: VerifyMethod,
    /// What was seen or run to show the criterion holds; empty until then.
    #[serde(default)]
    pub evidence: String,
    /// Why this check no longer applies; a dropped check needs no evidence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dropped: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<GoalCriterion>,
    pub status: GoalStatus,
    pub requester: String,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub plan: serde_json::Value,
}

/// What keeps a goal moving between tasks. Held in the goal's plan JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Drive {
    pub next_step: String,
    pub waiting_on: String,
    pub waiting_since: String,
    pub due: String,
    pub last_nudged_at: String,
    pub nudges: u32,
    pub snoozed_until: String,
}

const DAY_SECS: i64 = 86_400;

fn epoch(stamp: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(stamp) {
        return Some(t.timestamp());
    }
    let date = chrono::NaiveDate::parse_from_str(stamp, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

impl Drive {
    pub fn due_soon(&self, now: i64) -> bool {
        epoch(&self.due).is_some_and(|due| due - now < 3 * DAY_SECS)
    }

    /// First nudge after a day of waiting, then every two days, every 12 hours near a due date.
    pub fn nudge_due(&self, now: i64) -> bool {
        if self.waiting_on.is_empty() || epoch(&self.snoozed_until).is_some_and(|until| until > now) {
            return false;
        }
        let gap = if self.due_soon(now) { DAY_SECS / 2 } else if self.nudges == 0 { DAY_SECS } else { 2 * DAY_SECS };
        let from = epoch(&self.last_nudged_at).or_else(|| epoch(&self.waiting_since));
        from.is_none_or(|from| now - from >= gap)
    }

    pub fn waiting_days(&self, now: i64) -> Option<i64> {
        epoch(&self.waiting_since).map(|since| (now - since) / DAY_SECS)
    }

    pub fn days_since_nudge(&self, now: i64) -> Option<i64> {
        epoch(&self.last_nudged_at).map(|at| (now - at) / DAY_SECS)
    }
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) }
}

impl Goal {
    pub fn drive(&self) -> Drive {
        serde_json::from_value(self.plan["drive"].clone()).unwrap_or_default()
    }

    pub fn set_drive(&mut self, drive: &Drive) {
        self.plan["drive"] = serde_json::json!(drive);
    }

    pub fn new(id: String, title: String, outcome: String, criteria: Vec<GoalCriterion>, requester: String, now: String) -> Self {
        Self {
            id,
            title,
            outcome,
            acceptance: criteria,
            status: GoalStatus::Open,
            requester,
            created_at: now,
            closed_at: None,
            plan: serde_json::json!({}),
        }
    }

    /// Record evidence for criteria named by number or by text. Names that match
    /// nothing come back, so a typo is not silently dropped.
    pub fn attach_evidence(&mut self, items: &[(String, String)]) -> Vec<String> {
        let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let mut unknown = Vec::new();
        for (key, text) in items {
            let k = key.trim().trim_start_matches('#');
            let number = k.parse::<usize>().ok();
            let hit = self.acceptance.iter().enumerate().position(|(i, c)| number == Some(i + 1) || norm(k) == norm(&c.criterion));
            match hit {
                Some(i) => self.acceptance[i].evidence = text.trim().to_string(),
                None => unknown.push(key.clone()),
            }
        }
        unknown
    }

    /// Replace the checks when the scope moves. A check that goes stays on record
    /// with the reason, so the owner can see what changed; evidence on unchanged
    /// text is kept.
    pub fn replace_acceptance(&mut self, new: Vec<GoalCriterion>, reason: &str) -> Result<(), String> {
        let reason = reason.trim();
        let (old_dropped, old): (Vec<_>, Vec<_>) = std::mem::take(&mut self.acceptance).into_iter().partition(|c| !c.dropped.is_empty());
        let gone: Vec<GoalCriterion> = old.iter().filter(|o| !new.iter().any(|n| n.criterion == o.criterion)).cloned().collect();
        if !gone.is_empty() && reason.is_empty() {
            self.acceptance = old.into_iter().chain(old_dropped).collect();
            return Err(format!("say why the scope changed: pass reason, since this removes {}", gone.iter().map(|c| clip(&c.criterion, 80)).collect::<Vec<_>>().join("; ")));
        }
        self.acceptance = new
            .into_iter()
            .map(|mut c| {
                c.dropped.clear();
                c.evidence = old.iter().find(|o| o.criterion == c.criterion).map(|o| o.evidence.clone()).unwrap_or_default();
                c
            })
            .collect();
        let kept_drops: Vec<GoalCriterion> = old_dropped.into_iter().filter(|d| !self.acceptance.iter().any(|c| c.criterion == d.criterion)).collect();
        self.acceptance.extend(kept_drops);
        self.acceptance.extend(gone.into_iter().map(|c| GoalCriterion { dropped: reason.to_string(), ..c }));
        Ok(())
    }

    /// Checks dropped along the way, as `text: reason`.
    pub fn dropped_checks(&self) -> Vec<String> {
        self.acceptance.iter().filter(|c| !c.dropped.is_empty()).map(|c| format!("{}: {}", clip(&c.criterion, 120), clip(&c.dropped, 200))).collect()
    }

    /// Criteria with no evidence, as `n. text`.
    pub fn unevidenced(&self) -> Vec<String> {
        self.acceptance
            .iter()
            .enumerate()
            .filter(|(_, c)| c.dropped.is_empty() && c.evidence.trim().is_empty())
            .map(|(i, c)| format!("{}. [{}] {}", i + 1, c.method.as_str(), clip(&c.criterion, 160)))
            .collect()
    }

    /// What stops the goal closing: open tasks and unevidenced criteria.
    /// A goal with no acceptance criteria cannot be closed either.
    pub fn close_gaps(&self, tasks: &[Task]) -> Vec<String> {
        let mut gaps: Vec<String> = tasks
            .iter()
            .filter(|t| is_open(t))
            .map(|t| format!("task {} \"{}\" is {}", t.id, clip(&t.title, 80), t.status))
            .collect();
        if self.acceptance.is_empty() {
            gaps.push("the goal has no acceptance criteria; add some with update_goal".into());
        }
        gaps.extend(self.unevidenced().into_iter().map(|c| format!("no evidence for criterion {c}")));
        gaps
    }

    pub fn fully_evidenced(&self) -> bool {
        !self.acceptance.is_empty() && self.unevidenced().is_empty()
    }
}

/// What stops a goal on the board needing its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Open, and no task is running, ready or in review.
    Stalled,
    /// Every criterion has evidence and no task is open, yet the goal is open.
    ReadyToClose,
    /// The next move is named as the user's.
    Waiting,
}

impl Attention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stalled => "stalled",
            Self::ReadyToClose => "ready_to_close",
            Self::Waiting => "waiting_on_user",
        }
    }
}

/// `(done, total)` slices, a dropped (cancelled) slice not counting.
pub fn progress(tasks: &[Task]) -> (usize, usize) {
    let live = tasks.iter().filter(|t| t.status != TaskStatus::Cancelled);
    (live.clone().filter(|t| t.status == TaskStatus::Done).count(), live.count())
}

pub fn attention(goal: &Goal, tasks: &[Task]) -> Option<Attention> {
    if goal.status != GoalStatus::Open {
        return None;
    }
    if goal.fully_evidenced() && !tasks.iter().any(is_open) {
        return Some(Attention::ReadyToClose);
    }
    if !goal.drive().waiting_on.is_empty() {
        return Some(Attention::Waiting);
    }
    let moving = tasks.iter().any(|t| matches!(t.status, TaskStatus::InProgress | TaskStatus::Todo | TaskStatus::InReview));
    (!moving).then_some(Attention::Stalled)
}

/// Failed counts as open: it needs a retry or a cancel, not silence.
pub fn is_open(task: &Task) -> bool {
    !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled)
}

/// Slice number of a task in the goal's plan (0-based, stable across replans).
pub fn slice_number(plan: &serde_json::Value, task_id: &str) -> Option<u64> {
    plan["slices"].as_array()?.iter().find(|s| s["task_id"] == task_id)?["n"].as_u64()
}

fn slice_state(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Done => "done",
        TaskStatus::InProgress => "in progress",
        TaskStatus::InReview => "in review",
        TaskStatus::Blocked => "waiting",
        TaskStatus::Todo => "ready",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "dropped",
    }
}

/// What a task's agent should know of the goal around it: the outcome, its own
/// slice, what each neighbour owns and whether it is done, and (when `full`) the
/// reports of finished neighbours its blockers do not already carry.
pub fn goal_block(store: &Store, task: &Task, full: bool) -> String {
    let Some(goal) = task.goal_id.as_deref().and_then(|id| store.get_goal(id).ok().flatten()) else {
        return String::new();
    };
    let siblings: Vec<Task> = store.goal_tasks(&goal.id).unwrap_or_default().into_iter().filter(|t| t.id != task.id).collect();
    let label = |t: &Task| match slice_number(&goal.plan, &t.id) {
        Some(n) => format!("slice {n}"),
        None => "task".to_string(),
    };
    let mut out = format!("[goal]\ngoal: {}: {}\n", clip(&goal.title, 120), clip(&goal.outcome, if full { 500 } else { 200 }));
    if full && !goal.acceptance.is_empty() {
        let checks: Vec<String> = goal.acceptance.iter().filter(|c| c.dropped.is_empty()).map(|c| clip(&c.criterion, 100)).collect();
        out.push_str(&format!("goal acceptance: {}\n", checks.join("; ")));
    }
    out.push_str(&format!("this is {}: {}\n", label(task), clip(&task.title, 100)));
    let others: Vec<&Task> = siblings.iter().filter(|t| t.status != TaskStatus::Cancelled).collect();
    if !others.is_empty() {
        out.push_str("other slices (each agent owns its own; do not redo theirs):\n");
        for t in &others {
            let agent = store
                .list_task_agents(&t.id)
                .ok()
                .and_then(|r| r.into_iter().find(|m| m.removed_at.is_none()))
                .map(|m| format!(", {}", m.agent_id))
                .unwrap_or_default();
            out.push_str(&format!("  - {} \"{}\" ({}{agent}): {}\n", label(t), clip(&t.title, 80), slice_state(&t.status), clip(&t.description, 100)));
        }
    }
    if full {
        let blockers = store.blockers_of(&task.id).unwrap_or_default();
        let reports: Vec<String> = others
            .iter()
            .filter(|t| t.status == TaskStatus::Done && !blockers.contains(&t.id))
            .filter_map(|t| t.result.as_ref().map(|r| format!("  - {} \"{}\": {}\n", label(t), clip(&t.title, 80), clip(&r.summary, 300))))
            .take(6)
            .collect();
        if !reports.is_empty() {
            out.push_str(&format!("reports from finished slices:\n{}", reports.concat()));
        }
    }
    out.push_str("[/goal]\n");
    out
}

const GOAL_COLUMNS: &str = "id, title, outcome, acceptance_json, status, requester, created_at, closed_at, plan_json";

fn goal_from_row(row: &rusqlite::Row<'_>) -> Result<Goal, rusqlite::Error> {
    let bad = |i: usize, e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    };
    let acceptance: String = row.get(3)?;
    let status: String = row.get(4)?;
    let plan: String = row.get(8)?;
    Ok(Goal {
        id: row.get(0)?,
        title: row.get(1)?,
        outcome: row.get(2)?,
        acceptance: serde_json::from_str(&acceptance).map_err(|e| bad(3, e))?,
        status: GoalStatus::parse(&status).unwrap_or(GoalStatus::Open),
        requester: row.get(5)?,
        created_at: row.get(6)?,
        closed_at: row.get(7)?,
        plan: serde_json::from_str(&plan).map_err(|e| bad(8, e))?,
    })
}

impl Store {
    pub fn create_goal(&self, goal: &Goal) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                &format!("INSERT INTO goals ({GOAL_COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
                params![
                    goal.id,
                    goal.title,
                    goal.outcome,
                    serde_json::to_string(&goal.acceptance).unwrap_or_else(|_| "[]".into()),
                    goal.status.as_str(),
                    goal.requester,
                    goal.created_at,
                    goal.closed_at,
                    goal.plan.to_string(),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_goal(&self, id: &str) -> Result<Option<Goal>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(&format!("SELECT {GOAL_COLUMNS} FROM goals WHERE id = ?1"), params![id], goal_from_row).optional()
        })
    }

    /// Every mutable column; the origin (`created_at`, `requester`) stays.
    pub fn save_goal(&self, goal: &Goal) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE goals SET title = ?2, outcome = ?3, acceptance_json = ?4, status = ?5,
                    closed_at = ?6, plan_json = ?7 WHERE id = ?1",
                params![
                    goal.id,
                    goal.title,
                    goal.outcome,
                    serde_json::to_string(&goal.acceptance).unwrap_or_else(|_| "[]".into()),
                    goal.status.as_str(),
                    goal.closed_at,
                    goal.plan.to_string(),
                ],
            )?;
            Ok(())
        })
    }

    /// Goals newest first; `None` lists every status.
    pub fn list_goals(&self, status: Option<GoalStatus>) -> Result<Vec<Goal>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {GOAL_COLUMNS} FROM goals WHERE (?1 IS NULL OR status = ?1) ORDER BY created_at DESC"
            ))?;
            let rows = stmt.query_map(params![status.map(GoalStatus::as_str)], goal_from_row)?;
            rows.collect()
        })
    }

    /// The tasks linked to a goal, oldest first.
    pub fn goal_tasks(&self, goal_id: &str) -> Result<Vec<Task>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE goal_id = ?1 ORDER BY created_at, rowid"
            ))?;
            let rows = stmt.query_map(params![goal_id], task_from_row)?;
            rows.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new(
            "g1".into(),
            "Ship it".into(),
            "Users can sign in".into(),
            vec![
                GoalCriterion { criterion: "tests pass".into(), method: VerifyMethod::Command, evidence: String::new(), dropped: String::new() },
                GoalCriterion { criterion: "login screen looks right".into(), method: VerifyMethod::Visual, evidence: String::new(), dropped: String::new() },
            ],
            "user".into(),
            "2026-01-01T00:00:00Z".into(),
        )
    }

    fn task(id: &str, goal: Option<&str>, status: TaskStatus) -> Task {
        let mut t = Task::filed_by_human(id.into(), format!("T {id}"), "do".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        t.goal_id = goal.map(str::to_string);
        t.status = status;
        t
    }

    #[test]
    fn goals_round_trip_and_list_by_status() {
        let db = Store::open_in_memory().unwrap();
        let mut g = goal();
        db.create_goal(&g).unwrap();
        assert_eq!(db.get_goal("g1").unwrap().unwrap(), g);
        g.status = GoalStatus::Closed;
        g.closed_at = Some("2026-01-02T00:00:00Z".into());
        db.save_goal(&g).unwrap();
        assert_eq!(db.list_goals(Some(GoalStatus::Open)).unwrap().len(), 0);
        assert_eq!(db.list_goals(None).unwrap().len(), 1);
        assert!(db.get_goal("nope").unwrap().is_none());
    }

    #[test]
    fn linked_tasks_come_back_by_goal() {
        let db = Store::open_in_memory().unwrap();
        db.create_goal(&goal()).unwrap();
        db.create_task(&task("a", Some("g1"), TaskStatus::Todo)).unwrap();
        db.create_task(&task("b", None, TaskStatus::Todo)).unwrap();
        let linked = db.goal_tasks("g1").unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].id, "a");
        assert_eq!(linked[0].goal_id.as_deref(), Some("g1"));
    }

    #[test]
    fn closing_is_refused_for_open_tasks_and_missing_evidence() {
        let mut g = goal();
        let tasks = [task("a", Some("g1"), TaskStatus::InProgress), task("b", Some("g1"), TaskStatus::Done), task("c", Some("g1"), TaskStatus::Failed)];
        let gaps = g.close_gaps(&tasks);
        assert_eq!(gaps.len(), 4, "{gaps:?}");
        assert!(gaps[0].contains("task a") && gaps[1].contains("task c"));
        let unknown = g.attach_evidence(&[("1".into(), "cargo test: ok".into()), ("Login Screen looks right".into(), "shot.png".into()), ("9".into(), "x".into())]);
        assert_eq!(unknown, ["9"]);
        assert!(g.fully_evidenced());
        assert_eq!(g.close_gaps(&tasks[1..2]), Vec::<String>::new());
    }

    #[test]
    fn a_goal_without_criteria_cannot_close() {
        let mut g = goal();
        g.acceptance.clear();
        assert!(g.close_gaps(&[]).iter().any(|g| g.contains("no acceptance criteria")));
    }

    #[test]
    fn an_older_database_gains_goal_id_without_losing_tasks() {
        let db = Store::open_in_memory().unwrap();
        db.create_task(&task("old", None, TaskStatus::Todo)).unwrap();
        db.with_connection(|c| {
            c.execute_batch("DROP INDEX tasks_goal; ALTER TABLE tasks DROP COLUMN goal_id;")?;
            crate::coordination::schema::ensure(c)
        })
        .unwrap();
        assert_eq!(db.get_task("old").unwrap().unwrap().goal_id, None);
    }

    #[test]
    fn attention_flags_stalled_and_ready_goals_only() {
        let mut g = goal();
        let t = |id: &str, st| task(id, Some("g1"), st);
        assert_eq!(attention(&g, &[]), Some(Attention::Stalled));
        assert_eq!(attention(&g, &[t("a", TaskStatus::Blocked), t("b", TaskStatus::Failed)]), Some(Attention::Stalled));
        for moving in [TaskStatus::InProgress, TaskStatus::Todo, TaskStatus::InReview] {
            assert_eq!(attention(&g, &[t("a", moving), t("b", TaskStatus::Blocked)]), None);
        }
        assert_eq!(attention(&g, &[t("a", TaskStatus::Done)]), Some(Attention::Stalled));
        g.attach_evidence(&[("1".into(), "ok".into()), ("2".into(), "ok".into())]);
        assert_eq!(attention(&g, &[t("a", TaskStatus::Done), t("b", TaskStatus::Cancelled)]), Some(Attention::ReadyToClose));
        assert_eq!(attention(&g, &[t("a", TaskStatus::InProgress)]), None);
        g.status = GoalStatus::Closed;
        assert_eq!(attention(&g, &[]), None);
        assert_eq!(progress(&[t("a", TaskStatus::Done), t("b", TaskStatus::Cancelled), t("c", TaskStatus::Todo)]), (1, 2));
    }
}
