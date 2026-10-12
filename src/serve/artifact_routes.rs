use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{unauthorized, Shared};

const LINK_TTL_SECS: i64 = 6 * 60 * 60;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/artifacts", get(list))
        .route("/a/{exp}/{sig}/{*rest}", get(serve))
}

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    session: String,
}

async fn list(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let artifacts: Vec<_> = crate::artifacts::list(&q.session)
        .into_iter()
        .map(|a| {
            let versions: Vec<_> = a
                .versions
                .iter()
                .map(|v| {
                    json!({
                        "n": v.n,
                        "created_at": v.created_at,
                        "entry": v.entry,
                        "source": v.source,
                        "url": crate::artifacts::signed_path(&d.token, &q.session, &a.id, v, LINK_TTL_SECS),
                    })
                })
                .collect();
            json!({
                "id": a.id,
                "title": a.title,
                "kind": a.kind,
                "caption": a.caption,
                "versions": versions,
            })
        })
        .collect();
    Json(json!({ "artifacts": artifacts })).into_response()
}

async fn serve(State(d): State<Shared>, AxPath((exp, sig, rest)): AxPath<(i64, String, String)>) -> Response {
    let Some(path) = crate::artifacts::resolve_signed(&d.token, exp, &sig, &rest) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, crate::artifacts::content_type(&path)),
                (header::CONTENT_SECURITY_POLICY, crate::artifacts::CSP),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::CACHE_CONTROL, "private, max-age=300"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
