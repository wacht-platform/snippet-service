//! Self-update — check the latest GitHub release and, if it's newer, replace the
//! running binary in place. Best-effort throughout: any network / parse / write
//! failure leaves the current binary untouched. Disabled by `SNIPPET_NO_UPDATE`.
//!
//! The TUI runs a one-shot check at startup (`check_and_update`) and, on success,
//! shows a "restart to apply" notice. The serve daemon checks periodically and,
//! when supervised, restarts itself so the new code takes effect (see `serve`).

use std::path::Path;

const REPO: &str = "wacht-platform/snippet-service";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const UA: &str = concat!("snippet/", env!("CARGO_PKG_VERSION"), " (self-update)");

/// Whether auto-update is turned off for this process.
pub fn disabled() -> bool {
    std::env::var_os("SNIPPET_NO_UPDATE").is_some()
}

/// The release asset for THIS build's platform, or None when we ship no prebuilt
/// binary for it (auto-update is then a no-op).
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("snippet-x86_64-unknown-linux-gnu.tar.gz"),
        ("macos", "aarch64") => Some("snippet-aarch64-apple-darwin.tar.gz"),
        _ => None,
    }
}

/// Latest published version (tag with any leading `v` stripped), or None if the
/// network / API doesn't cooperate.
pub async fn latest_version(client: &reqwest::Client) -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = client.get(url).header(reqwest::header::USER_AGENT, UA).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    Some(tag.trim().trim_start_matches('v').to_string())
}

/// Whether `latest` is a strictly newer semver than the running build.
pub fn is_newer(latest: &str) -> bool {
    match (parse(latest), parse(CURRENT_VERSION)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Download `version`'s binary for this platform and atomically replace the
/// running executable. The new code takes effect on the next launch.
pub async fn download_and_replace(client: &reqwest::Client, version: &str) -> Result<(), String> {
    let asset = asset_name().ok_or("no prebuilt binary for this platform")?;
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/{asset}");
    let bytes = client
        .get(url)
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    let bin = extract_binary(&bytes)?;
    // A real binary is multi-MB and starts with a known magic — guard against a
    // truncated download or an HTML error page slipping through.
    if bin.len() < 1_000_000 || !looks_like_executable(&bin) {
        return Err("downloaded artifact doesn't look like a snippet binary".to_string());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    replace_exe(&exe, &bin)
}

/// One-shot: if a newer release exists, update the binary in place. Returns the
/// new version when an update was applied. No-op when disabled.
pub async fn check_and_update(client: &reqwest::Client) -> Option<String> {
    if disabled() {
        return None;
    }
    let latest = latest_version(client).await?;
    if !is_newer(&latest) {
        return None;
    }
    download_and_replace(client, &latest).await.ok()?;
    Some(latest)
}

fn looks_like_executable(b: &[u8]) -> bool {
    b.starts_with(&[0x7f, b'E', b'L', b'F'])        // ELF (Linux)
        || b.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) // Mach-O 64 (macOS)
        || b.starts_with(&[0xce, 0xfa, 0xed, 0xfe]) // Mach-O 32
        || b.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) // Mach-O universal
}

fn extract_binary(gz: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut archive = tar::Archive::new(GzDecoder::new(gz));
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let is_snippet = entry
            .path()
            .ok()
            .map(|p| p.file_name().and_then(|n| n.to_str()) == Some("snippet"))
            .unwrap_or(false);
        if is_snippet {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            return Ok(buf);
        }
    }
    Err("`snippet` not found in the release archive".to_string())
}

fn replace_exe(exe: &Path, new_bytes: &[u8]) -> Result<(), String> {
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    // Temp in the same dir so the rename stays on one filesystem (atomic).
    let tmp = dir.join(".snippet-update.tmp");
    std::fs::write(&tmp, new_bytes).map_err(|e| format!("write temp: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    // Replacing a running binary is safe on Unix: the running process keeps the
    // old inode open; the path now resolves to the new file for the next launch.
    std::fs::rename(&tmp, exe).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace binary: {e}")
    })
}

