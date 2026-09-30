use super::*;
use serde_json::json;

fn status(store: &crate::store::Store, value: &str) {
    store.save_session_scalar("worker", "workspace", "/workspace", None,
        value, "{}", "same", "same").unwrap();
}

fn send(store: &crate::store::Store) -> serde_json::Value {
    let (events, mut receiver) = broadcast::channel(8);
    emit_device_event_with_store(json!({"kind":"idle","session":"worker"}), Some(store), &events);
    let frame = receiver.try_recv().unwrap();
    assert_eq!(frame["kind"], "notification");
    assert_eq!(receiver.try_recv().unwrap()["kind"], "idle");
    frame["notification"].clone()
}

fn replay(store: &crate::store::Store, expected: &[serde_json::Value]) {
    let settings = store.load_control_settings().unwrap();
    assert_eq!(notification_page_from_store(store, &settings, 0, 100).unwrap()["events"], json!(expected));
    assert_eq!(notification_tuple_page(store, &settings, 0, 0, 100).unwrap()["events"], json!(expected));
}

#[test]
fn notification_stop_generation_survives_writes_but_not_resumption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let store = crate::store::Store::open(&path).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    status(&store, "idle");
    let old = send(&store);
    status(&store, "idle");
    store.set_session_last_active("worker", 42).unwrap();
    store.append_conversation_events("worker", &[crate::harness::HarnessEvent::UserQuestion {
        questions: json!({"questions":[]})
    }], "later").unwrap();
    replay(&store, &[old.clone()]);
    status(&store, "running");
    replay(&store, &[]);
    status(&store, "idle");
    replay(&store, &[]);
    let new = send(&store);
    assert_ne!(old["stop_identity"], new["stop_identity"]);
    drop(store);
    let store = crate::store::Store::open(&path).unwrap();
    replay(&store, &[new]);
    store.append_conversation_messages("worker", &[crate::llm::HarnessMessage::User {
        content: "resume".into(),
    }], "later").unwrap();
    replay(&store, &[]);
    status(&store, "idle");
    replay(&store, &[]);
}

#[test]
fn notification_stop_stale_scan_is_bounded_and_advances() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("store.db")).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    status(&store, "idle");
    let identity = store.current_stop_identity("worker").unwrap().unwrap();
    for _ in 0..501 {
        store.append_notification_event(json!({"kind":"idle","session":"worker",
            "destination":{"type":"session","id":"worker"},"stop_identity":identity}), 86400).unwrap();
    }
    status(&store, "running");
    status(&store, "idle");
    let active = send(&store);
    let legacy = notification_page_from_store(&store, &settings, 0, 1).unwrap();
    let time = notification_tuple_page(&store, &settings, 0, 0, 1).unwrap();
    for page in [&legacy, &time] {
        assert_eq!(page["events"], json!([]));
        assert_eq!(page["has_more"], true);
    }
    assert_eq!(legacy["next_cursor"], 500);
    assert_eq!(time["next_cursor"]["event_id"], 500);
    assert_eq!(notification_page_from_store(&store, &settings, 500, 1).unwrap()["events"], json!([active.clone()]));
    assert_eq!(notification_tuple_page(&store, &settings,
        time["next_cursor"]["created_at"].as_i64().unwrap(), 500, 1).unwrap()["events"], json!([active]));
}
