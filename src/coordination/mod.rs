//! SQLite-backed coordination control plane.
//!
//! This module is intentionally independent from the legacy JSON Mission Control
//! store. It is the authoritative store for the new agent directory and board.

mod agents;
mod db;
mod events;
mod transfers;
pub mod types;
mod work;

pub use db::{CoordinationDb, CoordinationDbError};
pub use types::{Handoff, SessionLease};
pub use work::{Assignment, AssignmentStatus};
