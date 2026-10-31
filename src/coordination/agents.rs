use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, types::Type};

use super::{Store, StoreError, types::{Agent, AgentKind, AgentStatus}};

#[derive(Debug, PartialEq, Eq)]
pub enum AgentRemoval {
    Removed,
    NotFound,
    MissionControl,
    OnOpenTasks(Vec<String>),
}

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
    /// update it. `status` is set only on insert: a boot or a rebuild must not
    /// resume an agent someone paused.
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

    /// Pause, drain, disable or resume an agent. Returns whether it exists.
    pub fn set_agent_status(&self, id: &str, status: &AgentStatus) -> Result<bool, StoreError> {
        let timestamp = now();
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE agents SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, enum_text(status), timestamp],
            )? == 1)
        })
    }

    /// Remove an agent for good. Mission Control cannot be removed, and an agent
    /// still on an unfinished task is refused so that work is not orphaned.
    /// Its finished task memberships, coordination notes and undelivered
    /// messages go with it.
    pub fn delete_agent(&self, id: &str) -> Result<AgentRemoval, StoreError> {
        self.with_connection(|conn| {
            let kind: Option<String> = conn
                .query_row("SELECT kind FROM agents WHERE id = ?1", params![id], |row| {
                    row.get(0)
                })
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            let Some(kind) = kind else {
                return Ok(AgentRemoval::NotFound);
            };
            if kind == enum_text(&AgentKind::MissionControl) {
                return Ok(AgentRemoval::MissionControl);
            }
            let open: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT t.title FROM task_agents ta JOIN tasks t ON t.id = ta.task_id
                     WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL
                       AND t.status NOT IN ('done', 'failed', 'cancelled')
                     ORDER BY t.updated_at DESC",
                )?;
                stmt.query_map(params![id], |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            };
            if !open.is_empty() {
                return Ok(AgentRemoval::OnOpenTasks(open));
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM task_agents WHERE agent_id = ?1", params![id])?;
            tx.execute("DELETE FROM agent_board WHERE agent_id = ?1", params![id])?;
            tx.execute(
                "DELETE FROM message_deliveries
                 WHERE recipient_kind = 'agent' AND recipient_id = ?1 AND delivered_at IS NULL",
                params![id],
            )?;
            tx.execute("DELETE FROM agent_colors WHERE agent_id = ?1", params![id])?;
            tx.execute("DELETE FROM agent_shapes WHERE agent_id = ?1", params![id])?;
            tx.execute("DELETE FROM agent_nicknames WHERE agent_id = ?1", params![id])?;
            tx.execute("DELETE FROM agent_profiles WHERE agent_id = ?1", params![id])?;
            tx.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(AgentRemoval::Removed)
        })
    }

    pub fn agent_nicknames(&self) -> Result<std::collections::HashMap<String, String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT agent_id, nickname FROM agent_nicknames")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    pub fn set_agent_nickname(&self, id: &str, nickname: Option<&str>) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            match nickname {
                Some(nickname) => conn.execute(
                    "INSERT INTO agent_nicknames (agent_id, nickname) VALUES (?1, ?2)
                     ON CONFLICT(agent_id) DO UPDATE SET nickname = excluded.nickname",
                    params![id, nickname],
                )?,
                None => conn.execute("DELETE FROM agent_nicknames WHERE agent_id = ?1", params![id])?,
            };
            Ok(true)
        })
    }

    pub fn agent_colors(&self) -> Result<std::collections::HashMap<String, String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT agent_id, color FROM agent_colors")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    pub fn agent_shapes(&self) -> Result<std::collections::HashMap<String, String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT agent_id, shape FROM agent_shapes")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    pub fn agent_profiles(&self) -> Result<std::collections::HashMap<String, String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT agent_id, profile FROM agent_profiles")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// The inference profile an agent works on, if one is set.
    pub fn agent_profile(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            match conn.query_row(
                "SELECT profile FROM agent_profiles WHERE agent_id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            ) {
                Ok(profile) => Ok(Some(profile).filter(|p| !p.trim().is_empty())),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
    }

    /// Set or clear (`None`) the agent's profile. Returns whether the agent exists.
    pub fn set_agent_profile(&self, id: &str, profile: Option<&str>) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            match profile {
                Some(profile) => conn.execute(
                    "INSERT INTO agent_profiles (agent_id, profile) VALUES (?1, ?2)
                     ON CONFLICT(agent_id) DO UPDATE SET profile = excluded.profile",
                    params![id, profile],
                )?,
                None => conn.execute("DELETE FROM agent_profiles WHERE agent_id = ?1", params![id])?,
            };
            Ok(true)
        })
    }

    pub fn set_agent_color(&self, id: &str, color: Option<&str>) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            match color {
                Some(color) => conn.execute(
                    "INSERT INTO agent_colors (agent_id, color) VALUES (?1, ?2)
                     ON CONFLICT(agent_id) DO UPDATE SET color = excluded.color",
                    params![id, color],
                )?,
                None => conn.execute("DELETE FROM agent_colors WHERE agent_id = ?1", params![id])?,
            };
            Ok(true)
        })
    }

    pub fn set_agent_shape(&self, id: &str, shape: Option<&str>) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            match shape {
                Some(shape) => conn.execute(
                    "INSERT INTO agent_shapes (agent_id, shape) VALUES (?1, ?2)
                     ON CONFLICT(agent_id) DO UPDATE SET shape = excluded.shape",
                    params![id, shape],
                )?,
                None => conn.execute("DELETE FROM agent_shapes WHERE agent_id = ?1", params![id])?,
            };
            Ok(true)
        })
    }

    /// Whether an agent may be handed new work. Paused, draining and disabled
    /// agents keep what they hold but take nothing new; an unknown id takes
    /// nothing at all.
    pub fn agent_takes_work(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .get_agent(id)?
            .is_some_and(|agent| agent.status == AgentStatus::Active))
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
    fn agent_shape_is_set_cleared_and_removed_with_the_agent() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&agent("a1", "A")).unwrap();
        assert!(!db.set_agent_shape("ghost-agent", Some("cat")).unwrap(), "unknown agent");
        assert!(db.set_agent_shape("a1", Some("cat")).unwrap());
        assert!(db.set_agent_shape("a1", Some("onigiri")).unwrap());
        assert_eq!(db.agent_shapes().unwrap().get("a1").map(String::as_str), Some("onigiri"));
        assert!(db.set_agent_shape("a1", None).unwrap());
        assert!(db.agent_shapes().unwrap().is_empty());
        db.set_agent_shape("a1", Some("bear")).unwrap();
        db.delete_agent("a1").unwrap();
        assert!(db.agent_shapes().unwrap().is_empty());
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

        // A boot or rebuild must not resume a paused agent.
        db.set_agent_status("mission-control", &AgentStatus::Paused).unwrap();
        db.upsert_agent(&renamed).unwrap();
        assert!(!db.agent_takes_work("mission-control").unwrap());
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
