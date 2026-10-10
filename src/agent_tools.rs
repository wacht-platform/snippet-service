use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;

const APPROVALS_FILE: &str = ".approved.json";
const MAX_SPEC_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct CustomTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_schema")]
    pub parameters: Value,
    pub command: String,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(skip)]
    pub hash: String,
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

pub fn tools_dir(agent_id: &str) -> Option<PathBuf> {
    crate::coordination::AgentHome::new(
        crate::coordination::agents_root(&crate::config::snippet_home().join("mission-control")),
        agent_id,
    )
    .ok()
    .map(|home| home.tools_path())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

pub fn load(dir: &Path) -> Vec<CustomTool> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut tools: Vec<CustomTool> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter(|e| e.file_name() != APPROVALS_FILE)
        .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX_SPEC_BYTES))
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let mut tool: CustomTool = serde_json::from_str(&raw).ok()?;
            if !valid_name(&tool.name) || tool.command.trim().is_empty() {
                return None;
            }
            if !tool.parameters.is_object() {
                tool.parameters = empty_schema();
            }
            let mut hasher = DefaultHasher::new();
            raw.hash(&mut hasher);
            tool.hash = format!("{:016x}", hasher.finish());
            Some(tool)
        })
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools.dedup_by(|a, b| a.name == b.name);
    tools
}

pub fn find(dir: &Path, name: &str) -> Option<CustomTool> {
    load(dir).into_iter().find(|t| t.name == name)
}

pub fn definition(tool: &CustomTool) -> NativeToolDefinition {
    let mut schema = tool.parameters.clone();
    if let Some(obj) = schema.as_object_mut() {
        obj.entry("type").or_insert(json!("object"));
        obj.entry("properties").or_insert(json!({}));
    }
    NativeToolDefinition {
        name: tool.name.clone(),
        description: format!("{} (custom tool)", tool.description.trim()),
        input_schema: schema,
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => shell_quote(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items.iter().map(render).collect::<Vec<_>>().join(" "),
        Value::Object(_) => shell_quote(&value.to_string()),
    }
}

pub fn expand(tool: &CustomTool, arguments: &Value) -> Result<String, String> {
    let args = arguments.as_object().cloned().unwrap_or_default();
    let required: Vec<&str> = tool
        .parameters
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| args.get(*name).is_none_or(Value::is_null))
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing required argument(s): {}", missing.join(", ")));
    }
    let mut out = String::with_capacity(tool.command.len());
    let mut rest = tool.command.as_str();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = after[..end].trim();
        out.push_str(&args.get(key).map(render).unwrap_or_default());
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn approvals(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(dir.join(APPROVALS_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn is_approved(dir: &Path, tool: &CustomTool) -> bool {
    approvals(dir).contains_key(&tool.name)
}

pub fn approve(dir: &Path, tool: &CustomTool) {
    let mut all = approvals(dir);
    all.insert(tool.name.clone(), tool.hash.clone());
    if let Ok(body) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(dir.join(APPROVALS_FILE), body);
    }
}

pub struct CustomCall {
    pub tool: CustomTool,
    pub command: String,
    pub vault: Vec<String>,
    pub needs_first_approval: bool,
}

pub fn prepare(dir: &Path, name: &str, arguments: &Value) -> Option<Result<CustomCall, String>> {
    let tool = find(dir, name)?;
    Some(expand(&tool, arguments).map(|command| CustomCall {
        vault: crate::vault::Vault::load().referenced_names(&command),
        needs_first_approval: !is_approved(dir, &tool),
        command,
        tool,
    }))
}

pub fn approval_summary(call: &CustomCall) -> String {
    let mut parts = Vec::new();
    if call.needs_first_approval {
        parts.push(format!("new custom tool `{}`", call.tool.name));
    } else {
        parts.push(format!("custom tool `{}`", call.tool.name));
    }
    if !call.vault.is_empty() {
        parts.push(format!("uses vault secret(s) [{}]", call.vault.join(", ")));
    }
    format!("⚠ {} — runs: {}", parts.join(", "), call.command)
}

pub fn bash_arguments(call: &CustomCall) -> Value {
    let mut args = json!({"command": call.command, "label": call.tool.name});
    if let Some(timeout) = call.tool.timeout_seconds {
        args["timeout_seconds"] = json!(timeout);
    }
    args
}

pub fn prompt_section(dir: &Path) -> String {
    format!(
        "## Your custom tools\n\nYou can give yourself tools. Each JSON file in `{}` defines one: `name` (lowercase, digits and underscores), `description`, `parameters` (a JSON Schema object with `properties` and `required`), `command` (a bash template; `{{{{arg}}}}` is replaced by that argument, shell-quoted for you, and an omitted optional argument becomes empty), and optional `timeout_seconds`. Write or edit one with `change_files`; it's available on your next call, no restart needed. A new tool waits for the user's approval the first time it runs, so tell them what it does; editing it afterwards needs no new approval. A delegated lane can use your tools too, and anything that needs approval there reaches the user through your chat. The command runs in the workspace like `bash`, and can use vault secrets as `$NAME` (those runs always ask the user first). Make a tool for a command you run repeatedly with different arguments, not for one-offs.",
        dir.display()
    )
}
