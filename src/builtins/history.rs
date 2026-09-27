use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{expect_object, object_schema};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct RecallContextTool;

#[derive(Debug, Deserialize)]
struct RecallContextArgs {
    #[serde(default)]
    archive_id: Option<i64>,
    #[serde(default)]
    archive_ids: Option<Vec<i64>>,
    #[serde(default)]
    from_id: Option<i64>,
    #[serde(default)]
    to_id: Option<i64>,
}

#[async_trait]
impl Tool for RecallContextTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "recall_context".to_string(),
            description: "Recall and hydrate the full, unabridged payload of past turns from the archived history. Pass a single `archive_id` (e.g. 12), a list of `archive_ids` (e.g. [12, 13, 14]), or a range using `from_id` and `to_id` (e.g. from_id: 10, to_id: 15). Returns the complete raw tool outputs, error traces, file diffs, or user prompts."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "archive_id": {
                        "type": "integer",
                        "description": "Numeric ID of a single turn to hydrate (e.g. 42 for #42)."
                    },
                    "archive_ids": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "List of turn IDs to hydrate together as a group."
                    },
                    "from_id": {
                        "type": "integer",
                        "description": "Starting turn ID for a contiguous range query."
                    },
                    "to_id": {
                        "type": "integer",
                        "description": "Ending turn ID for a contiguous range query."
                    }
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: RecallContextArgs = expect_object("recall_context", arguments)?;
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("store unavailable: {e}")))?;
        let Some(session_id) = ctx.durable_session_id() else {
            return Err(ToolError::msg(
                "History archive is unavailable here: this run has no durable session.",
            ));
        };

        // 1. Contiguous range
        if let (Some(from), Some(to)) = (args.from_id, args.to_id) {
            let turns = crate::history_archive::recall_turn_range(&store, session_id, from, to, 20)
                .map_err(|e| ToolError::msg(format!("Failed to recall turn range: {e}")))?;
            return Ok(ToolResult::success(json!({
                "from_id": from,
                "to_id": to,
                "count": turns.len(),
                "turns": turns,
            })));
        }

        // 2. Batch list of IDs
        if let Some(ids) = args.archive_ids.filter(|l| !l.is_empty()) {
            let turns = crate::history_archive::recall_turns(&store, session_id, &ids)
                .map_err(|e| ToolError::msg(format!("Failed to recall turns: {e}")))?;
            return Ok(ToolResult::success(json!({
                "requested_ids": ids,
                "count": turns.len(),
                "turns": turns,
            })));
        }

        // 3. Single turn ID
        if let Some(id) = args.archive_id {
            match crate::history_archive::recall_turn(&store, session_id, id) {
                Ok(Some(turn)) => return Ok(ToolResult::success(json!(turn))),
                Ok(None) => return Err(ToolError::msg(format!("Turn #{id} not found in archive."))),
                Err(e) => return Err(ToolError::msg(format!("Failed to recall turn #{id}: {e}"))),
            }
        }

        Err(ToolError::msg(
            "recall_context requires either `archive_id`, `archive_ids`, or `from_id` + `to_id`.",
        ))
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
        let Some(session_id) = ctx.durable_session_id() else {
            return Err(ToolError::msg(
                "History archive is unavailable here: this run has no durable session.",
            ));
        };
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
