use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, thiserror::Error)]
pub enum CoordinationDbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database path has no parent: {0}")]
    NoParent(PathBuf),
    #[error("database lock poisoned")]
    LockPoisoned,
    #[error("handoff record is malformed: {0}")]
    HandoffDecode(String),
}

/// Shared SQLite connection for the local coordination control plane.
/// Mutations must be short and must never perform external work while locked.
#[derive(Clone)]
pub struct CoordinationDb {
    connection: Arc<Mutex<Connection>>,
}

impl CoordinationDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoordinationDbError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CoordinationDbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
        } else {
            return Err(CoordinationDbError::NoParent(path.to_path_buf()));
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )?;
        configure(&connection)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn open_in_memory() -> Result<Self, CoordinationDbError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, CoordinationDbError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| CoordinationDbError::LockPoisoned)?;
        Ok(f(&connection)?)
    }

    pub fn integrity_check(&self) -> Result<bool, CoordinationDbError> {
        self.with_connection(|connection| {
            let result: String =
                connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            Ok(result == "ok")
        })
    }
}

fn configure(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA recursive_triggers = ON;",
    )
}

fn migrate(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "PRAGMA user_version = 2;

         CREATE TABLE IF NOT EXISTS agents (
             id TEXT PRIMARY KEY NOT NULL,
             display_name TEXT NOT NULL,
             handle TEXT NOT NULL UNIQUE,
             kind TEXT NOT NULL,
             status TEXT NOT NULL,
             role TEXT NOT NULL,
             capabilities_json TEXT NOT NULL,
             max_concurrent_assignments INTEGER NOT NULL,
             version INTEGER NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             last_heartbeat_at TEXT
         );

         CREATE TABLE IF NOT EXISTS assignments (
             id TEXT PRIMARY KEY NOT NULL,
             goal_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             agent_id TEXT NOT NULL REFERENCES agents(id),
             status TEXT NOT NULL,
             scope TEXT NOT NULL,
             definition_of_done TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS assignments_agent_status
             ON assignments(agent_id, status);
         CREATE INDEX IF NOT EXISTS assignments_session_status
             ON assignments(session_id, status);

         CREATE TABLE IF NOT EXISTS session_leases (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL,
             lease_id TEXT NOT NULL UNIQUE,
             assignment_id TEXT NOT NULL REFERENCES assignments(id),
             agent_id TEXT NOT NULL REFERENCES agents(id),
             fencing_token INTEGER NOT NULL,
             acquired_at TEXT NOT NULL,
             renewed_at TEXT NOT NULL,
             expires_at TEXT NOT NULL,
             released_at TEXT,
             release_reason TEXT
         );

         CREATE TABLE IF NOT EXISTS handoffs (
             id TEXT PRIMARY KEY NOT NULL,
             goal_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             source_assignment_id TEXT NOT NULL REFERENCES assignments(id),
             target_assignment_id TEXT NOT NULL REFERENCES assignments(id),
             context_mode TEXT NOT NULL,
             content_json TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             created_at TEXT NOT NULL,
             acknowledged_at TEXT
         );

         CREATE TABLE IF NOT EXISTS board_threads (
             id TEXT PRIMARY KEY NOT NULL,
             scope TEXT NOT NULL,
             subject_id TEXT,
             title TEXT NOT NULL,
             created_at TEXT NOT NULL,
             archived_at TEXT
         );

         CREATE TABLE IF NOT EXISTS board_participants (
             thread_id TEXT NOT NULL REFERENCES board_threads(id) ON DELETE CASCADE,
             actor_id TEXT NOT NULL,
             actor_kind TEXT NOT NULL,
             PRIMARY KEY (thread_id, actor_id)
         );

         CREATE TABLE IF NOT EXISTS board_events (
             event_id TEXT PRIMARY KEY NOT NULL,
             thread_id TEXT NOT NULL REFERENCES board_threads(id) ON DELETE CASCADE,
             partition_key TEXT NOT NULL,
             sequence INTEGER NOT NULL,
             event_type TEXT NOT NULL,
             actor_kind TEXT NOT NULL,
             actor_id TEXT NOT NULL,
             payload_version INTEGER NOT NULL,
             payload_json TEXT NOT NULL,
             causation_id TEXT,
             correlation_id TEXT,
             idempotency_key TEXT NOT NULL,
             created_at TEXT NOT NULL,
             UNIQUE (partition_key, sequence),
             UNIQUE (thread_id, actor_id, idempotency_key)
         );
         CREATE INDEX IF NOT EXISTS board_events_thread_sequence
             ON board_events(thread_id, sequence);

         CREATE TABLE IF NOT EXISTS outbox (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             event_id TEXT NOT NULL UNIQUE REFERENCES board_events(event_id) ON DELETE CASCADE,
             attempts INTEGER NOT NULL DEFAULT 0,
             available_at TEXT NOT NULL,
             delivered_at TEXT
         );

         -- The task board. A task is the unit of work a human creates; Mission
         -- Control decomposes it into assignments, and each task owns one board
         -- thread (its message room) so the agents on that task can talk without
         -- a global room every participant has to read.
         CREATE TABLE IF NOT EXISTS tasks (
             id TEXT PRIMARY KEY NOT NULL,
             title TEXT NOT NULL,
             description TEXT NOT NULL,
             status TEXT NOT NULL,
             priority INTEGER NOT NULL DEFAULT 0,
             created_by_kind TEXT NOT NULL,
             created_by_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             completed_at TEXT,
             -- The board thread that IS this task's message room. Derived from
             -- the id so it can never drift from the task it belongs to.
             thread_id TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS tasks_status_priority
             ON tasks(status, priority DESC, created_at);
         CREATE INDEX IF NOT EXISTS tasks_thread
             ON tasks(thread_id);

         -- Directed edges between tasks. `kind` distinguishes a blocks edge (an
         -- ordering constraint) from a relates_to edge (context), so the board
         -- can draw both without inferring intent.
         CREATE TABLE IF NOT EXISTS task_links (
             from_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
             to_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
             kind TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (from_task_id, to_task_id, kind)
         );
         CREATE INDEX IF NOT EXISTS task_links_to ON task_links(to_task_id, kind);

         -- Which agents are on a task, and what they own. Kept as a membership
         -- row with a removal timestamp rather than a hard delete: the record of
         -- who worked on a task outlives the assignment, and the roster is
         -- expected to change as Mission Control learns more about the work.
         CREATE TABLE IF NOT EXISTS task_agents (
             task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
             agent_id TEXT NOT NULL REFERENCES agents(id),
             role TEXT NOT NULL DEFAULT '',
             added_at TEXT NOT NULL,
             removed_at TEXT,
             PRIMARY KEY (task_id, agent_id)
         );
         CREATE INDEX IF NOT EXISTS task_agents_agent
             ON task_agents(agent_id, removed_at);

         CREATE TABLE IF NOT EXISTS schema_metadata (
             key TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL
         );
         INSERT OR IGNORE INTO schema_metadata(key, value)
             VALUES ('coordination_schema_version', '2');",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_schema_and_integrity() {
        let db = CoordinationDb::open_in_memory().unwrap();
        assert!(db.integrity_check().unwrap());
        db.with_connection(|connection| {
            let foreign_keys: i64 =
                connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
            assert_eq!(foreign_keys, 1);
            Ok(())
        })
        .unwrap();
    }
}
