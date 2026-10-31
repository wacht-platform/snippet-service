//! Write gate for task sessions and lanes: the lease holder writes, inside the
//! task's paths, and never into a path another task owns.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::coordination::TaskStatus;
use crate::coordination::types::CoordinationEvent;
use crate::tools::ToolContext;

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Warn(String),
    Refuse(String),
}

fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn resolve(ctx: &ToolContext, raw: &str) -> PathBuf {
    let raw = raw.trim().trim_matches(['"', '\'']);
    let path = Path::new(raw);
    normalise(&if path.is_absolute() { path.to_path_buf() } else { ctx.current_dir().join(path) })
}

/// Paths a write-capable call touches, and whether it is a write at all.
fn write_targets(ctx: &ToolContext, tool: &str, args: &Value) -> Option<Vec<PathBuf>> {
    match tool {
        "change_files" => Some(
            args.get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|c| ["path", "to"].into_iter().filter_map(|k| c.get(k).and_then(Value::as_str)))
                .map(|p| resolve(ctx, p))
                .collect(),
        ),
        "bash" => {
            let command = args.get("command").and_then(Value::as_str)?;
            crate::shell_guard::shell_writes(command).map(|t| t.iter().map(|p| resolve(ctx, p)).collect())
        }
        _ => None,
    }
}

pub fn gate(ctx: &ToolContext, tool: &str, args: &Value) -> Gate {
    if ctx.is_mission_control() || !matches!(tool, "change_files" | "bash") {
        return Gate::Allow;
    }
    let Some(targets) = write_targets(ctx, tool, args) else { return Gate::Allow };
    if tool == "bash" && ctx.read_only_shell() {
        return Gate::Refuse("this lane is read-only: shell commands that modify files are refused. Report what you would change instead.".into());
    }
    let scope = ctx.write_scope();
    if tool == "change_files"
        && !scope.is_empty()
        && let Some(out) = targets.iter().find(|t| !scope.iter().any(|s| t.starts_with(s)))
    {
        let allowed: Vec<String> = scope.iter().map(|s| s.display().to_string()).collect();
        return Gate::Refuse(format!("`{}` is outside this lane's declared paths ({}). Ask the agent that handed you this to widen them.", out.display(), allowed.join(", ")));
    }
    task_gate(ctx, &targets)
}

fn task_gate(ctx: &ToolContext, targets: &[PathBuf]) -> Gate {
    let Some(session) = ctx.durable_session_id().or(ctx.lane_origin().map(|(s, _)| s)) else { return Gate::Allow };
    let Some(path) = ctx.store_path() else { return Gate::Allow };
    let Ok(store) = crate::store::Store::open_cached(path) else { return Gate::Allow };
    let Some(task) = store
        .list_tasks(Some(session), Some(&TaskStatus::InProgress))
        .ok()
        .and_then(|t| t.into_iter().find(|t| t.reporting_session.as_deref() == Some(session)))
    else {
        return Gate::Allow;
    };
    if let Ok(Some(lease)) = store.current_lease(&task.id) {
        let actor = if ctx.lane_origin().is_some() { ctx.agent_id().unwrap_or(&lease.agent_id) } else { &lease.agent_id };
        if actor != lease.agent_id {
            return Gate::Refuse(format!("task `{}` is leased to `{}` (token {}), not `{actor}`; only the holder writes.", task.id, lease.agent_id, lease.fencing_token));
        }
        if ctx.lease_token().is_some_and(|t| t != lease.fencing_token) {
            return Gate::Refuse(format!("the lease on task `{}` moved on to token {}; this lane started under an older one.", task.id, lease.fencing_token));
        }
    }
    let root = ctx.workspace_root();
    let inside: Vec<PathBuf> = targets.iter().filter(|t| t.starts_with(root)).cloned().collect();
    if inside.is_empty() {
        return Gate::Allow;
    }
    for (owner, owned) in store.task_path_conflicts(&task.id, &inside).unwrap_or_default() {
        let same_session = store.get_task(&owner).ok().flatten().is_some_and(|o| o.session_id == task.session_id);
        if !same_session {
            return Gate::Refuse(format!("`{}` is owned by task `{owner}`; wait for it or ask Mission Control to re-scope.", owned.display()));
        }
    }
    if task.owned_paths.is_empty() {
        return Gate::Allow;
    }
    let Some(outside) = inside.iter().find(|t| !task.owned_paths.iter().any(|o| t.starts_with(o))) else { return Gate::Allow };
    let note = format!("`{}` is outside the paths task `{}` owns; recorded as a conflict event.", outside.display(), task.id);
    record_conflict(&store, &task.thread_id, &task.id, &note);
    Gate::Warn(note)
}

fn record_conflict(store: &crate::store::Store, thread_id: &str, task_id: &str, note: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let key = uuid::Uuid::new_v4().to_string();
    let _ = store.append_event(&CoordinationEvent {
        event_id: key.clone(),
        thread_id: thread_id.to_string(),
        partition_key: format!("thread:{thread_id}"),
        sequence: 0,
        event_type: "task.ownership_conflict".into(),
        actor_kind: "system".into(),
        actor_id: "ownership".into(),
        payload_version: 1,
        payload: serde_json::json!({"body": note, "task_id": task_id}),
        causation_id: None,
        correlation_id: Some(task_id.to_string()),
        idempotency_key: key,
        created_at: now,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::Task;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, ToolContext, crate::store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let store = crate::store::Store::open_cached(db.clone()).unwrap();
        let ctx = ToolContext::new(dir.path()).unwrap().with_durable_session_id("s1").with_store_path(db);
        (dir, ctx, store)
    }

    fn running(store: &crate::store::Store, id: &str, session: &str, owned: Vec<PathBuf>) {
        let mut t = Task::filed_by_human(id.into(), id.into(), "d".into(), session.into(), 0, "2026-01-01T00:00:00Z".into());
        t.owned_paths = owned;
        store.create_task(&t).unwrap();
        store.claim_task_for_dispatch(id, "2026-01-01T00:00:01Z").unwrap();
    }

    fn edit(path: &str) -> Value {
        json!({"changes": [{"action": "create", "path": path, "content": "x"}]})
    }

    #[test]
    fn another_tasks_path_is_refused_and_a_stray_one_warns_with_an_event() {
        let (dir, ctx, store) = setup();
        let root = ctx.workspace_root().to_path_buf();
        running(&store, "mine", "s1", vec![root.join("src")]);
        running(&store, "theirs", "s2", vec![root.join("docs")]);
        assert_eq!(gate(&ctx, "change_files", &edit("src/a.rs")), Gate::Allow);
        assert!(matches!(gate(&ctx, "change_files", &edit("docs/a.md")), Gate::Refuse(m) if m.contains("theirs")));
        assert!(matches!(gate(&ctx, "change_files", &edit("other/a.md")), Gate::Warn(_)));
        let events = store.events_for_thread(&Task::thread_for("mine"), 0, 10).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.ownership_conflict"));
        assert_eq!(gate(&ctx, "bash", &json!({"command": "echo hi > docs/x"})), Gate::Refuse(format!("`{}` is owned by task `theirs`; wait for it or ask Mission Control to re-scope.", root.join("docs/x").display())));
        assert_eq!(gate(&ctx, "bash", &json!({"command": "ls"})), Gate::Allow);
        drop(dir);
    }

    #[test]
    fn lanes_obey_scope_read_only_and_the_lease() {
        let (dir, ctx, store) = setup();
        let root = ctx.workspace_root().to_path_buf();
        let lane = ctx.clone().with_write_scope(vec![root.join("src")]);
        assert_eq!(gate(&lane, "change_files", &edit("src/a.rs")), Gate::Allow);
        assert!(matches!(gate(&lane, "change_files", &edit("docs/a.rs")), Gate::Refuse(m) if m.contains("declared paths")));
        let ro = ctx.clone().with_read_only_shell(true);
        assert!(matches!(gate(&ro, "bash", &json!({"command": "rm -rf src"})), Gate::Refuse(_)));
        assert_eq!(gate(&ro, "bash", &json!({"command": "rg foo"})), Gate::Allow);

        running(&store, "t", "s1", vec![]);
        for id in ["a", "b"] {
            store
                .create_agent(&crate::coordination::types::Agent {
                    id: id.into(), display_name: id.into(), handle: id.into(),
                    kind: crate::coordination::types::AgentKind::Worker,
                    status: crate::coordination::types::AgentStatus::Active,
                    role: crate::coordination::types::AgentRole::Implementer, capabilities: vec![],
                })
                .unwrap();
        }
        store.add_task_agent_full("t", "a", "implementer", None, "", "active", "2026-01-01T00:00:00Z").unwrap();
        store.add_task_agent_full("t", "b", "reviewer", None, "", "waiting", "2026-01-01T00:00:00Z").unwrap();
        let stranger = ctx.clone().with_lane_origin("s1", "lane-1").with_agent_id("b");
        assert!(matches!(gate(&stranger, "change_files", &edit("x.rs")), Gate::Refuse(m) if m.contains("leased to `a`")));
        let holder = ctx.clone().with_lane_origin("s1", "lane-1").with_agent_id("a").with_lease_token(Some(1));
        assert_eq!(gate(&holder, "change_files", &edit("x.rs")), Gate::Allow);
        store.transfer_task_session_lease("t", "a", "b").unwrap();
        assert!(matches!(gate(&holder, "change_files", &edit("x.rs")), Gate::Refuse(_)));
        drop(dir);
    }
}
