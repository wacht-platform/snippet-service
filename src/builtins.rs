use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

const MAX_INLINE_CHARS: usize = 60_000;

pub fn coding_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.insert(ReadFileTool);
    registry.insert(ReadImageTool);
    registry.insert(WriteFileTool);
    registry.insert(AppendFileTool);
    registry.insert(EditFileTool);
    registry.insert(ReplaceFileContentTool);
    registry.insert(ListFilesTool);
    registry.insert(SearchFilesTool);
    registry.insert(SearchContentTool);
    registry.insert(ViewOutlineTool);
    registry.insert(BashTool);
    registry.insert(TerminateLoopTool);
    registry
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn expect_object<T>(tool: &str, arguments: Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    if !arguments.is_object() {
        return Err(ToolError::InvalidArguments {
            tool: tool.to_string(),
        });
    }
    Ok(serde_json::from_value(arguments)?)
}

/// A fast, non-cryptographic fingerprint of a returned slice. Used so the model
/// (and the edit tools) can tell whether the bytes they're acting on are the
/// ones they read. Ported in spirit from wacht's `slice_hash`.
fn slice_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub struct ReadFileTool;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    start_char: Option<usize>,
    #[serde(default)]
    end_char: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file from the workspace. Page large files with a line \
                range (start_line/end_line) or a 1-based char window (start_char/end_char). Returns \
                total_lines, total_chars and a slice_hash for the returned slice."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "start_char": {"type": "integer", "minimum": 1},
                    "end_char": {"type": "integer", "minimum": 1}
                }),
                &["path"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadFileArgs = expect_object("read_file", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        let content = tokio::fs::read_to_string(&path).await?;
        ctx.mark_read(&path);

        let total_lines = content.lines().count();
        let total_chars = content.chars().count();

        // A char window takes precedence over a line range when both are given.
        let (selected, mut range_meta) = if args.start_char.is_some() || args.end_char.is_some() {
            let chars: Vec<char> = content.chars().collect();
            let start = args.start_char.unwrap_or(1).max(1);
            let end = args.end_char.unwrap_or(total_chars).clamp(start, total_chars);
            let slice: String = if start <= total_chars {
                chars[start - 1..end].iter().collect()
            } else {
                String::new()
            };
            (slice, json!({"start_char": start, "end_char": end}))
        } else if args.start_line.is_some() || args.end_line.is_some() {
            let start = args.start_line.unwrap_or(1).max(1);
            let end = args.end_line.unwrap_or(usize::MAX);
            let slice = content
                .lines()
                .enumerate()
                .filter_map(|(idx, line)| {
                    let line_no = idx + 1;
                    (line_no >= start && line_no <= end).then_some(line)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (slice, json!({"start_line": start, "end_line": end}))
        } else {
            (content, json!({}))
        };

        let hash = slice_hash(&selected);
        // The model controls read size via the windows, so read_file pages itself
        // rather than spilling: an over-large slice is previewed with a hint to
        // narrow the window (the central spill in `tools.rs` exempts read_file).
        let truncated = selected.chars().count() > MAX_INLINE_CHARS;
        let content_field: String = if truncated {
            selected.chars().take(6000).collect()
        } else {
            selected
        };

        let mut out = json!({
            "path": args.path,
            "content": content_field,
            "total_lines": total_lines,
            "total_chars": total_chars,
            "slice_hash": hash,
            "truncated": truncated,
        });
        if let (Value::Object(o), Value::Object(r)) = (&mut out, range_meta.take()) {
            o.extend(r);
        }
        if truncated {
            out["hint"] = json!(
                "slice exceeds the inline limit; narrow it with start_char/end_char (or a smaller \
                 line range) to page through the file"
            );
        }
        Ok(ToolResult::success(out))
    }
}

pub struct WriteFileTool;

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "write_file".to_string(),
            description: "Create or replace a UTF-8 file in the workspace.".to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }),
                &["path", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WriteFileArgs = expect_object("write_file", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        ctx.check_write(&path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, args.content).await?;
        ctx.record_change(&path);
        Ok(ToolResult::success(
            json!({"path": args.path, "written": true}),
        ))
    }
}

pub struct AppendFileTool;

#[derive(Debug, Deserialize)]
struct AppendFileArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for AppendFileTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "append_file".to_string(),
            description: "Append content to the end of a UTF-8 file (creating it if absent), \
                inserting a newline separator when the file doesn't already end with one. Use this \
                instead of a shell `>>` redirect."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }),
                &["path", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        use tokio::io::AsyncWriteExt;
        let args: AppendFileArgs = expect_object("append_file", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        ctx.check_write(&path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let mut payload = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            payload.push('\n');
        }
        payload.push_str(&args.content);

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(payload.as_bytes()).await?;
        ctx.record_change(&path);

        let lines_written = args.content.lines().count();
        let total_lines = existing.lines().count() + lines_written;
        Ok(ToolResult::success(json!({
            "path": args.path,
            "appended": true,
            "lines_written": lines_written,
            "total_lines": total_lines,
        })))
    }
}

/// Sniff an image MIME type from the leading magic bytes.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
        let head = head.trim_start();
        (head.starts_with("<?xml") || head.starts_with("<svg")).then_some("image/svg+xml")
    }
}

pub struct ReadImageTool;

#[derive(Debug, Deserialize)]
struct ReadImageArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadImageTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_image".to_string(),
            description: "Inspect an image file in the workspace: returns its detected MIME type \
                and byte size (the raw bytes are not inlined). Use it to confirm an image exists \
                and what kind it is."
                .to_string(),
            input_schema: object_schema(json!({"path": {"type": "string"}}), &["path"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadImageArgs = expect_object("read_image", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        let bytes = tokio::fs::read(&path).await?;
        ctx.mark_read(&path);
        let mime = sniff_image_mime(&bytes).unwrap_or("application/octet-stream");
        Ok(ToolResult::success(json!({
            "path": args.path,
            "mime": mime,
            "size_bytes": bytes.len(),
        })))
    }
}

pub struct EditFileTool;

#[derive(Debug, Deserialize)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "edit_file".to_string(),
            description: "Replace exact text in a UTF-8 file. Fails if the match is missing."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                }),
                &["path", "old_string", "new_string"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: EditFileArgs = expect_object("edit_file", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        ctx.check_write(&path)?;
        let content = tokio::fs::read_to_string(&path).await?;
        if !content.contains(&args.old_string) {
            return Err(ToolError::msg(format!(
                "old_string was not found in `{}`",
                args.path
            )));
        }
        let updated = if args.replace_all {
            content.replace(&args.old_string, &args.new_string)
        } else {
            content.replacen(&args.old_string, &args.new_string, 1)
        };
        tokio::fs::write(&path, updated).await?;
        ctx.record_change(&path);
        Ok(ToolResult::success(
            json!({"path": args.path, "edited": true}),
        ))
    }
}

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
    200
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
                        "default": 200,
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
        let ext_set: Option<std::collections::HashSet<String>> = args.extensions.as_ref().map(|exts| {
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
                "Run a shell command in the workspace. Keep output narrow and deterministic. Use max_lines or max_bytes to limit output."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 1800},
                    "max_lines": {"type": "integer", "minimum": 1, "description": "Limit stdout/stderr output to this many lines. If omitted, uses max_bytes."},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 20000, "description": "Hard limit on output size in bytes."}
                }),
                &["command"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: BashArgs = expect_object("bash", arguments)?;
        let child = Command::new("sh")
            .arg("-lc")
            .arg(&args.command)
            .current_dir(ctx.workspace_root())
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

        // Apply line limit before byte limit if specified
        let (stdout_truncated, stderr_truncated) = if let Some(max_lines) = args.max_lines {
            (truncate_lines(&stdout_str, max_lines), truncate_lines(&stderr_str, max_lines))
        } else {
            (
                truncate_bytes(&stdout_str, args.max_bytes / 2),
                truncate_bytes(&stderr_str, args.max_bytes / 2),
            )
        };

        let value = json!({
            "command": args.command,
            "exit_code": output.status.code(),
            "success": output.status.success(),
            "stdout": stdout_truncated,
            "stderr": stderr_truncated,
        });

        // Final byte cap check
        let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
        if rendered.chars().count() > args.max_bytes {
            truncate_output_to(value, args.max_bytes)
        } else {
            Ok(ToolResult::success(value))
        }
    }
}

fn truncate_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: String = lines[..lines.len().min(max)].join("\n");
    if lines.len() > max {
        result.push_str(&format!("\n… +{} more lines", lines.len() - max));
    }
    result
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Back off to the nearest char boundary at or below the byte limit.
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = text[..end].to_string();
    result.push_str(&format!(
        "\n… truncated ({} of {} bytes shown)",
        end,
        text.len()
    ));
    result
}

fn truncate_output_to(value: Value, max_bytes: usize) -> Result<ToolResult, ToolError> {
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
    let truncated_len = max_bytes.min(4000);
    let preview: String = rendered.chars().take(truncated_len).collect();
    Ok(ToolResult::success(json!({
        "truncated": true,
        "data_omitted": true,
        "preview": preview,
        "original_stats": {
            "char_count": rendered.chars().count(),
            "size_bytes": rendered.len(),
        },
        "hint": "Output exceeded the inline limit; rerun with a narrower command or read a smaller slice.",
    })))
}

pub struct TerminateLoopTool;

#[async_trait]
impl Tool for TerminateLoopTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "terminate_loop".to_string(),
            description: "End your turn and hand control back to the user — this is the ONLY way to \
                stop the agent loop. A plain text reply by itself does NOT end the loop; you must \
                call `terminate_loop` to stop. Call it when you have answered the request, \
                delivered what was asked, or are blocked waiting on the user. Put your final \
                user-facing reply in the text beside this call; `summary` is a short internal note \
                of what was accomplished. It must be the only tool call in its response — finish \
                any work first, then terminate alone."
                .to_string(),
            input_schema: object_schema(json!({"summary": {"type": "string"}}), &["summary"]),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let Some(object) = arguments.as_object() else {
            return Err(ToolError::InvalidArguments {
                tool: "terminate_loop".to_string(),
            });
        };
        let Some(summary) = object
            .get("summary")
            .or_else(|| object.get("message"))
            .and_then(Value::as_str)
        else {
            return Err(ToolError::InvalidArguments {
                tool: "terminate_loop".to_string(),
            });
        };
        Ok(ToolResult::success(json!({"summary": summary})))
    }
}



pub struct SearchContentTool;

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
            description: "Search for a query string within the contents of workspace text files recursively.".to_string(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "The exact substring or pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search within (relative to workspace root). Defaults to workspace root."
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
                        "maximum": 500,
                        "default": 200,
                        "description": "Maximum number of matching lines to return."
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
        let query = if args.case_sensitive {
            args.query.clone()
        } else {
            args.query.to_lowercase()
        };
        
        let ext_set: Option<std::collections::HashSet<String>> = args.extensions.as_ref().map(|exts| {
            exts.iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect()
        });
        
        let max_results = args.max_results;
        
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
                        if !matches!(name.as_str(), ".git" | "target" | "node_modules" | ".snippet") {
                            stack.push(entry.path());
                        }
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
                    let Ok(content) = std::fs::read_to_string(&path) else {
                        // Skip binary files
                        continue;
                    };
                    
                    let rel = path.strip_prefix(&workspace_root).unwrap_or(&path);
                    for (idx, line) in content.lines().enumerate() {
                        let line_to_check = if args.case_sensitive {
                            line.to_string()
                        } else {
                            line.to_lowercase()
                        };
                        
                        if line_to_check.contains(&query) {
                            results.push(json!({
                                "path": rel.display().to_string(),
                                "line_number": idx + 1,
                                "content": line.trim(),
                            }));
                            if results.len() >= max_results {
                                return results;
                            }
                        }
                    }
                }
            }
            results
        })
        .await
        .map_err(|e| ToolError::msg(format!("search_content failed: {e}")))?;
        
        Ok(ToolResult::success(json!({
            "query": args.query,
            "count": found.len(),
            "results": found,
        })))
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
            description: "Show a high-level outline of classes, functions, structs, and methods defined in a workspace code file.".to_string(),
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
        let content = tokio::fs::read_to_string(&path).await?;
        ctx.mark_read(&path);

        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let outline_lines: Vec<Value> = content
            .lines()
            .enumerate()
            .filter_map(|(idx, line)| {
                let trimmed = line.trim();
                let is_outline = match ext.as_str() {
                    "rs" => {
                        trimmed.starts_with("fn ") 
                        || trimmed.starts_with("pub fn ") 
                        || trimmed.starts_with("struct ") 
                        || trimmed.starts_with("pub struct ") 
                        || trimmed.starts_with("enum ") 
                        || trimmed.starts_with("pub enum ") 
                        || trimmed.starts_with("impl ") 
                        || trimmed.starts_with("pub impl ") 
                        || trimmed.starts_with("trait ") 
                        || trimmed.starts_with("pub trait ") 
                        || trimmed.starts_with("mod ") 
                        || trimmed.starts_with("pub mod ")
                    }
                    "py" => {
                        trimmed.starts_with("def ") 
                        || trimmed.starts_with("class ")
                    }
                    "go" => {
                        trimmed.starts_with("func ") 
                        || (trimmed.starts_with("type ") && (trimmed.contains("struct") || trimmed.contains("interface")))
                    }
                    "js" | "ts" | "jsx" | "tsx" => {
                        trimmed.starts_with("class ") 
                        || trimmed.starts_with("export class ") 
                        || trimmed.starts_with("function ") 
                        || trimmed.starts_with("export function ") 
                        || (trimmed.starts_with("const ") && (trimmed.contains("=>") || trimmed.contains("function")))
                    }
                    _ => {
                        trimmed.starts_with("fn ") 
                        || trimmed.starts_with("def ") 
                        || trimmed.starts_with("class ") 
                        || trimmed.starts_with("struct ")
                    }
                };

                if is_outline {
                    Some(json!({
                        "line_number": idx + 1,
                        "content": line.to_string(),
                    }))
                } else {
                    None
                }
            })
            .collect();

        Ok(ToolResult::success(json!({
            "path": args.path,
            "outline": outline_lines,
        })))
    }
}

pub struct ReplaceFileContentTool;

#[derive(Debug, Deserialize)]
struct ReplaceFileContentArgs {
    path: String,
    start_line: usize,
    end_line: usize,
    target_content: String,
    replacement_content: String,
}

#[async_trait]
impl Tool for ReplaceFileContentTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "replace_file_content".to_string(),
            description: "Replace a contiguous block of text in a file. Specifies a 1-indexed line range [start_line, end_line] containing precisely the target_content to edit, and replaces it with replacement_content.".to_string(),
            input_schema: object_schema(
                json!({
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "target_content": {"type": "string", "description": "The exact string content inside the line range to be replaced."},
                    "replacement_content": {"type": "string", "description": "The content to replace it with."}
                }),
                &["path", "start_line", "end_line", "target_content", "replacement_content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReplaceFileContentArgs = expect_object("replace_file_content", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        ctx.check_write(&path)?;
        
        let content = tokio::fs::read_to_string(&path).await?;
        let lines: Vec<&str> = content.lines().collect();
        
        if args.start_line > lines.len() || args.end_line > lines.len() || args.start_line > args.end_line {
            return Err(ToolError::msg(format!(
                "Invalid line range [{}-{}] for file with {} lines",
                args.start_line, args.end_line, lines.len()
            )));
        }
        
        let slice = &lines[args.start_line - 1..args.end_line];
        let actual_target = slice.join("\n");
        
        if actual_target.trim() != args.target_content.trim() {
            return Err(ToolError::msg(format!(
                "Target content mismatch at line range [{}-{}]. Expected:\n{:?}\nBut found:\n{:?}",
                args.start_line, args.end_line, args.target_content, actual_target
            )));
        }
        
        let mut new_lines = Vec::new();
        if args.start_line > 1 {
            new_lines.extend_from_slice(&lines[0..args.start_line - 1]);
        }
        new_lines.push(&args.replacement_content);
        if args.end_line < lines.len() {
            new_lines.extend_from_slice(&lines[args.end_line..]);
        }
        
        let mut updated = new_lines.join("\n");
        if content.ends_with('\n') && !updated.ends_with('\n') {
            updated.push('\n');
        }
        tokio::fs::write(&path, updated).await?;
        ctx.record_change(&path);
        
        Ok(ToolResult::success(json!({
            "path": args.path,
            "replaced": true
        })))
    }
}
