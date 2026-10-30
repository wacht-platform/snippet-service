use super::*;

use serde_json::{Value, json};

use crate::recording_policy::{MAX_SNOOZE_MINUTES, RecordingPolicy, clamp_retention};

#[derive(Deserialize, Default)]
pub(crate) struct RecordingReq {
    retention_days: Option<u32>,
    snooze_minutes: Option<i64>,
    remove_rule: Option<String>,
    remove_window: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DeleteReq {
    id: String,
    #[serde(default)]
    mode: String,
}

pub(crate) fn view(policy: &RecordingPolicy, now_ms: i64, dir: &std::path::Path) -> Value {
    json!({
        "retention_days": policy.retention_days,
        "snooze_until": policy.snooze_until_ms.filter(|t| *t > now_ms),
        "rules": policy.rules,
        "windows": policy.windows.iter().filter(|w| w.to_ms > now_ms).collect::<Vec<_>>(),
        "recordings": crate::recordings::list(dir, 100),
    })
}

pub(crate) fn apply(policy: &mut RecordingPolicy, req: &RecordingReq, now_ms: i64) -> Result<(), String> {
    if let Some(days) = req.retention_days {
        policy.retention_days = clamp_retention(days);
    }
    if let Some(minutes) = req.snooze_minutes {
        if minutes == 0 {
            policy.snooze_until_ms = None;
        } else if (1..=MAX_SNOOZE_MINUTES).contains(&minutes) {
            policy.snooze(minutes, now_ms)?;
        } else {
            return Err("snooze_minutes must be 0 to clear, or 1 to 720".into());
        }
    }
    if let Some(id) = &req.remove_rule {
        policy.remove_rule(id);
    }
    if let Some(id) = &req.remove_window {
        policy.remove_window(id);
    }
    Ok(())
}

pub(crate) async fn get(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let policy = d.config.lock().unwrap().recording.clone();
    Json(view(&policy, crate::recording_policy::now_ms(), &crate::recordings::dir())).into_response()
}

pub(crate) async fn set(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<RecordingReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    d.reload_config().await;
    let now = crate::recording_policy::now_ms();
    let saved = {
        let mut c = d.config.lock().unwrap();
        let mut next = c.recording.clone();
        match apply(&mut next, &req, now) {
            Err(e) => Err(e),
            Ok(()) => {
                next.prune(now);
                c.recording = next.clone();
                crate::config::save_config(&c, &d.config_path).map(|_| next)
            }
        }
    };
    match saved {
        Ok(policy) => Json(view(&policy, now, &crate::recordings::dir())).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub(crate) async fn delete(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<DeleteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let redact = match req.mode.as_str() {
        "delete" | "" => false,
        "redact" => true,
        _ => return (StatusCode::BAD_REQUEST, Json(json!({"error": "mode must be delete or redact"}))).into_response(),
    };
    match call_track::delete_recording(&d.store, &crate::recordings::dir(), &req.id, redact) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_policy::When;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn view_has_the_documented_shape_and_hides_expired_things() {
        let tmp = tempfile::tempdir().unwrap();
        crate::recordings::write(tmp.path(), "abcdef123456", "mac", NOW, NOW + 1000, 2, "[10:00:00] hi\n").unwrap();
        let mut p = RecordingPolicy::default();
        p.snooze_until_ms = Some(NOW - 1);
        p.set_rule("standup", "auto", When { keywords: vec!["standup".into()], ..When::default() }, "user said so", NOW).unwrap();
        p.windows.push(crate::recording_policy::Window { id: "old".into(), label: "gone".into(), from_ms: 0, to_ms: NOW - 1, mode: "auto".into(), basis: "b".into(), created_at: 0 });
        let v = view(&p, NOW, tmp.path());
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["recordings", "retention_days", "rules", "snooze_until", "windows"]);
        assert_eq!(v["retention_days"], 60);
        assert!(v["snooze_until"].is_null() && v["windows"].as_array().unwrap().is_empty());
        assert_eq!(v["rules"][0]["when"]["keywords"][0], "standup");
        assert_eq!(v["rules"][0]["mode"], "auto");
        let rec = &v["recordings"][0];
        assert_eq!((rec["id"].as_str(), rec["lines"].as_u64()), (Some("abcdef123456"), Some(2)));
        assert!(rec["path"].as_str().unwrap().ends_with(".md") && rec["started"].is_string() && rec["ended"].is_string());
    }

    #[test]
    fn post_fields_apply_and_stay_bounded() {
        let mut p = RecordingPolicy::default();
        let id = p.set_rule("r", "ask", When::default(), "", NOW).unwrap();
        let req = RecordingReq { retention_days: Some(99_999), snooze_minutes: Some(30), remove_rule: Some(id), remove_window: None };
        apply(&mut p, &req, NOW).unwrap();
        assert_eq!((p.retention_days, p.snooze_until_ms, p.rules.len()), (3650, Some(NOW + 1_800_000), 0));
        apply(&mut p, &RecordingReq { snooze_minutes: Some(0), retention_days: Some(0), ..Default::default() }, NOW).unwrap();
        assert_eq!((p.retention_days, p.snooze_until_ms), (0, None));
        assert!(apply(&mut p, &RecordingReq { snooze_minutes: Some(721), ..Default::default() }, NOW).is_err());
    }
}
