//! Extras for the session list: last-turn change size and when a run began.

use serde::{Deserialize, Serialize};

/// Totals of the most recent turn that changed files (from its `turn_changes` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

static RUN_STARTS: std::sync::Mutex<Option<std::collections::HashMap<String, i64>>> = std::sync::Mutex::new(None);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// When the session's current run began, while it is running.
pub fn running_since(id: &str, status: &str, turn_started_at: Option<&str>, observed_now: i64) -> Option<i64> {
    let mut map = RUN_STARTS.lock().unwrap();
    let map = map.get_or_insert_with(Default::default);
    if status != "running" {
        map.remove(id);
        return None;
    }
    if let Some(at) = turn_started_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        return Some(at.timestamp());
    }
    Some(*map.entry(id.to_string()).or_insert(observed_now))
}

/// `running_since` for a listed session, reading the harness's turn start when it has one.
pub fn attach_running_since(s: &mut crate::session::SessionInfo) {
    let turn = (s.status == "running")
        .then(|| crate::session::state_path_for_id(&s.id).and_then(|p| crate::session::read_session_meta(&p)))
        .flatten()
        .and_then(|st| st.turn_started_at);
    s.running_since = running_since(&s.id, &s.status, turn.as_deref(), now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_totals_round_trip_from_the_stored_json() {
        let c: Changes = serde_json::from_str(r#"{"files":3,"added":10,"removed":2}"#).unwrap();
        assert_eq!(c, Changes { files: 3, added: 10, removed: 2 });
    }

    #[test]
    fn running_since_prefers_turn_start_then_first_observation() {
        let id = "rs-test";
        assert_eq!(running_since(id, "idle", None, 50), None);
        assert_eq!(running_since(id, "running", None, 100), Some(100));
        assert_eq!(running_since(id, "running", None, 200), Some(100), "kept while running");
        assert_eq!(running_since(id, "running", Some("1970-01-01T00:01:00Z"), 300), Some(60));
        assert_eq!(running_since(id, "idle", None, 400), None);
        assert_eq!(running_since(id, "running", None, 500), Some(500), "cleared on stop");
        running_since(id, "idle", None, 0);
    }
}
