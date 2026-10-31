//! Per-agent context ceiling: how many tokens an agent's sessions may hold
//! before compaction, below the model's own window.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub const MIN_CONTEXT_TOKENS: u64 = 16_000;

/// The window compaction works from: the agent's ceiling clamped to the model's
/// window (when known) and to a floor that still leaves room to work.
pub fn effective_context_window(model_window: u64, limit: Option<u64>) -> u64 {
    let Some(limit) = limit.filter(|l| *l > 0) else {
        return model_window;
    };
    let limit = limit.max(MIN_CONTEXT_TOKENS);
    if model_window == 0 { limit } else { limit.min(model_window) }
}

/// The directory agent behind a session, or Mission Control.
pub fn agent_for_session(agent_id: Option<&str>, mission_control: bool) -> Option<String> {
    if mission_control {
        return Some(crate::mission_control::SESSION_ID.to_string());
    }
    agent_id.map(str::to_string)
}

pub fn limit_for(agent_id: Option<&str>) -> Option<u64> {
    let agent_id = agent_id?;
    let store = crate::store::Store::open_cached(crate::store::default_db_path()).ok()?;
    store.agent_context_limit(agent_id).ok().flatten()
}

pub fn validate(tokens: Option<u64>) -> Result<Option<u64>, String> {
    match tokens {
        None | Some(0) => Ok(None),
        Some(t) if t < MIN_CONTEXT_TOKENS => {
            Err(format!("max_context_tokens must be at least {MIN_CONTEXT_TOKENS}"))
        }
        Some(t) => Ok(Some(t)),
    }
}

#[derive(Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    max_context_tokens: Option<u64>,
}

pub struct ContextLimitTool;

#[async_trait]
impl Tool for ContextLimitTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "context_limit".into(),
            description: "Show, set or clear your context ceiling: the tokens your sessions hold before compacting, never above the model's window. Applies from the next session start. Mission Control may pass agent_id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["show", "set", "clear"]},
                    "agent_id": {"type": "string", "description": "Mission Control only."},
                    "max_context_tokens": {"type": "integer", "minimum": MIN_CONTEXT_TOKENS}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: Args = serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let named = args.agent_id.as_deref().map(str::trim).filter(|a| !a.is_empty());
        let agent = match (ctx.is_mission_control(), named) {
            (true, Some(other)) => other.to_string(),
            (true, None) => crate::mission_control::SESSION_ID.to_string(),
            (false, Some(_)) => return Err(ToolError::msg("only Mission Control can set another agent's limit")),
            (false, None) => ctx
                .agent_id()
                .filter(|a| *a != crate::coordination::SNIPPET_AGENT_ID)
                .ok_or_else(|| ToolError::msg("this session has no directory agent"))?
                .to_string(),
        };
        let store = crate::store::Store::open_cached(crate::store::default_db_path())
            .map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let tokens = match args.action.as_str() {
            "show" => None,
            "clear" => Some(None),
            "set" => Some(validate(args.max_context_tokens).map_err(ToolError::msg)?),
            other => return Err(ToolError::msg(format!("unknown action `{other}`"))),
        };
        if let Some(tokens) = tokens
            && !store.set_agent_context_limit(&agent, tokens).map_err(|e| ToolError::msg(e.to_string()))?
        {
            return Err(ToolError::msg(format!("no agent `{agent}`")));
        }
        let current = store.agent_context_limit(&agent).map_err(|e| ToolError::msg(e.to_string()))?;
        Ok(ToolResult::success(json!({"agent_id": agent, "max_context_tokens": current})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_is_clamped_to_the_model_window_and_a_floor() {
        assert_eq!(effective_context_window(200_000, None), 200_000);
        assert_eq!(effective_context_window(200_000, Some(0)), 200_000);
        assert_eq!(effective_context_window(200_000, Some(80_000)), 80_000);
        assert_eq!(effective_context_window(200_000, Some(1_000_000)), 200_000);
        assert_eq!(effective_context_window(200_000, Some(1_000)), MIN_CONTEXT_TOKENS);
        assert_eq!(effective_context_window(0, Some(90_000)), 90_000);
    }

    #[test]
    fn validation_rejects_tiny_ceilings_and_treats_zero_as_clear() {
        assert_eq!(validate(None), Ok(None));
        assert_eq!(validate(Some(0)), Ok(None));
        assert!(validate(Some(500)).is_err());
        assert_eq!(validate(Some(64_000)), Ok(Some(64_000)));
    }

    #[test]
    fn mission_control_sessions_resolve_to_its_agent() {
        assert_eq!(agent_for_session(None, true).as_deref(), Some("mission-control"));
        assert_eq!(agent_for_session(Some("x-research"), false).as_deref(), Some("x-research"));
        assert_eq!(agent_for_session(None, false), None);
    }

    #[test]
    fn store_round_trips_a_ceiling_for_known_agents_only() {
        use crate::coordination::types::{Agent, AgentKind, AgentRole, AgentStatus};
        let store = crate::store::Store::open_in_memory().unwrap();
        assert!(!store.set_agent_context_limit("ghost", Some(64_000)).unwrap());
        store
            .create_agent(&Agent {
                id: "researcher".into(),
                display_name: "Researcher".into(),
                handle: "researcher".into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                role: AgentRole::Implementer,
                capabilities: vec![],
            })
            .unwrap();
        assert!(store.set_agent_context_limit("researcher", Some(64_000)).unwrap());
        assert_eq!(store.agent_context_limit("researcher").unwrap(), Some(64_000));
        assert_eq!(store.agent_context_limits().unwrap().get("researcher"), Some(&64_000));
        assert!(store.set_agent_context_limit("researcher", None).unwrap());
        assert_eq!(store.agent_context_limit("researcher").unwrap(), None);
    }
}
