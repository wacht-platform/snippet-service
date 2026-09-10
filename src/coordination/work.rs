use rusqlite::{OptionalExtension, params};

use super::{CoordinationDb, CoordinationDbError};

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

/// Optional narrowing for an assignment page. Filters are applied in SQL so a
/// filtered page is still filled to `limit` — filtering after pagination would
/// silently return short pages and break paging.
#[derive(Debug, Clone, Default)]
pub struct AssignmentFilter<'a> {
    pub agent_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

impl CoordinationDb {
    pub fn create_assignment(&self, assignment: &Assignment) -> Result<(), CoordinationDbError> {
        self.with_connection(|conn| {
            conn.execute("INSERT INTO assignments (id, goal_id, session_id, agent_id, status, scope, definition_of_done, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)", params![assignment.id, assignment.goal_id, assignment.session_id, assignment.agent_id, status_text(&assignment.status), assignment.scope, assignment.definition_of_done, assignment.created_at])?;
            Ok(())
        })
    }

    pub fn get_assignment(&self, id: &str) -> Result<Option<Assignment>, CoordinationDbError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT id,goal_id,session_id,agent_id,status,scope,definition_of_done,created_at,updated_at FROM assignments WHERE id=?1")?;
            Ok(stmt.query_row(params![id], |row| Ok(Assignment { id:row.get(0)?, goal_id:row.get(1)?, session_id:row.get(2)?, agent_id:row.get(3)?, status:parse_status(row.get(4)?)?, scope:row.get(5)?, definition_of_done:row.get(6)?, created_at:row.get(7)?, updated_at:row.get(8)? })).optional()?)
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
    ) -> Result<Vec<Assignment>, CoordinationDbError> {
        let (after_created, after_id) = match after {
            Some((created, id)) => (Some(created), Some(id)),
            None => (None, None),
        };
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, goal_id, session_id, agent_id, status, scope,
                        definition_of_done, created_at, updated_at
                 FROM assignments
                 WHERE (?1 IS NULL OR (created_at, id) > (?1, ?2))
                   AND (?3 IS NULL OR agent_id = ?3)
                   AND (?4 IS NULL OR session_id = ?4)
                 ORDER BY created_at, id
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    after_created,
                    after_id,
                    filter.agent_id,
                    filter.session_id,
                    limit
                ],
                |row| {
                    Ok(Assignment {
                        id: row.get(0)?,
                        goal_id: row.get(1)?,
                        session_id: row.get(2)?,
                        agent_id: row.get(3)?,
                        status: parse_status(row.get(4)?)?,
                        scope: row.get(5)?,
                        definition_of_done: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )?;
            rows.collect()
        })
    }

    pub fn transition_assignment(
        &self,
        id: &str,
        from: AssignmentStatus,
        to: AssignmentStatus,
        updated_at: &str,
    ) -> Result<bool, CoordinationDbError> {
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
        let db = CoordinationDb::open_in_memory().unwrap();
        let agent = Agent {
            id: "w".into(),
            display_name: "Worker".into(),
            handle: "worker".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 2,
            version: 1,
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
        let db = CoordinationDb::open_in_memory().unwrap();
        for id in ["w1", "w2"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                role: AgentRole::Implementer,
                capabilities: vec![],
                max_concurrent_assignments: 2,
                version: 1,
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
}
