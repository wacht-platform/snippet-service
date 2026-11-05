//! Acknowledged handovers: the lease moves only when the incoming agent says so.

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Handover {
    pub id: String,
    pub task_id: String,
    pub from_agent: String,
    pub to_agent: String,
    pub done: String,
    pub next: String,
    pub risks: String,
    pub files: Vec<String>,
    pub created_at: String,
    pub acked_at: Option<String>,
}

impl Handover {
    pub fn state(&self) -> &'static str {
        if self.acked_at.is_some() { "acked" } else { "pending" }
    }

    /// The handover as the task API shows it.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id, "from": self.from_agent, "to": self.to_agent, "state": self.state(),
            "created_at": self.created_at, "acked_at": self.acked_at,
        })
    }
}

const COLUMNS: &str = "id, task_id, from_agent, to_agent, done, next, risks, files_json, created_at, acked_at";

fn from_row(r: &rusqlite::Row<'_>) -> Result<Handover, rusqlite::Error> {
    Ok(Handover {
        id: r.get(0)?,
        task_id: r.get(1)?,
        from_agent: r.get(2)?,
        to_agent: r.get(3)?,
        done: r.get(4)?,
        next: r.get(5)?,
        risks: r.get(6)?,
        files: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        created_at: r.get(8)?,
        acked_at: r.get(9)?,
    })
}

impl Store {
    /// Record a handover. The lease stays with `from_agent` until it is acked.
    pub fn create_handoff(&self, h: &Handover) -> Result<(), StoreError> {
        if let Some(open) = self.pending_handoff(&h.task_id)? {
            return Err(StoreError::InvalidInput(format!(
                "handover {} to `{}` is still waiting to be acknowledged",
                open.id, open.to_agent
            )));
        }
        self.with_connection(|c| {
            c.execute(
                &format!("INSERT INTO handoffs ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)"),
                params![
                    h.id, h.task_id, h.from_agent, h.to_agent, h.done, h.next, h.risks,
                    serde_json::to_string(&h.files).unwrap_or_else(|_| "[]".into()), h.created_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn pending_handoff(&self, task_id: &str) -> Result<Option<Handover>, StoreError> {
        self.with_connection(|c| {
            c.query_row(
                &format!("SELECT {COLUMNS} FROM handoffs WHERE task_id = ?1 AND acked_at IS NULL ORDER BY created_at DESC LIMIT 1"),
                params![task_id],
                from_row,
            )
            .optional()
        })
    }

    /// The latest handover, pending or acked.
    pub fn latest_handoff(&self, task_id: &str) -> Result<Option<Handover>, StoreError> {
        self.with_connection(|c| {
            c.query_row(
                &format!("SELECT {COLUMNS} FROM handoffs WHERE task_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1"),
                params![task_id],
                from_row,
            )
            .optional()
        })
    }

    /// The incoming agent acknowledges: the lease moves and a new token is minted.
    pub fn ack_handoff(&self, task_id: &str, by_agent: &str, now: &str) -> Result<Handover, StoreError> {
        let Some(mut h) = self.pending_handoff(task_id)? else {
            return Err(StoreError::InvalidInput("no handover is waiting on this task".into()));
        };
        if h.to_agent != by_agent {
            return Err(StoreError::InvalidInput(format!("the handover is addressed to `{}`", h.to_agent)));
        }
        self.transfer_task_session_lease(task_id, &h.from_agent, &h.to_agent)?;
        self.with_connection(|c| c.execute("UPDATE handoffs SET acked_at = ?2 WHERE id = ?1", params![h.id, now]))?;
        h.acked_at = Some(now.to_string());
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::Task;

    fn handover(task: &str, to: &str) -> Handover {
        Handover {
            id: format!("h-{task}-{to}"), task_id: task.into(), from_agent: "a".into(), to_agent: to.into(),
            done: "wrote it".into(), next: "review".into(), risks: "flaky".into(), files: vec!["src/a.rs".into()],
            created_at: "2026-01-01T00:00:00Z".into(), acked_at: None,
        }
    }

    #[test]
    fn the_old_holder_keeps_the_lease_until_the_new_one_acks() {
        let db = Store::open_in_memory().unwrap();
        db.create_task(&Task::filed_by_human("t".into(), "t".into(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into())).unwrap();
        for id in ["a", "b"] {
            db.create_agent(&crate::coordination::types::Agent {
                id: id.into(), display_name: id.into(), handle: id.into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                role: crate::coordination::types::AgentRole::Implementer, capabilities: vec![],
            })
            .unwrap();
        }
        db.add_task_agent_full("t", "a", "implementer", None, "", "active", "2026-01-01T00:00:00Z").unwrap();
        db.add_task_agent_full("t", "b", "reviewer", None, "", "waiting", "2026-01-01T00:00:00Z").unwrap();
        db.create_handoff(&handover("t", "b")).unwrap();
        assert!(db.create_handoff(&handover("t", "b")).is_err(), "one open handover at a time");
        assert_eq!(db.current_lease("t").unwrap().unwrap().agent_id, "a");

        assert!(db.ack_handoff("t", "a", "2026-01-01T00:12:00Z").is_err(), "only the addressee acks");
        let acked = db.ack_handoff("t", "b", "2026-01-01T00:12:00Z").unwrap();
        assert_eq!(acked.state(), "acked");
        let lease = db.current_lease("t").unwrap().unwrap();
        assert_eq!((lease.agent_id.as_str(), lease.fencing_token), ("b", 2));
        assert!(db.pending_handoff("t").unwrap().is_none());
    }
}
