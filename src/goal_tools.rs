//! Mission Control's goal tools: a goal is the user's whole outcome, held as
//! data with its own acceptance criteria and the tasks that serve it.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::{Goal, GoalCriterion, GoalStatus};
use crate::llm::NativeToolDefinition;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

fn mc_store(ctx: &ToolContext) -> Result<Store, ToolError> {
    ctx.mission_control_root()
        .ok_or_else(|| ToolError::msg("Goal tools are only available in the Mission Control session."))?;
    ctx.store().map_err(|e| ToolError::msg(format!("open store: {e}")))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn find(store: &Store, id: &str) -> Result<Goal, ToolError> {
    store
        .get_goal(id.trim())
        .map_err(|e| ToolError::msg(format!("look up goal: {e}")))?
        .ok_or_else(|| ToolError::msg(format!("no goal `{id}`; list_goals shows them")))
}

fn save(store: &Store, goal: &Goal) -> Result<(), ToolError> {
    store.save_goal(goal).map_err(|e| ToolError::msg(format!("save goal: {e}")))
}

fn tasks_of(store: &Store, id: &str) -> Result<Vec<crate::coordination::Task>, ToolError> {
    store.goal_tasks(id).map_err(|e| ToolError::msg(format!("goal tasks: {e}")))
}

fn criteria_schema() -> Value {
    json!({"type":"array","items":{"type":"object","properties":{"criterion":{"type":"string"},"method":{"type":"string","enum":["command","artifact","visual","human"]}},"required":["criterion"]},"description":"checks that prove the whole outcome, end to end"})
}

fn evidence_schema() -> Value {
    json!({"type":"array","items":{"type":"object","properties":{"criterion":{"type":"string","description":"number or text"},"evidence":{"type":"string"}},"required":["criterion","evidence"]},"description":"evidence that criteria hold: what was run or seen"})
}

#[derive(Deserialize)]
struct EvidenceArg {
    criterion: String,
    evidence: String,
}

fn pairs(items: &[EvidenceArg]) -> Vec<(String, String)> {
    items.iter().map(|e| (e.criterion.clone(), e.evidence.clone())).collect()
}

/// Goal row for listings: progress without the plan or the full text.
pub fn goal_row(goal: &Goal, tasks: &[crate::coordination::Task]) -> Value {
    let (done, total) = crate::coordination::goals::progress(tasks);
    json!({
        "id": goal.id, "title": goal.title, "status": goal.status,
        "tasks": format!("{done}/{total}"),
        "evidence": format!("{}/{}", goal.acceptance.len() - goal.unevidenced().len(), goal.acceptance.len()),
        "drive": goal.drive(),
    })
}

fn drive_schema() -> Value {
    json!({
        "next_step":{"type":"string","description":"the very next thing that moves this goal"},
        "waiting_on":{"type":"string","description":"what the user owes for it to move, e.g. sign off on the four targets. Empty clears it."},
        "waiting_since":{"type":"string","description":"optional YYYY-MM-DD, when it has already been waiting; defaults to now"},
        "due":{"type":"string","description":"YYYY-MM-DD; empty clears it"},
    })
}

#[derive(Deserialize, Default)]
struct DriveArgs {
    #[serde(default)]
    next_step: Option<String>,
    #[serde(default)]
    waiting_on: Option<String>,
    #[serde(default)]
    waiting_since: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    nudged: bool,
    #[serde(default)]
    snooze_days: Option<i64>,
}

fn checked_date(field: &str, raw: &str) -> Result<String, ToolError> {
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").map(|_| raw.to_string()).map_err(|_| ToolError::msg(format!("{field} must be YYYY-MM-DD, got `{raw}`")))
}

fn apply_drive(goal: &mut Goal, args: &DriveArgs) -> Result<(), ToolError> {
    let mut drive = goal.drive();
    if let Some(next) = &args.next_step {
        drive.next_step = next.trim().to_string();
    }
    if let Some(waiting) = &args.waiting_on {
        let waiting = waiting.trim();
        if waiting != drive.waiting_on {
            drive.waiting_on = waiting.to_string();
            drive.waiting_since = if waiting.is_empty() { String::new() } else { now() };
            drive.last_nudged_at = String::new();
            drive.nudges = 0;
            drive.snoozed_until = String::new();
        }
    }
    if let Some(since) = args.waiting_since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        drive.waiting_since = checked_date("waiting_since", since)?;
    }
    if let Some(due) = args.due.as_deref().map(str::trim) {
        drive.due = if due.is_empty() { String::new() } else { checked_date("due", due)? };
    }
    if args.nudged {
        drive.last_nudged_at = now();
        drive.nudges += 1;
    }
    if let Some(days) = args.snooze_days {
        let until = chrono::Utc::now() + chrono::Duration::days(days.clamp(1, 90));
        drive.snoozed_until = until.to_rfc3339();
    }
    goal.set_drive(&drive);
    Ok(())
}

#[derive(Deserialize)]
struct CreateGoalArgs {
    title: String,
    outcome: String,
    #[serde(default)]
    acceptance: Vec<GoalCriterion>,
    #[serde(default)]
    requester: Option<String>,
    #[serde(flatten)]
    drive: DriveArgs,
}

pub struct CreateGoal;
#[async_trait]
impl Tool for CreateGoal {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "create_goal".into(),
            description: "Record a long-horizon goal: the user's whole outcome and the checks that prove it. Link tasks to it with create_mission_task `goal`.".into(),
            input_schema: {
                let mut properties = json!({"title":{"type":"string"},"outcome":{"type":"string","description":"the finished thing, at the bar the user would be proud of"},"acceptance":criteria_schema(),"requester":{"type":"string","description":"who asked; default user"}});
                properties.as_object_mut().unwrap().extend(drive_schema().as_object().unwrap().clone());
                json!({"type":"object","properties":properties,"required":["title","outcome","acceptance"],"additionalProperties":false})
            },
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CreateGoalArgs = serde_json::from_value(arguments)
            .map_err(|_| ToolError::InvalidArguments { tool: "create_goal".into() })?;
        let store = mc_store(ctx)?;
        if args.title.trim().is_empty() || args.outcome.trim().is_empty() {
            return Err(ToolError::msg("title and outcome must be non-empty"));
        }
        if args.acceptance.is_empty() {
            return Err(ToolError::msg("acceptance is required: the checks that prove the outcome"));
        }
        let acceptance = args
            .acceptance
            .into_iter()
            .map(|c| GoalCriterion { evidence: String::new(), dropped: String::new(), ..c })
            .collect();
        let requester = args.requester.as_deref().map(str::trim).filter(|r| !r.is_empty()).unwrap_or("user");
        let mut goal = Goal::new(uuid::Uuid::new_v4().to_string(), args.title.trim().into(), args.outcome.trim().into(), acceptance, requester.into(), now());
        apply_drive(&mut goal, &args.drive)?;
        store.create_goal(&goal).map_err(|e| ToolError::msg(format!("create goal: {e}")))?;
        Ok(ToolResult::success(json!({"goal": goal})))
    }
}

#[derive(Deserialize)]
struct UpdateGoalArgs {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    acceptance: Option<Vec<GoalCriterion>>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    evidence: Vec<EvidenceArg>,
    #[serde(default)]
    status: Option<String>,
    #[serde(flatten)]
    drive: DriveArgs,
}

pub struct UpdateGoal;
#[async_trait]
impl Tool for UpdateGoal {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "update_goal".into(),
            description: "Change a goal's title, outcome or acceptance. When the scope moves, change the checks to match: one you remove needs a reason, stays on record as dropped and no longer blocks closing; evidence on unchanged text is kept. Also record evidence for criteria, set what drives it (next_step, waiting_on, due), record a nudge you just gave (nudged) or their 'not now' (snooze_days), or cancel it (status cancelled; needs no open task) or reopen it. Closing is close_goal.".into(),
            input_schema: {
                let mut properties = json!({"id":{"type":"string"},"title":{"type":"string"},"outcome":{"type":"string"},"acceptance":criteria_schema(),"reason":{"type":"string","description":"why the scope changed; required when acceptance removes a check"},"evidence":evidence_schema(),"status":{"type":"string","enum":["open","cancelled"]},"nudged":{"type":"boolean","description":"you just nudged the user about what this waits on"},"snooze_days":{"type":"integer","description":"they said not now: no nudge for this many days"}});
                properties.as_object_mut().unwrap().extend(drive_schema().as_object().unwrap().clone());
                json!({"type":"object","properties":properties,"required":["id"],"additionalProperties":false})
            },
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: UpdateGoalArgs = serde_json::from_value(arguments)
            .map_err(|_| ToolError::InvalidArguments { tool: "update_goal".into() })?;
        let store = mc_store(ctx)?;
        let mut goal = find(&store, &args.id)?;
        if let Some(t) = args.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            goal.title = t.into();
        }
        if let Some(o) = args.outcome.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
            goal.outcome = o.into();
        }
        if let Some(new) = args.acceptance {
            goal.replace_acceptance(new, &args.reason).map_err(ToolError::msg)?;
        }
        let unknown = goal.attach_evidence(&pairs(&args.evidence));
        apply_drive(&mut goal, &args.drive)?;
        match args.status.as_deref().map(str::trim) {
            None | Some("") => {}
            Some("open") => {
                goal.status = GoalStatus::Open;
                goal.closed_at = None;
            }
            Some("cancelled") => {
                let open: Vec<String> = tasks_of(&store, &goal.id)?.iter().filter(|t| crate::coordination::goals::is_open(t)).map(|t| format!("{} ({})", t.id, t.status)).collect();
                if !open.is_empty() {
                    return Err(ToolError::msg(format!("cancel or finish the open tasks first: {}", open.join(", "))));
                }
                goal.status = GoalStatus::Cancelled;
                goal.closed_at = Some(now());
            }
            Some(other) => return Err(ToolError::msg(format!("status must be open or cancelled, got `{other}`; close with close_goal"))),
        }
        save(&store, &goal)?;
        let mut out = json!({"goal": goal});
        if !unknown.is_empty() {
            out["unmatched_evidence"] = json!(unknown);
        }
        Ok(ToolResult::success(out))
    }
}

#[derive(Deserialize)]
struct CloseGoalArgs {
    id: String,
    #[serde(default)]
    evidence: Vec<EvidenceArg>,
}

pub struct CloseGoal;
#[async_trait]
impl Tool for CloseGoal {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "close_goal".into(),
            description: "Close a goal once the user has the result. Refused, with the list, while any linked task is open or any check still in force lacks evidence; pass evidence to record it in the same call. The result lists dropped checks: tell the user about them.".into(),
            input_schema: json!({"type":"object","properties":{"id":{"type":"string"},"evidence":evidence_schema()},"required":["id"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CloseGoalArgs = serde_json::from_value(arguments)
            .map_err(|_| ToolError::InvalidArguments { tool: "close_goal".into() })?;
        let store = mc_store(ctx)?;
        let mut goal = find(&store, &args.id)?;
        if goal.status != GoalStatus::Open {
            return Err(ToolError::msg(format!("goal is already {}", goal.status.as_str())));
        }
        let unknown = goal.attach_evidence(&pairs(&args.evidence));
        let gaps = goal.close_gaps(&tasks_of(&store, &goal.id)?);
        if !gaps.is_empty() {
            save(&store, &goal)?;
            return Err(ToolError::msg(format!("goal not closed; missing:\n- {}", gaps.join("\n- "))));
        }
        goal.status = GoalStatus::Closed;
        goal.closed_at = Some(now());
        save(&store, &goal)?;
        let mut out = json!({"closed": goal.id, "title": goal.title});
        let dropped = goal.dropped_checks();
        if !dropped.is_empty() {
            out["dropped_checks"] = json!(dropped);
        }
        if !unknown.is_empty() {
            out["unmatched_evidence"] = json!(unknown);
        }
        Ok(ToolResult::success(out))
    }
}

#[derive(Deserialize)]
struct ListGoalsArgs {
    #[serde(default)]
    status: Option<String>,
}

pub struct ListGoals;
#[async_trait]
impl Tool for ListGoals {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_goals".into(),
            description: "Goals with task progress (done/live) and evidence progress; open ones by default.".into(),
            input_schema: json!({"type":"object","properties":{"status":{"type":"string","enum":["open","closed","cancelled","all"]}},"additionalProperties":false}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ListGoalsArgs = serde_json::from_value(arguments)
            .map_err(|_| ToolError::InvalidArguments { tool: "list_goals".into() })?;
        let store = mc_store(ctx)?;
        let status = match args.status.as_deref().map(str::trim) {
            None | Some("") | Some("open") => Some(GoalStatus::Open),
            Some("all") => None,
            Some(other) => Some(GoalStatus::parse(other).ok_or_else(|| ToolError::msg("status must be open, closed, cancelled or all"))?),
        };
        let goals = store.list_goals(status).map_err(|e| ToolError::msg(format!("list goals: {e}")))?;
        let rows = goals
            .iter()
            .map(|g| tasks_of(&store, &g.id).map(|t| goal_row(g, &t)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ToolResult::success(json!({"goals": rows})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{Task, TaskStatus};

    fn mc(dir: &std::path::Path) -> ToolContext {
        ToolContext::mission_control(dir).unwrap().with_store_path(dir.join("snippet.db"))
    }

    async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> Result<Value, String> {
        tool.execute(ctx, args).await.map(|r| r.value["data"].clone()).map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn a_goal_closes_only_when_tasks_are_done_and_every_check_has_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mc(dir.path());
        let made = call(&CreateGoal, &ctx, json!({"title": "Ship", "outcome": "It ships", "acceptance": [{"criterion": "tests pass", "method": "command"}, {"criterion": "looks right"}]})).await.unwrap();
        let id = made["goal"]["id"].as_str().unwrap().to_string();
        assert_eq!(made["goal"]["acceptance"][1]["method"], "human");

        let store = ctx.store().unwrap();
        let mut task = Task::filed_by_human("t1".into(), "Build".into(), "do".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        task.goal_id = Some(id.clone());
        store.create_task(&task).unwrap();

        let err = call(&CloseGoal, &ctx, json!({"id": id})).await.unwrap_err();
        assert!(err.contains("task t1") && err.contains("1. [command] tests pass") && err.contains("2. [human] looks right"), "{err}");

        store.update_task_in("t1", "2026-01-02T00:00:00Z", |t| t.status = TaskStatus::Done).unwrap();
        let err = call(&CloseGoal, &ctx, json!({"id": id, "evidence": [{"criterion": "1", "evidence": "cargo test ok"}]})).await.unwrap_err();
        assert!(!err.contains("task t1") && !err.contains("tests pass") && err.contains("looks right"), "{err}");

        let listed = call(&ListGoals, &ctx, json!({})).await.unwrap();
        assert_eq!(listed["goals"][0]["tasks"], "1/1");
        assert_eq!(listed["goals"][0]["evidence"], "1/2");

        call(&CloseGoal, &ctx, json!({"id": id, "evidence": [{"criterion": "looks right", "evidence": "screenshot"}]})).await.unwrap();
        assert!(call(&ListGoals, &ctx, json!({})).await.unwrap()["goals"].as_array().unwrap().is_empty());
        assert_eq!(call(&ListGoals, &ctx, json!({"status": "closed"})).await.unwrap()["goals"][0]["status"], "closed");
    }

    #[tokio::test]
    async fn update_keeps_evidence_and_cancel_needs_no_open_task() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mc(dir.path());
        let made = call(&CreateGoal, &ctx, json!({"title": "G", "outcome": "O", "acceptance": [{"criterion": "a", "method": "command"}]})).await.unwrap();
        let id = made["goal"]["id"].as_str().unwrap().to_string();
        call(&UpdateGoal, &ctx, json!({"id": id, "evidence": [{"criterion": "1", "evidence": "ran"}]})).await.unwrap();
        let out = call(&UpdateGoal, &ctx, json!({"id": id, "acceptance": [{"criterion": "a", "method": "command"}, {"criterion": "b"}], "evidence": [{"criterion": "zzz", "evidence": "x"}]})).await.unwrap();
        assert_eq!(out["goal"]["acceptance"][0]["evidence"], "ran");
        assert_eq!(out["goal"]["acceptance"][1]["evidence"], "");
        assert_eq!(out["unmatched_evidence"][0], "zzz");

        let store = ctx.store().unwrap();
        let mut task = Task::filed_by_human("t1".into(), "T".into(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        task.goal_id = Some(id.clone());
        store.create_task(&task).unwrap();
        assert!(call(&UpdateGoal, &ctx, json!({"id": id, "status": "cancelled"})).await.unwrap_err().contains("t1"));
        store.update_task_in("t1", "2026-01-02T00:00:00Z", |t| t.status = TaskStatus::Cancelled).unwrap();
        call(&UpdateGoal, &ctx, json!({"id": id, "status": "cancelled"})).await.unwrap();
        assert!(open_goal_check(&store, &id).is_err());
    }

    fn open_goal_check(store: &Store, id: &str) -> Result<(), String> {
        let g = store.get_goal(id).unwrap().unwrap();
        if g.status == GoalStatus::Open { Ok(()) } else { Err(g.status.as_str().into()) }
    }

    #[tokio::test]
    async fn goal_tools_refuse_outside_mission_control() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path()).unwrap().with_store_path(dir.path().join("snippet.db"));
        assert!(call(&ListGoals, &ctx, json!({})).await.unwrap_err().contains("Mission Control"));
    }

    #[test]
    fn a_task_can_only_link_to_an_open_goal() {
        let store = Store::open_in_memory().unwrap();
        let mut g = Goal::new("g".into(), "G".into(), "O".into(), vec![], "u".into(), "2026-01-01T00:00:00Z".into());
        store.create_goal(&g).unwrap();
        assert_eq!(crate::mission_tools::open_goal_id(&store, Some("g")).unwrap().as_deref(), Some("g"));
        assert_eq!(crate::mission_tools::open_goal_id(&store, None).unwrap(), None);
        assert!(crate::mission_tools::open_goal_id(&store, Some("nope")).is_err());
        g.status = GoalStatus::Closed;
        store.save_goal(&g).unwrap();
        assert!(crate::mission_tools::open_goal_id(&store, Some("g")).unwrap_err().to_string().contains("closed"));
    }
}
