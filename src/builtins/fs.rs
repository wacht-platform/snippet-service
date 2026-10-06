use super::*;
use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

/// Lines of context shown around each change in the result.
const PREVIEW_CONTEXT: usize = 2;
/// Cap on preview lines per change so a large replacement can't flood context.
const PREVIEW_MAX_LINES: usize = 40;

pub struct ViewImageTool;

#[derive(Debug, Deserialize)]
struct ViewImageArgs {
    path: String,
}

#[async_trait]
impl Tool for ViewImageTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "view_image".to_string(),
            description: "Look at an image file (png, jpg, gif, webp): the picture is attached \
                to your context so you can see it. Use this for screenshots, diagrams and generated \
                images. For text files use bash (cat -n, sed -n, rg -n)."
                .to_string(),
            input_schema: object_schema(
                json!({"path": {"type": "string", "description": "Path to the image, relative to the current directory or absolute."}}),
                &["path"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ViewImageArgs = expect_object("view_image", arguments)?;
        let path = ctx.resolve_workspace_path(&args.path)?;
        let bytes = tokio::fs::read(&path).await?;
        let Some(mime) = sniff_image_mime(&bytes) else {
            return Err(ToolError::msg(format!(
                "`{}` is not a png, jpg, gif or webp image ({} bytes). For text files use bash.",
                args.path,
                bytes.len()
            )));
        };
        Ok(ToolResult::success(json!({
            "path": args.path,
            "mime": mime,
            "size_bytes": bytes.len(),
        })))
    }
}

pub struct ChangeFilesTool;

#[derive(Debug, Deserialize)]
struct ChangeFilesArgs {
    changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Change {
    Create {
        path: String,
        #[serde(alias = "text")]
        content: String,
        #[serde(default)]
        overwrite: bool,
    },
    Replace {
        path: String,
        #[serde(alias = "old_string", alias = "old", alias = "search")]
        find: String,
        #[serde(alias = "new_string", alias = "new", alias = "replace", alias = "replacement")]
        with: String,
        #[serde(default, alias = "replace_all")]
        all: bool,
    },
    Delete {
        path: String,
    },
    #[serde(alias = "rename")]
    Move {
        path: String,
        #[serde(alias = "new_path", alias = "destination")]
        to: String,
    },
}

impl Change {
    fn label(&self) -> (&'static str, String) {
        match self {
            Change::Create { path, .. } => ("create", path.clone()),
            Change::Replace { path, .. } => ("replace", path.clone()),
            Change::Delete { path } => ("delete", path.clone()),
            Change::Move { path, .. } => ("move", path.clone()),
        }
    }
}

/// A file's state while a batch is staged: what was on disk before the batch and
/// what it will be after. `None` means the file does not exist.
struct Staged {
    display: String,
    before: Option<String>,
    after: Option<String>,
}

/// What one change did, for the result.
struct Applied {
    action: &'static str,
    path: String,
    added: usize,
    removed: usize,
    /// For a single replace: the file, and the first line and line count of the
    /// new text, so the result can show the changed region.
    touched: Option<(PathBuf, usize, usize)>,
    note: Option<String>,
}

#[async_trait]
impl Tool for ChangeFilesTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "change_files".to_string(),
            description: "Create, edit, delete or move text files. This is how you change source \
                and other text files; read and search them with bash. Directories, binary files and \
                generated output (build folders, caches, node_modules) are handled with bash \
                (`rm -rf`, `cargo clean`, `git clean`). Pass a list of changes; they are applied \
                in order and all-or-nothing: if any change fails, no file is touched and the error \
                names the failing change.\n\n\
                Actions:\n\
                - replace: swap `find` for `with` in an existing file. `find` must be copied \
                exactly from the current file (whitespace differences are tolerated) and must match \
                exactly once; include a neighbouring line to make it unique, or set \"all\": true to \
                replace every match. Keep `find` small: just the lines you change plus enough to be \
                unique. Several replaces in the same file are fine; each sees the result of the ones \
                before it.\n\
                - create: write a new file with `content` (parent folders are created). Fails if the \
                file exists unless \"overwrite\": true; prefer replace for edits.\n\
                - delete: remove a file.\n\
                - move: rename `path` to `to`.\n\n\
                The result shows the changed lines with their line numbers, so there is no need to \
                re-read a file just to check an edit."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "changes": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {"type": "string", "enum": ["create", "replace", "delete", "move"]},
                                "path": {"type": "string", "description": "File path, relative to the current directory or absolute."},
                                "find": {"type": "string", "description": "replace: the exact existing text to change."},
                                "with": {"type": "string", "description": "replace: the new text."},
                                "all": {"type": "boolean", "description": "replace: change every match instead of requiring exactly one."},
                                "content": {"type": "string", "description": "create: the full file content."},
                                "overwrite": {"type": "boolean", "description": "create: allow replacing an existing file."},
                                "to": {"type": "string", "description": "move: the new path."}
                            },
                            "required": ["action", "path"]
                        }
                    }
                }),
                &["changes"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ChangeFilesArgs = expect_object("change_files", arguments)?;
        if args.changes.is_empty() {
            return Err(ToolError::msg("`changes` is empty: pass at least one change."));
        }
        let mut staged: HashMap<PathBuf, Staged> = HashMap::new();
        let mut order: Vec<PathBuf> = Vec::new();
        let mut applied: Vec<Applied> = Vec::new();
        let total = args.changes.len();
        for (index, change) in args.changes.into_iter().enumerate() {
            let (action, path) = change.label();
            let prefix = if total > 1 {
                format!("change {} of {total} ({action} `{path}`): ", index + 1)
            } else {
                String::new()
            };
            match stage_change(ctx, &mut staged, &mut order, change) {
                Ok(done) => applied.push(done),
                Err(message) => {
                    return Err(ToolError::msg(format!(
                        "{prefix}{message}\n\nNo files were changed."
                    )));
                }
            }
        }
        commit(ctx, &staged, &order).await?;

        let notes: Vec<String> = applied
            .iter()
            .filter_map(|a| a.note.as_ref().map(|n| format!("{}: {n}", a.path)))
            .collect();
        // One preview per edited file, from its final content, spanning every
        // region this batch changed in it.
        let mut regions: Vec<(PathBuf, String, usize, usize)> = Vec::new();
        for a in &applied {
            let Some((path, first, lines)) = &a.touched else {
                continue;
            };
            let end = first + lines.max(&1) - 1;
            match regions.iter_mut().find(|r| &r.0 == path) {
                Some(r) => {
                    r.2 = r.2.min(*first);
                    r.3 = r.3.max(end);
                }
                None => regions.push((path.clone(), a.path.clone(), *first, end)),
            }
        }
        let preview = regions
            .iter()
            .filter_map(|(path, display, first, end)| {
                let content = staged.get(path)?.after.as_deref()?;
                Some(format!(
                    "{display}\n{}",
                    numbered_preview(content, *first, end + 1 - first)
                ))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let summary = applied
            .iter()
            .map(|a| match a.action {
                "delete" => format!("deleted {}", a.path),
                "move" => format!("moved {}", a.path),
                "create" => format!("created {} ({} lines)", a.path, a.added),
                _ => format!("edited {} (+{} -{})", a.path, a.added, a.removed),
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut out = json!({ "summary": summary });
        if !notes.is_empty() {
            out["notes"] = json!(notes.join("; "));
        }
        if !preview.is_empty() {
            out["changed_lines"] = json!(preview);
        }
        Ok(ToolResult::success(out))
    }
}

fn load<'a>(
    ctx: &ToolContext,
    staged: &'a mut HashMap<PathBuf, Staged>,
    order: &mut Vec<PathBuf>,
    display: &str,
) -> Result<&'a mut Staged, String> {
    let path = ctx
        .resolve_workspace_path(display)
        .map_err(|e| e.to_string())?;
    if crate::vault::is_protected_path(&path) {
        return Err("the vault file cannot be changed with this tool.".to_string());
    }
    if !staged.contains_key(&path) {
        let before = if path.is_file() {
            match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(format!(
                        "`{display}` is not a UTF-8 text file; use bash for binary files."
                    ));
                }
                Err(e) => return Err(format!("cannot read `{display}`: {e}")),
            }
        } else if path.exists() {
            return Err(format!(
                "`{display}` is a directory; change_files only handles files. Remove or move a directory with bash (`rm -rf`, `mv`, or the tool's own clean command)."
            ));
        } else {
            None
        };
        staged.insert(
            path.clone(),
            Staged {
                display: display.to_string(),
                after: before.clone(),
                before,
            },
        );
        order.push(path.clone());
    }
    Ok(staged.get_mut(&path).expect("inserted above"))
}

fn line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.lines().count() }
}

/// Numbered view of the lines a change touched, plus a little context.
fn numbered_preview(content: &str, first_line: usize, changed_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = first_line.saturating_sub(1 + PREVIEW_CONTEXT).min(lines.len());
    let end = (first_line.saturating_sub(1) + changed_lines.max(1) + PREVIEW_CONTEXT).min(lines.len());
    let mut out = Vec::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        if out.len() >= PREVIEW_MAX_LINES {
            out.push(format!("… ({} more lines)", end - start - PREVIEW_MAX_LINES));
            break;
        }
        out.push(format!("{:>6}\t{line}", start + i + 1));
    }
    out.join("\n")
}

/// Length of a `cat -n` / `rg -n` style line-number prefix on `line`, if any.
fn line_number_prefix(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let sep = trimmed[digits..].chars().next()?;
    if !matches!(sep, '\t' | ':' | '│' | '|') {
        return None;
    }
    Some(line.len() - trimmed.len() + digits + sep.len_utf8())
}

/// A weak model sometimes pastes lines copied from numbered output. When every
/// non-empty line carries a line-number prefix, strip them.
fn strip_line_numbers(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines
        .iter()
        .any(|l| !l.trim().is_empty() && line_number_prefix(l).is_none())
    {
        return None;
    }
    if !lines.iter().any(|l| line_number_prefix(l).is_some()) {
        return None;
    }
    Some(
        lines
            .iter()
            .map(|l| line_number_prefix(l).map(|n| &l[n..]).unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn stage_change(
    ctx: &ToolContext,
    staged: &mut HashMap<PathBuf, Staged>,
    order: &mut Vec<PathBuf>,
    change: Change,
) -> Result<Applied, String> {
    match change {
        Change::Create {
            path,
            content,
            overwrite,
        } => {
            let entry = load(ctx, staged, order, &path)?;
            let existed = entry.after.is_some();
            if existed && !overwrite {
                return Err(format!(
                    "`{path}` already exists. Use replace to edit it, or set \"overwrite\": true to \
                     replace the whole file."
                ));
            }
            let removed = entry.after.as_deref().map(line_count).unwrap_or(0);
            let added = line_count(&content);
            entry.after = Some(content);
            Ok(Applied {
                action: "create",
                path,
                added,
                removed,
                touched: None,
                note: existed.then(|| "replaced the existing file".to_string()),
            })
        }
        Change::Replace {
            path,
            find,
            with,
            all,
        } => {
            let entry = load(ctx, staged, order, &path)?;
            let Some(current) = entry.after.clone() else {
                return Err(format!("`{path}` does not exist. Use create for a new file."));
            };
            if find.is_empty() {
                return Err("`find` is empty: copy the exact text to change.".to_string());
            }
            if find == with {
                return Err(
                    "`find` and `with` are identical, so this change does nothing. If the file \
                     already has the text you want, skip this change."
                        .to_string(),
                );
            }
            let mut note = None;
            let (find, with) = if current.contains(&find) {
                (find, with)
            } else {
                match strip_line_numbers(&find) {
                    Some(stripped) if current.contains(&stripped) => {
                        note = Some("line-number prefixes in `find` were ignored".to_string());
                        let with = strip_line_numbers(&with).unwrap_or(with);
                        (stripped, with)
                    }
                    _ => (find, with),
                }
            };
            let exact = current.matches(&find).count();
            let (updated, first_line, matched, replacements) = if exact > 0 {
                if exact > 1 && !all {
                    return Err(ambiguous_diagnostic(
                        &current,
                        match_lines(&current, &find),
                        exact,
                        &path,
                        "",
                    ));
                }
                let offset = current.find(&find).unwrap_or(0);
                let first_line = current[..offset].matches('\n').count() + 1;
                let updated = if all {
                    current.replace(&find, &with)
                } else {
                    current.replacen(&find, &with, 1)
                };
                (updated, first_line, find.clone(), exact)
            } else if all {
                return Err(edit_diagnostic(&current, &find, &path));
            } else {
                match flexible_replace(&current, &find, &with) {
                    Flex::Replaced {
                        updated,
                        start_line,
                        matched_text,
                    } => {
                        note.get_or_insert_with(|| {
                            "matched after ignoring whitespace differences".to_string()
                        });
                        (updated, start_line, matched_text, 1)
                    }
                    Flex::Ambiguous(starts) => {
                        let n = starts.len();
                        return Err(ambiguous_diagnostic(
                            &current,
                            starts,
                            n,
                            &path,
                            " once whitespace is ignored",
                        ));
                    }
                    Flex::NoMatch => return Err(edit_diagnostic(&current, &find, &path)),
                }
            };
            let removed = line_count(&matched) * replacements;
            let added = line_count(&with) * replacements;
            let touched = if replacements == 1 {
                let resolved = ctx.resolve_workspace_path(&path).map_err(|e| e.to_string())?;
                Some((resolved, first_line, line_count(&with)))
            } else {
                note = Some(format!("replaced {replacements} occurrences"));
                None
            };
            entry.after = Some(updated);
            Ok(Applied {
                action: "replace",
                path,
                added,
                removed,
                touched,
                note,
            })
        }
        Change::Delete { path } => {
            let entry = load(ctx, staged, order, &path)?;
            let Some(current) = entry.after.take() else {
                return Err(format!("`{path}` does not exist."));
            };
            Ok(Applied {
                action: "delete",
                path,
                added: 0,
                removed: line_count(&current),
                touched: None,
                note: None,
            })
        }
        Change::Move { path, to } => {
            let Some(content) = load(ctx, staged, order, &path)?.after.clone() else {
                return Err(format!("`{path}` does not exist."));
            };
            let target = load(ctx, staged, order, &to)?;
            if target.after.is_some() {
                return Err(format!("`{to}` already exists; move will not overwrite it."));
            }
            target.after = Some(content);
            load(ctx, staged, order, &path)?.after = None;
            Ok(Applied {
                action: "move",
                path: format!("{path} → {to}"),
                added: 0,
                removed: 0,
                touched: None,
                note: None,
            })
        }
    }
}

/// Write every staged file. If a write fails part-way, files already written are
/// restored to their original contents so the batch stays all-or-nothing.
async fn commit(
    ctx: &ToolContext,
    staged: &HashMap<PathBuf, Staged>,
    order: &[PathBuf],
) -> Result<(), ToolError> {
    let mut done: Vec<&PathBuf> = Vec::new();
    for path in order {
        let entry = &staged[path];
        if entry.before == entry.after {
            continue;
        }
        let result = match &entry.after {
            Some(text) => {
                let parent_ok = match path.parent() {
                    Some(parent) => tokio::fs::create_dir_all(parent).await,
                    None => Ok(()),
                };
                match parent_ok {
                    Ok(()) => tokio::fs::write(path, text).await,
                    Err(e) => Err(e),
                }
            }
            None => tokio::fs::remove_file(path).await,
        };
        if let Err(error) = result {
            for written in done.iter().rev() {
                let original = &staged[*written];
                let _ = match &original.before {
                    Some(text) => tokio::fs::write(written, text).await,
                    None => tokio::fs::remove_file(written).await,
                };
            }
            return Err(ToolError::msg(format!(
                "writing `{}` failed: {error}. Earlier files in this batch were restored; no \
                 files were changed.",
                entry.display
            )));
        }
        done.push(path);
    }
    for path in done {
        ctx.record_change(path);
    }
    Ok(())
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
        "`find` matches {n} places in `{path}`{qualifier}. Either set \"all\": true to change \
         every occurrence, or make `find` unique by adding the line ABOVE the one match you want:"
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
        "`find` text not found in `{path}`. Matching ignores only whitespace (indentation, line breaks, repeated spaces); every other character must be exact. Look at the current text (sed -n or rg -n in bash), copy a small unique snippet exactly, and do not resend the same near-match."
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
}
