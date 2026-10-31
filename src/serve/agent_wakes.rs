use axum::extract::{Json, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;

use crate::agent_wakes::{self, Setter, WakeSpec};
use super::{mission_error, unauthorized, Auth, Shared};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/wakes", get(list_wakes))
        .route("/wake-runs", get(list_runs))
        .route("/wakes/{*owner}", get(get_wake).put(put_wake).delete(delete_wake))
        .route("/agents/{agent_id}/wake", get(get_wake).put(put_wake).delete(delete_wake))
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

fn not_found(owner: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"owner": owner, "error": "no wake schedule"}))).into_response()
}

async fn list_wakes(State(d): State<Shared>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match agent_wakes::list(&d.recurring_root) {
        Ok(jobs) => {
            let now = now();
            let wakes: Vec<_> = jobs.iter().map(|j| agent_wakes::view_with_runs(&d.recurring_root, j, now)).collect();
            Json(json!({"wakes": wakes, "limits": agent_wakes::limits()})).into_response()
        }
        Err(error) => mission_error(error),
    }
}

async fn get_wake(State(d): State<Shared>, AxumPath(owner): AxumPath<String>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match agent_wakes::get(&d.recurring_root, &owner) {
        Ok(Some(job)) => Json(agent_wakes::view_with_runs(&d.recurring_root, &job, now())).into_response(),
        Ok(None) => not_found(&owner),
        Err(error) => mission_error(error),
    }
}

async fn put_wake(
    State(d): State<Shared>,
    AxumPath(owner): AxumPath<String>,
    Query(a): Query<Auth>,
    Json(spec): Json<WakeSpec>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let kind = match agent_wakes::resolve_owner(&d.store, &owner) {
        Ok(kind) => kind,
        Err(error) => return (StatusCode::NOT_FOUND, error).into_response(),
    };
    match agent_wakes::set(&d.recurring_root, &owner, kind, spec, Setter::User, now()) {
        Ok(job) => Json(agent_wakes::view_with_runs(&d.recurring_root, &job, now())).into_response(),
        Err(error) => mission_error(error),
    }
}

async fn delete_wake(State(d): State<Shared>, AxumPath(owner): AxumPath<String>, Query(a): Query<Auth>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    match agent_wakes::clear(&d.recurring_root, &owner) {
        Ok(true) => Json(json!({"owner": owner, "deleted": true})).into_response(),
        Ok(false) => not_found(&owner),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct RunsQuery {
    token: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

async fn list_runs(State(d): State<Shared>, Query(q): Query<RunsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let store = match crate::recurring::store_at(&d.recurring_root) {
        Ok(store) => store,
        Err(error) => return mission_error(error),
    };
    let owner = q.owner.as_deref().map(str::trim).filter(|o| !o.is_empty());
    match store.list_recurring_runs(None, owner, q.limit.unwrap_or(50).clamp(1, 500)) {
        Ok(runs) => Json(json!({"runs": runs})).into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::{delete, post};

    #[test]
    fn routes_do_not_clash_with_agent_routes() {
        async fn stub() {}
        let _ = super::router()
            .merge(super::super::recurring::router())
            .route("/agents/{agent_id}/board", axum::routing::get(stub))
            .route("/agents/{agent_id}/status", post(stub))
            .route("/agents/{agent_id}", delete(stub));
    }
}
