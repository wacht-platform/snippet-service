use super::*;
use std::process::Stdio;
use tokio::process::Command;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    max_lines: Option<usize>,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    /// Run detached (long-lived servers/watchers): returns immediately, output goes
    /// to a log file, and it's tracked in the live background-process list.
    #[serde(default)]
    background: bool,
}

fn default_max_bytes() -> usize {
    20000
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "bash".to_string(),
            description:
                "Run a shell command in the workspace. Keep output narrow and deterministic. Always provide a clear, concise `label` describing the semantic intent of the command. Use max_lines or max_bytes to limit output. Set background=true for long-lived processes (dev servers, watchers): it returns immediately, redirects output to a log file, and tracks the process in the live background-process list — inspect logs or terminate it using `manage_process`. For interactive/async apps you must control programmatically (a browser, a REPL, an emulator), do NOT script the whole interaction in one shot: start the app once with background=true, then drive it surgically across small follow-up calls (browser via its remote-debugging port, REPL via a fifo stdin), reading the new output between steps, and terminate it via `manage_process` when done."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string", "description": "The exact shell command line string to execute."},
                    "label": {"type": "string", "description": "A concise description of what this command does (typically under 10 words, e.g. 'Run test suite', 'Check git status', 'Install dependencies', 'Build backend'). Required."},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800},
                    "max_lines": {"type": "integer", "minimum": 1, "description": "Limit stdout/stderr output to this many lines. If omitted, uses max_bytes."},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 20000, "description": "Hard limit on output size in bytes."},
                    "background": {"type": "boolean", "default": false, "description": "Run detached and return immediately; for servers/watchers that should keep running. Output goes to a log file; the process shows up in the background-process list."}
                }),
                &["command", "label"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: BashArgs = expect_object("bash", arguments)?;
        // Vault secrets referenced as $NAME are injected into the CHILD env only —
        // the command string and the result never carry values (results are also
        // scrubbed at the harness choke point).
        let vault_env = crate::vault::Vault::load().env_for_command(&args.command);

        let label = args.label.as_deref().map(str::trim).filter(|s| !s.is_empty());

        if args.background {
            let id = crate::bg::new_id();
            let log_path = crate::bg::log_path(ctx.workspace_root(), &id);
            std::fs::create_dir_all(crate::bg::bg_dir(ctx.workspace_root()))
                .map_err(|e| ToolError::msg(format!("background dir: {e}")))?;
            let log = std::fs::File::create(&log_path)
                .map_err(|e| ToolError::msg(format!("background log: {e}")))?;
            let log_err = log.try_clone().map_err(|e| ToolError::msg(e.to_string()))?;
            // Detached: redirect to the log; keep running across tool calls. A
            // detached task awaits it only to record its exit status (the process
            // itself isn't blocked on us).
            let mut child = Command::new("sh")
                .arg("-lc")
                .arg(format!("{PATH_PRELUDE}{}", args.command))
                .current_dir(ctx.current_dir())
                .env(
                    "SNIPPET_SHADOW_GIT",
                    crate::checkpoint::shadow_dir(ctx.workspace_root()),
                )
                .envs(agent_env(ctx))
                .envs(vault_env)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()?;
            let pid = child.id().unwrap_or(0);
            crate::bg::record(ctx.workspace_root(), &id, &args.command, label, pid).ok();
            let status_path = crate::bg::status_path(ctx.workspace_root(), &id);
            tokio::spawn(async move {
                let code = match child.wait().await {
                    Ok(s) => s
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string()),
                    Err(_) => "?".to_string(),
                };
                let _ = std::fs::write(status_path, code);
            });
            let mut res = json!({
                "command": args.command,
                "background": true,
                "id": id,
                "pid": pid,
                "log": log_path.display().to_string(),
                "note": "started in the background and still running. Use `manage_process` to inspect logs or terminate it, or tail the log file directly. It appears in your [background_processes] list.",
            });
            if let Some(lbl) = label {
                res["label"] = json!(lbl);
            }
            return Ok(ToolResult::success(res));
        }

        let start_dir = ctx.current_dir();
        let pwd_file = std::env::temp_dir().join(format!(
            "snippet-pwd-{}",
            &uuid::Uuid::new_v4().to_string()[..12]
        ));
        let script = format!(
            "{PATH_PRELUDE}__snippet_pwd_file='{}'; trap 'pwd > \"$__snippet_pwd_file\"' EXIT\n{}",
            pwd_file.display(),
            args.command
        );
        let child = Command::new("sh")
            .arg("-lc")
            .arg(&script)
            .current_dir(&start_dir)
            .kill_on_drop(true)
            // The shadow checkpoint repo's git-dir, so the agent can review its own
            // changes: `git --git-dir=$SNIPPET_SHADOW_GIT --work-tree=. diff checkpoint`.
            .env(
                "SNIPPET_SHADOW_GIT",
                crate::checkpoint::shadow_dir(ctx.workspace_root()),
            )
            .envs(agent_env(ctx))
            .envs(vault_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let timeout = std::time::Duration::from_secs(args.timeout_seconds.unwrap_or(120).min(1800));
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                let _ = std::fs::remove_file(&pwd_file);
                ToolError::msg(format!("command timed out after {}s", timeout.as_secs()))
            })??;
        let end_dir = std::fs::read_to_string(&pwd_file)
            .ok()
            .map(|s| std::path::PathBuf::from(s.trim()))
            .filter(|p| p.is_dir());
        let _ = std::fs::remove_file(&pwd_file);
        if let Some(dir) = end_dir.as_ref() {
            ctx.set_current_dir(dir.clone());
        }

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);

        // Always save the full, unabridged execution log to disk
        let logs_dir = ctx.workspace_root().join(".snippet").join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        let file_name = format!(
            "bash_{}_{}.log",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let log_path = logs_dir.join(&file_name);
        let full_raw = format!(
            "=== LABEL ===\n{}\n\n=== COMMAND ===\n{}\n\n=== EXIT CODE: {} (success: {}) ===\n\n=== STDOUT ===\n{}\n\n=== STDERR ===\n{}",
            label.unwrap_or(""),
            args.command,
            output.status.code().unwrap_or(-1),
            output.status.success(),
            stdout_str,
            stderr_str,
        );
        let _ = std::fs::write(&log_path, &full_raw);
        let rel_log_path = format!(".snippet/logs/{file_name}");

        let total_stdout_lines = stdout_str.lines().count();
        let total_stderr_lines = stderr_str.lines().count();
        let total_lines = total_stdout_lines + total_stderr_lines;
        let total_bytes = stdout_str.len() + stderr_str.len();

        let byte_budget = (args.max_bytes / 2).max(4000);
        let (stdout_display, stdout_truncated) = format_output_preview(
            &stdout_str,
            args.max_lines,
            byte_budget,
            &rel_log_path,
        );
        let (stderr_display, stderr_truncated) = format_output_preview(
            &stderr_str,
            args.max_lines,
            byte_budget,
            &rel_log_path,
        );
        let is_truncated = stdout_truncated || stderr_truncated;

        let mut value = json!({
            "command": args.command,
            "exit_code": output.status.code(),
            "stdout": stdout_display,
        });
        if !stderr_display.trim().is_empty() {
            value["stderr"] = json!(stderr_display);
        }
        if let Some(lbl) = label {
            value["label"] = json!(lbl);
        }
        if let Some(dir) = end_dir.filter(|d| *d != start_dir) {
            value["cwd"] = json!(dir.display().to_string());
        }

        if is_truncated {
            value["truncated"] = json!(true);
            value["saved_output_path"] = json!(rel_log_path);
            value["total_lines"] = json!(total_lines);
            value["total_bytes"] = json!(total_bytes);
            value["hint"] = json!(format!(
                "Output exceeded display limit; full output ({total_lines} lines, {total_bytes} bytes) saved to `{rel_log_path}`. Read the part you need with `sed -n`, `rg`, `head` or `tail`."
            ));
        }

        Ok(ToolResult::success(value))
    }
}

/// Environment every agent shell gets: the session id `snippet history` is
/// scoped to, and the folder of the running snippet binary so its subcommands
/// resolve to the same version as the daemon.
fn agent_env(ctx: &ToolContext) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(id) = ctx.durable_session_id() {
        env.push(("SNIPPET_SESSION_ID".to_string(), id.to_string()));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        env.push(("SNIPPET_BIN_DIR".to_string(), dir.display().to_string()));
    }
    env
}

/// Prepended to every command. `sh -l` sources the login profile, which may
/// reset PATH, so the snippet binary is put first after that has run.
const PATH_PRELUDE: &str = "[ -n \"$SNIPPET_BIN_DIR\" ] && export PATH=\"$SNIPPET_BIN_DIR:$PATH\"\n";

fn format_output_preview(
    text: &str,
    max_lines: Option<usize>,
    max_bytes: usize,
    log_ref: &str,
) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let total_bytes = text.len();

    let line_ceiling = max_lines.unwrap_or(100);
    if total_lines <= line_ceiling && total_bytes <= max_bytes {
        return (text.to_string(), false);
    }

    // Keep head and tail lines so start of command and ending errors are both visible
    let half = (line_ceiling / 2).max(10);
    let head_count = half.min(total_lines);
    let tail_count = half.min(total_lines.saturating_sub(head_count));
    let omitted = total_lines.saturating_sub(head_count + tail_count);

    let head = &lines[..head_count];
    let tail = &lines[total_lines.saturating_sub(tail_count)..];

    let mut result = String::new();
    result.push_str(&head.join("\n"));
    if omitted > 0 {
        result.push_str(&format!(
            "\n\n… <truncated {omitted} lines; full output ({total_lines} lines, {total_bytes} bytes) saved to {log_ref}> …\n\n"
        ));
    } else {
        result.push_str(&format!(
            "\n\n… <truncated to size limit; full output saved to {log_ref}> …\n\n"
        ));
    }
    result.push_str(&tail.join("\n"));

    (result, true)
}
