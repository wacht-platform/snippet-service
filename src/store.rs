use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database path has no parent: {0}")]
    NoParent(PathBuf),
    #[error("database lock poisoned")]
    LockPoisoned,
    #[error("handoff record is malformed: {0}")]
    HandoffDecode(String),
    #[error("task {id} was already terminal ({status}); refusing to overwrite its result")]
    AlreadyTerminal { id: String, status: String },
    #[error("task {id} is {status}; only blocked or failed work can be retried")]
    NotRetryable { id: String, status: String },
    #[error("task status must be terminal, got {0}")]
    NotTerminal(String),
    #[error("could not encode session state: {0}")]
    ScalarEncode(String),
    #[error("no such table in this store: {0}")]
    UnknownTable(String),
}

/// Canonical location of the snippet database.
///
/// Top-level on purpose: this is the general-purpose store for the whole device
/// (coordination, sessions, conversations), not a file belonging to any one
/// subsystem. It used to live under `mission-control/` only because the
/// coordination plane grew out of that store.
pub fn default_db_path() -> PathBuf {
    crate::config::snippet_home().join("snippet.db")
}

fn cached_stores() -> &'static Mutex<HashMap<PathBuf, Store>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Store>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Shared SQLite connection for the local snippet store.
/// Mutations must be short and must never perform external work while locked.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    /// The file this store opened, or `:memory:` for a test store. Kept so a CLI
    /// can report where the data actually landed rather than re-deriving it.
    path: PathBuf,
}

impl Store {
    pub fn open_cached(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let cache = cached_stores();
        if let Some(store) = cache
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(&path)
            .cloned()
        {
            return Ok(store);
        }

        let opened = Self::open(&path)?;
        let mut cache = cache.lock().map_err(|_| StoreError::LockPoisoned)?;
        Ok(cache.entry(path).or_insert(opened).clone())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?;
        } else {
            return Err(StoreError::NoParent(path.to_path_buf()));
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
            path: path.to_path_buf(),
        })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?;
        Ok(f(&connection)?)
    }

    pub fn integrity_check(&self) -> Result<bool, StoreError> {
        self.with_connection(|connection| {
            let result: String =
                connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            Ok(result == "ok")
        })
    }

    /// Every table this store defines, alphabetical.
    ///
    /// `sqlite_%` internal tables are excluded: `sqlite_sequence` is an
    /// autoincrement bookkeeping row, not schema, and listing it as a table would
    /// misrepresent the shape.
    pub fn table_names(&self) -> Result<Vec<String>, StoreError> {
        self.with_connection(|connection| {
            let mut stmt = connection.prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect()
        })
    }

    /// Row count for one table. The name is validated against the real schema
    /// rather than interpolated on trust, so this cannot become an injection
    /// point when a caller passes a user-supplied name.
    pub fn table_row_count(&self, table: &str) -> Result<i64, StoreError> {
        if !self.table_names()?.iter().any(|name| name == table) {
            return Err(StoreError::UnknownTable(table.to_string()));
        }
        self.with_connection(|connection| {
            Ok(
                connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?,
            )
        })
    }

    /// Where this store's file lives, for a CLI that has to report it.
    pub fn path(&self) -> &Path {
        &self.path
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

/// Bring the database up to date: each domain ensures its own tables.
///
/// The store owns the CONNECTION, not the schema. Coordination and conversations
/// each declare their own DDL next to the code that uses it, so a table's shape
/// lives with its domain instead of accumulating here — which is how this file
/// came to look like the coordination plane owned the whole database.
///
/// Order matters only in that `agents` must exist before anything referencing it;
/// the domains are otherwise independent.
fn migrate(connection: &Connection) -> Result<(), rusqlite::Error> {
    crate::coordination::schema::ensure(connection)?;
    crate::conversations::ensure_schema(connection)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_open_reuses_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snippet.db");
        let first = Store::open_cached(&path).unwrap();
        let second = Store::open_cached(&path).unwrap();

        assert!(Arc::ptr_eq(&first.connection, &second.connection));
    }

    #[test]
    fn initializes_schema_and_integrity() {
        let db = Store::open_in_memory().unwrap();
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
