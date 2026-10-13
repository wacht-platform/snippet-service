use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{unauthorized, Auth, Shared};

const LINK_TTL_SECS: i64 = 6 * 60 * 60;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/artifacts", get(list))
        .route("/artifacts/zip", post(zip_artifact))
        .route("/zip", post(zip_files))
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

async fn serve(
    State(d): State<Shared>,
    AxPath((exp, sig, rest)): AxPath<(i64, String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(path) = crate::artifacts::resolve_signed(&d.token, exp, &sig, &rest) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let len = bytes.len() as u64;
    let common = [
        (header::CONTENT_TYPE, crate::artifacts::content_type(&path)),
        (header::CONTENT_SECURITY_POLICY, crate::artifacts::CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "private, max-age=300"),
        (header::ACCEPT_RANGES, "bytes"),
    ];
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("bytes="))
        .and_then(|spec| {
            let (a, b) = spec.split(',').next()?.split_once('-')?;
            let (a, b) = (a.trim(), b.trim());
            if a.is_empty() {
                let suffix: u64 = b.parse().ok()?;
                (suffix > 0).then(|| (len.saturating_sub(suffix), len.saturating_sub(1)))
            } else {
                let start: u64 = a.parse().ok()?;
                let end = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
                Some((start, end))
            }
        });
    match range {
        Some((start, end)) if len > 0 && start <= end && start < len => (
            StatusCode::PARTIAL_CONTENT,
            common,
            [(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))],
            bytes[start as usize..=end as usize].to_vec(),
        )
            .into_response(),
        Some(_) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response(),
        None => (common, bytes).into_response(),
    }
}

#[derive(Deserialize)]
struct ZipArtifactReq {
    session: String,
    id: String,
    version: Option<u32>,
}

async fn zip_artifact(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ZipArtifactReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let Some(artifact) = crate::artifacts::get(&req.session, &req.id) else {
        return (StatusCode::NOT_FOUND, "no such artifact").into_response();
    };
    let Some(version) = req
        .version
        .and_then(|n| artifact.versions.iter().find(|v| v.n == n))
        .or(artifact.latest())
    else {
        return (StatusCode::NOT_FOUND, "no versions").into_response();
    };
    let session_dir = crate::artifacts::session_dir_name(&req.session);
    let Some(dir) = crate::artifacts::version_dir(&session_dir, &artifact.id, version.n) else {
        return (StatusCode::BAD_REQUEST, "bad artifact").into_response();
    };
    let sources: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    let name = format!("{}-v{}", artifact.id, version.n);
    match tokio::task::spawn_blocking(move || crate::artifacts::zip_to_temp(&name, &sources)).await {
        Ok(Ok(path)) => Json(json!({ "path": path.display().to_string() })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ZipFilesReq {
    name: Option<String>,
    paths: Vec<String>,
}

async fn zip_files(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<ZipFilesReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let sources: Vec<std::path::PathBuf> = req
        .paths
        .iter()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.exists())
        .collect();
    if sources.is_empty() {
        return (StatusCode::BAD_REQUEST, "none of the paths exist").into_response();
    }
    let name = req.name.unwrap_or_else(|| "files".to_string());
    match tokio::task::spawn_blocking(move || crate::artifacts::zip_to_temp(&name, &sources)).await {
        Ok(Ok(path)) => Json(json!({ "path": path.display().to_string() })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
