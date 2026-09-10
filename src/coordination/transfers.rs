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

    pub fn get_handoff(&self, id: &str) -> Result<Option<Handoff>, CoordinationDbError> {
        let raw = self.with_connection(|conn| {
            conn.query_row(
                "SELECT content_json FROM handoffs WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })?;
        decode_handoff(raw)
    }

    /// The oldest handoff targeting `target_assignment_id`, if any. A successor
    /// must acknowledge this before it can take the turn.
    pub fn handoff_for_target(
        &self,
        target_assignment_id: &str,
    ) -> Result<Option<Handoff>, CoordinationDbError> {
        let raw = self.with_connection(|conn| {
            conn.query_row(
                "SELECT content_json FROM handoffs
                 WHERE target_assignment_id = ?1
                 ORDER BY created_at ASC, id ASC LIMIT 1",
                params![target_assignment_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })?;
        decode_handoff(raw)
    }

    /// True when an unacknowledged handoff targets this assignment — i.e. the
    /// handover is incomplete and the turn must not change owners yet.
    pub fn has_unacknowledged_handoff(
        &self,
        target_assignment_id: &str,
    ) -> Result<bool, CoordinationDbError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM handoffs
                 WHERE target_assignment_id = ?1 AND acknowledged_at IS NULL",
                params![target_assignment_id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
    }

    pub fn acknowledge_handoff_by_target(
        &self,
        target_assignment_id: &str,
        at: &str,
    ) -> Result<Option<Handoff>, CoordinationDbError> {
        let raw = self.with_connection(|conn| {
            let changed = conn.execute(
                "UPDATE handoffs SET acknowledged_at=?1
                 WHERE target_assignment_id=?2 AND acknowledged_at IS NULL",
                params![at, target_assignment_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            conn.query_row(
                "SELECT content_json FROM handoffs
                 WHERE target_assignment_id = ?1
                 ORDER BY created_at ASC, id ASC LIMIT 1",
                params![target_assignment_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })?;
        decode_handoff(raw)
    }

    /// Handoffs that no successor has acknowledged yet, oldest first. Drives the
    /// handoff inspector: the human (or a successor agent) sees what is waiting.
    pub fn list_pending_handoffs(&self) -> Result<Vec<Handoff>, CoordinationDbError> {
        let raws = self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content_json FROM handoffs
                 WHERE acknowledged_at IS NULL
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;
        raws.into_iter()
            .map(|raw| {
                serde_json::from_str(&raw)
                    .map_err(|e| CoordinationDbError::HandoffDecode(e.to_string()))
            })
            .collect()
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

    /// The current unexpired, unreleased lease holder for a session, if any.
    /// Highest fencing token wins if several rows exist.
    pub fn active_lease(
        &self,
        session_id: &str,
        now: &str,
    ) -> Result<Option<SessionLease>, CoordinationDbError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT session_id, lease_id, assignment_id, agent_id, fencing_token,
                        acquired_at, renewed_at, expires_at
                 FROM session_leases
                 WHERE session_id=?1 AND released_at IS NULL AND expires_at > ?2
                 ORDER BY fencing_token DESC LIMIT 1",
                params![session_id, now],
                |row| {
                    Ok(SessionLease {
                        session_id: row.get(0)?,
                        lease_id: row.get(1)?,
                        assignment_id: row.get(2)?,
                        agent_id: row.get(3)?,
                        fencing_token: row.get::<_, i64>(4)? as u64,
                        acquired_at: row.get(5)?,
                        renewed_at: row.get(6)?,
                        expires_at: row.get(7)?,
                    })
                },
            )
            .optional()
        })
    }

    /// True when `lease_id` + `fencing_token` still own the session's active
    /// turn. A stale holder — one whose lease expired, was released, or was
    /// superseded by a higher token — fails this check and must not mutate.
    pub fn check_fence(
        &self,
        session_id: &str,
        lease_id: &str,
        fencing_token: u64,
        now: &str,
    ) -> Result<bool, CoordinationDbError> {
        Ok(self.active_lease(session_id, now)?.is_some_and(|active| {
            active.lease_id == lease_id && active.fencing_token == fencing_token
        }))
    }

    /// Test-only: overwrite a handoff's stored payload without touching its
    /// recorded hash, to exercise tamper detection.
    #[doc(hidden)]
    pub fn set_handoff_content_for_test(
        &self,
        id: &str,
        content_json: &str,
    ) -> Result<(), CoordinationDbError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE handoffs SET content_json = ?1 WHERE id = ?2",
                params![content_json, id],
            )?;
            Ok(())
        })
    }

    /// Test-only: force a lease past its expiry so fence checks can be exercised
    /// without sleeping.
    #[doc(hidden)]
    pub fn expire_lease_for_test(&self, lease_id: &str) -> Result<(), CoordinationDbError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE session_leases SET expires_at = ?1 WHERE lease_id = ?2",
                params!["2000-01-01T00:00:00+00:00", lease_id],
            )?;
            Ok(())
        })
    }
}

/// Decode a handoff row's JSON payload, propagating a malformed row as a typed
/// conversion error rather than panicking.
fn decode_handoff(raw: Option<String>) -> Result<Option<Handoff>, CoordinationDbError> {
    match raw {
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| CoordinationDbError::HandoffDecode(e.to_string())),
        None => Ok(None),
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

    /// RFC3339 timestamp `secs` after a fixed epoch. The store compares times as
    /// strings, which only orders correctly for real RFC3339 values.
    fn ts(secs: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z").unwrap()
            + chrono::Duration::seconds(secs))
        .to_rfc3339()
    }

    fn worker() -> Agent {
        Agent {
            id: "w".into(),
            display_name: "W".into(),
            handle: "w".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 2,
            version: 1,
        }
    }

    fn assignment(id: &str) -> Assignment {
        Assignment {
            id: id.into(),
            goal_id: "g".into(),
            session_id: "s".into(),
            agent_id: "w".into(),
            status: AssignmentStatus::Accepted,
            scope: "x".into(),
            definition_of_done: "y".into(),
            created_at: ts(0),
            updated_at: ts(0),
        }
    }

    fn lease(id: &str) -> SessionLease {
        SessionLease {
            session_id: "s".into(),
            lease_id: id.into(),
            assignment_id: "a".into(),
            agent_id: "w".into(),
            fencing_token: 0,
            acquired_at: ts(0),
            renewed_at: ts(0),
            expires_at: ts(100),
        }
    }

    #[test]
    fn active_lease_reports_the_current_holder() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.create_agent(&worker()).unwrap();
        db.create_assignment(&assignment("a")).unwrap();
        assert!(db.active_lease("s", &ts(1)).unwrap().is_none());

        db.acquire_lease(&lease("l")).unwrap();
        assert_eq!(
            db.active_lease("s", &ts(50)).unwrap().unwrap().lease_id,
            "l"
        );
        // Past expiry, the holder is no longer active.
        assert!(db.active_lease("s", &ts(101)).unwrap().is_none());
    }

    #[test]
    fn fence_accepts_only_the_current_token_and_rejects_a_replaced_holder() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.create_agent(&worker()).unwrap();
        db.create_assignment(&assignment("a")).unwrap();

        let first = db.acquire_lease(&lease("l1")).unwrap().unwrap();
        assert!(
            db.check_fence("s", "l1", first.fencing_token, &ts(50))
                .unwrap()
        );

        // Replace the holder: release, then a successor takes the next token.
        db.release_lease("l1", &ts(51), "handoff").unwrap();
        let second = db.acquire_lease(&lease("l2")).unwrap().unwrap();
        assert!(second.fencing_token > first.fencing_token);

        // The stale holder's next write is refused; the new holder's is accepted.
        assert!(
            !db.check_fence("s", "l1", first.fencing_token, &ts(52))
                .unwrap()
        );
        assert!(
            db.check_fence("s", "l2", second.fencing_token, &ts(52))
                .unwrap()
        );
        // Right lease id with a bumped token is also refused.
        assert!(
            !db.check_fence("s", "l2", second.fencing_token + 1, &ts(52))
                .unwrap()
        );
    }

    #[test]
    fn fence_rejects_an_expired_holder() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.create_agent(&worker()).unwrap();
        db.create_assignment(&assignment("a")).unwrap();
        let held = db.acquire_lease(&lease("l")).unwrap().unwrap();
        assert!(
            db.check_fence("s", "l", held.fencing_token, &ts(99))
                .unwrap()
        );
        // Expiry revokes ownership without any explicit release.
        assert!(
            !db.check_fence("s", "l", held.fencing_token, &ts(100))
                .unwrap()
        );
    }

    fn handoff(id: &str, target: &str) -> Handoff {
        Handoff {
            id: id.into(),
            goal_id: "g".into(),
            session_id: "s".into(),
            source_assignment_id: "a".into(),
            target_assignment_id: target.into(),
            context_mode: crate::coordination::types::ContextMode::FreshNeedsContext,
            objective: "finish".into(),
            definition_of_done: "tests pass".into(),
            scope: "src".into(),
            non_goals: vec![],
            workspace_ref: serde_json::json!({}),
            completed_summary: "half done".into(),
            next_action: "review".into(),
            decisions: vec![],
            risks: vec![],
            blockers: vec![],
            dependencies: vec![],
            artifacts: vec![],
            verification: vec![],
            context_manifest: serde_json::json!({}),
            created_at: ts(0),
            content_hash: String::new(),
        }
    }

    #[test]
    fn pending_handoffs_are_listed_until_acknowledged() {
        let db = CoordinationDb::open_in_memory().unwrap();
        db.create_agent(&worker()).unwrap();
        db.create_assignment(&assignment("a")).unwrap();
        db.create_assignment(&assignment("a2")).unwrap();
        let mut h = handoff("h1", "a2");
        h.content_hash = h.compute_content_hash();
        db.create_handoff(&h).unwrap();

        assert_eq!(db.list_pending_handoffs().unwrap().len(), 1);
        assert!(db.has_unacknowledged_handoff("a2").unwrap());
        // Acknowledging by target clears it from the pending list.
        assert!(
            db.acknowledge_handoff_by_target("a2", &ts(5))
                .unwrap()
                .is_some()
        );
        assert!(db.list_pending_handoffs().unwrap().is_empty());
        assert!(!db.has_unacknowledged_handoff("a2").unwrap());
    }
}
