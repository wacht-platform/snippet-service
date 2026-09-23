use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

/// Lifecycle of a task on the board.
///
/// This is the ONE task vocabulary. It used to be two — the board spoke
/// `todo/in_progress/blocked/done/cancelled` while a parallel JSON store spoke
/// `pending/…/failed` — so a task's state depended on which store you asked.
/// The union is what a task actually goes through: `Todo` is filed-but-unstarted
/// (the dispatcher's queue), and `Failed` is how a worker reports a hard stop,
/// which the board previously had no way to express.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// Terminal states are finished: nothing dispatches them and nothing
    /// reopens them implicitly. `Blocked` is NOT terminal — it is waiting.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// How two tasks relate. `Blocks` is an ordering constraint the scheduler must
/// respect; `RelatesTo` is context a reader should see. Kept as one table with a
/// discriminator so the board can draw both without inferring intent from which
/// table a row happens to live in.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskLinkKind {
    Blocks,
    RelatesTo,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    /// Higher sorts first within a status column. 0 is the default.
    pub priority: i64,
    pub created_by_kind: String,
    pub created_by_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    /// The board thread that IS this task's message room.
    pub thread_id: String,
    // ---- dispatch state (folded in from the retired JSON store) ----
    /// The durable session this task is routed to. Empty until dispatched.
    pub session_id: String,
    /// Structured briefing for the successor, when the task was handed off.
    pub handoff: Option<TaskHandoff>,
    /// How the envelope should be delivered to the target session.
    pub handoff_mode: HandoffMode,
    /// The session allowed to report this task's outcome. Set at dispatch time,
    /// so `report_mission_task` is only honoured for the bound caller.
    pub reporting_session: Option<String>,
    /// Consecutive failed dispatch attempts; reset on successful delivery. At
    /// the ceiling the task parks as Blocked instead of spinning the loop.
    pub dispatch_failures: u32,
    /// Terminal outcome, when a worker reported one.
    pub result: Option<TaskResult>,
    /// Pending / undelivered notification markers for Mission Control.
    pub notifications: Vec<NotificationMarker>,
    /// Workspace paths this task is *currently* writing to, for ownership
    /// conflict detection.
    pub owned_paths: Vec<std::path::PathBuf>,
    /// Inference profile the target session should run on, when the dispatcher
    /// named one. A model is bound when a session's loop starts, so this is
    /// applied at dispatch — restarting a running session, not waiting for one.
    /// `None` leaves the session on whatever it already has.
    pub profile: Option<String>,
}

/// Structured handoff information passed into a task.
///
/// The briefing a task carries into the session that will do it. This is the
/// dispatched task carries.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskHandoff {
    pub description: String,
    pub paths: Vec<std::path::PathBuf>,
    pub context: std::collections::BTreeMap<String, String>,
}

/// How a dispatched task should be delivered to its target session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    /// The target session already holds the relevant context.
    #[default]
    Resume,
    /// The target lacks context; the handoff description is the full briefing.
    Fresh,
}

impl HandoffMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "resume" => Some(Self::Resume),
            "fresh" => Some(Self::Fresh),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fresh => "fresh",
        }
    }
}

/// Terminal outcome reported by the worker that ran the task.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskResult {
    pub summary: String,
    pub artifacts: Vec<std::path::PathBuf>,
    pub authoritative: bool,
}

/// A notification marker attached to a task, for Mission Control to read.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct NotificationMarker {
    pub target: String,
    pub kind: String,
    pub message: String,
    pub delivered: bool,
}

/// Create a task's initial dispatch state in one value, so a caller cannot
/// forget `handoff_mode` and leave delivery semantics unset.
impl Task {
    /// The message room for a task, derived from its id so the two can never
    /// drift. Every participant on the task — agents and the human — reads and
    /// posts to exactly this thread.
    pub fn thread_for(id: &str) -> String {
        format!("task:{id}")
    }

    /// A task as the human files it: unstarted and owned by nobody yet, but
    /// already pointed at the session that should do the work.
    ///
    /// The target session is REQUIRED. A task with no session can never be
    /// dispatched — the loop claims it, finds no destination, and parks it as
    /// Blocked after the failure ceiling — so accepting one would file dead
    /// weight instead of work. Constructed here rather than at each call site so
    /// a new field's default is decided once, next to the type it belongs to.
    pub fn filed_by_human(
        id: String,
        title: String,
        description: String,
        session_id: String,
        priority: i64,
        now: String,
    ) -> Self {
        Self {
            thread_id: Self::thread_for(&id),
            id,
            title,
            description,
            status: TaskStatus::Todo,
            priority,
            created_by_kind: "human".into(),
            created_by_id: "local".into(),
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
            session_id,
            handoff: None,
            // A human's description IS the briefing: the target session holds no
            // prior context about work someone just thought of, so the envelope
            // must be self-contained.
            handoff_mode: HandoffMode::Fresh,
            reporting_session: None,
            dispatch_failures: 0,
            result: None,
            notifications: Vec::new(),
            owned_paths: Vec::new(),
            profile: None,
        }
    }

    /// A task routed to a session by Mission Control or an agent tool. Unlike a
    /// human filing, this carries the target session, owned paths, and delivery
    /// semantics from the moment it exists.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatched_to(
        id: String,
        session_id: String,
        title: String,
        description: String,
        owned_paths: Vec<std::path::PathBuf>,
        handoff_mode: HandoffMode,
        created_by_kind: &str,
        created_by_id: &str,
        now: String,
    ) -> Self {
        Self {
            thread_id: Self::thread_for(&id),
            id,
            title,
            description,
            status: TaskStatus::Todo,
            priority: 0,
            created_by_kind: created_by_kind.to_string(),
            created_by_id: created_by_id.to_string(),
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
            session_id,
            handoff: None,
            handoff_mode,
            reporting_session: None,
            dispatch_failures: 0,
            result: None,
            notifications: Vec::new(),
            owned_paths,
            profile: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskLink {
    pub from_task_id: String,
    pub to_task_id: String,
    pub kind: TaskLinkKind,
    pub created_at: String,
}

/// One agent's membership on a task. `removed_at` is set instead of deleting the
/// row: the roster changes as Mission Control learns more about the work, and
/// "who worked on this" has to outlive the assignment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskAgent {
    pub task_id: String,
    pub agent_id: String,
    pub role: String,
    pub added_at: String,
    pub removed_at: Option<String>,
}

/// Optional narrowing for a task page. Applied in SQL, so a filtered page is
/// still filled to `limit` — filtering after paging would return short pages.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter<'a> {
    pub status: Option<TaskStatus>,
    pub agent_id: Option<&'a str>,
}

fn status_text(status: &TaskStatus) -> String {
    serde_json::to_string(status)
        .unwrap()
        .trim_matches('"')
        .to_string()
}

fn link_text(kind: &TaskLinkKind) -> String {
    serde_json::to_string(kind)
        .unwrap()
        .trim_matches('"')
        .to_string()
}

fn parse_status(value: String, column: usize) -> Result<TaskStatus, rusqlite::Error> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })
}

fn parse_link_kind(value: String, column: usize) -> Result<TaskLinkKind, rusqlite::Error> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })
}

/// Decode an optional JSON column. `NULL` is `None`; a malformed payload is a
/// conversion error rather than a silent default, so a corrupted row surfaces
/// instead of reading as "no handoff".
fn decode_optional_json<T: serde::de::DeserializeOwned>(
    raw: Option<String>,
    column: usize,
) -> Result<Option<T>, rusqlite::Error> {
    match raw {
        None => Ok(None),
        // Decode as `Option<T>`, not `T`: a written `None` is the JSON literal
        // `null`, and deserializing that into `T` is an error rather than `None`.
        Some(text) => serde_json::from_str::<Option<T>>(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        }),
    }
}

/// Decode a NOT NULL JSON column that has a default, so a row written before the
/// column existed still reads as an empty collection.
fn decode_json_vec<T: serde::de::DeserializeOwned>(
    raw: String,
    column: usize,
) -> Result<Vec<T>, rusqlite::Error> {
    serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn task_from_row(row: &rusqlite::Row<'_>) -> Result<Task, rusqlite::Error> {
    Ok(Task {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        status: parse_status(row.get(3)?, 3)?,
        priority: row.get(4)?,
        created_by_kind: row.get(5)?,
        created_by_id: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        completed_at: row.get(9)?,
        thread_id: row.get(10)?,
        session_id: row.get(11)?,
        handoff: decode_optional_json(row.get(12)?, 12)?,
        handoff_mode: HandoffMode::parse(&row.get::<_, String>(13)?).unwrap_or_default(),
        reporting_session: row.get(14)?,
        dispatch_failures: row.get::<_, i64>(15)? as u32,
        result: decode_optional_json(row.get(16)?, 16)?,
        notifications: decode_json_vec(row.get(17)?, 17)?,
        owned_paths: decode_json_vec(row.get(18)?, 18)?,
        profile: row.get(19)?,
    })
}

const TASK_COLUMNS: &str = "id, title, description, status, priority, created_by_kind,
     created_by_id, created_at, updated_at, completed_at, thread_id, session_id,
     handoff_json, handoff_mode, reporting_session, dispatch_failures, result_json,
     notifications_json, owned_paths_json, profile";

/// Persist every mutable column of a task. Takes the connection so it composes
/// into the read-modify-write transaction in `update_task_in` — a separate
/// connection would break the isolation that transaction exists to provide.
///
/// `created_at` and `created_by_*` are deliberately absent: a task's origin is
/// history, not state, and a rewrite must not be able to change it.
fn write_task(conn: &rusqlite::Connection, task: &Task) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE tasks SET
            title = ?2,
            description = ?3,
            status = ?4,
            priority = ?5,
            updated_at = ?6,
            completed_at = ?7,
            session_id = ?8,
            handoff_json = ?9,
            handoff_mode = ?10,
            reporting_session = ?11,
            dispatch_failures = ?12,
            result_json = ?13,
            notifications_json = ?14,
            owned_paths_json = ?15,
            profile = ?16
         WHERE id = ?1",
        params![
            task.id,
            task.title,
            task.description,
            status_text(&task.status),
            task.priority,
            task.updated_at,
            task.completed_at,
            task.session_id,
            serde_json::to_string(&task.handoff).unwrap_or_else(|_| "null".into()),
            task.handoff_mode.as_str(),
            task.reporting_session,
            task.dispatch_failures,
            serde_json::to_string(&task.result).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&task.notifications).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&task.owned_paths).unwrap_or_else(|_| "[]".into()),
            task.profile,
        ],
    )?;
    Ok(())
}

impl Store {
    /// Create a task AND its message room in one transaction.
    ///
    /// The thread row is written here with `scope = 'task'` and the task id as
    /// `subject_id`, so a task room is distinguishable from the shared room. The
    /// `INSERT OR IGNORE` in `append_event` then no-ops on it, which is why that
    /// path needs no special case.
    pub fn create_task(&self, task: &Task) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO tasks (id, title, description, status, priority,
                    created_by_kind, created_by_id, created_at, updated_at,
                    completed_at, thread_id, session_id, handoff_json, handoff_mode,
                    reporting_session, dispatch_failures, result_json,
                    notifications_json, owned_paths_json, profile)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                params![
                    task.id,
                    task.title,
                    task.description,
                    status_text(&task.status),
                    task.priority,
                    task.created_by_kind,
                    task.created_by_id,
                    task.created_at,
                    task.completed_at,
                    task.thread_id,
                    task.session_id,
                    serde_json::to_string(&task.handoff).unwrap_or_else(|_| "null".into()),
                    task.handoff_mode.as_str(),
                    task.reporting_session,
                    task.dispatch_failures,
                    serde_json::to_string(&task.result).unwrap_or_else(|_| "null".into()),
                    serde_json::to_string(&task.notifications).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&task.owned_paths).unwrap_or_else(|_| "[]".into()),
                    task.profile,
                ],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO board_threads
                 (id, scope, subject_id, title, created_at)
                 VALUES (?1, 'task', ?2, ?3, ?4)",
                params![task.thread_id, task.id, task.title, task.created_at],
            )?;
            // The creator is a participant from the start, so the room has an
            // audience the moment it exists.
            tx.execute(
                "INSERT OR REPLACE INTO board_participants (thread_id, actor_id, actor_kind)
                 VALUES (?1, ?2, ?3)",
                params![task.thread_id, task.created_by_id, task.created_by_kind],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"))?;
            Ok(stmt.query_row(params![id], task_from_row).optional()?)
        })
    }

    /// Every task matching a filter, in board order.
    ///
    /// The dispatcher's whole queue is `list_tasks(None, Some(Todo))`, and the
    /// session view is `list_tasks(Some(id), None)` — the two reads that used to
    /// walk a directory of JSON files.
    pub fn list_tasks(
        &self,
        session_id: Option<&str>,
        status: Option<&TaskStatus>,
    ) -> Result<Vec<Task>, StoreError> {
        let status = status.map(status_text);
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE (?1 IS NULL OR session_id = ?1)
                   AND (?2 IS NULL OR status = ?2)
                 ORDER BY priority DESC, created_at, id"
            ))?;
            let rows = stmt.query_map(params![session_id, status], task_from_row)?;
            rows.collect()
        })
    }

    /// Tasks whose title or description contains `query`, case-insensitively.
    pub fn find_tasks(
        &self,
        session_id: Option<&str>,
        query: &str,
    ) -> Result<Vec<Task>, StoreError> {
        let needle = format!("%{}%", query.to_lowercase());
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE (?1 IS NULL OR session_id = ?1)
                   AND (lower(title) LIKE ?2 OR lower(description) LIKE ?2)
                 ORDER BY priority DESC, created_at, id"
            ))?;
            let rows = stmt.query_map(params![session_id, needle], task_from_row)?;
            rows.collect()
        })
    }

    /// Read-modify-write one task in a single transaction, stamping `updated_at`.
    ///
    /// This is the ONE mutation path. The dispatcher, the REST handlers and the
    /// worker tool calls all funnel through it, so a concurrent notification or
    /// report can never be lost to a stale read — which is what the process-wide
    /// file lock existed to prevent when the store was JSON files.
    pub fn update_task_in(
        &self,
        id: &str,
        now: &str,
        f: impl FnOnce(&mut Task),
    ) -> Result<Task, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut task: Task = tx
                .query_row(
                    &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                    params![id],
                    task_from_row,
                )
                .optional()?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            f(&mut task);
            task.updated_at = now.to_string();
            write_task(&tx, &task)?;
            tx.commit()?;
            Ok(task)
        })
    }

    /// Atomically claim a `Todo` task for dispatch: flips it to `InProgress`
    /// exactly once, binding the reporting session.
    ///
    /// The load/check/write runs in one transaction, so concurrent dispatchers
    /// (the loop and a REST path) race through it and only one wins. Returning
    /// the task unchanged when it is already claimed lets the loser exit without
    /// delivering a second envelope.
    pub fn claim_task_for_dispatch(
        &self,
        id: &str,
        now: &str,
    ) -> Result<Task, StoreError> {
        self.update_task_in(id, now, |task| {
            if task.status != TaskStatus::Todo {
                return;
            }
            task.status = TaskStatus::InProgress;
            task.reporting_session = Some(task.session_id.clone());
        })
    }

    /// Move a task to a terminal status with its result.
    ///
    /// Refuses a non-terminal status and refuses to overwrite an already-terminal
    /// task, so a second completion cannot flip a `Done` task to `Failed` or
    /// replace its result.
    pub fn complete_task(
        &self,
        id: &str,
        status: TaskStatus,
        result: TaskResult,
        now: &str,
    ) -> Result<Task, StoreError> {
        if !status.is_terminal() {
            return Err(StoreError::NotTerminal(format!("{status:?}")));
        }
        let kind = match status {
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            _ => "info",
        };
        let mut already_terminal = None;
        let task = self.update_task_in(id, now, |task| {
            if task.status.is_terminal() {
                already_terminal = Some(task.status.clone());
                return;
            }
            task.status = status.clone();
            task.result = Some(result.clone());
            task.owned_paths.clear();
            task.reporting_session = None;
            task.notifications.push(NotificationMarker {
                target: "mission_control".into(),
                kind: kind.into(),
                message: result.summary.clone(),
                delivered: false,
            });
        })?;
        if let Some(existing) = already_terminal {
            return Err(StoreError::AlreadyTerminal {
                id: id.to_string(),
                status: format!("{existing:?}"),
            });
        }
        Ok(task)
    }

    /// Re-queue a blocked or failed task so dispatch can deliver it again.
    ///
    /// Refuses `Done`/`Cancelled` (finished) and `InProgress` (still delivered to
    /// a worker) — only work that is genuinely waiting can be retried.
    pub fn retry_task(&self, id: &str, now: &str) -> Result<Task, StoreError> {
        let mut refused = None;
        let task = self.update_task_in(id, now, |task| {
            if matches!(
                task.status,
                TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::InProgress
            ) {
                refused = Some(task.status.clone());
                return;
            }
            task.status = TaskStatus::Todo;
            task.dispatch_failures = 0;
            task.reporting_session = None;
            task.owned_paths.clear();
            task.notifications.push(NotificationMarker {
                target: "mission_control".into(),
                kind: "info".into(),
                message: "re-queued after temporary failure".into(),
                delivered: true,
            });
        })?;
        if let Some(status) = refused {
            return Err(StoreError::NotRetryable {
                id: id.to_string(),
                status: format!("{status:?}"),
            });
        }
        Ok(task)
    }

    /// Return every `Blocked` task whose dependencies are all terminal back to
    /// `Todo`, so the dispatch loop can pick them up. Returns the ids re-queued.
    ///
    /// Tasks parked by the dispatch retry ceiling are excluded: they are blocked
    /// with no dependencies, and re-queuing them would spin the loop forever.
    /// A user resets those explicitly.
    pub fn unblock_ready_tasks(
        &self,
        now: &str,
    ) -> Result<Vec<String>, StoreError> {
        let blocked = self.list_tasks(None, Some(&TaskStatus::Blocked))?;
        let mut unblocked = Vec::new();
        for task in blocked {
            if task.dispatch_failures > 0 {
                continue;
            }
            let blockers = self.blockers_of(&task.id)?;
            if blockers.is_empty() {
                continue;
            }
            let all_done = blockers.iter().all(|id| {
                self.get_task(id)
                    .ok()
                    .flatten()
                    .is_some_and(|other| other.status.is_terminal())
            });
            if !all_done {
                continue;
            }
            let updated = self.update_task_in(&task.id, now, |t| {
                if t.status == TaskStatus::Blocked {
                    t.status = TaskStatus::Todo;
                }
            })?;
            if updated.status == TaskStatus::Todo {
                unblocked.push(task.id);
            }
        }
        Ok(unblocked)
    }

    /// Other in-progress tasks that already own any of `candidate_paths`.
    ///
    /// Only `InProgress` counts as ownership: a blocked or finished task has
    /// released its paths, so re-dispatching into them is safe.
    pub fn task_path_conflicts(
        &self,
        candidate_task_id: &str,
        candidate_paths: &[std::path::PathBuf],
    ) -> Result<Vec<(String, std::path::PathBuf)>, StoreError> {
        let active = self.list_tasks(None, Some(&TaskStatus::InProgress))?;
        let mut conflicts = Vec::new();
        for task in active {
            if task.id == candidate_task_id {
                continue;
            }
            for path in candidate_paths {
                if task.owned_paths.iter().any(|owned| owned == path) {
                    conflicts.push((task.id.clone(), path.clone()));
                }
            }
        }
        Ok(conflicts)
    }

    /// Tasks in board order: status column order is left to the caller, but
    /// within it the highest priority wins and ties break oldest-first so a
    /// stable queue does not reshuffle between reads.
    pub fn list_tasks_page(
        &self,
        filter: &TaskFilter<'_>,
        after: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<Vec<Task>, StoreError> {
        let (after_created, after_id) = match after {
            Some((created, id)) => (Some(created), Some(id)),
            None => (None, None),
        };
        let status = filter.status.as_ref().map(status_text);
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE (?1 IS NULL OR (created_at, id) > (?1, ?2))
                   AND (?3 IS NULL OR status = ?3)
                   AND (?4 IS NULL OR EXISTS (
                        SELECT 1 FROM task_agents ta
                        WHERE ta.task_id = tasks.id
                          AND ta.agent_id = ?4
                          AND ta.removed_at IS NULL))
                 ORDER BY priority DESC, created_at, id
                 LIMIT ?5"
            ))?;
            let rows = stmt.query_map(
                params![after_created, after_id, status, filter.agent_id, limit],
                task_from_row,
            )?;
            rows.collect()
        })
    }

    /// Patch the mutable fields. `None` leaves a field untouched, so a caller can
    /// change one thing without having to read-modify-write the whole row.
    pub fn update_task(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<&str>,
        priority: Option<i64>,
        now: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE tasks SET
                    title = COALESCE(?2, title),
                    description = COALESCE(?3, description),
                    priority = COALESCE(?4, priority),
                    updated_at = ?5
                 WHERE id = ?1",
                params![id, title, description, priority, now],
            )? == 1)
        })
    }

    /// Move a task between columns. `completed_at` is stamped on `Done` and
    /// cleared on any move out of it, so the timestamp always describes the
    /// current state rather than the last time it was ever done.
    pub fn set_task_status(
        &self,
        id: &str,
        status: &TaskStatus,
        now: &str,
    ) -> Result<bool, StoreError> {
        let completed = if *status == TaskStatus::Done {
            Some(now)
        } else {
            None
        };
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE tasks
                 SET status = ?2,
                     completed_at = ?3,
                     updated_at = ?4
                 WHERE id = ?1",
                params![id, status_text(status), completed, now],
            )? == 1)
        })
    }

    pub fn link_tasks(&self, link: &TaskLink) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO task_links (from_task_id, to_task_id, kind, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    link.from_task_id,
                    link.to_task_id,
                    link_text(&link.kind),
                    link.created_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn unlink_tasks(
        &self,
        from_task_id: &str,
        to_task_id: &str,
        kind: &TaskLinkKind,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "DELETE FROM task_links
                 WHERE from_task_id = ?1 AND to_task_id = ?2 AND kind = ?3",
                params![from_task_id, to_task_id, link_text(kind)],
            )? == 1)
        })
    }

    /// Every edge touching the task, in BOTH directions.
    ///
    /// An incoming `blocks` edge is as important as an outgoing one — it is what
    /// tells a reader this task is waiting on something. Returning only outgoing
    /// edges would render a blocked task as unblocked.
    pub fn task_links(&self, task_id: &str) -> Result<Vec<TaskLink>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT from_task_id, to_task_id, kind, created_at
                 FROM task_links
                 WHERE from_task_id = ?1 OR to_task_id = ?1
                 ORDER BY created_at, from_task_id, to_task_id",
            )?;
            let rows = stmt.query_map(params![task_id], |row| {
                Ok(TaskLink {
                    from_task_id: row.get(0)?,
                    to_task_id: row.get(1)?,
                    kind: parse_link_kind(row.get(2)?, 2)?,
                    created_at: row.get(3)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Tasks this one is blocked BY — the reverse of the stored edge.
    ///
    /// Stored as `A blocks B` and read as "B is blocked by A", so the query
    /// follows `to_task_id` and identifies the blockers by `from_task_id`.
    pub fn blockers_of(&self, task_id: &str) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT from_task_id FROM task_links
                 WHERE to_task_id = ?1 AND kind = 'blocks'
                 ORDER BY created_at, from_task_id",
            )?;
            let rows = stmt.query_map(params![task_id], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
    }

    /// Add an agent to the task and its message room.
    ///
    /// Re-adding an agent who was removed REVIVES the row rather than inserting a
    /// second one, so the primary key stays meaningful and a re-join does not
    /// lose the original `added_at`.
    pub fn add_task_agent(
        &self,
        task_id: &str,
        agent_id: &str,
        role: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let thread: Option<String> = tx
                .query_row(
                    "SELECT thread_id FROM tasks WHERE id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(thread_id) = thread else {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            };
            tx.execute(
                "INSERT INTO task_agents (task_id, agent_id, role, added_at, removed_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)
                 ON CONFLICT(task_id, agent_id) DO UPDATE SET
                     role = excluded.role,
                     removed_at = NULL",
                params![task_id, agent_id, role, now],
            )?;
            // Membership on the task IS membership in its room: an agent on the
            // task can always read and post to it.
            tx.execute(
                "INSERT OR REPLACE INTO board_participants (thread_id, actor_id, actor_kind)
                 VALUES (?1, ?2, 'agent')",
                params![thread_id, agent_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn remove_task_agent(
        &self,
        task_id: &str,
        agent_id: &str,
        now: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE task_agents SET removed_at = ?3
                 WHERE task_id = ?1 AND agent_id = ?2 AND removed_at IS NULL",
                params![task_id, agent_id, now],
            )? == 1)
        })
    }

    /// The task roster, active members first.
    ///
    /// Includes removed members so the UI can show who worked on it; `removed_at`
    /// is how a caller tells them apart.
    pub fn list_task_agents(&self, task_id: &str) -> Result<Vec<TaskAgent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT task_id, agent_id, role, added_at, removed_at
                 FROM task_agents
                 WHERE task_id = ?1
                 ORDER BY (removed_at IS NULL) DESC, added_at",
            )?;
            let rows = stmt.query_map(params![task_id], |row| {
                Ok(TaskAgent {
                    task_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    role: row.get(2)?,
                    added_at: row.get(3)?,
                    removed_at: row.get(4)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Park the work a failed session was doing, so it can be retried or
    /// reassigned instead of sitting InProgress forever.
    ///
    /// A dispatched worker session that dies without calling
    /// `report_mission_task` leaves its task looking active. This is the one
    /// place that notices, so the board does not quietly accumulate work that
    /// nothing is doing. The notification marker is deliberately left
    /// undelivered: it is what surfaces the failure to Mission Control.
    pub fn block_tasks_for_failed_session(
        &self,
        session_id: &str,
        detail: &str,
        now: &str,
    ) -> Result<usize, StoreError> {
        if session_id.is_empty() {
            return Ok(0);
        }
        let message = if detail.trim().is_empty() {
            "worker session failed without reporting; retry or reassign".to_string()
        } else {
            format!("worker session failed: {detail}")
        };
        let tasks = self.list_tasks(Some(session_id), Some(&TaskStatus::InProgress))?;
        let mut parked = 0usize;
        for task in tasks {
            // Only the task this session was actually reporting for. A session id
            // reused across tasks must not park work someone else is doing.
            if task.reporting_session.as_deref() != Some(session_id) {
                continue;
            }
            let id = task.id.clone();
            if self
                .update_task_in(&id, now, |t| {
                    if t.status != TaskStatus::InProgress {
                        return;
                    }
                    t.status = TaskStatus::Blocked;
                    t.reporting_session = None;
                    t.owned_paths.clear();
                    t.notifications.push(NotificationMarker {
                        target: "mission_control".into(),
                        kind: "blocked".into(),
                        message: message.clone(),
                        delivered: false,
                    });
                })
                .is_ok()
            {
                parked += 1;
            }
        }
        Ok(parked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task::filed_by_human(
            id.into(),
            format!("Task {id}"),
            "do the thing".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        )
    }

    fn db() -> Store {
        Store::open_in_memory().unwrap()
    }

    /// A task's profile must survive both write paths.
    ///
    /// The column is written by two separate statements (`create_task`'s INSERT
    /// and `write_task`'s UPDATE) and read by a third. A column added to only one
    /// of them round-trips as `None`, which would silently drop the model the
    /// dispatcher chose — the task would run on the session's existing model with
    /// nothing reported.
    #[test]
    fn a_tasks_profile_survives_create_and_update() {
        let db = db();

        // Absent by default: an ordinary task must not acquire a model.
        let plain = task("t-plain");
        assert_eq!(plain.profile, None);
        db.create_task(&plain).unwrap();
        assert_eq!(
            db.get_task("t-plain").unwrap().unwrap().profile,
            None,
            "a task filed without a profile must stay model-agnostic"
        );

        // Set at creation.
        let mut routed = task("t-routed");
        routed.profile = Some("xai".into());
        db.create_task(&routed).unwrap();
        assert_eq!(
            db.get_task("t-routed").unwrap().unwrap().profile.as_deref(),
            Some("xai"),
            "the INSERT must carry the profile"
        );

        // And preserved across an unrelated update, which rewrites every column.
        let updated = db
            .update_task_in("t-routed", "2026-01-02T00:00:00Z", |t| {
                t.title = "renamed".into();
            })
            .unwrap();
        assert_eq!(updated.profile.as_deref(), Some("xai"));
        assert_eq!(
            db.get_task("t-routed").unwrap().unwrap().profile.as_deref(),
            Some("xai"),
            "the UPDATE must carry the profile, not clear it"
        );
    }

    #[test]
    fn creating_a_task_creates_its_message_room() {
        let db = db();
        db.create_task(&task("t1")).unwrap();

        let thread: String = db
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT scope || '|' || COALESCE(subject_id,'') FROM board_threads
                     WHERE id = ?1",
                    params![Task::thread_for("t1")],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(thread, "task|t1", "the room must be scoped to the task");

        // The creator is a participant, so the room has an audience immediately.
        let participants: i64 = db
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM board_participants WHERE thread_id = ?1",
                    params![Task::thread_for("t1")],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(participants, 1);
    }

    #[test]
    fn a_task_room_survives_an_event_append() {
        // `append_event` also INSERT OR IGNOREs the thread row with scope
        // 'system'. This proves the task's own metadata is not overwritten.
        let db = db();
        db.create_task(&task("t1")).unwrap();
        db.append_event(&crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: Task::thread_for("t1"),
            partition_key: format!("thread:{}", Task::thread_for("t1")),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: "human".into(),
            actor_id: "local".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "hello"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k1".into(),
            created_at: "2026-01-01T00:00:01Z".into(),
        })
        .unwrap();

        let scope: String = db
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT scope FROM board_threads WHERE id = ?1",
                    params![Task::thread_for("t1")],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(scope, "task", "appending must not reset the room's scope");
    }

    #[test]
    fn blockers_are_read_from_the_reverse_edge() {
        let db = db();
        db.create_task(&task("a")).unwrap();
        db.create_task(&task("b")).unwrap();
        db.link_tasks(&TaskLink {
            from_task_id: "a".into(),
            to_task_id: "b".into(),
            kind: TaskLinkKind::Blocks,
            created_at: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();

        // "a blocks b" means b is blocked BY a.
        assert_eq!(db.blockers_of("b").unwrap(), vec!["a".to_string()]);
        assert!(db.blockers_of("a").unwrap().is_empty());
        // Both directions are visible from either end.
        assert_eq!(db.task_links("a").unwrap().len(), 1);
        assert_eq!(db.task_links("b").unwrap().len(), 1);
    }

    #[test]
    fn re_adding_an_agent_revives_the_roster_row() {
        let db = db();
        db.create_task(&task("t1")).unwrap();
        db.create_agent(&crate::coordination::types::Agent {
            id: "a1".into(),
            display_name: "Ada".into(),
            handle: "ada".into(),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            role: crate::coordination::types::AgentRole::Implementer,
            capabilities: vec![],
        })
        .unwrap();

        db.add_task_agent("t1", "a1", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();
        db.remove_task_agent("t1", "a1", "2026-01-01T00:01:00Z")
            .unwrap();
        assert_eq!(db.list_task_agents("t1").unwrap()[0].removed_at.is_some(), true);

        db.add_task_agent("t1", "a1", "reviewer", "2026-01-01T00:02:00Z")
            .unwrap();
        let roster = db.list_task_agents("t1").unwrap();
        assert_eq!(roster.len(), 1, "a re-join must revive, not duplicate");
        assert_eq!(roster[0].role, "reviewer");
        assert_eq!(roster[0].removed_at, None);
    }

    #[test]
    fn done_stamps_completion_and_leaving_done_clears_it() {
        let db = db();
        db.create_task(&task("t1")).unwrap();

        db.set_task_status("t1", &TaskStatus::Done, "2026-01-02T00:00:00Z")
            .unwrap();
        let done = db.get_task("t1").unwrap().unwrap();
        assert_eq!(done.completed_at.as_deref(), Some("2026-01-02T00:00:00Z"));

        db.set_task_status("t1", &TaskStatus::InProgress, "2026-01-03T00:00:00Z")
            .unwrap();
        let reopened = db.get_task("t1").unwrap().unwrap();
        assert_eq!(
            reopened.completed_at, None,
            "a reopened task must not keep a stale completion time"
        );
    }

    #[test]
    fn a_filtered_page_is_still_full() {
        let db = db();
        for i in 0..6 {
            let mut t = task(&format!("t{i}"));
            t.status = if i % 2 == 0 {
                TaskStatus::Todo
            } else {
                TaskStatus::Blocked
            };
            db.create_task(&t).unwrap();
        }
        // Filtering in SQL must fill the page; post-pagination filtering would
        // return 3 and look like the end of the list.
        let todos = db
            .list_tasks_page(
                &TaskFilter {
                    status: Some(TaskStatus::Todo),
                    ..Default::default()
                },
                None,
                10,
            )
            .unwrap();
        assert_eq!(todos.len(), 3);
        assert!(todos.iter().all(|t| t.status == TaskStatus::Todo));
    }
}
