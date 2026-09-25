pub mod fs;
pub mod shell_search;
pub mod web;
pub mod memory;
pub mod history;

pub use fs::*;
pub use shell_search::*;
pub use web::*;
pub use memory::*;
pub use history::*;

use serde::Deserialize;
use serde_json::{Value, json};
use crate::tools::{ToolError, ToolRegistry};

pub fn coding_tools(
    exa_api_key: Option<String>,
    memory: crate::memory::MemoryLimits,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.insert(ReadFileTool);
    registry.insert(ReadImageTool);
    registry.insert(WriteFileTool);
    registry.insert(AppendFileTool);
    registry.insert(EditFileTool);
    registry.insert(ListFilesTool);
    registry.insert(SearchFilesTool);
    registry.insert(SearchContentTool);
    registry.insert(ViewOutlineTool);
    registry.insert(CodeMapTool);
    registry.insert(BashTool);
    registry.insert(RecallContextTool);
    registry.insert(SearchHistoryTool);
    // Skill tools only when the user actually has skills installed — otherwise they
    // are dead weight in every prompt's tool list.
    if !crate::skills::discover().is_empty() {
        registry.insert(SearchSkillsTool);
        registry.insert(SkillTool);
    }
    // web_search / web_read are offered only when an Exa key is configured.
    if let Some(key) = exa_api_key.filter(|k| !k.trim().is_empty()) {
        registry.insert(WebSearchTool {
            api_key: key.clone(),
        });
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
            registry.insert(MemoryIndexTool {
                index_budget: memory.index_budget_chars,
            });
            registry.insert(MemoryDeleteTool);
            registry.insert(MemoryRuleTool);
            registry.insert(MemoryPatternTool);
        }
    }
    registry
}

pub(crate) fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub(crate) fn expect_object<T>(tool: &str, arguments: Value) -> Result<T, ToolError>
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

pub(crate) fn slice_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

