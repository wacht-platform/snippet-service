use super::*;

use std::collections::{HashMap, HashSet};
use std::process::Stdio;

use axum::extract::{Path as AxPath, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use crate::config::InferenceProfileConfig;
use crate::llm::{ModelOutput, StreamBuffer, StreamHandle};

const MCP_SERVER: &str = "snippet";
const MCP_PREFIX: &str = "mcp__snippet__";
const QUIET_TOOLS: [&str; 3] = ["update_plan", "ask_user", "set_session_title"];
const COMMAND_MARK: &str = "\u{1}command\u{1}";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CliCompaction {
    Idle,
    Saving,
    Compacting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliKind {
    ClaudeCode,
    Antigravity,
}

impl CliKind {
    fn memory_save_after(self) -> std::time::Duration {
        match self {
            CliKind::ClaudeCode => std::time::Duration::from_secs(50 * 60),
            CliKind::Antigravity => std::time::Duration::from_secs(3 * 60),
        }
    }

    fn of(provider: &str) -> Self {
        if provider == "antigravity" { Self::Antigravity } else { Self::ClaudeCode }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Antigravity => "Antigravity",
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Antigravity => "antigravity",
        }
    }

    fn user_line(self, text: &str) -> Value {
        let message = json!({"role": "user", "content": text});
        match self {
            Self::ClaudeCode => json!({"type": "user", "message": message}),
            Self::Antigravity => json!({"event": "user", "message": message}),
        }
    }
}

struct CliSession {
    kind: CliKind,
    key: String,
    id: Option<String>,
    started: bool,
    synced: usize,
    tool_guide: String,
    forking: bool,
    mark: String,
    tokens: u64,
    compactions: u64,
}

impl CliSession {
    fn open(kind: CliKind, snippet_session: &str, reuse: bool) -> Self {
        let key = sanitize_key(snippet_session);
        let file = Self::sidecar(kind, &key);
        let raw = std::fs::read_to_string(&file).unwrap_or_default();
        let mut lines = raw.lines();
        let existing = lines
            .next()
            .map(|id| id.trim().to_string())
            .filter(|id| reuse && !id.is_empty());
        let synced = lines.next().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        let (mut forking, mut mark, mut tokens, mut compactions) = (false, String::new(), 0u64, 0u64);
        for line in lines {
            let line = line.trim();
            if line == "fork" {
                forking = true;
            } else if let Some(v) = line.strip_prefix("m=") {
                mark = v.to_string();
            } else if let Some(v) = line.strip_prefix("t=") {
                tokens = v.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("c=") {
                compactions = v.parse().unwrap_or(0);
            }
        }
        let fresh = |id: Option<String>| Self {
            kind,
            key: key.clone(),
            id,
            started: false,
            synced: 0,
            tool_guide: String::new(),
            forking: false,
            mark: String::new(),
            tokens: 0,
            compactions: 0,
        };
        let session = match (kind, existing) {
            (_, Some(id)) => Self {
                kind,
                key: key.clone(),
                id: Some(id),
                started: true,
                synced,
                tool_guide: String::new(),
                forking,
                mark,
                tokens,
                compactions,
            },
            (CliKind::ClaudeCode, None) => fresh(Some(uuid::Uuid::new_v4().to_string())),
            (CliKind::Antigravity, None) => fresh(None),
        };
        if !session.started {
            session.save();
        }
        session
    }

    fn sidecar(kind: CliKind, key: &str) -> PathBuf {
        let dir = crate::config::snippet_home().join(kind.dir()).join("sessions");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(key)
    }

    fn save(&self) {
        let file = Self::sidecar(self.kind, &self.key);
        match self.id.as_deref() {
            Some(id) => {
                let flag = if self.forking { "\nfork" } else { "" };
                let _ = std::fs::write(
                    file,
                    format!(
                        "{id}\n{}{flag}\nm={}\nt={}\nc={}",
                        self.synced, self.mark, self.tokens, self.compactions
                    ),
                );
            }
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    fn adopt(&mut self, id: &str) {
        self.started = true;
        if self.id.as_deref() != Some(id) {
            self.id = Some(id.to_string());
            self.save();
        }
    }

    fn mark_synced(&mut self, messages: &[HarnessMessage], compactions: u64, tokens: u64) {
        let upto = messages.len();
        let mark = transcript_mark(messages, upto);
        if self.synced != upto || self.mark != mark || self.compactions != compactions || self.tokens != tokens {
            self.synced = upto;
            self.mark = mark;
            self.compactions = compactions;
            self.tokens = tokens;
            self.save();
        }
    }

    fn restart(&mut self) {
        self.id = match self.kind {
            CliKind::ClaudeCode => Some(uuid::Uuid::new_v4().to_string()),
            CliKind::Antigravity => None,
        };
        self.started = false;
        self.forking = false;
        self.synced = 0;
        self.mark.clear();
        self.tokens = 0;
        self.save();
    }
}

fn transcript_mark(messages: &[HarnessMessage], upto: usize) -> String {
    use std::hash::{Hash, Hasher};
    if upto == 0 || upto > messages.len() {
        return String::new();
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    upto.hash(&mut h);
    match &messages[upto - 1] {
        HarnessMessage::User { content } => ("user", content.as_str()).hash(&mut h),
        HarnessMessage::Assistant { content, tool_calls } => {
            ("assistant", content.as_str()).hash(&mut h);
            for call in tool_calls {
                call.name.hash(&mut h);
            }
        }
        HarnessMessage::ToolResult { tool_name, tool_call_id, .. } => {
            ("tool", tool_name.as_str(), tool_call_id.as_str()).hash(&mut h)
        }
        HarnessMessage::Summary { kind, content } => ("summary", kind.as_str(), content.as_str()).hash(&mut h),
        HarnessMessage::System { .. } => "system".hash(&mut h),
    }
    format!("{:016x}", h.finish())
}

const HANDOFF_BUDGET: usize = 40_000;

fn clip_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text.to_string()
    }
}

fn call_target(arguments: &Value) -> Option<String> {
    for key in ["file_path", "path", "command", "cmd", "query", "pattern", "url", "description", "title"] {
        if let Some(v) = arguments.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()) {
            return Some(clip_chars(&v.replace('\n', " "), 100));
        }
    }
    None
}

fn touched_paths(arguments: &Value, out: &mut Vec<String>) {
    let mut push = |p: &str| {
        let p = p.trim();
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    for key in ["file_path", "path", "new_path"] {
        if let Some(p) = arguments.get(key).and_then(Value::as_str) {
            push(p);
        }
    }
    for key in ["paths", "files", "changes"] {
        if let Some(list) = arguments.get(key).and_then(Value::as_array) {
            for item in list {
                match item {
                    Value::String(p) => push(p),
                    Value::Object(_) => {
                        for k in ["path", "file_path", "new_path"] {
                            if let Some(p) = item.get(k).and_then(Value::as_str) {
                                push(p);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn writes_files(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["edit", "write", "change", "patch", "create", "delete", "move", "rename", "replace"]
        .iter()
        .any(|w| n.contains(w))
}

struct Handoff {
    text: String,
    dropped: usize,
}

const DURABLE_SAVE: &str = "First save to global, project or agent memory what holds beyond this conversation: user preferences and rules, decisions and why, root causes, project facts and conventions. This conversation's own working state belongs in the summary, not in memory. Replace stale entries instead of adding duplicates, remove what is no longer true, and commit. An agent curates its own agent memory here and nowhere else: keep what still earns its place, rewrite what changed, and evict what is stale. ";

fn handoff(messages: &[HarnessMessage], fresh: bool) -> Option<Handoff> {
    let mut entries: Vec<String> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut summary: Option<String> = None;
    for m in messages {
        match m {
            HarnessMessage::User { content } if !content.trim().is_empty() => {
                entries.push(format!("user: {}", clip_chars(content, 2_000)));
            }
            HarnessMessage::Assistant { content, tool_calls } => {
                let mut line = format!("assistant: {}", clip_chars(content, 2_000));
                for call in tool_calls {
                    match call_target(&call.arguments) {
                        Some(t) => line.push_str(&format!("\n  · {} {t}", call.name)),
                        None => line.push_str(&format!("\n  · {}", call.name)),
                    }
                    if writes_files(&call.name) {
                        touched_paths(&call.arguments, &mut touched);
                    }
                }
                if !content.trim().is_empty() || !tool_calls.is_empty() {
                    entries.push(line);
                }
            }
            HarnessMessage::Summary { content, .. } if !content.trim().is_empty() => {
                summary = Some(clip_chars(content, 8_000));
            }
            _ => {}
        }
    }
    if entries.is_empty() && summary.is_none() {
        return None;
    }
    let mut used = 0;
    let mut keep = entries.len();
    while keep > 0 && used + entries[keep - 1].len() <= HANDOFF_BUDGET {
        used += entries[keep - 1].len();
        keep -= 1;
    }
    let dropped = keep;
    let dropped_requests: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            HarnessMessage::User { content } if !content.trim().is_empty() => Some(content.as_str()),
            _ => None,
        })
        .take(entries[..keep].iter().filter(|e| e.starts_with("user: ")).count())
        .map(|c| clip_chars(&c.replace('\n', " "), 200))
        .collect();
    entries.drain(..keep);
    let mut out = String::from("<conversation_so_far>\n");
    out.push_str(if fresh {
        "This session's conversation so far, from before this runtime joined it. Treat it as your own history.\n"
    } else {
        "What happened in this session while another runtime was handling it, since your last turn. Treat it as your own history.\n"
    });
    if let Some(summary) = summary {
        out.push_str(&format!("\nSummary of earlier work:\n{summary}\n"));
    }
    if !dropped_requests.is_empty() {
        let skip = dropped_requests.len().saturating_sub(40);
        let earlier = if skip > 0 { format!("({skip} earlier requests not listed)\n") } else { String::new() };
        out.push_str(&format!(
            "\nEarlier requests, oldest first ({dropped} older messages are not shown in full):\n{earlier}- {}\n",
            dropped_requests[skip..].join("\n- ")
        ));
    }
    if !entries.is_empty() {
        out.push_str("\n");
        out.push_str(&entries.join("\n\n"));
        out.push('\n');
    }
    if !touched.is_empty() {
        let shown: Vec<&String> = touched.iter().take(40).collect();
        out.push_str(&format!(
            "\nFiles changed in that time{}:\n{}\n",
            if touched.len() > shown.len() { format!(" (first {})", shown.len()) } else { String::new() },
            shown.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
        ));
    }
    out.push_str("</conversation_so_far>\n\n");
    Some(Handoff { text: out, dropped })
}

#[derive(Default)]
struct CliParse {
    tool_names: HashMap<String, String>,
    agy_text: HashMap<i64, String>,
    agy_tools: HashSet<i64>,
    calls: Vec<crate::llm::TokenUsage>,
    claude_call: Option<crate::llm::TokenUsage>,
    served: ServedResults,
    envelope_marker: Option<PathBuf>,
}

pub struct CliAgentModel {
    profile: InferenceProfileConfig,
}

impl CliAgentModel {
    pub fn new(profile: InferenceProfileConfig) -> Self {
        Self { profile }
    }
}

#[async_trait::async_trait]
impl AgentModel for CliAgentModel {
    async fn generate(
        &mut self,
        _messages: &[HarnessMessage],
        _tools: &[NativeToolDefinition],
        _force_tool: bool,
        _sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        Err(ToolError::msg(format!(
            "the {} profile runs through its CLI and can't serve a direct model call",
            self.profile.provider
        )))
    }

    fn supports_images(&self) -> bool {
        true
    }

    fn cli_agent_profile(&self) -> Option<InferenceProfileConfig> {
        Some(self.profile.clone())
    }
}

async fn run_probe(bin: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        Command::new(bin).args(args).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn claude_rate_limits_path() -> PathBuf {
    crate::config::snippet_home().join("claude-code/rate_limits.json")
}

pub fn claude_rate_limits() -> Option<crate::llm::RateLimitSnapshot> {
    serde_json::from_str(&std::fs::read_to_string(claude_rate_limits_path()).ok()?).ok()
}

pub async fn cli_agent_status(provider: &str) -> Value {
    let kind = CliKind::of(provider);
    let bin = match kind {
        CliKind::ClaudeCode => claude_binary(),
        CliKind::Antigravity => crate::antigravity::binary(),
    };
    let Some(bin) = bin else {
        return json!({"provider": provider, "label": kind.label(), "installed": false, "signed_in": false});
    };
    let version = run_probe(&bin, &["--version"])
        .await
        .map(|v| v.split_whitespace().next().unwrap_or("").to_string());
    let (signed_in, account) = match kind {
        CliKind::ClaudeCode => {
            let status = run_probe(&bin, &["auth", "status"])
                .await
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or(Value::Null);
            (
                status.get("loggedIn").and_then(Value::as_bool).unwrap_or(false),
                status.get("authMethod").and_then(Value::as_str).map(str::to_string),
            )
        }
        CliKind::Antigravity => (crate::antigravity::models().await.is_ok(), None),
    };
    json!({
        "provider": provider,
        "label": kind.label(),
        "installed": true,
        "path": bin.display().to_string(),
        "version": version,
        "signed_in": signed_in,
        "account": account,
    })
}

struct McpCall {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Value>,
}

type SteerQueue = std::sync::Arc<std::sync::Mutex<Vec<String>>>;
type ServedResults = std::sync::Arc<std::sync::Mutex<HashMap<String, std::collections::VecDeque<Value>>>>;

#[derive(Clone)]
struct McpState {
    token: String,
    tools: Vec<NativeToolDefinition>,
    calls: mpsc::UnboundedSender<McpCall>,
    steers: SteerQueue,
    served: ServedResults,
    images: bool,
}

struct CliProcess {
    child: Child,
    stdin: ChildStdin,
    account: Option<String>,
}

fn claude_account_token(id: &str) -> Option<String> {
    if id == crate::accounts::MAIN {
        return None;
    }
    let raw = crate::accounts::read_credential("claude-code", id)?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("token").and_then(Value::as_str).map(str::to_string)
}

enum CliLine {
    Event(u64, Value),
    Exited(u64, String),
}

fn claude_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNIPPET_CLAUDE_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("claude"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [home.join(".local/bin/claude"), home.join(".claude/local/claude")]
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn sanitize_key(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

fn tool_prompt_preamble(kind: CliKind) -> String {
    match kind {
        CliKind::ClaudeCode => format!(
            "## Tools in this runtime\n\nThe tools named in this prompt are served to you as `{MCP_PREFIX}<name>`: where it says `bash`, call `{MCP_PREFIX}bash`, and so on. These are your only tools: you have no built-in ones in this session. Read and search files with `bash` (cat, sed -n, rg, ls), and make every change, command, question and delegation through the snippet tools."
        ),
        CliKind::Antigravity => format!(
            "## Tools in this runtime\n\nThe tools named in these instructions are served by the `{}` MCP server: call them with call_mcp_tool, ServerName `{}` and ToolName exactly as named here (`bash`, `change_files`, `update_plan`, `ask_user`, `delegate_task`, and the rest). Every call_mcp_tool call also needs `toolSummary` (a few words on what the call does) and `toolAction` (a short verb phrase, such as `Running tests`); a call without them is rejected before it runs. These are your only working tools: every built-in tool of yours is switched off in this session. Read and search files with the snippet `bash` tool (cat, sed -n, rg, ls), research with its `web_search` and `web_read`, and make every change, command, question and delegation through the snippet tools. The one exception: when a tool result says its output was saved to a file inside your own Antigravity folder, read that file with view_file.",
            crate::antigravity::MCP_SERVER_NAME,
            crate::antigravity::MCP_SERVER_NAME,
        ),
    }
}

fn tool_guide(defs: &[NativeToolDefinition]) -> String {
    let mut out = String::from("## Snippet tools and their arguments\n\nCall each through call_mcp_tool with ServerName `snippet_snippet`, plus `toolSummary` and `toolAction` on every call. Pass every required argument by its exact name; optional ones in brackets.\n");
    for def in defs {
        let schema = &def.input_schema;
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let args: Vec<String> = schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| {
                props
                    .iter()
                    .map(|(name, prop)| {
                        let ty = prop.get("type").and_then(Value::as_str).unwrap_or("any");
                        if required.contains(&name.as_str()) {
                            format!("{name}: {ty}")
                        } else {
                            format!("[{name}: {ty}]")
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let summary: String = def
            .description
            .split(". ")
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        out.push_str(&format!("- `{}`({}) — {}\n", def.name, args.join(", "), summary.trim_end_matches('.')));
    }
    out
}

fn vault_names(arguments: &Value) -> Vec<String> {
    arguments
        .get("command")
        .and_then(Value::as_str)
        .map(|command| crate::vault::Vault::load().referenced_names(command))
        .unwrap_or_default()
}

fn unwrap_custom(name: String, arguments: Value) -> (String, Value) {
    if name != "custom_tool" {
        return (name, arguments);
    }
    let inner = arguments.get("name").and_then(Value::as_str).unwrap_or("custom_tool").to_string();
    let args = arguments.get("arguments").cloned().unwrap_or_else(|| json!({}));
    (inner, args)
}

const IMAGE_PATH_KEY: &str = "__image_path";
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

fn image_block(path: &str) -> Option<(&'static str, String)> {
    use base64::Engine;
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let (mime, bytes) = crate::builtins::fs::image_for_model(&bytes)?;
    Some((mime, base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// A user message for the CLI, with the app's `[attached image — …: /path]`
/// markers turned into the images themselves for Claude Code, and into a
/// pointer to its own file viewer for agy, whose input takes text only.
fn user_line_with_attachments(kind: CliKind, text: &str) -> Value {
    static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[attached (image|file) — [^\]/]*(/[^\]]+)\]").expect("valid regex")
    });
    if !MARKER.is_match(text) {
        return kind.user_line(text);
    }
    let mut images = Vec::new();
    let text = MARKER
        .replace_all(text, |caps: &regex::Captures| {
            let path = caps[2].trim().to_string();
            let kind_word = &caps[1];
            if kind_word == "image" {
                images.push(path.clone());
                if kind == CliKind::Antigravity {
                    return format!("[attached image: {path} — open it with view_file to see it]");
                }
            }
            format!("[attached {kind_word}: {path}]")
        })
        .into_owned();
    if kind != CliKind::ClaudeCode {
        return kind.user_line(&text);
    }
    let mut content = vec![json!({"type": "text", "text": text})];
    for path in &images {
        if let Some((mime, data)) = image_block(path) {
            content.push(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}));
        }
    }
    json!({"type": "user", "message": {"role": "user", "content": content}})
}

fn error_result(code: &str, message: &str) -> Value {
    json!({"schema_version": 1, "status": "error", "error": {"code": code, "message": message}})
}

fn tool_result_from_block(block: &Value) -> Value {
    let text = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.get("status").is_some()
    {
        return value;
    }
    let failed = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    if failed {
        error_result("tool_error", &text)
    } else {
        json!({"schema_version": 1, "status": "success", "data": {"output": text}})
    }
}

async fn mcp_handler(
    AxState(state): AxState<McpState>,
    AxPath(token): AxPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    match body {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(reply) = mcp_dispatch(&state, item).await {
                    out.push(reply);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        item => match mcp_dispatch(&state, item).await {
            Some(reply) => Json(reply).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

async fn mcp_dispatch(state: &McpState, request: Value) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": MCP_SERVER, "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": state.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })).collect::<Vec<_>>()
        }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (reply, rx) = oneshot::channel();
            let name_for_record = name.clone();
            if state.calls.send(McpCall { name, arguments, reply }).is_err() {
                return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "session ended"}}));
            }
            let mut value = rx
                .await
                .unwrap_or_else(|_| error_result("interrupted", "The call was interrupted."));
            let image_path = value
                .as_object_mut()
                .and_then(|o| o.remove(IMAGE_PATH_KEY))
                .and_then(|v| v.as_str().map(str::to_string));
            let vault = crate::vault::Vault::load();
            if !vault.is_empty() {
                vault.scrub_value(&mut value);
            }
            if let Ok(mut served) = state.served.lock() {
                let queue = served.entry(name_for_record).or_default();
                queue.push_back(value.clone());
                if queue.len() > 32 {
                    queue.pop_front();
                }
            }
            let failed = value.get("status").and_then(Value::as_str) == Some("error");
            let mut content = vec![json!({"type": "text", "text": crate::llm::render_tool_result(&value)})];
            if let Some(path) = image_path {
                match image_block(&path).filter(|_| state.images) {
                    Some((mime, data)) => content.push(json!({"type": "image", "data": data, "mimeType": mime})),
                    None => content.push(json!({"type": "text", "text": format!("To see the image itself, open {path} with your own file viewer (view_file).")})),
                }
            }
            if let Some(marker) = crate::antigravity::envelope_marker(&format!("/mcp/{}", state.token))
                && std::fs::remove_file(&marker).is_ok()
            {
                content.push(json!({"type": "text", "text": "<system-reminder>Your previous call_mcp_tool call was rejected before it ran because it left out toolSummary or toolAction. Include both on every call.</system-reminder>"}));
            }
            let steers: Vec<String> = state.steers.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
            if !steers.is_empty() {
                content.push(json!({"type": "text", "text": format!(
                    "<user_message_mid_turn>\nThe user sent this while you were working. It takes priority: adjust what you are doing now.\n{}\n</user_message_mid_turn>",
                    steers.join("\n\n")
                )}));
            }
            json!({
                "content": content,
                "isError": failed,
            })
        }
        _ => {
            return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Give a forked Snippet session its own copy of the source's CLI
/// conversation, so it resumes with the real history and its prompt cache
/// instead of a replayed digest. Only for a fork at the latest point: neither
/// CLI can branch an earlier message, so an earlier fork keeps the replay.
/// Claude Code forks on the session's first run (--fork-session); agy's
/// conversation file is copied now.
pub fn fork_cli_conversations(source: &str, fork: &str, source_len: usize, fork_len: usize) {
    if fork_len < source_len {
        return;
    }
    for kind in [CliKind::ClaudeCode, CliKind::Antigravity] {
        let raw = std::fs::read_to_string(CliSession::sidecar(kind, &sanitize_key(source))).unwrap_or_default();
        let mut lines = raw.lines();
        let Some(id) = lines.next().map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        let synced: usize = lines.next().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        let target = CliSession::sidecar(kind, &sanitize_key(fork));
        let written = match kind {
            CliKind::ClaudeCode => std::fs::write(&target, format!("{id}\n{synced}\nfork")).map_err(|e| e.to_string()),
            CliKind::Antigravity => crate::antigravity::fork_conversation(id)
                .and_then(|copy| std::fs::write(&target, format!("{copy}\n{synced}")).map_err(|e| e.to_string())),
        };
        if let Err(e) = written {
            eprintln!("fork {} conversation for {fork}: {e}", kind.label());
        }
    }
}

async fn start_mcp_server(
    tools: Vec<NativeToolDefinition>,
    steers: SteerQueue,
    served: ServedResults,
    images: bool,
) -> Result<(String, mpsc::UnboundedReceiver<McpCall>), String> {
    let (calls, rx) = mpsc::unbounded_channel();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = McpState { token: token.clone(), tools, calls, steers, served, images };
    let app = Router::new()
        .route(
            "/mcp/{token}",
            post(mcp_handler).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind tool bridge: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}/mcp/{token}"), rx))
}

impl CodingHarness {
    fn cli_meta_definitions(&self, once: bool, lanes: bool) -> Vec<NativeToolDefinition> {
        meta::conversation_meta_definitions_for(false, lanes)
            .into_iter()
            .filter(|d| !once || !matches!(d.name.as_str(), "ask_user" | "monitor"))
            .filter(|d| !self.config.hidden_meta.contains(&d.name.as_str()))
            .collect()
    }

    fn cli_system_prompt(&self, state: &HarnessState, session: &CliSession) -> String {
        let kind = session.kind;
        let system = state
            .messages
            .iter()
            .find_map(|m| match m {
                HarnessMessage::System { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.config.system_prompt.clone());
        let mut prompt = format!("{system}\n\n{}", tool_prompt_preamble(kind));
        if !session.tool_guide.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&session.tool_guide);
        }
        prompt
    }

    fn memory_on(&self) -> bool {
        self.config.memory_enabled && self.context.lane_origin().is_none()
    }

    fn compaction_save_prompt(&self) -> Option<String> {
        self.memory_on().then(|| format!("[compaction] This conversation is about to be compacted. {}Then reply with one line saying what you saved.", DURABLE_SAVE))
    }

    fn idle_memory_prompt(&self) -> Option<String> {
        self.memory_on().then(|| "[memory] The user has stepped away. Before they're back, save what this conversation established since your last memory save that holds beyond it: user preferences and rules, decisions and why, root causes, project facts and conventions, in global, project or agent memory. Update or remove stale entries instead of adding duplicates, keep entries short, and commit. If nothing durable came up, change nothing. Don't continue the work. Then reply with one line saying what you saved, or that nothing needed saving.".to_string())
    }

    fn agy_compaction_prompt(&self) -> String {
        let save = if self.memory_on() { DURABLE_SAVE } else { "" };
        format!(
            "[compaction] This conversation is about to be compacted: it will continue in a fresh conversation that starts from your summary. {save}Then reply with only the summary, about 2k tokens: the user's requests in order (one line each), the goal and what counts as done, progress (done, in progress, left), decisions and constraints, the next steps and the exact values (paths, ids, error strings) they need."
        )
    }

    fn compact_command(&self) -> String {
        "/compact Keep the summary lean, about 2k tokens: the user's requests in order (one line each), the goal and what counts as done, progress (done, in progress, left), decisions and constraints, the next steps and the exact values (paths, ids, error strings) they need.".to_string()
    }

    fn effort_arg(profile: &InferenceProfileConfig) -> Option<&str> {
        profile
            .reasoning_effort
            .as_deref()
            .filter(|e| matches!(*e, "low" | "medium" | "high" | "xhigh" | "max"))
    }

    fn claude_command(
        &self,
        profile: &InferenceProfileConfig,
        prompt: &str,
        mcp_url: &str,
        session: &CliSession,
        account: Option<&str>,
    ) -> Result<Command, String> {
        let bin = claude_binary().ok_or_else(|| {
            "Claude Code isn't installed or isn't on PATH (set SNIPPET_CLAUDE_BIN to its path)".to_string()
        })?;
        let mcp_config = json!({"mcpServers": {MCP_SERVER: {"type": "http", "url": mcp_url}}});
        let mut cmd = Command::new(bin);
        cmd.current_dir(self.context.workspace_root())
            .arg("-p")
            .args(["--input-format", "stream-json", "--output-format", "stream-json"])
            .args(["--include-partial-messages", "--verbose"])
            .args(["--system-prompt", prompt])
            .args(["--mcp-config", &mcp_config.to_string(), "--strict-mcp-config"])
            .args(["--tools", ""])
            .args(["--allowedTools", &format!("mcp__{MCP_SERVER}")])
            .args(["--permission-prompts", "none"])
            .args(["--setting-sources", ""])
            .env("MCP_TOOL_TIMEOUT", "3600000")
            .env("MCP_TIMEOUT", "30000")
            .env("CLAUDE_CODE_ENTRYPOINT", "snippet");
        if let Some(token) = account.and_then(claude_account_token) {
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token);
        }
        if !profile.model.trim().is_empty() {
            cmd.args(["--model", profile.model.trim()]);
        }
        if let Some(effort) = Self::effort_arg(profile) {
            cmd.args(["--effort", effort]);
        }
        let compact_threshold = profile
            .context_window
            .saturating_mul(profile.compact_at_pct.clamp(1, 100) as u64)
            / 100;
        if compact_threshold > 0 {
            let auto_tokens = compact_threshold.clamp(100_000, 1_000_000);
            cmd.env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", auto_tokens.to_string());
        }
        if let Some(id) = session.id.as_deref() {
            cmd.args([if session.started { "--resume" } else { "--session-id" }, id]);
            if session.forking {
                cmd.arg("--fork-session");
            }
        }
        Ok(cmd)
    }

    fn antigravity_command(
        &self,
        profile: &InferenceProfileConfig,
        prompt: &str,
        mcp_url: &str,
        session: &CliSession,
    ) -> Result<Command, String> {
        let bin = crate::antigravity::binary().ok_or_else(|| {
            "The Antigravity CLI (agy) isn't installed or isn't on PATH (set SNIPPET_AGY_BIN to its path)".to_string()
        })?;
        let home = crate::antigravity::prepare_home(&session.key, mcp_url, prompt)?;
        let real = crate::antigravity::real_home().ok_or("HOME is not set")?;
        let xdg = |var: &str, fallback: &str| std::env::var_os(var).unwrap_or_else(|| real.join(fallback).into_os_string());
        let mut cmd = Command::new(bin);
        cmd.current_dir(self.context.workspace_root())
            .arg("--print=")
            .args(["--input-format", "stream-json", "--output-format", "stream-json"])
            .arg("--disable-slash-commands")
            .env("XDG_CONFIG_HOME", xdg("XDG_CONFIG_HOME", ".config"))
            .env("XDG_CACHE_HOME", xdg("XDG_CACHE_HOME", ".cache"))
            .env("XDG_DATA_HOME", xdg("XDG_DATA_HOME", ".local/share"))
            .env("HOME", home)
            .env(crate::antigravity::SESSION_ENV, mcp_url);
        let model = profile.model.trim();
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        if let Some(id) = session.id.as_deref() {
            cmd.args(["--conversation", id]);
        }
        Ok(cmd)
    }

    async fn spawn_cli(
        &self,
        profile: &InferenceProfileConfig,
        state: &HarnessState,
        mcp_url: &str,
        lines: &mpsc::UnboundedSender<CliLine>,
        generation: u64,
        session: &CliSession,
    ) -> Result<CliProcess, String> {
        let kind = session.kind;
        let prompt = self.cli_system_prompt(state, session);
        let account = match kind {
            CliKind::ClaudeCode => crate::accounts::active("claude-code"),
            CliKind::Antigravity => None,
        };
        let mut cmd = match kind {
            CliKind::ClaudeCode => self.claude_command(profile, &prompt, mcp_url, session, account.as_deref())?,
            CliKind::Antigravity => self.antigravity_command(profile, &prompt, mcp_url, session)?,
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let label = kind.label();
        let mut child = cmd.spawn().map_err(|e| format!("start {label}: {e}"))?;
        let stdin = child.stdin.take().ok_or(format!("{label} stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or(format!("{label} stdout unavailable"))?;
        let stderr = child.stderr.take();
        let tx = lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(CliLine::Event(generation, value));
                }
            }
            let mut tail = String::new();
            if let Some(stderr) = stderr {
                let mut err_reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), err_reader.next_line())
                        .await
                        .unwrap_or(Ok(None))
                {
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > 4000 {
                        tail.drain(..tail.len() - 4000);
                    }
                }
            }
            let _ = tx.send(CliLine::Exited(generation, tail.trim().to_string()));
        });
        Ok(CliProcess { child, stdin, account })
    }

    pub async fn run_cli_agent(
        &self,
        profile: InferenceProfileConfig,
        initial_request: Option<String>,
        input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
    ) -> Result<HarnessState, ToolError> {
        self.drive_cli(profile, initial_request, input_rx, factory, sink, false)
            .await
    }

    pub async fn run_cli_lane(
        &self,
        profile: InferenceProfileConfig,
        brief: String,
    ) -> Result<HarnessOutcome, ToolError> {
        let (_keep, input_rx) = mpsc::unbounded_channel();
        let state = self
            .drive_cli(profile, Some(brief), input_rx, None, None, true)
            .await?;
        if state.final_text.is_none() {
            let message = state
                .events
                .iter()
                .rev()
                .find_map(|e| match e {
                    HarnessEvent::ModelError { message } => Some(message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "The CLI agent finished without a summary".to_string());
            return Err(ToolError::msg(message));
        }
        Ok(HarnessOutcome {
            final_text: state.final_text,
            events: state.events,
            iterations: state.iterations,
        })
    }

    async fn drive_cli(
        &self,
        profile: InferenceProfileConfig,
        initial_request: Option<String>,
        mut input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
        once: bool,
    ) -> Result<HarnessState, ToolError> {
        let mut state = self.load_or_initialize_state(initial_request).await?;
        if matches!(
            state.status,
            HarnessStatus::Completed | HarnessStatus::Failed | HarnessStatus::Interrupted
        ) {
            state.status = if once { HarnessStatus::Running } else { HarnessStatus::Idle };
        }
        let (lane_tx, mut lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<crate::lanes::LaneProgress>();
        let lanes_enabled = !once && self.config.allow_lane_control && factory.is_some();
        let had_interrupted_lanes = state.lanes.iter().any(|lane| lane.status == LaneStatus::Running);
        let (lane_approval_tx, mut lane_approval_rx) =
            mpsc::unbounded_channel::<crate::lanes::LaneApprovalRequest>();
        let mut lanes = self
            .new_lane_manager(factory, lane_tx, progress_tx, &state)
            .with_approvals((!once).then_some(lane_approval_tx));
        let mut lane_approvals: std::collections::VecDeque<crate::lanes::LaneApprovalRequest> =
            std::collections::VecDeque::new();
        let mut lane_prompt_shown = false;
        if had_interrupted_lanes {
            lanes.resume_interrupted();
        }
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches = WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        watches.restore(&state.watches);
        let mut vars = LoopVars::default();
        let meta_defs = self.cli_meta_definitions(once, lanes_enabled);
        let meta_names: Vec<String> = meta_defs.iter().map(|d| d.name.clone()).collect();
        let mut bridge_tools = self.tools.definitions();
        if self.tools.custom_dir().is_some() {
            bridge_tools.push(NativeToolDefinition {
                name: "custom_tool".into(),
                description: "Run one of your custom tools by name, including one you created or changed during this conversation that isn't in your tool list yet.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The custom tool's name."},
                        "arguments": {"type": "object", "description": "Its arguments, as its parameters schema describes."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
            });
        }
        bridge_tools.extend(meta_defs);
        let guide = tool_guide(&bridge_tools);
        let steers: SteerQueue = Default::default();
        let served: ServedResults = Default::default();
        let (mcp_url, mut mcp_rx) = start_mcp_server(bridge_tools, steers.clone(), served.clone(), true).await.map_err(ToolError::msg)?;
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<CliLine>();
        let mut process: Option<CliProcess> = None;
        let mut account_limit: Option<i64> = None;
        let mut generation = 0u64;
        let kind = CliKind::of(&profile.provider);
        let session_key = self.session_id().unwrap_or_else(|| "main".to_string());
        let mut session = CliSession::open(kind, &session_key, self.config.resume);
        let mut last_mode_line = String::new();
        let mut last_task_note = String::new();
        let mut compaction = CliCompaction::Idle;
        let mut compact_wanted = false;
        let mut save_at: Option<tokio::time::Instant> = None;
        let mut memory_saving = false;
        let mut after_compaction: Option<String> = None;
        let mut cli_window: u64 = 0;
        if kind == CliKind::Antigravity {
            session.tool_guide = guide;
        }
        let mut parse = CliParse {
            served,
            envelope_marker: crate::antigravity::envelope_marker(&mcp_url),
            ..Default::default()
        };
        let mut pending_approval: Option<McpCall> = None;
        let mut pending_vault = false;
        let mut pending_custom: Option<crate::agent_tools::CustomTool> = None;
        let mut approval_index = 0usize;
        let mut running: FuturesUnordered<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> =
            FuturesUnordered::new();
        let mut outbox: Vec<String> = Vec::new();
        let mut held_envelopes: Vec<String> = Vec::new();

        if state.status == HarnessStatus::Running
            && let Some(text) = state.messages.iter().rev().find_map(|m| match m {
                HarnessMessage::User { content } => Some(content.clone()),
                _ => None,
            })
        {
            outbox.push(text);
        }
        self.persist(&mut state, &lanes).await?;

        loop {
            if state.status == HarnessStatus::Running {
                lane_prompt_shown = false;
            }
            if compact_wanted
                && compaction == CliCompaction::Idle
                && state.status == HarnessStatus::Idle
                && state.pending_question.is_none()
                && pending_approval.is_none()
                && outbox.is_empty()
            {
                compact_wanted = false;
                if session.started && session.id.is_some() {
                    state.compacting = true;
                    state.status = HarnessStatus::Running;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "history_compaction_pass".to_string(),
                        reasoning: "Saving working state to memory, then compacting the conversation.".to_string(),
                    });
                    let save = match kind {
                        CliKind::ClaudeCode => self.compaction_save_prompt(),
                        CliKind::Antigravity => Some(self.agy_compaction_prompt()),
                    };
                    state.reflected_events = Some(state.events.len());
                    save_at = None;
                    match save {
                        Some(prompt) => {
                            compaction = CliCompaction::Saving;
                            outbox.push(prompt);
                        }
                        None => {
                            compaction = CliCompaction::Compacting;
                            outbox.push(format!("{COMMAND_MARK}{}", self.compact_command()));
                        }
                    }
                    self.persist(&mut state, &lanes).await?;
                }
            }
            if !lane_prompt_shown
                && pending_approval.is_none()
                && state.pending_question.is_none()
                && state.status == HarnessStatus::Idle
                && outbox.is_empty()
                && let Some(request) = lane_approvals.front()
            {
                state.events.push(HarnessEvent::ApprovalRequest {
                    tool_name: request.tool_name.clone(),
                    summary: format!("lane «{}» · {}", request.lane, request.summary),
                    index: 1,
                    total: lane_approvals.len(),
                });
                state.status = HarnessStatus::WaitingForInput;
                lane_prompt_shown = true;
                self.persist(&mut state, &lanes).await?;
            }
            if !outbox.is_empty() {
                if session.started && session.synced > 0 {
                    let len = state.messages.len();
                    let prior_end = len.saturating_sub(outbox.len());
                    let rewritten = session.synced > len
                        || (!session.mark.is_empty() && transcript_mark(&state.messages, session.synced) != session.mark);
                    let away = !rewritten && prior_end > session.synced;
                    let behind = away
                        && handoff(&state.messages[session.synced..prior_end], false)
                            .is_some_and(|h| h.dropped > 0);
                    let threshold = self
                        .config
                        .context_window_tokens
                        .saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64)
                        / 100;
                    let full = (away || once) && threshold > 0 && session.tokens >= threshold;
                    if rewritten || behind || full {
                        if let Some(mut p) = process.take() {
                            let _ = p.child.start_kill();
                        }
                        generation += 1;
                        session.restart();
                        state.last_prompt_tokens = 0;
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "runtime_reseeded".to_string(),
                            reasoning: if rewritten {
                                format!("This session's history was rewound or compacted elsewhere, so {} starts a fresh conversation from it.", kind.label())
                            } else if behind {
                                format!("{} missed more than a catch-up can carry, so it starts a fresh conversation from this session's history.", kind.label())
                            } else {
                                format!("{}'s own conversation was near its context limit, so it starts a fresh one from this session's history.", kind.label())
                            },
                        });
                        self.persist(&mut state, &lanes).await?;
                    }
                }
                if kind == CliKind::ClaudeCode
                    && let Some(p) = process.as_mut()
                    && p.account != crate::accounts::active("claude-code")
                {
                    let _ = p.child.start_kill();
                    process = None;
                }
                if process.is_none() {
                    generation += 1;
                    match self
                        .spawn_cli(&profile, &state, &mcp_url, &line_tx, generation, &session)
                        .await
                    {
                        Ok(p) => {
                            if kind == CliKind::ClaudeCode {
                                session.started = true;
                            }
                            process = Some(p);
                        }
                        Err(error) => {
                            outbox.clear();
                            state.events.push(HarnessEvent::ModelError { message: error });
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            if once {
                                break;
                            }
                        }
                    }
                }
                if let Some(p) = process.as_mut() {
                    if session_key == crate::mission_control::SESSION_ID {
                        let mode = format!("{} {}", crate::mission_autonomy::clock_line(), crate::mission_autonomy::mode_line());
                        if mode != last_mode_line
                            && let Some(first) = outbox.first_mut()
                        {
                            *first = format!("<system-reminder>{mode}</system-reminder>\n\n{first}");
                            last_mode_line = mode;
                        }
                    }
                    let prior_end = state.messages.len().saturating_sub(outbox.len());
                    let fresh = session.synced == 0;
                    if prior_end > session.synced.max(1)
                        && let Some(context) = handoff(
                            &state.messages[session.synced.max(1)..prior_end],
                            fresh,
                        )
                        && let Some(first) = outbox.first_mut()
                    {
                        *first = format!("{}{first}", context.text);
                    }
                    let note = crate::coordination_tools::task_awareness(&self.context);
                    let mut reminder: Vec<String> = after_compaction.take().into_iter().collect();
                    if note != last_task_note {
                        reminder.push(if note.is_empty() {
                            "You have no open task now.".to_string()
                        } else {
                            note.clone()
                        });
                        last_task_note = note;
                    }
                    if !reminder.is_empty()
                        && let Some(first) = outbox.iter_mut().find(|t| !t.starts_with(COMMAND_MARK))
                    {
                        *first = format!("<system-reminder>{}</system-reminder>\n\n{first}", reminder.join("\n\n"));
                    }
                    session.mark_synced(&state.messages, state.compactions, state.last_prompt_tokens);
                    for text in std::mem::take(&mut outbox) {
                        let text = match text.strip_prefix(COMMAND_MARK) {
                            Some(command) => command.to_string(),
                            None if kind == CliKind::ClaudeCode && text.trim_start().starts_with('/') => {
                                format!(" {}", text.trim_start())
                            }
                            None => text,
                        };
                        let line = user_line_with_attachments(kind, &text);
                        let sent = p.stdin.write_all(format!("{line}\n").as_bytes()).await;
                        if sent.is_err() || p.stdin.flush().await.is_err() {
                            state.events.push(HarnessEvent::ModelError {
                                message: format!("{} stopped accepting input", kind.label()),
                            });
                            state.status = HarnessStatus::Failed;
                            process = None;
                            break;
                        }
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep_until(save_at.unwrap_or_else(tokio::time::Instant::now)), if save_at.is_some() && state.status == HarnessStatus::Idle && outbox.is_empty() => {
                    save_at = None;
                    if super::memory_reflection::reflection_scope(&state).is_some()
                        && session.started
                        && session.id.is_some()
                        && let Some(prompt) = self.idle_memory_prompt()
                    {
                        memory_saving = true;
                        state.status = HarnessStatus::Running;
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "idle_memory_save".to_string(),
                            reasoning: "Saving what this conversation learned to memory while you're away.".to_string(),
                        });
                        outbox.push(prompt);
                        self.persist(&mut state, &lanes).await?;
                    }
                }
                input = input_rx.recv() => {
                    let Some(input) = input else { break };
                    save_at = None;
                    match input {
                        LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            if state.pending_question.is_some() && is_held_envelope(&text) {
                                held_envelopes.push(text);
                                continue;
                            }
                            if state.pending_question.is_some() {
                                let mid_turn = state.status == HarnessStatus::Running && process.is_some();
                                state.status = HarnessStatus::WaitingForInput;
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                let answer = match state.messages.last() {
                                    Some(HarnessMessage::User { content }) => content.clone(),
                                    _ => text,
                                };
                                if mid_turn && kind == CliKind::Antigravity {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(answer);
                                    }
                                } else {
                                    outbox.push(answer);
                                }
                            } else if state.status == HarnessStatus::Running {
                                state.events.push(HarnessEvent::Steer { text: text.clone() });
                                state.messages.push(HarnessMessage::User { content: text.clone() });
                                self.bump_activity();
                                if kind == CliKind::Antigravity && process.is_some() {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(text);
                                    }
                                } else {
                                    outbox.push(text);
                                }
                            } else {
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                outbox.push(text);
                            }
                            if state.pending_question.is_none() {
                                for held in held_envelopes.drain(..) {
                                    state.events.push(HarnessEvent::Steer { text: held.clone() });
                                    state.messages.push(HarnessMessage::User { content: held.clone() });
                                    outbox.push(held);
                                }
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Approve | LoopInput::ApproveAll | LoopInput::Deny => {
                            if pending_approval.is_none() && lane_prompt_shown {
                                if let Some(request) = lane_approvals.pop_front() {
                                    let _ = request.reply.send(!matches!(input, LoopInput::Deny));
                                }
                                lane_prompt_shown = false;
                                if state.status == HarnessStatus::WaitingForInput && state.pending_question.is_none() {
                                    state.status = HarnessStatus::Idle;
                                }
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            }
                            let Some(call) = pending_approval.take() else { continue };
                            state.status = HarnessStatus::Running;
                            if matches!(input, LoopInput::ApproveAll) && !pending_vault {
                                state.approval_mode = ApprovalMode::Auto;
                            }
                            pending_vault = false;
                            let custom = pending_custom.take();
                            if !matches!(input, LoopInput::Deny)
                                && let (Some(tool), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                            {
                                crate::agent_tools::approve(dir, tool);
                            }
                            if matches!(input, LoopInput::Deny) {
                                let _ = call.reply.send(error_result(
                                    "user_denied",
                                    "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach or ask what they'd prefer.",
                                ));
                            } else {
                                running.push(Box::pin(self.execute_cli_call(call)));
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Interrupt => {
                            if let Some(mut p) = process.take() {
                                let _ = p.child.start_kill();
                            }
                            generation += 1;
                            running.clear();
                            pending_approval = None;
                            state.pending_question = None;
                            if let Some(s) = sink.as_ref() {
                                StreamBuffer::clear(s);
                            }
                            state.status = HarnessStatus::Interrupted;
                            self.persist(&mut state, &lanes).await?;
                            state.status = HarnessStatus::Idle;
                        }
                        LoopInput::SteerQueued(id) => {
                            let Some(text) = take_queued(&mut state, &id) else {
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            };
                            if state.status == HarnessStatus::Running && process.is_some() {
                                state.events.push(HarnessEvent::Steer { text: text.clone() });
                                state.messages.push(HarnessMessage::User { content: text.clone() });
                                self.bump_activity();
                                if kind == CliKind::Antigravity {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(text);
                                    }
                                } else {
                                    outbox.push(text);
                                }
                            } else {
                                if state.pending_question.is_some() {
                                    state.status = HarnessStatus::WaitingForInput;
                                }
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                let message = match state.messages.last() {
                                    Some(HarnessMessage::User { content }) => content.clone(),
                                    _ => text,
                                };
                                outbox.push(message);
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Compact => {
                            compact_wanted = true;
                        }
                        other => {
                            if self.apply_input(&mut state, other) {
                                break;
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                    }
                }
                line = line_rx.recv() => {
                    let Some(line) = line else { continue };
                    match line {
                        CliLine::Event(g, event) if g == generation => {
                            if let Some(id) = event.get("conversation_id").and_then(Value::as_str)
                                && kind == CliKind::Antigravity
                            {
                                session.adopt(id);
                            }
                            if session.forking
                                && kind == CliKind::ClaudeCode
                                && event.get("type").and_then(Value::as_str) == Some("system")
                                && event.get("subtype").and_then(Value::as_str) == Some("init")
                                && let Some(id) = event.get("session_id").and_then(Value::as_str)
                            {
                                session.forking = false;
                                session.id = Some(id.to_string());
                                session.save();
                            }
                            if event.get("type").and_then(Value::as_str) == Some("system")
                                && event.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                            {
                                let meta = event.get("compact_metadata");
                                let tokens = |key: &str| meta.and_then(|m| m.get(key)).and_then(Value::as_u64).unwrap_or(0);
                                let trigger = meta.and_then(|m| m.get("trigger")).and_then(Value::as_str).unwrap_or("auto");
                                state.compactions += 1;
                                state.reflected_events = Some(state.events.len());
                                state.last_prompt_tokens = tokens("post_tokens");
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "history_compacted".to_string(),
                                    reasoning: format!(
                                        "Compacted the conversation ({trigger}) from {} to {} tokens.",
                                        tokens("pre_tokens"),
                                        tokens("post_tokens")
                                    ),
                                });
                                last_task_note.clear();
                                self.persist(&mut state, &lanes).await?;
                            }
                            let final_before = state.final_text.clone();
                            let (changed, finished) = match kind {
                                CliKind::ClaudeCode => (
                                    self.apply_cli_event(&mut state, &event, sink.as_ref(), &mut parse),
                                    event.get("type").and_then(Value::as_str) == Some("result"),
                                ),
                                CliKind::Antigravity => self.apply_agy_event(&mut state, &event, sink.as_ref(), &mut parse),
                            };
                            if kind == CliKind::ClaudeCode
                                && event.get("type").and_then(Value::as_str) == Some("rate_limit_event")
                            {
                                let info = event.get("rate_limit_info");
                                let account = process.as_ref().and_then(|p| p.account.clone());
                                if let (Some(account), Some(snapshot)) = (account.as_deref(), state.rate_limit.as_ref()) {
                                    crate::accounts::record_usage("claude-code", account, snapshot);
                                }
                                if info.and_then(|i| i.get("status")).and_then(Value::as_str) == Some("rejected") {
                                    account_limit = Some(info.and_then(|i| i.get("resetsAt")).and_then(Value::as_i64).unwrap_or(0));
                                }
                            }
                            if finished
                                && kind == CliKind::ClaudeCode
                                && event.get("is_error").and_then(Value::as_bool) == Some(true)
                                && event.get("api_error_status").and_then(Value::as_u64) == Some(429)
                                && account_limit.is_none()
                            {
                                account_limit = Some(0);
                            }
                            if finished
                                && kind == CliKind::ClaudeCode
                                && event.get("is_error").and_then(Value::as_bool) == Some(true)
                                && let Some(hint) = account_limit.take()
                                && let Some(from) = process.as_ref().and_then(|p| p.account.clone())
                            {
                                let until = crate::accounts::reset_for("claude-code", &from, Some(hint));
                                if let Some(next) = crate::accounts::rotate("claude-code", &from, until) {
                                    if matches!(state.events.last(), Some(HarnessEvent::ModelError { .. })) {
                                        state.events.pop();
                                    }
                                    state.final_text = final_before.clone();
                                    state.events.push(HarnessEvent::SystemDecision {
                                        step: "account_switched".to_string(),
                                        reasoning: format!(
                                            "Claude account {} reached its usage limit (resets {}), so this continues on {}.",
                                            crate::accounts::label("claude-code", &from),
                                            crate::routing::reset_label(until),
                                            crate::accounts::label("claude-code", &next),
                                        ),
                                    });
                                    if let Some(mut p) = process.take() {
                                        let _ = p.child.start_kill();
                                    }
                                    generation += 1;
                                    state.status = HarnessStatus::Running;
                                    outbox.push("The last turn stopped at a usage limit before it finished. Pick up where it stopped and finish it.".to_string());
                                    self.persist(&mut state, &lanes).await?;
                                    continue;
                                }
                            }
                            if finished {
                                account_limit = None;
                                if let Some(usage) = event.get("modelUsage").and_then(Value::as_object) {
                                    cli_window = usage
                                        .values()
                                        .filter_map(|m| m.get("contextWindow").and_then(Value::as_u64))
                                        .max()
                                        .unwrap_or(cli_window);
                                }
                                match compaction {
                                    CliCompaction::Saving if kind == CliKind::Antigravity => {
                                        state.final_text = final_before.clone();
                                        let summary = state.messages.iter().rev().find_map(|m| match m {
                                            HarnessMessage::Assistant { content, .. } if !content.trim().is_empty() => Some(content.trim().to_string()),
                                            _ => None,
                                        });
                                        if let Some(mut p) = process.take() {
                                            let _ = p.child.start_kill();
                                        }
                                        generation += 1;
                                        session.id = None;
                                        session.started = false;
                                        session.mark_synced(&state.messages, state.compactions + 1, 0);
                                        after_compaction = Some(match summary {
                                            Some(summary) => format!("This conversation continues from an earlier one that was compacted. Summary of it:\n{summary}"),
                                            None => "This conversation continues from an earlier one that was compacted.".to_string(),
                                        });
                                        last_task_note.clear();
                                        state.compactions += 1;
                                        state.reflected_events = Some(state.events.len());
                                        state.last_prompt_tokens = 0;
                                        state.events.push(HarnessEvent::SystemDecision {
                                            step: "history_compacted".to_string(),
                                            reasoning: "Compacted the conversation: working state saved to memory, and a fresh conversation starts from its summary.".to_string(),
                                        });
                                        state.compacting = false;
                                        compaction = CliCompaction::Idle;
                                        state.status = HarnessStatus::Idle;
                                    }
                                    CliCompaction::Saving => {
                                        state.final_text = final_before.clone();
                                        state.status = HarnessStatus::Running;
                                        compaction = CliCompaction::Compacting;
                                        outbox.push(format!("{COMMAND_MARK}{}", self.compact_command()));
                                    }
                                    CliCompaction::Compacting => {
                                        state.final_text = final_before.clone();
                                        state.compacting = false;
                                        compaction = CliCompaction::Idle;
                                        if state.status == HarnessStatus::Running {
                                            state.status = HarnessStatus::Idle;
                                        }
                                    }
                                    CliCompaction::Idle => {
                                        if self.config.memory_enabled && self.context.lane_origin().is_none() {
                                            self.tidy_memory();
                                        }
                                        if memory_saving {
                                            memory_saving = false;
                                            state.reflected_events = Some(state.events.len());
                                        } else if !once
                                            && self.config.memory_enabled
                                            && self.config.memory_reflect
                                            && self.context.lane_origin().is_none()
                                            && self.context.owner() == "main"
                                            && self.context.agent_id().is_none()
                                        {
                                            save_at = Some(tokio::time::Instant::now() + kind.memory_save_after());
                                        }
                                        let window = match cli_window {
                                            0 => self.config.context_window_tokens,
                                            reported => reported.min(self.config.context_window_tokens.max(1)),
                                        };
                                        let threshold = window.saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64) / 100;
                                        if !once && threshold > 0 && state.last_prompt_tokens >= threshold {
                                            compact_wanted = true;
                                        }
                                    }
                                }
                                session.mark_synced(&state.messages, state.compactions, state.last_prompt_tokens);
                                if state.pending_question.is_some() {
                                    state.status = HarnessStatus::WaitingForInput;
                                }
                                if kind == CliKind::Antigravity {
                                    crate::antigravity::refresh_quota_if_stale(300);
                                }
                                let leftover: Vec<String> = steers.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                                if !leftover.is_empty() {
                                    outbox.push(leftover.join("\n\n"));
                                    state.status = HarnessStatus::Running;
                                }
                            }
                            if !parse.calls.is_empty() {
                                let model = if profile.model.trim().is_empty() { "default" } else { profile.model.trim() };
                                let owner = self.context.durable_session_id().map(str::to_string).unwrap_or_else(|| session_key.clone());
                                for usage in std::mem::take(&mut parse.calls) {
                                    crate::usage_ledger::record(&owner, &profile.provider, model, &usage);
                                }
                            }
                            if changed {
                                self.persist(&mut state, &lanes).await?;
                            }
                            if once && finished && running.is_empty() {
                                break;
                            }
                        }
                        CliLine::Exited(g, stderr) if g == generation => {
                            process = None;
                            if state.status == HarnessStatus::Running {
                                let message = if stderr.is_empty() {
                                    format!("{} exited before finishing the turn", kind.label())
                                } else {
                                    format!("{} exited: {stderr}", kind.label())
                                };
                                state.events.push(HarnessEvent::ModelError { message });
                                state.status = HarnessStatus::Failed;
                                self.persist(&mut state, &lanes).await?;
                            }
                            if once {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Some(request) = lane_approval_rx.recv() => lane_approvals.push_back(request),
                Some(result) = lane_rx.recv() => {
                    self.inject_lane_result(&mut state, &mut lanes, &result);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                Some(progress) = progress_rx.recv() => {
                    lanes.record_progress(&progress);
                    if progress.kind == "message" {
                        self.inject_lane_message(&mut state, &lanes, &progress);
                        self.wake_with_last_message(&mut state, &mut outbox);
                    }
                    self.persist(&mut state, &lanes).await?;
                }
                Some(event) = watch_rx.recv() => {
                    self.inject_watch_event(&mut state, &mut watches, &event);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                call = mcp_rx.recv() => {
                    let Some(mut call) = call else { continue };
                    if call.name == "custom_tool" {
                        call.name = call.arguments.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        call.arguments = call.arguments.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    }
                    let custom = self
                        .tools
                        .custom_dir()
                        .filter(|_| self.tools.is_custom(&call.name))
                        .and_then(|dir| crate::agent_tools::prepare(dir, &call.name, &call.arguments));
                    let custom = match custom {
                        Some(Err(message)) => {
                            let _ = call.reply.send(error_result("invalid_arguments", &format!("{}: {message}", call.name)));
                            continue;
                        }
                        Some(Ok(custom)) => Some(custom),
                        None => None,
                    };
                    if let Some(custom) = custom {
                        let gated = custom.needs_first_approval || !custom.vault.is_empty();
                        if gated && once {
                            match self.config.lane_approval.clone() {
                                Some(route) => {
                                    let summary = crate::agent_tools::approval_summary(&custom);
                                    let first = custom.needs_first_approval.then(|| custom.tool.clone());
                                    running.push(Box::pin(self.lane_gated_call(call, route, summary, first)));
                                }
                                None => {
                                    let _ = call.reply.send(error_result(
                                        "custom_tool_needs_approval",
                                        &format!("The custom tool `{}` needs the user's approval to run (it's new, or uses vault secrets), which isn't available in this run. Report that it needs approving, so it runs on the main thread.", call.name),
                                    ));
                                }
                            }
                        } else if gated || state.approval_mode == ApprovalMode::Manual {
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: call.name.clone(),
                                summary: crate::agent_tools::approval_summary(&custom),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_vault = gated;
                            pending_custom = custom.needs_first_approval.then(|| custom.tool.clone());
                            pending_approval = Some(call);
                            self.persist(&mut state, &lanes).await?;
                        } else {
                            running.push(Box::pin(self.execute_cli_call(call)));
                        }
                        continue;
                    }
                    match call.name.as_str() {
                        "ask_user" => match parse_ask_user(&call.arguments) {
                            Ok(questions) => {
                                state.pending_question = Some(questions.clone());
                                state.events.push(HarnessEvent::UserQuestion { questions });
                                let _ = call.reply.send(json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": {
                                        "asked": true,
                                        "note": "The question is on the user's screen. End your turn now: make no more tool calls and don't guess the answer. Their reply arrives as your next message, starting with [answer].",
                                    }
                                }));
                                self.persist(&mut state, &lanes).await?;
                            }
                            Err(message) => {
                                let _ = call.reply.send(error_result("invalid_arguments", &message));
                            }
                        },
                        name if meta_names.iter().any(|m| m == name) => {
                            let (value, _) = self.dispatch_meta(&mut state, &mut lanes, &mut watches, name, &call.arguments);
                            let _ = call.reply.send(value);
                            self.persist(&mut state, &lanes).await?;
                        }
                        "bash" if !vault_names(&call.arguments).is_empty() => {
                            let names = vault_names(&call.arguments).join(", ");
                            if once {
                                match self.config.lane_approval.clone() {
                                    Some(route) => {
                                        let summary = format!(
                                            "⚠ uses vault secret(s) [{names}] — {}",
                                            super::prompts::approval_summary("bash", &call.arguments)
                                        );
                                        running.push(Box::pin(self.lane_gated_call(call, route, summary, None)));
                                    }
                                    None => {
                                        let _ = call.reply.send(error_result(
                                            "vault_needs_confirmation",
                                            &format!("Using vault secret(s) [{names}] requires user confirmation, which isn't available in a delegated/headless run. Report that this step needs the secret, so it's done on the main thread where the user can approve it."),
                                        ));
                                    }
                                }
                                continue;
                            }
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: "bash".to_string(),
                                summary: format!(
                                    "⚠ uses vault secret(s) [{names}] — {}",
                                    super::prompts::approval_summary("bash", &call.arguments)
                                ),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_approval = Some(call);
                            pending_vault = true;
                            self.persist(&mut state, &lanes).await?;
                        }
                        name if state.approval_mode == ApprovalMode::Manual && MUTATING_TOOLS.contains(&name) => {
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: name.to_string(),
                                summary: super::prompts::approval_summary(name, &call.arguments),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_approval = Some(call);
                            self.persist(&mut state, &lanes).await?;
                        }
                        _ => running.push(Box::pin(self.execute_cli_call(call))),
                    }
                }
                Some(()) = running.next(), if !running.is_empty() => {}
            }
        }
        if let Some(mut p) = process.take() {
            let _ = p.child.start_kill();
        }
        Ok(state)
    }

    fn wake_with_last_message(&self, state: &mut HarnessState, outbox: &mut Vec<String>) {
        if let Some(HarnessMessage::User { content }) = state.messages.last() {
            outbox.push(content.clone());
        }
        if state.status == HarnessStatus::Idle {
            state.status = HarnessStatus::Running;
        }
    }

    async fn lane_gated_call(
        &self,
        call: McpCall,
        route: crate::lanes::LaneApprovalRoute,
        summary: String,
        first_run: Option<crate::agent_tools::CustomTool>,
    ) {
        if !route.ask(&call.name, &summary).await {
            let _ = call.reply.send(error_result(
                "user_denied",
                "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach, or report what you needed.",
            ));
            return;
        }
        if let (Some(tool), Some(dir)) = (first_run.as_ref(), self.tools.custom_dir()) {
            crate::agent_tools::approve(dir, tool);
        }
        self.execute_cli_call(call).await;
    }

    async fn execute_cli_call(&self, call: McpCall) {
        let value = match self.tools.execute(&self.context, &call.name, call.arguments).await {
            Ok(mut result) if call.name == "view_image" => {
                let resolved = result
                    .value
                    .pointer("/data/path")
                    .and_then(Value::as_str)
                    .and_then(|p| self.context.resolve_workspace_path(p).ok());
                if let (Some(path), Some(object)) = (resolved, result.value.as_object_mut()) {
                    object.insert(IMAGE_PATH_KEY.to_string(), json!(path.display().to_string()));
                }
                result.value
            }
            Ok(result) => result.value,
            Err(ToolError::UnknownTool(name)) => {
                let available: Vec<String> = self.tools.definitions().into_iter().map(|d| d.name).collect();
                let hint = if self.context.workspace_root() == crate::mission_control::workspace_path() {
                    " You are Mission Control: to check on, run or change anything in a project, ask the session that owns it (create_mission_task to that session, or inspect_session)."
                } else {
                    ""
                };
                error_result(
                    "unknown_tool",
                    &format!("`{name}` isn't a tool in this session.{hint} Tools you have: {}.", available.join(", ")),
                )
            }
            Err(error) => {
                let text = error.to_string();
                let argument_error = matches!(error, ToolError::Json(_) | ToolError::InvalidArguments { .. })
                    || ["missing field", "unknown field", "invalid type", "unknown variant", "expected JSON object"]
                        .iter()
                        .any(|marker| text.contains(marker));
                let schema = self
                    .tools
                    .definitions()
                    .into_iter()
                    .find(|d| argument_error && d.name == call.name)
                    .map(|d| d.input_schema);
                let message = match schema {
                    Some(schema) => format!(
                        "{error}\nCall `{}` again with arguments matching this schema: {schema}",
                        call.name
                    ),
                    None => error.to_string(),
                };
                error_result("tool_error", &message)
            }
        };
        let _ = call.reply.send(value);
    }

    fn apply_agy_event(
        &self,
        state: &mut HarnessState,
        event: &Value,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> (bool, bool) {
        match event.get("event").and_then(Value::as_str) {
            Some("step_update") => {
                let Some(step) = event.get("step_update") else { return (false, false) };
                let index = step.get("step_index").and_then(Value::as_i64).unwrap_or(-1);
                let done = step.get("state").and_then(Value::as_str);
                match step.get("step_type").and_then(Value::as_str) {
                    Some("agent_response") => {
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                            parse.agy_text.entry(index).or_default().push_str(delta);
                            if let Some(sink) = sink {
                                StreamBuffer::append(sink, delta);
                            }
                        }
                        if let (Some(delta), Some(sink)) = (step.get("thinking_delta").and_then(Value::as_str), sink) {
                            StreamBuffer::append_thinking(sink, delta);
                        }
                        if done != Some("DONE") {
                            return (false, false);
                        }
                        if let Some(usage) = step.get("usage") {
                            let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                            let prompt = n("input_tokens") + n("cache_read_tokens");
                            let completion = n("output_tokens");
                            state.last_prompt_tokens = prompt;
                            state.prompt_tokens += prompt;
                            state.completion_tokens += completion;
                            state.total_tokens += prompt + completion;
                            state.cache_read_tokens += n("cache_read_tokens");
                            parse.calls.push(crate::llm::TokenUsage {
                                prompt_tokens: prompt,
                                completion_tokens: completion,
                                total_tokens: prompt + completion,
                                cache_read_tokens: n("cache_read_tokens"),
                                ..Default::default()
                            });
                        }
                        let text = parse.agy_text.remove(&index).unwrap_or_default().trim().to_string();
                        if text.is_empty() {
                            return (false, false);
                        }
                        if let Some(sink) = sink {
                            StreamBuffer::clear(sink);
                        }
                        state.messages.push(HarnessMessage::Assistant { content: text.clone(), tool_calls: Vec::new() });
                        state.events.push(HarnessEvent::AssistantText { text });
                        (true, false)
                    }
                    Some("tool") => {
                        let info = step.get("tool_info").cloned().unwrap_or(Value::Null);
                        let raw = info.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let params = info.get("parameters").cloned().unwrap_or(Value::Null);
                        let mcp = raw == "call_mcp_tool";
                        let (name, arguments) = if mcp {
                            unwrap_custom(
                                params.get("ToolName").and_then(Value::as_str).unwrap_or("tool").to_string(),
                                params.get("Arguments").cloned().unwrap_or(Value::Null),
                            )
                        } else if let Some(path) = (raw == "view_file")
                            .then(|| params.get("AbsolutePath").and_then(Value::as_str))
                            .flatten()
                            .filter(|p| !p.contains("/.gemini/antigravity-cli/") && crate::antigravity::is_image(p))
                        {
                            ("view_image".to_string(), json!({ "path": path }))
                        } else {
                            (raw.to_string(), params)
                        };
                        let schema_read = raw == "view_file"
                            && arguments
                                .get("AbsolutePath")
                                .and_then(Value::as_str)
                                .is_some_and(|p| p.contains("/.gemini/antigravity-cli/"));
                        let redirected = done == Some("ERROR")
                            && info
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|m| m.contains("denied by pre-tool hook"));
                        let envelope_rejected = done == Some("ERROR")
                            && info
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|m| m.contains("missing properties") && (m.contains("toolSummary") || m.contains("toolAction")));
                        let quiet = schema_read || redirected || envelope_rejected || QUIET_TOOLS.contains(&name.as_str());
                        let mut changed = false;
                        if envelope_rejected {
                            if let Some(marker) = parse.envelope_marker.as_ref() {
                                if let Some(dir) = marker.parent() {
                                    let _ = std::fs::create_dir_all(dir);
                                }
                                let _ = std::fs::write(marker, &name);
                            }
                            if parse.agy_tools.remove(&index)
                                && let Some(at) = state.events.iter().rposition(|e| {
                                    matches!(e, HarnessEvent::ToolCall { tool_name, .. } if *tool_name == name)
                                })
                            {
                                state.events.remove(at);
                                state.events_rewritten = true;
                                changed = true;
                            }
                        }
                        let announce = mcp || done.is_some_and(|d| d == "DONE" || d == "ERROR");
                        if announce && !quiet && parse.agy_tools.insert(index) {
                            state.events.push(HarnessEvent::ToolCall { tool_name: name.clone(), arguments });
                            changed = true;
                        }
                        let own = if mcp && matches!(done, Some("DONE") | Some("ERROR")) {
                            parse.served.lock().ok().and_then(|mut served| served.get_mut(&name).and_then(|q| q.pop_front()))
                        } else {
                            None
                        };
                        let result = match done {
                            Some(_) if own.is_some() => own.unwrap_or_default(),
                            Some("DONE") => {
                                let output = info.get("output").and_then(Value::as_str).unwrap_or("").to_string();
                                serde_json::Deserializer::from_str(output.trim_start())
                                    .into_iter::<Value>()
                                    .next()
                                    .and_then(Result::ok)
                                    .filter(|v| mcp && v.get("status").is_some())
                                    .unwrap_or_else(|| json!({"schema_version": 1, "status": "success", "data": {"output": output}}))
                            }
                            Some("ERROR") => {
                                let message = info.pointer("/error/message").and_then(Value::as_str).unwrap_or("tool failed");
                                serde_json::from_str::<Value>(message)
                                    .ok()
                                    .filter(|v| mcp && v.get("status").is_some())
                                    .unwrap_or_else(|| error_result("tool_error", message))
                            }
                            _ => return (changed, false),
                        };
                        if !quiet {
                            state.events.push(HarnessEvent::ToolResult { tool_name: name, result });
                            changed = true;
                        }
                        (changed, false)
                    }
                    _ => (false, false),
                }
            }
            Some("result") => {
                let result = event.get("result").cloned().unwrap_or(Value::Null);
                if let Some(sink) = sink {
                    StreamBuffer::clear(sink);
                }
                parse.agy_text.clear();
                parse.agy_tools.clear();
                state.iterations += result.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let error = result.get("error").and_then(Value::as_str).unwrap_or("");
                let cancelled = error.contains("context canceled");
                let response = result.get("response").and_then(Value::as_str).map(str::trim).unwrap_or("");
                let replied = !response.is_empty()
                    || state
                        .events
                        .iter()
                        .rev()
                        .take_while(|e| !matches!(e, HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. }))
                        .any(|e| matches!(e, HarnessEvent::AssistantText { .. }));
                if result.get("status").and_then(Value::as_str) == Some("ERROR") && !cancelled && !replied {
                    let message = if error.is_empty() { "Antigravity reported an error" } else { error }.to_string();
                    state.events.push(HarnessEvent::ModelError { message });
                } else if let Some(text) = result.get("response").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
                    state.final_text = Some(text.to_string());
                }
                if state.status == HarnessStatus::Running {
                    state.status = HarnessStatus::Idle;
                }
                (true, true)
            }
            _ => (false, false),
        }
    }

    fn apply_cli_event(
        &self,
        state: &mut HarnessState,
        event: &Value,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> bool {
        let tool_names = &mut parse.tool_names;
        match event.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let n = |v: Option<&Value>, k: &str| v.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
                match event.pointer("/event/type").and_then(Value::as_str) {
                    Some("message_start") => {
                        let usage = event.pointer("/event/message/usage");
                        let prompt = n(usage, "input_tokens") + n(usage, "cache_read_input_tokens") + n(usage, "cache_creation_input_tokens");
                        parse.claude_call = Some(crate::llm::TokenUsage {
                            prompt_tokens: prompt,
                            completion_tokens: n(usage, "output_tokens"),
                            cache_read_tokens: n(usage, "cache_read_input_tokens"),
                            cache_creation_tokens: n(usage, "cache_creation_input_tokens"),
                            ..Default::default()
                        });
                    }
                    Some("message_delta") => {
                        let out = n(event.pointer("/event/usage"), "output_tokens");
                        if let Some(call) = parse.claude_call.as_mut()
                            && out > 0
                        {
                            call.completion_tokens = out;
                        }
                    }
                    Some("message_stop") => {
                        if let Some(mut call) = parse.claude_call.take() {
                            call.total_tokens = call.prompt_tokens + call.completion_tokens;
                            state.last_prompt_tokens = call.prompt_tokens;
                            parse.calls.push(call);
                        }
                    }
                    _ => {}
                }
                if let (Some(sink), Some(delta)) = (sink, event.pointer("/event/delta")) {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                StreamBuffer::append(sink, text);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                StreamBuffer::append_thinking(sink, text);
                            }
                        }
                        _ => {}
                    }
                }
                false
            }
            Some("assistant") => {
                if state.status == HarnessStatus::Idle {
                    state.status = HarnessStatus::Running;
                }
                let blocks = event.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut changed = false;
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = block.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
                            if !text.is_empty() {
                                if let Some(sink) = sink {
                                    StreamBuffer::clear(sink);
                                }
                                state.messages.push(HarnessMessage::Assistant { content: text.clone(), tool_calls: Vec::new() });
                                state.events.push(HarnessEvent::AssistantText { text });
                                changed = true;
                            }
                        }
                        Some("tool_use") => {
                            let raw = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let (name, arguments) = unwrap_custom(
                                raw.strip_prefix(MCP_PREFIX).unwrap_or(raw).to_string(),
                                block.get("input").cloned().unwrap_or(Value::Null),
                            );
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                tool_names.insert(id.to_string(), name.clone());
                            }
                            if !QUIET_TOOLS.contains(&name.as_str()) {
                                state.events.push(HarnessEvent::ToolCall { tool_name: name, arguments });
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                }
                changed
            }
            Some("user") => {
                let blocks = event.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut changed = false;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    let name = tool_names.get(id).cloned().unwrap_or_else(|| "tool".to_string());
                    if QUIET_TOOLS.contains(&name.as_str()) {
                        continue;
                    }
                    state.events.push(HarnessEvent::ToolResult { tool_name: name, result: tool_result_from_block(&block) });
                    changed = true;
                }
                changed
            }
            Some("rate_limit_event") => {
                let windows = event.pointer("/rate_limit_info/unifiedWindows");
                let window = |key: &str, minutes: i64| {
                    windows.and_then(|w| w.get(key)).map(|w| crate::llm::RateLimitWindow {
                        used_percent: w.get("utilization").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                        window_minutes: minutes,
                        resets_at: w.get("resetsAt").and_then(Value::as_i64).unwrap_or(0),
                    })
                };
                let snapshot = crate::llm::RateLimitSnapshot {
                    primary: window("five_hour", 300),
                    secondary: window("seven_day", 10080),
                    label: None,
                };
                if let Ok(body) = serde_json::to_string(&snapshot) {
                    let path = claude_rate_limits_path();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(path, body);
                }
                state.rate_limit = Some(snapshot);
                true
            }
            Some("result") => {
                if let Some(sink) = sink {
                    StreamBuffer::clear(sink);
                }
                let usage = event.get("usage").cloned().unwrap_or(Value::Null);
                let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                let prompt = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
                let completion = n("output_tokens");
                state.prompt_tokens += prompt;
                state.completion_tokens += completion;
                state.total_tokens += prompt + completion;
                state.cache_read_tokens += n("cache_read_input_tokens");
                state.iterations += event.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let failed = event.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                if failed {
                    let message = event.get("result").and_then(Value::as_str)
                        .or_else(|| event.get("subtype").and_then(Value::as_str))
                        .unwrap_or("Claude Code reported an error").to_string();
                    if matches!(state.events.last(), Some(HarnessEvent::AssistantText { text }) if text.trim() == message.trim()) {
                        state.events.pop();
                        if matches!(state.messages.last(), Some(HarnessMessage::Assistant { content, tool_calls }) if tool_calls.is_empty() && content.trim() == message.trim()) {
                            state.messages.pop();
                        }
                    }
                    state.events.push(HarnessEvent::ModelError { message });
                } else if let Some(text) = event.get("result").and_then(Value::as_str) {
                    state.final_text = Some(text.to_string());
                }
                if state.status == HarnessStatus::Running {
                    state.status = HarnessStatus::Idle;
                }
                true
            }
            _ => false,
        }
    }
}
