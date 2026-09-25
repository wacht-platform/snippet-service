use super::*;
use std::process::Stdio;
use tokio::process::Command;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct ListFilesTool;

#[derive(Debug, Deserialize)]
struct ListFilesArgs {
    #[serde(default = "default_dot")]
    path: String,
}

fn default_dot() -> String {
    ".".to_string()
}

#[async_trait]
impl Tool for ListFilesTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_files".to_string(),
            description: "List direct children of a workspace directory.".to_string(),
            input_schema: object_schema(json!({"path": {"type": "string"}}), &[]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ListFilesArgs = expect_object("list_files", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        let mut dir = tokio::fs::read_dir(path).await?;
        let mut entries = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let file_type = entry.file_type().await?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": if file_type.is_dir() { "dir" } else { "file" },
            }));
        }
        Ok(ToolResult::success(
            json!({"path": args.path, "entries": entries}),
        ))
    }
}

pub struct SearchFilesTool;

#[derive(Debug, Deserialize)]
struct SearchFilesArgs {
    pattern: String,
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default = "default_search_extensions")]
    extensions: Option<Vec<String>>,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_search_path() -> String {
    ".".to_string()
}

fn default_search_extensions() -> Option<Vec<String>> {
    None
}

fn default_max_results() -> usize {
    60
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "search_files".to_string(),
            description: "Find files by name pattern or extension within the workspace. Use for locating files when you know the name or extension but not the exact path."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "pattern": {
                        "type": "string",
                        "description": "Glob-like pattern to match filenames (e.g. 'main', '*.rs', 'config*')."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search within. Defaults to workspace root."
                    },
                    "extensions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional list of file extensions to filter by (e.g. ['rs', 'toml']). Overrides pattern if specified."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 60,
                        "description": "Maximum number of results to return."
                    }
                }),
                &["pattern"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SearchFilesArgs = expect_object("search_files", arguments)?;
        let search_root = ctx.resolve_workspace_path(&args.path)?;
        let workspace_root = ctx.workspace_root().to_path_buf();
        let pattern_lower = args.pattern.to_lowercase();
        let ext_set: Option<std::collections::HashSet<String>> =
            args.extensions.as_ref().map(|exts| {
                exts.iter()
                    .map(|e| e.trim_start_matches('.').to_lowercase())
                    .collect()
            });
        let max_results = args.max_results;
        let path_label = args.path.clone();
        let pattern_label = args.pattern.clone();

        // Recursive walk on a blocking thread; only owned, Send values are moved in.
        let found = tokio::task::spawn_blocking(move || {
            let mut results: Vec<Value> = Vec::new();
            let mut stack = vec![search_root];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    let name = entry.file_name().to_string_lossy().to_string();
                    if file_type.is_dir() {
                        if !matches!(name.as_str(), ".git" | "target" | "node_modules") {
                            stack.push(entry.path());
                        }
                        continue;
                    }

                    let name_lower = name.to_lowercase();
                    let matches_pattern = if pattern_lower.is_empty() || pattern_lower == "*" {
                        true
                    } else if let Some(star) = pattern_lower.find('*') {
                        name_lower.starts_with(&pattern_lower[..star])
                            && name_lower.ends_with(&pattern_lower[star + 1..])
                    } else {
                        name_lower.contains(&pattern_lower)
                    };
                    if !matches_pattern {
                        continue;
                    }

                    if let Some(exts) = &ext_set {
                        let ext_ok = entry
                            .path()
                            .extension()
                            .map(|e| exts.contains(&e.to_string_lossy().to_lowercase()))
                            .unwrap_or(false);
                        if !ext_ok {
                            continue;
                        }
                    }

                    let path = entry.path();
                    let rel = path.strip_prefix(&workspace_root).unwrap_or(&path);
                    results.push(json!({"path": rel.display().to_string(), "name": name}));
                    if results.len() >= max_results {
                        return results;
                    }
                }
            }
            results
        })
        .await
        .map_err(|e| ToolError::msg(format!("search_files failed: {e}")))?;

        Ok(ToolResult::success(json!({
            "path": path_label,
            "pattern": pattern_label,
            "count": found.len(),
            "results": found,
        })))
    }
}

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
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
                "Run a shell command in the workspace. Keep output narrow and deterministic. Use max_lines or max_bytes to limit output. Set background=true for long-lived processes (dev servers, watchers): it returns immediately, redirects output to a log file, and tracks the process in the live background-process list — tail the log or `kill <pid>` to manage it. For interactive/async apps you must control programmatically (a browser, a REPL, an emulator), do NOT script the whole interaction in one shot: start the app once with background=true, then drive it surgically across small follow-up calls (browser via its remote-debugging port, REPL via a fifo stdin), reading the new output between steps, and kill the pid when done."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800},
                    "max_lines": {"type": "integer", "minimum": 1, "description": "Limit stdout/stderr output to this many lines. If omitted, uses max_bytes."},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 20000, "description": "Hard limit on output size in bytes."},
                    "background": {"type": "boolean", "default": false, "description": "Run detached and return immediately; for servers/watchers that should keep running. Output goes to a log file; the process shows up in the background-process list."}
                }),
                &["command"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: BashArgs = expect_object("bash", arguments)?;
        // Vault secrets referenced as $NAME are injected into the CHILD env only —
        // the command string and the result never carry values (results are also
        // scrubbed at the harness choke point).
        let vault_env = crate::vault::Vault::load().env_for_command(&args.command);

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
                .arg(&args.command)
                .current_dir(ctx.workspace_root())
                .env(
                    "SNIPPET_SHADOW_GIT",
                    crate::checkpoint::shadow_dir(ctx.workspace_root()),
                )
                .envs(vault_env)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()?;
            let pid = child.id().unwrap_or(0);
            crate::bg::record(ctx.workspace_root(), &id, &args.command, pid).ok();
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
            return Ok(ToolResult::success(json!({
                "command": args.command,
                "background": true,
                "id": id,
                "pid": pid,
                "log": log_path.display().to_string(),
                "note": "started in the background and still running. tail the log file to see output, or `kill <pid>` to stop it. it appears in your background-process list.",
            })));
        }

        let child = Command::new("sh")
            .arg("-lc")
            .arg(&args.command)
            .current_dir(ctx.workspace_root())
            .kill_on_drop(true)
            // The shadow checkpoint repo's git-dir, so the agent can review its own
            // changes: `git --git-dir=$SNIPPET_SHADOW_GIT --work-tree=. diff checkpoint`.
            .env(
                "SNIPPET_SHADOW_GIT",
                crate::checkpoint::shadow_dir(ctx.workspace_root()),
            )
            .envs(vault_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let timeout = std::time::Duration::from_secs(args.timeout_seconds.unwrap_or(120).min(1800));
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                ToolError::msg(format!("command timed out after {}s", timeout.as_secs()))
            })??;

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
            "=== COMMAND ===\n{}\n\n=== EXIT CODE: {} (success: {}) ===\n\n=== STDOUT ===\n{}\n\n=== STDERR ===\n{}",
            args.command,
            output.status.code().unwrap_or(-1),
            output.status.success(),
            stdout_str,
            stderr_str,
        );
        let _ = std::fs::write(&log_path, &full_raw);
        let rel_log_path = format!(".snippet/logs/{file_name}");
        let abs_log_path = log_path.display().to_string();

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
            "success": output.status.success(),
            "stdout": stdout_display,
            "stderr": stderr_display,
            "saved_output_path": rel_log_path,
            "log_path": abs_log_path,
            "total_lines": total_lines,
            "total_bytes": total_bytes,
        });

        if is_truncated {
            value["truncated"] = json!(true);
            value["hint"] = json!(format!(
                "Output exceeded display limit; full output ({total_lines} lines, {total_bytes} bytes) saved to `{rel_log_path}`. Inspect it using read_file, grep, head, or tail."
            ));
        }

        Ok(ToolResult::success(value))
    }
}

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

pub struct SearchContentTool;

/// How `search_content` matches a line: a compiled regex (default), or a literal
/// substring fallback when the query isn't a valid regex.
enum Matcher {
    Regex(regex::Regex),
    Literal(String),
}

#[derive(Debug, Deserialize)]
struct SearchContentArgs {
    query: String,
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default)]
    extensions: Option<Vec<String>>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

#[async_trait]
impl Tool for SearchContentTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "search_content".to_string(),
            description: "Search file contents recursively with a regular expression (RE2 syntax: `|` alternation, `.*`, `\\b`, char classes, etc.). Case-insensitive by default. An invalid regex is treated as a literal substring.".to_string(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Regular expression to search for (RE2 syntax). Use `\\b`, `|`, `.*`, char classes, etc. Plain text works too (it's a valid regex). An invalid regex falls back to a literal substring match."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search within (relative to workspace root). A file searches just that file; a directory is walked recursively. Defaults to the workspace root."
                    },
                    "extensions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional list of file extensions to limit search (e.g. ['rs', 'toml'])."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether to perform a case-sensitive search. Defaults to false."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 150,
                        "default": 60,
                        "description": "Maximum number of matching lines to return (capped at 150)."
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SearchContentArgs = expect_object("search_content", arguments)?;
        let search_root = ctx.resolve_workspace_path(&args.path)?;
        let workspace_root = ctx.workspace_root().to_path_buf();
        // Treat the query as a regex (models write grep-style patterns: `a|b`, `.*`,
        // `\b`). Fall back to a literal substring if it doesn't compile.
        let matcher = match regex::RegexBuilder::new(&args.query)
            .case_insensitive(!args.case_sensitive)
            .build()
        {
            Ok(re) => Matcher::Regex(re),
            Err(_) => Matcher::Literal(if args.case_sensitive {
                args.query.clone()
            } else {
                args.query.to_lowercase()
            }),
        };

        let ext_set: Option<std::collections::HashSet<String>> =
            args.extensions.as_ref().map(|exts| {
                exts.iter()
                    .map(|e| e.trim_start_matches('.').to_lowercase())
                    .collect()
            });

        // Clamp so the result stays under the inline ceiling and is never spilled to
        // a scratch file (which would drop the `count` and render as "0 matches").
        let max_results = args.max_results.min(150);
        let case_sensitive = args.case_sensitive;

        let found = tokio::task::spawn_blocking(move || {
            let mut results: Vec<Value> = Vec::new();

            // Scan one file for the query; returns true once max_results is reached.
            let scan_file = |path: &std::path::Path, results: &mut Vec<Value>| -> bool {
                if let Some(exts) = &ext_set {
                    let ext_ok = path
                        .extension()
                        .map(|e| exts.contains(&e.to_string_lossy().to_lowercase()))
                        .unwrap_or(false);
                    if !ext_ok {
                        return false;
                    }
                }
                let Ok(content) = std::fs::read_to_string(path) else {
                    return false; // skip binary/unreadable files
                };
                let rel = path.strip_prefix(&workspace_root).unwrap_or(path);
                for (idx, line) in content.lines().enumerate() {
                    let hit = match &matcher {
                        Matcher::Regex(re) => re.is_match(line),
                        Matcher::Literal(q) => {
                            let l = if case_sensitive {
                                line.to_string()
                            } else {
                                line.to_lowercase()
                            };
                            l.contains(q)
                        }
                    };
                    if hit {
                        // Truncate long/minified lines so a big match set can't blow
                        // past the inline ceiling and get spilled.
                        let trimmed = line.trim();
                        let snippet: String = if trimmed.chars().count() > 200 {
                            trimmed.chars().take(200).collect::<String>() + "…"
                        } else {
                            trimmed.to_string()
                        };
                        results.push(json!({
                            "path": rel.display().to_string(),
                            "line_number": idx + 1,
                            "content": snippet,
                        }));
                        if results.len() >= max_results {
                            return true;
                        }
                    }
                }
                false
            };

            // A FILE path searches just that file; a DIRECTORY (or the workspace root)
            // is walked recursively. This makes `path` work whether the model scopes by
            // file or by folder — previously a file path hit read_dir and returned 0.
            if search_root.is_file() {
                scan_file(&search_root, &mut results);
                return results;
            }

            let mut stack = vec![search_root];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    let name = entry.file_name().to_string_lossy().to_string();
                    if file_type.is_dir() {
                        if !matches!(
                            name.as_str(),
                            ".git" | "target" | "node_modules" | ".snippet"
                        ) {
                            stack.push(entry.path());
                        }
                        continue;
                    }
                    if scan_file(&entry.path(), &mut results) {
                        return results;
                    }
                }
            }
            results
        })
        .await
        .map_err(|e| ToolError::msg(format!("search_content failed: {e}")))?;

        let capped = found.len() >= max_results;
        let mut out = json!({
            "query": args.query,
            "count": found.len(),
            "results": found,
            "truncated": capped,
        });
        if capped {
            out["hint"] = json!(
                "result list was capped — there may be more matches; narrow the query or pass a \
                 `path` to focus the search"
            );
        }
        Ok(ToolResult::success(out))
    }
}

pub struct CodeMapTool;

#[derive(Debug, Deserialize)]
struct CodeMapArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

#[async_trait]
impl Tool for CodeMapTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "code_map".to_string(),
            description: "Map the declarations (functions, types, methods, classes) across the \
                whole project (or a subdirectory), grouped by file, via language-aware parsing — \
                a fast way to learn what exists and where before reading. Optionally narrow with \
                `path` (a subdirectory) and/or `query` (only symbols whose signature contains the \
                text). Respects .gitignore. Covers the same languages as view_outline; other \
                languages are skipped (use search_content for those)."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string", "description": "Subdirectory to map (relative to workspace root). Defaults to the whole project."},
                    "query": {"type": "string", "description": "Only include symbols whose signature contains this text (case-insensitive)."}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CodeMapArgs = expect_object("code_map", arguments)?;
        let root = match &args.path {
            Some(p) => ctx.resolve_workspace_path(p)?,
            None => ctx.workspace_root().to_path_buf(),
        };
        let root_label = args.path.clone().unwrap_or_else(|| ".".to_string());
        let workspace_root = ctx.workspace_root().to_path_buf();
        let query = args.query.map(|q| q.to_lowercase());

        // Bounded so the result never trips the inline-output spill (which would drop
        // counts). The model narrows with `path`/`query` for anything bigger.
        const MAX_FILES: usize = 300;
        const MAX_SYMBOLS: usize = 300;
        const MAX_PER_FILE: usize = 40;

        let (files, symbol_count, truncated) = tokio::task::spawn_blocking(move || {
            let mut files: Vec<Value> = Vec::new();
            let mut symbol_count = 0usize;
            let mut file_count = 0usize;
            let mut truncated = false;
            for entry in ignore::WalkBuilder::new(&root).build() {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if !crate::outline::is_supported(&ext) {
                    continue;
                }
                if file_count >= MAX_FILES || symbol_count >= MAX_SYMBOLS {
                    truncated = true;
                    break;
                }
                file_count += 1;
                let Ok(content) = std::fs::read_to_string(path) else {
                    continue;
                };
                let Some(symbols) = crate::outline::outline_source(&ext, &content) else {
                    continue;
                };
                let rel = path.strip_prefix(&workspace_root).unwrap_or(path);
                let mut items: Vec<String> = Vec::new();
                for s in &symbols {
                    if let Some(q) = &query {
                        if !s.signature.to_lowercase().contains(q.as_str()) {
                            continue;
                        }
                    }
                    if items.len() >= MAX_PER_FILE {
                        break;
                    }
                    items.push(format!("{} {} :{}", s.kind, s.signature, s.line));
                    symbol_count += 1;
                    if symbol_count >= MAX_SYMBOLS {
                        truncated = true;
                        break;
                    }
                }
                if !items.is_empty() {
                    files.push(json!({ "path": rel.display().to_string(), "symbols": items }));
                }
            }
            (files, symbol_count, truncated)
        })
        .await
        .map_err(|e| ToolError::msg(format!("code_map failed: {e}")))?;

        let mut out = json!({
            "root": root_label,
            "file_count": files.len(),
            "symbol_count": symbol_count,
            "files": files,
            "truncated": truncated,
        });
        if truncated {
            out["hint"] = json!(
                "map was capped — narrow with `path` (a subdirectory) or `query` to see the rest"
            );
        }
        Ok(ToolResult::success(out))
    }
}

pub struct ViewOutlineTool;

#[derive(Debug, Deserialize)]
struct ViewOutlineArgs {
    path: String,
}

#[async_trait]
impl Tool for ViewOutlineTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "view_outline".to_string(),
            description: "Map the structure of ONE source file: its top-level declarations \
                (functions, structs, enums, traits, classes, methods) with line numbers. Use it to \
                see what a file contains and where things are defined WITHOUT reading the whole \
                file — far cheaper than read_file for a large file or a first-pass overview; then \
                read_file the specific lines you actually need. Parses Rust, Python, JavaScript, \
                TypeScript/TSX, Go, Java, C, and C++ (real signatures + doc comments); other \
                languages return a 'not supported' note — use search_content / read_file there. \
                If given a directory it lists the folder's contents (use list_files for that), \
                then point view_outline at a specific file."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to the code file relative to the workspace root."
                    }
                }),
                &["path"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ViewOutlineArgs = expect_object("view_outline", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        // view_outline maps a single file, but the model often aims it at a folder.
        // Rather than error, list the directory (like list_files) so the call still
        // makes progress and the model can pick a file to outline next.
        if path.is_dir() {
            let mut dir = tokio::fs::read_dir(&path).await?;
            let mut entries = Vec::new();
            while let Some(entry) = dir.next_entry().await? {
                let file_type = entry.file_type().await?;
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "kind": if file_type.is_dir() { "dir" } else { "file" },
                }));
            }
            return Ok(ToolResult::success(json!({
                "path": args.path,
                "is_directory": true,
                "entries": entries,
                "note": "This path is a directory, not a file — listed its contents instead. \
                         Call view_outline on a specific file inside it to map its declarations.",
            })));
        }
        let content = tokio::fs::read_to_string(&path).await?;
        ctx.mark_read(&path);

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Tree-sitter structural outline (real signatures + the language's doc-comment
        // standard, with nested methods) for bundled languages; unsupported languages
        // get an honest "not supported" note rather than a fake heuristic.
        if let Some(symbols) = crate::outline::outline_source(&ext, &content) {
            let items: Vec<Value> = symbols
                .iter()
                .map(|s| {
                    let mut o = json!({
                        "kind": s.kind,
                        "signature": s.signature,
                        "line_number": s.line,
                        "depth": s.depth,
                    });
                    if let Some(doc) = &s.doc {
                        o["doc"] = json!(doc);
                    }
                    o
                })
                .collect();
            return Ok(ToolResult::success(json!({
                "path": args.path,
                "language": ext,
                "symbol_count": items.len(),
                "outline": items,
            })));
        }

        Ok(ToolResult::success(json!({
            "path": args.path,
            "supported": false,
            "note": format!(
                "No structural outline for `.{ext}` files — view_outline supports rust, python, \
                 javascript, typescript, tsx, go, java, c, c++. Use search_content to locate \
                 definitions, or read_file to read this file directly."
            ),
        })))
    }
}

