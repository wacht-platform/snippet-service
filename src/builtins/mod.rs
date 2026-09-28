pub mod fs;
pub mod shell_search;
pub mod web;
pub mod skills;
pub mod process;

pub use fs::*;
pub use shell_search::*;
pub use web::*;
pub use skills::*;
pub use process::*;

use serde::Deserialize;
use serde_json::{Value, json};
use crate::tools::{ToolError, ToolRegistry};

pub fn coding_tools(exa_api_key: Option<String>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.insert(BashTool);
    registry.insert(ChangeFilesTool);
    registry.insert(ViewImageTool);
    registry.insert(ManageProcessTool);
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


