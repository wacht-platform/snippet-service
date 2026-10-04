use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const SESSION_ENV: &str = "SNIPPET_MCP_URL";
pub const MCP_SERVER_NAME: &str = "snippet_snippet";
const RULE_CHUNK_BYTES: usize = 20_000;

const ALLOWED: &[&str] = &["list_resources", "read_resource", "finish", "wait", "wait_5_seconds"];

pub fn binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNIPPET_AGY_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("agy"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }
    real_home().map(|home| home.join(".local/bin/agy")).filter(|p| p.is_file())
}

pub fn real_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn run_hook() -> Result<(), String> {
    let mut raw = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw);
    println!("{}", decision(&raw, std::env::var(SESSION_ENV).is_ok()));
    Ok(())
}

fn decision(raw: &str, in_snippet: bool) -> Value {
    if !in_snippet {
        return json!({"decision": "allow"});
    }
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let name = payload.pointer("/toolCall/name").and_then(Value::as_str).unwrap_or("");
    if name == "call_mcp_tool" {
        let server = payload
            .pointer("/toolCall/args/ServerName")
            .and_then(Value::as_str)
            .unwrap_or("");
        if server == MCP_SERVER_NAME {
            return json!({"decision": "allow"});
        }
        return json!({"decision": "deny", "reason": "Only the snippet MCP server is available in a snippet session."});
    }
    if ALLOWED.contains(&name) {
        return json!({"decision": "allow"});
    }
    if name == "view_file" {
        let own = real_home()
            .map(|home| home.join(".gemini"))
            .zip(payload.pointer("/toolCall/args/AbsolutePath").and_then(Value::as_str))
            .is_some_and(|(root, path)| Path::new(path).starts_with(root));
        if own {
            return json!({"decision": "allow"});
        }
    }
    let instead = match name {
        "view_file" | "list_dir" | "find_by_name" | "grep_search" | "command_status" => {
            "the snippet `bash` tool (cat, sed -n, ls, rg)"
        }
        "search_web" => "the snippet `web_search` tool",
        "read_url_content" => "the snippet `web_read` tool",
        "run_command" | "send_command_input" => "the snippet `bash` tool",
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file" | "notebook_edit" => {
            "the snippet `change_files` tool"
        }
        "ask_question" | "ask_permission" | "ask_custom_permission" => "the snippet `ask_user` tool",
        "invoke_subagent" | "define_subagent" | "manage_subagents" | "browser_subagent" => {
            "the snippet `delegate_task` tool"
        }
        "schedule" | "manage_task" | "manage_inbox" | "send_message" | "run_workflow" => {
            return json!({
                "decision": "deny",
                "reason": format!("`{name}` is off in this snippet session. To wait for a delegated lane or a watched file, just end your turn: snippet delivers the report to you as a new message. For a file to follow, use the snippet `monitor` tool."),
            });
        }
        _ => "the matching snippet tool",
    };
    json!({
        "decision": "deny",
        "reason": format!("In this snippet session `{name}` is handled by snippet: call {instead} through call_mcp_tool on the `{MCP_SERVER_NAME}` server instead."),
    })
}

pub fn prepare_home(key: &str, mcp_url: &str, prompt: &str) -> Result<PathBuf, String> {
    let real = real_home().ok_or("HOME is not set")?.join(".gemini");
    let home = crate::config::snippet_home().join("antigravity/homes").join(key);
    let gemini = home.join(".gemini");
    let plugin = gemini.join("config/plugins/snippet");
    let rules = plugin.join("rules");
    std::fs::create_dir_all(&rules).map_err(|e| format!("create {}: {e}", rules.display()))?;
    std::fs::create_dir_all(gemini.join("antigravity-cli")).map_err(|e| e.to_string())?;
    link_entries(&real, &gemini, &["config", "antigravity-cli"])?;
    link_entries(&real.join("config"), &gemini.join("config"), &["plugins"])?;
    link_entries(
        &real.join("antigravity-cli"),
        &gemini.join("antigravity-cli"),
        &["settings.json", "mcp"],
    )?;

    let mut settings: Value = std::fs::read_to_string(real.join("antigravity-cli/settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let rule = format!("mcp({MCP_SERVER_NAME}/*)");
    let permissions = settings
        .as_object_mut()
        .map(|o| o.entry("permissions").or_insert_with(|| json!({})))
        .filter(|p| p.is_object())
        .ok_or("antigravity settings are malformed")?;
    let allow = permissions
        .as_object_mut()
        .map(|o| o.entry("allow").or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut)
        .ok_or("antigravity settings are malformed")?;
    if !allow.iter().any(|v| v.as_str() == Some(rule.as_str())) {
        allow.push(json!(rule));
    }
    write_if_changed(&gemini.join("antigravity-cli/settings.json"), &settings)?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    write_if_changed(&plugin.join("plugin.json"), &json!({"name": "snippet"}))?;
    write_if_changed(
        &plugin.join("mcp_config.json"),
        &json!({"mcpServers": {"snippet": {"url": mcp_url}}}),
    )?;
    write_if_changed(
        &plugin.join("hooks.json"),
        &json!({"snippet-gate": {"PreToolUse": [{"matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("'{}' agy-hook", exe.display()),
            "timeout": 10,
        }]}]}}),
    )?;

    if let Ok(entries) = std::fs::read_dir(&rules) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    for (index, chunk) in rule_chunks(prompt).iter().enumerate() {
        std::fs::write(rules.join(format!("{index:02}-snippet.md")), chunk)
            .map_err(|e| format!("write rules: {e}"))?;
    }
    Ok(home)
}

fn write_if_changed(path: &Path, value: &Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    if std::fs::read_to_string(path).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn link_entries(real: &Path, private: &Path, skip: &[&str]) -> Result<(), String> {
    if let Ok(entries) = std::fs::read_dir(private) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if skip.iter().any(|s| name == *s) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            let target = real.join(&name);
            let newer = match std::fs::metadata(&target).and_then(|m| m.modified()) {
                Ok(real_time) => meta.modified().map(|t| t > real_time).unwrap_or(false),
                Err(_) => true,
            };
            if newer {
                std::fs::copy(&path, &target).map_err(|e| format!("sync {}: {e}", target.display()))?;
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    let Ok(entries) = std::fs::read_dir(real) else { return Ok(()) };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let link = private.join(&name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(entry.path(), &link)
            .map_err(|e| format!("link {}: {e}", link.display()))?;
    }
    Ok(())
}

fn rule_chunks(prompt: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in prompt.split("\n\n") {
        if !current.is_empty() && current.len() + paragraph.len() + 2 > RULE_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
        }
        let mut rest = paragraph;
        while rest.len() > RULE_CHUNK_BYTES {
            let mut cut = RULE_CHUNK_BYTES;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            chunks.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(rest);
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

pub async fn models() -> Result<Vec<String>, String> {
    let bin = binary().ok_or("Antigravity CLI (agy) isn't installed")?;
    let out = tokio::process::Command::new(bin)
        .arg("models")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let models: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(id, _)| id.trim().to_string()))
        .filter(|id| !id.is_empty())
        .collect();
    if models.is_empty() {
        return Err("agy listed no models — is it signed in?".to_string());
    }
    Ok(models)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct QuotaCache {
    fetched_at: i64,
    groups: Vec<crate::llm::RateLimitSnapshot>,
}

static QUOTA_REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn quota_path() -> PathBuf {
    crate::config::snippet_home().join("antigravity/quota.json")
}

pub fn cached_quota() -> Option<(i64, Vec<crate::llm::RateLimitSnapshot>)> {
    let cache: QuotaCache = serde_json::from_str(&std::fs::read_to_string(quota_path()).ok()?).ok()?;
    Some((cache.fetched_at, cache.groups))
}

pub fn refresh_quota_if_stale(max_age_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    if cached_quota().is_some_and(|(at, _)| now - at < max_age_secs) {
        return;
    }
    if QUOTA_REFRESHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Ok(groups) = read_quota_screen()
            && !groups.is_empty()
        {
            let cache = QuotaCache { fetched_at: chrono::Utc::now().timestamp(), groups };
            if let Ok(body) = serde_json::to_string(&cache) {
                let path = quota_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(path, body);
            }
        }
        QUOTA_REFRESHING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

#[cfg(unix)]
fn read_quota_screen() -> Result<Vec<crate::llm::RateLimitSnapshot>, String> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::time::{Duration, Instant};

    let bin = binary().ok_or("agy isn't installed")?;
    let cbin = std::ffi::CString::new(bin.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let cwd = std::ffi::CString::new(std::env::temp_dir().as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let (rows, cols) = (60u16, 130u16);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let mut master_fd = -1;
    let pid = unsafe { libc::forkpty(&mut master_fd, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
    if pid < 0 {
        return Err(format!("forkpty: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            libc::chdir(cwd.as_ptr());
            libc::setenv(c"TERM".as_ptr(), c"xterm-256color".as_ptr(), 1);
            let argv = [cbin.as_ptr(), std::ptr::null()];
            libc::execv(cbin.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let mut parser = vt100::Parser::new(rows, cols, 0);
    let pump = |master: &mut std::fs::File, parser: &mut vt100::Parser, until: &dyn Fn(&str) -> bool, limit: Duration| {
        let start = Instant::now();
        let mut buf = [0u8; 65536];
        while start.elapsed() < limit {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => parser.process(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
            if until(&parser.screen().contents()) {
                break;
            }
        }
    };
    pump(&mut master, &mut parser, &|s| s.contains("Antigravity CLI") && s.contains('>'), Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(500));
    let _ = master.write_all(b"/usage");
    pump(&mut master, &mut parser, &|_| false, Duration::from_millis(1200));
    let _ = master.write_all(b"\r");
    pump(&mut master, &mut parser, &|s| s.matches("Five Hour Limit Remaining").count() >= 2, Duration::from_secs(15));
    pump(&mut master, &mut parser, &|_| false, Duration::from_millis(400));
    let screen = parser.screen().contents();
    let _ = master.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(200));
    let _ = master.write_all(b"\x03\x03");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    Ok(parse_quota_screen(&screen, chrono::Utc::now().timestamp()))
}

#[cfg(not(unix))]
fn read_quota_screen() -> Result<Vec<crate::llm::RateLimitSnapshot>, String> {
    Err("quota reading needs a Unix pty".into())
}

fn parse_quota_screen(screen: &str, now: i64) -> Vec<crate::llm::RateLimitSnapshot> {
    let lines: Vec<&str> = screen.lines().map(str::trim).collect();
    let mut groups = Vec::new();
    let mut current: Option<crate::llm::RateLimitSnapshot> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.ends_with("MODELS") && line.chars().all(|c| c.is_ascii_uppercase() || c == ' ') {
            if let Some(group) = current.take() {
                groups.push(group);
            }
            let mut label = line.to_ascii_lowercase();
            if let Some(first) = label.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            current = Some(crate::llm::RateLimitSnapshot {
                label: Some(label.replace("gpt", "GPT").replace("claude", "Claude").replace("gemini", "Gemini")),
                ..Default::default()
            });
        } else if let (Some(group), Some(minutes)) = (
            current.as_mut(),
            match line {
                "Weekly Limit Remaining" => Some(10080),
                "Five Hour Limit Remaining" => Some(300),
                _ => None,
            },
        ) {
            let remaining = lines
                .get(i + 1)
                .and_then(|l| l.rsplit(' ').next())
                .and_then(|p| p.trim_end_matches('%').parse::<f64>().ok());
            let resets_in = lines.get(i + 2).and_then(|l| l.strip_prefix("Refreshes in ")).map(parse_duration_secs);
            if let Some(remaining) = remaining {
                let window = crate::llm::RateLimitWindow {
                    used_percent: (100.0 - remaining).clamp(0.0, 100.0),
                    window_minutes: minutes,
                    resets_at: resets_in.map(|s| now + s).unwrap_or(0),
                };
                if minutes == 300 {
                    group.primary = Some(window);
                } else {
                    group.secondary = Some(window);
                }
            }
        }
        i += 1;
    }
    if let Some(group) = current {
        groups.push(group);
    }
    groups.into_iter().filter(|g| g.primary.is_some() || g.secondary.is_some()).collect()
}

fn parse_duration_secs(text: &str) -> i64 {
    text.split_whitespace()
        .filter_map(|part| {
            let (num, unit) = part.split_at(part.find(|c: char| !c.is_ascii_digit())?);
            let n: i64 = num.parse().ok()?;
            Some(match unit {
                "d" => n * 86400,
                "h" => n * 3600,
                "m" => n * 60,
                "s" => n,
                _ => 0,
            })
        })
        .sum()
}
