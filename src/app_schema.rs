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

         CREATE TABLE IF NOT EXISTS mission_autonomy (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             state_json TEXT NOT NULL
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

         -- Durable notification feed, shared by `/notifications` and its replay alias.
         CREATE TABLE IF NOT EXISTS notification_sources (
             source_key TEXT PRIMARY KEY NOT NULL,
             event_id INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS notification_sources_event
             ON notification_sources(event_id);
         CREATE TABLE IF NOT EXISTS notification_sequence (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             next_id INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS notification_journal (
             event_id INTEGER PRIMARY KEY NOT NULL,
             kind TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             payload_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS notification_journal_created
             ON notification_journal(created_at);
         CREATE INDEX IF NOT EXISTS notification_journal_cursor
             ON notification_journal(created_at, event_id);
         CREATE TABLE IF NOT EXISTS notification_clock (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS ambient_events (
             id TEXT PRIMARY KEY NOT NULL,
             device TEXT NOT NULL DEFAULT '',
             started_at TEXT NOT NULL,
             duration_ms INTEGER NOT NULL DEFAULT 0,
             status TEXT NOT NULL,
             text TEXT NOT NULL DEFAULT '',
             error TEXT,
             created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS ambient_events_started
             ON ambient_events(started_at);
         CREATE INDEX IF NOT EXISTS ambient_events_created
             ON ambient_events(created_at);
         CREATE INDEX IF NOT EXISTS ambient_events_status
             ON ambient_events(status);
         CREATE VIRTUAL TABLE IF NOT EXISTS ambient_events_fts
             USING fts5(text, content='ambient_events', content_rowid='rowid');
         CREATE TABLE IF NOT EXISTS ambient_turns (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             conversation_id TEXT NOT NULL,
             role TEXT NOT NULL,
             text TEXT NOT NULL,
             device TEXT NOT NULL DEFAULT '',
             created_at INTEGER NOT NULL,
             played_at INTEGER
         );
         CREATE INDEX IF NOT EXISTS ambient_turns_device
             ON ambient_turns(device, id);
         CREATE INDEX IF NOT EXISTS ambient_turns_created
             ON ambient_turns(created_at);
         CREATE TABLE IF NOT EXISTS ambient_calls (
             id TEXT PRIMARY KEY NOT NULL,
             device TEXT NOT NULL,
             detected_at INTEGER NOT NULL,
             started_at INTEGER NOT NULL,
             ended_at INTEGER,
             state TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS ambient_call_lines (
             call_id TEXT NOT NULL,
             at_ms INTEGER NOT NULL,
             speaker TEXT NOT NULL,
             text TEXT NOT NULL,
             event_ids TEXT NOT NULL DEFAULT '[]'
         );
         CREATE INDEX IF NOT EXISTS ambient_call_lines_call
             ON ambient_call_lines(call_id, at_ms);
         CREATE TABLE IF NOT EXISTS ambient_chunks (
             turn_id INTEGER NOT NULL,
             seq INTEGER NOT NULL,
             ms INTEGER NOT NULL,
             text TEXT NOT NULL,
             PRIMARY KEY (turn_id, seq)
         );
"#,
    )?;
    let have: Vec<String> = {
        let mut stmt = connection.prepare("PRAGMA table_info(ambient_events)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<Result<_, _>>()?
    };
    for (name, ddl) in [
        ("verdict", "ALTER TABLE ambient_events ADD COLUMN verdict TEXT"),
        ("addressed", "ALTER TABLE ambient_events ADD COLUMN addressed REAL"),
        ("judge", "ALTER TABLE ambient_events ADD COLUMN judge TEXT"),
        ("intent", "ALTER TABLE ambient_events ADD COLUMN intent TEXT"),
        ("kept", "ALTER TABLE ambient_events ADD COLUMN kept INTEGER NOT NULL DEFAULT 1"),
        ("seq", "ALTER TABLE ambient_events ADD COLUMN seq INTEGER"),
        ("continued", "ALTER TABLE ambient_events ADD COLUMN continued INTEGER NOT NULL DEFAULT 0"),
        ("during_playback", "ALTER TABLE ambient_events ADD COLUMN during_playback INTEGER NOT NULL DEFAULT 0"),
        ("media", "ALTER TABLE ambient_events ADD COLUMN media INTEGER NOT NULL DEFAULT 0"),
        ("routed", "ALTER TABLE ambient_events ADD COLUMN routed INTEGER NOT NULL DEFAULT 1"),
        ("delivered", "ALTER TABLE ambient_events ADD COLUMN delivered INTEGER NOT NULL DEFAULT 0"),
        ("turn_key", "ALTER TABLE ambient_events ADD COLUMN turn_key TEXT"),
        ("user_voice", "ALTER TABLE ambient_events ADD COLUMN user_voice REAL"),
        ("other_party", "ALTER TABLE ambient_events ADD COLUMN other_party REAL"),
        ("played", "ALTER TABLE ambient_events ADD COLUMN played REAL"),
        ("speaker", "ALTER TABLE ambient_events ADD COLUMN speaker TEXT"),
    ] {
        if !have.iter().any(|c| c == name) {
            connection.execute(ddl, [])?;
        }
    }
    connection.execute("DROP TABLE IF EXISTS speakers", [])?;
    for column in ["speakers", "speaker_summary"] {
        if have.iter().any(|c| c == column) {
            connection.execute(&format!("ALTER TABLE ambient_events DROP COLUMN {column}"), [])?;
        }
    }
    let have_turns: Vec<String> = {
        let mut stmt = connection.prepare("PRAGMA table_info(ambient_turns)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<Result<_, _>>()?
    };
    for (name, ddl) in [
        ("status", "ALTER TABLE ambient_turns ADD COLUMN status TEXT NOT NULL DEFAULT 'played'"),
        ("heard_text", "ALTER TABLE ambient_turns ADD COLUMN heard_text TEXT"),
        ("chunk_count", "ALTER TABLE ambient_turns ADD COLUMN chunk_count INTEGER NOT NULL DEFAULT 0"),
        ("ended", "ALTER TABLE ambient_turns ADD COLUMN ended INTEGER NOT NULL DEFAULT 0"),
        ("played_through", "ALTER TABLE ambient_turns ADD COLUMN played_through INTEGER NOT NULL DEFAULT -1"),
    ] {
        if !have_turns.iter().any(|c| c == name) {
            connection.execute(ddl, [])?;
        }
    }
    let legacy: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'mission_duty')",
        [],
        |row| row.get(0),
    )?;
    if legacy {
        connection.execute_batch(
            "INSERT OR IGNORE INTO mission_autonomy (id, state_json) SELECT id, state_json FROM mission_duty;
             DROP TABLE mission_duty;",
        )?;
    }
    Ok(())
}
