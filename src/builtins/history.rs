use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{expect_object, object_schema};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct RecallContextTool;

#[derive(Debug, Deserialize)]
struct RecallContextArgs {
    archive_id: i64,
}

#[async_trait]
impl Tool for RecallContextTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "recall_context".to_string(),
            description: "Recall and hydrate the full, unabridged payload of a past turn from the archived history using its archive_id (from [archived_history] or search_history). Returns the complete raw tool output, error trace, file diff, or user prompt."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "archive_id": {
                        "type": "integer",
                        "description": "The numeric ID of the turn to hydrate (e.g. 42 for #42)."
                    }
                }),
                &["archive_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: RecallContextArgs = expect_object("recall_context", arguments)?;
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("store unavailable: {e}")))?;
        match crate::history_archive::recall_turn(&store, args.archive_id) {
            Ok(Some(turn)) => Ok(ToolResult::success(json!({
                "archive_id": turn.archive_id,
                "role": turn.role,
                "tool_name": turn.tool_name,
                "summary": turn.summary,
                "affected_paths": turn.affected_paths,
                "status": turn.status,
                "created_at": turn.created_at,
                "payload": turn.payload,
            }))),
            Ok(None) => Err(ToolError::msg(format!(
                "Turn #{} not found in archive.",
                args.archive_id
            ))),
            Err(e) => Err(ToolError::msg(format!("Failed to recall turn #{}: {e}", args.archive_id))),
        }
    }
}

pub struct SearchHistoryTool;

#[derive(Debug, Deserialize)]
struct SearchHistoryArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for SearchHistoryTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "search_history".to_string(),
            description: "Full-text search (BM25) across all past conversation turns, tool calls, bash commands, errors, and file edits in this session. Returns matching turns with archive IDs, summaries, and snippets."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Search query: exact identifier, filename, error code, or keyword."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 5,
                        "description": "Maximum number of results to return (default: 5)."
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SearchHistoryArgs = expect_object("search_history", arguments)?;
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("store unavailable: {e}")))?;
        let session_id = ctx.durable_session_id().unwrap_or("default");
        let limit = args.limit.unwrap_or(5).clamp(1, 20);

        let matches = crate::history_archive::search_history(&store, session_id, &args.query, limit)
            .map_err(|e| ToolError::msg(format!("history search failed: {e}")))?;

        Ok(ToolResult::success(json!({
            "query": args.query,
            "count": matches.len(),
            "matches": matches,
        })))
    }
}
