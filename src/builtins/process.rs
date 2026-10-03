use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{expect_object, object_schema};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct ManageProcessTool;

#[derive(Debug, Deserialize)]
struct ManageProcessArgs {
    action: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
}

#[async_trait]
impl Tool for ManageProcessTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "manage_process".to_string(),
            description: "Manage background processes started via `bash {background: true}`. Use to list running processes, read trailing log lines, or terminate them cleanly."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["list", "kill", "log"],
                        "description": "The management action to perform: 'list' (show all tracked background processes), 'kill' (terminate a running background process), or 'log' (view trailing log output)."
                    },
                    "id": {
                        "type": "string",
                        "description": "Background process ID (as returned by `bash` with background:true or listed in [background_processes]). Required for 'kill' and 'log'."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 50,
                        "description": "Number of trailing log lines to retrieve (default 50, max 1000). Only used when action='log'."
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ManageProcessArgs = expect_object("manage_process", arguments)?;
        let id_clean = args.id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match args.action.as_str() {
            "list" => {
                let processes = crate::bg::list(ctx.workspace_root());
                let running_count = processes.iter().filter(|p| p.running).count();
                Ok(ToolResult::success(json!({
                    "processes": processes,
                    "total": processes.len(),
                    "running": running_count,
                })))
            }
            "kill" => {
                let Some(id) = id_clean else {
                    return Err(ToolError::msg("Missing required argument 'id' for action 'kill'"));
                };
                match crate::bg::kill_by_id(ctx.workspace_root(), id) {
                    Ok(true) => Ok(ToolResult::success(json!({
                        "id": id,
                        "status": "terminated",
                        "note": format!("Background process '{id}' was sent SIGTERM"),
                    }))),
                    Ok(false) => Ok(ToolResult::success(json!({
                        "id": id,
                        "status": "already_stopped",
                        "note": format!("Background process '{id}' was already terminated or exited"),
                    }))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Err(ToolError::msg(format!("Background process with ID '{id}' not found")))
                    }
                    Err(e) => Err(ToolError::msg(format!("Failed to kill background process '{id}': {e}"))),
                }
            }
            "log" => {
                let Some(id) = id_clean else {
                    return Err(ToolError::msg("Missing required argument 'id' for action 'log'"));
                };
                let lines_to_read = args.lines.unwrap_or(50).clamp(1, 1000);
                match crate::bg::tail_log(ctx.workspace_root(), id, lines_to_read) {
                    Ok((log_content, truncated)) => Ok(ToolResult::success(json!({
                        "id": id,
                        "requested_lines": lines_to_read,
                        "truncated": truncated,
                        "log": log_content,
                    }))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Err(ToolError::msg(format!("Log for background process '{id}' not found")))
                    }
                    Err(e) => Err(ToolError::msg(format!("Failed to read log for '{id}': {e}"))),
                }
            }
            other => Err(ToolError::msg(format!(
                "Invalid action '{other}'. Supported actions are 'list', 'kill', and 'log'."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_manage_process_list_empty() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let res = tool
            .execute(&ctx, json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["total"], 0);
        assert_eq!(res.value["data"]["running"], 0);
    }

    #[tokio::test]
    async fn test_manage_process_kill_missing_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "kill"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Missing required argument 'id'"));
    }

    #[tokio::test]
    async fn test_manage_process_log_missing_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "log"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Missing required argument 'id'"));
    }

    #[tokio::test]
    async fn test_manage_process_kill_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "kill", "id": "unknown_proc"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_manage_process_log_success() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let id = "testlog1";
        crate::bg::record(temp.path(), id, "npm run dev", Some("Dev Server"), 88888).unwrap();
        let log_file = crate::bg::log_path(temp.path(), id);
        std::fs::write(&log_file, "server starting...\nlistening on port 3000\n").unwrap();

        let res = tool
            .execute(&ctx, json!({"action": "log", "id": id, "lines": 10}))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["id"], id);
        assert!(res.value["data"]["log"].as_str().unwrap().contains("listening on port 3000"));
    }

    #[tokio::test]
    async fn test_manage_process_invalid_action() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "restart", "id": "abc"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid action 'restart'"));
    }
}
