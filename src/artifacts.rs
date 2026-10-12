//! Artifacts: deliverables the agent keeps iterating on, stored per session
//! with their versions so the user can view the latest and step back.
//!
//! ```text
//! ~/.snippet/artifacts/<session dir>/<id>/manifest.json
//! ~/.snippet/artifacts/<session dir>/<id>/v<N>/<entry and its files>
//! ```
//!
//! A version is a copy of the presented file, or of the presented folder. A
//! folder without an `index.html` is a collection and gets a generated index
//! page. Pages are served from signed `/a/...` links that never carry the
//! daemon token, under a CSP sandbox that gives each page an opaque origin.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const KEEP_VERSIONS: usize = 10;
const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub n: u32,
    pub created_at: i64,
    pub entry: String,
    pub hash: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub title: String,
    pub kind: String,
    #[serde(default)]
    pub caption: Option<String>,
    pub versions: Vec<Version>,
}

impl Artifact {
    pub fn latest(&self) -> Option<&Version> {
        self.versions.last()
    }
}

pub struct Presented {
    pub artifact: Artifact,
    pub version: u32,
    pub unchanged: bool,
}

pub fn root() -> PathBuf {
    crate::config::snippet_home().join("artifacts")
}

pub fn session_dir_name(session: &str) -> String {
    crate::memory::session_dir_name(session)
}

fn session_dir(session: &str) -> PathBuf {
    root().join(session_dir_name(session))
}

fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

pub fn version_dir(session_dir: &str, id: &str, n: u32) -> Option<PathBuf> {
    (safe_component(session_dir) && safe_component(id))
        .then(|| root().join(session_dir).join(id).join(format!("v{n}")))
}

fn load(dir: &Path) -> Option<Artifact> {
    serde_json::from_str(&fs::read_to_string(dir.join(MANIFEST)).ok()?).ok()
}

fn save(dir: &Path, artifact: &Artifact) -> Result<(), String> {
    let text = serde_json::to_string_pretty(artifact).map_err(|e| e.to_string())?;
    fs::write(dir.join(MANIFEST), text).map_err(|e| format!("write manifest: {e}"))
}

pub fn list(session: &str) -> Vec<Artifact> {
    let Ok(entries) = fs::read_dir(session_dir(session)) else {
        return Vec::new();
    };
    let mut out: Vec<Artifact> = entries.flatten().filter_map(|e| load(&e.path())).collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.latest().map(|v| v.created_at).unwrap_or(0)));
    out
}

pub fn get(session: &str, id: &str) -> Option<Artifact> {
    safe_component(id).then(|| load(&session_dir(session).join(id))).flatten()
}

pub fn delete_session(session: &str) {
    let _ = fs::remove_dir_all(session_dir(session));
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "artifact".to_string() } else { out }
}

pub fn kind_of(path: &Path) -> &'static str {
    if path.is_dir() {
        return if path.join("index.html").is_file() { "html" } else { "collection" };
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "html",
        "md" | "markdown" | "mdx" => "markdown",
        "svg" => "svg",
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image",
        "csv" | "tsv" => "csv",
        "pdf" => "pdf",
        "json" | "yaml" | "yml" | "toml" | "rs" | "dart" | "py" | "js" | "ts" | "tsx" | "go"
        | "sh" | "sql" | "txt" | "diff" | "patch" => "code",
        _ => "file",
    }
}

fn hash_path(path: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    if path.is_dir() {
        let mut files = Vec::new();
        collect(path, path, &mut files);
        files.sort();
        for rel in files {
            hasher.update(rel.as_bytes());
            hasher.update(fs::read(path.join(&rel)).map_err(|e| format!("read {rel}: {e}"))?);
        }
    } else {
        hasher.update(fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?);
    }
    Ok(hasher.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect())
}

fn collect(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if path.is_dir() {
            collect(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

fn copy_into(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut files = Vec::new();
    collect(source, source, &mut files);
    for rel in files {
        let to = dest.join(&rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::copy(source.join(&rel), &to).map_err(|e| format!("copy {rel}: {e}"))?;
    }
    Ok(())
}

/// Store `source` as a new version of an artifact: a new one titled `title`,
/// or the next version of `id`. Content identical to the latest version adds
/// nothing.
pub fn present(
    session: &str,
    source: &Path,
    title: Option<&str>,
    id: Option<&str>,
    caption: Option<String>,
) -> Result<Presented, String> {
    let base = session_dir(session);
    let mut artifact = match id {
        Some(id) => get(session, id).ok_or_else(|| {
            format!("no artifact `{id}` in this session; leave out artifact_id to create a new one")
        })?,
        None => {
            let stem = source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("artifact");
            let title = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or(stem);
            let mut id = slug(title);
            let mut n = 2;
            while base.join(&id).exists() {
                id = format!("{}-{n}", slug(title));
                n += 1;
            }
            Artifact {
                id,
                title: title.to_string(),
                kind: kind_of(source).to_string(),
                caption: None,
                versions: Vec::new(),
            }
        }
    };
    if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
        artifact.title = title.to_string();
    }
    if caption.is_some() {
        artifact.caption = caption;
    }
    let hash = hash_path(source)?;
    if let Some(last) = artifact.latest().filter(|v| v.hash == hash) {
        let version = last.n;
        return Ok(Presented { artifact, version, unchanged: true });
    }
    let n = artifact.latest().map(|v| v.n + 1).unwrap_or(1);
    let dir = base.join(&artifact.id);
    let dest = dir.join(format!("v{n}"));
    let entry = if source.is_dir() {
        copy_into(source, &dest)?;
        if !dest.join("index.html").is_file() {
            fs::write(dest.join("index.html"), collection_index(&artifact.title, &dest))
                .map_err(|e| format!("write index: {e}"))?;
        }
        "index.html".to_string()
    } else {
        fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
        let name = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        fs::copy(source, dest.join(&name)).map_err(|e| format!("copy: {e}"))?;
        name
    };
    artifact.kind = kind_of(source).to_string();
    artifact.versions.push(Version {
        n,
        created_at: chrono::Utc::now().timestamp(),
        entry,
        hash,
        source: source.display().to_string(),
    });
    while artifact.versions.len() > KEEP_VERSIONS {
        let old = artifact.versions.remove(0);
        let _ = fs::remove_dir_all(dir.join(format!("v{}", old.n)));
    }
    save(&dir, &artifact)?;
    Ok(Presented { artifact, version: n, unchanged: false })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn collection_index(title: &str, dir: &Path) -> String {
    let mut files = Vec::new();
    collect(dir, dir, &mut files);
    files.sort();
    let mut images = String::new();
    let mut others = String::new();
    for rel in &files {
        let href = rel
            .split('/')
            .map(|p| p.replace('%', "%25").replace(' ', "%20").replace('#', "%23").replace('?', "%3F"))
            .collect::<Vec<_>>()
            .join("/");
        let name = escape(rel);
        let image = matches!(
            Path::new(rel).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg")
        );
        if image {
            images.push_str(&format!(
                "<a class=\"tile\" href=\"{href}\"><img src=\"{href}\" alt=\"{name}\" loading=\"lazy\"><span>{name}</span></a>"
            ));
        } else {
            others.push_str(&format!("<li><a href=\"{href}\">{name}</a></li>"));
        }
    }
    let title = escape(title);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>\
:root{{color-scheme:light dark;--bg:#fff;--fg:#1a1a1a;--muted:#6b6b6b;--line:#e6e6e6}}\
@media (prefers-color-scheme:dark){{:root{{--bg:#121110;--fg:#ece9e4;--muted:#8f8a83;--line:#2a2826}}}}\
body{{margin:0;padding:24px 16px;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}}\
h1{{font-size:20px;margin:0 0 4px}}p{{color:var(--muted);margin:0 0 20px}}\
.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(160px,1fr));gap:12px}}\
.tile{{display:block;color:inherit;text-decoration:none;border:1px solid var(--line);border-radius:10px;overflow:hidden}}\
.tile img{{display:block;width:100%;aspect-ratio:1;object-fit:cover;background:var(--line)}}\
.tile span{{display:block;padding:6px 8px;font-size:12px;color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}\
ul{{padding-left:18px}}a{{color:inherit}}</style></head><body>\
<h1>{title}</h1><p>{} files</p><div class=\"grid\">{images}</div><ul>{others}</ul></body></html>",
        files.len()
    )
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn signature(secret: &str, scope: &str, exp: i64) -> String {
    hmac_sha256(secret.as_bytes(), format!("artifact:{scope}:{exp}").as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A link to one version's entry, valid for `ttl_secs`: `/a/<exp>/<sig>/<session dir>/<id>/v<n>/<entry>`.
pub fn signed_path(secret: &str, session: &str, id: &str, version: &Version, ttl_secs: i64) -> String {
    let exp = chrono::Utc::now().timestamp() + ttl_secs;
    let scope = format!("{}/{id}/v{}", session_dir_name(session), version.n);
    let entry = version
        .entry
        .split('/')
        .map(|p| p.replace('%', "%25").replace(' ', "%20").replace('#', "%23").replace('?', "%3F"))
        .collect::<Vec<_>>()
        .join("/");
    format!("/a/{exp}/{}/{scope}/{entry}", signature(secret, &scope, exp))
}

/// Resolve a signed link's remainder (`<session dir>/<id>/v<n>/<file>`) to a
/// file on disk, or None when the signature, expiry or path doesn't hold.
pub fn resolve_signed(secret: &str, exp: i64, sig: &str, rest: &str) -> Option<PathBuf> {
    if exp < chrono::Utc::now().timestamp() {
        return None;
    }
    let mut parts = rest.splitn(4, '/');
    let session_dir = parts.next()?;
    let id = parts.next()?;
    let version = parts.next()?;
    let file = parts.next().unwrap_or("index.html");
    let n: u32 = version.strip_prefix('v')?.parse().ok()?;
    let scope = format!("{session_dir}/{id}/v{n}");
    let expected = signature(secret, &scope, exp);
    if expected.len() != sig.len() || expected.bytes().zip(sig.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    let dir = version_dir(session_dir, id, n)?;
    if file.split('/').any(|p| p.is_empty() || p == ".." || p == "." || p.starts_with('.')) {
        return None;
    }
    let path = dir.join(file);
    let path = if path.is_dir() { path.join("index.html") } else { path };
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(dir.canonicalize().ok()?).then_some(canonical)
}

pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "md" | "markdown" | "txt" | "csv" | "tsv" | "rs" | "py" | "ts" | "dart" | "go" | "sh"
        | "toml" | "yaml" | "yml" | "sql" | "diff" | "patch" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The policy every artifact page is served under: a sandbox without
/// same-origin, so the page runs in an opaque origin and can't reach the
/// daemon's API, plus a short list of CDNs for scripts, styles and fonts.
pub const CSP: &str = "sandbox allow-scripts allow-popups allow-forms allow-modals allow-downloads; \
default-src 'self' 'unsafe-inline' data: blob: https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com https://fonts.googleapis.com https://fonts.gstatic.com; \
img-src 'self' data: blob: https:; connect-src 'self'; frame-ancestors *";

/// Zip `sources` (files, or folders kept as folders) into a temporary file
/// for download, returning its path. Zips older than an hour are cleared.
pub fn zip_to_temp(name: &str, sources: &[PathBuf]) -> Result<PathBuf, String> {
    use std::io::Write;
    let dir = crate::config::snippet_home().join("tmp").join("zips");
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    if let Ok(entries) = fs::read_dir(&dir) {
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for entry in entries.flatten() {
            if entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < hour_ago) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let stem = slug(name.trim_end_matches(".zip"));
    let out = dir.join(format!("{stem}-{}.zip", chrono::Utc::now().timestamp_millis()));
    let file = fs::File::create(&out).map_err(|e| format!("create zip: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut used = std::collections::HashSet::new();
    let mut add = |zip: &mut zip::ZipWriter<fs::File>, entry: String, path: &Path| -> Result<(), String> {
        let mut entry = entry;
        let mut n = 2;
        while !used.insert(entry.clone()) {
            entry = match entry.rsplit_once('.') {
                Some((base, ext)) if !base.is_empty() => format!("{base}-{n}.{ext}"),
                _ => format!("{entry}-{n}"),
            };
            n += 1;
        }
        zip.start_file(entry, options).map_err(|e| e.to_string())?;
        zip.write_all(&fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?)
            .map_err(|e| e.to_string())
    };
    for source in sources {
        let base = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        if source.is_dir() {
            let mut files = Vec::new();
            collect(source, source, &mut files);
            files.sort();
            for rel in files {
                add(&mut zip, format!("{base}/{rel}"), &source.join(&rel))?;
            }
        } else if source.is_file() {
            add(&mut zip, base, source)?;
        }
    }
    zip.finish().map_err(|e| format!("finish zip: {e}"))?;
    Ok(out)
}
