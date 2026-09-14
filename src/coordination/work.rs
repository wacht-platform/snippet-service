use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStatus {
    Offered,
    Accepted,
    Active,
    AwaitingHandoff,
    Completed,
    Blocked,
    Failed,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    pub goal_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub status: AssignmentStatus,
    pub scope: String,
    pub definition_of_done: String,
    /// Inference profile to run THIS dispatch with, chosen per dispatch by
    /// Mission Control or the human. Empty means "use the session's own" — an
    /// unset value must not silently override a session's configured model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The agent that created this dispatch, when one did.
    ///
    /// Completion writes a report back to THIS agent's board, which is what
    /// makes a coordinator's memory close the loop on its own dispatches. None
    /// for work a human created directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn status_text(status: &AssignmentStatus) -> String {
    serde_json::to_string(status)
        .unwrap()
        .trim_matches('"')
        .into()
}
fn parse_status(value: String) -> Result<AssignmentStatus, rusqlite::Error> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// One assignment row, in the column order every `SELECT` in this module uses.
fn assignment_from_row(row: &rusqlite::Row<'_>) -> Result<Assignment, rusqlite::Error> {
    Ok(Assignment {
        id: row.get(0)?,
        goal_id: row.get(1)?,
        session_id: row.get(2)?,
        agent_id: row.get(3)?,
        status: parse_status(row.get(4)?)?,
        scope: row.get(5)?,
        definition_of_done: row.get(6)?,
        profile: row.get(9)?,
        dispatched_by: row.get(10)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

/// The columns `assignment_from_row` reads, so a `SELECT` cannot drift from its
/// decoder when a column is added.
const ASSIGNMENT_COLUMNS: &str = "id, goal_id, session_id, agent_id, status, scope, \
                                  definition_of_done, created_at, updated_at, profile, \
                                  dispatched_by";

/// Optional narrowing for an assignment page. Filters are applied in SQL so a
/// filtered page is still filled to `limit` — filtering after pagination would
/// silently return short pages and break paging.
#[derive(Debug, Clone, Default)]
pub struct AssignmentFilter<'a> {
    pub agent_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub status: Option<AssignmentStatus>,
}

impl Store {
    pub fn create_assignment(&self, assignment: &Assignment) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute("INSERT INTO assignments (id, goal_id, session_id, agent_id, status, scope, definition_of_done, created_at, updated_at, profile, dispatched_by) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,?9,?10)", params![assignment.id, assignment.goal_id, assignment.session_id, assignment.agent_id, status_text(&assignment.status), assignment.scope, assignment.definition_of_done, assignment.created_at, assignment.profile, assignment.dispatched_by])?;
            Ok(())
        })
    }

    pub fn get_assignment(&self, id: &str) -> Result<Option<Assignment>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ASSIGNMENT_COLUMNS} FROM assignments WHERE id = ?1"
            ))?;
            Ok(stmt.query_row(params![id], assignment_from_row).optional()?)
        })
    }

    /// One keyset page of assignments ordered by `(created_at, id)`. Pass the
    /// previous page's last pair as `after`; `None` starts from the beginning.
    /// A short page signals the end.
    pub fn list_assignments_page(
        &self,
        filter: &AssignmentFilter<'_>,
        after: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<Vec<Assignment>, StoreError> {
        let (after_created, after_id) = match after {
            Some((created, id)) => (Some(created), Some(id)),
            None => (None, None),
        };
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ASSIGNMENT_COLUMNS}
                 FROM assignments
                 WHERE (?1 IS NULL OR (created_at, id) > (?1, ?2))
                   AND (?3 IS NULL OR agent_id = ?3)
                   AND (?4 IS NULL OR session_id = ?4)
                   AND (?5 IS NULL OR status = ?5)
                 ORDER BY created_at, id
                 LIMIT ?6"
            ))?;
            let rows = stmt.query_map(
                params![
                    after_created,
                    after_id,
                    filter.agent_id,
                    filter.session_id,
                    filter.status.as_ref().map(status_text),
                    limit
                ],
                assignment_from_row,
            )?;
            rows.collect()
        })
    }

    /// Every assignment still awaiting delivery, oldest first.
    ///
    /// An assignment is created by whichever path offered it — a Mission Control
    /// tool call or the REST route — and delivery is the dispatcher's job, not
    /// the creator's. Selecting on `dispatched_at IS NULL` is what makes that
    /// single path: the marker is durable, so a restart re-drives an assignment
    /// that was never handed over instead of dropping it, and a delivered one is
    /// not re-delivered on the next boot.
    ///
    /// Only `offered` assignments need delivery: once one is accepted or parked
    /// there is nothing left to hand over.
    pub fn list_undelivered_assignments(&self) -> Result<Vec<Assignment>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ASSIGNMENT_COLUMNS}
                 FROM assignments
                 WHERE dispatched_at IS NULL AND status = 'offered'
                 ORDER BY created_at, id"
            ))?;
            let rows = stmt.query_map([], assignment_from_row)?;
            rows.collect()
        })
    }

    /// Record that an assignment has been handed to its session. Compare-and-set
    /// so two racing dispatchers cannot both claim it.
    pub fn mark_assignment_dispatched(
        &self,
        id: &str,
        at: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let changed = conn.execute(
                "UPDATE assignments SET dispatched_at = ?1
                 WHERE id = ?2 AND dispatched_at IS NULL",
                params![at, id],
            )?;
            Ok(changed == 1)
        })
    }

    /// Count a failed delivery attempt and return the new count, so the
    /// dispatcher can park an assignment that retrying cannot fix.
    pub fn record_assignment_dispatch_failure(
        &self,
        id: &str,
    ) -> Result<u32, StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE assignments SET dispatch_failures = dispatch_failures + 1 WHERE id = ?1",
                params![id],
            )?;
            let count: i64 = conn.query_row(
                "SELECT dispatch_failures FROM assignments WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(count as u32)
        })
    }

    pub fn transition_assignment(
        &self,
        id: &str,
        from: AssignmentStatus,
        to: AssignmentStatus,
        updated_at: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE assignments SET status=?1, updated_at=?2 WHERE id=?3 AND status=?4",
                params![status_text(&to), updated_at, id, status_text(&from)],
            )?;
            tx.commit()?;
            Ok(changed == 1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentRole, AgentStatus};

    #[test]
    fn assignment_transition_is_compare_and_swap() {
        let db = Store::open_in_memory().unwrap();
        let agent = Agent {
            id: "w".into(),
            display_name: "Worker".into(),
            handle: "worker".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
        };
        db.create_agent(&agent).unwrap();
        let a = Assignment {
            id: "a".into(),
            goal_id: "g".into(),
            session_id: "s".into(),
            agent_id: "w".into(),
            status: AssignmentStatus::Offered,
            scope: "src".into(),
            definition_of_done: "tests".into(),
            profile: None,
            dispatched_by: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        db.create_assignment(&a).unwrap();
        assert!(
            db.transition_assignment(
                "a",
                AssignmentStatus::Offered,
                AssignmentStatus::Accepted,
                "2"
            )
            .unwrap()
        );
        assert!(
            !db.transition_assignment(
                "a",
                AssignmentStatus::Offered,
                AssignmentStatus::Active,
                "3"
            )
            .unwrap()
        );
    }

    #[test]
    fn assignment_pages_advance_by_keyset() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&Agent {
            id: "w".into(),
            display_name: "W".into(),
            handle: "w".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
        })
        .unwrap();
        for (id, created) in [
            ("a1", "2020-01-01T00:00:01Z"),
            ("a2", "2020-01-01T00:00:02Z"),
            ("a3", "2020-01-01T00:00:03Z"),
        ] {
            db.create_assignment(&Assignment {
                id: id.into(),
                goal_id: "g".into(),
                session_id: "s".into(),
                agent_id: "w".into(),
                status: AssignmentStatus::Offered,
                scope: "x".into(),
                definition_of_done: "y".into(),
                profile: None,
                dispatched_by: None,
                created_at: created.into(),
                updated_at: created.into(),
            })
            .unwrap();
        }

        let first = db
            .list_assignments_page(&AssignmentFilter::default(), None, 2)
            .unwrap();
        assert_eq!(
            first.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        let last = first.last().unwrap();
        let second = db
            .list_assignments_page(
                &AssignmentFilter::default(),
                Some((&last.created_at, &last.id)),
                2,
            )
            .unwrap();
        assert_eq!(
            second.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a3"]
        );
    }

    /// A filtered page must still be filled to the limit: filtering after
    /// pagination would return short pages and silently break paging.
    #[test]
    fn assignment_pages_apply_filters_before_limiting() {
        let db = Store::open_in_memory().unwrap();
        for id in ["w1", "w2"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                role: AgentRole::Implementer,
                capabilities: vec![],
            })
            .unwrap();
        }
        // Interleave two agents so an unfiltered page would be mixed.
        let rows = [
            ("a1", "w1", "2020-01-01T00:00:01Z"),
            ("a2", "w2", "2020-01-01T00:00:02Z"),
            ("a3", "w1", "2020-01-01T00:00:03Z"),
            ("a4", "w1", "2020-01-01T00:00:04Z"),
        ];
        for (id, agent, created) in rows {
            db.create_assignment(&Assignment {
                id: id.into(),
                goal_id: "g".into(),
                session_id: "s".into(),
                agent_id: agent.into(),
                status: AssignmentStatus::Offered,
                scope: "x".into(),
                definition_of_done: "y".into(),
                profile: None,
                dispatched_by: None,
                created_at: created.into(),
                updated_at: created.into(),
            })
            .unwrap();
        }

        let filter = AssignmentFilter {
            agent_id: Some("w1"),
            ..Default::default()
        };
        // w1 has three assignments; a limit of 2 must return exactly 2 of them
        // (not fewer because w2's row occupied a slot).
        let page = db.list_assignments_page(&filter, None, 2).unwrap();
        assert_eq!(
            page.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a1", "a3"]
        );
        let last = page.last().unwrap();
        let next = db
            .list_assignments_page(&filter, Some((&last.created_at, &last.id)), 2)
            .unwrap();
        assert_eq!(
            next.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a4"]
        );
    }

    /// Delivery is the dispatcher's job, so an assignment is undelivered until
    /// it is explicitly marked. The mark is compare-and-set so two racing
    /// dispatchers cannot both claim the same offer.
    #[test]
    fn dispatch_marks_the_assignment_exactly_once() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&Agent {
            id: "w".into(),
            display_name: "Worker".into(),
            handle: "worker".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
        })
        .unwrap();
        db.create_assignment(&Assignment {
            id: "a".into(),
            goal_id: "g".into(),
            session_id: "s".into(),
            agent_id: "w".into(),
            status: AssignmentStatus::Offered,
            scope: "x".into(),
            definition_of_done: "y".into(),
            profile: None,
            dispatched_by: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .unwrap();

        // An offer created by ANY path starts undelivered — this is what makes
        // a tool-created assignment reach the worker, not just a REST one.
        assert_eq!(db.list_undelivered_assignments().unwrap().len(), 1);

        assert!(db.mark_assignment_dispatched("a", "2").unwrap());
        assert!(
            !db.mark_assignment_dispatched("a", "3").unwrap(),
            "a delivered assignment must not be re-delivered"
        );
        assert!(db.list_undelivered_assignments().unwrap().is_empty());
    }
}
