use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, types::Type};

use super::{CoordinationDb, CoordinationDbError, types::Agent};

fn now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap()
        .trim_matches('"')
        .to_string()
}

fn decode_enum<T: serde::de::DeserializeOwned>(
    value: String,
    column: usize,
) -> Result<T, rusqlite::Error> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(e)))
}

impl CoordinationDb {
    pub fn create_agent(&self, agent: &Agent) -> Result<(), CoordinationDbError> {
        let capabilities = serde_json::to_string(&agent.capabilities).unwrap();
        let timestamp = now();
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agents
                 (id, display_name, handle, kind, status, role, capabilities_json,
                  max_concurrent_assignments, max_concurrent_sessions, version, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    agent.id,
                    agent.display_name,
                    agent.handle,
                    enum_text(&agent.kind),
                    enum_text(&agent.status),
                    enum_text(&agent.role),
                    capabilities,
                    agent.max_concurrent_assignments,
                    agent.max_concurrent_sessions,
                    agent.version,
                    timestamp,
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>, CoordinationDbError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, display_name, handle, kind, status, role, capabilities_json,
                        max_concurrent_assignments, max_concurrent_sessions, version
                 FROM agents ORDER BY display_name",
            )?;
            let rows = stmt.query_map([], |row| {
                let kind: String = row.get(3)?;
                let status: String = row.get(4)?;
                let role: String = row.get(5)?;
                let capabilities: String = row.get(6)?;
                Ok(Agent {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    handle: row.get(2)?,
                    kind: decode_enum(kind, 3)?,
                    status: decode_enum(status, 4)?,
                    role: decode_enum(role, 5)?,
                    capabilities: serde_json::from_str(&capabilities).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(6, Type::Text, Box::new(e))
                    })?,
                    max_concurrent_assignments: row.get::<_, i64>(7)? as u32,
                    max_concurrent_sessions: row.get::<_, i64>(8)? as u32,
                    version: row.get::<_, i64>(9)? as u64,
                })
            })?;
            rows.collect()
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Option<Agent>, CoordinationDbError> {
        Ok(self.list_agents()?.into_iter().find(|agent| agent.id == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{AgentKind, AgentRole, AgentStatus};

    #[test]
    fn agent_round_trip() {
        let db = CoordinationDb::open_in_memory().unwrap();
        let agent = Agent {
            id: "a1".into(),
            display_name: "Rust worker".into(),
            handle: "rust".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec!["rust".into()],
            max_concurrent_assignments: 2,
            max_concurrent_sessions: 1,
            version: 1,
        };
        db.create_agent(&agent).unwrap();
        assert_eq!(db.get_agent("a1").unwrap().unwrap(), agent);
    }
}
