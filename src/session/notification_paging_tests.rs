use super::*;
use serde_json::json;

#[test]
fn bounded_pages_advance_over_filtered_and_malformed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    for _ in 0..502 {
        store.append_notification_event(json!({"kind":"done","destination":{"type":"session","id":"worker"}}), 86400).unwrap();
    }
    store.with_connection(|conn| {
        conn.execute("UPDATE notification_journal SET payload_json = 'broken' WHERE event_id <= 500", [])?;
        Ok(())
    }).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    let first = notification_page_from_store(&store, &settings, 0, 1).unwrap();
    assert_eq!(first["next_cursor"], 500);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["events"], json!([]));
    let second = notification_page_from_store(&store, &settings, 500, 1).unwrap();
    assert_eq!(second["next_cursor"], 501);
    assert_eq!(second["has_more"], true);
    let third = notification_page_from_store(&store, &settings, 501, 1).unwrap();
    assert_eq!(third["next_cursor"], 502);
    assert_eq!(third["has_more"], false);
    assert_eq!(third["events"].as_array().unwrap().len(), 1);
    settings.notification_policy = "none".into();
    let filtered = notification_page_from_store(&store, &settings, 0, 1).unwrap();
    assert_eq!(filtered["next_cursor"], 500);
    assert_eq!(filtered["has_more"], true);
}

#[test]
fn notification_tuple_pages_ties_filters_expiry_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let now = chrono::Utc::now().timestamp();
    for _ in 0..502 {
        store.append_notification_event(json!({"kind":"idle","destination":{"type":"session","id":"worker"}}), 86400).unwrap();
    }
    store.with_connection(|conn| {
        conn.execute("UPDATE notification_journal SET created_at = ?1", [now])?;
        conn.execute("UPDATE notification_journal SET payload_json = 'broken' WHERE event_id <= 500", [])?;
        Ok(())
    }).unwrap();
    let mut settings = crate::mission_control::ControlSettings::default();
    settings.notification_policy = "all_sessions".into();
    let first = notification_tuple_page(&store, &settings, now - 10800, 0, 1).unwrap();
    assert_eq!(first["next_cursor"], json!({"created_at":now,"event_id":500}));
    assert_eq!(first["events"], json!([]));
    assert_eq!(first["has_more"], true);
    let second = notification_tuple_page(&store, &settings, now, 500, 1).unwrap();
    assert_eq!(second["next_cursor"], json!({"created_at":now,"event_id":501}));
    assert_eq!(second["events"][0]["kind"], "idle");
    assert_eq!(second["has_more"], true);
    let third = notification_tuple_page(&store, &settings, now, 501, 1).unwrap();
    assert_eq!(third["next_cursor"]["event_id"], 502);
    assert_eq!(third["has_more"], false);
    settings.notification_policy = "none".into();
    let filtered = notification_tuple_page(&store, &settings, now, 500, 1).unwrap();
    assert_eq!(filtered["events"], json!([]));
    assert_eq!(filtered["next_cursor"]["event_id"], 502);
    settings.notification_policy = "all_sessions".into();
    store.with_connection(|conn| {
        conn.execute("UPDATE notification_journal SET payload_json = json_set(payload_json, '$.expires_at', 0) WHERE event_id = 501", [])?;
        conn.execute("UPDATE notification_journal SET created_at = ?1 WHERE event_id = 502", [now - 86401])?;
        Ok(())
    }).unwrap();
    let expired = notification_tuple_page(&store, &settings, 0, 0, 500).unwrap();
    assert_eq!(expired["events"], json!([]));
    assert_eq!(expired["has_more"], true);
    let expired = notification_tuple_page(&store, &settings, now, 499, 500).unwrap();
    assert_eq!(expired["events"], json!([]));
    assert_eq!(expired["next_cursor"]["event_id"], 501);
    store.append_notification_event(json!({"kind":"done"}), 86400).unwrap();
    assert!(!store.notification_events_since(0, 501).unwrap().iter().any(|e| e["event_id"] == 502));
}

#[test]
fn notification_clock_survives_pruning_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notifications.db");
    let store = crate::store::Store::open(&path).unwrap();
    let future = chrono::Utc::now().timestamp() + 3600;
    store.with_connection(|conn| {
        conn.execute("INSERT INTO notification_clock VALUES (1, ?1)", [future])?;
        Ok(())
    }).unwrap();
    let first = store.append_notification_event(json!({"kind":"done"}), 86400).unwrap();
    assert_eq!(first["created_at"], future);
    store.with_connection(|conn| { conn.execute("DELETE FROM notification_journal", [])?; Ok(()) }).unwrap();
    drop(store);
    let store = crate::store::Store::open(&path).unwrap();
    let second = store.append_notification_event(json!({"kind":"done"}), 86400).unwrap();
    assert_eq!(second["created_at"], future);
    assert!(second["event_id"].as_u64().unwrap() > first["event_id"].as_u64().unwrap());
}

#[test]
fn corrupt_deduplicated_record_returns_error_without_new_append() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("notifications.db")).unwrap();
    let event = json!({"kind":"task.message","source_key":"source"});
    store.append_notification_event(event.clone(), 86400).unwrap();
    store.with_connection(|conn| {
        conn.execute("UPDATE notification_journal SET payload_json = 'broken'", [])?;
        Ok(())
    }).unwrap();
    assert!(store.append_notification_event(event, 86400).is_err());
    assert_eq!(store.notification_events_since(0, 501).unwrap().len(), 1);
}
