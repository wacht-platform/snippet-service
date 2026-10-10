//! App-level stores: managed sessions, control settings, recurring jobs, and the
//! notification journal.
//!
//! These were JSON files under `~/.snippet`. They live in the same SQLite
//! database as everything else now, so there is one durable store for the whole
//! device rather than a store per subsystem — and a write is a transaction
//! rather than an atomic-file dance.
//!
//! Separate from `coordination::*` on purpose: that is the agent control plane,
//! this is the app's own bookkeeping. They share the connection, nothing else.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::mission_control::{ControlSettings, ManagedSession, SessionStatus};
use crate::recurring::{Delivery, RecurringJob, Schedule};
use crate::store::{Store, StoreError};

/// Where the app-level rows live for `root`.
///
/// Any root inside `~/.snippet` is the real device store and resolves to the
/// single database. A test root — a tempdir, or an explicit override — gets its
/// own file beside it, so tests stay isolated without a global override that
/// would leak between them.
pub fn app_db_path(root: &Path) -> PathBuf {
    if root.starts_with(crate::config::snippet_home()) {
        crate::store::default_db_path()
    } else {
        root.join("snippet.db")
    }
}

fn encode<T: Serialize>(what: &str, value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value)
        .map_err(|e| StoreError::AppDecode(format!("encode {what}: {e}")))
}

/// The persisted spelling of a session status.
fn status_str(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Active => "active",
        SessionStatus::Archived => "archived",
    }
}

fn status_from(raw: &str) -> SessionStatus {
    match raw {
        "archived" => SessionStatus::Archived,
        _ => SessionStatus::Active,
    }
}

impl Store {
    // ---- managed sessions -------------------------------------------------

    pub fn upsert_managed_session(&self, session: &ManagedSession) -> Result<(), StoreError> {
        let tags = encode("session tags", &session.tags)?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO managed_sessions
                   (id, label, workspace, status, created_at, updated_at, tags_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(id) DO UPDATE SET
                   label = excluded.label,
                   workspace = excluded.workspace,
                   status = excluded.status,
                   updated_at = excluded.updated_at,
                   tags_json = excluded.tags_json",
                params![
                    session.id,
                    session.label,
                    session.workspace.to_string_lossy(),
                    status_str(&session.status),
                    session.created_at as i64,
                    session.updated_at as i64,
                    tags,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_managed_session(&self, id: &str) -> Result<Option<ManagedSession>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, label, workspace, status, created_at, updated_at, tags_json
                 FROM managed_sessions WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], managed_session_from_row)?;
            match rows.next() {
                Some(row) => Ok(Some(row?)),
                None => Ok(None),
            }
        })
    }

    /// Every managed session, optionally only the active ones.
    ///
    /// Newest first: the UI lists recent work, and an unordered read would
    /// reshuffle between refreshes.
    pub fn list_managed_sessions(
        &self,
        active_only: bool,
    ) -> Result<Vec<ManagedSession>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, label, workspace, status, created_at, updated_at, tags_json
                 FROM managed_sessions
                 WHERE (?1 = 0 OR status = 'active')
                 ORDER BY created_at DESC, id",
            )?;
            let rows = stmt.query_map(params![active_only as i64], managed_session_from_row)?;
            rows.collect()
        })
    }

    pub fn managed_session_exists(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM managed_sessions WHERE id = ?1",
                    params![id],
                    |_| Ok(()),
                )
                .optional_present()?)
        })
    }

    // ---- control settings -------------------------------------------------

    pub fn load_control_settings(&self) -> Result<ControlSettings, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT mission_control_session_id, notification_policy
                 FROM control_settings WHERE id = 1",
            )?;
            let mut rows = stmt.query_map([], |row| {
                Ok(ControlSettings {
                    mission_control_session_id: row.get(0)?,
                    notification_policy: row.get(1)?,
                })
            })?;
            match rows.next() {
                Some(row) => Ok(row?),
                // Never written yet: the defaults are the defaults, and a read
                // must not create a row.
                None => Ok(ControlSettings::default()),
            }
        })
    }

    pub fn save_control_settings(&self, settings: &ControlSettings) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO control_settings
                   (id, mission_control_session_id, notification_policy)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                   mission_control_session_id = excluded.mission_control_session_id,
                   notification_policy = excluded.notification_policy",
                params![
                    settings.mission_control_session_id,
                    settings.notification_policy
                ],
            )?;
            Ok(())
        })
    }

    pub fn load_autonomy_json(&self) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT state_json FROM mission_autonomy WHERE id = 1")?;
            let mut rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.next().transpose()?)
        })
    }

    pub fn save_autonomy_json(&self, json: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO mission_autonomy (id, state_json) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET state_json = excluded.state_json",
                params![json],
            )?;
            Ok(())
        })
    }

    // ---- recurring jobs ---------------------------------------------------

    pub fn upsert_recurring_job(&self, job: &RecurringJob) -> Result<(), StoreError> {
        let schedule = encode("job schedule", &job.schedule)?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO recurring_jobs
                   (id, title, session_id, prompt, plan_path, schedule_json, delivery,
                    enabled, next_run_at, last_run_at, last_error, queued,
                    created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(id) DO UPDATE SET
                   title = excluded.title,
                   session_id = excluded.session_id,
                   prompt = excluded.prompt,
                   plan_path = excluded.plan_path,
                   schedule_json = excluded.schedule_json,
                   delivery = excluded.delivery,
                   enabled = excluded.enabled,
                   next_run_at = excluded.next_run_at,
                   last_run_at = excluded.last_run_at,
                   last_error = excluded.last_error,
                   queued = excluded.queued,
                   updated_at = excluded.updated_at",
                params![
                    job.id,
                    job.title,
                    job.session_id,
                    job.prompt,
                    job.plan_path,
                    schedule,
                    delivery_str(&job.delivery),
                    job.enabled as i64,
                    job.next_run_at as i64,
                    job.last_run_at.map(|v| v as i64),
                    job.last_error,
                    job.queued as i64,
                    job.created_at as i64,
                    job.updated_at as i64,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_recurring_job(&self, id: &str) -> Result<Option<RecurringJob>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM recurring_jobs WHERE id = ?1"
            ))?;
            let mut rows = stmt.query_map(params![id], recurring_job_from_row)?;
            match rows.next() {
                Some(row) => Ok(Some(row?)),
                None => Ok(None),
            }
        })
    }

    pub fn list_recurring_jobs(&self) -> Result<Vec<RecurringJob>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM recurring_jobs ORDER BY created_at, id"
            ))?;
            let rows = stmt.query_map([], recurring_job_from_row)?;
            rows.collect()
        })
    }

    pub fn delete_recurring_job(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute("DELETE FROM recurring_jobs WHERE id = ?1", params![id])? == 1)
        })
    }

    // ---- notification journal ---------------------------------------------

    pub fn append_notification_event(
        &self,
        mut event: serde_json::Value,
        retention_secs: i64,
    ) -> Result<serde_json::Value, StoreError> {
        let kind = event
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string();
        let now = chrono::Utc::now().timestamp();
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            if let Some(key) = event.get("source_key").and_then(|v| v.as_str()) {
                let existing = tx.query_row(
                    "SELECT payload_json FROM notification_journal JOIN notification_sources USING(event_id) WHERE source_key = ?1",
                    params![key], |row| row.get::<_, String>(0),
                ).optional()?;
                if let Some(raw) = existing {
                    return serde_json::from_str(&raw)
                        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
                            0, rusqlite::types::Type::Text, Box::new(error),
                        ));
                }
            }
            let created_at: i64 = tx.query_row(
                "SELECT MAX(?1, COALESCE((SELECT created_at FROM notification_clock WHERE id = 1), 0),
                 COALESCE((SELECT MAX(created_at) FROM notification_journal), 0))",
                params![now], |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO notification_clock VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET created_at = excluded.created_at",
                params![created_at],
            )?;
            let floor: i64 = tx.query_row(
                "SELECT COALESCE(MAX(event_id), 0) + 1 FROM notification_journal",
                [],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO notification_sequence (id, next_id) VALUES (1, ?1)
                 ON CONFLICT (id) DO UPDATE SET next_id = MAX(next_id, ?1)",
                params![floor],
            )?;
            let id: i64 = tx.query_row(
                "SELECT next_id FROM notification_sequence WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE notification_sequence SET next_id = ?1 WHERE id = 1",
                params![id + 1],
            )?;
            if let Some(obj) = event.as_object_mut() {
                obj.insert("event_id".into(), serde_json::json!(id));
                obj.insert("notification_id".into(), serde_json::json!(uuid::Uuid::new_v4().to_string()));
                obj.insert("created_at".into(), serde_json::json!(created_at));
                obj.insert("expires_at".into(), serde_json::json!(now + retention_secs));
            }
            tx.execute(
                "INSERT INTO notification_journal (event_id, kind, created_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, kind, created_at, event.to_string()],
            )?;
            if let Some(key) = event.get("source_key").and_then(|v| v.as_str()) {
                tx.execute("INSERT OR IGNORE INTO notification_sources VALUES (?1, ?2)", params![key, id])?;
            }
            tx.execute("DELETE FROM notification_sources WHERE event_id IN (SELECT event_id FROM notification_journal WHERE created_at < ?1)", params![now - retention_secs])?;
            tx.execute(
                "DELETE FROM notification_journal WHERE created_at < ?1",
                params![now - retention_secs],
            )?;
            tx.commit()?;
            Ok(event)
        })
    }

    pub fn notification_events_after_cursor(
        &self,
        created_at: i64,
        event_id: u64,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, created_at, payload_json FROM notification_journal
                 WHERE (created_at, event_id) > (?1, ?2)
                 ORDER BY created_at, event_id LIMIT 501",
            )?;
            let rows = stmt.query_map(params![created_at, event_id as i64], |row| {
                let id: i64 = row.get(0)?;
                let timestamp: i64 = row.get(1)?;
                let raw: String = row.get(2)?;
                let mut value = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok().filter(|v| v.is_object()).unwrap_or_else(|| serde_json::json!({}));
                value["event_id"] = serde_json::json!(id);
                value["created_at"] = serde_json::json!(timestamp);
                Ok(value)
            })?;
            rows.collect()
        })
    }

    pub fn notification_events_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, created_at, payload_json FROM notification_journal
                 WHERE event_id > ?1 ORDER BY event_id LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![since as i64, limit.clamp(1, 501) as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, created_at, raw) = row?;
                let mut value = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .filter(|value| value.is_object())
                    .unwrap_or_else(|| serde_json::json!({}));
                // The id and timestamp are columns, not payload fields, so they
                // are re-attached here. A caller filtering on `event_id` (which
                // is how a client resumes) would otherwise see nothing.
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("event_id".into(), serde_json::json!(id));
                    obj.entry("created_at")
                        .or_insert_with(|| serde_json::json!(created_at));
                }
                out.push(value);
            }
            Ok(out)
        })
    }

}

/// `Option` from a presence query, so a caller reads "exists" without an error
/// path for the ordinary absent case.
trait OptionalPresent {
    fn optional_present(self) -> Result<bool, rusqlite::Error>;
}

impl OptionalPresent for Result<(), rusqlite::Error> {
    fn optional_present(self) -> Result<bool, rusqlite::Error> {
        match self {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

const JOB_COLUMNS: &str = "id, title, session_id, prompt, plan_path, schedule_json, delivery, \
                           enabled, next_run_at, last_run_at, last_error, queued, \
                           created_at, updated_at";

fn delivery_str(delivery: &Delivery) -> &'static str {
    match delivery {
        Delivery::Goal => "goal",
        Delivery::Message => "message",
    }
}

fn delivery_from(raw: &str) -> Delivery {
    match raw {
        "message" => Delivery::Message,
        _ => Delivery::Goal,
    }
}

fn managed_session_from_row(row: &rusqlite::Row<'_>) -> Result<ManagedSession, rusqlite::Error> {
    let raw_tags: String = row.get(6)?;
    let tags = serde_json::from_str(&raw_tags).unwrap_or_default();
    Ok(ManagedSession {
        id: row.get(0)?,
        label: row.get(1)?,
        workspace: PathBuf::from(row.get::<_, String>(2)?),
        status: status_from(&row.get::<_, String>(3)?),
        created_at: row.get::<_, i64>(4)? as u64,
        updated_at: row.get::<_, i64>(5)? as u64,
        tags,
    })
}

fn recurring_job_from_row(row: &rusqlite::Row<'_>) -> Result<RecurringJob, rusqlite::Error> {
    let raw_schedule: String = row.get(5)?;
    // A schedule that cannot be decoded is a corrupt row; default to a daily
    // run rather than dropping the job, so the user still sees it exists.
    let schedule: Schedule = serde_json::from_str(&raw_schedule)
        .unwrap_or(Schedule::Daily { hour: 0, minute: 0 });
    Ok(RecurringJob {
        id: row.get(0)?,
        title: row.get(1)?,
        session_id: row.get(2)?,
        prompt: row.get(3)?,
        plan_path: row.get(4)?,
        schedule,
        delivery: delivery_from(&row.get::<_, String>(6)?),
        enabled: row.get::<_, i64>(7)? != 0,
        next_run_at: row.get::<_, i64>(8)? as u64,
        last_run_at: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        last_error: row.get(10)?,
        queued: row.get::<_, i64>(11)? != 0,
        created_at: row.get::<_, i64>(12)? as u64,
        updated_at: row.get::<_, i64>(13)? as u64,
    })
}
