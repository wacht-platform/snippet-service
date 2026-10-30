//! Settled sessions: parked conversations that routing skips and that are deleted later.
//!
//! A session idle for `COLD_AFTER_SECS` is assumed to have lost its provider prompt cache.
//! The daemon settles such a session once its memory is saved, and deletes it
//! `DELETE_AFTER_SECS` after it was settled. Any message sent into it wakes it.

use crate::store::Store;

/// Idle time after which the prompt cache is assumed purged and a session is auto-settled.
pub const COLD_AFTER_SECS: i64 = 3600;
/// How long a session stays settled before the daemon deletes it (its memory repo is kept).
pub const DELETE_AFTER_SECS: i64 = 15 * 24 * 3600;
/// Interval between settle sweeps.
pub const SWEEP_EVERY_SECS: u64 = 300;
/// Most sessions settled (one reflection model call each) per sweep.
pub const MAX_SETTLES_PER_TICK: usize = 2;
/// Most expired sessions deleted per sweep.
pub const MAX_DELETES_PER_TICK: usize = 20;
/// Upper bound on the memory reflection that precedes settling.
pub const REFLECT_TIMEOUT_SECS: u64 = 300;

pub fn idle_secs(now: i64, last_active: i64) -> i64 {
    (now - last_active).max(0)
}

pub fn cache_state(idle: i64) -> &'static str {
    if idle < COLD_AFTER_SECS { "warm" } else { "cold" }
}

/// What decides whether a session may be settled.
#[derive(Debug, Clone, Default)]
pub struct SettleFacts {
    pub id: String,
    pub kind: String,
    pub role: String,
    pub status: String,
    pub settled: bool,
    pub live_busy: bool,
    pub has_goal: bool,
    pub has_open_task: bool,
    pub has_lanes_or_watches: bool,
    pub has_queued_input: bool,
}

/// Why a session must not be settled, or `None` when it may.
pub fn refusal(f: &SettleFacts) -> Option<String> {
    let no = |why: &str| Some(format!("not settled: {why}"));
    if crate::mission_control::is_session_id(&f.id) || f.role == "mission_control" || f.kind == "mission_control" {
        return no("it is the Mission Control session");
    }
    if crate::session::is_inbox_session_id(&f.id) || f.kind == "inbox" {
        return no("it is an agent inbox");
    }
    if crate::session::is_lane_session_id(&f.id) {
        return no("it is a lane session");
    }
    if f.live_busy || matches!(f.status.as_str(), "running" | "waiting_for_input") {
        return no("it is running or waiting for the user (question or approval pending)");
    }
    if f.has_goal {
        return no("it has an ongoing goal");
    }
    if f.has_open_task {
        return no("a task assigned to it is not finished");
    }
    if f.has_lanes_or_watches {
        return no("it has running lanes or active watches");
    }
    if f.has_queued_input {
        return no("it has queued messages");
    }
    None
}

/// Whether the auto-sweep should try to settle this session now.
pub fn due_for_auto_settle(f: &SettleFacts, idle: i64) -> bool {
    !f.settled && idle >= COLD_AFTER_SECS && refusal(f).is_none()
}

/// Sessions whose settled time is more than `DELETE_AFTER_SECS` ago.
pub fn expired_settled(store: &Store, now: i64) -> Vec<String> {
    store
        .sessions_settled_before(now - DELETE_AFTER_SECS)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::SessionRow;

    fn facts() -> SettleFacts {
        SettleFacts {
            id: "s1".into(),
            kind: "conversation".into(),
            role: "standard".into(),
            status: "idle".into(),
            ..Default::default()
        }
    }

    #[test]
    fn cache_flag_boundary() {
        assert_eq!(cache_state(0), "warm");
        assert_eq!(cache_state(COLD_AFTER_SECS - 1), "warm");
        assert_eq!(cache_state(COLD_AFTER_SECS), "cold");
        assert_eq!(idle_secs(10, 50), 0);
        assert_eq!(idle_secs(100, 40), 60);
    }

    #[test]
    fn eligibility_rules() {
        assert!(refusal(&facts()).is_none());
        let cases: Vec<Box<dyn Fn(&mut SettleFacts)>> = vec![
            Box::new(|f| f.id = crate::mission_control::SESSION_ID.into()),
            Box::new(|f| f.role = "mission_control".into()),
            Box::new(|f| f.kind = "inbox".into()),
            Box::new(|f| f.id = "inbox-agent".into()),
            Box::new(|f| f.live_busy = true),
            Box::new(|f| f.status = "running".into()),
            Box::new(|f| f.status = "waiting_for_input".into()),
            Box::new(|f| f.has_goal = true),
            Box::new(|f| f.has_open_task = true),
            Box::new(|f| f.has_lanes_or_watches = true),
            Box::new(|f| f.has_queued_input = true),
        ];
        for (i, mutate) in cases.iter().enumerate() {
            let mut f = facts();
            mutate(&mut f);
            assert!(refusal(&f).is_some(), "case {i} should refuse");
        }
    }

    #[test]
    fn auto_settle_needs_cold_unsettled_eligible() {
        let f = facts();
        assert!(!due_for_auto_settle(&f, COLD_AFTER_SECS - 1));
        assert!(due_for_auto_settle(&f, COLD_AFTER_SECS));
        let settled = SettleFacts { settled: true, ..facts() };
        assert!(!due_for_auto_settle(&settled, COLD_AFTER_SECS * 5));
    }

    fn row(id: &str) -> SessionRow {
        SessionRow {
            id: id.into(),
            workspace_key: "wk".into(),
            workspace: "/w".into(),
            title: None,
            status: "idle".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_active: Some(1),
            profile: None,
            role: "standard".into(),
            agent_id: None,
            legacy_id: None,
            kind: "conversation".into(),
            settled_at: None,
        }
    }

    #[test]
    fn delete_selection_uses_settled_at_and_wake_clears_it() {
        let db = Store::open_in_memory().unwrap();
        for id in ["old", "recent", "awake"] {
            db.create_conversation(&row(id), &[], "[]").unwrap();
        }
        let now = 10_000_000;
        db.set_session_settled("old", Some(now - DELETE_AFTER_SECS - 1)).unwrap();
        db.set_session_settled("recent", Some(now - DELETE_AFTER_SECS + 1)).unwrap();
        assert_eq!(expired_settled(&db, now), vec!["old".to_string()]);
        assert!(db.get_session_row("recent").unwrap().unwrap().settled_at.is_some());
        assert!(db.get_session_row("awake").unwrap().unwrap().settled_at.is_none());

        db.set_session_last_active("old", now).unwrap();
        assert!(db.get_session_row("old").unwrap().unwrap().settled_at.is_none());
        assert!(expired_settled(&db, now).is_empty());
    }

    #[test]
    fn migration_adds_settled_at_to_an_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, workspace_key TEXT NOT NULL, workspace TEXT NOT NULL,
                  title TEXT, status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                  state_json TEXT NOT NULL DEFAULT '{}', last_active INTEGER, profile TEXT,
                  role TEXT NOT NULL DEFAULT 'standard', agent_id TEXT);
                 INSERT INTO sessions (id, workspace_key, workspace, status, created_at, updated_at)
                  VALUES ('legacy', 'k', '/w', 'idle', 'a', 'b');",
            )
            .unwrap();
        }
        let db = Store::open(&path).unwrap();
        let row = db.get_session_row("legacy").unwrap().unwrap();
        assert_eq!(row.settled_at, None);
        assert!(db.set_session_settled("legacy", Some(5)).unwrap());
        assert_eq!(db.get_session_row("legacy").unwrap().unwrap().settled_at, Some(5));
    }
}
