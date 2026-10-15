use super::*;
use crate::harness::HarnessEvent;
use crate::routing::{self, LimitHit, Route};

fn route_for(id: &str) -> Option<Route> {
    if id == crate::mission_control::SESSION_ID {
        Some(Route::Mission)
    } else if crate::session::is_inbox_session_id(id) {
        Some(Route::Inbox)
    } else {
        None
    }
}

async fn profile_of(d: &Daemon, id: &str) -> String {
    let live = d.sessions.lock().await.get(id).and_then(|s| s.profile.clone());
    live.or_else(|| crate::session::state_path_for_id(id).and_then(|p| crate::session::read_session_profile(&p)))
        .or_else(|| d.config.lock().unwrap().active_setup.clone())
        .unwrap_or_default()
}

async fn send_input(d: &Daemon, id: &str, text: String) {
    let tx = d.sessions.lock().await.get(id).filter(|s| !s.join.is_finished()).map(|s| s.input_tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(LoopInput::UserMessage(text));
    }
}

async fn continue_on(d: &Daemon, id: &str, from: &str, to: &str, resets_at: i64) -> Result<(), String> {
    d.run_session_on_profile(id, to, true).await?;
    record_notice(
        d,
        id,
        HarnessEvent::ProfileSwitched { from: from.to_string(), to: to.to_string(), resets_at },
    )
    .await;
    send_input(
        d,
        id,
        format!("[routing] {from} hit its usage limit, so this session now runs on {to}. Pick up where the last turn stopped and finish it."),
    )
    .await;
    Ok(())
}

pub(crate) async fn routing_loop(d: Shared, mut rx: tokio::sync::mpsc::UnboundedReceiver<LimitHit>) {
    while let Some(hit) = rx.recv().await {
        let id = hit.session_id.as_str();
        if crate::session::is_lane_session_id(id) || !d.store.has_conversation(id).unwrap_or(false) {
            continue;
        }
        let profile = profile_of(&d, id).await;
        routing::mark_limited(&profile, hit.resets_at);
        let resets_at = routing::limited_until(&profile).unwrap_or(hit.resets_at);
        let cfg = d.config.lock().unwrap().clone();
        let next = route_for(id).and_then(|route| routing::next_after(&cfg, route, &profile));
        if let Some(next) = next {
            if let Err(e) = continue_on(&d, id, &profile, &next, resets_at).await {
                eprintln!("routing {id}: could not switch to {next}: {e}");
            } else {
                continue;
            }
        }
        let options = if route_for(id).is_some() { Vec::new() } else { routing::alternatives(&cfg, &profile) };
        record_notice(&d, id, HarnessEvent::LimitReached { profile, resets_at, options }).await;
    }
}

#[derive(Deserialize)]
pub(crate) struct LimitReq {
    session: String,
    action: String,
    #[serde(default)]
    profile: Option<String>,
}

pub(crate) async fn resolve_limit(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<LimitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let id = req.session.as_str();
    let current = profile_of(&d, id).await;
    let resets_at = routing::limited_until(&current).unwrap_or(0);
    match req.action.as_str() {
        "switch" => {
            let Some(to) = req.profile.filter(|p| !p.trim().is_empty()) else {
                return (StatusCode::BAD_REQUEST, "profile required").into_response();
            };
            match continue_on(&d, id, &current, &to, resets_at).await {
                Ok(()) => Json(serde_json::json!({ "profile": to })).into_response(),
                Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
            }
        }
        "wait" => {
            let until = resets_at.max(chrono::Utc::now().timestamp() + 60);
            record_notice(&d, id, HarnessEvent::LimitWait { profile: current.clone(), resets_at: until }).await;
            let d2 = d.clone();
            let session = id.to_string();
            tokio::spawn(async move {
                let wait = (until - chrono::Utc::now().timestamp()).max(0) as u64;
                tokio::time::sleep(std::time::Duration::from_secs(wait + 5)).await;
                let live = d2.sessions.lock().await.get(&session).is_some_and(|s| !s.join.is_finished());
                if !live && d2.run_session_on_profile(&session, &current, false).await.is_err() {
                    return;
                }
                send_input(
                    &d2,
                    &session,
                    format!("[routing] {current}'s usage window has reset. Pick up where the last turn stopped and finish it."),
                )
                .await;
            });
            Json(serde_json::json!({ "resets_at": until })).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "action must be switch or wait").into_response(),
    }
}
