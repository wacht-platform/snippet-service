//! Pull request review through the GitHub CLI. The daemon never holds a
//! GitHub token: every call is `gh`, run non-interactively in the session's
//! workspace, so "logged in to gh" is the only requirement and private repos
//! and GitHub Enterprise work the same way.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use super::session_routes::resolve_session_dir;
use super::{unauthorized, Auth, Shared};

const GH_TIMEOUT: Duration = Duration::from_secs(45);

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/github/status", get(status))
        .route("/github/prs", get(list))
        .route("/github/pr", get(detail))
        .route("/github/pr/diff", get(diff))
        .route("/github/pr/review", post(review))
        .route("/github/pr/comment", post(comment))
        .route("/github/pr/merge", post(merge))
        .route("/github/pr/reply", post(reply))
        .route("/github/pr/thread", post(resolve_thread))
        .route("/github/commit/diff", get(commit_diff))
        .route("/github/pr/state", post(pr_state))
}

async fn gh(dir: &Path, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("gh isn't available: {e}"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await.map_err(|e| e.to_string())?;
    }
    let out = tokio::time::timeout(GH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "gh timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("gh exited with {}", out.status) } else { err })
    }
}

async fn gh_json(dir: &Path, args: &[&str]) -> Result<Value, String> {
    let text = gh(dir, args, None).await?;
    serde_json::from_str(&text).map_err(|e| format!("gh returned unreadable JSON: {e}"))
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn session_dir(session: &str) -> Result<PathBuf, Response> {
    resolve_session_dir(session)
}

#[derive(Deserialize)]
struct SessionQuery {
    token: Option<String>,
    session: String,
}

/// Whether PR review is available for this session: gh installed and logged
/// in, and the workspace is a GitHub repository.
async fn status(State(d): State<Shared>, Query(q): Query<SessionQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(_) => return Json(json!({ "available": false, "reason": "no workspace" })).into_response(),
    };
    if gh(&dir, &["--version"], None).await.is_err() {
        return Json(json!({ "available": false, "reason": "gh is not installed" })).into_response();
    }
    if gh(&dir, &["auth", "status"], None).await.is_err() {
        return Json(json!({ "available": false, "reason": "gh is not logged in" })).into_response();
    }
    let repo = match gh_json(&dir, &["repo", "view", "--json", "nameWithOwner,url,viewerPermission"]).await {
        Ok(repo) => repo,
        Err(_) => {
            return Json(json!({ "available": false, "reason": "not a GitHub repository" })).into_response();
        }
    };
    let viewer = gh(&dir, &["api", "user", "--jq", ".login"], None)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let settings = gh_json(&dir, &["api", "repos/{owner}/{repo}"]).await.unwrap_or(Value::Null);
    let allowed = |key: &str| settings.get(key).and_then(Value::as_bool).unwrap_or(true);
    Json(json!({
        "available": true,
        "repo": repo.get("nameWithOwner"),
        "url": repo.get("url"),
        "permission": repo.get("viewerPermission"),
        "viewer": viewer,
        "merge_methods": {
            "merge": allowed("allow_merge_commit"),
            "squash": allowed("allow_squash_merge"),
            "rebase": allowed("allow_rebase_merge"),
        },
        "auto_merge": settings.get("allow_auto_merge").and_then(Value::as_bool).unwrap_or(false),
        "delete_branch_on_merge": settings.get("delete_branch_on_merge").and_then(Value::as_bool).unwrap_or(false),
    }))
    .into_response()
}

const LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,state,reviewDecision,statusCheckRollup,additions,deletions,updatedAt,url,labels,reviewRequests";

#[derive(Deserialize)]
struct ListQuery {
    token: Option<String>,
    session: String,
    /// open (default), mine, review, closed
    filter: Option<String>,
}

async fn list(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let mut args = vec!["pr", "list", "--limit", "60", "--json", LIST_FIELDS];
    match q.filter.as_deref().unwrap_or("open") {
        "mine" => args.extend(["--author", "@me"]),
        "review" => args.extend(["--search", "review-requested:@me"]),
        "closed" => args.extend(["--state", "closed"]),
        _ => args.extend(["--state", "open"]),
    }
    match gh_json(&dir, &args).await {
        Ok(prs) => Json(json!({ "prs": prs })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

const DETAIL_FIELDS: &str = "number,title,body,author,state,isDraft,headRefName,baseRefName,additions,deletions,changedFiles,files,commits,reviews,comments,reviewDecision,statusCheckRollup,mergeable,mergeStateStatus,url,createdAt,updatedAt,labels,reviewRequests,headRefOid,autoMergeRequest,isCrossRepository";

#[derive(Deserialize)]
struct PrQuery {
    token: Option<String>,
    session: String,
    number: u64,
}

async fn detail(State(d): State<Shared>, Query(q): Query<PrQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = q.number.to_string();
    let view_args = ["pr", "view", number.as_str(), "--json", DETAIL_FIELDS];
    let number_field = format!("n={number}");
    let query_field = format!("query={THREADS_QUERY}");
    let threads_args = [
        "api",
        "graphql",
        "-F",
        "owner={owner}",
        "-F",
        "repo={repo}",
        "-F",
        number_field.as_str(),
        "-f",
        query_field.as_str(),
    ];
    let (view, threads) = tokio::join!(gh_json(&dir, &view_args), gh_json(&dir, &threads_args));
    let mut pr = match view {
        Ok(pr) => pr,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, e),
    };
    let threads: Vec<Value> = threads
        .ok()
        .and_then(|t| t.pointer("/data/repository/pullRequest/reviewThreads/nodes").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            let comments: Vec<Value> = t
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    json!({
                        "id": c.get("databaseId"),
                        "body": c.get("body"),
                        "author": c.pointer("/author/login"),
                        "created_at": c.get("createdAt"),
                        "review_id": c.pointer("/pullRequestReview/id"),
                    })
                })
                .collect();
            json!({
                "id": t.get("id"),
                "path": t.get("path"),
                "line": t.get("line").filter(|l| !l.is_null()).or(t.get("originalLine")),
                "start_line": t.get("startLine").filter(|l| !l.is_null()).or(t.get("originalStartLine")),
                "side": t.get("diffSide"),
                "resolved": t.get("isResolved"),
                "outdated": t.get("isOutdated"),
                "comments": comments,
            })
        })
        .collect();
    if let Some(obj) = pr.as_object_mut() {
        obj.insert("threads".to_string(), Value::Array(threads));
    }
    Json(pr).into_response()
}

async fn diff(State(d): State<Shared>, Query(q): Query<PrQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    match gh(&dir, &["pr", "diff", &q.number.to_string()], None).await {
        Ok(text) => Json(json!({ "diff": text })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct InlineComment {
    path: String,
    line: u64,
    side: Option<String>,
    start_line: Option<u64>,
    start_side: Option<String>,
    body: String,
}

#[derive(Deserialize)]
struct ReviewReq {
    session: String,
    number: u64,
    /// APPROVE, REQUEST_CHANGES or COMMENT
    event: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comments: Vec<InlineComment>,
}

async fn review(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ReviewReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let event = req.event.to_ascii_uppercase();
    if !matches!(event.as_str(), "APPROVE" | "REQUEST_CHANGES" | "COMMENT") {
        return fail(StatusCode::BAD_REQUEST, "event must be APPROVE, REQUEST_CHANGES or COMMENT");
    }
    if event != "APPROVE" && req.body.trim().is_empty() && req.comments.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "a comment or request for changes needs a summary or an inline comment");
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let comments: Vec<Value> = req
        .comments
        .iter()
        .filter(|c| !c.body.trim().is_empty())
        .map(|c| {
            let side = c.side.clone().unwrap_or_else(|| "RIGHT".to_string());
            let mut comment = json!({
                "path": c.path,
                "line": c.line,
                "side": side,
                "body": c.body,
            });
            if let Some(start) = c.start_line.filter(|start| *start < c.line) {
                comment["start_line"] = json!(start);
                comment["start_side"] = json!(c.start_side.clone().unwrap_or(side));
            }
            comment
        })
        .collect();
    let payload = json!({ "event": event, "body": req.body, "comments": comments }).to_string();
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews", req.number);
    match gh(&dir, &["api", "-X", "POST", &path, "--input", "-"], Some(&payload)).await {
        Ok(text) => {
            let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            Json(json!({ "submitted": true, "url": reply.get("html_url") })).into_response()
        }
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CommentReq {
    session: String,
    number: u64,
    body: String,
}

async fn comment(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<CommentReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.body.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "the comment is empty");
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    match gh(&dir, &["pr", "comment", &req.number.to_string(), "--body-file", "-"], Some(&req.body)).await {
        Ok(text) => Json(json!({ "posted": true, "url": text.trim() })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct MergeReq {
    session: String,
    number: u64,
    /// merge (default), squash or rebase
    method: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    /// The head commit the user reviewed; the merge is refused if it moved.
    #[serde(default)]
    head: Option<String>,
    /// Merge once requirements are met instead of now.
    #[serde(default)]
    auto: bool,
    /// Turn off a pending auto-merge.
    #[serde(default)]
    disable_auto: bool,
    /// Merge without waiting for requirements (admins only).
    #[serde(default)]
    admin: bool,
    /// Delete the remote head branch once merged.
    #[serde(default)]
    delete_branch: bool,
    #[serde(default)]
    branch: Option<String>,
}

async fn merge(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<MergeReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = req.number.to_string();
    if req.disable_auto {
        return match gh(&dir, &["pr", "merge", &number, "--disable-auto"], None).await {
            Ok(_) => Json(json!({ "auto": false })).into_response(),
            Err(e) => fail(StatusCode::BAD_GATEWAY, e),
        };
    }
    let flag = match req.method.as_deref().unwrap_or("merge") {
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => "--merge",
    };
    let mut args: Vec<String> = vec!["pr".into(), "merge".into(), number.clone(), flag.into()];
    if let Some(subject) = req.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        args.extend(["--subject".into(), subject.to_string()]);
    }
    if let Some(body) = req.body.as_deref() {
        args.extend(["--body".into(), body.to_string()]);
    }
    if let Some(head) = req.head.as_deref().filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()) && !h.is_empty()) {
        args.extend(["--match-head-commit".into(), head.to_string()]);
    }
    if req.auto {
        args.push("--auto".into());
    } else if req.admin {
        args.push("--admin".into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = gh(&dir, &refs, None).await {
        return fail(StatusCode::BAD_GATEWAY, e);
    }
    if req.auto {
        return Json(json!({ "auto": true })).into_response();
    }
    let mut deleted = false;
    if req.delete_branch
        && let Some(branch) = req.branch.as_deref().filter(|b| !b.is_empty() && !b.contains(".."))
    {
        let path = format!("repos/{{owner}}/{{repo}}/git/refs/heads/{branch}");
        deleted = gh(&dir, &["api", "-X", "DELETE", &path], None).await.is_ok();
    }
    Json(json!({ "merged": true, "branch_deleted": deleted })).into_response()
}

#[derive(Deserialize)]
struct StateReq {
    session: String,
    number: u64,
    /// ready, draft, close, reopen, update or update_rebase
    action: String,
}

async fn pr_state(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<StateReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let number = req.number.to_string();
    let args: Vec<&str> = match req.action.as_str() {
        "ready" => vec!["pr", "ready", &number],
        "draft" => vec!["pr", "ready", &number, "--undo"],
        "close" => vec!["pr", "close", &number],
        "reopen" => vec!["pr", "reopen", &number],
        "update" => vec!["pr", "update-branch", &number],
        "update_rebase" => vec!["pr", "update-branch", &number, "--rebase"],
        _ => return fail(StatusCode::BAD_REQUEST, "unknown action"),
    };
    match gh(&dir, &args, None).await {
        Ok(_) => Json(json!({ "done": true })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

const THREADS_QUERY: &str = "query($owner: String!, $repo: String!, $n: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $n) { reviewThreads(first: 100) { nodes { id isResolved isOutdated path line originalLine startLine originalStartLine diffSide comments(first: 50) { nodes { databaseId body createdAt author { login } pullRequestReview { id } } } } } } } }";

#[derive(Deserialize)]
struct ReplyReq {
    session: String,
    number: u64,
    comment_id: u64,
    body: String,
}

async fn reply(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ReplyReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.body.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "the reply is empty");
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let path = format!("repos/{{owner}}/{{repo}}/pulls/{}/comments/{}/replies", req.number, req.comment_id);
    let payload = json!({ "body": req.body }).to_string();
    match gh(&dir, &["api", "-X", "POST", &path, "--input", "-"], Some(&payload)).await {
        Ok(_) => Json(json!({ "posted": true })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct ThreadReq {
    session: String,
    thread_id: String,
    resolved: bool,
}

async fn resolve_thread(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<ThreadReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let mutation = if req.resolved {
        "mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"
    } else {
        "mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"
    };
    let id_field = format!("id={}", req.thread_id);
    match gh(&dir, &["api", "graphql", "-F", &id_field, "-f", &format!("query={mutation}")], None).await {
        Ok(_) => Json(json!({ "resolved": req.resolved })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CommitQuery {
    token: Option<String>,
    session: String,
    sha: String,
}

async fn commit_diff(State(d): State<Shared>, Query(q): Query<CommitQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    if q.sha.is_empty() || !q.sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return fail(StatusCode::BAD_REQUEST, "bad commit id");
    }
    let dir = match session_dir(&q.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let path = format!("repos/{{owner}}/{{repo}}/commits/{}", q.sha);
    match gh(&dir, &["api", "-H", "Accept: application/vnd.github.diff", &path], None).await {
        Ok(text) => Json(json!({ "diff": text })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, e),
    }
}
