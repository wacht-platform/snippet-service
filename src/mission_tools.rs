use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::mission_control::{self, TaskResult, TaskStatus};
use crate::session::{list_device_sessions, state_path_for_id};
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
}

fn root(ctx: &ToolContext) -> Result<PathBuf, ToolError> {
    ctx.mission_control_root()
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError::msg(
                "Mission Control tools are only available in the Mission Control session.",
            )
        })
}

fn task_view(task: &mission_control::TaskRecord) -> Value {
    json!({
        "id": task.id, "session_id": task.session_id, "title": task.title,
        "description": task.description, "status": task.status,
        "dependencies": task.dependencies, "handoff": task.handoff,
        "result": task.result, "owned_paths": task.owned_paths,
        "notifications": task.notifications, "updated_at": task.updated_at,
    })
}

pub fn add_mission_control_tools(registry: &mut ToolRegistry) {
    registry.insert(ListSessions);
    registry.insert(InspectSession);
    registry.insert(ListMissionTasks);
    registry.insert(CreateMissionTask);
    registry.insert(ArchiveMissionSession);
}

pub fn add_worker_report_tool(registry: &mut ToolRegistry) {
    registry.insert(ReportMissionTask);
}

pub struct ListSessions;
#[async_trait]
impl Tool for ListSessions {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "list_sessions".into(), description: "List durable device sessions with workspace, title, status, and last activity. Use it before routing work.".into(), input_schema: schema(json!({}), &[]) }
    }
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let sessions = list_device_sessions();
        Ok(ToolResult::success(json!({"sessions": sessions})))
    }
}

#[derive(Deserialize)]
struct SessionArgs {
    session_id: String,
    #[serde(default = "default_limit")]
    event_limit: usize,
}
fn default_limit() -> usize {
    30
}
pub struct InspectSession;
#[async_trait]
impl Tool for InspectSession {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "inspect_session".into(), description: "Read a bounded durable status/history summary for one session. Use only when routing, diagnosing, or synthesizing work.".into(), input_schema: schema(json!({"session_id":{"type":"string"}, "event_limit":{"type":"integer","minimum":1,"maximum":100}}), &["session_id"]) }
    }
    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SessionArgs =
            serde_json::from_value(arguments).map_err(|_| ToolError::InvalidArguments {
                tool: "inspect_session".into(),
            })?;
        let path =
            state_path_for_id(&args.session_id).ok_or_else(|| ToolError::msg("unknown session"))?;
        let state =
            crate::harness::deserialize_state(&std::fs::read(&path)?).map_err(ToolError::msg)?;
        let from = state.events.len().saturating_sub(args.event_limit.min(100));
        Ok(ToolResult::success(json!({
            "id": args.session_id, "workspace": state.workspace, "title": state.title,
            "status": state.status, "pending_question": state.pending_question,
            "approval_mode": state.approval_mode, "lanes": state.lanes,
            "recent_events": &state.events[from..],
        })))
    }
}

pub struct ListMissionTasks;
#[async_trait]
impl Tool for ListMissionTasks {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "list_mission_tasks".into(), description: "List Mission Control's durable task board, including queued, active, blocked, and completed work.".into(), input_schema: schema(json!({}), &[]) }
    }
    async fn execute(&self, ctx: &ToolContext, _arguments: Value) -> Result<ToolResult, ToolError> {
        let root = root(ctx)?;
        let tasks = mission_control::list_tasks(&root, None, None).map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({"tasks": tasks.iter().map(task_view).collect::<Vec<_>>() }),
        ))
    }
}

#[derive(Deserialize)]
struct CreateTaskArgs {
    title: String,
    description: String,
    session_id: String,
    /// `resume` (default): target session already has the context. `fresh`:
    /// the description is a self-contained briefing for a session that lacks it.
    #[serde(default)]
    handoff_mode: Option<String>,
    #[serde(default)]
    owned_paths: Vec<String>,
}
pub struct CreateMissionTask;
#[async_trait]
impl Tool for CreateMissionTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "create_mission_task".into(), description: "Persist and queue a structured handoff to an existing durable session. The daemon dispatches it safely; do not use this for trivial work. handoff_mode: 'resume' when the target session already has the context, 'fresh' when the description must be a self-contained briefing.".into(), input_schema: schema(json!({"title":{"type":"string"}, "description":{"type":"string"}, "session_id":{"type":"string"}, "handoff_mode":{"type":"string","enum":["resume","fresh"]}, "owned_paths":{"type":"array","items":{"type":"string"}}}), &["title","description","session_id"]) }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CreateTaskArgs =
            serde_json::from_value(arguments).map_err(|_| ToolError::InvalidArguments {
                tool: "create_mission_task".into(),
            })?;
        if args.title.trim().is_empty() || args.description.trim().is_empty() {
            return Err(ToolError::msg("title and description must be non-empty"));
        }
        let root = root(ctx)?;
        let path = state_path_for_id(&args.session_id)
            .ok_or_else(|| ToolError::msg("unknown target session"))?;
        let state =
            crate::harness::deserialize_state(&std::fs::read(&path)?).map_err(ToolError::msg)?;
        if mission_control::get_session(&root, &args.session_id).is_err() {
            mission_control::create_session(
                &root,
                &args.session_id,
                state.title.as_deref().unwrap_or("Managed session"),
                std::path::Path::new(&state.workspace),
            )
            .map_err(ToolError::msg)?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let handoff_mode = match args.handoff_mode.as_deref() {
            None | Some("resume") => mission_control::HandoffMode::Resume,
            Some("fresh") => mission_control::HandoffMode::Fresh,
            Some(other) => {
                return Err(ToolError::msg(format!(
                    "handoff_mode must be 'resume' or 'fresh', got '{other}'"
                )));
            }
        };
        let owned_paths = if args.owned_paths.is_empty() {
            vec![PathBuf::from(&state.workspace)]
        } else {
            args.owned_paths.iter().map(PathBuf::from).collect()
        };
        let task = mission_control::update_task(
            &root,
            &mission_control::create_task(
                &root,
                &id,
                &args.session_id,
                args.title.trim(),
                args.description.trim(),
                owned_paths,
            )
            .map_err(ToolError::msg)?
            .id,
            |t| t.handoff_mode = handoff_mode,
        )
        .map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({"task": task_view(&task), "note":"Persisted as pending. The daemon will dispatch it when its workspace is available."}),
        ))
    }
}

#[derive(Deserialize)]
struct ArchiveArgs {
    session_id: String,
}
pub struct ArchiveMissionSession;
#[async_trait]
impl Tool for ArchiveMissionSession {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "archive_mission_session".into(), description: "Archive a managed session without deleting its history. Use after work is complete or at the user's request.".into(), input_schema: schema(json!({"session_id":{"type":"string"}}), &["session_id"]) }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ArchiveArgs =
            serde_json::from_value(arguments).map_err(|_| ToolError::InvalidArguments {
                tool: "archive_mission_session".into(),
            })?;
        let session = mission_control::archive_session(&root(ctx)?, &args.session_id)
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({"archived": true, "session_id": session.id}),
        ))
    }
}

#[derive(Deserialize)]
struct ReportArgs {
    task_id: String,
    status: String,
    summary: String,
    #[serde(default)]
    artifacts: Vec<String>,
}
pub struct ReportMissionTask;
#[async_trait]
impl Tool for ReportMissionTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition { name: "report_mission_task".into(), description: "Report a Mission Control task's completion, failure, or blocker. Use only for the [mission_control_task] envelope delivered to this session.".into(), input_schema: schema(json!({"task_id":{"type":"string"}, "status":{"type":"string","enum":["done","blocked","failed"]}, "summary":{"type":"string"}, "artifacts":{"type":"array","items":{"type":"string"}}}), &["task_id","status","summary"]) }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReportArgs =
            serde_json::from_value(arguments).map_err(|_| ToolError::InvalidArguments {
                tool: "report_mission_task".into(),
            })?;
        // Authority binding: the caller must be the durable session this task
        // was dispatched to. Tasks are bound at dispatch time via
        // `claim_task_for_dispatch`; anything else is rejected.
        let Some(caller) = ctx.durable_session_id() else {
            return Err(ToolError::msg(
                "this session is not bound to a Mission Control task; report_mission_task is only available to dispatched task sessions",
            ));
        };
        let root = mission_control::MissionControlStore::default_root(None);
        let status = match args.status.as_str() {
            "done" => TaskStatus::Done,
            "blocked" => TaskStatus::Blocked,
            "failed" => TaskStatus::Failed,
            _ => return Err(ToolError::msg("status must be done, blocked, or failed")),
        };
        {
            let bound = mission_control::get_task(&root, &args.task_id)
                .map_err(|_| ToolError::msg("unknown task"))?;
            if bound.reporting_session.as_deref() != Some(caller) {
                return Err(ToolError::msg("task was not dispatched to this session"));
            }
        }
        let task = if status.is_terminal() {
            mission_control::complete_task(
                &root,
                &args.task_id,
                status,
                TaskResult {
                    summary: args.summary,
                    artifacts: args.artifacts.into_iter().map(PathBuf::from).collect(),
                    authoritative: true,
                },
            )
            .map_err(ToolError::msg)?
        } else {
            mission_control::update_task(&root, &args.task_id, |task| {
                task.status = status;
                task.owned_paths.clear(); // release ownership while blocked
                task.notifications
                    .push(mission_control::NotificationMarker {
                        target: "mission_control".into(),
                        kind: "blocked".into(),
                        message: args.summary,
                        delivered: false,
                    });
            })
            .map_err(ToolError::msg)?
        };
        Ok(ToolResult::success(json!({"task": task_view(&task)})))
    }
}
