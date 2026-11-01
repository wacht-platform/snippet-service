//! plan_goal and replan_goal: decompose a goal into slices, file them as linked
//! tasks in one call, and route parallel slices so they never share a workspace.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::goals::is_open;
use crate::coordination::{Criterion, Goal, GoalStatus, Task, TaskLink, TaskLinkKind, TaskStatus, VerifyMethod};
use crate::llm::NativeToolDefinition;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceArg {
    title: String,
    brief: String,
    #[serde(default)]
    acceptance: Vec<Criterion>,
    #[serde(default)]
    depends_on: Vec<usize>,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default, alias = "agent", alias = "agent_id")]
    suggested_agent: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    verify_mode: Option<String>,
    #[serde(default)]
    owned_paths: Vec<String>,
}

fn slice_schema(what: &str) -> Value {
    json!({"type":"array","items":{"type":"object","properties":{
        "title":{"type":"string"},
        "brief":{"type":"string","description":"what this slice owns and delivers; the worker also sees the goal and the other slices"},
        "acceptance":{"type":"array","items":{"type":"object","properties":{"criterion":{"type":"string"},"method":{"type":"string","enum":["command","artifact","visual","human"]}},"required":["criterion","method"]}},
        "depends_on":{"type":"array","items":{"type":"integer"},"description":what},
        "blocked_by":{"type":"array","items":{"type":"string"},"description":"ids of existing tasks this slice also waits on"},
        "suggested_agent":{"type":"string","description":"specialist from list_coordination_agents"},
        "session":{"type":"string","description":"session to run in; defaults to the plan's session"},
        "read_only":{"type":"boolean"},
        "verify_mode":{"type":"string","enum":["self","reviewer","mission_control","user"],"description":"default: self if read_only, mission_control if every check is human, else a reviewer lane"},
        "owned_paths":{"type":"array","items":{"type":"string"}}
    },"required":["title","brief"],"additionalProperties":false}})
}

fn default_verify(slice: &SliceArg) -> &'static str {
    if slice.read_only {
        "self"
    } else if !slice.acceptance.is_empty() && slice.acceptance.iter().all(|c| c.method == VerifyMethod::Human) {
        "mission_control"
    } else {
        "reviewer"
    }
}

/// A planned or running task as seen by the router.
struct Node {
    task_id: String,
    workspace: String,
    /// Indexes of nodes that must finish before this one (transitively).
    ancestors: HashSet<usize>,
}

/// Earlier nodes sharing `workspace` that could run alongside a slice whose
/// ancestors are `ancestors`: the ones it does not wait on.
fn parallel_conflicts(nodes: &[Node], workspace: &str, ancestors: &HashSet<usize>) -> Vec<usize> {
    if workspace.is_empty() {
        return Vec::new();
    }
    (0..nodes.len()).filter(|k| nodes[*k].workspace == workspace && !ancestors.contains(k)).collect()
}

fn workspace_of_session(id: &str) -> String {
    crate::session::state_path_for_id(id)
        .and_then(|p| crate::session::read_session_meta(&p))
        .map(|s| s.workspace)
        .unwrap_or_default()
}

fn is_git_repo(dir: &str) -> bool {
    std::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--git-dir"]).output().is_ok_and(|o| o.status.success())
}

fn store_of(ctx: &ToolContext) -> Result<Store, ToolError> {
    ctx.mission_control_root().ok_or_else(|| ToolError::msg("Goal tools are only available in the Mission Control session."))?;
    ctx.store().map_err(|e| ToolError::msg(format!("open store: {e}")))
}

fn open_goal(store: &Store, id: &str) -> Result<Goal, ToolError> {
    let goal = store
        .get_goal(id.trim())
        .map_err(|e| ToolError::msg(format!("look up goal: {e}")))?
        .ok_or_else(|| ToolError::msg(format!("no goal `{id}`; list_goals shows them")))?;
    if goal.status != GoalStatus::Open {
        return Err(ToolError::msg(format!("goal is {}; reopen it with update_goal first", goal.status.as_str())));
    }
    Ok(goal)
}

fn transitive_blockers(store: &Store, id: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(next) = stack.pop() {
        for b in store.blockers_of(&next).unwrap_or_default() {
            if seen.insert(b.clone()) {
                stack.push(b);
            }
        }
    }
    seen
}

fn plan_entries(goal: &Goal) -> Vec<Value> {
    goal.plan["slices"].as_array().cloned().unwrap_or_default()
}

fn save_plan(store: &Store, goal: &mut Goal, entries: Vec<Value>) -> Result<(), ToolError> {
    goal.plan = json!({"slices": entries});
    store.save_goal(goal).map_err(|e| ToolError::msg(format!("save plan: {e}")))
}

/// File `slices` as tasks of `goal`, recording each in the plan as it lands.
async fn file_slices(ctx: &ToolContext, store: &Store, goal: &mut Goal, slices: &[SliceArg], default_session: Option<&str>) -> Result<Vec<Value>, ToolError> {
    let mut titles = HashSet::new();
    for (i, s) in slices.iter().enumerate() {
        if s.title.trim().is_empty() || s.brief.trim().is_empty() {
            return Err(ToolError::msg(format!("slice {i}: title and brief must be non-empty")));
        }
        if !titles.insert(s.title.trim().to_lowercase()) {
            return Err(ToolError::msg(format!("slice {i}: titles must be distinct (\"{}\")", s.title.trim())));
        }
        if !s.read_only && s.acceptance.is_empty() {
            return Err(ToolError::msg(format!("slice {i}: acceptance is required unless read_only")));
        }
        if let Some(bad) = s.depends_on.iter().find(|d| **d >= i) {
            return Err(ToolError::msg(format!("slice {i}: depends_on {bad} must be the index of an earlier slice (0-based, in this list)")));
        }
        if s.session.as_deref().map(str::trim).filter(|x| !x.is_empty()).or(default_session).is_none() {
            return Err(ToolError::msg(format!("slice {i}: name a session (on the slice or for the plan)")));
        }
    }
    let mut sessions = Vec::new();
    for s in slices {
        let query = s.session.as_deref().map(str::trim).filter(|x| !x.is_empty()).or(default_session).unwrap_or_default();
        sessions.push(crate::mission_tools::resolve_target_session(query)?);
    }

    let live: Vec<Task> = store
        .goal_tasks(&goal.id)
        .map_err(|e| ToolError::msg(format!("goal tasks: {e}")))?
        .into_iter()
        .filter(|t| is_open(t) && t.status != TaskStatus::Failed)
        .collect();
    let mut nodes: Vec<Node> = live
        .iter()
        .map(|t| Node { task_id: t.id.clone(), workspace: workspace_of_session(&t.session_id), ancestors: HashSet::new() })
        .collect();
    for k in 0..nodes.len() {
        let up = transitive_blockers(store, &nodes[k].task_id);
        nodes[k].ancestors = (0..nodes.len()).filter(|j| up.contains(&nodes[*j].task_id)).collect();
    }
    let mut entries = plan_entries(goal);
    let first = entries.len();
    let mut rows = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for (i, slice) in slices.iter().enumerate() {
        let mut session = sessions[i].clone();
        let mut workspace = workspace_of_session(&session);
        let mut blockers: Vec<String> = slice.depends_on.iter().map(|d| ids[*d].clone()).collect();
        blockers.extend(slice.blocked_by.iter().map(|b| b.trim().to_string()).filter(|b| !b.is_empty()));
        let mut ancestors: HashSet<usize> = HashSet::new();
        for b in &blockers {
            let mut up = transitive_blockers(store, b);
            up.insert(b.clone());
            ancestors.extend((0..nodes.len()).filter(|k| up.contains(&nodes[*k].task_id)));
        }
        let mut note = String::new();
        let conflicts = parallel_conflicts(&nodes, &workspace, &ancestors);
        if !conflicts.is_empty() {
            let title = format!("{}: {}", goal.title.trim(), slice.title.trim());
            let isolated = is_git_repo(&workspace)
                .then(|| crate::session::create_blank_session(std::path::Path::new(&workspace), &title, true, crate::session::WorkspaceMode::Worktree).ok())
                .flatten()
                .filter(|s| workspace_of_session(&s.id) != workspace);
            match isolated {
                Some(info) => {
                    session = info.id;
                    workspace = workspace_of_session(&session);
                    note = "runs in its own worktree session: another slice works the same repo in parallel".into();
                }
                None => {
                    for k in &conflicts {
                        blockers.push(nodes[*k].task_id.clone());
                        ancestors.insert(*k);
                        ancestors.extend(nodes[*k].ancestors.iter().copied());
                    }
                    note = "waits for slices sharing its workspace: no worktree is possible there".into();
                }
            }
        }
        blockers.dedup();
        let mut args = json!({
            "title": slice.title, "description": slice.brief, "session": session, "handoff_mode": "fresh",
            "acceptance": slice.acceptance, "read_only": slice.read_only, "blocked_by": blockers,
            "owned_paths": slice.owned_paths, "goal": goal.id,
            "verify_mode": slice.verify_mode.as_deref().unwrap_or_else(|| default_verify(slice)),
        });
        if let Some(agent) = slice.suggested_agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            args["agent_id"] = json!(agent);
        }
        let made = crate::mission_tools::create_task_value(ctx, args).await.map_err(|e| {
            ToolError::msg(format!("slice {i} \"{}\" not filed ({} filed before it, kept in the plan): {e}", slice.title.trim(), rows.len()))
        })?;
        let task_id = made["task"]["id"].as_str().unwrap_or_default().to_string();
        let n = entries.len();
        entries.push(json!({"n": n, "task_id": task_id, "title": slice.title.trim(), "depends_on": slice.depends_on.iter().map(|d| first + d).collect::<Vec<_>>()}));
        save_plan(store, goal, entries.clone())?;
        nodes.push(Node { task_id: task_id.clone(), workspace, ancestors });
        ids.push(task_id.clone());
        rows.push(json!({
            "slice": n, "task_id": task_id, "title": slice.title.trim(), "session": session,
            "agent": made["agent_id"], "verify_mode": made["verify_mode"], "waiting_on": made["waiting_on"], "note": note,
        }));
    }
    Ok(rows)
}

#[derive(Deserialize)]
struct PlanGoalArgs {
    goal: String,
    slices: Vec<SliceArg>,
    #[serde(default)]
    session: Option<String>,
}

pub struct PlanGoal;
#[async_trait]
impl Tool for PlanGoal {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "plan_goal".into(),
            description: "Decompose an open goal into slices and file them all as linked tasks in one call: depends_on becomes blocked_by, independent slices start in parallel, and parallel slices on one repo get separate worktree sessions. Each worker sees the goal, its slice and the other slices. Once per goal; change a plan with replan_goal.".into(),
            input_schema: json!({"type":"object","properties":{
                "goal":{"type":"string","description":"goal id"},
                "session":{"type":"string","description":"default session for slices that name none"},
                "slices":slice_schema("0-based indexes of earlier slices in this list this one needs finished first")
            },"required":["goal","slices"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PlanGoalArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(format!("plan_goal arguments: {e}")))?;
        let store = store_of(ctx)?;
        let mut goal = open_goal(&store, &args.goal)?;
        if !plan_entries(&goal).is_empty() {
            return Err(ToolError::msg("this goal already has a plan; use replan_goal to add, drop or reorder slices"));
        }
        if args.slices.is_empty() {
            return Err(ToolError::msg("slices must not be empty"));
        }
        let rows = file_slices(ctx, &store, &mut goal, &args.slices, args.session.as_deref()).await?;
        Ok(ToolResult::success(json!({"goal": goal.id, "slices": rows})))
    }
}

#[derive(Deserialize)]
struct ReorderArg {
    task_id: String,
    blocked_by: Vec<String>,
}

#[derive(Deserialize)]
struct ReplanArgs {
    goal: String,
    #[serde(default)]
    add: Vec<SliceArg>,
    #[serde(default)]
    drop: Vec<String>,
    #[serde(default)]
    reorder: Vec<ReorderArg>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    session: Option<String>,
}

pub struct ReplanGoal;
#[async_trait]
impl Tool for ReplanGoal {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "replan_goal".into(),
            description: "Change a goal's plan and keep finished work: add slices (as in plan_goal; depends_on indexes this add list, blocked_by names existing tasks), drop slices by task id (cancelled with the reason; done ones are kept), or reorder unstarted slices by giving a task its new blocked_by.".into(),
            input_schema: json!({"type":"object","properties":{
                "goal":{"type":"string"},
                "add":slice_schema("0-based indexes of earlier slices in this add list"),
                "drop":{"type":"array","items":{"type":"string"},"description":"task ids to drop"},
                "reorder":{"type":"array","items":{"type":"object","properties":{"task_id":{"type":"string"},"blocked_by":{"type":"array","items":{"type":"string"}}},"required":["task_id","blocked_by"]}},
                "reason":{"type":"string","description":"why the plan changed; required with drop"},
                "session":{"type":"string","description":"default session for added slices that name none"}
            },"required":["goal"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReplanArgs = serde_json::from_value(arguments).map_err(|e| ToolError::msg(format!("replan_goal arguments: {e}")))?;
        let store = store_of(ctx)?;
        let mut goal = open_goal(&store, &args.goal)?;
        let linked: HashSet<String> = store.goal_tasks(&goal.id).map_err(|e| ToolError::msg(format!("goal tasks: {e}")))?.into_iter().map(|t| t.id).collect();
        let reason = args.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
        if !args.drop.is_empty() && reason.is_none() {
            return Err(ToolError::msg("a reason is required to drop slices"));
        }
        for id in args.drop.iter().chain(args.reorder.iter().map(|r| &r.task_id)) {
            if !linked.contains(id.trim()) {
                return Err(ToolError::msg(format!("task `{id}` is not part of this goal")));
            }
        }
        let mut dropped = Vec::new();
        let mut kept = Vec::new();
        for id in args.drop.iter().map(|i| i.trim()) {
            let task = store.get_task(id).ok().flatten();
            if task.as_ref().is_some_and(|t| t.status == TaskStatus::Done) {
                kept.push(id.to_string());
                continue;
            }
            let cancel = json!({"task_id": id, "reason": format!("dropped from the plan: {}", reason.unwrap_or_default())});
            crate::mission_tools::CancelMissionTask.execute(ctx, cancel).await?;
            dropped.push(id.to_string());
        }
        let mut reordered = Vec::new();
        let now = chrono::Utc::now().to_rfc3339();
        for r in &args.reorder {
            let id = r.task_id.trim();
            let task = store.get_task(id).ok().flatten().ok_or_else(|| ToolError::msg(format!("no task `{id}`")))?;
            if !matches!(task.status, TaskStatus::Todo | TaskStatus::Blocked) {
                return Err(ToolError::msg(format!("task `{id}` is {}; only unstarted slices can be reordered", task.status)));
            }
            let old = store.blockers_of(id).unwrap_or_default();
            for b in &old {
                let _ = store.unlink_tasks(b, id, &TaskLinkKind::Blocks);
            }
            for b in r.blocked_by.iter().map(|b| b.trim()).filter(|b| !b.is_empty()) {
                let link = TaskLink { from_task_id: b.to_string(), to_task_id: id.to_string(), kind: TaskLinkKind::Blocks, created_at: now.clone() };
                if let Err(e) = store.link_tasks(&link) {
                    for o in &old {
                        let _ = store.link_tasks(&TaskLink { from_task_id: o.clone(), to_task_id: id.to_string(), kind: TaskLinkKind::Blocks, created_at: now.clone() });
                    }
                    return Err(ToolError::msg(format!("reorder {id}: {e}")));
                }
            }
            let waiting = store.blockers_of(id).unwrap_or_default().iter().any(|b| store.get_task(b).ok().flatten().is_some_and(|t| !t.status.releases_dependants()));
            let _ = store.update_task_in(id, &now, |t| match (waiting, &t.status) {
                (true, TaskStatus::Todo) => t.status = TaskStatus::Blocked,
                (false, TaskStatus::Blocked) if t.dispatch_failures == 0 => t.status = TaskStatus::Todo,
                _ => {}
            });
            reordered.push(id.to_string());
        }
        let added = if args.add.is_empty() { Vec::new() } else { file_slices(ctx, &store, &mut goal, &args.add, args.session.as_deref()).await? };
        Ok(ToolResult::success(json!({"goal": goal.id, "added": added, "dropped": dropped, "kept_done": kept, "reordered": reordered})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(task: &str, ws: &str, anc: &[usize]) -> Node {
        Node { task_id: task.into(), workspace: ws.into(), ancestors: anc.iter().copied().collect() }
    }

    #[test]
    fn only_unrelated_slices_in_one_workspace_conflict() {
        let nodes = [node("a", "/repo", &[]), node("b", "/repo", &[0]), node("c", "/other", &[])];
        let none: HashSet<usize> = HashSet::new();
        assert_eq!(parallel_conflicts(&nodes, "/repo", &none), vec![0, 1]);
        let after_b: HashSet<usize> = [0, 1].into_iter().collect();
        assert!(parallel_conflicts(&nodes, "/repo", &after_b).is_empty());
        assert!(parallel_conflicts(&nodes, "/elsewhere", &none).is_empty());
        assert!(parallel_conflicts(&nodes, "", &none).is_empty());
    }

    #[test]
    fn verify_mode_follows_what_the_checks_are() {
        let slice = |read_only: bool, methods: &[&str]| -> SliceArg {
            serde_json::from_value(json!({"title":"t","brief":"b","read_only":read_only,"acceptance":methods.iter().map(|m| json!({"criterion":"c","method":m})).collect::<Vec<_>>()})).unwrap()
        };
        assert_eq!(default_verify(&slice(true, &[])), "self");
        assert_eq!(default_verify(&slice(false, &["human", "human"])), "mission_control");
        assert_eq!(default_verify(&slice(false, &["command", "human"])), "reviewer");
    }

    async fn call(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> Result<Value, String> {
        tool.execute(ctx, args).await.map(|r| r.value["data"].clone()).map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn bad_slices_are_refused_before_anything_is_filed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::mission_control(dir.path()).unwrap().with_store_path(dir.path().join("snippet.db"));
        let store = ctx.store().unwrap();
        let mut goal = Goal::new("g".into(), "G".into(), "O".into(), vec![], "u".into(), "2026-01-01T00:00:00Z".into());
        store.create_goal(&goal).unwrap();
        let ac = json!([{"criterion": "c", "method": "command"}]);
        let plan = |slices: Value| json!({"goal": "g", "session": "s", "slices": slices});
        let err = |slices: Value| async { call(&PlanGoal, &ctx, plan(slices)).await.unwrap_err() };
        assert!(err(json!([{"title": "a", "brief": "b", "depends_on": [0], "acceptance": ac}])).await.contains("earlier slice"));
        assert!(err(json!([{"title": "a", "brief": "b", "acceptance": ac}, {"title": "A", "brief": "b", "acceptance": ac}])).await.contains("distinct"));
        assert!(err(json!([{"title": "a", "brief": "b"}])).await.contains("acceptance"));
        assert!(call(&PlanGoal, &ctx, json!({"goal": "g", "slices": [{"title": "a", "brief": "b", "acceptance": ac}]})).await.unwrap_err().contains("name a session"));
        assert!(call(&PlanGoal, &ctx, json!({"goal": "g", "slices": []})).await.unwrap_err().contains("not be empty"));
        assert!(store.goal_tasks("g").unwrap().is_empty());
        goal.plan = json!({"slices": [{"n": 0, "task_id": "x", "title": "x", "depends_on": []}]});
        store.save_goal(&goal).unwrap();
        assert!(err(json!([{"title": "a", "brief": "b", "acceptance": ac}])).await.contains("replan_goal"));
    }

    #[tokio::test]
    async fn replan_drops_cancel_reorder_rewires_and_done_work_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::mission_control(dir.path()).unwrap().with_store_path(dir.path().join("snippet.db"));
        let store = ctx.store().unwrap();
        store.create_goal(&Goal::new("g".into(), "G".into(), "O".into(), vec![], "u".into(), "2026-01-01T00:00:00Z".into())).unwrap();
        let mk = |id: &str, status: TaskStatus| {
            let mut t = Task::filed_by_human(id.into(), id.into(), "d".into(), "".into(), 0, "2026-01-01T00:00:00Z".into());
            t.goal_id = Some("g".into());
            t.status = status;
            store.create_task(&t).unwrap();
        };
        mk("a", TaskStatus::Done);
        mk("b", TaskStatus::Todo);
        mk("c", TaskStatus::Blocked);
        store.link_tasks(&TaskLink { from_task_id: "b".into(), to_task_id: "c".into(), kind: TaskLinkKind::Blocks, created_at: "2026-01-01T00:00:00Z".into() }).unwrap();
        let out = call(&ReplanGoal, &ctx, json!({"goal": "g", "drop": ["a", "b"], "reason": "scope cut", "reorder": [{"task_id": "c", "blocked_by": ["a"]}]})).await.unwrap();
        assert_eq!(out["kept_done"], json!(["a"]));
        assert_eq!(out["dropped"], json!(["b"]));
        assert_eq!(store.get_task("b").unwrap().unwrap().status, TaskStatus::Cancelled);
        assert_eq!(store.get_task("a").unwrap().unwrap().status, TaskStatus::Done);
        assert_eq!(store.blockers_of("c").unwrap(), vec!["a".to_string()]);
        assert_eq!(store.get_task("c").unwrap().unwrap().status, TaskStatus::Todo);
        assert!(call(&ReplanGoal, &ctx, json!({"goal": "g", "drop": ["c"]})).await.unwrap_err().contains("reason"));
        assert!(call(&ReplanGoal, &ctx, json!({"goal": "g", "drop": ["zzz"], "reason": "x"})).await.unwrap_err().contains("not part"));
    }

    #[test]
    fn each_slice_sees_the_goal_its_neighbours_and_finished_reports() {
        let store = Store::open_in_memory().unwrap();
        let mut goal = Goal::new("g".into(), "Launch".into(), "Site is live".into(), vec![crate::coordination::GoalCriterion { criterion: "loads".into(), method: VerifyMethod::Command, evidence: String::new() }], "u".into(), "2026-01-01T00:00:00Z".into());
        goal.plan = json!({"slices": [{"n": 0, "task_id": "a"}, {"n": 1, "task_id": "b"}, {"n": 2, "task_id": "c"}]});
        store.create_goal(&goal).unwrap();
        for (id, status) in [("a", TaskStatus::Done), ("b", TaskStatus::InProgress), ("c", TaskStatus::Blocked)] {
            let mut t = Task::filed_by_human(id.into(), format!("Slice {id}"), format!("owns {id}"), "s".into(), 0, "2026-01-01T00:00:00Z".into());
            t.goal_id = Some("g".into());
            t.status = status;
            if id == "a" {
                t.result = Some(crate::coordination::TaskResult { summary: "schema shipped".into(), artifacts: vec![], authoritative: true });
            }
            store.create_task(&t).unwrap();
        }
        let c = store.get_task("c").unwrap().unwrap();
        let full = crate::coordination::goals::goal_block(&store, &c, true);
        for needle in ["goal: Launch: Site is live", "goal acceptance: loads", "this is slice 2", "slice 0 \"Slice a\" (done", "slice 1 \"Slice b\" (in progress", "reports from finished slices", "schema shipped"] {
            assert!(full.contains(needle), "{needle} missing in\n{full}");
        }
        let compact = crate::coordination::goals::goal_block(&store, &c, false);
        assert!(!compact.contains("schema shipped") && compact.contains("slice 1"), "{compact}");
        store.link_tasks(&TaskLink { from_task_id: "a".into(), to_task_id: "c".into(), kind: TaskLinkKind::Blocks, created_at: "2026-01-01T00:00:00Z".into() }).unwrap();
        assert!(!crate::coordination::goals::goal_block(&store, &c, true).contains("reports from finished slices"));
        let plain = Task::filed_by_human("p".into(), "P".into(), "d".into(), "s".into(), 0, "2026-01-01T00:00:00Z".into());
        assert_eq!(crate::coordination::goals::goal_block(&store, &plain, true), "");
    }

    /// Run with a throwaway HOME: it writes real session state.
    /// `HOME=$(mktemp -d) cargo test --lib plan_goal_end_to_end -- --ignored`
    #[tokio::test]
    #[ignore = "writes session state under HOME"]
    async fn plan_goal_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [&["init", "-q"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]] {
            assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
        }
        let info = crate::session::create_blank_session(&repo, "home", false, crate::session::WorkspaceMode::Folder).unwrap();
        let ctx = ToolContext::mission_control(dir.path().join("mc")).unwrap().with_store_path(dir.path().join("snippet.db"));
        let store = ctx.store().unwrap();
        store
            .create_agent(&crate::coordination::types::Agent {
                id: "snippet".into(),
                display_name: "Snippet".into(),
                handle: "snippet".into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                role: crate::coordination::types::AgentRole::Implementer,
                capabilities: vec![],
            })
            .unwrap();
        let goal = call(&crate::goal_tools::CreateGoal, &ctx, json!({"title": "Ship", "outcome": "Shipped", "acceptance": [{"criterion": "works", "method": "command"}]})).await.unwrap();
        let gid = goal["goal"]["id"].as_str().unwrap().to_string();
        let ac = json!([{"criterion": "tests pass", "method": "command"}]);
        let out = call(&PlanGoal, &ctx, json!({"goal": gid, "session": info.id, "slices": [
            {"title": "API", "brief": "build the api", "acceptance": ac},
            {"title": "UI", "brief": "build the ui", "acceptance": ac},
            {"title": "Docs", "brief": "write docs", "read_only": true},
            {"title": "Integrate", "brief": "wire them", "acceptance": [{"criterion": "looks right", "method": "human"}], "depends_on": [0, 1]}
        ]})).await.unwrap();
        let rows = out["slices"].as_array().unwrap();
        let sessions: Vec<&str> = rows.iter().map(|r| r["session"].as_str().unwrap()).collect();
        assert_eq!(sessions[0], info.id);
        assert_ne!(sessions[1], sessions[0], "parallel slice in the same repo gets its own worktree session");
        assert_ne!(sessions[2], sessions[0]);
        assert_ne!(sessions[2], sessions[1]);
        assert_eq!(rows[3]["verify_mode"], "mission_control");
        assert_eq!(rows[2]["verify_mode"], "self");
        let tasks = store.goal_tasks(&gid).unwrap();
        assert_eq!(tasks.len(), 4);
        let integrate = tasks.iter().find(|t| t.title == "Integrate").unwrap();
        assert_eq!(integrate.status, TaskStatus::Blocked);
        assert_eq!(store.blockers_of(&integrate.id).unwrap().len(), 2);
        let block = crate::coordination::goals::goal_block(&store, integrate, true);
        assert!(block.contains("slice 0 \"API\"") && block.contains("this is slice 3"), "{block}");
        let g = store.get_goal(&gid).unwrap().unwrap();
        assert_eq!(g.plan["slices"][3]["depends_on"], json!([0, 1]));
    }
}
