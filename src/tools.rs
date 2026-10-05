use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;

use crate::llm::NativeToolDefinition;

// Re-export so `crate::tools::coding_tools` resolves (the tool registry is defined
// in `builtins`); lanes.rs imports it via this path.
pub use crate::builtins::coding_tools;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("{0}")]
    Message(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("tool `{tool}` expected JSON object arguments")]
    InvalidArguments { tool: String },
    #[error("path `{path}` escapes workspace root `{root}`")]
    PathEscapesWorkspace { path: String, root: String },
    /// A model request that failed, tagged with whether a retry could plausibly
    /// help. Fatal (`retryable: false`) covers auth/permission/not-found/bad-
    /// request — retrying those just floods the screen and never succeeds.
    #[error("{message}")]
    ModelRequest { message: String, retryable: bool },
}

impl ToolError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn model_request(message: impl Into<String>, retryable: bool) -> Self {
        Self::ModelRequest {
            message: message.into(),
            retryable,
        }
    }

    /// Whether the harness should attempt recovery after this error. Non-model
    /// errors default to retryable (treated as transient until proven otherwise).
    pub fn retryable(&self) -> bool {
        match self {
            Self::ModelRequest { retryable, .. } => *retryable,
            _ => true,
        }
    }
}

/// Stable content fingerprint for the staleness guard (DefaultHasher over bytes).
fn content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

pub type BrowserSummaryProvider = Arc<dyn Fn() -> String + Send + Sync>;


#[derive(Clone)]
pub struct ToolContext {
    workspace_root: PathBuf,
    owner: String,
    browser_summary: Option<BrowserSummaryProvider>,
    /// Whole-file content hashes this context last saw per path — recorded on read
    /// and after its own writes. A write is rejected when the file on disk no
    /// longer matches the recorded hash, catching any change since (including
    /// external edits, another lane, or a concurrent process). Optimistic
    /// concurrency; replaces the former lock registry.
    seen: Arc<Mutex<HashMap<PathBuf, u64>>>,
    mission_control: bool,
    /// Durable session identity (state path relative to the workspaces root).
    /// Set for daemon-managed sessions; lets `report_mission_task` verify the
    /// caller actually owns the task it is reporting on.
    durable_session_id: Option<String>,
    /// Same store the daemon dispatcher uses. Set for Mission Control so tools
    /// do not recompute the root from HOME independently.
    mission_control_root: Option<PathBuf>,
    /// Absolute path to the SQLite store (`~/.snippet/snippet.db`). Set for every
    /// daemon-managed session so coordination tools reach the same database the
    /// daemon writes.
    store_path: Option<PathBuf>,
    /// The directory agent id this session runs as (specialized sessions only),
    /// so board writes and direct messages are attributed to the agent.
    agent_id: Option<String>,
    /// The shell's working directory, carried across `bash` calls. Relative
    /// paths in the file tools resolve against it too, so there is one "here".
    current_dir: Arc<Mutex<PathBuf>>,
}

impl ToolContext {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        Self::with_owner(workspace_root, "main")
    }

    pub fn mission_control(workspace_root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        let root = workspace_root.into();
        let mut context = Self::with_owner(root.clone(), "mission_control")?;
        context.mission_control = true;
        context.mission_control_root = Some(root);
        Ok(context)
    }

    pub fn with_browser_summary(
        workspace_root: impl Into<PathBuf>,
        browser_summary: BrowserSummaryProvider,
    ) -> Result<Self, ToolError> {
        Self::with_owner_and_browser_summary(workspace_root, "main", Some(browser_summary))
    }

    /// Build a context with an `owner` label (e.g. "main" or a lane id). Each
    /// context tracks its own seen-hashes; staleness is detected against the file
    /// on disk, so lanes need not share any state.
    pub fn with_owner(
        workspace_root: impl Into<PathBuf>,
        owner: impl Into<String>,
    ) -> Result<Self, ToolError> {
        Self::with_owner_and_browser_summary(workspace_root, owner, None)
    }

    fn with_owner_and_browser_summary(
        workspace_root: impl Into<PathBuf>,
        owner: impl Into<String>,
        browser_summary: Option<BrowserSummaryProvider>,
    ) -> Result<Self, ToolError> {
        let root = workspace_root.into();
        let root = if root.exists() {
            root.canonicalize()?
        } else {
            root
        };
        Ok(Self {
            current_dir: Arc::new(Mutex::new(root.clone())),
            workspace_root: root,
            owner: owner.into(),
            browser_summary,
            seen: Arc::new(Mutex::new(HashMap::new())),
            mission_control: false,
            durable_session_id: None,
            mission_control_root: None,
            store_path: None,
            agent_id: None,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Where the next `bash` call starts and relative file paths resolve.
    pub fn current_dir(&self) -> PathBuf {
        self.current_dir
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_current_dir(&self, dir: PathBuf) {
        if dir.is_dir() {
            *self.current_dir.lock().unwrap_or_else(|e| e.into_inner()) = dir;
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn is_mission_control(&self) -> bool {
        self.mission_control
    }

    /// Bind this context to a durable daemon-managed session id. Called by the
    /// serve layer when starting/resuming managed sessions.
    pub fn with_durable_session_id(mut self, id: impl Into<String>) -> Self {
        self.durable_session_id = Some(id.into());
        self
    }

    /// Bind a durable id that may be absent, so a caller wiring several kinds of
    /// session can apply it uniformly rather than branching on `Option` itself.
    pub fn with_durable_session_id_opt(mut self, id: Option<String>) -> Self {
        self.durable_session_id = id;
        self
    }

    pub fn durable_session_id(&self) -> Option<&str> {
        self.durable_session_id.as_deref()
    }

    pub fn mission_control_root(&self) -> Option<PathBuf> {
        self.mission_control_root.clone()
    }

    /// Bind the store path for this session's tools.
    pub fn with_store_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.store_path = Some(path.into());
        self
    }

    pub fn store_path(&self) -> Option<PathBuf> {
        self.store_path.clone()
    }

    pub fn store(&self) -> Result<crate::store::Store, crate::store::StoreError> {
        let path = self.store_path.clone().unwrap_or_else(crate::store::default_db_path);
        crate::store::Store::open_cached(path)
    }

    /// Bind the directory agent id this session runs as.
    pub fn with_agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    /// Bind an optional agent id, so a caller wiring several kinds of session
    /// applies it uniformly instead of branching on `Option` itself.
    pub fn with_agent_id_opt(mut self, id: Option<String>) -> Self {
        self.agent_id = id;
        self
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }


    pub fn browser_summary(&self) -> Option<String> {
        self.browser_summary.as_ref().map(|provider| provider())
    }

    /// Reject a write to `path` when the file on disk differs from what this
    /// context last saw — i.e. it was changed underneath us (by another lane, an
    /// external edit, or another process) since we read it.
    pub fn check_write(&self, path: &Path) -> Result<(), ToolError> {
        let stored = self.seen.lock().unwrap().get(path).copied();
        if let Some(stored) = stored {
            // Compare against the file's CURRENT on-disk bytes. A missing/unreadable
            // file isn't "stale" — let the write itself surface any real error.
            if let Ok(current) = std::fs::read(path) {
                if content_hash(&current) != stored {
                    return Err(ToolError::msg(format!(
                        "`{}` changed on disk since you last read it. Re-read it before writing \
                         so your change is based on its current contents.",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Record that this context has seen `path`'s current on-disk contents (read).
    pub fn mark_read(&self, path: &Path) {
        self.remember(path);
    }

    /// Whether `path` is tracked in `seen` and matches its current bytes on disk.
    pub fn is_file_unchanged(&self, path: &Path) -> bool {
        let stored = self.seen.lock().unwrap().get(path).copied();
        if let Some(stored) = stored {
            if let Ok(current) = std::fs::read(path) {
                return content_hash(&current) == stored;
            }
        }
        false
    }

    /// Record that this context just wrote `path`, so its own follow-up writes
    /// aren't flagged stale.
    pub fn record_change(&self, path: &Path) {
        self.remember(path);
    }

    fn remember(&self, path: &Path) {
        if let Ok(bytes) = std::fs::read(path) {
            self.seen
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), content_hash(&bytes));
        }
    }

    pub fn resolve_workspace_path(&self, raw: &str) -> Result<PathBuf, ToolError> {
        // No workspace jail: the working directory is just the base for relative
        // paths. Absolute paths and `~` resolve as given, so the agent can read or
        // edit any file you point it at (bash already has full access anyway).
        Ok(normalize_workspace_path(&self.current_dir(), raw))
    }
}

fn normalize_workspace_path(root: &Path, raw: &str) -> PathBuf {
    // Expand a leading `~` to the home directory (so `~/code/wacht` works).
    let expanded = if raw == "~" || raw.starts_with("~/") {
        match std::env::var_os("HOME") {
            Some(home) => format!("{}{}", home.to_string_lossy(), &raw[1..]),
            None => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    let candidate = Path::new(&expanded);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub value: Value,
}

impl ToolResult {
    pub fn success(value: Value) -> Self {
        Self {
            value: json!({
                "schema_version": 1,
                "status": "success",
                "data": value,
            }),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            value: json!({
                "schema_version": 1,
                "status": "error",
                "error": {
                    "code": "tool_execution_error",
                    "message": message.into(),
                },
            }),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> NativeToolDefinition;
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    custom_dir: Option<PathBuf>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let name = tool.definition().name;
        self.tools.insert(name, Box::new(tool));
    }

    pub fn with_custom_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.custom_dir = dir;
        self
    }

    pub fn custom_dir(&self) -> Option<&Path> {
        self.custom_dir.as_deref().filter(|_| self.tools.contains_key("bash"))
    }

    fn custom_tools(&self) -> Vec<crate::agent_tools::CustomTool> {
        self.custom_dir()
            .map(crate::agent_tools::load)
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| !self.tools.contains_key(&tool.name))
            .collect()
    }

    pub fn definitions(&self) -> Vec<NativeToolDefinition> {
        let mut defs: Vec<NativeToolDefinition> =
            self.tools.values().map(|tool| tool.definition()).collect();
        defs.extend(self.custom_tools().iter().map(crate::agent_tools::definition));
        defs
    }

    pub fn is_custom(&self, name: &str) -> bool {
        !self.tools.contains_key(name) && self.custom_tools().iter().any(|t| t.name == name)
    }

    /// Remove a tool by name (used to derive scoped registries, e.g. read-only
    /// investigation lanes).
    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name) || self.is_custom(name)
    }

    pub async fn execute(
        &self,
        ctx: &ToolContext,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let tool = match self.tools.get(name) {
            Some(tool) => tool,
            None => return self.execute_custom(ctx, name, arguments).await,
        };
        let mut result = tool.execute(ctx, arguments).await?;
        result.value = bound_tool_output(ctx, name, result.value);
        Ok(result)
    }

    async fn execute_custom(
        &self,
        ctx: &ToolContext,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let dir = self.custom_dir().ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        let call = crate::agent_tools::prepare(dir, name, &arguments)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?
            .map_err(|e| ToolError::msg(format!("{name}: {e}")))?;
        if call.needs_first_approval {
            return Err(ToolError::msg(format!(
                "custom tool `{name}` is new and hasn't been approved by the user yet"
            )));
        }
        let bash = self.tools.get("bash").ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        let mut result = bash.execute(ctx, crate::agent_tools::bash_arguments(&call)).await?;
        if let Some(data) = result.value.get_mut("data").and_then(Value::as_object_mut) {
            data.insert("custom_tool".into(), Value::String(name.to_string()));
        }
        Ok(result)
    }
}

/// Inline ceiling before a tool result is spilled to a scratch file the agent
/// reads with the shell. Ported from wacht's `apply_output_postprocess`.
const MAX_INLINE_OUTPUT_CHARS: usize = 60_000;

/// Keep tool output bounded: when a result renders larger than the inline
/// ceiling, write the full payload to `<workspace>/.snippet/scratch/` and return
/// a small preview envelope pointing at it. `bash` truncates and saves its own
/// output and `view_image` carries an image, so they're exempt.
fn bound_tool_output(ctx: &ToolContext, name: &str, value: Value) -> Value {
    if matches!(name, "view_image" | "bash") {
        return value;
    }
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
    let char_count = rendered.chars().count();
    if char_count <= MAX_INLINE_OUTPUT_CHARS {
        return value;
    }

    let preview: String = rendered.chars().take(4000).collect();
    let stats = json!({ "char_count": char_count, "size_bytes": rendered.len() });

    let scratch = ctx.workspace_root().join(".snippet").join("scratch");
    let file_name = format!(
        "tool_output_{}_{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let saved = scratch.join(&file_name);

    let write = std::fs::create_dir_all(&scratch).and_then(|_| std::fs::write(&saved, &rendered));
    match write {
        Ok(()) => {
            let rel = saved
                .strip_prefix(ctx.workspace_root())
                .unwrap_or(&saved)
                .display()
                .to_string();
            json!({
                "truncated": true,
                "data_omitted": true,
                "preview": preview,
                "saved_output_path": rel,
                "original_stats": stats,
                "hint": format!(
                    "Output exceeded the inline limit; the full result was saved to `{rel}`. \
                     Read the part you need with `sed -n`, `head` or `rg` in bash, or rerun \
                     with a narrower command."
                ),
            })
        }
        // If the scratch write fails, fall back to an inline preview.
        Err(_) => json!({
            "truncated": true,
            "data_omitted": true,
            "preview": preview,
            "original_stats": stats,
            "hint": "Output exceeded the inline limit; rerun with a narrower command or read a smaller slice.",
        }),
    }
}
