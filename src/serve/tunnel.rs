use super::*;

/// Launch `cloudflared tunnel --url` and capture the printed `*.trycloudflare.com`
/// URL. cloudflared's output goes to a log FILE (not a parent pipe): if we piped it
/// and stopped reading, the pipe would fill and cloudflared would die on SIGPIPE,
/// killing the tunnel. The returned child must be kept alive for the tunnel to serve.
pub(super) async fn start_cloudflared_quick(
    bin: &std::path::Path,
    port: u16,
) -> Result<(String, tokio::process::Child), String> {
    let _ = std::fs::create_dir_all(snippet_dir());
    let log = snippet_dir().join("cloudflared.log");
    let _ = std::fs::remove_file(&log);
    let out = std::fs::File::create(&log).map_err(|e| format!("cloudflared log: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(bin)
        .args(["tunnel", "--no-autoupdate", "--url", &format!("http://localhost:{port}")])
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("launch cloudflared: {e}"))?;

    // ~30s: poll the log file for the assigned URL while cloudflared keeps running.
    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&log) {
            if let Some(url) = extract_trycloudflare_url(&content) {
                return Ok((url, child));
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = child.start_kill();
    Err("timed out waiting for the cloudflared URL".to_string())
}

/// Pull the first `https://*.trycloudflare.com` URL out of cloudflared's log output.
fn extract_trycloudflare_url(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(i) = line.find("https://") {
            let url: String =
                line[i..].chars().take_while(|c| !c.is_whitespace()).collect();
            if url.contains("trycloudflare.com") {
                return Some(url.trim_end_matches(['.', ',']).to_string());
            }
        }
    }
    None
}

/// Locate a usable cloudflared: prefer one on PATH, else a cached copy under
/// `~/.snippet/bin`, else download the official static binary for this OS/arch.
pub(super) async fn ensure_cloudflared() -> Result<PathBuf, String> {
    if std::process::Command::new("cloudflared")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(PathBuf::from("cloudflared"));
    }
    let bin = bin_dir().join("cloudflared");
    if bin.exists() {
        return Ok(bin);
    }
    download_cloudflared(&bin).await?;
    Ok(bin)
}

fn bin_dir() -> PathBuf {
    home_dir().join(".snippet").join("bin")
}

/// The cloudflared release asset for this platform (verified naming: Linux ships a
/// raw binary, macOS a `.tgz`).
fn cloudflared_asset() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("cloudflared-darwin-arm64.tgz"),
        ("macos", "x86_64") => Ok("cloudflared-darwin-amd64.tgz"),
        ("linux", "x86_64") => Ok("cloudflared-linux-amd64"),
        ("linux", "aarch64") => Ok("cloudflared-linux-arm64"),
        (os, arch) => Err(format!("no cloudflared build for {os}/{arch} — install it manually")),
    }
}

/// Fetch cloudflared in the foreground (with a progress bar) before detaching, so
/// the one-time download is visible. The detached child then finds it cached and
/// returns instantly. Runs on a current-thread runtime so no worker threads exist
/// across the subsequent fork.
pub fn ensure_cloudflared_foreground() -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(ensure_cloudflared())?;
    Ok(())
}

/// Redraw the single-line download progress bar in place (driven by chunk arrival).
fn draw_download_progress(spinner: char, done: u64, total: Option<u64>) {
    use std::io::Write;
    let mb = |b: u64| b as f64 / 1_000_000.0;
    let line = match total.filter(|t| *t > 0) {
        Some(t) => {
            let frac = (done as f64 / t as f64).min(1.0);
            let width = 24usize;
            let filled = (frac * width as f64).round() as usize;
            let bar: String = (0..width)
                .map(|i| if i < filled { '█' } else { '░' })
                .collect();
            format!(
                "\r  {spinner} cloudflared  {:>5.1} / {:>5.1} MB  [{bar}] {:>3.0}%",
                mb(done),
                mb(t),
                frac * 100.0
            )
        }
        None => format!("\r  {spinner} cloudflared  {:.1} MB", mb(done)),
    };
    print!("{line}");
    let _ = std::io::stdout().flush();
}

/// Fetch the official cloudflared static binary into `dest` (one-time, ~35 MB).
async fn download_cloudflared(dest: &std::path::Path) -> Result<(), String> {
    use futures_util::StreamExt;

    let asset = cloudflared_asset()?;
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    println!("  Fetching cloudflared (one-time) for {}…", std::env::consts::OS);

    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download cloudflared: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download cloudflared: {e}"))?;
    let total = resp.content_length();
    let mut stream = resp.bytes_stream();
    let mut bytes: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut tick = 0usize;
    draw_download_progress(FRAMES[0], 0, total);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download cloudflared: {e}"))?;
        bytes.extend_from_slice(&chunk);
        tick = tick.wrapping_add(1);
        draw_download_progress(FRAMES[tick % FRAMES.len()], bytes.len() as u64, total);
    }
    println!("\r\x1b[2K  ✓ cloudflared downloaded ({:.1} MB)", bytes.len() as f64 / 1_000_000.0);

    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    if asset.ends_with(".tgz") {
        // macOS: extract the `cloudflared` binary from the tarball via system `tar`.
        let tgz = dir.join("cloudflared.tgz");
        std::fs::write(&tgz, &bytes).map_err(|e| e.to_string())?;
        let ok = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(&tgz)
            .arg("-C")
            .arg(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&tgz);
        if !ok || !dest.exists() {
            return Err("failed to extract cloudflared from the .tgz".to_string());
        }
    } else {
        std::fs::write(dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}
