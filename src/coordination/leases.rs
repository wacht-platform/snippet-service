//! Session leases with fencing tokens. The roster says who is `active` on a
//! task; each change of holder is recorded here with a token that only grows,
//! so a write made under an older lease can be told from the current one.

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Lease {
    pub task_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub fencing_token: u64,
    pub acquired_at: String,
}

impl Store {
    /// Make the lease record agree with the roster's active member, minting a new
    /// token when the holder changed. Idempotent. `None` when nobody is active.
    pub fn sync_task_lease(&self, task_id: &str, now: &str) -> Result<Option<Lease>, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let task: Option<String> = tx
                .query_row("SELECT session_id FROM tasks WHERE id = ?1", params![task_id], |r| r.get(0))
                .optional()?;
            let Some(session_id) = task else { return Ok(None) };
            let holder: Option<String> = tx
                .query_row(
                    "SELECT agent_id FROM task_agents
                     WHERE task_id = ?1 AND status = 'active' AND removed_at IS NULL
                     ORDER BY added_at LIMIT 1",
                    params![task_id],
                    |r| r.get(0),
                )
                .optional()?;
            let current = open_lease(&tx, task_id)?;
            let lease = match (holder, current) {
                (Some(h), Some(c)) if c.agent_id == h => Some(c),
                (Some(h), current) => {
                    if current.is_some() {
                        tx.execute(
                            "UPDATE session_leases SET released_at = ?2, release_reason = 'transferred'
                             WHERE task_id = ?1 AND released_at IS NULL",
                            params![task_id, now],
                        )?;
                    }
                    let token: i64 = tx.query_row(
                        "SELECT COALESCE(MAX(fencing_token), 0) + 1 FROM session_leases WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )?;
                    tx.execute(
                        "INSERT INTO session_leases (task_id, session_id, agent_id, fencing_token, acquired_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![task_id, session_id, h, token, now],
                    )?;
                    Some(Lease { task_id: task_id.into(), session_id, agent_id: h, fencing_token: token as u64, acquired_at: now.into() })
                }
                (None, Some(_)) => {
                    tx.execute(
                        "UPDATE session_leases SET released_at = ?2, release_reason = 'vacated'
                         WHERE task_id = ?1 AND released_at IS NULL",
                        params![task_id, now],
                    )?;
                    None
                }
                (None, None) => None,
            };
            tx.commit()?;
            Ok(lease)
        })
    }

    /// The recorded lease, without syncing it; for read-only views.
    pub fn lease_of(&self, task_id: &str) -> Result<Option<Lease>, StoreError> {
        self.with_connection(|conn| open_lease(conn, task_id))
    }

    /// The task's current lease, after syncing it with the roster.
    pub fn current_lease(&self, task_id: &str) -> Result<Option<Lease>, StoreError> {
        self.sync_task_lease(task_id, &chrono::Utc::now().to_rfc3339())
    }

    /// Whether `token` is still the task's current fencing token.
    pub fn lease_token_is_current(&self, task_id: &str, token: u64) -> Result<bool, StoreError> {
        Ok(self.current_lease(task_id)?.is_none_or(|l| l.fencing_token == token))
    }
}

fn open_lease(conn: &rusqlite::Connection, task_id: &str) -> Result<Option<Lease>, rusqlite::Error> {
    conn.query_row(
        "SELECT task_id, session_id, agent_id, fencing_token, acquired_at FROM session_leases
         WHERE task_id = ?1 AND released_at IS NULL",
        params![task_id],
        |r| {
            Ok(Lease {
                task_id: r.get(0)?,
                session_id: r.get(1)?,
                agent_id: r.get(2)?,
                fencing_token: r.get::<_, i64>(3)? as u64,
                acquired_at: r.get(4)?,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::Task;

    #[test]
    fn tokens_grow_with_each_change_of_holder_and_stale_ones_are_refused() {
        let db = Store::open_in_memory().unwrap();
        let task = Task::filed_by_human("t".into(), "t".into(), "d".into(), "s1".into(), 0, "2026-01-01T00:00:00Z".into());
        db.create_task(&task).unwrap();
        for id in ["a", "b"] {
            db.create_agent(&crate::coordination::types::Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                role: crate::coordination::types::AgentRole::Implementer,
                capabilities: vec![],
            })
            .unwrap();
        }
        assert!(db.current_lease("t").unwrap().is_none());
        db.add_task_agent_full("t", "a", "implementer", None, "", "active", "2026-01-01T00:00:00Z").unwrap();
        db.add_task_agent_full("t", "b", "reviewer", None, "", "waiting", "2026-01-01T00:00:00Z").unwrap();
        let first = db.current_lease("t").unwrap().unwrap();
        assert_eq!((first.agent_id.as_str(), first.fencing_token), ("a", 1));
        assert_eq!(db.current_lease("t").unwrap().unwrap().fencing_token, 1, "idempotent");
        db.transfer_task_session_lease("t", "a", "b").unwrap();
        let second = db.current_lease("t").unwrap().unwrap();
        assert_eq!((second.agent_id.as_str(), second.fencing_token), ("b", 2));
        assert!(!db.lease_token_is_current("t", 1).unwrap());
        assert!(db.lease_token_is_current("t", 2).unwrap());
    }
}
