//! Durable direct messaging between coordination participants.
//!
//! A direct message is an ordinary board event on a thread whose scope is
//! `direct`. The thread id is derived from the two participants, so both ends
//! always resolve the same conversation instead of manufacturing a second one.
//!
//! Delivery is recorded per recipient in `message_deliveries`. The pending query
//! is a LEFT JOIN rather than a read of that table alone, so a message whose
//! delivery row was never written (a crash between the event insert and the
//! delivery insert) is still discovered and delivered once.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::types::CoordinationEvent;
use super::{Store, StoreError};

/// A participant reference in its canonical wire form: `kind:id`.
pub fn actor_ref(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// The canonical thread id for a pair of participants.
///
/// The pair is SORTED, so `a → b` and `b → a` are the same conversation. Two
/// unsorted ids would give each direction its own thread and the two ends would
/// never see each other's messages.
pub fn direct_thread_id(a: (&str, &str), b: (&str, &str)) -> String {
    let mut ends = [actor_ref(a.0, a.1), actor_ref(b.0, b.1)];
    ends.sort();
    format!("direct:{}", ends.join("|"))
}

/// One message awaiting delivery to one recipient.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDirectMessage {
    pub event: CoordinationEvent,
    pub recipient_kind: String,
    pub recipient_id: String,
}

/// A direct conversation as listed for one participant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectThreadSummary {
    pub thread_id: String,
    pub title: String,
    /// The other participant's kind and id.
    pub peer_kind: String,
    pub peer_id: String,
    pub unread: i64,
    pub last_sequence: i64,
    pub created_at: String,
}

const EVENT_COLUMNS: &str = "event_id, thread_id, partition_key, sequence, event_type,
     actor_kind, actor_id, payload_version, payload_json, causation_id, correlation_id,
     idempotency_key, created_at";

/// [`EVENT_COLUMNS`] qualified with the `e` alias.
///
/// The delivery query joins `message_deliveries`, which also has an `event_id`, so
/// the shared unqualified list is ambiguous there.
const EVENT_COLUMNS_QUALIFIED: &str = "e.event_id, e.thread_id, e.partition_key, e.sequence,
     e.event_type, e.actor_kind, e.actor_id, e.payload_version, e.payload_json,
     e.causation_id, e.correlation_id, e.idempotency_key, e.created_at";

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<CoordinationEvent, rusqlite::Error> {
    let payload_json: String = row.get(8)?;
    let payload = serde_json::from_str(&payload_json).map_err(|error| {
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

impl Store {
    /// Send one direct message, recording the conversation and the recipient's
    /// delivery state alongside the event.
    ///
    /// The thread and its participants are written FIRST with `INSERT OR IGNORE`.
    /// `append_event` also inserts the thread row, but with a generic scope, and
    /// its `OR IGNORE` means whichever ran first wins — so writing the direct
    /// thread here is what guarantees the scope is `direct`.
    pub fn send_direct_message(
        &self,
        sender: (&str, &str),
        recipient: (&str, &str),
        body: &str,
        idempotency_key: &str,
        created_at: &str,
    ) -> Result<CoordinationEvent, StoreError> {
        self.send_direct_message_from(sender, recipient, body, idempotency_key, created_at, None)
    }

    /// Send a direct message that originated in a SESSION.
    ///
    /// `origin_session` is what makes a dispatch-from-a-session traceable and
    /// answerable: the recipient's reply comes back to that session, and the
    /// session's own record shows the message it sent out. Without it the
    /// sender's session has no record of the exchange at all.
    #[allow(clippy::too_many_arguments)]
    pub fn send_direct_message_from(
        &self,
        sender: (&str, &str),
        recipient: (&str, &str),
        body: &str,
        idempotency_key: &str,
        created_at: &str,
        origin_session: Option<&str>,
    ) -> Result<CoordinationEvent, StoreError> {
        let thread_id = direct_thread_id(sender, recipient);
        let title = format!(
            "{} ↔ {}",
            recipient.1,
            actor_ref(sender.0, sender.1)
        );
        let sender_ref = actor_ref(sender.0, sender.1);
        let recipient_ref = actor_ref(recipient.0, recipient.1);

        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO board_threads (id, scope, subject_id, title, created_at)
                 VALUES (?1, 'direct', ?2, ?3, ?4)",
                params![thread_id, thread_id, title, created_at],
            )?;
            for (kind, id) in [sender, recipient] {
                conn.execute(
                    "INSERT OR IGNORE INTO board_participants (thread_id, actor_id, actor_kind)
                     VALUES (?1, ?2, ?3)",
                    params![thread_id, id, kind],
                )?;
            }
            Ok(())
        })?;

        let event = CoordinationEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            thread_id: thread_id.clone(),
            partition_key: format!("thread:{thread_id}"),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: sender.0.to_string(),
            actor_id: sender.1.to_string(),
            payload_version: 1,
            payload: {
                let mut payload = serde_json::json!({
                    "body": body,
                    "recipient": recipient_ref,
                });
                if let Some(session) = origin_session.filter(|s| !s.trim().is_empty()) {
                    payload["origin_session"] = serde_json::json!(session);
                }
                payload
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: idempotency_key.to_string(),
            created_at: created_at.to_string(),
        };
        let saved = self.append_event(&event)?;

        // The sender has by definition already seen this message; the recipient
        // has not, so its `read_at` stays NULL until it reads the thread.
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO message_deliveries
                 (event_id, recipient_kind, recipient_id, recipient, queued_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    saved.event_id,
                    recipient.0,
                    recipient.1,
                    recipient_ref,
                    created_at
                ],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO message_deliveries
                 (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at, read_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?5)",
                params![
                    saved.event_id,
                    sender.0,
                    sender.1,
                    sender_ref,
                    created_at
                ],
            )?;
            Ok(())
        })?;

        Ok(saved)
    }

    /// Direct messages not yet handed to their recipient, oldest first.
    ///
    /// A LEFT JOIN against `message_deliveries`: an event with no delivery row is
    /// reported as pending, which is what makes a crash between the two inserts
    /// self-healing rather than a silently dropped message.
    pub fn list_pending_direct_deliveries(
        &self,
        limit: u32,
    ) -> Result<Vec<PendingDirectMessage>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS_QUALIFIED}, p.actor_kind, p.actor_id
                 FROM board_events e
                 JOIN board_threads t ON t.id = e.thread_id AND t.scope = 'direct'
                 JOIN board_participants p
                   ON p.thread_id = e.thread_id AND p.actor_id <> e.actor_id
                 LEFT JOIN message_deliveries d
                   ON d.event_id = e.event_id
                  AND d.recipient_kind = p.actor_kind
                  AND d.recipient_id = p.actor_id
                 WHERE d.event_id IS NULL OR d.delivered_at IS NULL
                 ORDER BY e.created_at, e.sequence
                 LIMIT ?1"
            ))?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(PendingDirectMessage {
                    event: event_from_row(row)?,
                    recipient_kind: row.get(13)?,
                    recipient_id: row.get(14)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Mark a message delivered to one recipient. Idempotent.
    pub fn mark_direct_delivered(
        &self,
        event_id: &str,
        recipient_kind: &str,
        recipient_id: &str,
        at: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO message_deliveries
                 (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(event_id, recipient) DO UPDATE SET
                    delivered_at = COALESCE(message_deliveries.delivered_at, excluded.delivered_at)",
                params![
                    event_id,
                    recipient_kind,
                    recipient_id,
                    actor_ref(recipient_kind, recipient_id),
                    at
                ],
            )?;
            Ok(())
        })
    }

    /// Count a failed delivery attempt so a permanently undeliverable message is
    /// visible rather than retried forever in silence.
    pub fn record_direct_delivery_failure(
        &self,
        event_id: &str,
        recipient_kind: &str,
        recipient_id: &str,
        error: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO message_deliveries
                 (event_id, recipient_kind, recipient_id, recipient, queued_at, attempts, last_error)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5)
                 ON CONFLICT(event_id, recipient) DO UPDATE SET
                    attempts = message_deliveries.attempts + 1,
                    last_error = excluded.last_error",
                params![
                    event_id,
                    recipient_kind,
                    recipient_id,
                    actor_ref(recipient_kind, recipient_id),
                    error
                ],
            )?;
            Ok(())
        })
    }

    /// Mark every unread message in a thread as read for one participant.
    pub fn mark_direct_read(
        &self,
        reader_kind: &str,
        reader_id: &str,
        thread_id: &str,
        at: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE message_deliveries SET read_at = ?1
                 WHERE recipient_kind = ?2 AND recipient_id = ?3
                   AND event_id IN (SELECT event_id FROM board_events WHERE thread_id = ?4)",
                params![at, reader_kind, reader_id, thread_id],
            )?;
            Ok(())
        })
    }

    /// One participant's direct conversations, most recent first.
    pub fn list_direct_threads(
        &self,
        actor_kind: &str,
        actor_id: &str,
    ) -> Result<Vec<DirectThreadSummary>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.title, t.created_at, p.actor_kind, p.actor_id,
                        (SELECT COALESCE(MAX(sequence), 0) FROM board_events e
                          WHERE e.thread_id = t.id),
                        (SELECT COUNT(*) FROM message_deliveries d
                          JOIN board_events e ON e.event_id = d.event_id
                          WHERE e.thread_id = t.id
                            AND d.recipient_kind = ?1 AND d.recipient_id = ?2
                            AND d.read_at IS NULL)
                 FROM board_threads t
                 JOIN board_participants me
                   ON me.thread_id = t.id AND me.actor_id = ?2 AND me.actor_kind = ?1
                 JOIN board_participants p
                   ON p.thread_id = t.id AND NOT (p.actor_id = ?2 AND p.actor_kind = ?1)
                 WHERE t.scope = 'direct'
                 ORDER BY t.created_at DESC",
            )?;
            let rows = stmt.query_map(params![actor_kind, actor_id], |row| {
                Ok(DirectThreadSummary {
                    thread_id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: row.get(2)?,
                    peer_kind: row.get(3)?,
                    peer_id: row.get(4)?,
                    last_sequence: row.get(5)?,
                    unread: row.get(6)?,
                })
            })?;
            rows.collect()
        })
    }

    /// One page of a direct thread's messages, oldest first.
    ///
    /// `after_sequence: 0` means "from the beginning"; the caller pages forward
    /// with the returned newest sequence.
    pub fn direct_thread_events(
        &self,
        thread_id: &str,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Vec<CoordinationEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM board_events
                 WHERE thread_id = ?1 AND sequence > ?2
                 ORDER BY sequence LIMIT ?3"
            ))?;
            let rows = stmt.query_map(
                params![thread_id, after_sequence, limit],
                event_from_row,
            )?;
            rows.collect()
        })
    }

    /// The most recent messages of a thread, oldest first — the slice handed to a
    /// recipient when it is woken, so the exchange reads as a conversation.
    pub fn recent_direct_thread_events(
        &self,
        thread_id: &str,
        limit: u32,
    ) -> Result<Vec<CoordinationEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM board_events
                 WHERE thread_id = ?1 ORDER BY sequence DESC LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![thread_id, limit], event_from_row)?;
            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            Ok(events)
        })
    }

}

/// The human participant of the local device.
pub const LOCAL_HUMAN_ID: &str = "local";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentRole, AgentStatus};

    fn worker(id: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
        }
    }

    #[test]
    fn thread_id_is_order_independent() {
        let forward = direct_thread_id(("agent", "a"), ("human", "local"));
        let backward = direct_thread_id(("human", "local"), ("agent", "a"));
        assert_eq!(forward, backward);
        assert!(forward.starts_with("direct:"));
    }

    #[test]
    fn thread_id_separates_different_pairs() {
        assert_ne!(
            direct_thread_id(("agent", "a"), ("agent", "b")),
            direct_thread_id(("agent", "a"), ("agent", "c"))
        );
    }

    #[test]
    fn a_direct_message_is_pending_for_the_recipient_only() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        db.send_direct_message(
            ("agent", "a"),
            ("agent", "b"),
            "hello",
            "key-1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let pending = db.list_pending_direct_deliveries(50).unwrap();
        assert_eq!(pending.len(), 1, "exactly one undelivered recipient");
        assert_eq!(pending[0].recipient_id, "b");
        assert_eq!(pending[0].event.payload["body"], "hello");
    }

    #[test]
    fn replaying_an_idempotency_key_does_not_duplicate() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        for _ in 0..2 {
            db.send_direct_message(
                ("agent", "a"),
                ("agent", "b"),
                "hello",
                "same-key",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        let thread = direct_thread_id(("agent", "a"), ("agent", "b"));
        assert_eq!(db.direct_thread_events(&thread, 0, 50).unwrap().len(), 1);
    }

    #[test]
    fn delivery_is_recorded_then_absent_from_pending() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        let saved = db
            .send_direct_message(
                ("agent", "a"),
                ("agent", "b"),
                "hello",
                "key-1",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        db.mark_direct_delivered(&saved.event_id, "agent", "b", "2026-01-01T00:00:01Z")
            .unwrap();
        assert!(db.list_pending_direct_deliveries(50).unwrap().is_empty());
    }

    #[test]
    fn an_event_without_a_delivery_row_is_still_pending() {
        // Simulates a crash between the event insert and the delivery insert.
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        let thread = direct_thread_id(("agent", "a"), ("agent", "b"));
        db.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO board_threads (id, scope, subject_id, title, created_at)
                 VALUES (?1, 'direct', ?1, 'pair', '2026-01-01T00:00:00Z')",
                params![thread],
            )?;
            for id in ["a", "b"] {
                conn.execute(
                    "INSERT OR IGNORE INTO board_participants (thread_id, actor_id, actor_kind)
                     VALUES (?1, ?2, 'agent')",
                    params![thread, id],
                )?;
            }
            Ok(())
        })
        .unwrap();
        db.append_event(&CoordinationEvent {
            event_id: "e1".into(),
            thread_id: thread.clone(),
            partition_key: format!("thread:{thread}"),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: "agent".into(),
            actor_id: "a".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "orphan"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "orphan-key".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();

        let pending = db.list_pending_direct_deliveries(50).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].recipient_id, "b");
    }

    #[test]
    fn unread_is_counted_per_participant_and_cleared_on_read() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        let thread = direct_thread_id(("agent", "a"), ("agent", "b"));
        db.send_direct_message(("agent", "a"), ("agent", "b"), "one", "k1", "t1")
            .unwrap();
        db.send_direct_message(("agent", "a"), ("agent", "b"), "two", "k2", "t2")
            .unwrap();

        let for_b = db.list_direct_threads("agent", "b").unwrap();
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].unread, 2);
        assert_eq!(for_b[0].peer_id, "a");

        let for_a = db.list_direct_threads("agent", "a").unwrap();
        assert_eq!(for_a[0].unread, 0, "sender's own messages are already read");

        db.mark_direct_read("agent", "b", &thread, "t3").unwrap();
        assert_eq!(db.list_direct_threads("agent", "b").unwrap()[0].unread, 0);
    }

    #[test]
    fn both_directions_share_one_thread() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        let thread = direct_thread_id(("agent", "a"), ("agent", "b"));
        db.send_direct_message(("agent", "a"), ("agent", "b"), "ping", "k1", "t1")
            .unwrap();
        db.send_direct_message(("agent", "b"), ("agent", "a"), "pong", "k2", "t2")
            .unwrap();

        let events = db.direct_thread_events(&thread, 0, 50).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].actor_id, "a");
        assert_eq!(events[1].actor_id, "b");
        assert_eq!(
            db.list_direct_threads("agent", "a").unwrap().len(),
            1,
            "one conversation, not one per direction"
        );
    }

    #[test]
    fn recent_events_return_the_newest_slice_in_order() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        let thread = direct_thread_id(("agent", "a"), ("agent", "b"));
        for i in 0..5 {
            db.send_direct_message(
                ("agent", "a"),
                ("agent", "b"),
                &format!("m{i}"),
                &format!("k{i}"),
                &format!("2026-01-01T00:00:0{i}Z"),
            )
            .unwrap();
        }
        let recent = db.recent_direct_thread_events(&thread, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].payload["body"], "m3");
        assert_eq!(recent[1].payload["body"], "m4");
    }
}
