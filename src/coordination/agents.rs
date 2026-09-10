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
                  max_concurrent_assignments, version, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    agent.id,
                    agent.display_name,
                    agent.handle,
                    enum_text(&agent.kind),
                    enum_text(&agent.status),
                    enum_text(&agent.role),
                    capabilities,
                    agent.max_concurrent_assignments,
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
                        max_concurrent_assignments, version
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
                    version: row.get::<_, i64>(8)? as u64,
                })
            })?;
            rows.collect()
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Option<Agent>, CoordinationDbError> {
        Ok(self.list_agents()?.into_iter().find(|agent| agent.id == id))
    }

    /// One keyset page of agents ordered by `(display_name, id)` — a stable,
    /// total order. Pass the previous page's last `(display_name, id)` as
    /// `after`; `None` starts from the beginning. Callers detect the end by a
    /// short page.
    pub fn list_agents_page(
        &self,
        after: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<Vec<Agent>, CoordinationDbError> {
        self.with_connection(|conn| {
            let (after_name, after_id) = match after {
                Some((name, id)) => (Some(name), Some(id)),
                None => (None, None),
            };
            let mut stmt = conn.prepare(
                "SELECT id, display_name, handle, kind, status, role, capabilities_json,
                        max_concurrent_assignments, version
                 FROM agents
                 WHERE ?1 IS NULL OR (display_name, id) > (?1, ?2)
                 ORDER BY display_name, id
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![after_name, after_id, limit], |row| {
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
                    version: row.get::<_, i64>(8)? as u64,
                })
            })?;
            rows.collect()
        })
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
            version: 1,
        };
        db.create_agent(&agent).unwrap();
        assert_eq!(db.get_agent("a1").unwrap().unwrap(), agent);
    }

    fn agent(id: &str, name: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: name.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 1,
            version: 1,
        }
    }

    #[test]
    fn agent_pages_are_stable_and_advance_by_keyset() {
        let db = CoordinationDb::open_in_memory().unwrap();
        for (id, name) in [("b", "Beta"), ("a", "Alpha"), ("c", "Gamma")] {
            db.create_agent(&agent(id, name)).unwrap();
        }

        // Page 1: ordered by display_name, limited.
        let first = db.list_agents_page(None, 2).unwrap();
        assert_eq!(
            first.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );

        // Page 2 continues strictly after the last pair.
        let last = first.last().unwrap();
        let second = db
            .list_agents_page(Some((&last.display_name, &last.id)), 2)
            .unwrap();
        assert_eq!(
            second.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );

        // A cursor past the end yields a short (empty) page, not a restart.
        assert!(
            db.list_agents_page(Some(("Gamma", "c")), 2)
                .unwrap()
                .is_empty()
        );
    }
}
