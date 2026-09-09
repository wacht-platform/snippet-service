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
    pub max_concurrent_assignments: u32,
    pub max_concurrent_sessions: u32,
    pub version: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    ResumeInformed,
    FreshNeedsContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionLease {
    pub session_id: String,
    pub lease_id: String,
    pub assignment_id: String,
    pub agent_id: String,
    pub fencing_token: u64,
    pub acquired_at: String,
    pub renewed_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    pub id: String,
    pub goal_id: String,
    pub session_id: String,
    pub source_assignment_id: String,
    pub target_assignment_id: String,
    pub context_mode: ContextMode,
    pub objective: String,
    pub definition_of_done: String,
    pub scope: String,
    pub non_goals: Vec<String>,
    pub workspace_ref: Value,
    pub completed_summary: String,
    pub next_action: String,
    pub decisions: Vec<String>,
    pub risks: Vec<String>,
    pub blockers: Vec<String>,
    pub dependencies: Vec<String>,
    pub artifacts: Vec<Value>,
    pub verification: Vec<Value>,
    pub context_manifest: Value,
    pub created_at: String,
    pub content_hash: String,
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

    #[test]
    fn handoff_mode_is_explicit() {
        let raw = include_str!("../../tests/fixtures/coordination/handoff.json");
        let handoff: Handoff = serde_json::from_str(raw).unwrap();
        assert_eq!(handoff.context_mode, ContextMode::FreshNeedsContext);
    }
}
