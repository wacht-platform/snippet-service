use rusqlite::{OptionalExtension, params};

use super::{
    CoordinationDb, CoordinationDbError,
    types::{Handoff, SessionLease},
};

impl CoordinationDb {
    pub fn create_handoff(&self, handoff: &Handoff) -> Result<(), CoordinationDbError> {
        let content = serde_json::to_string(handoff).unwrap();
        self.with_connection(|conn| {
            conn.execute("INSERT INTO handoffs (id,goal_id,session_id,source_assignment_id,target_assignment_id,context_mode,content_json,content_hash,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![handoff.id, handoff.goal_id, handoff.session_id, handoff.source_assignment_id, handoff.target_assignment_id, serde_json::to_string(&handoff.context_mode).unwrap().trim_matches('"'), content, handoff.content_hash, handoff.created_at])?;
            Ok(())
        })
    }

    pub fn acknowledge_handoff(&self, id: &str, at: &str) -> Result<bool, CoordinationDbError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE handoffs SET acknowledged_at=?1 WHERE id=?2 AND acknowledged_at IS NULL",
                params![at, id],
            )? == 1)
        })
    }

    pub fn acquire_lease(
        &self,
        lease: &SessionLease,
    ) -> Result<Option<SessionLease>, CoordinationDbError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let active: Option<i64> = tx.query_row(
                "SELECT fencing_token FROM session_leases WHERE session_id=?1 AND released_at IS NULL AND expires_at > ?2",
                params![lease.session_id, lease.acquired_at],
                |r| r.get(0),
            ).optional()?;
            if active.is_some() { return Ok(None); }
            tx.execute(
                "UPDATE session_leases SET released_at=?1, release_reason='expired' WHERE session_id=?2 AND released_at IS NULL AND expires_at <= ?1",
                params![lease.acquired_at, lease.session_id],
            )?;
            let token = tx.query_row(
                "SELECT COALESCE(MAX(fencing_token),0)+1 FROM session_leases WHERE session_id=?1",
                params![lease.session_id],
                |r| r.get::<_, i64>(0),
            )? as u64;
            tx.execute("INSERT INTO session_leases (session_id,lease_id,assignment_id,agent_id,fencing_token,acquired_at,renewed_at,expires_at) VALUES (?1,?2,?3,?4,?5,?6,?6,?7)", params![lease.session_id,lease.lease_id,lease.assignment_id,lease.agent_id,token,lease.acquired_at,lease.expires_at])?;
            tx.commit()?;
            let mut acquired = lease.clone();
            acquired.fencing_token = token;
            Ok(Some(acquired))
        })
    }

    pub fn renew_lease(
        &self,
        lease_id: &str,
        fencing_token: u64,
        renewed_at: &str,
        expires_at: &str,
    ) -> Result<bool, CoordinationDbError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE session_leases SET renewed_at=?1, expires_at=?2 WHERE lease_id=?3 AND fencing_token=?4 AND released_at IS NULL",
                params![renewed_at, expires_at, lease_id, fencing_token],
            )? == 1)
        })
    }

    pub fn release_lease(
        &self,
        lease_id: &str,
        at: &str,
        reason: &str,
    ) -> Result<bool, CoordinationDbError> {
        self.with_connection(|conn| Ok(conn.execute("UPDATE session_leases SET released_at=?1, release_reason=?2 WHERE lease_id=?3 AND released_at IS NULL", params![at,reason,lease_id])? == 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{
        types::{Agent, AgentKind, AgentRole, AgentStatus},
        work::{Assignment, AssignmentStatus},
    };
    #[test]
    fn lease_is_single_active_owner() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.create_agent(&Agent {
            id: "w".into(),
            display_name: "W".into(),
            handle: "w".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 2,
            max_concurrent_sessions: 1,
            version: 1,
        })
        .unwrap();
        db.create_assignment(&Assignment {
            id: "a".into(),
            goal_id: "g".into(),
            session_id: "s".into(),
            agent_id: "w".into(),
            status: AssignmentStatus::Accepted,
            scope: "x".into(),
            definition_of_done: "y".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .unwrap();
        let l = SessionLease {
            session_id: "s".into(),
            lease_id: "l".into(),
            assignment_id: "a".into(),
            agent_id: "w".into(),
            fencing_token: 0,
            acquired_at: "1".into(),
            renewed_at: "1".into(),
            expires_at: "2".into(),
        };
        assert_eq!(
            db.acquire_lease(&l)
                .unwrap()
                .as_ref()
                .map(|x| x.fencing_token),
            Some(1)
        );
        assert!(db.renew_lease("l", 1, "1.5", "2.5").unwrap());
        assert!(!db.renew_lease("l", 99, "1.6", "2.6").unwrap());
        assert!(
            db.acquire_lease(&SessionLease {
                lease_id: "l2".into(),
                ..l.clone()
            })
            .unwrap()
            .is_none()
        );
        assert!(db.release_lease("l", "3", "handoff").unwrap());
        let next = db
            .acquire_lease(&SessionLease {
                lease_id: "l2".into(),
                ..l
            })
            .unwrap()
            .expect("released session can reacquire");
        assert_eq!(next.fencing_token, 2);
    }
}
