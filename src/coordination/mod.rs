//! SQLite-backed coordination control plane.
//!
//! This module is intentionally independent from the legacy JSON Mission Control
//! store. It is the authoritative store for the new agent directory and board.

mod agents;
mod db;
mod events;
mod identity;
mod transfers;
pub mod types;
mod work;

pub use db::{CoordinationDb, CoordinationDbError};
pub use identity::{AgentHome, IdentityError, IdentityMetadata};
pub use types::{Handoff, SessionLease};
pub use work::{Assignment, AssignmentFilter, AssignmentStatus};

/// Canonical location of the coordination SQLite database. Single source of truth
/// shared by the daemon and every session's coordination tools, so they all open
/// the same file.
pub fn default_db_path() -> std::path::PathBuf {
    crate::mission_control::MissionControlStore::default_root(None).join("coordination.sqlite3")
}

/// Directory holding durable agent homes, derived from the Mission Control root so
/// every caller (daemon, tools, tests with an injected root) agrees on the path.
/// `agents_root(parent_of(~/.snippet/mission-control))` == `~/.snippet/agents`.
pub fn agents_root(mission_control_root: &std::path::Path) -> std::path::PathBuf {
    mission_control_root
        .parent()
        .map(|home| home.join("agents"))
        .unwrap_or_else(|| crate::config::snippet_home().join("agents"))
}
