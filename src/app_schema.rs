//! Schema for the app-level stores: managed sessions, control settings,
//! recurring jobs, and the notification journal.
//!
//! These used to be JSON files under `~/.snippet`. They live in the same SQLite
//! database as everything else now, so there is one durable store for the whole
//! device rather than a store per subsystem.
//!
//! Kept apart from `coordination::schema` because these are not coordination:
//! that module describes the agent control plane, this one the app's own
//! bookkeeping. `store::migrate` runs both.
//!
//! Additive only: every statement is `IF NOT EXISTS`, so calling this against an
//! existing database is a no-op for tables that already exist.

use rusqlite::Connection;

pub fn ensure(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        r#"
         -- Managed sessions: the registry the Mission Control UI lists. The
         -- CONVERSATION of such a session lives in `sessions`, keyed by id;
         -- this holds the label/workspace/status the UI shows.
         CREATE TABLE IF NOT EXISTS managed_sessions (
             id TEXT PRIMARY KEY NOT NULL,
             label TEXT NOT NULL,
             workspace TEXT NOT NULL,
             status TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             tags_json TEXT NOT NULL DEFAULT '{}'
         );

         -- Single-row control settings. The CHECK makes a second row impossible,
         -- so a read never has to decide which one wins.
         CREATE TABLE IF NOT EXISTS control_settings (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             mission_control_session_id TEXT,
             notification_policy TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS recurring_jobs (
             id TEXT PRIMARY KEY NOT NULL,
             title TEXT NOT NULL,
             session_id TEXT NOT NULL,
             prompt TEXT NOT NULL,
             plan_path TEXT,
             schedule_json TEXT NOT NULL,
             delivery TEXT NOT NULL,
             enabled INTEGER NOT NULL,
             next_run_at INTEGER NOT NULL,
             last_run_at INTEGER,
             last_error TEXT,
             queued INTEGER NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );

         -- The device event journal, read back by `/notifications/replay`.
         --
         -- Nothing populates this at present: the emit path was removed pending
         -- a replacement, so the reader currently returns nothing. The table is
         -- kept so the replay endpoint and its clients stay in place and the
         -- incoming writer has somewhere to land.
         CREATE TABLE IF NOT EXISTS notification_journal (
             event_id INTEGER PRIMARY KEY NOT NULL,
             kind TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             payload_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS notification_journal_created
             ON notification_journal(created_at);
"#,
    )
}
