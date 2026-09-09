use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::coordination::{CoordinationDb, types::CoordinationEvent};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

pub fn add_coordination_tools(registry: &mut ToolRegistry) {
    registry.insert(ListCoordinationAgents);
    registry.insert(PostCoordinationMessage);
    registry.insert(CreateCoordinationAssignment);
}

fn db(ctx: &ToolContext) -> Result<CoordinationDb, ToolError> {
    let root = ctx
        .mission_control_root()
        .ok_or_else(|| ToolError::msg("coordination tools require Mission Control context"))?;
    CoordinationDb::open(root.join("coordination.sqlite3"))
        .map_err(|e| ToolError::msg(format!("open coordination database: {e}")))
}

pub struct ListCoordinationAgents;
#[async_trait]
impl Tool for ListCoordinationAgents {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
        name: "list_coordination_agents".into(),
        description: "List specialized agents registered in the SQLite coordination directory. Use this to choose a direct recipient; Mission Control does not need to relay their messages.".into(),
        input_schema: schema(json!({}), &[]),
    }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let agents = db(ctx)?
            .list_agents()
            .map_err(|e| ToolError::msg(format!("list agents: {e}")))?;
        Ok(ToolResult::success(json!({"agents": agents})))
    }
}

#[derive(Deserialize)]
struct PostArgs {
    thread_id: String,
    actor_kind: String,
    actor_id: String,
    body: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

pub struct PostCoordinationMessage;
#[async_trait]
impl Tool for PostCoordinationMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
        name: "post_coordination_message".into(),
        description: "Post a direct message or update to a coordination board thread. This communicates with other permitted agents directly; it does not transfer ownership or acquire a session lease.".into(),
        input_schema: schema(json!({"thread_id":{"type":"string"},"actor_kind":{"type":"string"},"actor_id":{"type":"string"},"body":{"type":"string"},"idempotency_key":{"type":"string"}}), &["thread_id","actor_kind","actor_id","body"]),
    }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PostArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.thread_id.trim().is_empty()
            || args.actor_kind.trim().is_empty()
            || args.actor_id.trim().is_empty()
        {
            return Err(ToolError::msg(
                "thread_id, actor_kind, and actor_id must not be empty",
            ));
        }
        if args.body.trim().is_empty() {
            return Err(ToolError::msg("body must not be empty"));
        }
        let key = args
            .idempotency_key
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: args.thread_id.clone(),
            partition_key: format!("thread:{}", args.thread_id),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: args.actor_kind,
            actor_id: args.actor_id,
            payload_version: 1,
            payload: json!({"body": args.body}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: key,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let saved = db(ctx)?
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        Ok(ToolResult::success(json!({"event": saved})))
    }
}

#[derive(Deserialize)]
struct AssignmentArgs {
    id: String,
    goal_id: String,
    session_id: String,
    agent_id: String,
    scope: String,
    definition_of_done: String,
}

pub struct CreateCoordinationAssignment;
#[async_trait]
impl Tool for CreateCoordinationAssignment {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "create_coordination_assignment".into(),
            description: "Create an offered assignment for a specialized agent. This records ownership intent; the agent must accept it and acquire a session lease before execution. A board message alone never grants ownership.".into(),
            input_schema: schema(json!({
                "id":{"type":"string"},"goal_id":{"type":"string"},"session_id":{"type":"string"},"agent_id":{"type":"string"},"scope":{"type":"string"},"definition_of_done":{"type":"string"}
            }), &["id","goal_id","session_id","agent_id","scope","definition_of_done"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: AssignmentArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        let assignment = crate::coordination::Assignment {
            id: args.id,
            goal_id: args.goal_id,
            session_id: args.session_id,
            agent_id: args.agent_id,
            status: crate::coordination::AssignmentStatus::Offered,
            scope: args.scope,
            definition_of_done: args.definition_of_done,
            created_at: now.clone(),
            updated_at: now,
        };
        db(ctx)?
            .create_assignment(&assignment)
            .map_err(|e| ToolError::msg(format!("create assignment: {e}")))?;
        Ok(ToolResult::success(json!({"assignment": assignment})))
    }
}
