//! Members and turns: who works a task from which conversation, and who did what
//! while holding the turn.

use rusqlite::{OptionalExtension, params};

use super::tasks::{TASK_COLUMNS, task_from_row};
use super::{Store, StoreError, Task, TaskAgent, TaskStatus};

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Turn {
    pub id: i64,
    pub agent_id: String,
    pub session_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    #[serde(skip)]
    pub snapshot: String,
    pub files: Vec<String>,
    pub commits: Vec<String>,
}

const TURN_COLUMNS: &str = "id, agent_id, session_id, started_at, ended_at, snapshot, files_json, commits_json";

fn turn_from_row(row: &rusqlite::Row<'_>) -> Result<Turn, rusqlite::Error> {
    let list = |raw: String| serde_json::from_str(&raw).unwrap_or_default();
    Ok(Turn {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        snapshot: row.get(5)?,
        files: list(row.get(6)?),
        commits: list(row.get(7)?),
    })
}

impl Store {
    pub fn open_turn(&self, task_id: &str, agent_id: &str, session_id: &str, snapshot: &str, now: &str) -> Result<i64, StoreError> {
        self.with_connection(|c| {
            c.execute(
                "INSERT INTO task_turns (task_id, agent_id, session_id, started_at, snapshot) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![task_id, agent_id, session_id, now, snapshot],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn close_turn(&self, id: i64, files: &[String], commits: &[String], now: &str) -> Result<(), StoreError> {
        let json = |list: &[String]| serde_json::to_string(list).unwrap_or_else(|_| "[]".into());
        self.with_connection(|c| {
            c.execute(
                "UPDATE task_turns SET ended_at = ?2, files_json = ?3, commits_json = ?4 WHERE id = ?1",
                params![id, now, json(files), json(commits)],
            )
        })?;
        Ok(())
    }

    pub fn open_turn_of(&self, task_id: &str) -> Result<Option<Turn>, StoreError> {
        self.with_connection(|c| {
            c.query_row(
                &format!("SELECT {TURN_COLUMNS} FROM task_turns WHERE task_id = ?1 AND ended_at IS NULL ORDER BY id DESC LIMIT 1"),
                params![task_id],
                turn_from_row,
            )
            .optional()
        })
    }

    /// Oldest first.
    pub fn list_turns(&self, task_id: &str) -> Result<Vec<Turn>, StoreError> {
        self.with_connection(|c| {
            let mut stmt = c.prepare(&format!("SELECT {TURN_COLUMNS} FROM task_turns WHERE task_id = ?1 ORDER BY id"))?;
            let rows = stmt.query_map(params![task_id], turn_from_row)?;
            rows.collect()
        })
    }

    pub fn set_member_session(&self, task_id: &str, agent_id: &str, session_id: &str) -> Result<(), StoreError> {
        self.with_connection(|c| {
            c.execute(
                "UPDATE task_agents SET work_session_id = ?3 WHERE task_id = ?1 AND agent_id = ?2 AND removed_at IS NULL",
                params![task_id, agent_id, session_id],
            )
        })?;
        Ok(())
    }

    /// The seat a conversation holds on an open task, if it holds one.
    pub fn member_by_session(&self, session_id: &str) -> Result<Option<TaskAgent>, StoreError> {
        self.with_connection(|c| {
            c.query_row(
                "SELECT ta.task_id, ta.agent_id, ta.work_session_id, ta.scope, ta.status, ta.role, ta.added_at, ta.removed_at
                 FROM task_agents ta JOIN tasks t ON t.id = ta.task_id
                 WHERE ta.work_session_id = ?1 AND ta.removed_at IS NULL
                   AND t.status NOT IN ('done', 'failed', 'cancelled')
                 ORDER BY t.created_at DESC LIMIT 1",
                params![session_id],
                |row| {
                    Ok(TaskAgent {
                        task_id: row.get(0)?,
                        agent_id: row.get(1)?,
                        work_session_id: row.get(2)?,
                        scope: row.get(3)?,
                        status: row.get(4)?,
                        role: row.get(5)?,
                        added_at: row.get(6)?,
                        removed_at: row.get(7)?,
                    })
                },
            )
            .optional()
        })
    }

    /// Tasks a conversation works: dispatched to it, or seated in it.
    pub fn tasks_worked_in(&self, session_id: &str, status: Option<&TaskStatus>) -> Result<Vec<Task>, StoreError> {
        let status = status.map(|s| s.as_str().to_string());
        self.with_connection(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE (session_id = ?1 OR reporting_session = ?1 OR id IN (
                        SELECT task_id FROM task_agents WHERE work_session_id = ?1 AND removed_at IS NULL))
                   AND (?2 IS NULL OR status = ?2)
                 ORDER BY priority DESC, created_at, id"
            ))?;
            let rows = stmt.query_map(params![session_id, status], task_from_row)?;
            rows.collect()
        })
    }
}
