use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use super::{CoordinationDb, CoordinationDbError, types::CoordinationEvent};

impl CoordinationDb {
    /// Append an event and its outbox entry atomically. Repeating an idempotency
    /// key returns the original event without creating a duplicate sequence.
    pub fn append_event(
        &self,
        event: &CoordinationEvent,
    ) -> Result<CoordinationEvent, CoordinationDbError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            if let Some(existing) = tx
                .query_row(
                    "SELECT event_id, thread_id, partition_key, sequence, event_type,
                            actor_kind, actor_id, payload_version, payload_json,
                            causation_id, correlation_id, idempotency_key, created_at
                     FROM board_events WHERE actor_id = ?1 AND idempotency_key = ?2",
                    params![event.actor_id, event.idempotency_key],
                    event_from_row,
                )
                .optional()?
            {
                tx.commit()?;
                return Ok(existing);
            }

            tx.execute(
                "INSERT OR IGNORE INTO board_threads
                 (id, scope, subject_id, title, created_at)
                 VALUES (?1, 'system', NULL, 'Coordination', ?2)",
                params![event.thread_id, event.created_at],
            )?;

            let next_sequence: u64 = tx.query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM board_events
                     WHERE partition_key = ?1",
                params![event.partition_key],
                |row| row.get(0),
            )?;
            let sequence = if event.sequence == 0 {
                next_sequence
            } else if next_sequence != event.sequence {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "event sequence must be {next_sequence}"
                )));
            } else {
                event.sequence
            };

            let payload = serde_json::to_string(&event.payload)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            tx.execute(
                "INSERT INTO board_events
                 (event_id, thread_id, partition_key, sequence, event_type, actor_kind,
                  actor_id, payload_version, payload_json, causation_id, correlation_id,
                  idempotency_key, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    event.event_id,
                    event.thread_id,
                    event.partition_key,
                    sequence,
                    event.event_type,
                    event.actor_kind,
                    event.actor_id,
                    event.payload_version,
                    payload,
                    event.causation_id,
                    event.correlation_id,
                    event.idempotency_key,
                    event.created_at,
                ],
            )?;
            tx.execute(
                "INSERT INTO outbox (event_id, available_at) VALUES (?1, ?2)",
                params![event.event_id, event.created_at],
            )?;
            tx.commit()?;
            let mut saved = event.clone();
            saved.sequence = sequence;
            Ok(saved)
        })
    }

    pub fn events_for_thread(
        &self,
        thread_id: &str,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Vec<CoordinationEvent>, CoordinationDbError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, thread_id, partition_key, sequence, event_type,
                        actor_kind, actor_id, payload_version, payload_json,
                        causation_id, correlation_id, idempotency_key, created_at
                 FROM board_events WHERE thread_id = ?1 AND sequence > ?2
                 ORDER BY sequence LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![thread_id, after_sequence, limit], event_from_row)?;
            rows.collect()
        })
    }

    pub fn events_after(
        &self,
        partition_key: &str,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Vec<CoordinationEvent>, CoordinationDbError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, thread_id, partition_key, sequence, event_type,
                        actor_kind, actor_id, payload_version, payload_json,
                        causation_id, correlation_id, idempotency_key, created_at
                 FROM board_events
                 WHERE partition_key = ?1 AND sequence > ?2
                 ORDER BY sequence LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![partition_key, after_sequence, limit],
                event_from_row,
            )?;
            rows.collect()
        })
    }
}

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<CoordinationEvent, rusqlite::Error> {
    let payload_json: String = row.get(8)?;
    let payload: Value = serde_json::from_str(&payload_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(CoordinationEvent {
        event_id: row.get(0)?,
        thread_id: row.get(1)?,
        partition_key: row.get(2)?,
        sequence: row.get(3)?,
        event_type: row.get(4)?,
        actor_kind: row.get(5)?,
        actor_id: row.get(6)?,
        payload_version: row.get(7)?,
        payload,
        causation_id: row.get(9)?,
        correlation_id: row.get(10)?,
        idempotency_key: row.get(11)?,
        created_at: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(sequence: u64, key: &str) -> CoordinationEvent {
        CoordinationEvent {
            event_id: format!("event-{key}"),
            thread_id: "thread-1".into(),
            partition_key: "goal:1".into(),
            sequence,
            event_type: "message.posted".into(),
            actor_kind: "agent".into(),
            actor_id: "agent-1".into(),
            payload_version: 1,
            payload: json!({"body": key}),
            causation_id: None,
            correlation_id: Some("goal-1".into()),
            idempotency_key: key.into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn append_replay_is_idempotent() {
        let db = CoordinationDb::open_in_memory().unwrap();
        let first = db.append_event(&event(1, "one")).unwrap();
        let replay = db.append_event(&event(1, "one")).unwrap();
        assert_eq!(replay, first);
        assert_eq!(db.events_after("goal:1", 0, 20).unwrap().len(), 1);
    }

    #[test]
    fn events_for_thread_filters_other_threads() {
        let db = CoordinationDb::open_in_memory().unwrap();
        let first = event(1, "first");
        db.append_event(&first).unwrap();
        let mut second = event(1, "second");
        second.event_id = "event-second".into();
        second.thread_id = "thread-other".into();
        second.partition_key = "thread:other".into();
        db.append_event(&second).unwrap();
        let events = db.events_for_thread("thread-1", 0, 20).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].idempotency_key, "first");
    }

    #[test]
    fn append_assigns_zero_sequence() {
        let db = CoordinationDb::open_in_memory().unwrap();
        let mut first = event(0, "auto");
        first.event_id = "event-auto".into();
        let saved = db.append_event(&first).unwrap();
        assert_eq!(saved.sequence, 1);
    }

    #[test]
    fn sequences_are_partition_local() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.append_event(&event(1, "one")).unwrap();
        let mut other = event(1, "two");
        other.partition_key = "session:1".into();
        other.event_id = "event-two".into();
        db.append_event(&other).unwrap();
        assert_eq!(db.events_after("goal:1", 0, 20).unwrap()[0].sequence, 1);
        assert_eq!(db.events_after("session:1", 0, 20).unwrap()[0].sequence, 1);
    }
}
