use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    MissionControl,
    Worker,
    Human,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Active,
    Paused,
    Draining,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Implementer,
    Reviewer,
    Tester,
    Researcher,
    Release,
    Coordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub display_name: String,
    pub handle: String,
    pub kind: AgentKind,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadScope {
    Workspace,
    Goal,
    Session,
    Direct,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoordinationEvent {
    pub event_id: String,
    pub thread_id: String,
    pub partition_key: String,
    pub sequence: u64,
    pub event_type: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub payload_version: u32,
    pub payload: Value,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
    pub idempotency_key: String,
    pub created_at: String,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_fixture_round_trips() {
        let raw = include_str!("../../tests/fixtures/coordination/agent.json");
        let agent: Agent = serde_json::from_str(raw).unwrap();
        assert_eq!(agent.handle, "implementer");
        assert_eq!(
            serde_json::from_str::<Agent>(&serde_json::to_string(&agent).unwrap()).unwrap(),
            agent
        );
    }

    #[test]
    fn event_fixture_round_trips() {
        let raw = include_str!("../../tests/fixtures/coordination/event.json");
        let event: CoordinationEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(event.sequence, 1);
        assert_eq!(event.event_type, "message.posted");
    }

}
