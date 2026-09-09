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
        "PRAGMA user_version = 1;

         CREATE TABLE IF NOT EXISTS agents (
             id TEXT PRIMARY KEY NOT NULL,
             display_name TEXT NOT NULL,
             handle TEXT NOT NULL UNIQUE,
             kind TEXT NOT NULL,
             status TEXT NOT NULL,
             role TEXT NOT NULL,
             capabilities_json TEXT NOT NULL,
             max_concurrent_assignments INTEGER NOT NULL,
             max_concurrent_sessions INTEGER NOT NULL,
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

         CREATE TABLE IF NOT EXISTS schema_metadata (
             key TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL
         );
         INSERT OR IGNORE INTO schema_metadata(key, value)
             VALUES ('coordination_schema_version', '1');",
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
