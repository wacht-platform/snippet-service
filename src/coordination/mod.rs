//! SQLite-backed coordination control plane.
//!
//! This module is the agent-facing half of the store: agent directory, board,
//! assignments, handoffs, and turn leases. The connection handle and schema live
//! in [`crate::store`], which is the general-purpose database for the whole
//! device — sessions and conversations use the same handle without being
//! coordination concepts.

mod agents;
pub mod board;
pub mod direct;
mod events;
mod identity;
pub mod schema;
mod tasks;
mod transfers;
pub mod types;
mod work;

pub use crate::store::{Store, StoreError};
pub use board::{
    BoardEntry, BoardEntryKind, BoardQuery, NewBoardEntry, completion_summary,
};
pub use direct::{
    DirectThreadSummary, LOCAL_HUMAN_ID, PendingDirectMessage, actor_ref, direct_thread_id,
};
pub use identity::{AgentHome, IdentityError};
pub use tasks::{
    HandoffMode, NotificationMarker, Task, TaskAgent, TaskFilter, TaskHandoff, TaskLink,
    TaskLinkKind, TaskResult, TaskStatus,
};
pub use types::{Handoff, SessionAgent, SessionLease};
pub use work::{Assignment, AssignmentFilter, AssignmentStatus};

/// The built-in general coding agent. Mission Control dispatches to it unless a
/// user names another agent, so the id is a fixed constant rather than something
/// each creator picks — "the default worker" has to be a stable address.
pub const SNIPPET_AGENT_ID: &str = "snippet";

/// Directory holding durable agent homes, derived from the Mission Control root so
/// every caller (daemon, tools, tests with an injected root) agrees on the path.
/// `agents_root(parent_of(~/.snippet/mission-control))` == `~/.snippet/agents`.
pub fn agents_root(mission_control_root: &std::path::Path) -> std::path::PathBuf {
    mission_control_root
        .parent()
        .map(|home| home.join("agents"))
        .unwrap_or_else(|| crate::config::snippet_home().join("agents"))
}
