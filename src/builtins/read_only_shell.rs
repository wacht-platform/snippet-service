//! A `bash` that only reads, for sessions that must not change files (an
//! agent's inbox). Every segment of a command must start with an allow-listed
//! reader, and redirects other than to /dev/null are refused.

use super::*;
use async_trait::async_trait;
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolResult};

pub struct ReadOnlyBashTool;

const READERS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "rg", "grep", "find", "tree", "stat", "file", "du", "df",
    "pwd", "echo", "printf", "date", "whoami", "uname", "which", "jq", "sort", "uniq", "cut",
    "tr", "sed", "basename", "dirname", "realpath", "readlink", "cd", "true", "column", "nl",
    "diff", "git",
];

const GIT_READS: &[&str] = &[
    "log", "show", "diff", "status", "rev-parse", "ls-files", "blame", "grep", "describe", "shortlog",
];

/// Why a command is refused, or `Ok` when it only reads.
pub fn read_only_verdict(command: &str) -> Result<(), String> {
    if command.contains('`') || command.contains("$(") || command.contains("<(") || command.contains(">(") {
        return Err("command substitution isn't allowed in this read-only shell".into());
    }
    let stripped = command
        .replace("2>&1", " ")
        .replace("&>/dev/null", " ")
        .replace("2>/dev/null", " ")
        .replace(">/dev/null", " ");
    if stripped.contains('>') {
        return Err("redirecting output to a file isn't allowed here; this session doesn't change files".into());
    }
    for segment in stripped.split(['|', ';', '&', '\n']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .skip_while(|w| w.contains('=') && !w.starts_with('-'))
            .collect();
        let Some(first) = words.first() else { continue };
        let program = first.rsplit('/').next().unwrap_or(first);
        if !READERS.contains(&program) {
            return Err(format!("`{program}` isn't a read-only command here"));
        }
        let args = &words[1..];
        let refused = match program {
            "git" => !git_subcommand(args).is_some_and(|sub| GIT_READS.contains(&sub))
                || args.iter().any(|a| a.starts_with("--output")),
            "sed" => args.iter().any(|a| a.starts_with("-i") || *a == "--in-place"),
            "find" => args.iter().any(|a| matches!(*a, "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint" | "-fprintf" | "-fls")),
            _ => false,
        };
        if refused {
            return Err(format!("`{}` changes things; this shell only reads", words.join(" ")));
        }
    }
    Ok(())
}

fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                it.next();
            }
            a if a.starts_with('-') => {}
            a => return Some(a),
        }
    }
    None
}

#[async_trait]
impl Tool for ReadOnlyBashTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "bash".to_string(),
            description: "Read-only shell for looking things up: ls, cat, head, rg, find, jq, and git log/show/diff/status. Anything that writes, moves, deletes, installs or builds is refused; work that changes files goes to Mission Control.".to_string(),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string"},
                    "label": {"type": "string", "description": "a few words on what it's for"},
                    "max_lines": {"type": "integer", "minimum": 1}
                }),
                &["command", "label"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let command = arguments.get("command").and_then(Value::as_str).unwrap_or("");
        if let Err(why) = read_only_verdict(command) {
            return Ok(ToolResult::error(why));
        }
        let mut forwarded = arguments.clone();
        if let Some(map) = forwarded.as_object_mut() {
            map.remove("background");
        }
        BashTool.execute(ctx, forwarded).await
    }
}

#[cfg(test)]
mod tests {
    use super::read_only_verdict;

    #[test]
    fn readers_pass_and_writers_are_refused() {
        for ok in [
            "ls -la src | head -20",
            "rg -n foo src 2>/dev/null; git log --oneline -5",
            "git -C /repo status --short && cat README.md",
            "sed -n 1,40p src/lib.rs",
            "LC_ALL=C sort file.txt | uniq -c",
        ] {
            assert!(read_only_verdict(ok).is_ok(), "{ok}");
        }
        for bad in [
            "echo hi > notes.md",
            "rm -rf target",
            "git commit -am x",
            "git push",
            "sed -i s/a/b/ f",
            "find . -name '*.tmp' -delete",
            "cat $(which ls)",
            "ls && cargo build",
            "python3 -c 'open(1)'",
            "cat a | tee b",
        ] {
            assert!(read_only_verdict(bad).is_err(), "{bad}");
        }
    }
}
