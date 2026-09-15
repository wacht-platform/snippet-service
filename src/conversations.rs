//! Durable conversations in the shared store.
//!
//! A conversation used to be one gzip'd MessagePack blob rewritten in full on
//! every step — so each persist re-serialized and recompressed the entire
//! transcript, and the cost grew with the history. Here a transcript is
//! **append-only rows**: adding a turn inserts one row, and nothing reads or
//! rewrites the messages it did not touch.
//!
//! The session's scalar state (status, goal, counters, lanes, checkpoints) is
//! kept as one JSON column on the `sessions` row. Those fields are small and
//! genuinely mutate together; splitting them into columns would add schema
//! churn for no read that needs it.


use rusqlite::{OptionalExtension, params};

use crate::harness::HarnessEvent;
use crate::llm::HarnessMessage;
use crate::store::{Store, StoreError};

/// Schema for durable conversations.
///
/// The DDL lives with the data it describes. [`crate::store`] owns the
/// connection and calls each domain's `ensure`; keeping the session tables here
/// is what stops the store from reading like the coordination plane's private
/// database, which is exactly how the naming drifted.
///
/// Additive only — every statement is `IF NOT EXISTS`.
pub fn ensure_schema(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY NOT NULL,
             workspace_key TEXT NOT NULL,
             workspace TEXT NOT NULL,
             title TEXT,
             status TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             tags_json TEXT NOT NULL DEFAULT '[]',
             -- The session's scalar state (status, goal, counters, lanes,
             -- checkpoints, queue), WITHOUT its two append-only logs. Those are
             -- rows here, so this column is small and its rewrite is cheap.
             state_json TEXT NOT NULL DEFAULT '{}',
             -- Last *user* activity, unix seconds. Drives the session list's
             -- order. Was a `.meta.json` sidecar, so a migrated session had no
             -- value and fell back to a file mtime that no longer exists.
             last_active INTEGER,
             -- Per-conversation model override (profile name). Was `.profile`.
             profile TEXT,
             -- 'mission_control' | 'standard'. Was `.role`.
             role TEXT NOT NULL DEFAULT 'standard',
             -- The agent working this session, if any. Was `.role`'s agent_id.
             agent_id TEXT,
             -- The path-shaped id this row had before ids became opaque
             -- (e.g. `snipett-2a3f/state.json`). Kept so an in-flight reference,
             -- a stored message payload, or a client holding the old id still
             -- resolves — and so a rollback has something to map back to.
             legacy_id TEXT,
             -- What kind of session this is: 'default' | 'conversation' |
             -- 'inbox' | 'mission_control'. This used to be parsed out of the
             -- id's suffix, which is the coupling opaque ids remove.
             kind TEXT NOT NULL DEFAULT 'conversation'
         );
         CREATE INDEX IF NOT EXISTS sessions_workspace
             ON sessions(workspace_key, updated_at DESC);

         -- One row per message. A transcript is APPENDED to, never rewritten:
         -- the previous single-blob state file re-serialized and recompressed the
         -- whole conversation on every step, so its cost grew with history.
         CREATE TABLE IF NOT EXISTS session_messages (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             role TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, ordinal)
         );

         -- The display event log, append-only for the same reason as messages.
         CREATE TABLE IF NOT EXISTS session_events (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             payload_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, ordinal)
         );

         -- Client request ids accepted by the daemon. Kept independently of a
         -- session row so pre-migration file sessions also get replay protection.
         CREATE TABLE IF NOT EXISTS request_nonces (
             session_id TEXT NOT NULL,
             nonce TEXT NOT NULL,
             accepted_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, nonce)
         );"#,
    )?;
    ensure_session_id_columns(connection)?;
    rewrite_legacy_session_ids(connection)?;
    Ok(())
}

/// Add `legacy_id` / `kind` to a database created before ids were opaque.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing to a table that already exists, so
/// the columns have to be added explicitly — otherwise a real database would
/// keep the old shape and every insert would fail on the missing column.
fn ensure_session_id_columns(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    let existing: Vec<String> = {
        let mut stmt = connection.prepare("PRAGMA table_info(sessions)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<Result<_, _>>()?
    };
    if !existing.iter().any(|c| c == "legacy_id") {
        connection.execute("ALTER TABLE sessions ADD COLUMN legacy_id TEXT", [])?;
    }
    if !existing.iter().any(|c| c == "kind") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN kind TEXT NOT NULL DEFAULT 'conversation'",
            [],
        )?;
    }
    connection.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS sessions_legacy_id ON sessions(legacy_id)",
        [],
    )?;
    Ok(())
}

/// Rewrite every path-shaped session id to an opaque uuid, once.
///
/// The id used to BE a filesystem path, so it carried `/state.json` or
/// `/conversations/<uuid>.json`. Those files stopped being read when the store
/// took over, leaving a path-shaped string whose only surviving effect was to
/// leak a filename into prompts, URLs and ids a model then mangled. This keeps
/// the old value in `legacy_id` so an in-flight reference still resolves and a
/// rollback has something to map back to.
///
/// Idempotent: a row whose id is already opaque is left alone, so this is safe
/// on every open.
fn rewrite_legacy_session_ids(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    let rows: Vec<String> = {
        let mut stmt = connection.prepare("SELECT id FROM sessions")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };

    // Every table holding a session id as a plain string. `session_messages` and
    // `session_events` have a real FK, so they move inside the deferred-FK
    // transaction below; the rest cannot fail on a constraint.
    const CHILD_COLUMNS: [(&str, &str); 7] = [
        ("session_messages", "session_id"),
        ("session_events", "session_id"),
        ("request_nonces", "session_id"),
        ("tasks", "session_id"),
        ("tasks", "reporting_session"),
        ("agent_board", "session_id"),
        ("recurring_jobs", "session_id"),
    ];
    const JSON_COLUMNS: [(&str, &str, &str); 3] = [
        ("notification_journal", "payload_json", "session_id"),
        ("board_events", "payload_json", "origin_session"),
        ("board_events", "payload_json", "recipient"),
    ];

    let plan: Vec<(String, String, String)> = rows
        .into_iter()
        .filter_map(|old| {
            let (new, kind) = canonical_session_id(&old);
            (new != old).then_some((old, new, kind))
        })
        .collect();
    if plan.is_empty() {
        return Ok(());
    }
    eprintln!(
        "[store] canonicalising {} session id(s) (file suffix removed)",
        plan.len()
    );

    // A table only exists once its subsystem has been opened, so the migration
    // must not assume the full schema — a database created before coordination
    // (or a test using `ensure_schema` alone) simply has fewer tables to move.
    // Skipping an absent one is correct: there are no rows in it to fix.
    let present: std::collections::HashSet<String> = {
        let mut stmt =
            connection.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
        names.collect::<Result<_, _>>()?
    };
    let tables: Vec<(&str, &str)> = CHILD_COLUMNS
        .into_iter()
        .filter(|(t, _)| present.contains(*t))
        .collect();
    let json_tables: Vec<(&str, &str, &str)> = JSON_COLUMNS
        .into_iter()
        .filter(|(t, _, _)| present.contains(*t))
        .collect();

    // ONE transaction: `defer_foreign_keys` only applies inside one, and the
    // child tables hold a real FK to `sessions.id`.
    let tx = connection.unchecked_transaction()?;
    let connection = &tx;
    connection.execute("PRAGMA defer_foreign_keys = ON", [])?;
    for (old, new, kind) in &plan {
        // The canonical id may ALREADY have a row: a daemon running the previous
        // build keeps writing the pre-migration id, so after an earlier rename it
        // re-creates the old row and one session ends up with two. Renaming then
        // would collide on the primary key of the transcript tables and abort the
        // open — the store would not load at all. Merge instead.
        let taken: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            rusqlite::params![new],
            |r| r.get(0),
        )?;
        if taken {
            merge_duplicate_session(connection, old, new)?;
            continue;
        }
        for (table, col) in &tables {
            connection.execute(
                &format!("UPDATE {table} SET {col} = ?1 WHERE {col} = ?2"),
                rusqlite::params![new, old],
            )?;
        }
        for (table, col, key) in &json_tables {
            let patched: Vec<(i64, String)> = {
                let mut stmt =
                    connection.prepare(&format!("SELECT rowid, {col} FROM {table} WHERE {col} LIKE ?1"))?;
                let like = format!("%{old}%");
                stmt.query_map(rusqlite::params![like], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (rowid, raw) in patched {
                let next = raw.replace(
                    &format!("\"{key}\":\"{old}\""),
                    &format!("\"{key}\":\"{new}\""),
                );
                if next != raw {
                    connection.execute(
                        &format!("UPDATE {table} SET {col} = ?1 WHERE rowid = ?2"),
                        rusqlite::params![next, rowid],
                    )?;
                }
            }
        }
        connection.execute(
            "UPDATE sessions SET id = ?1, legacy_id = COALESCE(legacy_id, ?2), kind = ?3 WHERE id = ?2",
            rusqlite::params![new, old, kind],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Fold a legacy-shaped row into the canonical row that already holds its id.
///
/// Both rows describe one session: the canonical one was renamed from the legacy
/// id, and the legacy id was then re-created by a process still running the old
/// build. The transcript tables key on `(session_id, ordinal)`, so a plain rename
/// would collide.
///
/// For the two append-only logs the fuller row wins, because the rows are the
/// SAME conversation: the shorter log is its prefix (the legacy row was written
/// from the same history and then extended). Replacing the canonical copy with
/// the legacy copy therefore keeps every message once, in order, with no
/// duplication — where appending would repeat the whole shared prefix.
///
/// Every other table is keyed by its own id, so those rows simply move; a row
/// that would collide with an existing canonical row is dropped as a duplicate.
fn merge_duplicate_session(
    connection: &rusqlite::Connection,
    legacy: &str,
    canonical: &str,
) -> Result<(), rusqlite::Error> {
    // Same reason as the rename: a table exists only once its subsystem has been
    // opened, so a table that is absent has no rows to move.
    let present: std::collections::HashSet<String> = {
        let mut stmt =
            connection.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
        names.collect::<Result<_, _>>()?
    };
    // Transcripts are APPEND-ONLY and two colliding rows almost always share a
    // prefix — the stale row is one the previous build kept appending to. Even
    // so, take the UNION rather than the longer transcript: "longer" is only
    // correct while one is a strict prefix of the other, and a deleted message
    // is not recoverable. Rows unique to the shorter side are kept, renumbered
    // past the base's last ordinal so they cannot collide on the primary key.
    for (table, has_role) in [("session_messages", true), ("session_events", false)] {
        if !present.contains(table) {
            continue;
        }
        let count = |id: &str| -> Result<i64, rusqlite::Error> {
            connection.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                rusqlite::params![id],
                |r| r.get(0),
            )
        };
        let (canonical_len, legacy_len) = (count(canonical)?, count(legacy)?);
        // The longer side supplies the identity (and therefore the ordinals);
        // the shorter side contributes only what it alone holds.
        let (base, other) = if canonical_len >= legacy_len {
            (canonical, legacy)
        } else {
            (legacy, canonical)
        };

        let existing: std::collections::HashSet<String> = {
            let mut stmt = connection.prepare(&format!(
                "SELECT ordinal || '|' || payload_json FROM {table} WHERE session_id = ?1"
            ))?;
            let rows = stmt.query_map(rusqlite::params![base], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut next: i64 = connection.query_row(
            &format!("SELECT COALESCE(MAX(ordinal), -1) + 1 FROM {table} WHERE session_id = ?1"),
            rusqlite::params![base],
            |r| r.get(0),
        )?;
        let rows: Vec<(i64, String, Option<String>)> = {
            let role_col = if has_role { ", role" } else { ", NULL" };
            let mut stmt = connection.prepare(&format!(
                "SELECT ordinal, payload_json{role_col} FROM {table}
                 WHERE session_id = ?1 ORDER BY ordinal"
            ))?;
            let out = stmt.query_map(rusqlite::params![other], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?))
            })?;
            out.collect::<Result<_, _>>()?
        };
        for (ordinal, payload, role) in rows {
            if existing.contains(&format!("{ordinal}|{payload}")) {
                continue; // the base already carries this exact row
            }
            match role {
                Some(role) => connection.execute(
                    &format!(
                        "INSERT INTO {table} (session_id, ordinal, role, payload_json, created_at)
                         SELECT ?1, ?2, ?3, payload_json, created_at FROM {table}
                         WHERE session_id = ?4 AND ordinal = ?5"
                    ),
                    rusqlite::params![base, next, role, other, ordinal],
                )?,
                None => connection.execute(
                    &format!(
                        "INSERT INTO {table} (session_id, ordinal, payload_json, created_at)
                         SELECT ?1, ?2, payload_json, created_at FROM {table}
                         WHERE session_id = ?3 AND ordinal = ?4"
                    ),
                    rusqlite::params![base, next, other, ordinal],
                )?,
            };
            next += 1;
        }
        connection.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            rusqlite::params![other],
        )?;
        if base == legacy {
            connection.execute(
                &format!("UPDATE {table} SET session_id = ?1 WHERE session_id = ?2"),
                rusqlite::params![canonical, legacy],
            )?;
        }
    }
    for (table, col) in [
        ("request_nonces", "session_id"),
        ("tasks", "session_id"),
        ("tasks", "reporting_session"),
        ("agent_board", "session_id"),
        ("recurring_jobs", "session_id"),
    ] {
        if !present.contains(table) {
            continue;
        }
        connection.execute(
            &format!("UPDATE OR IGNORE {table} SET {col} = ?1 WHERE {col} = ?2"),
            rusqlite::params![canonical, legacy],
        )?;
        connection.execute(
            &format!("DELETE FROM {table} WHERE {col} = ?1"),
            rusqlite::params![legacy],
        )?;
    }
    connection.execute(
        "DELETE FROM sessions WHERE id = ?1",
        rusqlite::params![legacy],
    )?;
    eprintln!("[store] merged duplicate session row `{legacy}` into `{canonical}`");
    Ok(())
}

/// The canonical id (and kind) a session id maps to: the path, minus the
/// filename.
///
/// A session id used to BE a filesystem path — `…/state.json` for a workspace's
/// default session, `…/conversations/<uuid>.json` for a saved one — even though
/// those files stopped being read when the store took over. The path component
/// is still a perfectly good KEY: it is unique, stable, and says which workspace
/// the session belongs to. The FILENAME is the part with no business in an
/// identifier, so that is what goes, along with the mangling it caused (a model
/// handed `…/state.json` dropped the suffix and the reply was rejected).
///
/// Deterministic on purpose: running this twice yields the same id, so the
/// rewrite is idempotent and needs no generated value to remember.
pub fn canonical_session_id(old: &str) -> (String, String) {
    if old == crate::mission_control::SESSION_ID {
        return (old.to_string(), "mission_control".to_string());
    }
    if let Some(dir) = old.strip_suffix("/state.json") {
        let kind = if dir.starts_with("inbox-") { "inbox" } else { "default" };
        return (dir.to_string(), kind.to_string());
    }
    if old.ends_with(".json") {
        return (old[..old.len() - ".json".len()].to_string(), "conversation".to_string());
    }
    let kind = if old.starts_with("inbox-") {
        "inbox"
    } else if old.contains("/conversations/") {
        "conversation"
    } else {
        "default"
    };
    (old.to_string(), kind.to_string())
}

/// A conversation's row: identity plus the scalar state, without its messages.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    /// Durable session id (the state path relative to the workspaces root, so
    /// existing references keep resolving).
    pub id: String,
    /// Stable key of the workspace folder (see `config::workspace_key`).
    pub workspace_key: String,
    /// Absolute folder the session runs in.
    pub workspace: String,
    pub title: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// Last *user* activity, unix seconds. Orders the session list. Was a
    /// `.meta.json` sidecar, so a session listing it from disk had to have that
    /// file; here the row is the only thing the list needs.
    pub last_active: Option<i64>,
    /// Per-conversation model override (profile name). Was `.profile`.
    pub profile: Option<String>,
    /// `mission_control` or `standard`. Was `.role`.
    pub role: String,
    /// The agent working this session. Was `.role`'s `agent_id`.
    pub agent_id: Option<String>,
    /// The path-shaped id this row had before ids became opaque, if it ever had
    /// one. The key an in-flight reference or a stored payload still names.
    pub legacy_id: Option<String>,
    /// `default` | `conversation` | `inbox` | `mission_control`.
    pub kind: String,
}

/// Session metadata that used to live in sidecar files.
///
/// Bundled so the importer can carry all of it in one argument rather than
/// growing a parameter list, and so "no sidecars present" is a `Default` rather
/// than a pile of `None`s at every call site.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionExtras {
    /// Last *user* activity, unix seconds. Was `.meta.json`.
    pub last_active: Option<i64>,
    /// Model override (profile name). Was `.profile`.
    pub profile: Option<String>,
    /// `mission_control` or `standard`. Was `.role`.
    pub role: Option<String>,
    /// The agent working this session. Was `.role`'s `agent_id`.
    pub agent_id: Option<String>,
}

/// A stored message, with its position so ordering survives a round trip.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub ordinal: i64,
    pub role: String,
    pub message: HarnessMessage,
    pub created_at: String,
}

/// The message-kind discriminator stored beside each payload.
///
/// Kept explicit rather than derived from the enum so the column can be indexed
/// and filtered in SQL without deserializing every payload.
fn role_of(message: &HarnessMessage) -> &'static str {
    match message {
        HarnessMessage::User { .. } => "user",
        HarnessMessage::Assistant { .. } => "assistant",
        HarnessMessage::ToolResult { .. } => "tool_result",
        HarnessMessage::Summary { .. } => "summary",
        HarnessMessage::System { .. } => "system",
    }
}

impl Store {
    /// Create a session with its initial transcript, in one transaction.
    ///
    /// The messages are written in the same transaction as the row so a session
    /// can never exist with a partially-written history.
    pub fn create_conversation(
        &self,
        session: &SessionRow,
        messages: &[HarnessMessage],
        tags_json: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                    created_at, updated_at, tags_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    session.id,
                    session.workspace_key,
                    session.workspace,
                    session.title,
                    session.status,
                    session.created_at,
                    session.updated_at,
                    tags_json,
                ],
            )?;
            for (ordinal, message) in messages.iter().enumerate() {
                insert_message(
                    &tx,
                    &session.id,
                    ordinal as i64,
                    message,
                    &session.updated_at,
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Append messages to the end of a transcript.
    ///
    /// This is the hot path: one INSERT per message, no read of what is already
    /// stored. `ordinal` continues from the current maximum, computed inside the
    /// same transaction so two writers cannot collide on a position.
    pub fn append_conversation_messages(
        &self,
        session_id: &str,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<Vec<i64>, StoreError> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM session_messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?;
            let mut written = Vec::with_capacity(messages.len());
            for message in messages {
                insert_message(&tx, session_id, next, message, now)?;
                written.push(next);
                next += 1;
            }
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            )?;
            tx.commit()?;
            Ok(written)
        })
    }

    /// Replace the transcript from `from_ordinal` onward.
    ///
    /// Compaction is the one legitimate rewrite: it folds a span of history into
    /// a summary. Scoping the delete to `>= from_ordinal` keeps everything
    /// before the cut byte-for-byte, instead of rewriting the whole table.
    pub fn replace_conversation_tail(
        &self,
        session_id: &str,
        from_ordinal: i64,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1 AND ordinal >= ?2",
                params![session_id, from_ordinal],
            )?;
            for (offset, message) in messages.iter().enumerate() {
                insert_message(&tx, session_id, from_ordinal + offset as i64, message, now)?;
            }
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Load a session's row, without its messages.
    /// A session by its id — or by the path-shaped id it used to have.
    ///
    /// THE lookup. Every reader funnels here, so accepting both forms in one
    /// place is what lets an id captured before the rewrite (in a message
    /// payload, a client cache, a log) still resolve to its session instead of
    /// silently matching nothing.
    pub fn get_session_row(&self, id: &str) -> Result<Option<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1 OR legacy_id = ?1"
            ))?;
            Ok(stmt.query_row(params![id], session_row_from).optional()?)
        })
    }

    /// Load a session's transcript in order.
    pub fn load_conversation_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<HarnessMessage>, StoreError> {
        let rows = self.load_conversation_rows(session_id, None)?;
        Ok(rows.into_iter().map(|row| row.message).collect())
    }

    /// Load at most the newest `limit` messages, in transcript order.
    ///
    /// Selection happens in SQL (newest-first with a LIMIT, reversed after), so
    /// opening a long conversation does not read the history it is not showing.
    pub fn load_recent_conversation(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.load_conversation_rows(session_id, Some(limit))
    }

    fn load_conversation_rows(
        &self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.with_connection(|conn| {
            let mut rows: Vec<StoredMessage> = match limit {
                Some(limit) => {
                    let mut stmt = conn.prepare(
                        "SELECT ordinal, role, payload_json, created_at
                         FROM session_messages WHERE session_id = ?1
                         ORDER BY ordinal DESC LIMIT ?2",
                    )?;
                    let mapped = stmt.query_map(params![session_id, limit], message_row_from)?;
                    mapped.collect::<Result<Vec<_>, _>>()?
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT ordinal, role, payload_json, created_at
                         FROM session_messages WHERE session_id = ?1
                         ORDER BY ordinal",
                    )?;
                    let mapped = stmt.query_map(params![session_id], message_row_from)?;
                    mapped.collect::<Result<Vec<_>, _>>()?
                }
            };
            if limit.is_some() {
                // The LIMIT took the newest rows; restore transcript order.
                rows.reverse();
            }
            Ok(rows)
        })
    }

    /// How many messages a session holds, without reading any of them.
    pub fn conversation_message_count(&self, session_id: &str) -> Result<i64, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?)
        })
    }

    /// Every session for a workspace, newest activity first — the session list.
    ///
    /// Reads only the `sessions` rows: the previous implementation had to open
    /// (and for older sessions, decompress) every transcript just to enumerate.
    pub fn list_conversations_for_workspace(
        &self,
        workspace_key: &str,
    ) -> Result<Vec<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions
                 WHERE workspace_key = ?1
                 ORDER BY updated_at DESC"
            ))?;
            let rows = stmt.query_map(params![workspace_key], session_row_from)?;
            rows.collect()
        })
    }

    /// Delete a session and its transcript.
    pub fn delete_conversation(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM request_nonces WHERE session_id = ?1",
                params![id],
            )?;
            let deleted = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])? == 1;
            tx.commit()?;
            Ok(deleted)
        })
    }

    /// Every session in the store, newest first — the device-wide catalog.
    ///
    /// The catalog used to be a `read_dir` walk of the workspaces tree, which
    /// meant a session living only in the store was invisible to it. The store is
    /// the inventory now; the walk survives only for sessions not yet moved.
    pub fn list_all_sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions
                 ORDER BY COALESCE(last_active, 0) DESC, updated_at DESC"
            ))?;
            let rows = stmt.query_map([], session_row_from)?;
            rows.collect()
        })
    }

    /// Record a session's last *user* activity.
    ///
    /// Separate from the scalar save because it is written on a different
    /// schedule: opening or attaching must NOT move it (the list would jump every
    /// time a chat was opened), so only a real user message calls this.
    pub fn set_session_last_active(&self, id: &str, last_active: i64) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET last_active = ?2 WHERE id = ?1",
                params![id, last_active],
            )?;
            Ok(())
        })
    }

    /// Set (or clear) a session's model override. Was the `.profile` sidecar.
    pub fn set_session_profile(&self, id: &str, profile: Option<&str>) -> Result<(), StoreError> {
        let value = profile.map(str::trim).filter(|p| !p.is_empty());
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET profile = ?2 WHERE id = ?1",
                params![id, value],
            )?;
            Ok(())
        })
    }

    /// Set a session's kind and the agent working it. Was the `.role` sidecar.
    pub fn set_session_role(
        &self,
        id: &str,
        role: &str,
        agent_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET role = ?2, agent_id = ?3 WHERE id = ?1",
                params![id, role, agent_id],
            )?;
            Ok(())
        })
    }

    /// Import a whole session — scalar state plus both logs — in one transaction.
    ///
    /// Unlike [`Self::create_conversation`] this takes the session's real scalar
    /// (counters, goal, checkpoints, lanes) and its event log, which is what a
    /// migration has and a new session does not. One transaction so an
    /// interrupted import leaves no half-written session behind: either the row
    /// and its full history are there or nothing is.
    ///
    /// Re-importing the same id replaces it, so a migration can be re-run safely
    /// after a failure without duplicating messages.
    pub fn import_session(
        &self,
        id: &str,
        workspace_key: &str,
        state: &crate::harness::HarnessState,
        extras: &SessionExtras,
    ) -> Result<(), StoreError> {
        let scalar = crate::harness::scalar_json(state).map_err(StoreError::ScalarEncode)?;
        let status = crate::session::status_str(state.status);
        let role = extras
            .role
            .clone()
            .unwrap_or_else(|| "standard".to_string());
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                    created_at, updated_at, tags_json, state_json,
                    last_active, profile, role, agent_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'[]',?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                    workspace_key = excluded.workspace_key,
                    workspace = excluded.workspace,
                    title = excluded.title,
                    status = excluded.status,
                    updated_at = excluded.updated_at,
                    state_json = excluded.state_json,
                    last_active = excluded.last_active,
                    profile = excluded.profile,
                    role = excluded.role,
                    agent_id = excluded.agent_id",
                params![
                    id,
                    workspace_key,
                    state.workspace,
                    state.title,
                    status,
                    state.created_at,
                    state.updated_at,
                    scalar,
                    extras.last_active,
                    extras.profile,
                    role,
                    extras.agent_id,
                ],
            )?;
            // Replace rather than append: an import states the whole truth, so a
            // re-run must not stack a second copy of every message.
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM session_events WHERE session_id = ?1",
                params![id],
            )?;
            for (ordinal, message) in state.messages.iter().enumerate() {
                insert_message(&tx, id, ordinal as i64, message, &state.updated_at)?;
            }
            for (ordinal, event) in state.events.iter().enumerate() {
                insert_event(&tx, id, ordinal as i64, event, &state.updated_at)?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}

const SESSION_COLUMNS: &str = "id, workspace_key, workspace, title, status, created_at, \
     updated_at, last_active, profile, role, agent_id, legacy_id, kind";

impl Store {
    /// Save a session's scalar state, creating the row on first write.
    ///
    /// Upsert rather than insert-or-fail: a session created before this store
    /// existed has no row, and the first persist must be able to establish one.
    /// `state_json` deliberately excludes the two append-only logs, so rewriting
    /// it on every persist stays cheap regardless of conversation length.
    #[allow(clippy::too_many_arguments)]
    pub fn save_session_scalar(
        &self,
        id: &str,
        workspace_key: &str,
        workspace: &str,
        title: Option<&str>,
        status: &str,
        scalar_json: &str,
        created_at: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                created_at, updated_at, state_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET
                workspace = excluded.workspace,
                title = excluded.title,
                status = excluded.status,
                updated_at = excluded.updated_at,
                state_json = excluded.state_json",
                params![
                    id,
                    workspace_key,
                    workspace,
                    title,
                    status,
                    created_at,
                    now,
                    scalar_json
                ],
            )?;
            Ok(())
        })
    }

    /// Read a session's stored scalar state, if this store has the session.
    pub fn load_session_scalar(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            Ok(conn
                .query_row(
                    "SELECT state_json FROM sessions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    /// Whether this store has a row for the session. Drives DB-vs-file selection on
    /// read, so a session that has not been migrated is still served from disk.
    pub fn has_conversation(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
    }
}

impl Store {
    /// Cheap change-detector: the row's `updated_at` plus the two log lengths.
    ///
    /// Lets a poller notice a change without loading the transcript. The attach
    /// stream needs this because it re-reads on a timer, and loading a long
    /// conversation just to discover nothing changed would defeat the point of
    /// moving it into the database.
    pub fn conversation_fingerprint(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            Ok(conn
                .query_row(
                    "SELECT s.updated_at,
                            (SELECT COUNT(*) FROM session_messages WHERE session_id = s.id),
                            (SELECT COUNT(*) FROM session_events WHERE session_id = s.id)
                     FROM sessions s WHERE s.id = ?1",
                    params![id],
                    |row| {
                        let updated: String = row.get(0)?;
                        let messages: i64 = row.get(1)?;
                        let events: i64 = row.get(2)?;
                        Ok(format!("{updated}|{messages}|{events}"))
                    },
                )
                .optional()?)
        })
    }

    /// Replace a transcript wholesale.
    ///
    /// The escape hatch for the writers that genuinely rewrite history —
    /// compaction, checkpoint rewind, interrupt rollback. Scoped as delete-all +
    /// insert rather than a diff, because after one of those the two lists have
    /// little in common and diffing would cost more than rewriting.
    pub fn replace_conversation_messages(
        &self,
        session_id: &str,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1",
                params![session_id],
            )?;
            for (ordinal, message) in messages.iter().enumerate() {
                insert_message(&tx, session_id, ordinal as i64, message, now)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Replace the event log wholesale. Same escape hatch as messages.
    pub fn replace_conversation_events(
        &self,
        session_id: &str,
        events: &[HarnessEvent],
        now: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_events WHERE session_id = ?1",
                params![session_id],
            )?;
            for (ordinal, event) in events.iter().enumerate() {
                insert_event(&tx, session_id, ordinal as i64, event, now)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Atomically claim a client request id. `false` means it was already
    /// accepted, so a reconnect must not deliver the same input again.
    pub fn claim_request_nonce(
        &self,
        session_id: &str,
        nonce: &str,
        accepted_at: i64,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let inserted = tx.execute(
                "INSERT INTO request_nonces (session_id, nonce, accepted_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, nonce) DO NOTHING",
                params![session_id, nonce, accepted_at],
            )? == 1;
            if inserted {
                tx.execute(
                    "DELETE FROM request_nonces
                     WHERE session_id = ?1
                       AND nonce NOT IN (
                           SELECT nonce FROM request_nonces
                           WHERE session_id = ?1
                           ORDER BY accepted_at DESC, rowid DESC
                           LIMIT 512
                       )",
                    params![session_id],
                )?;
            }
            tx.commit()?;
            Ok(inserted)
        })
    }

    /// Append events to the log, continuing from the current maximum ordinal.
    pub fn append_conversation_events(
        &self,
        session_id: &str,
        events: &[HarnessEvent],
        now: &str,
    ) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM session_events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?;
            for event in events {
                insert_event(&tx, session_id, next, event, now)?;
                next += 1;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Load the full event log in order.
    pub fn load_conversation_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<HarnessEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM session_events
                 WHERE session_id = ?1 ORDER BY ordinal",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                let raw: String = row.get(0)?;
                serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })?;
            rows.collect()
        })
    }
}

fn insert_event(
    conn: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    event: &HarnessEvent,
    now: &str,
) -> Result<(), rusqlite::Error> {
    let payload = serde_json::to_string(event)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO session_events (session_id, ordinal, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![session_id, ordinal, payload, now],
    )?;
    Ok(())
}

fn session_row_from(row: &rusqlite::Row<'_>) -> Result<SessionRow, rusqlite::Error> {
    Ok(SessionRow {
        id: row.get(0)?,
        workspace_key: row.get(1)?,
        workspace: row.get(2)?,
        title: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        last_active: row.get(7)?,
        profile: row.get(8)?,
        role: row.get(9)?,
        agent_id: row.get(10)?,
        legacy_id: row.get(11)?,
        kind: row.get(12)?,
    })
}

fn message_row_from(row: &rusqlite::Row<'_>) -> Result<StoredMessage, rusqlite::Error> {
    let payload: String = row.get(2)?;
    let message: HarnessMessage = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(StoredMessage {
        ordinal: row.get(0)?,
        role: row.get(1)?,
        message,
        created_at: row.get(3)?,
    })
}

fn insert_message(
    conn: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    message: &HarnessMessage,
    now: &str,
) -> Result<(), rusqlite::Error> {
    let payload = serde_json::to_string(message)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO session_messages (session_id, ordinal, role, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, ordinal, role_of(message), payload, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn session(id: &str) -> SessionRow {
        SessionRow {
            id: id.into(),
            workspace_key: "wk1".into(),
            workspace: "/code/thing".into(),
            title: Some("Thing".into()),
            status: "idle".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_active: None,
            profile: None,
            role: "standard".into(),
            agent_id: None,
            legacy_id: None,
            kind: "conversation".into(),
        }
    }

    fn user(text: &str) -> HarnessMessage {
        HarnessMessage::User {
            content: text.into(),
        }
    }

    #[test]
    fn request_nonces_are_idempotent_and_bounded() {
        let db = db();
        assert!(db.claim_request_nonce("s1", "first", 1).unwrap());
        assert!(!db.claim_request_nonce("s1", "first", 2).unwrap());
        for i in 0..513 {
            assert!(
                db.claim_request_nonce("s1", &format!("nonce-{i}"), i + 3)
                    .unwrap()
            );
        }
        let count: i64 = db
            .with_connection(|connection| {
                connection.query_row(
                    "SELECT COUNT(*) FROM request_nonces WHERE session_id = 's1'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(count, 512);
        assert!(db.claim_request_nonce("s1", "first", 600).unwrap());
    }

    #[test]
    fn appending_grows_the_transcript_without_rewriting_it() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("one")], "[]")
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 1);

        // Two separate appends: the second must continue the ordinals, not
        // restart them, so the transcript order survives.
        db.append_conversation_messages("s1", &[user("two")], "2026-01-01T00:00:01Z")
            .unwrap();
        db.append_conversation_messages("s1", &[user("three")], "2026-01-01T00:00:02Z")
            .unwrap();

        assert_eq!(db.conversation_message_count("s1").unwrap(), 3);
        let loaded = db.load_conversation_messages("s1").unwrap();
        assert_eq!(loaded, vec![user("one"), user("two"), user("three")]);
    }

    #[test]
    fn recent_load_reads_only_the_tail() {
        let db = db();
        let all: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        db.create_conversation(&session("s1"), &all, "[]").unwrap();

        let recent = db.load_recent_conversation("s1", 3).unwrap();
        assert_eq!(recent.len(), 3);
        // Newest selected, transcript order preserved.
        assert_eq!(recent[0].message, user("m7"));
        assert_eq!(recent[2].message, user("m9"));
    }

    #[test]
    fn compaction_replaces_only_the_tail() {
        let db = db();
        let all: Vec<_> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        db.create_conversation(&session("s1"), &all, "[]").unwrap();

        // Fold everything from ordinal 3 into one summary.
        db.replace_conversation_tail(
            "s1",
            3,
            &[HarnessMessage::Summary {
                kind: "compact".into(),
                content: "folded".into(),
            }],
            "2026-01-01T00:00:03Z",
        )
        .unwrap();

        let loaded = db.load_conversation_messages("s1").unwrap();
        assert_eq!(loaded.len(), 4, "3 kept + 1 summary");
        assert_eq!(loaded[0], user("m0"));
        assert_eq!(loaded[2], user("m2"));
        assert!(matches!(loaded[3], HarnessMessage::Summary { .. }));
    }

    #[test]
    fn the_session_list_does_not_touch_transcripts() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("a")], "[]")
            .unwrap();
        db.create_conversation(
            &SessionRow {
                id: "s2".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
                ..session("s2")
            },
            &[user("b")],
            "[]",
        )
        .unwrap();

        let rows = db.list_conversations_for_workspace("wk1").unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["s2", "s1"],
            "newest activity first"
        );
    }

    #[test]
    fn deleting_a_session_takes_its_transcript() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("a")], "[]")
            .unwrap();
        assert!(db.delete_conversation("s1").unwrap());
        assert_eq!(db.conversation_message_count("s1").unwrap(), 0);
        assert!(db.get_session_row("s1").unwrap().is_none());
    }

    #[test]
    fn deleting_a_session_takes_its_events_too() {
        // `create_conversation` writes messages but no events, so the event-log
        // cascade is only exercised by a full import — which is also the path the
        // migration uses.
        let db = db();
        let mut state = crate::harness::HarnessState::blank("/code/thing", Some("Thing".into()));
        state.messages = vec![user("one"), user("two")];
        state.events = vec![HarnessEvent::UserInput { text: "one".into() }];
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 2);
        assert_eq!(db.load_conversation_events("s1").unwrap().len(), 1);

        assert!(db.delete_conversation("s1").unwrap());

        // Both logs must go with the row. A leftover message or event would keep
        // the deleted conversation readable through the log tables.
        assert_eq!(db.conversation_message_count("s1").unwrap(), 0);
        assert!(db.load_conversation_events("s1").unwrap().is_empty());
        assert!(db.get_session_row("s1").unwrap().is_none());
    }

    #[test]
    fn re_importing_replaces_instead_of_duplicating() {
        // The migration is re-runnable, so importing the same id twice must not
        // stack a second copy of every message.
        let db = db();
        let mut state = crate::harness::HarnessState::blank("/code/thing", Some("Thing".into()));
        state.messages = vec![user("one")];
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 1);
    }
}

#[cfg(test)]
mod session_id_migration_tests {
    use super::*;
    use rusqlite::Connection;

    fn insert_session(conn: &Connection, id: &str, legacy: Option<&str>) {
        conn.execute(
            "INSERT INTO sessions (id, workspace_key, workspace, status, created_at, updated_at, legacy_id)
             VALUES (?1, 'wk', '/code/thing', 'idle', 't', 't', ?2)",
            rusqlite::params![id, legacy],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, sid: &str, ordinal: i64, body: &str) {
        conn.execute(
            "INSERT INTO session_messages (session_id, ordinal, role, payload_json, created_at)
             VALUES (?1, ?2, 'user', ?3, 't')",
            rusqlite::params![sid, ordinal, body],
        )
        .unwrap();
    }

    fn session_ids(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT id FROM sessions ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn messages(conn: &Connection, sid: &str) -> Vec<String> {
        conn.prepare(
            "SELECT ordinal || '|' || payload_json FROM session_messages
             WHERE session_id = ?1 ORDER BY ordinal",
        )
        .unwrap()
        .query_map(rusqlite::params![sid], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// A renamed session whose legacy id was re-created must MERGE, not fail.
    ///
    /// Renaming would collide on `(session_id, ordinal)` and abort the open, so
    /// the store would not load at all. Observed live: a daemon on the previous
    /// build kept writing the pre-migration id, so one session ended up with two
    /// rows and the next start could not open the database.
    #[test]
    fn a_recreated_legacy_row_merges_instead_of_colliding() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        // The canonical row holds a stale PREFIX of the same conversation — the
        // real shape: the stale copy shares ordinals and payloads with the row
        // that kept being appended to.
        insert_session(&conn, "ws-1", Some("ws-1/state.json"));
        for i in 0..2 {
            insert_message(&conn, "ws-1", i, &format!("same {i}"));
        }
        insert_session(&conn, "ws-1/state.json", None);
        for i in 0..4 {
            insert_message(&conn, "ws-1/state.json", i, &format!("same {i}"));
        }

        rewrite_legacy_session_ids(&conn).unwrap();

        assert_eq!(session_ids(&conn), vec!["ws-1".to_string()]);
        // One conversation, not two: 4 messages, because both rows carry the SAME
        // two messages at ordinals 0 and 1 and the rest extends them.
        assert_eq!(
            messages(&conn, "ws-1"),
            vec!["0|same 0", "1|same 1", "2|same 2", "3|same 3"]
        );
    }

    /// Diverged rows keep BOTH transcripts.
    ///
    /// "Keep the longer one" is only correct while the shorter is a strict
    /// prefix. When the two actually diverge — which the live database showed is
    /// possible, since the stale row had unique request nonces only it held —
    /// discarding the shorter side would silently delete messages nothing can
    /// restore. The union renumbers the extra rows past the base's last ordinal
    /// so they cannot collide on the primary key.
    #[test]
    fn diverged_duplicates_keep_both_transcripts() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        insert_session(&conn, "ws-9", Some("ws-9/state.json"));
        for i in 0..2 {
            insert_message(&conn, "ws-9", i, &format!("shared {i}"));
        }
        insert_message(&conn, "ws-9", 2, "only-on-canonical");

        insert_session(&conn, "ws-9/state.json", None);
        for i in 0..2 {
            insert_message(&conn, "ws-9/state.json", i, &format!("shared {i}"));
        }
        insert_message(&conn, "ws-9/state.json", 2, "only-on-legacy");
        for i in 3..5 {
            insert_message(&conn, "ws-9/state.json", i, &format!("tail {i}"));
        }

        rewrite_legacy_session_ids(&conn).unwrap();

        assert_eq!(session_ids(&conn), vec!["ws-9".to_string()]);
        let got = messages(&conn, "ws-9");
        // Neither side's unique content is dropped. A row whose ordinal is
        // already taken on the base side is RENUMBERED past the base's last
        // ordinal, so match on the payload rather than the original position.
        assert!(
            got.iter().any(|m| m.ends_with("only-on-canonical")),
            "the shorter side's unique message must survive: {got:?}"
        );
        assert!(
            got.iter().any(|m| m.ends_with("only-on-legacy")),
            "the longer side's message must survive: {got:?}"
        );
        assert_eq!(
            got.len(),
            6,
            "2 shared + 4 unique all survive: {got:?}"
        );
        // The shared prefix is not repeated.
        assert_eq!(
            got.iter().filter(|m| m.ends_with("shared 0")).count(),
            1,
            "a shared row must not be duplicated: {got:?}"
        );
    }

    /// The ordinary case: a lone legacy row is renamed, and its transcript moves
    /// with it while `legacy_id` remembers the old value for rollback.
    #[test]
    fn a_lone_legacy_row_is_renamed_and_keeps_its_history() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        insert_session(&conn, "ws-2/state.json", None);
        insert_message(&conn, "ws-2/state.json", 0, "hello");

        rewrite_legacy_session_ids(&conn).unwrap();

        assert_eq!(session_ids(&conn), vec!["ws-2".to_string()]);
        let moved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_messages WHERE session_id='ws-2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(moved, 1, "the transcript moved with the id");
        let legacy: Option<String> = conn
            .query_row("SELECT legacy_id FROM sessions WHERE id='ws-2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(legacy.as_deref(), Some("ws-2/state.json"));
    }

    /// Called on every open, so a second run must change nothing.
    #[test]
    fn the_rewrite_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        insert_session(&conn, "ws-3/state.json", None);

        rewrite_legacy_session_ids(&conn).unwrap();
        let first = session_ids(&conn);
        rewrite_legacy_session_ids(&conn).unwrap();

        assert_eq!(first, session_ids(&conn));
    }
}
