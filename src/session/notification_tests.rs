use super::*;
use serde_json::json;

#[test]
fn durable_notification_journal_deduplicates_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notifications.db");
    let first;
    {
        let store = crate::store::Store::open(&path).unwrap();
        first = store.append_notification_event(json!({"kind":"task.message", "source_key":"source-1"}), 86400).unwrap();
        let duplicate = store.append_notification_event(json!({"kind":"task.message", "source_key":"source-1"}), 86400).unwrap();
        assert_eq!(first, duplicate);
        assert!(uuid::Uuid::parse_str(first["notification_id"].as_str().unwrap()).is_ok());
    }
    let store = crate::store::Store::open(&path).unwrap();
    assert_eq!(store.notification_events_since(0, 501).unwrap(), vec![first.clone()]);
    let next = store.append_notification_event(json!({"kind":"done"}), 86400).unwrap();
    assert!(next["event_id"].as_u64() > first["event_id"].as_u64());
}

#[test]
fn notification_policy_pagination_expiry_and_legacy_filtering() {
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    let future = chrono::Utc::now().timestamp() + 100;
    let row = |id, expiry| json!({"event_id":id,"notification_id":"uuid","expires_at":expiry,"destination":{"type":"session","id":"worker"}});
    let rows = vec![json!({"event_id":1,"notify":true}), row(2, 0), row(3, future), row(4, future)];
    let page = notification_page(rows.clone(), &settings, 0, 1);
    assert_eq!(page["next_cursor"], 3);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    settings.notification_policy = "none".into();
    let page = notification_page(rows.clone(), &settings, 0, 1);
    assert_eq!(page["next_cursor"], 4);
    assert_eq!(page["has_more"], false);
    assert!(page["events"].as_array().unwrap().is_empty());
    settings.notification_policy = "mission_control_only".into();
    assert!(!notification_allowed(&settings, &rows[2]));
    settings.mission_control_session_id = Some("worker".into());
    assert!(notification_allowed(&settings, &rows[2]));
}

#[test]
fn actionable_candidates_do_not_include_ui_only_events() {
    for kind in ["running", "idle", "term", "models", "activity"] {
        assert!(notification_candidate(&json!({"kind":kind,"session":"s"})).is_none());
    }
    for kind in ["waiting", "done", "error"] {
        let candidate =
            notification_candidate(&json!({"kind":kind,"session":"s","notify":true})).unwrap();
        assert_eq!(candidate["destination"], json!({"type":"session","id":"s"}));
        assert!(candidate.get("notify").is_none());
    }
    let direct = notification_candidate(&json!({"kind":"coordination_event","event":{"event_type":"direct_message.sent","event_id":"source","thread_id":"thread","payload":{"recipient":"session:s","body":"question"}}})).unwrap();
    assert_eq!(direct["destination"], json!({"type":"session","id":"s"}));
    assert_eq!(direct["thread_id"], "thread");
    assert_eq!(
        direct["payload"],
        json!({"recipient":"session:s","body":"question"})
    );
    assert_eq!(direct["source_key"], "source");
    assert!(notification_candidate(&json!({"kind":"coordination_event","event":{"event_type":"direct_message.sent","thread_id":"thread","payload":{"recipient":"human:local"}}})).is_none());
    let task = notification_candidate(&json!({"kind":"coordination_event","event":{"event_type":"task.message","event_id":"source","correlation_id":"task","payload":{"body":"help"}}})).unwrap();
    assert_eq!(task["destination"]["type"], "task");
}
