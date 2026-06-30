use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

const MAX_INLINE_CHARS: usize = 60_000;

pub fn coding_tools(exa_api_key: Option<String>, memory: crate::memory::MemoryLimits) -> ToolRegistry {
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
    registry.insert(CodeMapTool);
    registry.insert(BashTool);
    registry.insert(SkillTool);
    // web_search / web_read are offered only when an Exa key is configured.
    if let Some(key) = exa_api_key.filter(|k| !k.trim().is_empty()) {
        registry.insert(WebSearchTool { api_key: key.clone() });
        registry.insert(WebReadTool { api_key: key });
    }
    // Per-workspace memory: read is offered whenever enabled; writes only to the
    // main session (lanes are read-only, so they can't clobber the shared index).
    if memory.enabled {
        registry.insert(MemoryReadTool);
        if memory.writable {
            registry.insert(MemoryWriteTool {
                entry_budget: memory.entry_budget_chars,
                max_entries: memory.max_entries,
            });
            registry.insert(MemoryIndexTool { index_budget: memory.index_budget_chars });
            registry.insert(MemoryDeleteTool);
            registry.insert(MemoryRuleTool);
        }
    }
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
            // `clamp(start, total)` panics if start > total (e.g. an empty file), so
            // bound `end` independently and only slice when the window is in range.
            let end = args.end_char.unwrap_or(total_chars).min(total_chars).max(start);
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
            description: "Load an image file (png/jpg/webp/gif/bmp/svg) so you can SEE it — the \
                image is attached to your context and you can describe, analyze, or act on its \
                contents. Use this for screenshots, diagrams, mockups, or any image path the user \
                points you at. Call it once per image (multiple images are fine)."
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
        if args.old_string == args.new_string {
            return Err(ToolError::msg(
                "old_string and new_string are identical — this edit changes nothing. \
                 Supply the actual replacement, or skip the edit."
                    .to_string(),
            ));
        }
        let path = ctx.resolve_workspace_path(&args.path)?;
        ctx.check_write(&path)?;
        let content = tokio::fs::read_to_string(&path).await?;

        // 1. Exact match — fast path.
        let exact = content.matches(&args.old_string).count();
        if exact > 0 {
            if exact > 1 && !args.replace_all {
                return Err(ToolError::msg(format!(
                    "old_string matches {exact} places in `{}` — pass replace_all:true to change \
                     every occurrence, or add surrounding lines so it's unique.",
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
            return Ok(ToolResult::success(json!({"path": args.path, "edited": true})));
        }

        // 2. Whitespace-flexible fallback (single edit): match line-by-line ignoring
        // each line's indentation, then re-indent new_string to the file's actual
        // indentation. Rescues the common failure where the text is right but the
        // indentation is a few spaces off — instead of looping on "not found".
        if !args.replace_all {
            match flexible_replace(&content, &args.old_string, &args.new_string) {
                Flex::Replaced(updated) => {
                    tokio::fs::write(&path, updated).await?;
                    ctx.record_change(&path);
                    return Ok(ToolResult::success(json!({
                        "path": args.path,
                        "edited": true,
                        "note": "matched ignoring indentation; re-indented to the file",
                    })));
                }
                Flex::Ambiguous(n) => {
                    return Err(ToolError::msg(format!(
                        "old_string matches {n} places in `{}` once indentation is ignored — add \
                         surrounding lines so it's unique.",
                        args.path
                    )));
                }
                Flex::NoMatch => {}
            }
        }

        // 3. No match — return a diagnostic with the file region so the model can fix
        // its old_string instead of blindly retrying the same near-miss.
        Err(ToolError::msg(edit_diagnostic(&content, &args.old_string, &args.path)))
    }
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

enum Flex {
    Replaced(String),
    Ambiguous(usize),
    NoMatch,
}

/// Normalize a line for matching: drop leading/trailing whitespace AND collapse
/// internal whitespace runs to a single space. So `  foo( x )` and `foo(  x  )`
/// compare equal. A blank/whitespace-only line normalizes to "".
fn norm_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whitespace-tolerant single-block replace, line-by-line (no byte math). Matches
/// `old` against the file ignoring indentation, internal whitespace runs, AND
/// blank-line differences (blank lines are skipped on both sides). When exactly
/// one block matches, rebuilds the file with `new` in its place, re-indented to
/// the matched block's indentation. Skips CRLF files so it never rewrites line
/// endings; those fall through to the diagnostic.
fn flexible_replace(content: &str, old: &str, new: &str) -> Flex {
    if content.contains('\r') {
        return Flex::NoMatch;
    }
    let file_lines: Vec<&str> = content.lines().collect();

    // The needle: old's non-blank lines, normalized.
    let needle: Vec<String> = old.lines().map(norm_line).filter(|l| !l.is_empty()).collect();
    if needle.is_empty() {
        return Flex::NoMatch;
    }
    // File's non-blank lines, normalized, paired with their original line index.
    let file_nb: Vec<(usize, String)> = file_lines
        .iter()
        .enumerate()
        .map(|(i, l)| (i, norm_line(l)))
        .filter(|(_, l)| !l.is_empty())
        .collect();
    let m = needle.len();
    if file_nb.len() < m {
        return Flex::NoMatch;
    }

    // Find windows of file non-blank lines equal to the needle; record the
    // ORIGINAL span (first..=last, including any interior blank lines).
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for start in 0..=file_nb.len() - m {
        if (0..m).all(|k| file_nb[start + k].1 == needle[k]) {
            hits.push((file_nb[start].0, file_nb[start + m - 1].0));
        }
    }
    match hits.as_slice() {
        [] => Flex::NoMatch,
        &[(first, last)] => {
            let file_indent = leading_ws(file_lines[first]);
            let old_indent =
                leading_ws(old.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
            let new_block = new.lines().map(|line| {
                if line.trim().is_empty() {
                    String::new()
                } else {
                    let body = line.strip_prefix(old_indent).unwrap_or(line);
                    format!("{file_indent}{body}")
                }
            });
            let mut out: Vec<String> = file_lines[..first].iter().map(|s| s.to_string()).collect();
            out.extend(new_block);
            out.extend(file_lines[last + 1..].iter().map(|s| s.to_string()));
            let mut joined = out.join("\n");
            if content.ends_with('\n') {
                joined.push('\n');
            }
            Flex::Replaced(joined)
        }
        more => Flex::Ambiguous(more.len()),
    }
}

/// A helpful "not found" message: point the model at the file region near the
/// first line of its old_string so it can copy the exact text.
fn edit_diagnostic(content: &str, old: &str, path: &str) -> String {
    let first = old.split('\n').map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let lines: Vec<&str> = content.lines().collect();
    let near = (!first.is_empty())
        .then(|| {
            lines
                .iter()
                .position(|l| l.trim() == first)
                .or_else(|| lines.iter().position(|l| l.trim().contains(first)))
        })
        .flatten();
    let mut msg = format!(
        "old_string was not found in `{path}`. It doesn't match the file byte-for-byte — usually a \
         whitespace/indentation difference, or a line that isn't actually there. Copy the snippet \
         EXACTLY as read_file shows it (including leading spaces), keep it small, and make it unique."
    );
    if let Some(idx) = near {
        let lo = idx.saturating_sub(2);
        let hi = (idx + 8).min(lines.len());
        let region = lines[lo..hi]
            .iter()
            .enumerate()
            .map(|(k, l)| format!("{:>4}| {l}", lo + k + 1))
            .collect::<Vec<_>>()
            .join("\n");
        msg.push_str(&format!(
            "\n\nThe file near the first line of your old_string (use this exact text):\n{region}"
        ));
    }
    msg
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
                "Run a shell command in the workspace. Keep output narrow and deterministic. Use max_lines or max_bytes to limit output. Set background=true for long-lived processes (dev servers, watchers): it returns immediately, redirects output to a log file, and tracks the process in the live background-process list — tail the log or `kill <pid>` to manage it."
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
                .env("SNIPPET_SHADOW_GIT", crate::checkpoint::shadow_dir(ctx.workspace_root()))
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()?;
            let pid = child.id().unwrap_or(0);
            crate::bg::record(ctx.workspace_root(), &id, &args.command, pid).ok();
            let status_path = crate::bg::status_path(ctx.workspace_root(), &id);
            tokio::spawn(async move {
                let code = match child.wait().await {
                    Ok(s) => s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
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
            // The shadow checkpoint repo's git-dir, so the agent can review its own
            // changes: `git --git-dir=$SNIPPET_SHADOW_GIT --work-tree=. diff checkpoint`.
            .env("SNIPPET_SHADOW_GIT", crate::checkpoint::shadow_dir(ctx.workspace_root()))
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
                        "default": 150,
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

        let ext_set: Option<std::collections::HashSet<String>> = args.extensions.as_ref().map(|exts| {
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
                            let l = if case_sensitive { line.to_string() } else { line.to_lowercase() };
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
                        if !matches!(name.as_str(), ".git" | "target" | "node_modules" | ".snippet") {
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
                let Ok(content) = std::fs::read_to_string(path) else { continue };
                let Some(symbols) = crate::outline::outline_source(&ext, &content) else { continue };
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

        let ext = path.extension()
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
        
        if args.start_line == 0
            || args.start_line > lines.len()
            || args.end_line > lines.len()
            || args.start_line > args.end_line
        {
            return Err(ToolError::msg(format!(
                "Invalid line range [{}-{}] for file with {} lines (lines are 1-based)",
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

pub struct WebSearchTool {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_web_results")]
    num_results: usize,
}

fn default_web_results() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web (via Exa) for anything beyond the local workspace — current events, library/API docs, error messages, release notes, best practices. Returns ranked results with title, URL, publish date, and a text snippet from each page. Use a focused natural-language query.".to_string(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Natural-language search query."
                    },
                    "num_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 5,
                        "description": "How many results to return (1-10)."
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WebSearchArgs = expect_object("web_search", arguments)?;
        let num = args.num_results.clamp(1, 10);
        let body = json!({
            "query": args.query,
            "numResults": num,
            "type": "auto",
            "contents": { "text": { "maxCharacters": 1200 } },
        });

        let response = reqwest::Client::new()
            .post("https://api.exa.ai/search")
            .header("x-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| ToolError::msg(format!("exa request failed: {error}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ToolError::msg(format!("reading exa response failed: {error}")))?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Ok(ToolResult::error(format!(
                "exa search failed: HTTP {status}: {detail}"
            )));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::msg(format!("invalid exa response: {error}")))?;

        let results: Vec<Value> = parsed
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let snippet: String = item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(1000)
                            .collect();
                        json!({
                            "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                            "published_date": item.get("publishedDate").and_then(Value::as_str),
                            "snippet": snippet,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ToolResult::success(json!({
            "query": args.query,
            "count": results.len(),
            "results": results,
        })))
    }
}

pub struct WebReadTool {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
struct WebReadArgs {
    url: String,
    #[serde(default = "default_read_chars")]
    max_characters: usize,
}

fn default_read_chars() -> usize {
    8000
}

#[async_trait]
impl Tool for WebReadTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "web_read".to_string(),
            description: "Fetch and read the full text of a specific web page by URL (via Exa) — use after web_search to read a result in depth, or to read any known URL (docs page, issue, article). Returns the page's extracted text.".to_string(),
            input_schema: object_schema(
                json!({
                    "url": {
                        "type": "string",
                        "description": "The full URL of the page to read."
                    },
                    "max_characters": {
                        "type": "integer",
                        "minimum": 500,
                        "maximum": 10000,
                        "default": 8000,
                        "description": "Maximum characters of page text to return (Exa caps this at 10000)."
                    }
                }),
                &["url"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WebReadArgs = expect_object("web_read", arguments)?;
        let max_chars = args.max_characters.clamp(500, 10_000);
        let body = json!({
            "urls": [args.url],
            "text": { "maxCharacters": max_chars },
            // 0 = fetch fresh: the documented way to crawl a URL not already in
            // Exa's index (replaces the deprecated `livecrawl`).
            "maxAgeHours": 0,
        });

        let response = reqwest::Client::new()
            .post("https://api.exa.ai/contents")
            .header("x-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| ToolError::msg(format!("exa request failed: {error}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ToolError::msg(format!("reading exa response failed: {error}")))?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            return Ok(ToolResult::error(format!(
                "exa read failed: HTTP {status}: {detail}"
            )));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::msg(format!("invalid exa response: {error}")))?;

        let Some(result) = parsed
            .get("results")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
        else {
            // Exa reports per-URL failures in `statuses[].error` rather than results.
            let reason = parsed
                .get("statuses")
                .and_then(Value::as_array)
                .and_then(|s| s.first())
                .and_then(|s| s.get("error"))
                .and_then(Value::as_str)
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            return Ok(ToolResult::error(format!(
                "no content returned for `{}` — the page may be unreachable or blocked.{reason}",
                args.url
            )));
        };

        let text: String = result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(max_chars)
            .collect();

        Ok(ToolResult::success(json!({
            "url": result.get("url").and_then(Value::as_str).unwrap_or(&args.url),
            "title": result.get("title").and_then(Value::as_str).unwrap_or(""),
            "published_date": result.get("publishedDate").and_then(Value::as_str),
            "text": text,
        })))
    }
}

// --- Per-workspace memory tools (store: crate::memory::MemoryStore) -----------

fn memory_store(ctx: &ToolContext) -> crate::memory::MemoryStore {
    crate::memory::MemoryStore::for_workspace(ctx.workspace_root())
}

/// Memory writes are confined to the main session; lanes share the workspace and
/// would otherwise race on the single index file.
fn require_main_owner(ctx: &ToolContext) -> Result<(), ToolError> {
    if ctx.owner() != "main" {
        return Err(ToolError::msg(
            "workspace memory is read-only in delegated lanes — only the main session can write it",
        ));
    }
    Ok(())
}

pub struct MemoryReadTool;

#[derive(Debug, Deserialize)]
struct MemoryReadArgs {
    id: String,
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_read".to_string(),
            description: "Load the full content of a workspace memory entry by id (entry ids are listed in the [workspace_memory] index).".to_string(),
            input_schema: object_schema(
                json!({ "id": {"type": "string", "description": "entry id, e.g. build-and-test"} }),
                &["id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: MemoryReadArgs = expect_object("memory_read", arguments)?;
        let content = memory_store(ctx).read_entry(&args.id).map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "id": args.id, "content": content })))
    }
}

pub struct MemoryWriteTool {
    pub entry_budget: usize,
    pub max_entries: usize,
}

#[derive(Debug, Deserialize)]
struct MemoryWriteArgs {
    id: String,
    content: String,
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_write".to_string(),
            description: "Create or replace a workspace memory ENTRY — a durable fact, pointer, or \
                how-to playbook for THIS folder that should help future sessions. Use a short \
                kebab-case id. After writing, add or update a one-line pointer to it via \
                memory_index so it stays discoverable."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "id": {"type": "string", "description": "kebab-case slug, e.g. build-and-test"},
                    "content": {"type": "string"}
                }),
                &["id", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryWriteArgs = expect_object("memory_write", arguments)?;
        memory_store(ctx)
            .write_entry(&args.id, &args.content, self.entry_budget, self.max_entries)
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "id": args.id, "saved": true })))
    }
}

pub struct MemoryIndexTool {
    pub index_budget: usize,
}

#[derive(Debug, Deserialize)]
struct MemoryIndexArgs {
    content: String,
}

#[async_trait]
impl Tool for MemoryIndexTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_index".to_string(),
            description: "Replace the always-loaded workspace memory INDEX. Keep it lean: one short \
                line per entry — a label, a one-line summary, and the entry id to load with \
                memory_read. Must fit the index budget (oversize writes are rejected)."
                .to_string(),
            input_schema: object_schema(
                json!({ "content": {"type": "string"} }),
                &["content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryIndexArgs = expect_object("memory_index", arguments)?;
        memory_store(ctx).write_index(&args.content, self.index_budget).map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "saved": true })))
    }
}

pub struct MemoryDeleteTool;

#[derive(Debug, Deserialize)]
struct MemoryDeleteArgs {
    id: String,
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_delete".to_string(),
            description: "Delete a workspace memory entry by id. Also remove its line from the index with memory_index.".to_string(),
            input_schema: object_schema(
                json!({ "id": {"type": "string"} }),
                &["id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryDeleteArgs = expect_object("memory_delete", arguments)?;
        memory_store(ctx).delete_entry(&args.id).map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "id": args.id, "deleted": true })))
    }
}

pub struct MemoryRuleTool;

#[derive(Debug, Deserialize)]
struct MemoryRuleArgs {
    scope: String,
    content: String,
}

#[async_trait]
impl Tool for MemoryRuleTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_rule".to_string(),
            description: "Set the STANDING RULES that are always loaded into context and must be \
                followed. `scope`='global' applies in EVERY workspace — use it for cross-cutting \
                user preferences (e.g. \"when writing emails, don't use dashes\"); 'workspace' \
                applies to THIS folder only. Replaces the rule list at that scope, so include all \
                rules you want kept; pass empty content to clear. Keep them short and imperative; \
                never store secrets here."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["global", "workspace"]},
                    "content": {"type": "string", "description": "the full rule list for this scope (e.g. markdown bullets)"}
                }),
                &["scope", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryRuleArgs = expect_object("memory_rule", arguments)?;
        let store = match args.scope.as_str() {
            "global" => crate::memory::MemoryStore::global(),
            "workspace" => memory_store(ctx),
            other => {
                return Err(ToolError::msg(format!(
                    "scope must be 'global' or 'workspace', got '{other}'"
                )));
            }
        };
        store
            .write_rules(&args.content, crate::memory::rules_budget())
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "scope": args.scope, "saved": true })))
    }
}

pub struct SkillTool;

#[derive(Debug, Deserialize)]
struct SkillArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "skill".to_string(),
            description:
                "Load an Agent Skill by name — returns its full instructions (SKILL.md) and a list of its bundled files. Call this when a task matches one of the skills listed under [skills]. After loading, follow the instructions; read any referenced files with read_file and run bundled scripts with bash (their contents stay out of context until you do)."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "name": {"type": "string", "description": "The skill name, exactly as listed under [skills]."}
                }),
                &["name"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SkillArgs = expect_object("skill", arguments)?;
        match crate::skills::load(&args.name) {
            Some((sk, body, files)) => Ok(ToolResult::success(json!({
                "name": sk.name,
                "dir": sk.dir.display().to_string(),
                "instructions": body,
                "bundled_files": files,
            }))),
            None => Err(ToolError::msg(format!(
                "no such skill: {} (see the [skills] list)",
                args.name
            ))),
        }
    }
}
