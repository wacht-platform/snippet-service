use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;

use crate::llm::NativeToolDefinition;

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
}

impl ToolError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Stable content fingerprint for the staleness guard (DefaultHasher over bytes).
fn content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
pub struct ToolContext {
    workspace_root: PathBuf,
    owner: String,
    /// Whole-file content hashes this context last saw per path — recorded on read
    /// and after its own writes. A write is rejected when the file on disk no
    /// longer matches the recorded hash, catching any change since (including
    /// external edits, another lane, or a concurrent process). Optimistic
    /// concurrency; replaces the former lock registry.
    seen: Arc<Mutex<HashMap<PathBuf, u64>>>,
}

impl ToolContext {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        Self::with_owner(workspace_root, "main")
    }

    /// Build a context with an `owner` label (e.g. "main" or a lane id). Each
    /// context tracks its own seen-hashes; staleness is detected against the file
    /// on disk, so lanes need not share any state.
    pub fn with_owner(
        workspace_root: impl Into<PathBuf>,
        owner: impl Into<String>,
    ) -> Result<Self, ToolError> {
        let root = workspace_root.into();
        let root = if root.exists() {
            root.canonicalize()?
        } else {
            root
        };
        Ok(Self {
            workspace_root: root,
            owner: owner.into(),
            seen: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn owner(&self) -> &str {
        &self.owner
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
        Ok(normalize_workspace_path(&self.workspace_root, raw))
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

    pub fn definitions(&self) -> Vec<NativeToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub async fn execute(
        &self,
        ctx: &ToolContext,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        let mut result = tool.execute(ctx, arguments).await?;
        result.value = bound_tool_output(ctx, name, result.value);
        Ok(result)
    }
}

/// Inline ceiling before a tool result is spilled to a scratch file the agent
/// pages with `read_file`. Ported from wacht's `apply_output_postprocess`.
const MAX_INLINE_OUTPUT_CHARS: usize = 60_000;

/// Keep tool output bounded: when a result renders larger than the inline
/// ceiling, write the full payload to `<workspace>/.snippet/scratch/` and return
/// a small preview envelope pointing at it. `read_file`/`read_image` page
/// themselves, so they're exempt.
fn bound_tool_output(ctx: &ToolContext, name: &str, value: Value) -> Value {
    if matches!(name, "read_file" | "read_image") {
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
                     Page it with read_file using start_char/end_char windows, or rerun with a \
                     narrower command."
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
