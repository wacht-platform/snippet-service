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
