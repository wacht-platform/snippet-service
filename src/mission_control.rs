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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// How a dispatched task should be delivered to its target session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    /// The target session already holds the relevant context; deliver the task
    /// envelope as-is.
    #[default]
    Resume,
    /// The target session lacks the context; the full handoff description is
    /// the worker's briefing and must be self-contained.
    Fresh,
}

impl HandoffMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "resume" => Some(Self::Resume),
            "fresh" => Some(Self::Fresh),
            _ => None,
        }
    }
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
    /// How the handoff should be delivered (resume context vs fresh briefing).
    #[serde(default)]
    pub handoff_mode: HandoffMode,
    /// Session id allowed to report this task's outcome. Set at dispatch time;
    /// `report_mission_task` is only honoured for tasks bound to the caller.
    #[serde(default)]
    pub reporting_session: Option<String>,
    /// Consecutive failed dispatch attempts; reset on successful delivery.
    /// At the retry ceiling the task is parked as Blocked with a notification.
    #[serde(default)]
    pub dispatch_failures: u32,
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

/// Stable daemon session id for the single Mission Control conversation.
pub const SESSION_ID: &str = "mission-control";

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

/// Workspace the Mission Control agent runs in — the dedicated home, never a
/// project folder.
pub fn workspace_path() -> PathBuf {
    MissionControlStore::default_root(None)
}

/// The one conversation file. Open always resumes this; it is never minted
/// under a project `conversations/` directory.
pub fn session_state_path() -> PathBuf {
    workspace_path().join("session.json")
}

pub fn is_session_id(id: &str) -> bool {
    let id = id.trim();
    id == SESSION_ID || id == "mission-control/session.json"
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
    tasks_dir(root).join(format!("{}.json", safe_id(id)))
}

// ---------------------------------------------------------------------------
// Atomic I/O helpers
// ---------------------------------------------------------------------------

/// Process-wide serialisation for store mutations. All read-modify-write
/// helpers (`update_task`, `update_session`, settings updates, dispatch
/// claims) hold this lock across the whole cycle so concurrent callers — the
/// dispatch loop, REST handlers, worker tool calls — cannot lose updates.
static STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn store_lock() -> MutexGuard<'static, ()> {
    STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Monotonic counter making atomic-write temp files unique per write.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let content = serde_json::to_string_pretty(value).map_err(|e| format!("serialise: {e}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("record");
    let tmp = path.with_file_name(format!(".{file}.{pid}.{n}.tmp"));
    fs::write(&tmp, &content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", path.display()));
    }
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
/// mutates the session; the updated version is persisted atomically. Holds the
/// store lock across the read-modify-write cycle so concurrent callers cannot
/// lose updates.
pub fn update_session(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut ManagedSession),
) -> Result<ManagedSession, String> {
    let _guard = store_lock();
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

/// Create a new task record and persist it. `handoff_mode` is stored in the
/// same write so a dispatcher can never claim the task before its delivery
/// semantics are persisted.
pub fn create_task(
    root: &Path,
    id: &str,
    session_id: &str,
    title: &str,
    description: &str,
    owned_paths: Vec<PathBuf>,
) -> Result<TaskRecord, String> {
    create_task_with_mode(
        root,
        id,
        session_id,
        title,
        description,
        owned_paths,
        HandoffMode::default(),
    )
}

/// Like [`create_task`] with an explicit handoff delivery mode.
#[allow(clippy::too_many_arguments)]
pub fn create_task_with_mode(
    root: &Path,
    id: &str,
    session_id: &str,
    title: &str,
    description: &str,
    owned_paths: Vec<PathBuf>,
    handoff_mode: HandoffMode,
) -> Result<TaskRecord, String> {
    let _guard = store_lock();
    let now = epoch_secs();
    let task = TaskRecord {
        id: id.to_string(),
        session_id: session_id.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        status: TaskStatus::Pending,
        dependencies: Vec::new(),
        handoff: None,
        handoff_mode,
        reporting_session: None,
        dispatch_failures: 0,
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

/// Update a task record in-place (closure mutates, then persists). Holds the
/// store lock across the read-modify-write cycle so concurrent callers — the
/// dispatch loop, REST handlers, worker tool calls — cannot lose updates.
pub fn update_task(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut TaskRecord),
) -> Result<TaskRecord, String> {
    let _guard = store_lock();
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

/// Move a task to a terminal status and set its result. Returns an error
/// instead of panicking when handed a non-terminal status, and refuses to
/// overwrite an already-terminal task (a second completion must not flip a
/// `Done` task to `Failed` or replace its result).
pub fn complete_task(
    root: &Path,
    id: &str,
    status: TaskStatus,
    result: TaskResult,
) -> Result<TaskRecord, String> {
    if !status.is_terminal() {
        return Err(format!(
            "complete_task requires a terminal status, got {status:?}"
        ));
    }
    {
        let _guard = store_lock();
        let existing: TaskRecord = read_json(&task_path(root, id))?;
        if existing.status.is_terminal() {
            return Err(format!(
                "task {id} is already terminal ({:?}); refusing to overwrite its result",
                existing.status
            ));
        }
    }
    let kind = match status {
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        _ => "info",
    };
    let message = result.summary.clone();
    update_task(root, id, |t| {
        t.status = status;
        t.result = Some(result);
        t.owned_paths.clear();
        t.reporting_session = None;
        t.notifications.push(NotificationMarker {
            target: "mission_control".into(),
            kind: kind.into(),
            message,
            delivered: false,
        });
    })
}

/// Atomically claim a Pending task for dispatch: flips `Pending → InProgress`
/// exactly once. Concurrent dispatchers (loop + REST paths) race through this
/// compare-and-swap; only one caller wins and delivers the envelope.
pub fn claim_task_for_dispatch(root: &Path, id: &str) -> Result<TaskRecord, String> {
    let _guard = store_lock();
    let mut task: TaskRecord = read_json(&task_path(root, id))?;
    if task.status != TaskStatus::Pending {
        return Ok(task);
    }
    task.status = TaskStatus::InProgress;
    task.reporting_session = Some(task.session_id.clone());
    task.updated_at = epoch_secs();
    write_json(&task_path(root, id), &task)?;
    Ok(task)
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
///
/// Deliberately does NOT hold `store_lock()` for the whole pass: it calls
/// locking helpers (`archive_session` → `update_session`) and a non-reentrant
/// guard would self-deadlock. Task transitions go through `update_task` so each
/// read-modify-write cycle is individually serialized — a concurrent
/// notification or report can never be discarded by the archive write.
pub fn archive_all(root: &Path) -> Result<u32, String> {
    let sessions = list_sessions(root, true)?;
    let mut count = 0u32;
    for s in &sessions {
        archive_session(root, &s.id)?;
        // Also terminalise all non-terminal tasks for this session.
        let tasks = list_tasks(root, Some(&s.id), None)?;
        for t in tasks {
            if !t.status.is_terminal() {
                update_task(root, &t.id, |task| {
                    task.status = TaskStatus::Cancelled;
                    task.owned_paths.clear();
                    task.reporting_session = None;
                })?;
            }
        }
        count += 1;
    }
    Ok(count)
}

/// Re-queue a non-terminal or failed task so dispatch can deliver it again.
/// Use after a temporary failure (rate limit, dispatch ceiling, worker
/// `blocked`). Refuses Done/Cancelled — those are finished, not paused.
pub fn retry_task(root: &Path, id: &str) -> Result<TaskRecord, String> {
    update_task(root, id, |t| {
        if matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled) {
            return;
        }
        t.status = TaskStatus::Pending;
        t.dispatch_failures = 0;
        t.reporting_session = None;
        t.owned_paths.clear();
        t.notifications.push(NotificationMarker {
            target: "mission_control".into(),
            kind: "info".into(),
            message: "re-queued after temporary failure".into(),
            delivered: true,
        });
    })
    .and_then(|t| {
        if matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled) {
            Err(format!(
                "task {id} is {:?}; only blocked, failed, or in-progress work can be retried",
                t.status
            ))
        } else {
            Ok(t)
        }
    })
}

/// Return every Blocked task whose dependencies are all Done back to Pending
/// so the dispatch loop can pick them up. Called after any task completes;
/// cheap no-op when nothing is eligible. Returns the ids re-queued.
///
/// Tasks blocked by the dispatch retry ceiling (`dispatch_failures`) are NOT
/// re-queued here — a dependency-less blocked task is retry-exhausted, and the
/// user must explicitly reset it (e.g. via REST `status: "pending"` or
/// `retry_mission_task`).
pub fn unblock_ready_tasks(root: &Path) -> Result<Vec<String>, String> {
    let mut unblocked = Vec::new();
    let ids = {
        let _guard = store_lock();
        list_ids_in_dir(&tasks_dir(root))
            .into_iter()
            .filter_map(|id| {
                read_json::<TaskRecord>(&task_path(root, &id))
                    .ok()
                    .map(|t| (id, t))
            })
            .filter(|(_, t)| {
                t.status == TaskStatus::Blocked
                    && t.dispatch_failures == 0
                    && !t.dependencies.is_empty()
            })
            .collect::<Vec<_>>()
    };
    for (id, task) in ids {
        let ready = task.dependencies.iter().all(|dep| {
            read_json::<TaskRecord>(&task_path(root, &dep.task_id))
                .map(|other| other.status == TaskStatus::Done)
                .unwrap_or(false)
        });
        if !ready {
            continue;
        }
        // Transition through update_task so the requeue is serialized against
        // any concurrent mutation of the same record.
        if let Ok(updated) = update_task(root, &id, |t| {
            if t.status == TaskStatus::Blocked {
                t.status = TaskStatus::Pending;
            }
        }) {
            if updated.status == TaskStatus::Pending {
                unblocked.push(id);
            }
        }
    }
    Ok(unblocked)
}

// ---------------------------------------------------------------------------
// Store handle (thin; mutations serialise on the process-wide lock)
// ---------------------------------------------------------------------------

/// A thin wrapper providing interior-mutability-safe access to a store rooted
/// at a specific path.  Only serialises writes; reads are plain filesystem ops.
#[derive(Debug, Clone)]
pub struct MissionControlStore {
    root: PathBuf,
}

impl MissionControlStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Convenience: data-root from env with optional override.
    pub fn default_root(override_path: Option<&Path>) -> PathBuf {
        data_root(override_path)
    }
}

/// Ensure the dedicated Mission Control home exists (workspace + store).
pub fn ensure_home() -> Result<PathBuf, String> {
    let root = workspace_path();
    std::fs::create_dir_all(&root).map_err(|e| format!("create mission-control home: {e}"))?;
    Ok(root)
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
            handoff_mode: HandoffMode::Resume,
            reporting_session: Some("s1".into()),
            dispatch_failures: 0,
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
    fn dedicated_home_is_one_session() {
        assert_eq!(SESSION_ID, "mission-control");
        assert!(is_session_id(SESSION_ID));
        assert!(is_session_id("mission-control/session.json"));
        assert!(!is_session_id("gmata-backend-abc/conversations/x.json"));
        assert!(!is_session_id("foo/state.json"));
        let home = workspace_path();
        assert_eq!(session_state_path(), home.join("session.json"));
        assert!(home.ends_with("mission-control"));
    }

    #[test]
    fn store_creates_and_reads_via_lock() {
        let root = tmp();
        let store = MissionControlStore::new(root.path().to_path_buf());

        {
            let _guard = store_lock();
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

    #[test]
    fn task_paths_reject_traversal_ids() {
        let root = tmp();
        for evil in ["../../etc/passwd", "..\\..\\x", "a/../settings"] {
            let p = task_path(root.path(), evil);
            assert!(
                p.starts_with(tasks_dir(root.path())),
                "{evil} escaped the store"
            );
        }
        // Session ids keep their existing escaping (slash ids roundtrip).
        create_session(root.path(), "ws/a", "Nested", Path::new("/w")).unwrap();
        assert!(get_session(root.path(), "ws/a").is_ok());
    }

    #[test]
    fn claim_is_one_shot_and_binds_reporter() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();

        let claimed = claim_task_for_dispatch(root.path(), "t1").unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        assert_eq!(claimed.reporting_session.as_deref(), Some("s1"));

        // A second dispatcher must not re-claim or reset the binding.
        let again = claim_task_for_dispatch(root.path(), "t1").unwrap();
        assert_eq!(again.status, TaskStatus::InProgress);
        assert_eq!(again.reporting_session.as_deref(), Some("s1"));
    }

    #[test]
    fn blocked_tasks_resume_when_dependencies_complete() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "dep", "s1", "Dep", "D", vec![]).unwrap();
        create_task(root.path(), "waiter", "s1", "W", "D", vec![]).unwrap();
        add_dependency(root.path(), "waiter", "dep", None).unwrap();
        update_task(root.path(), "waiter", |t| t.status = TaskStatus::Blocked).unwrap();

        // Dependency not done yet → stays blocked.
        assert!(unblock_ready_tasks(root.path()).unwrap().is_empty());
        assert_eq!(
            get_task(root.path(), "waiter").unwrap().status,
            TaskStatus::Blocked
        );

        complete_task(root.path(), "dep", TaskStatus::Done, TaskResult::default()).unwrap();
        let unblocked = unblock_ready_tasks(root.path()).unwrap();
        assert_eq!(unblocked, vec!["waiter".to_string()]);
        assert_eq!(
            get_task(root.path(), "waiter").unwrap().status,
            TaskStatus::Pending
        );
    }

    #[test]
    fn worker_blocked_tasks_without_deps_stay_blocked() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();
        update_task(root.path(), "t1", |t| t.status = TaskStatus::Blocked).unwrap();
        assert!(unblock_ready_tasks(root.path()).unwrap().is_empty());
        assert_eq!(
            get_task(root.path(), "t1").unwrap().status,
            TaskStatus::Blocked
        );
    }

    #[test]
    fn retry_task_requeues_blocked_and_failed() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();
        update_task(root.path(), "t1", |t| {
            t.status = TaskStatus::Blocked;
            t.dispatch_failures = 5;
            t.reporting_session = Some("s1".into());
        })
        .unwrap();
        let retried = retry_task(root.path(), "t1").unwrap();
        assert_eq!(retried.status, TaskStatus::Pending);
        assert_eq!(retried.dispatch_failures, 0);
        assert!(retried.reporting_session.is_none());

        update_task(root.path(), "t1", |t| t.status = TaskStatus::Failed).unwrap();
        assert_eq!(
            retry_task(root.path(), "t1").unwrap().status,
            TaskStatus::Pending
        );

        complete_task(
            root.path(),
            "t1",
            TaskStatus::Done,
            TaskResult::default(),
        )
        .unwrap();
        assert!(retry_task(root.path(), "t1").is_err());
    }

    #[test]
    fn complete_task_rejects_non_terminal_status() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![PathBuf::from("/w")]).unwrap();
        let err = complete_task(
            root.path(),
            "t1",
            TaskStatus::InProgress,
            TaskResult::default(),
        );
        assert!(err.is_err());
    }

    #[test]
    fn handoff_modes_roundtrip() {
        let root = tmp();
        create_session(root.path(), "s1", "S", Path::new("/w")).unwrap();
        create_task(root.path(), "t1", "s1", "T", "D", vec![]).unwrap();
        update_task(root.path(), "t1", |t| {
            t.handoff_mode = HandoffMode::Fresh;
        })
        .unwrap();
        assert_eq!(
            get_task(root.path(), "t1").unwrap().handoff_mode,
            HandoffMode::Fresh
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSettings {
    #[serde(default)]
    pub mission_control_session_id: Option<String>,
    #[serde(default = "default_notification_policy")]
    pub notification_policy: String,
}

fn default_notification_policy() -> String {
    "mission_control_only".to_string()
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            mission_control_session_id: None,
            notification_policy: default_notification_policy(),
        }
    }
}

fn settings_path(root: &Path) -> PathBuf {
    root.join("settings.json")
}

pub fn load_settings(root: &Path) -> ControlSettings {
    read_json(&settings_path(root)).unwrap_or_default()
}

pub fn save_settings(root: &Path, settings: &ControlSettings) -> Result<(), String> {
    write_json(&settings_path(root), settings)
}

pub fn set_mission_control_session(
    root: &Path,
    session_id: &str,
) -> Result<ControlSettings, String> {
    let mut settings = load_settings(root);
    settings.mission_control_session_id = Some(session_id.to_string());
    save_settings(root, &settings)?;
    Ok(settings)
}

pub fn set_notification_policy(root: &Path, policy: &str) -> Result<ControlSettings, String> {
    if !matches!(policy, "mission_control_only" | "all_sessions" | "none") {
        return Err(
            "notification policy must be mission_control_only, all_sessions, or none".to_string(),
        );
    }
    let mut settings = load_settings(root);
    settings.notification_policy = policy.to_string();
    save_settings(root, &settings)?;
    Ok(settings)
}
