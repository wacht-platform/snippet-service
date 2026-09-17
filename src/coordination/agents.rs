use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, types::Type};

use super::{Store, StoreError, types::Agent};

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

pub(crate) fn decode_enum<T: serde::de::DeserializeOwned>(
    value: String,
    column: usize,
) -> Result<T, rusqlite::Error> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(e)))
}

impl Store {
    pub fn create_agent(&self, agent: &Agent) -> Result<(), StoreError> {
        let capabilities = serde_json::to_string(&agent.capabilities).unwrap();
        let timestamp = now();
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agents
                 (id, display_name, handle, kind, status, role, capabilities_json,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    agent.id,
                    agent.display_name,
                    agent.handle,
                    enum_text(&agent.kind),
                    enum_text(&agent.status),
                    enum_text(&agent.role),
                    capabilities,
                    timestamp,
                ],
            )?;
            Ok(())
        })
    }

    /// Create the agent, or refresh it if the id already exists.
    ///
    /// The daemon registers its built-in agents (Mission Control) on every boot,
    /// so this has to be idempotent — `create_agent` is a plain INSERT and would
    /// hit the primary key on the second start. The DESCRIPTIVE fields are
    /// refreshed so a changed display name or role lands; `created_at` is
    /// preserved, because rewriting it would erase the row's history rather than
    /// update it.
    pub fn upsert_agent(&self, agent: &Agent) -> Result<(), StoreError> {
        let capabilities = serde_json::to_string(&agent.capabilities).unwrap();
        let timestamp = now();
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agents
                 (id, display_name, handle, kind, status, role, capabilities_json,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                     display_name = excluded.display_name,
                     handle = excluded.handle,
                     kind = excluded.kind,
                     status = excluded.status,
                     role = excluded.role,
                     capabilities_json = excluded.capabilities_json,
                     updated_at = excluded.updated_at",
                params![
                    agent.id,
                    agent.display_name,
                    agent.handle,
                    enum_text(&agent.kind),
                    enum_text(&agent.status),
                    enum_text(&agent.role),
                    capabilities,
                    timestamp,
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, display_name, handle, kind, status, role, capabilities_json
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
                })
            })?;
            rows.collect()
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Option<Agent>, StoreError> {
        Ok(self.list_agents()?.into_iter().find(|agent| agent.id == id))
    }

    /// Active assignment summaries in one set-based query. The endpoint uses
    /// this instead of walking tasks and querying each roster separately.
    pub fn list_agent_assigned_sessions(
        &self,
    ) -> Result<Vec<(String, String, String, String, i64)>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ta.agent_id, s.id, COALESCE(s.title, ''),
                        COALESCE(json_extract(s.state_json, '$.conversation'), ''),
                        COALESCE(s.last_active, 0)
                 FROM task_agents ta
                 JOIN tasks t ON t.id = ta.task_id
                 JOIN sessions s ON s.id = t.session_id
                 WHERE ta.removed_at IS NULL
                   AND t.status NOT IN ('done', 'cancelled', 'failed')
                   AND t.session_id <> ''
                 GROUP BY ta.agent_id, s.id, s.title, s.state_json, s.last_active
                 ORDER BY ta.agent_id, MAX(s.updated_at) DESC, s.id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            rows.collect()
        })
    }

    /// One keyset page of agents ordered by `(display_name, id)` — a stable,
    /// total order. Pass the previous page's last `(display_name, id)` as
    /// `after`; `None` starts from the beginning. Callers detect the end by a
    /// short page.
    pub fn list_agents_page(
        &self,
        after: Option<(&str, &str)>,
        limit: u32,
    ) -> Result<Vec<Agent>, StoreError> {
        self.with_connection(|conn| {
            let (after_name, after_id) = match after {
                Some((name, id)) => (Some(name), Some(id)),
                None => (None, None),
            };
            let mut stmt = conn.prepare(
                "SELECT id, display_name, handle, kind, status, role, capabilities_json
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
        let db = Store::open_in_memory().unwrap();
        let agent = Agent {
            id: "a1".into(),
            display_name: "Rust worker".into(),
            handle: "rust".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec!["rust".into()],
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
        }
    }

    #[test]
    fn upsert_agent_is_idempotent_and_refreshes_descriptors() {
        // The daemon registers its built-in agents on EVERY boot, so a plain
        // INSERT would hit the primary key on the second start.
        let db = Store::open_in_memory().unwrap();
        let first = agent("mission-control", "Mission Control");
        db.upsert_agent(&first).unwrap();
        db.upsert_agent(&first).unwrap();

        assert_eq!(db.list_agents().unwrap().len(), 1, "must not duplicate");

        // A changed descriptor lands; created_at is preserved rather than
        // rewritten, so the row keeps its history.
        let renamed = Agent {
            display_name: "Coordinator".into(),
            kind: AgentKind::MissionControl,
            role: AgentRole::Coordinator,
            ..agent("mission-control", "Coordinator")
        };
        db.upsert_agent(&renamed).unwrap();

        let stored = db.get_agent("mission-control").unwrap().unwrap();
        assert_eq!(stored.display_name, "Coordinator");
        assert_eq!(stored.kind, AgentKind::MissionControl);
        assert_eq!(db.list_agents().unwrap().len(), 1, "still one row");
    }

    #[test]
    fn agent_pages_are_stable_and_advance_by_keyset() {
        let db = Store::open_in_memory().unwrap();
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
