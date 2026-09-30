use super::*;
use crate::harness::HarnessEvent;
use serde_json::json;

fn persist_wait(store: &crate::store::Store, question: bool, stamp: &str) {
    let scalar = if question { json!({"pending_question":{"questions":["same question"]}}) } else { json!({}) };
    store.save_session_scalar("worker", "workspace", "/workspace", None,
        "waiting_for_input", &scalar.to_string(), stamp, stamp).unwrap();
    let event = if question {
        HarnessEvent::UserQuestion { questions: scalar["pending_question"].clone() }
    } else {
        HarnessEvent::ApprovalRequest { tool_name: "bash".into(), summary: "action".into(), index: 1, total: 1 }
    };
    store.append_conversation_events("worker", &[event], stamp).unwrap();
}

fn resolve(store: &crate::store::Store) {
    store.save_session_scalar("worker", "workspace", "/workspace", None,
        "running", "{}", "created", "updated").unwrap();
}

fn send_wait(store: &crate::store::Store) -> serde_json::Value {
    let (events, mut receiver) = broadcast::channel(8);
    emit_device_event_with_store(json!({"kind":"waiting","session":"worker"}), Some(store), &events);
    let frame = receiver.try_recv().unwrap();
    assert_eq!(frame["kind"], "notification");
    assert_eq!(receiver.try_recv().unwrap()["kind"], "waiting");
    frame["notification"].clone()
}

fn assert_replay(store: &crate::store::Store, expected: &[serde_json::Value]) {
    let settings = store.load_control_settings().unwrap();
    let legacy = notification_page_from_store(store, &settings, 0, 100).unwrap();
    let time = notification_tuple_page(store, &settings, 0, 0, 100).unwrap();
    assert_eq!(legacy["events"], json!(expected));
    assert_eq!(time["events"], json!(expected));
    let last = store.notification_events_since(0, 501).unwrap().last().unwrap().clone();
    assert_eq!(legacy["next_cursor"], last["event_id"]);
    assert_eq!(time["next_cursor"]["event_id"], last["event_id"]);
}

#[test]
fn notification_second_device_skips_answered_question_and_keeps_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notifications.db");
    let store = crate::store::Store::open(&path).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    persist_wait(&store, true, "first");
    let old = send_wait(&store);
    assert_eq!(old["attention"]["type"], "question");
    assert_replay(&store, &[old.clone()]);
    resolve(&store);
    assert_replay(&store, &[]);
    persist_wait(&store, true, "second");
    let new = send_wait(&store);
    assert_ne!(old["attention"], new["attention"]);
    drop(store);
    let store = crate::store::Store::open(&path).unwrap();
    assert_replay(&store, &[new]);
    settings.notification_policy = "none".into();
    store.save_control_settings(&settings).unwrap();
    assert_replay(&store, &[]);
}

#[test]
fn notification_second_device_preserves_active_approval_and_skips_resolved_approval() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    persist_wait(&store, false, "first");
    let old = send_wait(&store);
    assert_eq!(old["attention"]["type"], "approval");
    assert_replay(&store, &[old]);
    resolve(&store);
    assert_replay(&store, &[]);
    persist_wait(&store, false, "second");
    let new = send_wait(&store);
    assert_replay(&store, &[new]);
    resolve(&store);
    assert_replay(&store, &[]);
}

#[test]
fn notification_resolved_attention_scan_is_bounded_and_advances_both_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    persist_wait(&store, true, "old");
    let attention = store.current_attention("worker").unwrap().unwrap();
    for _ in 0..501 {
        store.append_notification_event(json!({"kind":"waiting","session":"worker",
            "destination":{"type":"session","id":"worker"},"attention":attention}), 86400).unwrap();
    }
    resolve(&store);
    persist_wait(&store, false, "new");
    let active = send_wait(&store);
    let legacy = notification_page_from_store(&store, &settings, 0, 1).unwrap();
    let time = notification_tuple_page(&store, &settings, 0, 0, 1).unwrap();
    for page in [&legacy, &time] {
        assert_eq!(page["events"], json!([]));
        assert_eq!(page["has_more"], true);
    }
    assert_eq!(legacy["next_cursor"], 500);
    assert_eq!(time["next_cursor"]["event_id"], 500);
    let legacy = notification_page_from_store(&store, &settings, 500, 1).unwrap();
    let time = notification_tuple_page(&store, &settings,
        time["next_cursor"]["created_at"].as_i64().unwrap(), 500, 1).unwrap();
    assert_eq!(legacy["events"], json!([active.clone()]));
    assert_eq!(time["events"], json!([active]));
    store.append_notification_event(json!({"kind":"waiting","session":"worker"}), 86400).unwrap();
    assert!(notification_page_from_store(&store, &settings, 502, 1).unwrap()["events"].as_array().unwrap().is_empty());
    assert!(notification_tuple_page(&store, &settings, 0, 502, 500).unwrap()["events"].as_array().unwrap().iter().all(|event| event["attention"].is_object()));
}
