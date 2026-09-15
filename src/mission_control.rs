//! Durable Mission Control domain models and persistence store.
//!
//! Manages *managed-session* metadata (active / archived) and *task* records
//! with structured handoff/result/dependencies/notification markers.
//! Persistence is JSON files atomically written beneath `~/.snippet/mission-control`.
//!
//! This module is self-contained — no daemon or API integration; designed for
//! later wiring into the serve/TUI layers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::Store;

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
    crate::config::snippet_home().join("mission-control")
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

// ManagedSession CRUD
// ---------------------------------------------------------------------------

/// The store these rows live in. One device database for the real root; a
/// test root gets its own file so tests stay isolated.
fn app_store(root: &Path) -> Result<Store, String> {
    Store::open_cached(crate::app_store::app_db_path(root)).map_err(|e| e.to_string())
}

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
    app_store(root)?
        .upsert_managed_session(&session)
        .map_err(|e| e.to_string())?;
    Ok(session)
}

/// Load a managed session by id.
pub fn get_session(root: &Path, id: &str) -> Result<ManagedSession, String> {
    app_store(root)?
        .get_managed_session(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no managed session `{id}`"))
}

/// Update a managed session in-place.  The caller provides a closure that
/// mutates the session; the updated version is persisted. The update is a
/// single transaction, so concurrent callers cannot lose each other's writes —
/// the file version needed a process-wide lock to approximate that, and could
/// not survive a second process at all.
pub fn update_session(
    root: &Path,
    id: &str,
    f: impl FnOnce(&mut ManagedSession),
) -> Result<ManagedSession, String> {
    let store = app_store(root)?;
    let mut session = store
        .get_managed_session(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no managed session `{id}`"))?;
    f(&mut session);
    session.updated_at = epoch_secs();
    store
        .upsert_managed_session(&session)
        .map_err(|e| e.to_string())?;
    Ok(session)
}

/// List all managed sessions.  If `active_only` is true, only active ones
/// are returned.
///
/// Agent inboxes are filtered out here, not at the store: a stale row can name
/// one (an older build registered handoff targets before the routability guard
/// existed). An inbox is private mail, not a session a person manages, so it
/// must never reach a list rendered to the human.
pub fn list_sessions(root: &Path, active_only: bool) -> Result<Vec<ManagedSession>, String> {
    let mut sessions = app_store(root)?
        .list_managed_sessions(active_only)
        .map_err(|e| e.to_string())?;
    sessions.retain(|s| !crate::session::is_inbox_session_id(&s.id));
    Ok(sessions)
}

/// Find sessions whose label contains `query` (case-insensitive).
pub fn find_sessions(root: &Path, query: &str) -> Result<Vec<ManagedSession>, String> {
    let q = query.to_lowercase();
    Ok(list_sessions(root, false)?
        .into_iter()
        .filter(|s| s.label.to_lowercase().contains(&q))
        .collect())
}

/// Archive a session (set status to `Archived`).
pub fn archive_session(root: &Path, id: &str) -> Result<ManagedSession, String> {
    update_session(root, id, |s| s.status = SessionStatus::Archived)
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
    fn list_sessions_hides_agent_inboxes() {
        let root = tmp();
        create_session(root.path(), "s1", "Real work", Path::new("/w")).unwrap();
        create_session(root.path(), "inbox-snippet", "Snippet inbox", Path::new("/a")).unwrap();

        let listed = list_sessions(root.path(), false).unwrap();
        assert_eq!(listed.len(), 1, "an inbox is private mail, not a managed session");
        assert_eq!(listed[0].id, "s1");

        // Still addressable by id — filtering the list must not orphan it.
        assert!(get_session(root.path(), "inbox-snippet").is_ok());
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

    // -- MissionControlStore ----------------------------------------------

    #[test]
    fn store_default_root_is_under_snippet_home() {
        let path = MissionControlStore::default_root(None);
        assert!(path.to_string_lossy().contains("mission-control"));
        assert_eq!(path, crate::config::snippet_home().join("mission-control"));
        assert!(!path.starts_with("/tmp/.snippet"));
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

        create_session(store.root(), "s1", "Locked", Path::new("/")).unwrap();

        let s = get_session(store.root(), "s1").unwrap();
        assert_eq!(s.label, "Locked");
    }

    // -- Persistence survives "restart" (read back from the store) --------

    #[test]
    fn session_persists_across_separate_reads() {
        let root = tmp();
        create_session(root.path(), "s1", "Persistent", Path::new("/w")).unwrap();
        update_session(root.path(), "s1", |s| {
            s.tags.insert("key".into(), "value".into());
        })
        .unwrap();

        // Simulate a "restart" by re-reading through a fresh handle.
        let s = get_session(root.path(), "s1").unwrap();
        assert_eq!(s.tags["key"], "value");
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

pub fn load_settings(root: &Path) -> ControlSettings {
    Store::open_cached(crate::app_store::app_db_path(root))
        .and_then(|store| store.load_control_settings())
        .unwrap_or_default()
}

pub fn save_settings(root: &Path, settings: &ControlSettings) -> Result<(), String> {
    Store::open_cached(crate::app_store::app_db_path(root))
        .map_err(|e| e.to_string())?
        .save_control_settings(settings)
        .map_err(|e| e.to_string())
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
