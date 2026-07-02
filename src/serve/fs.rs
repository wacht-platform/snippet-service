use super::*;

#[derive(Serialize)]
struct FsEntry {
    name: String,
    path: String,
    is_dir: bool,
    git: bool,
}

#[derive(Serialize)]
struct FsListing {
    path: String,
    parent: Option<String>,
    entries: Vec<FsEntry>,
}

#[derive(Deserialize)]
pub(super) struct FsQuery {
    token: Option<String>,
    path: Option<String>,
}

// GET /fs?path= — one directory level (lazy folder tree). Defaults to $HOME.
pub(super) async fn browse_fs(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let path = q
        .path
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&path) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // hide dotfiles
            }
            let p = e.path();
            let is_dir = p.is_dir();
            entries.push(FsEntry {
                git: is_dir && p.join(".git").exists(),
                name,
                path: p.display().to_string(),
                is_dir,
            });
        }
    }
    // Directories first, then alphabetical.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Json(FsListing {
        parent: path.parent().map(|p| p.display().to_string()),
        path: path.display().to_string(),
        entries,
    })
    .into_response()
}

#[derive(Serialize)]
struct FileContent {
    path: String,
    content: String,
    size: u64,
    truncated: bool,
    binary: bool,
    /// Fingerprint of the full on-disk bytes — the editor sends it back on save
    /// for optimistic-concurrency conflict detection.
    hash: String,
}

/// Fast non-cryptographic fingerprint of file bytes (same scheme used elsewhere).
fn hash_bytes(b: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Deserialize)]
pub(super) struct FsWriteReq {
    path: String,
    content: String,
    /// If set, the write is refused when the file on disk no longer matches it
    /// (someone — the agent or another editor — changed it since it was opened).
    #[serde(default)]
    prev_hash: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct FsMkdirReq {
    /// Absolute path of the new directory to create.
    path: String,
}

#[derive(Deserialize)]
pub(super) struct FsDeleteReq {
    /// Absolute path of the file or directory to delete (dirs are removed recursively).
    path: String,
}

// POST /fs/write {path, content, prev_hash?} — atomic write (temp + rename) with
// optimistic-concurrency conflict detection. Token-gated; UTF-8 text only.
pub(super) async fn write_fs_file(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsWriteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    if let Some(prev) = req.prev_hash.as_deref() {
        match std::fs::read(&path) {
            Ok(cur) if hash_bytes(&cur) != prev => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "ok": false,
                        "conflict": true,
                        "error": "file changed on disk since you opened it — reload before saving",
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"ok": false, "conflict": true, "error": "file no longer exists"})),
                )
                    .into_response();
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&parent) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create dir: {e}")).into_response();
    }
    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{fname}.snippet.tmp"));
    let bytes = req.content.into_bytes();
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("write temp: {e}")).into_response();
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")).into_response();
    }
    Json(serde_json::json!({"ok": true, "hash": hash_bytes(&bytes), "size": bytes.len()})).into_response()
}

// POST /fs/mkdir {path} — create a new directory (token-gated).
pub(super) async fn make_fs_dir(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsMkdirReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "error": "a file or folder with that name already exists"})),
        )
            .into_response();
    }
    if let Err(e) = std::fs::create_dir_all(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("create dir: {e}")).into_response();
    }
    Json(serde_json::json!({"ok": true, "path": path.to_string_lossy()})).into_response()
}

// POST /fs/delete {path} — remove a file or directory (recursive). Token-gated.
pub(super) async fn delete_fs_path(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<FsDeleteReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    if req.path.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    }
    let path = PathBuf::from(&req.path);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Json(serde_json::json!({"ok": true, "already_gone": true})).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let res = if meta.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match res {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")).into_response(),
    }
}

// GET /fs/download?path= — stream a file's raw bytes (any type) for the app to
// save to the device. Token-gated.
const MAX_FS_DOWNLOAD_BYTES: u64 = 50 * 1024 * 1024;
pub(super) async fn download_fs_file(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if meta.is_dir() {
        return (StatusCode::BAD_REQUEST, "path is a directory").into_response();
    }
    if meta.len() > MAX_FS_DOWNLOAD_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "file too large to download").into_response();
    }
    match std::fs::read(&path) {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Cap a single file read for the mobile viewer.
const MAX_FS_FILE_BYTES: usize = 512 * 1024;

// GET /fs/file?path= — read one text file's contents (for the in-app file viewer).
pub(super) async fn read_fs_file(State(d): State<Shared>, Query(q): Query<FsQuery>) -> Response {
    if !d.authed(&q.token) {
        return unauthorized();
    }
    let Some(path) = q.path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
        return (StatusCode::BAD_REQUEST, "path required").into_response();
    };
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => {
            let size = m.len();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            };
            let truncated = bytes.len() > MAX_FS_FILE_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_FS_FILE_BYTES)];
            // Binary if it has a NUL or isn't valid UTF-8 up to the cut.
            let binary = slice.contains(&0) || std::str::from_utf8(slice).is_err();
            let content = if binary {
                String::new()
            } else {
                String::from_utf8_lossy(slice).to_string()
            };
            Json(FileContent { path: path.display().to_string(), content, size, truncated, binary, hash: hash_bytes(&bytes) }).into_response()
        }
        Ok(_) => (StatusCode::BAD_REQUEST, "not a file").into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct UploadReq {
    data_base64: String,
    #[serde(default)]
    name: Option<String>,
    /// When set, save into this directory under the original [name] (instead of
    /// a temp dir with a generated name) — used by the file-explorer upload.
    #[serde(default)]
    dir: Option<String>,
}

// POST /fs/upload {data_base64, name?} — save an uploaded file (e.g. an image the
// user posts from the app) to a temp dir and return its absolute path, which the
// agent can then view with `read_image` (or read).
pub(super) async fn upload_fs_file(State(d): State<Shared>, Query(a): Query<Auth>, Json(req): Json<UploadReq>) -> Response {
    if !d.authed(&a.token) {
        return unauthorized();
    }
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(req.data_base64.trim()) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad base64: {e}")).into_response(),
    };
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty upload").into_response();
    }
    // Targeted upload into a directory under the original filename, or (default)
    // a temp dir with a generated name (for chat attachments).
    let path = if let Some(dir) = req.dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let name = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).file_name())
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return (StatusCode::BAD_REQUEST, "name required when uploading to a directory").into_response();
        };
        let p = PathBuf::from(dir).join(name);
        if p.exists() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok": false, "error": "a file with that name already exists"})),
            )
                .into_response();
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        p
    } else {
        let ext = req
            .name
            .as_deref()
            .and_then(|n| std::path::Path::new(n).extension())
            .and_then(|e| e.to_str())
            .filter(|e| e.len() <= 5)
            .unwrap_or("png");
        let dir = std::env::temp_dir().join("snippet-uploads");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        dir.join(format!("{}.{ext}", uuid::Uuid::new_v4().simple()))
    };
    if let Err(e) = std::fs::write(&path, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(serde_json::json!({"path": path.display().to_string(), "size": bytes.len()})).into_response()
}
