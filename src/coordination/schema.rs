//! Schema for the coordination control plane.
//!
//! The DDL lives with the domain it describes rather than in [`crate::store`].
//! That module owns the CONNECTION and runs each domain's `ensure` in order; it
//! deliberately does not own the table definitions, because a table's shape is a
//! fact about its subsystem, and centralising the SQL there made the store look
//! like it was the coordination plane's private database.
//!
//! Additive only: every statement is `IF NOT EXISTS`, so calling this against an
//! existing database is a no-op for tables that already exist.

use rusqlite::Connection;

pub fn ensure(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS agents (
             id TEXT PRIMARY KEY NOT NULL,
             display_name TEXT NOT NULL,
             handle TEXT NOT NULL UNIQUE,
             kind TEXT NOT NULL,
             status TEXT NOT NULL,
             role TEXT NOT NULL,
             capabilities_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             last_heartbeat_at TEXT
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
             thread_id TEXT NOT NULL,
             -- Dispatch state. This used to live in a parallel JSON store that
             -- duplicated the board; one task store means the board and the
             -- dispatcher read and write the same row. The *_json columns hold
             -- the same shapes the JSON record serialized.
             session_id TEXT NOT NULL DEFAULT '',
             handoff_json TEXT,
             handoff_mode TEXT NOT NULL DEFAULT 'resume',
             reporting_session TEXT,
             dispatch_failures INTEGER NOT NULL DEFAULT 0,
             result_json TEXT,
             notifications_json TEXT NOT NULL DEFAULT '[]',
             owned_paths_json TEXT NOT NULL DEFAULT '[]',
             -- Inference profile the target session should run on. Set by the
             -- dispatcher; NULL leaves the session's own model alone.
             profile TEXT
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

         -- Per-recipient delivery state for direct messages. A direct thread is a
         -- board thread with scope 'direct'; this table is what makes delivery
         -- durable, so a message accepted before a restart is still delivered
         -- after it, and a reconnect cannot deliver the same message twice.
         CREATE TABLE IF NOT EXISTS message_deliveries (
             event_id TEXT NOT NULL,
             recipient_kind TEXT NOT NULL,
             recipient_id TEXT NOT NULL,
             recipient TEXT NOT NULL,
             queued_at TEXT NOT NULL,
             delivered_at TEXT,
             read_at TEXT,
             attempts INTEGER NOT NULL DEFAULT 0,
             last_error TEXT,
             PRIMARY KEY (event_id, recipient)
         );
         CREATE INDEX IF NOT EXISTS message_deliveries_pending
             ON message_deliveries(recipient, delivered_at);

         -- Per-agent coordination memory. Each agent records what it dispatched,
         -- what came back, and what it learned, so recall ("have I done this
         -- before, and how did it go") is a query rather than a re-read of every
         -- transcript. This is MEMORY, not status: a dispatch's authoritative
         -- state lives in `assignments`, and mirroring it here would create a
         -- second source of truth that drifts.
         CREATE TABLE IF NOT EXISTS agent_board (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             agent_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             session_id TEXT,
             workspace TEXT,
             summary TEXT NOT NULL,
             -- Links a dispatch to its later report.
             correlation_id TEXT,
             created_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS agent_board_agent_created
             ON agent_board(agent_id, created_at DESC);
         CREATE INDEX IF NOT EXISTS agent_board_agent_workspace
             ON agent_board(agent_id, workspace);
"#,
    )?;
    add_missing_columns(connection)?;
    Ok(())
}

/// Add columns that a database created by an earlier build will not have.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op on a table that already exists, so a
/// new column in the DDL reaches fresh databases only. Without this, the first
/// write naming `tasks.profile` fails on every existing install.
fn add_missing_columns(connection: &Connection) -> Result<(), rusqlite::Error> {
    let existing: Vec<String> = {
        let mut stmt = connection.prepare("PRAGMA table_info(tasks)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<Result<_, _>>()?
    };
    if !existing.iter().any(|column| column == "profile") {
        connection.execute("ALTER TABLE tasks ADD COLUMN profile TEXT", [])?;
    }
    Ok(())
}
