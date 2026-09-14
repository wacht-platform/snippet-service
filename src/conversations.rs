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

use std::path::Path;

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
             agent_id TEXT
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
    )
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

impl SessionExtras {
    /// Gather the sidecar metadata sitting beside a state file.
    ///
    /// The sidecars are the ONLY source for these values until the columns are
    /// backfilled, so an import has to read them while they still exist — this is
    /// the step that makes deleting them later safe rather than destructive.
    pub fn from_sidecars(state_path: &Path) -> Self {
        let sidecar = crate::session::read_session_sidecar(state_path);
        Self {
            last_active: Some(crate::session::session_last_active(state_path)),
            profile: crate::session::read_session_profile(state_path),
            role: sidecar
                .as_ref()
                .map(|s| crate::session::role_str(s.role).to_string()),
            agent_id: sidecar.and_then(|s| s.agent_id),
        }
    }
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
    pub fn get_session_row(&self, id: &str) -> Result<Option<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
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
     updated_at, last_active, profile, role, agent_id";

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

/// One session migration attempt, so a caller can report what happened per
/// session rather than a bare count.
#[derive(Debug, Clone, PartialEq)]
pub struct MigratedSession {
    pub id: String,
    pub workspace: String,
    pub messages: usize,
    pub events: usize,
}

/// Whether a file-backed session needs importing: no row yet, or the row is
/// behind the file.
///
/// "Has a row" is NOT the same question as "the row is current". A session that
/// keeps being used after it was migrated has a row AND newer content on disk, so
/// skipping on row-presence silently freezes that session at its migration
/// snapshot — the transcript keeps growing on disk while reads serve the stale
/// copy. Comparing the file's own `updated_at` against the row's is what makes a
/// re-run actually re-sync instead of reporting a no-op as success.
///
/// The comparison is on PARSED timestamps, not strings: lexicographic ordering of
/// RFC3339 with variable fractional digits is wrong (`.9` sorts before `.10`
/// although it is later), and a row that merely LOOKS newer would be skipped.
/// The failure modes are asymmetric, so an unparseable timestamp imports: a
/// redundant import is a harmless rewrite, while a wrong skip strands the session
/// stale forever.
///
/// A row ALSO imports when it predates the metadata columns. Those sessions were
/// imported by a build that had no `last_active`/`profile`/`role`, so their files
/// have not changed since and the timestamp test would skip them forever —
/// leaving every one of them with no activity stamp and no model override. Same
/// shape of bug as the original: "has a row" is not "the row is complete".
fn needs_import(store: &Store, id: &str, file_updated_at: &str) -> bool {
    let row = match store.get_session_row(id) {
        Ok(None) => return true,
        // An unreadable store must not read as "already imported" — that would
        // skip the session and lose the new content.
        Err(_) => return true,
        Ok(Some(row)) => row,
    };
    if row.last_active.is_none() {
        return true;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(file_updated_at)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(&row.updated_at).ok());
    match parsed {
        Some((file, row)) => row < file,
        None => true,
    }
}

/// Report which file-backed sessions WOULD migrate, without writing anything.
///
/// Takes the store so a dry run applies the SAME staleness rule the real run
/// does; otherwise it would list every session forever and the preview would
/// misrepresent what is about to happen.
pub fn scan_file_sessions(
    store: &crate::store::Store,
) -> std::io::Result<(Vec<MigratedSession>, Vec<(String, String)>)> {
    let root = crate::config::workspaces_root();
    let mut found = Vec::new();
    let mut failed = Vec::new();
    for (id, path) in workspace_state_paths(&root) {
        match crate::session::read_session_file(&path) {
            Some(state) => {
                if needs_import(store, &id, &state.updated_at) {
                    found.push(MigratedSession {
                        id,
                        workspace: state.workspace.clone(),
                        messages: state.messages.len(),
                        events: state.events.len(),
                    });
                }
            }
            None => failed.push((id, "state unreadable".to_string())),
        }
    }
    Ok((found, failed))
}

/// Every session state file under the workspaces root, with its resolved id.
///
/// One walk, shared by the scan and the migration, so a dry run cannot disagree
/// with the real run about which sessions exist.
///
/// Mission Control is included explicitly: its session lives under
/// `mission-control/`, not the workspaces root, so a walk of that root could
/// never see it — which is why it stayed file-backed while everything else moved.
fn workspace_state_paths(root: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut out = Vec::new();
    let mc = crate::mission_control::session_state_path();
    if mc.exists() {
        out.push((crate::mission_control::SESSION_ID.to_string(), mc));
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // A workspace dir holds a default `state.json` plus one file per saved
        // conversation. Both are sessions and both are migrated. Sidecars
        // (`.meta.json`, `.nonces.json`) live in the same directory, so the
        // shared predicate — not a bare `.json` check — decides what is one.
        let mut state_paths = vec![dir.join("state.json")];
        if let Ok(convs) = std::fs::read_dir(dir.join("conversations")) {
            for conv in convs.flatten() {
                let path = conv.path();
                if crate::session::is_conversation_json(&path) {
                    state_paths.push(path);
                }
            }
        }
        for state_path in state_paths {
            if state_path.exists() {
                let id = crate::session::session_id_for_state_path(&state_path);
                out.push((id, state_path));
            }
        }
    }
    out
}

/// Copy every file-backed session into the store.
///
/// A READ-ONLY pass over the state files: nothing on disk is modified or
/// deleted, so a migration can be re-run and the originals remain the fallback
/// until the caller is satisfied. `import_session` replaces by id, which makes a
/// re-run idempotent instead of duplicating every message.
///
/// Sessions already in the store are skipped, so this is cheap to re-run as new
/// file-backed sessions appear.
pub fn migrate_file_sessions(
    store: &crate::store::Store,
) -> std::io::Result<(Vec<MigratedSession>, Vec<(String, String)>)> {
    let root = crate::config::workspaces_root();
    let mut migrated = Vec::new();
    let mut failed = Vec::new();
    for (id, state_path) in workspace_state_paths(&root) {
        // Read the FILE, not the store: the file is the source of truth for an
        // import, and going through the dual reader would hand back the row we
        // are trying to refresh.
        let Some(state) = crate::session::read_session_file(&state_path) else {
            failed.push((id, "state unreadable".to_string()));
            continue;
        };
        if !needs_import(store, &id, &state.updated_at) {
            continue;
        }
        let workspace = if state.workspace.trim().is_empty() {
            // A pre-field state has no folder; fall back to the workspace dir so
            // the row still carries a usable path rather than an empty string.
            state_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        } else {
            state.workspace.clone()
        };
        let key = crate::config::workspace_key(Path::new(&workspace));
        let extras = SessionExtras::from_sidecars(&state_path);
        match store.import_session(&id, &key, &state, &extras) {
            Ok(()) => migrated.push(MigratedSession {
                id,
                workspace,
                messages: state.messages.len(),
                events: state.events.len(),
            }),
            Err(error) => failed.push((id, error.to_string())),
        }
    }
    Ok((migrated, failed))
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
