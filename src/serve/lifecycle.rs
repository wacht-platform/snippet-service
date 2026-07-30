use super::*;

/// File lock for coordinating binary updates. The running serve daemon holds a
/// SHARED lock on this file while the installer takes an EXCLUSIVE lock to atomically
/// write a new binary hash. On startup, serve compares the stored hash against the
/// running binary — a mismatch means "I was replaced, restart me."
pub fn lock_path() -> PathBuf {
    snippet_dir().join("serve.lock")
}

/// Acquire a shared flock on the lock file. Returns the file handle (lock is held
/// for the lifetime of the process). Used by the running serve daemon.
pub fn acquire_shared_lock() -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let dir = snippet_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path())
        .ok()?;
    unsafe {
        let ret = libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB);
        if ret == 0 { Some(file) } else { None }
    }
}

/// Write the current binary's hash into the lock file. A process that has just
/// launched is already running the selected binary, so any older hash is stale.
pub fn stamp_binary_hash() {
    let Some(hash) = compute_binary_hash() else {
        return;
    };
    let _ = write_binary_hash(&lock_path(), &hash);
}

fn write_binary_hash(path: &std::path::Path, hash: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX);
    }
    file.set_len(0)?;
    file.write_all(hash.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn compute_binary_hash() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bytes = std::fs::read(&exe).ok()?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

pub(super) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

// --- background daemon lifecycle ---

pub(super) fn snippet_dir() -> PathBuf {
    home_dir().join(".snippet")
}
pub(super) fn pid_path() -> PathBuf {
    snippet_dir().join("serve.pid")
}
fn log_path() -> PathBuf {
    snippet_dir().join("serve.log")
}
pub(super) fn state_json_path() -> PathBuf {
    snippet_dir().join("serve.json")
}

/// Binary modification stamp — used to detect updates and auto-restart.
fn binary_stamp_path() -> PathBuf {
    snippet_dir().join("serve.binary.stamp")
}

/// Check if the binary has been updated since the last serve start.
/// Returns true if the binary is newer than the stored stamp (or no stamp exists).
pub fn binary_updated() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let mtime = match std::fs::metadata(&exe).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    // No stamp → first run, don't restart.
    let Ok(stamp) = std::fs::read_to_string(binary_stamp_path()) else {
        return false;
    };
    let stamp: std::time::SystemTime = match stamp.trim().parse::<u64>().ok() {
        Some(secs) => std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        None => return false,
    };
    mtime > stamp
}

/// Record the current binary's mtime so next startup can detect updates.
pub fn stamp_binary() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let Ok(mtime) = std::fs::metadata(&exe).and_then(|m| m.modified()) else {
        return;
    };
    let Ok(secs) = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
    else {
        return;
    };
    let _ = std::fs::create_dir_all(snippet_dir());
    let _ = std::fs::write(binary_stamp_path(), secs.to_string());
}

/// Persist the live connection (url + token) so the launching parent and
/// `serve --status` can reprint the QR. Written 0600 — it holds the auth token.
pub(super) fn write_serve_state(public_url: &str, token: &str, api_host: &str, api_port: u16) {
    let _ = std::fs::create_dir_all(snippet_dir());
    let payload = serde_json::json!({
        "url": public_url,
        "token": token,
        "pid": std::process::id(),
        "api_url": format!("http://{api_host}:{api_port}"),
    });
    let path = state_json_path();
    if std::fs::write(&path, payload.to_string()).is_ok() {
        crate::config::set_private(&path);
    }
}

/// Fully detach the current process into the background (double-fork + setsid via
/// the `daemonize` crate, output to the log). Run by the spawned worker before it
/// builds the runtime and serves.
pub fn daemonize_self() -> Result<(), String> {
    std::fs::create_dir_all(snippet_dir()).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_path()).map_err(|e| format!("open log: {e}"))?;
    // The log can carry sensitive runtime output — owner-only like the token file.
    crate::config::set_private(&log_path());
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    daemonize::Daemonize::new()
        .pid_file(pid_path())
        .working_directory(home_dir())
        .stdout(daemonize::Stdio::from(log))
        .stderr(daemonize::Stdio::from(log2))
        .start()
        .map_err(|e| format!("daemonize: {e}"))
}

/// Foreground launcher (what `snippet serve` runs): spawn the detached worker, wait
/// for it to publish the connection, print the QR here, then exit. This is why the
/// QR shows on the terminal even though the server runs in the background.
pub fn launch_and_show(
    host: &str,
    port: u16,
    token: &str,
    no_tunnel: bool,
    public_url: Option<String>,
    config_path: &std::path::Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    if let Some(pid) = running_pid() {
        return Err(format!(
            "snippet serve is already running (pid {pid}). `snippet serve --stop` first."
        ));
    }
    std::fs::create_dir_all(snippet_dir()).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(state_json_path());

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Hand the token to the worker via the 0600 token file, NEVER argv — the
    // command line of the long-lived daemon is world-readable (`ps`,
    // /proc/<pid>/cmdline) on multi-user machines. `resolve_token` in the worker
    // reads the file back.
    persist_token(token);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config")
        .arg(config_path)
        .arg("serve")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string());
    if no_tunnel {
        cmd.arg("--no-tunnel");
    }
    if let Some(u) = &public_url {
        cmd.arg("--public-url").arg(u);
    }
    // The worker re-enters `serve` with this marker set and runs the server; we (the
    // launcher) stay alive to print the QR. The worker redirects output to the log.
    cmd.env("__SNIPPET_SERVE_WORKER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    cmd.spawn().map_err(|e| format!("spawn worker: {e}"))?;

    println!("\n  snippet serve — bringing up the tunnel…");
    for _ in 0..120 {
        if let Some((url, tok)) = read_serve_state() {
            print_connection(&url, &tok);
            println!("  Running in the background.  stop: snippet serve --stop\n");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!(
        "the server didn't come up within 60s — check the log: {}",
        log_path().display()
    ))
}

/// Resolves on SIGTERM/SIGINT; pends forever if the handlers can't be installed
/// (so the server arm of the select still wins).
pub(super) async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut intr) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        _ => return std::future::pending().await,
    };
    tokio::select! {
        _ = term.recv() => {},
        _ = intr.recv() => {},
    }
}

fn read_serve_state() -> Option<(String, String)> {
    let bytes = std::fs::read(state_json_path()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some((
        v["url"].as_str()?.to_string(),
        v["token"].as_str()?.to_string(),
    ))
}

/// The running daemon's pid, if the pidfile points at a live process that is
/// actually a snippet binary. The pidfile survives crashes and reboots, and pids
/// get recycled — without the identity check, `--stop` would SIGTERM whatever
/// unrelated process inherited the number, and `serve` would refuse to start.
fn running_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let comm = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !comm.status.success() {
        return None; // no such process
    }
    let name = String::from_utf8_lossy(&comm.stdout).trim().to_string();
    // `comm` is the executable name (possibly truncated by the kernel) — match on
    // the binary name, not the full path.
    let is_snippet = std::path::Path::new(&name)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("snippet"))
        .unwrap_or(false);
    is_snippet.then_some(pid)
}

/// Stop the background daemon (kills its whole process group → tunnel too).
pub fn stop() -> Result<(), String> {
    let Some(pid) = running_pid() else {
        let _ = std::fs::remove_file(pid_path());
        return Err("snippet serve is not running".to_string());
    };
    // SIGTERM the daemon; its handler tears down the tunnel before exiting.
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(state_json_path());
    println!("stopped snippet serve (pid {pid})");
    Ok(())
}

/// Print the current daemon status + reprint the QR/connection if it's running.
pub fn status() -> Result<(), String> {
    match (running_pid(), read_serve_state()) {
        (Some(pid), Some((url, token))) => {
            println!("snippet serve running (pid {pid})");
            print_connection(&url, &token);
            Ok(())
        }
        (Some(pid), None) => {
            println!("snippet serve running (pid {pid}) — connection not published yet");
            Ok(())
        }
        (None, _) => {
            println!("snippet serve is not running");
            Ok(())
        }
    }
}

/// Print the QR + connection string the mobile app scans/pastes: a JSON payload
/// `{url, token}` (the app derives wss/https from the URL).
fn print_connection(public_url: &str, token: &str) {
    let connection = connection_string(public_url, token);
    println!("\n  Scan the QR in the snippet app, or paste this connection string:\n");
    if let Ok(code) = qrcode::QrCode::new(connection.as_bytes()) {
        let rendered = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{rendered}");
    }
    println!("  {connection}\n");
}

/// The single connection string the app pastes/scans: the public URL carrying the
/// auth token as a query param (e.g. https://host/?token=abc).
fn connection_string(public_url: &str, token: &str) -> String {
    let sep = if public_url.contains('?') { '&' } else { '?' };
    format!("{public_url}{sep}token={token}")
}

/// Resolve the auth token: an explicit one wins; else reuse the persisted token so
/// restarts keep the same token (only the tunnel URL changes); else generate + save.
pub fn resolve_token(explicit: Option<String>) -> String {
    if let Some(t) = explicit.filter(|s| !s.trim().is_empty()) {
        return t;
    }
    let path = snippet_dir().join("serve.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 16 {
            return t;
        }
    }
    let t = uuid::Uuid::new_v4().simple().to_string();
    let _ = std::fs::create_dir_all(snippet_dir());
    if std::fs::write(&path, &t).is_ok() {
        crate::config::set_private(&path);
    }
    t
}

// --- auto-start service (launchd / systemd) ---

/// Supervised mode doesn't daemonize, so record our pid here so `serve --status`
/// can find us (the service manager owns actual lifecycle/restart).
pub fn write_own_pidfile() {
    let _ = std::fs::create_dir_all(snippet_dir());
    let _ = std::fs::write(pid_path(), std::process::id().to_string());
}

/// Persisted serve runtime config (`~/.config/snippet/serve.toml`). The auto-start
/// service reads this on every boot rather than having the settings frozen into
/// its plist/unit — so re-running `--enable` (or hand-editing this one file)
/// changes how the daemon comes up without touching the service definition.
/// Absent/empty file → the daemon starts with CLI defaults.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ServeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(default)]
    pub no_tunnel: bool,
}

/// XDG-style config dir, shared by the CLI and the auto-start service so both
/// agree on one location regardless of working directory. `$XDG_CONFIG_HOME` wins
/// (Linux/systemd convention); otherwise `~/.config/snippet` (also sensible on
/// macOS). This holds user-editable config; secrets/runtime stay in ~/.snippet.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("snippet")
}

fn serve_settings_path() -> PathBuf {
    config_dir().join("serve.toml")
}

impl ServeSettings {
    /// Load from disk; an absent or unparseable file yields defaults.
    pub fn load() -> Self {
        std::fs::read_to_string(serve_settings_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist 0600.
    fn save(&self) -> Result<(), String> {
        let _ = std::fs::create_dir_all(config_dir());
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let path = serve_settings_path();
        std::fs::write(&path, body).map_err(|e| format!("write serve.toml: {e}"))?;
        crate::config::set_private(&path);
        Ok(())
    }
}

/// Persist an explicit auth token to `~/.snippet/serve.token` (0600) so the daemon
/// reuses it across restarts without it ever appearing on a command line.
pub fn persist_token(token: &str) {
    let _ = std::fs::create_dir_all(snippet_dir());
    let path = snippet_dir().join("serve.token");
    if std::fs::write(&path, token).is_ok() {
        crate::config::set_private(&path);
    }
}

/// The fixed command the service runs. Runtime settings come from serve.toml,
/// not from these args, so the service definition never has to change.
/// `--config` is a top-level arg, so it precedes the `serve` subcommand.
fn service_args(config_path: &std::path::Path) -> Vec<String> {
    vec![
        "--config".to_string(),
        config_path.display().to_string(),
        "serve".to_string(),
        "--supervised".to_string(),
    ]
}

/// Install snippet serve as an OS service that auto-starts on boot/login. The
/// serve flags are written to serve.toml (read at runtime); the service itself
/// just runs `snippet serve --supervised`. launchd on macOS, systemd `--user` on
/// Linux. Re-running updates serve.toml in place — there is only ever one service.
pub fn install_service(
    host: &str,
    port: u16,
    token: Option<&str>,
    no_tunnel: bool,
    public_url: Option<&str>,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Persist the settings the supervised daemon will read on boot.
    ServeSettings {
        host: Some(host.to_string()),
        port: Some(port),
        public_url: public_url.map(str::to_string),
        no_tunnel,
    }
    .save()?;
    if let Some(t) = token {
        persist_token(t);
    }
    let args = service_args(config_path);
    // Free the port: a manually-started daemon would block the service's bind.
    let _ = stop();

    #[cfg(target_os = "macos")]
    {
        install_launchd(&exe, &args)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd(&exe, &args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (&exe, &args);
        Err("auto-start is only supported on macOS and Linux".to_string())
    }
}

/// Remove the auto-start service installed by `install_service`.
pub fn uninstall_service() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let plist = launch_agent_path();
        if plist.exists() {
            let uid = current_uid();
            if let Some(uid) = &uid {
                let _ = run(
                    "launchctl",
                    &["bootout", &format!("gui/{uid}/{}", SERVICE_LABEL)],
                );
            }
            let _ = run("launchctl", &["unload", "-w", &plist.display().to_string()]);
            std::fs::remove_file(&plist).map_err(|e| format!("remove plist: {e}"))?;
            println!("✓ Removed launchd agent: {}", plist.display());
        } else {
            println!("no launchd agent installed");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let unit = systemd_unit_path();
        let _ = run(
            "systemctl",
            &["--user", "disable", "--now", "snippet-serve.service"],
        );
        if unit.exists() {
            std::fs::remove_file(&unit).map_err(|e| format!("remove unit: {e}"))?;
            println!("✓ Removed systemd user unit: {}", unit.display());
        } else {
            println!("no systemd user unit installed");
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err("auto-start is only supported on macOS and Linux".to_string());
    }
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(state_json_path());
    Ok(())
}

/// Run a command to completion, returning whether it exited 0 (errors → false).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll for the published connection (the service starts asynchronously) and print
/// the QR + string once it's up.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_service_connection() {
    for _ in 0..30 {
        if let Some((url, tok)) = read_serve_state() {
            print_connection(&url, &tok);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("  (starting… run `snippet serve --status` for the connection string)");
}

#[cfg(target_os = "macos")]
pub(super) const SERVICE_LABEL: &str = "com.snippet.serve";

#[cfg(target_os = "macos")]
fn launch_agent_path() -> PathBuf {
    home_dir().join("Library/LaunchAgents/com.snippet.serve.plist")
}

#[cfg(target_os = "macos")]
pub(super) fn current_uid() -> Option<String> {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install_launchd(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    let plist = launch_agent_path();
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut prog = format!(
        "    <string>{}</string>\n",
        xml_escape(&exe.display().to_string())
    );
    for a in args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let home = home_dir().display().to_string();
    // cloudflared lives in ~/.snippet/bin; include common brew/system paths too.
    let path_env = format!("{home}/.snippet/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    let log = log_path().display().to_string();
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>WorkingDirectory</key>
  <string>{home_x}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home_x}</string>
    <key>PATH</key>
    <string>{path_x}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log_x}</string>
  <key>StandardErrorPath</key>
  <string>{log_x}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        prog = prog,
        home_x = xml_escape(&home),
        path_x = xml_escape(&path_env),
        log_x = xml_escape(&log),
    );
    std::fs::write(&plist, content).map_err(|e| format!("write plist: {e}"))?;

    let plist_s = plist.display().to_string();
    let loaded = if let Some(uid) = current_uid() {
        // Modern domain-target API; bootout first so a re-enable is idempotent.
        let target = format!("gui/{uid}/{SERVICE_LABEL}");
        let _ = run("launchctl", &["bootout", &target]);
        let ok = run("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist_s]);
        if ok {
            let _ = run("launchctl", &["enable", &target]);
        }
        ok
    } else {
        false
    };
    // Fall back to the legacy load -w if bootstrap is unavailable.
    if !loaded {
        let _ = run("launchctl", &["unload", &plist_s]);
        if !run("launchctl", &["load", "-w", &plist_s]) {
            return Err("launchctl could not load the agent".to_string());
        }
    }

    println!("✓ Installed launchd agent: {}", plist.display());
    println!("  Auto-starts at login and restarts on crash.  Disable: snippet serve --disable");
    print_service_connection();
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> PathBuf {
    home_dir().join(".config/systemd/user/snippet-serve.service")
}

/// Quote an arg for a systemd `ExecStart` line (which is shell-like word-split).
#[cfg(target_os = "linux")]
fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(target_os = "linux")]
fn install_systemd(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    let unit = systemd_unit_path();
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut exec = sh_quote(&exe.display().to_string());
    for a in args {
        exec.push(' ');
        exec.push_str(&sh_quote(a));
    }
    let home = home_dir().display().to_string();
    let path_env = format!("{home}/.snippet/bin:/usr/local/bin:/usr/bin:/bin");
    let content = format!(
        r#"[Unit]
Description=snippet serve (remote-control daemon)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec}
WorkingDirectory={home}
Environment=HOME={home}
Environment=PATH={path_env}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#,
    );
    std::fs::write(&unit, content).map_err(|e| format!("write unit: {e}"))?;

    let _ = run("systemctl", &["--user", "daemon-reload"]);
    if !run(
        "systemctl",
        &["--user", "enable", "--now", "snippet-serve.service"],
    ) {
        return Err(
            "systemctl --user enable failed (is a user systemd session available?)".to_string(),
        );
    }
    // Survive logout / start at boot without an active login session.
    if let Ok(user) = std::env::var("USER") {
        let _ = run("loginctl", &["enable-linger", &user]);
    }

    println!("✓ Installed systemd user unit: {}", unit.display());
    println!(
        "  Enabled + started; lingering on so it survives logout.  Disable: snippet serve --disable"
    );
    print_service_connection();
    Ok(())
}

/// True when the OS auto-start unit/agent is already on disk.
fn service_unit_installed() -> bool {
    #[cfg(target_os = "linux")]
    {
        return systemd_unit_path().exists();
    }
    #[cfg(target_os = "macos")]
    {
        return launch_agent_path().exists();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Start an already-installed auto-start service (no reinstall, no console spam).
fn start_installed_service() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if !run("systemctl", &["--user", "start", "snippet-serve.service"]) {
            // enable --now covers a unit that exists but was disabled.
            if !run(
                "systemctl",
                &["--user", "enable", "--now", "snippet-serve.service"],
            ) {
                return Err("systemctl --user start/enable snippet-serve failed".to_string());
            }
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let plist = launch_agent_path();
        let plist_s = plist.display().to_string();
        if let Some(uid) = current_uid() {
            let target = format!("gui/{uid}/{SERVICE_LABEL}");
            let _ = run("launchctl", &["kickstart", "-k", &target]);
            if run("launchctl", &["print", &target]) {
                return Ok(());
            }
            let _ = run("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist_s]);
            let _ = run("launchctl", &["enable", &target]);
            let _ = run("launchctl", &["kickstart", "-k", &target]);
            return Ok(());
        }
        let _ = run("launchctl", &["load", "-w", &plist_s]);
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("auto-start is only supported on macOS and Linux".to_string())
    }
}

/// Quiet install used by the TUI: same as `install_service` defaults, no QR/status prints.
fn install_service_quiet(config_path: &std::path::Path) -> Result<(), String> {
    let settings = ServeSettings::load();
    let host = settings
        .host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = settings.port.unwrap_or(8787);
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    ServeSettings {
        host: Some(host),
        port: Some(port),
        public_url: settings.public_url.clone(),
        no_tunnel: settings.no_tunnel,
    }
    .save()?;
    let args = service_args(config_path);
    // Don't stop() here — caller already knows nothing is answering /health.
    #[cfg(target_os = "macos")]
    {
        install_launchd_quiet(&exe, &args)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd_quiet(&exe, &args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (&exe, &args);
        Err("auto-start is only supported on macOS and Linux".to_string())
    }
}

#[cfg(target_os = "linux")]
fn install_systemd_quiet(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    let unit = systemd_unit_path();
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut exec = sh_quote(&exe.display().to_string());
    for a in args {
        exec.push(' ');
        exec.push_str(&sh_quote(a));
    }
    let home = home_dir().display().to_string();
    let path_env = format!("{home}/.snippet/bin:/usr/local/bin:/usr/bin:/bin");
    let content = format!(
        r#"[Unit]
Description=snippet serve (remote-control daemon)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec}
WorkingDirectory={home}
Environment=HOME={home}
Environment=PATH={path_env}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#,
    );
    std::fs::write(&unit, content).map_err(|e| format!("write unit: {e}"))?;
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    if !run(
        "systemctl",
        &["--user", "enable", "--now", "snippet-serve.service"],
    ) {
        return Err(
            "systemctl --user enable failed (is a user systemd session available?)".to_string(),
        );
    }
    if let Ok(user) = std::env::var("USER") {
        let _ = run("loginctl", &["enable-linger", &user]);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_launchd_quiet(exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    // Reuse the normal writer; suppress connection spam by not calling print_*.
    // install_launchd prints — duplicate the essential load steps only.
    let plist = launch_agent_path();
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut prog = format!(
        "    <string>{}</string>\n",
        xml_escape(&exe.display().to_string())
    );
    for a in args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let home = home_dir().display().to_string();
    let path_env = format!("{home}/.snippet/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    let log = log_path().display().to_string();
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>WorkingDirectory</key>
  <string>{home_x}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home_x}</string>
    <key>PATH</key>
    <string>{path_x}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log_x}</string>
  <key>StandardErrorPath</key>
  <string>{log_x}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        prog = prog,
        home_x = xml_escape(&home),
        path_x = xml_escape(&path_env),
        log_x = xml_escape(&log),
    );
    std::fs::write(&plist, content).map_err(|e| format!("write plist: {e}"))?;
    let plist_s = plist.display().to_string();
    if let Some(uid) = current_uid() {
        let target = format!("gui/{uid}/{SERVICE_LABEL}");
        let _ = run("launchctl", &["bootout", &target]);
        if !run("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist_s]) {
            let _ = run("launchctl", &["unload", &plist_s]);
            if !run("launchctl", &["load", "-w", &plist_s]) {
                return Err("launchctl could not load the agent".to_string());
            }
        } else {
            let _ = run("launchctl", &["enable", &target]);
        }
    } else {
        let _ = run("launchctl", &["unload", &plist_s]);
        if !run("launchctl", &["load", "-w", &plist_s]) {
            return Err("launchctl could not load the agent".to_string());
        }
    }
    Ok(())
}

/// Make sure the serve OS service is installed, enabled, and started.
/// No-op path when the unit already exists: just `start` / `enable --now`.
/// Used by the TUI so opening the app always brings the daemon up.
pub fn ensure_service(config_path: &std::path::Path) -> Result<(), String> {
    if service_unit_installed() {
        start_installed_service()
    } else {
        install_service_quiet(config_path)
    }
}

#[cfg(test)]
mod tests {
    use super::write_binary_hash;

    #[test]
    fn startup_stamp_replaces_stale_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("serve.lock");
        std::fs::write(&path, "old-binary-hash\n").expect("seed stale hash");

        write_binary_hash(&path, "new-binary-hash").expect("stamp current hash");

        assert_eq!(
            std::fs::read_to_string(path).expect("read stamped hash"),
            "new-binary-hash"
        );
    }
}
