use rusqlite::{OptionalExtension, params};

use super::{CoordinationDb, CoordinationDbError};

/// Lifecycle of a task on the board.
///
/// Deliberately smaller than [`super::work::AssignmentStatus`]: a task is what a
/// HUMAN asked for, and the assignment states that matter to a worker (offered,
/// accepted, awaiting handoff) are not meaningful to the person who filed it.
/// Mission Control owns the decomposition; the task just moves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Blocked,
    Done,
    Cancelled,
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
}

impl Task {
    /// The message room for a task, derived from its id so the two can never
    /// drift. Every participant on the task — agents and the human — reads and
    /// posts to exactly this thread.
    pub fn thread_for(id: &str) -> String {
        format!("task:{id}")
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
    })
}

const TASK_COLUMNS: &str = "id, title, description, status, priority, created_by_kind,
     created_by_id, created_at, updated_at, completed_at, thread_id";

impl CoordinationDb {
    /// Create a task AND its message room in one transaction.
    ///
    /// The thread row is written here with `scope = 'task'` and the task id as
    /// `subject_id`, so a task room is distinguishable from the shared room. The
    /// `INSERT OR IGNORE` in `append_event` then no-ops on it, which is why that
    /// path needs no special case.
    pub fn create_task(&self, task: &Task) -> Result<(), CoordinationDbError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO tasks (id, title, description, status, priority,
                    created_by_kind, created_by_id, created_at, updated_at,
                    completed_at, thread_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10)",
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

    pub fn get_task(&self, id: &str) -> Result<Option<Task>, CoordinationDbError> {
        self.with_connection(|conn| {
            let mut stmt =
                conn.prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"))?;
            Ok(stmt.query_row(params![id], task_from_row).optional()?)
        })
    }

    /// Tasks in board order: status column order is left to the caller, but
    /// within it the highest priority wins and ties break oldest-first so a
    /// stable queue does not reshuffle between reads.
    pub fn list_tasks_page(
        &self,
        filter: &TaskFilter<'_>,
        after: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<Vec<Task>, CoordinationDbError> {
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
    ) -> Result<bool, CoordinationDbError> {
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
    ) -> Result<bool, CoordinationDbError> {
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

    pub fn link_tasks(&self, link: &TaskLink) -> Result<(), CoordinationDbError> {
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
    ) -> Result<bool, CoordinationDbError> {
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
    pub fn task_links(&self, task_id: &str) -> Result<Vec<TaskLink>, CoordinationDbError> {
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
    pub fn blockers_of(&self, task_id: &str) -> Result<Vec<String>, CoordinationDbError> {
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
    ) -> Result<(), CoordinationDbError> {
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
    ) -> Result<bool, CoordinationDbError> {
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
    pub fn list_task_agents(&self, task_id: &str) -> Result<Vec<TaskAgent>, CoordinationDbError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            title: format!("Task {id}"),
            description: "do the thing".into(),
            status: TaskStatus::Todo,
            priority: 0,
            created_by_kind: "human".into(),
            created_by_id: "local".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            completed_at: None,
            thread_id: Task::thread_for(id),
        }
    }

    fn db() -> CoordinationDb {
        CoordinationDb::open_in_memory().unwrap()
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
            max_concurrent_assignments: 1,
            version: 1,
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
