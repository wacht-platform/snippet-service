use super::*;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
const MAX_INLINE_CHARS: usize = 40_000;

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
            description:
                "Read a file from the workspace. Text: UTF-8 with optional line/char paging \
                (start_line/end_line or start_char/end_char). Images (png/jpg/webp/gif/bmp/svg): \
                auto-routes to vision — same as read_image — so you SEE the pixels; no need to \
                pick the other tool. Returns total_lines/total_chars/slice_hash for text, or \
                mime/size_bytes for images."
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
        if crate::vault::is_protected_path(&path) {
            return Err(ToolError::msg(
                "the vault file is off-limits — secret VALUES are never readable. Use a secret as $NAME in bash; its value is injected into the process and redacted from output.",
            ));
        }
        let head = {
            use tokio::io::AsyncReadExt;
            let mut f = tokio::fs::File::open(&path).await?;
            let mut buf = vec![0u8; 512];
            let n = f.read(&mut buf).await?;
            buf.truncate(n);
            buf
        };
        if let Some(mime) = sniff_image_mime(&head) {
            let bytes = tokio::fs::read(&path).await?;
            ctx.mark_read(&path);
            return Ok(ToolResult::success(json!({
                "path": args.path,
                "mime": mime,
                "size_bytes": bytes.len(),
                "via": "read_file",
            })));
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(ToolError::msg(format!(
                    "not a UTF-8 text file ({e}). For images use read_image (or read_file on png/jpg/webp/gif/bmp/svg — those auto-route to vision)."
                )));
            }
            Err(e) => return Err(e.into()),
        };
        ctx.mark_read(&path);

        let total_lines = content.lines().count();
        let total_chars = content.chars().count();

        // A char window takes precedence over a line range when both are given.
        let (selected, mut range_meta) = if args.start_char.is_some() || args.end_char.is_some() {
            let chars: Vec<char> = content.chars().collect();
            let start = args.start_char.unwrap_or(1).max(1);
            let end = args
                .end_char
                .unwrap_or(total_chars)
                .min(total_chars)
                .max(start);
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
    } else {
        None
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
                image is attached to your context. Prefer this when you know the path is an image; \
                read_file on the same path also auto-routes to vision. Call once per image."
                .to_string(),
            input_schema: object_schema(json!({"path": {"type": "string"}}), &["path"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadImageArgs = expect_object("read_image", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        let bytes = tokio::fs::read(&path).await?;
        ctx.mark_read(&path);
        let mime = sniff_image_mime(&bytes);
        let mime_str = mime.unwrap_or("application/octet-stream");
        if mime.is_none() || bytes.len() < 8 {
            return Err(ToolError::msg(&format!(
                "Invalid or empty image file at '{}'. Size: {} bytes, detected MIME: {}. \
                 The file may be corrupted, empty, or not a valid image format.",
                args.path,
                bytes.len(),
                mime_str
            )));
        }
        Ok(ToolResult::success(json!({
            "path": args.path,
            "mime": mime_str,
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
                &["path", "old_string", "new_string", "replace_all"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: EditFileArgs = expect_object("edit_file", arguments)?;
        if args.old_string == args.new_string {
            return Err(ToolError::msg(
                "old_string and new_string are identical — this edit changes nothing.\n\
                 - The file was NOT modified.\n\
                 - DO NOT re-read the file: the file is unchanged.\n\
                 - If the file already matches your desired state, skip this edit and proceed to the next step.\n\
                 - If you intended to modify the code, ensure `new_string` actually contains the new replacement."
                    .to_string(),
            ));
        }
        let path = ctx.resolve_workspace_path(&args.path)?;
        let content = tokio::fs::read_to_string(&path).await?;

        // 1. Exact match — fast path.
        let exact = content.matches(&args.old_string).count();
        if exact > 0 {
            if exact > 1 && !args.replace_all {
                return Err(ToolError::msg(ambiguous_diagnostic(
                    &content,
                    match_lines(&content, &args.old_string),
                    exact,
                    &args.path,
                    "",
                )));
            }
            let updated = if args.replace_all {
                content.replace(&args.old_string, &args.new_string)
            } else {
                content.replacen(&args.old_string, &args.new_string, 1)
            };
            tokio::fs::write(&path, &updated).await?;
            ctx.record_change(&path);
            let mut res = json!({"path": args.path, "edited": true});
            if exact == 1 {
                let start_offset = content.find(&args.old_string).unwrap_or(0);
                let start_line = content[..start_offset].matches('\n').count() + 1;
                res["diff"] = json!(format_diff_snippet(&args.old_string, &args.new_string, start_line));
            } else {
                res["replacements"] = json!(exact);
            }
            return Ok(ToolResult::success(res));
        }

        // 2. Whitespace-flexible fallback (single edit): normalize only insignificant
        // source whitespace, preserve the exact source span, and insert new_string
        // unchanged. Non-whitespace source tokens must still match exactly.
        if !args.replace_all {
            match flexible_replace(&content, &args.old_string, &args.new_string) {
                Flex::Replaced { updated, start_line, matched_text } => {
                    tokio::fs::write(&path, &updated).await?;
                    ctx.record_change(&path);
                    let diff = format_diff_snippet(&matched_text, &args.new_string, start_line);
                    return Ok(ToolResult::success(json!({
                        "path": args.path,
                        "edited": true,
                        "diff": diff,
                        "note": "matched after whitespace normalization; replacement preserved unchanged",
                    })));
                }
                Flex::Ambiguous(starts) => {
                    let n = starts.len();
                    return Err(ToolError::msg(ambiguous_diagnostic(
                        &content,
                        starts,
                        n,
                        &args.path,
                        " once indentation is ignored",
                    )));
                }
                Flex::NoMatch => {}
            }
        }

        // 3. No match — return a diagnostic with the file region so the model can fix
        // its old_string. If the file changed on disk since last read, note it.
        let stale_note = if ctx.check_write(&path).is_err() {
            format!("\n\nNote: `{}` was modified on disk since you last read it (e.g. by shell or external tool).", args.path)
        } else {
            String::new()
        };

        Err(ToolError::msg(format!(
            "{}{stale_note}",
            edit_diagnostic(&content, &args.old_string, &args.path)
        )))
    }
}

enum Flex {
    Replaced {
        updated: String,
        start_line: usize,
        matched_text: String,
    },
    /// 1-based first line of each matching block.
    Ambiguous(Vec<usize>),
    NoMatch,
}

fn format_diff_snippet(old_s: &str, new_s: &str, start_line: usize) -> String {
    let old_count = old_s.lines().count().max(1);
    let new_count = new_s.lines().count().max(1);
    let mut diff = format!("@@ -{start_line},{old_count} +{start_line},{new_count} @@\n");
    for line in old_s.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new_s.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

/// 1-based line number of each occurrence of `needle` in `content` (capped).
fn match_lines(content: &str, needle: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = content[from..].find(needle) {
        let at = from + pos;
        lines.push(content[..at].matches('\n').count() + 1);
        from = at + needle.len().max(1);
        if lines.len() >= 5 {
            break;
        }
    }
    lines
}

/// Ambiguous-match error with the DISAMBIGUATORS in it: each match's line number
/// plus the line right above it, ready to prepend to old_string. Without these
/// the model knows the edit is ambiguous but has nothing to make it unique with —
/// the classic retry loop.
fn ambiguous_diagnostic(
    content: &str,
    starts: Vec<usize>,
    n: usize,
    path: &str,
    qualifier: &str,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut msg = format!(
        "old_string matches {n} places in `{path}`{qualifier} — the text is identical at each. \
         Either pass replace_all:true to change every occurrence, or make old_string unique by \
         prepending the line ABOVE the one match you want:"
    );
    for start in starts.iter().take(5) {
        let above = start
            .checked_sub(2)
            .and_then(|i| lines.get(i))
            .map(|l| l.trim_end())
            .filter(|l| !l.trim().is_empty());
        match above {
            Some(a) => {
                let clipped: String = a.chars().take(90).collect();
                msg.push_str(&format!(
                    "\n- match at line {start}, preceded by: {clipped}"
                ));
            }
            None => msg.push_str(&format!(
                "\n- match at line {start} (top of file / blank line above)"
            )),
        }
    }
    msg
}

struct NormalizedSource {
    /// Whitespace is represented by one separator; all non-whitespace bytes are
    /// copied exactly from the source.
    text: String,
    /// Source start offset for each byte in `text`.
    starts: Vec<usize>,
    /// Source end offset for each byte in `text`.
    ends: Vec<usize>,
}

/// Normalize only insignificant source whitespace while retaining the source byte span
/// for every normalized match. Outside quoted strings, whitespace around punctuation
/// is ignored and whitespace between identifier characters becomes one separator.
/// Whitespace inside quoted strings is preserved exactly, so string contents cannot
/// be changed by a lenient source match.
fn normalize_source(source: &str) -> NormalizedSource {
    let mut text = String::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut pending_ws: Option<(usize, usize)> = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut previous_word = false;

    let mut emit = |ch: char, start: usize, end: usize| {
        text.push(ch);
        starts.extend(std::iter::repeat_n(start, ch.len_utf8()));
        ends.extend(std::iter::repeat_n(end, ch.len_utf8()));
    };

    for (start, ch) in source.char_indices() {
        let end = start + ch.len_utf8();
        if let Some(active_quote) = quote {
            emit(ch, start, end);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            previous_word = false;
            continue;
        }

        let current_word = ch.is_alphanumeric() || ch == '_';
        if ch == '\'' || ch == '"' || ch == '`' {
            pending_ws = None;
            quote = Some(ch);
            emit(ch, start, end);
            previous_word = false;
            continue;
        }

        if ch.is_whitespace() {
            pending_ws = Some(match pending_ws {
                Some((ws_start, _)) => (ws_start, end),
                None => (start, end),
            });
            continue;
        }

        if let Some((ws_start, ws_end)) = pending_ws.take() {
            if previous_word && current_word {
                emit(' ', ws_start, ws_end);
            }
        }
        emit(ch, start, end);
        previous_word = current_word;
    }

    NormalizedSource { text, starts, ends }
}

/// Whitespace-tolerant source-span replace. The source and old_string are
/// normalized only for comparison; the exact source byte range is then replaced
/// with new_string unchanged. This means indentation, line wrapping, and blank
/// lines may differ, but a changed identifier, operator, or expression cannot
/// match accidentally. A unique match is required.
fn flexible_replace(content: &str, old: &str, new: &str) -> Flex {
    let needle = normalize_source(old).text;
    if needle.is_empty() {
        return Flex::NoMatch;
    }
    let source = normalize_source(content);
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = source.text[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        hits.push((source.starts[start], source.ends[end - 1]));
        from = start + needle.len().max(1);
    }

    match hits.as_slice() {
        [] => Flex::NoMatch,
        &[(start, end)] => {
            let matched_text = content[start..end].to_string();
            let start_line = content[..start].matches('\n').count() + 1;
            let mut updated = String::with_capacity(content.len() + new.len());
            updated.push_str(&content[..start]);
            updated.push_str(new);
            updated.push_str(&content[end..]);
            Flex::Replaced {
                updated,
                start_line,
                matched_text,
            }
        }
        more => Flex::Ambiguous(
            more.iter()
                .map(|(start, _)| content[..*start].matches('\n').count() + 1)
                .collect(),
        ),
    }
}

/// A helpful "not found" message: point the model at the file region near the
/// first line of its old_string so it can copy the exact text.
fn edit_diagnostic(content: &str, old: &str, path: &str) -> String {
    let first = old
        .split('\n')
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
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
        "old_string not found in `{path}` — source matching ignores only whitespace (indentation, line breaks, and repeated spaces). Copy the exact non-whitespace text from read_file, keep it small and unique, and do not resend the same near-match."
    );
    if let Some(idx) = near {
        let lo = idx.saturating_sub(1);
        let hi = (idx + 4).min(lines.len());
        let region = lines[lo..hi]
            .iter()
            .enumerate()
            .map(|(k, l)| format!("{:>4}| {l}", lo + k + 1))
            .collect::<Vec<_>>()
            .join("\n");
        msg.push_str(&format!("\n\nActual text there:\n{region}"));
    }
    msg
}

#[cfg(test)]
mod edit_matching_tests {
    use super::*;

    #[test]
    fn flexible_replace_ignores_only_source_whitespace() {
        let source = "const value = build(\n    first,\tsecond\n);\n";
        let old = "  const   value = build(first, second);  ";
        let replacement = "const value = replacement(first, second);";

        let Flex::Replaced { updated, .. } = flexible_replace(source, old, replacement) else {
            panic!("whitespace-normalized source should match");
        };
        assert_eq!(updated, format!("{replacement}\n"));
    }

    #[test]
    fn flexible_replace_rejects_changed_source_tokens() {
        let source = "const value = build(first, second);\n";
        let old = "const value = build(first, changed);";

        assert!(matches!(
            flexible_replace(source, old, "replacement"),
            Flex::NoMatch
        ));
    }

    #[test]
    fn flexible_replace_rejects_ambiguous_normalized_source() {
        let source = "const value = build(first, second);\n\nconst value = build(first, second);\n";
        let old = "const value = build(first, second);";

        let Flex::Ambiguous(lines) = flexible_replace(source, old, "replacement") else {
            panic!("repeated normalized source should be ambiguous");
        };
        assert_eq!(lines, vec![1, 3]);
    }

    #[tokio::test]
    async fn test_edit_file_rejects_identical_strings() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = EditFileTool;
        let file_path = temp.path().join("test.txt");
        std::fs::write(&file_path, "hello world\n").unwrap();

        let err = tool
            .execute(
                &ctx,
                json!({
                    "path": "test.txt",
                    "old_string": "hello world",
                    "new_string": "hello world",
                    "replace_all": false,
                }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("old_string and new_string are identical"));
        assert!(err.to_string().contains("DO NOT re-read the file"));
    }

    #[tokio::test]
    async fn test_edit_file_succeeds_when_old_string_matches_after_external_change() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = EditFileTool;
        let file_path = temp.path().join("script.py");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        // Mark read
        ctx.mark_read(&file_path);

        // Simulate external shell change to line 1
        std::fs::write(&file_path, "line 1 modified by shell\nline 2\nline 3\n").unwrap();

        // Editing line 2 should succeed because line 2 is uniquely present in current content
        let res = tool
            .execute(
                &ctx,
                json!({
                    "path": "script.py",
                    "old_string": "line 2",
                    "new_string": "line 2 edited",
                    "replace_all": false,
                }),
            )
            .await
            .unwrap();
        assert_eq!(res.value["data"]["edited"], true);

        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "line 1 modified by shell\nline 2 edited\nline 3\n");
    }
}


