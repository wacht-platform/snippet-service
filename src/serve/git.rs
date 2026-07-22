use super::*;

// ---- git operations (server-side, shells out to the system `git`) -----------
// Shelling the real `git` is the only approach with full feature parity (cred
// helpers, SSH config, every edge case). Args are passed as a VECTOR (never via
// `sh -c`), so user-supplied paths/messages/refs can't inject shell. Write ops
// take the daemon's git_write lock so a user action can't race the agent's edits.

/// Run `git -C <dir> <args...>` (no shell) with a timeout. Returns
/// (exit_code, stdout, stderr, truncated).
async fn run_git<I, S>(
    dir: &std::path::Path,
    args: I,
) -> Result<(i32, String, String, bool), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let fut = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        // Kill the child if we time out — a hung git left running keeps holding
        // repo locks (index.lock) after the daemon-side write lock is released.
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(120), fut).await {
        Ok(Ok(o)) => {
            let (so, t1) = clip_output(&o.stdout, 100_000);
            let (se, t2) = clip_output(&o.stderr, 100_000);
            Ok((o.status.code().unwrap_or(-1), so, se, t1 || t2))
        }
        Ok(Err(e)) => Err(format!("failed to run git (is it installed?): {e}")),
        Err(_) => Err("git timed out after 120s".to_string()),
    }
}

/// Standard JSON for a write op: ok flag + raw streams so the app can show errors.
fn git_result(code: i32, stdout: String, stderr: String, truncated: bool) -> Response {
    Json(serde_json::json!({
        "ok": code == 0,
        "exit_code": code,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": truncated,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct GitReq {
    session: String,
}

// POST /git/status {session} — branch, upstream, ahead/behind, and changed files.
pub(super) async fn git_status(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // -uall lists every untracked file individually (not a collapsed `dir/`), so
    // the app can show what's inside a new folder.
    match run_git(&dir, ["status", "--porcelain=v1", "-b", "-z", "-uall"]).await {
        Ok((0, stdout, _, _)) => Json(parse_status(&stdout)).into_response(),
        Ok((_, _, stderr, _)) => {
            Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn parse_branch_header(rest: &str) -> (String, Option<String>, i64, i64) {
    if let Some(b) = rest.strip_prefix("No commits yet on ") {
        return (b.trim().to_string(), None, 0, 0);
    }
    let (left, bracket) = match rest.split_once(" [") {
        Some((l, b)) => (l, Some(b.trim_end_matches(']'))),
        None => (rest, None),
    };
    let (branch, upstream) = match left.split_once("...") {
        Some((b, u)) => (b.to_string(), Some(u.to_string())),
        None => (left.to_string(), None),
    };
    let (mut ahead, mut behind) = (0i64, 0i64);
    if let Some(b) = bracket {
        for part in b.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
    (branch, upstream, ahead, behind)
}

/// Parse `git status --porcelain=v1 -b -z` into a structured snapshot.
fn parse_status(z: &str) -> serde_json::Value {
    let parts: Vec<&str> = z.split('\0').collect();
    let (mut branch, mut upstream, mut ahead, mut behind) = (String::new(), None, 0i64, 0i64);
    let mut files = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let tok = parts[i];
        if tok.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = tok.strip_prefix("## ") {
            let (b, u, a2, be) = parse_branch_header(rest);
            branch = b;
            upstream = u;
            ahead = a2;
            behind = be;
        } else if tok.len() >= 3 {
            let bytes = tok.as_bytes();
            let x = bytes[0] as char;
            let y = bytes[1] as char;
            let path = &tok[3..];
            // Rename/copy entries carry the original path in the NEXT NUL field.
            let mut orig: Option<String> = None;
            if x == 'R' || x == 'C' {
                if let Some(o) = parts.get(i + 1) {
                    orig = Some((*o).to_string());
                    i += 1;
                }
            }
            files.push(serde_json::json!({
                "path": path,
                "orig": orig,
                "x": x.to_string(),
                "y": y.to_string(),
                "staged": x != ' ' && x != '?',
                "unstaged": y != ' ' && y != '?',
                "untracked": x == '?',
            }));
        }
        i += 1;
    }
    serde_json::json!({
        "ok": true,
        "branch": branch,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
        "clean": files.is_empty(),
        "files": files,
    })
}

#[derive(Deserialize)]
pub(super) struct GitDiffReq {
    session: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    staged: bool,
    /// Untracked (new) file: show its whole content as an add-diff via
    /// `git diff --no-index` (plain `git diff` shows nothing for untracked files).
    #[serde(default)]
    untracked: bool,
}

// POST /git/diff {session, file?, staged?, untracked?} — unified diff (clipped).
pub(super) async fn git_diff(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitDiffReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let file = req.file.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let args: Vec<String> = if req.untracked {
        let Some(f) = file else {
            return (StatusCode::BAD_REQUEST, "untracked diff needs a file").into_response();
        };
        // /dev/null → file shows the entire file as additions.
        vec![
            "diff".into(),
            "--no-index".into(),
            "--".into(),
            "/dev/null".into(),
            f.to_string(),
        ]
    } else {
        let mut a = vec!["diff".into()];
        if req.staged {
            a.push("--staged".into());
        }
        if let Some(f) = file {
            a.push("--".into());
            a.push(f.to_string());
        }
        a
    };
    match run_git(&dir, &args).await {
        Ok((code, stdout, stderr, truncated)) => Json(serde_json::json!({
            // `git diff --no-index` exits 1 when files differ — that's success here.
            "ok": code == 0 || (req.untracked && code == 1),
            "patch": stdout,
            "stderr": stderr,
            "truncated": truncated,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitLogReq {
    session: String,
    #[serde(default)]
    limit: Option<u32>,
}

// POST /git/log {session, limit?} — recent commits as structured records.
pub(super) async fn git_log(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitLogReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let n = req.limit.unwrap_or(50).clamp(1, 500);
    // Unit-separator (\x1f) between fields, record-separator (\x1e) between commits.
    let fmt = "--pretty=format:%H\x1f%h\x1f%an\x1f%ad\x1f%s\x1e";
    match run_git(&dir, ["log", &format!("-n{n}"), "--date=short", fmt]).await {
        Ok((0, stdout, _, _)) => {
            let commits: Vec<serde_json::Value> = stdout
                .split('\x1e')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .filter_map(|rec| {
                    let f: Vec<&str> = rec.split('\x1f').collect();
                    (f.len() == 5).then(|| serde_json::json!({
                        "hash": f[0], "short": f[1], "author": f[2], "date": f[3], "subject": f[4],
                    }))
                })
                .collect();
            Json(serde_json::json!({"ok": true, "commits": commits})).into_response()
        }
        Ok((_, _, stderr, _)) => {
            Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/branches {session} — local branches + which is current.
pub(super) async fn git_branches(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match run_git(&dir, ["branch", "--format=%(HEAD)\x1f%(refname:short)"]).await {
        Ok((0, stdout, _, _)) => {
            let mut current = String::new();
            let branches: Vec<String> = stdout
                .lines()
                .filter_map(|l| l.split_once('\x1f'))
                .map(|(head, name)| {
                    if head == "*" {
                        current = name.to_string();
                    }
                    name.to_string()
                })
                .collect();
            Json(serde_json::json!({"ok": true, "current": current, "branches": branches}))
                .into_response()
        }
        Ok((_, _, stderr, _)) => {
            Json(serde_json::json!({"ok": false, "error": stderr.trim()})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitStageReq {
    session: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    all: bool,
}

// POST /git/stage {session, paths?, all?} — `git add`.
pub(super) async fn git_stage(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitStageReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["add".into()];
    if req.all {
        args.push("-A".into());
    } else if !req.paths.is_empty() {
        args.push("--".into());
        args.extend(req.paths.iter().cloned());
    } else {
        return (
            StatusCode::BAD_REQUEST,
            "no paths (pass paths[] or all:true)",
        )
            .into_response();
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitUnstageReq {
    session: String,
    #[serde(default)]
    paths: Vec<String>,
}

// POST /git/unstage {session, paths?} — `git restore --staged` (all if none given).
pub(super) async fn git_unstage(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitUnstageReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["restore".into(), "--staged".into(), "--".into()];
    if req.paths.is_empty() {
        args.push(".".into());
    } else {
        args.extend(req.paths.iter().cloned());
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitCommitReq {
    session: String,
    message: String,
    #[serde(default)]
    amend: bool,
}

// POST /git/commit {session, message, amend?} — commit the staged index.
pub(super) async fn git_commit(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitCommitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.message.trim().is_empty() && !req.amend {
        return (StatusCode::BAD_REQUEST, "empty commit message").into_response();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["commit".into(), "-m".into(), req.message.clone()];
    if req.amend {
        args.push("--amend".into());
    }
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitCheckoutReq {
    session: String,
    target: String,
    #[serde(default)]
    create: bool,
}

// POST /git/checkout {session, target, create?} — switch (or create) a branch.
pub(super) async fn git_checkout(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitCheckoutReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.target.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty target").into_response();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut args: Vec<String> = vec!["checkout".into()];
    if req.create {
        args.push("-b".into());
    }
    args.push(req.target.clone());
    let _lock = d.git_write.lock().await;
    match run_git(&dir, &args).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/push {session} — push the current branch (uses the box's git creds).
pub(super) async fn git_push(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["push"]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// POST /git/pull {session} — fast-forward-only pull (surfaces non-ff for the user).
pub(super) async fn git_pull(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["pull", "--ff-only"]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct GitStashReq {
    session: String,
    #[serde(default)]
    op: Option<String>,
}

// POST /git/stash {session, op?} — op = push (default) | pop | list | drop.
pub(super) async fn git_stash(
    State(d): State<Shared>,
    Query(a): Query<Auth>,
    Json(req): Json<GitStashReq>,
) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let sub = match req.op.as_deref().unwrap_or("push") {
        "pop" => "pop",
        "list" => "list",
        "drop" => "drop",
        "push" | "save" | "" => "push",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown stash op `{other}`"),
            )
                .into_response()
        }
    };
    let _lock = d.git_write.lock().await;
    match run_git(&dir, ["stash", sub]).await {
        Ok((c, so, se, t)) => git_result(c, so, se, t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
