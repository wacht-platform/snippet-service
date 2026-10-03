use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const SESSION_ENV: &str = "SNIPPET_MCP_URL";
pub const MCP_SERVER_NAME: &str = "snippet_snippet";
const RULE_CHUNK_BYTES: usize = 20_000;

const ALLOWED: &[&str] = &[
    "view_file",
    "list_dir",
    "find_by_name",
    "grep_search",
    "read_url_content",
    "search_web",
    "list_resources",
    "read_resource",
    "list_permissions",
    "command_status",
    "finish",
    "wait",
    "wait_5_seconds",
];

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
    let instead = match name {
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
