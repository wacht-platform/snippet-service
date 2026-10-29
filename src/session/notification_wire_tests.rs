use super::*;
use serde_json::json;

#[test]
fn notification_frame_matches_durable_feed_and_preserves_ui_event() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    store.save_control_settings(&settings).unwrap();
    let (events, mut receiver) = broadcast::channel(8);
    let ui_event = json!({"kind":"coordination_event","event":{"event_type":"direct_message.sent","event_id":"source","thread_id":"thread","payload":{"recipient":"session:worker","body":"question"}}});

    emit_device_event_with_store(ui_event.clone(), Some(&store), &events);

    let frame = receiver.try_recv().unwrap();
    let page = notification_page(store.notification_events_since(0, 501).unwrap(), &settings, 0, 100);
    let saved = &page["events"][0];
    assert_eq!(frame, json!({"kind":"notification","notification":saved}));
    assert!(uuid::Uuid::parse_str(saved["notification_id"].as_str().unwrap()).is_ok());
    assert!(saved["event_id"].as_u64().unwrap() > 0);
    assert_eq!(saved["destination"], json!({"type":"session","id":"worker"}));
    assert_eq!(saved["payload"], ui_event["event"]["payload"]);
    assert_eq!(receiver.try_recv().unwrap(), ui_event);
    assert!(matches!(receiver.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    assert_ne!(frame["kind"], "notification_available");
    assert!(frame.get("cursor").is_none());
}

#[test]
fn call_events_reach_the_feed_whatever_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "none".into();
    store.save_control_settings(&settings).unwrap();
    let (events, mut receiver) = broadcast::channel(8);
    let prompt = json!({"kind":"call_prompt","source_key":"call_prompt:c1","ttl_secs":120,
        "payload":{"type":"call_prompt","call_id":"c1","expires_at":1,"title":"Track this call?","body":"b"}});

    emit_device_event_with_store(prompt.clone(), Some(&store), &events);
    emit_device_event_with_store(prompt.clone(), Some(&store), &events);

    let frame = receiver.try_recv().unwrap();
    let saved = &frame["notification"];
    assert_eq!(frame["kind"], "notification");
    assert_eq!(saved["kind"], "call_prompt");
    assert_eq!(saved["destination"], json!({"type":"session","id":crate::mission_control::SESSION_ID}));
    assert_eq!(saved["payload"], prompt["payload"]);
    assert!(saved.get("ttl_secs").is_none());
    let lifetime = saved["expires_at"].as_i64().unwrap() - saved["created_at"].as_i64().unwrap();
    assert!((119..=121).contains(&lifetime), "{lifetime}");
    let page = notification_page(store.notification_events_since(0, 501).unwrap(), &settings, 0, 100);
    assert_eq!(page["events"].as_array().unwrap().len(), 1, "source_key dedupes a repeated prompt");
    assert_eq!(&page["events"][0], saved);
}
