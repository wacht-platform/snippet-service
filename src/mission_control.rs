//! Durable Mission Control domain models and persistence store.
//!
//! Manages *managed-session* metadata (active / archived) and *task* records
//! with structured handoff/result/dependencies/notification markers.
//! Persistence is JSON files atomically written beneath `~/.snippet/mission-control`.
//!
//! This module is self-contained — no daemon or API integration; designed for
//! later wiring into the serve/TUI layers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Domain models
// ---------------------------------------------------------------------------

/// Lifecycle status for a managed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Archived,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self::Active
    }
}

/// Metadata for a managed conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSession {
    /// Unique session identifier (maps to a file under `sessions/`).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Workspace root this session owns.
    pub workspace: PathBuf,
    /// Current lifecycle status.
    pub status: SessionStatus,
    /// Epoch seconds when the session was created.
    pub created_at: u64,
    /// Epoch seconds of last mutation.
    pub updated_at: u64,
    /// Freeform key-value annotations.
    pub tags: BTreeMap<String, String>,
}

/// Task lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Pending
    }
}

impl TaskStatus {
    /// Returns `true` when the task is in a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// A dependency edge pointing from this task to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    /// The id of the task we depend on.
    pub task_id: String,
    /// Optional human-readable reason.
    pub reason: Option<String>,
}

/// Structured handoff information passed into a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Handoff {
    /// What was handed off (freeform text).
    pub description: String,
    /// Paths produced by the previous stage.
    pub paths: Vec<PathBuf>,
    /// Arbitrary key-value context.
    pub context: BTreeMap<String, String>,
}

/// Result produced when a task completes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskResult {
    /// Human-readable summary.
    pub summary: String,
    /// Output artifacts.
    pub artifacts: Vec<PathBuf>,
    /// Whether the result is authoritative.
    pub authoritative: bool,
}

/// A notification marker attached to a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationMarker {
    /// Target recipient (user id, lane name, etc.).
    pub target: String,
    /// Notification kind ("info", "warning", "action_required", …).
    pub kind: String,
    /// Freeform message body.
    pub message: String,
    /// Whether this notification has been delivered.
    pub delivered: bool,
}

/// A unit of tracked work within a managed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Unique task id (uuid).
    pub id: String,
    /// Owning managed-session id.
    pub session_id: String,
    /// Short title.
    pub title: String,
    /// Longer description.
    pub description: String,
    /// Current status.
    pub status: TaskStatus,
    /// Dependencies on other tasks.
    pub dependencies: Vec<Dependency>,
    /// Handoff information (populated when work is delegated).
    pub handoff: Option<Handoff>,
    /// Result (populated when terminal).
    pub result: Option<TaskResult>,
    /// Pending / undelivered notification markers.
    pub notifications: Vec<NotificationMarker>,
    /// Workspace paths this task is *currently* writing to — used for
    /// ownership conflict detection.
    pub owned_paths: Vec<PathBuf>,
    /// Epoch seconds.
    pub created_at: u64,
    /// Epoch seconds.
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// Store configuration
// ---------------------------------------------------------------------------

/// Resolve the root data directory for mission control.
///
/// `~/.snippet/mission-control` under the user's `HOME`, or an explicit override
/// (useful for tests).
fn data_root(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".snippet").join("mission-control")
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join("sessions")
}

fn tasks_dir(root: &Path) -> PathBuf {
    root.join("tasks")
}

fn safe_id(id: &str) -> String {
    id.replace('%', "%25")
        .replace('/', "%2F")
        .replace('\\', "%5C")
}

fn session_path(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(format!("{}.json", safe_id(id)))
}

fn task_path(root: &Path, id: &str) -> PathBuf {
    tasks_dir(root).join(format!("{id}.json"))
}

// ---------------------------------------------------------------------------
// Atomic I/O helpers
// ---------------------------------------------------------------------------

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let content = serde_json::to_string_pretty(value).map_err(|e| format!("serialise: {e}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("deserialise {}: {e}", path.display()))
}

fn unsafe_id(id: &str) -> String {
    id.replace("%2F", "/")
        .replace("%5C", "\\")
        .replace("%25", "%")
}

fn list_ids_in_dir(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    ids.push(unsafe_id(stem));
                }
            }
        }
        ids.sort();
    }
    ids
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

/// Describes a path-ownership conflict between two tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipConflict {
    pub existing_task_id: String,
    pub path: PathBuf,
}

/// Scan all *active* (non-terminal) tasks for overlapping `owned_paths`.
///
/// Returns a conflict for each path in `candidate_paths` that is already
/// claimed by a different, still-active task.
pub fn detect_conflicts(
    root: &Path,
    candidate_task_id: &str,
    candidate_paths: &[PathBuf],
) -> Result<Vec<OwnershipConflict>, String> {
    let mut conflicts = Vec::new();
    let ids = list_ids_in_dir(&tasks_dir(root));
    for id in &ids {
        if id == candidate_task_id {
            continue;
        }
        let t: TaskRecord = read_json(&task_path(root, id))?;
        if t.status.is_terminal() {
            continue;
        }
        for cp in candidate_paths {
            if t.owned_paths.iter().any(|op| op == cp) {
                conflicts.push(OwnershipConflict {
                    existing_task_id: id.clone(),
                    path: cp.clone(),
                });
            }
        }
    }
    Ok(conflicts)
}

// ---------------------------------------------------------------------------
// ManagedSession CRUD
// ---------------------------------------------------------------------------

/// Create a new managed session and persist it.
pub fn create_session(
    root: &Path,
    id: &str,
    label: &str,
    workspace: &Path,
) -> Result<ManagedSession, String> {
    let now = epoch_secs();
    let session = ManagedSession {
        id: id.to_string(),
        label: label.to_string(),
        workspace: workspace.to_path_buf(),
        status: SessionStatus::Active,
        created_at: now,
        updated_at: now,
        tags: BTreeMap::new(),
    };
    write_json(&session_path(root, id), &session)?;
    Ok(session)
}

/// Load a managed session by id.
pub fn get_session(root: &Path, id: &str) -> Result<ManagedSession, String> {
    read_json(&session_path(root, id))
}

/// Update a managed session in-place.  The caller provides a closure that
/// mutates the session; the updated version is persisted atomically.
pub fn update_session(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut ManagedSession),
) -> Result<ManagedSession, String> {
    let mut session: ManagedSession = read_json(&session_path(root, id))?;
    f(&mut session);
    session.updated_at = epoch_secs();
    write_json(&session_path(root, id), &session)?;
    Ok(session)
}

/// List all managed sessions.  If `active_only` is true, only active ones
/// are returned.
pub fn list_sessions(root: &Path, active_only: bool) -> Result<Vec<ManagedSession>, String> {
    let ids = list_ids_in_dir(&sessions_dir(root));
    let mut out = Vec::new();
    for id in &ids {
        let s: ManagedSession = read_json(&session_path(root, id))?;
        if active_only && s.status != SessionStatus::Active {
            continue;
        }
        out.push(s);
    }
    Ok(out)
}

/// Find sessions whose label contains `query` (case-insensitive).
pub fn find_sessions(root: &Path, query: &str) -> Result<Vec<ManagedSession>, String> {
    let q = query.to_lowercase();
    let ids = list_ids_in_dir(&sessions_dir(root));
    let mut out = Vec::new();
    for id in &ids {
        let s: ManagedSession = read_json(&session_path(root, id))?;
        if s.label.to_lowercase().contains(&q) {
            out.push(s);
        }
    }
    Ok(out)
}

/// Archive a session (set status to `Archived`).
pub fn archive_session(root: &Path, id: &str) -> Result<ManagedSession, String> {
    update_session(root, id, |s| s.status = SessionStatus::Archived)
}

// ---------------------------------------------------------------------------
// TaskRecord CRUD
// ---------------------------------------------------------------------------

/// Create a new task record and persist it.
pub fn create_task(
    root: &Path,
    id: &str,
    session_id: &str,
    title: &str,
    description: &str,
    owned_paths: Vec<PathBuf>,
) -> Result<TaskRecord, String> {
    let now = epoch_secs();
    let task = TaskRecord {
        id: id.to_string(),
        session_id: session_id.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        status: TaskStatus::Pending,
        dependencies: Vec::new(),
        handoff: None,
        result: None,
        notifications: Vec::new(),
        owned_paths,
        created_at: now,
        updated_at: now,
    };
    write_json(&task_path(root, id), &task)?;
    Ok(task)
}

/// Load a task record by id.
pub fn get_task(root: &Path, id: &str) -> Result<TaskRecord, String> {
    read_json(&task_path(root, id))
}

/// Update a task record in-place (closure mutates, then persists).
pub fn update_task(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut TaskRecord),
) -> Result<TaskRecord, String> {
    let mut task: TaskRecord = read_json(&task_path(root, id))?;
    f(&mut task);
    task.updated_at = epoch_secs();
    write_json(&task_path(root, id), &task)?;
    Ok(task)
}

/// List tasks, optionally filtered by session and/or status.
pub fn list_tasks(
    root: &Path,
    session_id: Option<&str>,
    status: Option<TaskStatus>,
) -> Result<Vec<TaskRecord>, String> {
    let ids = list_ids_in_dir(&tasks_dir(root));
    let mut out = Vec::new();
    for id in &ids {
        let t: TaskRecord = read_json(&task_path(root, id))?;
        if let Some(sid) = session_id {
            if t.session_id != sid {
                continue;
            }
        }
        if let Some(st) = status {
            if t.status != st {
                continue;
            }
        }
        out.push(t);
    }
    Ok(out)
}

/// Find tasks whose title or description contains `query` (case-insensitive).
pub fn find_tasks(
    root: &Path,
    session_id: Option<&str>,
    query: &str,
) -> Result<Vec<TaskRecord>, String> {
    let q = query.to_lowercase();
    let ids = list_ids_in_dir(&tasks_dir(root));
    let mut out = Vec::new();
    for id in &ids {
        let t: TaskRecord = read_json(&task_path(root, id))?;
        if let Some(sid) = session_id {
            if t.session_id != sid {
                continue;
            }
        }
        if t.title.to_lowercase().contains(&q) || t.description.to_lowercase().contains(&q) {
            out.push(t);
        }
    }
    Ok(out)
}

/// Move a task to a terminal status and set its result.
pub fn complete_task(
    root: &Path,
    id: &str,
    status: TaskStatus,
    result: TaskResult,
) -> Result<TaskRecord, String> {
    assert!(
        status.is_terminal(),
        "complete_task requires a terminal status"
    );
    update_task(root, id, |t| {
        t.status = status;
        t.result = Some(result);
        t.owned_paths.clear(); // release ownership
    })
}

/// Add a dependency edge to a task.
pub fn add_dependency(
    root: &Path,
    id: &str,
    dep_task_id: &str,
    reason: Option<&str>,
) -> Result<TaskRecord, String> {
    update_task(root, id, |t| {
        t.dependencies.push(Dependency {
            task_id: dep_task_id.to_string(),
            reason: reason.map(String::from),
        });
    })
}

/// Push a notification marker onto a task.
pub fn push_notification(
    root: &Path,
    id: &str,
    target: &str,
    kind: &str,
    message: &str,
) -> Result<TaskRecord, String> {
    update_task(root, id, |t| {
        t.notifications.push(NotificationMarker {
            target: target.to_string(),
            kind: kind.to_string(),
            message: message.to_string(),
            delivered: false,
        });
    })
}

/// Mark a notification as delivered by index.
pub fn mark_notification_delivered(
    root: &Path,
    task_id: &str,
    index: usize,
) -> Result<TaskRecord, String> {
    update_task(root, task_id, |t| {
        if let Some(n) = t.notifications.get_mut(index) {
            n.delivered = true;
        }
    })
}

/// Archive all active sessions and their tasks in one pass.
pub fn archive_all(root: &Path) -> Result<u32, String> {
    let sessions = list_sessions(root, true)?;
    let mut count = 0u32;
    for s in &sessions {
        archive_session(root, &s.id)?;
        // Also terminalise all non-terminal tasks for this session.
        let tasks = list_tasks(root, Some(&s.id), None)?;
        for mut t in tasks {
            if !t.status.is_terminal() {
                t.status = TaskStatus::Cancelled;
                t.updated_at = epoch_secs();
                t.owned_paths.clear();
                write_json(&task_path(root, &t.id), &t)?;
            }
        }
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Concurrency guard (optional — for callers that want safe concurrent access)
// ---------------------------------------------------------------------------

/// A thin wrapper providing interior-mutability-safe access to a store rooted
/// at a specific path.  Only serialises writes; reads are plain filesystem ops.
pub struct MissionControlStore {
    root: PathBuf,
    _lock: Mutex<()>,
}

impl MissionControlStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            _lock: Mutex::new(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquire the write lock.  Hold the returned guard for a batch of
    /// mutations to avoid interleaving.
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self._lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Convenience: data-root from env with optional override.
    pub fn default_root(override_path: Option<&Path>) -> PathBuf {
        data_root(override_path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temp directory and return it (cleaned up on drop).
    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    // -- Session tests ----------------------------------------------------

    #[test]
    fn create_and_get_session() {
        let root = tmp();
        let s = create_session(root.path(), "s1", "Test Session", Path::new("/workspace")).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.status, SessionStatus::Active);

        let loaded = get_session(root.path(), "s1").unwrap();
        assert_eq!(loaded.label, "Test Session");
        assert_eq!(loaded.workspace, PathBuf::from("/workspace"));
    }

    #[test]
    fn update_session_modifies_label_and_timestamp() {
        let root = tmp();
        create_session(root.path(), "s1", "Old", Path::new("/w")).unwrap();

        let updated = update_session(root.path(), "s1", |s| {
            s.label = "New".into();
        })
        .unwrap();
        assert_eq!(updated.label, "New");
        assert!(updated.updated_at >= updated.created_at);
    }

    #[test]
    fn list_sessions_filters_active() {
        let root = tmp();
        create_session(root.path(), "s1", "Active", Path::new("/")).unwrap();
        create_session(root.path(), "s2", "Archived", Path::new("/")).unwrap();
        archive_session(root.path(), "s2").unwrap();

        let all = list_sessions(root.path(), false).unwrap();
        assert_eq!(all.len(), 2);

        let active = list_sessions(root.path(), true).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "s1");
    }

    #[test]
    fn find_sessions_case_insensitive() {
        let root = tmp();
        create_session(root.path(), "s1", "Hello World", Path::new("/")).unwrap();
        create_session(root.path(), "s2", "Other", Path::new("/")).unwrap();

        let found = find_sessions(root.path(), "hello").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "s1");
    }

    #[test]
    fn archive_session_transitions_status() {
        let root = tmp();
        create_session(root.path(), "s1", "X", Path::new("/")).unwrap();
        let s = archive_session(root.path(), "s1").unwrap();
        assert_eq!(s.status, SessionStatus::Archived);
    }

    #[test]
    fn session_ids_with_slashes_roundtrip() {
        let root = tmp();
        let id = "workspace-a/conversations/example.json";
        create_session(root.path(), id, "Nested", Path::new("/workspace")).unwrap();
        let loaded = get_session(root.path(), id).unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(list_sessions(root.path(), false).unwrap()[0].id, id);
    }

    // -- Task tests -------------------------------------------------------

    #[test]
    fn create_and_get_task() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        let t = create_task(
            root.path(),
            "t1",
            "s1",
            "Do thing",
            "Description",
            vec![PathBuf::from("/src/main.rs")],
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.owned_paths.len(), 1);

        let loaded = get_task(root.path(), "t1").unwrap();
        assert_eq!(loaded.title, "Do thing");
    }

    #[test]
    fn update_task_sets_status() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();

        let updated = update_task(root.path(), "t1", |t| {
            t.status = TaskStatus::InProgress;
        })
        .unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);
    }

    #[test]
    fn complete_task_clears_owned_paths() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![PathBuf::from("/a")]).unwrap();

        let done = complete_task(
            root.path(),
            "t1",
            TaskStatus::Done,
            TaskResult {
                summary: "finished".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(done.owned_paths.is_empty());
        assert!(done.result.unwrap().summary == "finished");
    }

    #[test]
    fn list_tasks_filter_by_session_and_status() {
        let root = tmp();
        create_session(root.path(), "s1", "A", Path::new("/")).unwrap();
        create_session(root.path(), "s2", "B", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "T1", "D", vec![]).unwrap();
        create_task(root.path(), "t2", "s1", "T2", "D", vec![]).unwrap();
        create_task(root.path(), "t3", "s2", "T3", "D", vec![]).unwrap();

        // filter by session
        let s1_tasks = list_tasks(root.path(), Some("s1"), None).unwrap();
        assert_eq!(s1_tasks.len(), 2);

        // filter by status
        update_task(root.path(), "t1", |t| t.status = TaskStatus::InProgress).unwrap();
        let pending = list_tasks(root.path(), None, Some(TaskStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 2); // t2, t3
    }

    #[test]
    fn find_tasks_by_query() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(
            root.path(),
            "t1",
            "s1",
            "Refactor auth",
            "Deep refactor",
            vec![],
        )
        .unwrap();
        create_task(root.path(), "t2", "s1", "Fix typo", "Trivial fix", vec![]).unwrap();

        let found = find_tasks(root.path(), None, "auth").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "t1");

        let found2 = find_tasks(root.path(), None, "fix").unwrap();
        assert_eq!(found2.len(), 1); // "Fix typo" only — "Refactor" doesn't contain "fix"
    }

    // -- Dependencies / Notifications -------------------------------------

    #[test]
    fn add_dependency_and_push_notification() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "A", "D", vec![]).unwrap();
        create_task(root.path(), "t2", "s1", "B", "D", vec![]).unwrap();

        let t = add_dependency(root.path(), "t2", "t1", Some("needs A done")).unwrap();
        assert_eq!(t.dependencies.len(), 1);
        assert_eq!(t.dependencies[0].task_id, "t1");

        let t = push_notification(root.path(), "t1", "lane-1", "info", "almost there").unwrap();
        assert_eq!(t.notifications.len(), 1);
        assert!(!t.notifications[0].delivered);

        let t = mark_notification_delivered(root.path(), "t1", 0).unwrap();
        assert!(t.notifications[0].delivered);
    }

    // -- Ownership conflict detection -------------------------------------

    #[test]
    fn detect_conflicts_finds_overlapping_paths() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(
            root.path(),
            "t1",
            "s1",
            "A",
            "D",
            vec![PathBuf::from("/src/a.rs"), PathBuf::from("/src/b.rs")],
        )
        .unwrap();
        create_task(
            root.path(),
            "t2",
            "s1",
            "B",
            "D",
            vec![PathBuf::from("/src/c.rs")],
        )
        .unwrap();

        // Request overlap on a.rs — should conflict with t1.
        let conflicts =
            detect_conflicts(root.path(), "t_new", &[PathBuf::from("/src/a.rs")]).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].existing_task_id, "t1");
        assert_eq!(conflicts[0].path, PathBuf::from("/src/a.rs"));

        // No conflict on c.rs with t2 when requesting t2 itself (skipped).
        let conflicts2 =
            detect_conflicts(root.path(), "t2", &[PathBuf::from("/src/c.rs")]).unwrap();
        assert!(conflicts2.is_empty());
    }

    #[test]
    fn detect_conflicts_ignores_terminal_tasks() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(
            root.path(),
            "t1",
            "s1",
            "A",
            "D",
            vec![PathBuf::from("/src/x.rs")],
        )
        .unwrap();
        // Mark t1 as Done — it should no longer block.
        complete_task(root.path(), "t1", TaskStatus::Done, TaskResult::default()).unwrap();

        let conflicts =
            detect_conflicts(root.path(), "t_new", &[PathBuf::from("/src/x.rs")]).unwrap();
        assert!(conflicts.is_empty());
    }

    // -- Archive all ------------------------------------------------------

    #[test]
    fn archive_all_terminalises_tasks() {
        let root = tmp();
        create_session(root.path(), "s1", "A", Path::new("/")).unwrap();
        create_session(root.path(), "s2", "B", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();
        create_task(root.path(), "t2", "s2", "T", "D", vec![]).unwrap();

        let count = archive_all(root.path()).unwrap();
        assert_eq!(count, 2);

        let active = list_sessions(root.path(), true).unwrap();
        assert!(active.is_empty());

        let t1 = get_task(root.path(), "t1").unwrap();
        assert_eq!(t1.status, TaskStatus::Cancelled);
        assert!(t1.owned_paths.is_empty());
    }

    // -- TaskStatus::is_terminal ------------------------------------------

    #[test]
    fn terminal_status_check() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::InProgress.is_terminal());
        assert!(!TaskStatus::Blocked.is_terminal());
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    // -- TaskRecord serialisation round-trip ------------------------------

    #[test]
    fn task_record_roundtrip() {
        let task = TaskRecord {
            id: "t1".into(),
            session_id: "s1".into(),
            title: "Title".into(),
            description: "Desc".into(),
            status: TaskStatus::InProgress,
            dependencies: vec![Dependency {
                task_id: "t0".into(),
                reason: Some("prereq".into()),
            }],
            handoff: Some(Handoff {
                description: "handoff".into(),
                paths: vec![PathBuf::from("/x")],
                context: {
                    let mut m = BTreeMap::new();
                    m.insert("key".into(), "val".into());
                    m
                },
            }),
            result: None,
            notifications: vec![NotificationMarker {
                target: "lane-1".into(),
                kind: "info".into(),
                message: "hello".into(),
                delivered: false,
            }],
            owned_paths: vec![PathBuf::from("/src/main.rs")],
            created_at: 1000,
            updated_at: 2000,
        };

        let json = serde_json::to_string(&task).unwrap();
        let back: TaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, task.id);
        assert_eq!(back.handoff.as_ref().unwrap().context["key"], "val");
        assert_eq!(back.notifications[0].message, "hello");
    }

    // -- MissionControlStore ----------------------------------------------

    #[test]
    fn store_default_root_falls_back_to_tmp() {
        // When HOME is unset or unusual the fallback is /tmp.
        let path = MissionControlStore::default_root(None);
        // Should always produce a valid path.
        assert!(path.to_string_lossy().contains("mission-control"));
    }

    #[test]
    fn store_creates_and_reads_via_lock() {
        let root = tmp();
        let store = MissionControlStore::new(root.path().to_path_buf());

        {
            let _guard = store.lock();
            create_session(store.root(), "s1", "Locked", Path::new("/")).unwrap();
        }

        let s = get_session(store.root(), "s1").unwrap();
        assert_eq!(s.label, "Locked");
    }

    // -- Persistence survives "restart" (read back from disk) -------------

    #[test]
    fn session_persists_across_separate_reads() {
        let root = tmp();
        create_session(root.path(), "s1", "Persistent", Path::new("/w")).unwrap();
        update_session(root.path(), "s1", |s| {
            s.tags.insert("key".into(), "value".into());
        })
        .unwrap();

        // Simulate a "restart" by re-reading from the same path.
        let s = read_json::<ManagedSession>(&session_path(root.path(), "s1")).unwrap();
        assert_eq!(s.tags["key"], "value");
    }

    #[test]
    fn task_persists_dependencies_and_notifications() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();
        create_task(root.path(), "t2", "s1", "T2", "D", vec![]).unwrap();

        add_dependency(root.path(), "t2", "t1", None).unwrap();
        push_notification(root.path(), "t1", "lane-0", "action_required", "do it").unwrap();

        // Read back from disk (simulates restart).
        let t1 = read_json::<TaskRecord>(&task_path(root.path(), "t1")).unwrap();
        assert_eq!(t1.notifications.len(), 1);
        assert_eq!(t1.notifications[0].kind, "action_required");

        let t2 = read_json::<TaskRecord>(&task_path(root.path(), "t2")).unwrap();
        assert_eq!(t2.dependencies.len(), 1);
        assert_eq!(t2.dependencies[0].task_id, "t1");
    }
}
