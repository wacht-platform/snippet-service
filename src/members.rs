//! Seating agents on a task and passing the turn between them.
//!
//! Each member works from its own conversation (its identity, model, memory and
//! cache) in the task's one worktree, and they meet in the task's room. Only the
//! member holding the turn changes files.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::coordination::handoffs::Handover;
use crate::coordination::{Task, TaskAgent};
use crate::store::Store;

/// Above this many dirty paths a snapshot stops hashing and notes the paths only.
const MAX_HASHED_PATHS: usize = 5000;

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub head: String,
    pub files: BTreeMap<String, String>,
}

fn git(workspace: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(workspace).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the worktree holds beyond HEAD. Hashes are taken so an edit to a file that
/// was already dirty still counts, which a plain `git status` can't show.
pub fn snapshot(workspace: &Path) -> Snapshot {
    let Some(head) = git(workspace, &["rev-parse", "HEAD"]) else { return Snapshot::default() };
    let status = git(workspace, &["status", "--porcelain=v1", "-z", "-uall"]).unwrap_or_default();
    let mut paths: Vec<String> = Vec::new();
    let mut parts = status.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = parts.next() {
        let Some(path) = entry.get(3..) else { continue };
        if entry.starts_with('R') || entry.starts_with('C') {
            let _ = parts.next();
        }
        if !path.starts_with(".snippet/") {
            paths.push(path.to_string());
        }
    }
    let mut files = BTreeMap::new();
    let hashable = paths.len() <= MAX_HASHED_PATHS;
    for path in paths {
        let full = workspace.join(&path);
        let hash = if !full.exists() {
            "deleted".to_string()
        } else if hashable && full.is_file() {
            git(workspace, &["hash-object", "--", &path]).map(|h| h.trim().to_string()).unwrap_or_default()
        } else {
            "dirty".to_string()
        };
        files.insert(path, hash);
    }
    Snapshot { head: head.trim().to_string(), files }
}

/// Files and commits that differ between two snapshots of one worktree.
pub fn changes(from: &Snapshot, to: &Snapshot, workspace: &Path) -> (Vec<String>, Vec<String>) {
    let mut files: Vec<String> = from
        .files
        .keys()
        .chain(to.files.keys())
        .filter(|path| from.files.get(*path) != to.files.get(*path))
        .cloned()
        .collect();
    let mut commits = Vec::new();
    if !from.head.is_empty() && !to.head.is_empty() && from.head != to.head {
        let range = format!("{}..{}", from.head, to.head);
        if let Some(listed) = git(workspace, &["diff", "--name-only", &from.head, &to.head]) {
            files.extend(listed.lines().filter(|l| !l.starts_with(".snippet/")).map(str::to_string));
        }
        if let Some(listed) = git(workspace, &["log", "--format=%h %s", &range]) {
            commits = listed.lines().map(str::to_string).collect();
        }
    }
    files.sort();
    files.dedup();
    (files, commits)
}

pub fn task_workspace(store: &Store, task: &Task) -> Option<std::path::PathBuf> {
    // A task may name its session by the old `.json` path, which only some rows keep as legacy_id.
    [Some(task.session_id.as_str()), task.reporting_session.as_deref()]
        .into_iter()
        .flatten()
        .flat_map(|id| [id, id.strip_suffix(".json").unwrap_or(id)])
        .find_map(|id| store.get_session_row(id).ok().flatten())
        .map(|row| std::path::PathBuf::from(row.workspace))
        .filter(|p| !p.as_os_str().is_empty())
}

/// The conversation holding the turn on a task.
pub fn working_session(store: &Store, task: &Task) -> String {
    store
        .list_task_agents(&task.id)
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.status == "active" && m.removed_at.is_none())
        .and_then(|m| m.work_session_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| task.session_id.clone())
}

/// Who's in the room, for a member being woken. Empty until a task has seats.
pub fn room_line(roster: &[TaskAgent]) -> String {
    let live: Vec<&TaskAgent> = roster.iter().filter(|m| m.removed_at.is_none()).collect();
    if !live.iter().any(|m| m.work_session_id.as_deref().is_some_and(|s| !s.is_empty())) {
        return String::new();
    }
    let members: Vec<String> = live
        .iter()
        .map(|m| {
            if m.status == "active" {
                format!("{} ({}, holds the turn)", m.agent_id, m.role)
            } else {
                format!("{} ({})", m.agent_id, m.role)
            }
        })
        .collect();
    format!("members: {}", members.join("; "))
}

/// Starts the record of who changed what. Called when work is dispatched, so the
/// first member's changes are measured from there rather than from a commit
/// that may already have uncommitted work on top of it.
pub fn open_first_turn(store: &Store, task: &Task, workspace: &Path, now: &str) {
    if store.list_turns(&task.id).map(|t| !t.is_empty()).unwrap_or(true) {
        return;
    }
    let holder = store
        .list_task_agents(&task.id)
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.status == "active" && m.removed_at.is_none());
    let Some(holder) = holder else { return };
    let session = holder.work_session_id.as_deref().unwrap_or(&task.session_id);
    if holder.work_session_id.is_none() {
        let _ = store.set_member_session(&task.id, &holder.agent_id, session);
    }
    let snap = serde_json::to_string(&snapshot(workspace)).unwrap_or_default();
    let _ = store.open_turn(&task.id, &holder.agent_id, session, &snap, now);
}

/// Files changed since the task was dispatched, not counting what was already
/// uncommitted then. `None` when the task has no record yet.
pub fn changed_since_dispatch(store: &Store, task: &Task, workspace: &Path) -> Option<Vec<String>> {
    let first = store.list_turns(&task.id).ok()?.into_iter().next()?;
    let from: Snapshot = serde_json::from_str(&first.snapshot).ok()?;
    Some(changes(&from, &snapshot(workspace), workspace).0)
}

/// Ends the open turn and records what it changed.
pub fn close_turn(store: &Store, task: &Task, workspace: &Path, now: &str) -> (Vec<String>, Vec<String>) {
    let Ok(Some(turn)) = store.open_turn_of(&task.id) else { return Default::default() };
    let from: Snapshot = serde_json::from_str(&turn.snapshot).unwrap_or_default();
    let (files, commits) = changes(&from, &snapshot(workspace), workspace);
    let _ = store.close_turn(turn.id, &files, &commits, now);
    (files, commits)
}

/// The member's own conversation on this task, created the first time it's needed.
pub fn seat(store: &Store, task: &Task, agent_id: &str, now: &str) -> Result<String, String> {
    let roster = store.list_task_agents(&task.id).map_err(|e| e.to_string())?;
    let member = roster.iter().find(|m| m.agent_id == agent_id && m.removed_at.is_none());
    if let Some(session) = member.and_then(|m| m.work_session_id.clone()).filter(|s| !s.is_empty()) {
        return Ok(session);
    }
    if member.is_none() {
        store
            .add_task_agent_full(&task.id, agent_id, "collaborator", None, "", "waiting", now)
            .map_err(|e| e.to_string())?;
    }
    let workspace = task_workspace(store, task).ok_or_else(|| format!("task `{}` has no workspace to seat anyone in", task.id))?;
    let title = format!("{} ({agent_id})", task.title);
    let info = crate::session::create_blank_session_in(store, &workspace, &title, true, crate::session::WorkspaceMode::Folder)?;
    store
        .set_session_role(&info.id, crate::session::role_str(crate::session::SessionRole::Standard), Some(agent_id))
        .map_err(|e| e.to_string())?;
    // Without an inherited profile the seat falls to the global default, which may be the very quota the task was pinned away from.
    let profile = task
        .profile
        .clone()
        .or_else(|| store.agent_profile(agent_id).ok().flatten())
        .or_else(|| store.get_session_row(&task.session_id).ok().flatten().and_then(|row| row.profile));
    if let Some(profile) = profile {
        let _ = store.set_session_profile(&info.id, Some(&profile));
    }
    store.set_member_session(&task.id, agent_id, &info.id).map_err(|e| e.to_string())?;
    Ok(info.id)
}

/// The task's dispatched conversation is the holder's seat. Recorded the moment a
/// second member joins, which is when it first matters whose conversation it is.
pub fn seat_holder(store: &Store, task: &Task) -> Option<TaskAgent> {
    let holder = store
        .list_task_agents(&task.id)
        .ok()?
        .into_iter()
        .find(|m| m.status == "active" && m.removed_at.is_none())?;
    if holder.work_session_id.as_deref().is_none_or(str::is_empty) {
        store.set_member_session(&task.id, &holder.agent_id, &task.session_id).ok()?;
    }
    Some(holder)
}

pub struct Passed {
    pub session: String,
    pub files: Vec<String>,
    pub commits: Vec<String>,
}

/// Moves the turn in one step: closes the holder's record, seats the next member,
/// moves the lease and points task reporting at the new holder's conversation.
pub fn pass_turn(store: &Store, task: &Task, to: &str, handover: &Handover, now: &str) -> Result<Passed, String> {
    seat_holder(store, task);
    let session = seat(store, task, to, now)?;
    let workspace = task_workspace(store, task);
    let (files, commits) = workspace.as_deref().map(|w| close_turn(store, task, w, now)).unwrap_or_default();
    store.create_handoff(handover).map_err(|e| e.to_string())?;
    store.ack_handoff(&task.id, to, now).map_err(|e| e.to_string())?;
    let snap = workspace.as_deref().map(|w| serde_json::to_string(&snapshot(w)).unwrap_or_default()).unwrap_or_default();
    let _ = store.open_turn(&task.id, to, &session, &snap, now);
    store
        .update_task_in(&task.id, now, |t| t.reporting_session = Some(session.clone()))
        .map_err(|e| e.to_string())?;
    Ok(Passed { session, files, commits })
}

/// The handover as the room reads it. Measured changes sit beside the claim so a
/// reader can tell what the member said from what actually moved.
pub fn handover_body(handover: &Handover, passed: &Passed) -> String {
    let mut body = format!(
        "{} passed the turn to {}.\nDone: {}{}\nRisks: {}",
        handover.from_agent,
        handover.to_agent,
        handover.done,
        if handover.next.is_empty() { String::new() } else { format!("\nNext: {}", handover.next) },
        if handover.risks.is_empty() { "none stated" } else { &handover.risks },
    );
    if !handover.files.is_empty() {
        body.push_str(&format!("\nLook at: {}", handover.files.join(", ")));
    }
    body.push_str(&format!(
        "\nChanged in that turn (measured): {}",
        if passed.files.is_empty() { "nothing".to_string() } else { passed.files.join(", ") }
    ));
    if !passed.commits.is_empty() {
        body.push_str(&format!("\nCommits: {}", passed.commits.join("; ")));
    }
    body.push_str(&format!(
        "\nIt is {}'s turn. When your part is done, pass_turn to whoever goes next, or report_mission_task if the work is finished.",
        handover.to_agent
    ));
    body
}
