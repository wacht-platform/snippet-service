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
            max_concurrent_sessions: 1,
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
}
