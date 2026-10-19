use super::*;

const TAILSCALE_HTTPS_PORT: u16 = 8443;

pub(super) enum Running {
    Process(tokio::process::Child),
    Tailscale { bin: PathBuf, funnel: bool },
    Nothing,
}

impl Running {
    pub(super) async fn stop(self) {
        match self {
            Running::Process(mut child) => {
                let _ = child.start_kill();
            }
            Running::Tailscale { bin, funnel } => {
                let _ = tokio::process::Command::new(&bin)
                    .args([
                        if funnel { "funnel" } else { "serve" },
                        &format!("--https={TAILSCALE_HTTPS_PORT}"),
                        "off",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
            Running::Nothing => {}
        }
    }
}

pub(super) async fn open(tunnel: Tunnel, port: u16) -> Result<(String, Running), String> {
    let local = format!("http://127.0.0.1:{port}");
    match tunnel {
        Tunnel::Url(u) => Ok((u, Running::Nothing)),
        Tunnel::None => Ok((local, Running::Nothing)),
        Tunnel::Cloudflare => {
            let bin = find_cloudflared().ok_or(
                "cloudflared is not installed. Install it (`brew install cloudflared`, or https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) and try again.",
            )?;
            start_cloudflared_quick(&bin, port).await
        }
        Tunnel::Tailscale { funnel } => {
            let bin = find_tailscale().ok_or(
                "Tailscale is not installed. Install it from https://tailscale.com/download, run `tailscale up`, and try again.",
            )?;
            let dns = tailscale_dns_name(&bin).await.ok_or_else(|| {
                "Tailscale is installed but not connected. Run `tailscale up` and try again."
                    .to_string()
            })?;
            start_tailscale(&bin, &dns, port, funnel).await
        }
        Tunnel::Auto => {
            if let Some(bin) = find_tailscale() {
                if let Some(dns) = tailscale_dns_name(&bin).await {
                    match start_tailscale(&bin, &dns, port, false).await {
                        Ok(up) => return Ok(up),
                        Err(e) => println!("tailscale serve did not start: {e}"),
                    }
                }
            }
            if let Some(bin) = find_cloudflared() {
                return start_cloudflared_quick(&bin, port).await;
            }
            println!("{}", setup_help(port));
            Ok((local, Running::Nothing))
        }
    }
}

pub fn setup_help(port: u16) -> String {
    format!(
        "No tunnel is set up, so the daemon is only reachable from this machine (http://127.0.0.1:{port}).
To reach it from your phone, set up one of these, then run `snippet serve --restart`:
  Tailscale    install it from https://tailscale.com/download on this machine and your phone,
               then run `tailscale up`. Private to your tailnet, and the address never changes.
  Cloudflare   install cloudflared (`brew install cloudflared`, or
               https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/).
               A public https link that changes on every restart.
Pick one explicitly with `snippet serve --tunnel tailscale|funnel|cloudflare`, or bring your own with --public-url."
    )
}

fn runs(bin: &std::path::Path, arg: &str) -> bool {
    std::process::Command::new(bin)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn first_working(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|bin| runs(bin, "version")).cloned()
}

fn find_cloudflared() -> Option<PathBuf> {
    first_working(&[
        PathBuf::from("cloudflared"),
        home_dir().join(".snippet").join("bin").join("cloudflared"),
        PathBuf::from("/opt/homebrew/bin/cloudflared"),
        PathBuf::from("/usr/local/bin/cloudflared"),
    ])
}

fn find_tailscale() -> Option<PathBuf> {
    first_working(&[
        PathBuf::from("tailscale"),
        PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
        PathBuf::from("/opt/homebrew/bin/tailscale"),
        PathBuf::from("/usr/local/bin/tailscale"),
    ])
}

async fn tailscale_dns_name(bin: &std::path::Path) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(bin)
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if v.get("BackendState").and_then(|s| s.as_str()) != Some("Running") {
        return None;
    }
    let dns = v.pointer("/Self/DNSName")?.as_str()?.trim_end_matches('.');
    (!dns.is_empty()).then(|| dns.to_string())
}

async fn start_tailscale(
    bin: &std::path::Path,
    dns: &str,
    port: u16,
    funnel: bool,
) -> Result<(String, Running), String> {
    let verb = if funnel { "funnel" } else { "serve" };
    let run = tokio::process::Command::new(bin)
        .args([
            verb,
            "--bg",
            &format!("--https={TAILSCALE_HTTPS_PORT}"),
            &format!("http://127.0.0.1:{port}"),
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .map_err(|_| {
            format!(
                "`tailscale {verb}` is waiting on something it needs (usually HTTPS or Funnel switched on for your tailnet). Run `tailscale {verb} --bg --https={TAILSCALE_HTTPS_PORT} http://127.0.0.1:{port}` once by hand to see the link."
            )
        })?
        .map_err(|e| format!("launch tailscale: {e}"))?;
    if !out.status.success() {
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let hint = if said.contains("Access denied") || said.contains("permission") {
            "\nOn Linux, let your user manage Tailscale once: `sudo tailscale set --operator=$USER`."
        } else {
            ""
        };
        return Err(format!("tailscale {verb}: {}{hint}", said.trim()));
    }
    Ok((
        format!("https://{dns}:{TAILSCALE_HTTPS_PORT}"),
        Running::Tailscale {
            bin: bin.to_path_buf(),
            funnel,
        },
    ))
}

/// Launch `cloudflared tunnel --url` and capture the printed `*.trycloudflare.com`
/// URL. cloudflared's output goes to a log FILE (not a parent pipe): if we piped it
/// and stopped reading, the pipe would fill and cloudflared would die on SIGPIPE,
/// killing the tunnel. The returned child must be kept alive for the tunnel to serve.
async fn start_cloudflared_quick(
    bin: &std::path::Path,
    port: u16,
) -> Result<(String, Running), String> {
    let _ = std::fs::create_dir_all(snippet_dir());
    let log = snippet_dir().join("cloudflared.log");
    let _ = std::fs::remove_file(&log);
    let out = std::fs::File::create(&log).map_err(|e| format!("cloudflared log: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(bin)
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://localhost:{port}"),
        ])
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("launch cloudflared: {e}"))?;

    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&log) {
            if let Some(url) = extract_trycloudflare_url(&content) {
                return Ok((url, Running::Process(child)));
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = child.start_kill();
    Err("timed out waiting for the cloudflared URL".to_string())
}

fn extract_trycloudflare_url(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(i) = line.find("https://") {
            let url: String = line[i..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if url.contains("trycloudflare.com") {
                return Some(url.trim_end_matches(['.', ',']).to_string());
            }
        }
    }
    None
}
