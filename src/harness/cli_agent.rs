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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliKind {
    ClaudeCode,
    Antigravity,
}

impl CliKind {
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
        let session = match (kind, existing) {
            (_, Some(id)) => Self { kind, key, id: Some(id), started: true, synced, tool_guide: String::new() },
            (CliKind::ClaudeCode, None) => Self {
                kind,
                key,
                id: Some(uuid::Uuid::new_v4().to_string()),
                started: false,
                synced: 0,
                tool_guide: String::new(),
            },
            (CliKind::Antigravity, None) => Self { kind, key, id: None, started: false, synced: 0, tool_guide: String::new() },
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
                let _ = std::fs::write(file, format!("{id}\n{}", self.synced));
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

    fn mark_synced(&mut self, upto: usize) {
        if self.synced != upto {
            self.synced = upto;
            self.save();
        }
    }
}

fn missed_context(messages: &[HarnessMessage]) -> Option<String> {
    const BUDGET: usize = 30_000;
    const PER_MESSAGE: usize = 2_000;
    let clip = |text: &str| -> String {
        let text = text.trim();
        if text.chars().count() > PER_MESSAGE {
            format!("{}…", text.chars().take(PER_MESSAGE).collect::<String>())
        } else {
            text.to_string()
        }
    };
    let mut entries: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            HarnessMessage::User { content } if !content.trim().is_empty() => Some(format!("user: {}", clip(content))),
            HarnessMessage::Assistant { content, tool_calls } => {
                let tools: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
                let mut line = format!("assistant: {}", clip(content));
                if !tools.is_empty() {
                    line.push_str(&format!(" (used {})", tools.join(", ")));
                }
                (!content.trim().is_empty() || !tools.is_empty()).then_some(line)
            }
            HarnessMessage::Summary { content, .. } => Some(format!("summary of earlier work: {}", clip(content))),
            _ => None,
        })
        .collect();
    let mut used = 0;
    let mut keep = entries.len();
    while keep > 0 && used + entries[keep - 1].len() <= BUDGET {
        used += entries[keep - 1].len();
        keep -= 1;
    }
    let dropped = keep;
    entries.drain(..keep);
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from("<conversation_so_far>\nThis session's earlier conversation, from before this runtime joined it. Treat it as your own history.\n");
    if dropped > 0 {
        out.push_str(&format!("({dropped} older messages omitted)\n"));
    }
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n</conversation_so_far>\n\n");
    Some(out)
}

#[derive(Default)]
struct CliParse {
    tool_names: HashMap<String, String>,
    agy_text: HashMap<i64, String>,
    agy_tools: HashSet<i64>,
    turn_usage: Option<crate::llm::TokenUsage>,
    agy_steps: crate::llm::TokenUsage,
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

#[derive(Clone)]
struct McpState {
    token: String,
    tools: Vec<NativeToolDefinition>,
    calls: mpsc::UnboundedSender<McpCall>,
    steers: SteerQueue,
}

struct CliProcess {
    child: Child,
    stdin: ChildStdin,
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
            "## Tools in this runtime\n\nThe tools named in these instructions are served by the `{}` MCP server: call them with call_mcp_tool, ServerName `{}` and ToolName exactly as named here (`bash`, `change_files`, `update_plan`, `ask_user`, `delegate_task`, and the rest). These are your only working tools: every built-in tool of yours is switched off in this session. Read and search files with the snippet `bash` tool (cat, sed -n, rg, ls), research with its `web_search` and `web_read`, and make every change, command, question and delegation through the snippet tools. The one exception: when a tool result says its output was saved to a file inside your own Antigravity folder, read that file with view_file.",
            crate::antigravity::MCP_SERVER_NAME,
            crate::antigravity::MCP_SERVER_NAME,
        ),
    }
}

fn tool_guide(defs: &[NativeToolDefinition]) -> String {
    let mut out = String::from("## Snippet tools and their arguments\n\nCall each through call_mcp_tool with ServerName `snippet_snippet`. Pass every required argument by its exact name; optional ones in brackets.\n");
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
            if state.calls.send(McpCall { name, arguments, reply }).is_err() {
                return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "session ended"}}));
            }
            let value = rx
                .await
                .unwrap_or_else(|_| error_result("interrupted", "The call was interrupted."));
            let failed = value.get("status").and_then(Value::as_str) == Some("error");
            let mut content = vec![json!({"type": "text", "text": value.to_string()})];
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

async fn start_mcp_server(
    tools: Vec<NativeToolDefinition>,
    steers: SteerQueue,
) -> Result<(String, mpsc::UnboundedReceiver<McpCall>), String> {
    let (calls, rx) = mpsc::unbounded_channel();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = McpState { token: token.clone(), tools, calls, steers };
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
            .arg("--disable-slash-commands")
            .env("MCP_TOOL_TIMEOUT", "3600000")
            .env("MCP_TIMEOUT", "30000");
        if !profile.model.trim().is_empty() {
            cmd.args(["--model", profile.model.trim()]);
        }
        if let Some(effort) = Self::effort_arg(profile) {
            cmd.args(["--effort", effort]);
        }
        if let Some(id) = session.id.as_deref() {
            cmd.args([if session.started { "--resume" } else { "--session-id" }, id]);
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
        let mut cmd = match kind {
            CliKind::ClaudeCode => self.claude_command(profile, &prompt, mcp_url, session)?,
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
                while let Ok(Some(line)) = err_reader.next_line().await {
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > 4000 {
                        tail.drain(..tail.len() - 4000);
                    }
                }
            }
            let _ = tx.send(CliLine::Exited(generation, tail.trim().to_string()));
        });
        Ok(CliProcess { child, stdin })
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
        let mut lanes = self.new_lane_manager(factory, lane_tx, progress_tx, &state);
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
        bridge_tools.extend(meta_defs);
        let guide = tool_guide(&bridge_tools);
        let steers: SteerQueue = Default::default();
        let (mcp_url, mut mcp_rx) = start_mcp_server(bridge_tools, steers.clone()).await.map_err(ToolError::msg)?;
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<CliLine>();
        let mut process: Option<CliProcess> = None;
        let mut generation = 0u64;
        let kind = CliKind::of(&profile.provider);
        let session_key = self.session_id().unwrap_or_else(|| "main".to_string());
        let mut session = CliSession::open(kind, &session_key, self.config.resume);
        if kind == CliKind::Antigravity {
            session.tool_guide = guide;
        }
        let mut parse = CliParse::default();
        let mut pending_approval: Option<McpCall> = None;
        let mut pending_question: Option<McpCall> = None;
        let mut approval_index = 0usize;
        let mut running = FuturesUnordered::new();
        let mut outbox: Vec<String> = Vec::new();

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
            if !outbox.is_empty() {
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
                    let prior_end = state.messages.len().saturating_sub(outbox.len());
                    if prior_end > session.synced.max(1)
                        && let Some(context) = missed_context(&state.messages[session.synced.max(1)..prior_end])
                        && let Some(first) = outbox.first_mut()
                    {
                        *first = format!("{context}{first}");
                    }
                    session.mark_synced(state.messages.len());
                    for text in std::mem::take(&mut outbox) {
                        let line = kind.user_line(&text);
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
                input = input_rx.recv() => {
                    let Some(input) = input else { break };
                    match input {
                        LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            if let Some(call) = pending_question.take() {
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                let _ = call.reply.send(json!({"schema_version": 1, "status": "success", "data": {"answer": text}}));
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
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Approve | LoopInput::ApproveAll | LoopInput::Deny => {
                            let Some(call) = pending_approval.take() else { continue };
                            state.status = HarnessStatus::Running;
                            if matches!(input, LoopInput::ApproveAll) {
                                state.approval_mode = ApprovalMode::Auto;
                            }
                            if matches!(input, LoopInput::Deny) {
                                let _ = call.reply.send(error_result(
                                    "user_denied",
                                    "The user denied this action. Do not retry it as-is — adjust your approach or ask what they'd prefer.",
                                ));
                            } else {
                                running.push(self.execute_cli_call(call));
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Interrupt => {
                            if let Some(mut p) = process.take() {
                                let _ = p.child.start_kill();
                            }
                            running.clear();
                            pending_approval = None;
                            pending_question = None;
                            state.pending_question = None;
                            if let Some(s) = sink.as_ref() {
                                StreamBuffer::clear(s);
                            }
                            state.status = HarnessStatus::Interrupted;
                            self.persist(&mut state, &lanes).await?;
                            state.status = HarnessStatus::Idle;
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
                            let (changed, finished) = match kind {
                                CliKind::ClaudeCode => (
                                    self.apply_cli_event(&mut state, &event, sink.as_ref(), &mut parse),
                                    event.get("type").and_then(Value::as_str) == Some("result"),
                                ),
                                CliKind::Antigravity => self.apply_agy_event(&mut state, &event, sink.as_ref(), &mut parse),
                            };
                            if finished {
                                session.mark_synced(state.messages.len());
                                let leftover: Vec<String> = steers.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                                if !leftover.is_empty() {
                                    outbox.push(leftover.join("\n\n"));
                                    state.status = HarnessStatus::Running;
                                }
                            }
                            if let Some(usage) = parse.turn_usage.take() {
                                let model = if profile.model.trim().is_empty() { "default" } else { profile.model.trim() };
                                let owner = self.context.durable_session_id().map(str::to_string).unwrap_or_else(|| session_key.clone());
                                crate::usage_ledger::record(&owner, &profile.provider, model, &usage);
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
                Some(result) = lane_rx.recv() => {
                    self.inject_lane_result(&mut state, &mut lanes, &result);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                Some(progress) = progress_rx.recv() => {
                    lanes.record_progress(&progress);
                    self.persist(&mut state, &lanes).await?;
                }
                Some(event) = watch_rx.recv() => {
                    self.inject_watch_event(&mut state, &mut watches, &event);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                call = mcp_rx.recv() => {
                    let Some(call) = call else { continue };
                    match call.name.as_str() {
                        "ask_user" => match parse_ask_user(&call.arguments) {
                            Ok(questions) => {
                                state.pending_question = Some(questions.clone());
                                state.events.push(HarnessEvent::UserQuestion { questions });
                                state.status = HarnessStatus::WaitingForInput;
                                pending_question = Some(call);
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
                        _ => running.push(self.execute_cli_call(call)),
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

    async fn execute_cli_call(&self, call: McpCall) {
        let value = match self.tools.execute(&self.context, &call.name, call.arguments).await {
            Ok(result) => result.value,
            Err(error) => {
                let schema = self
                    .tools
                    .definitions()
                    .into_iter()
                    .find(|d| d.name == call.name)
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
                            state.last_prompt_tokens = n("input_tokens") + n("cache_read_tokens");
                            let turn = &mut parse.agy_steps;
                            turn.prompt_tokens += n("input_tokens");
                            turn.completion_tokens += n("output_tokens");
                            turn.cache_read_tokens += n("cache_read_tokens");
                            turn.total_tokens = turn.prompt_tokens + turn.completion_tokens;
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
                            (
                                params.get("ToolName").and_then(Value::as_str).unwrap_or("tool").to_string(),
                                params.get("Arguments").cloned().unwrap_or(Value::Null),
                            )
                        } else {
                            (raw.to_string(), params)
                        };
                        let schema_read = raw == "view_file"
                            && arguments.get("AbsolutePath").and_then(Value::as_str).is_some_and(|p| p.contains("/antigravity-cli/mcp/"));
                        let redirected = done == Some("ERROR")
                            && info
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|m| m.contains("denied by pre-tool hook"));
                        let quiet = schema_read || redirected || QUIET_TOOLS.contains(&name.as_str());
                        let mut changed = false;
                        let announce = mcp || done.is_some_and(|d| d == "DONE" || d == "ERROR");
                        if announce && !quiet && parse.agy_tools.insert(index) {
                            state.events.push(HarnessEvent::ToolCall { tool_name: name.clone(), arguments });
                            changed = true;
                        }
                        let result = match done {
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
                let turn = std::mem::take(&mut parse.agy_steps);
                state.prompt_tokens += turn.prompt_tokens;
                state.completion_tokens += turn.completion_tokens;
                state.total_tokens += turn.total_tokens;
                state.cache_read_tokens += turn.cache_read_tokens;
                parse.turn_usage = Some(turn);
                state.iterations += result.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                if result.get("status").and_then(Value::as_str) == Some("ERROR") {
                    let message = result
                        .get("error")
                        .and_then(Value::as_str)
                        .filter(|m| !m.is_empty())
                        .unwrap_or("Antigravity reported an error")
                        .to_string();
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
                            let name = raw.strip_prefix(MCP_PREFIX).unwrap_or(raw).to_string();
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                tool_names.insert(id.to_string(), name.clone());
                            }
                            if !QUIET_TOOLS.contains(&name.as_str()) {
                                state.events.push(HarnessEvent::ToolCall {
                                    tool_name: name,
                                    arguments: block.get("input").cloned().unwrap_or(Value::Null),
                                });
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
                state.rate_limit = Some(crate::llm::RateLimitSnapshot {
                    primary: window("five_hour", 300),
                    secondary: window("seven_day", 10080),
                });
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
                parse.turn_usage = Some(crate::llm::TokenUsage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: prompt + completion,
                    cache_read_tokens: n("cache_read_input_tokens"),
                    cache_creation_tokens: n("cache_creation_input_tokens"),
                });
                state.prompt_tokens += prompt;
                state.completion_tokens += completion;
                state.total_tokens += prompt + completion;
                state.cache_read_tokens += n("cache_read_input_tokens");
                state.last_prompt_tokens = prompt;
                state.iterations += event.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let failed = event.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                if failed {
                    let message = event.get("result").and_then(Value::as_str)
                        .or_else(|| event.get("subtype").and_then(Value::as_str))
                        .unwrap_or("Claude Code reported an error").to_string();
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
