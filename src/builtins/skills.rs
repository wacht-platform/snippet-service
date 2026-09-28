use super::*;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct SearchSkillsTool;

#[derive(Debug, Deserialize)]
struct SearchSkillsArgs {
    #[serde(default)]
    query: String,
}

#[async_trait]
impl Tool for SearchSkillsTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "search_skills".to_string(),
            description:
                "Find a reusable Agent Skill relevant to the task. Skills are installed procedures / playbooks / recipes (a specific workflow, a tool or API integration, a generation or formatting routine). They are NOT preloaded into your context, so search here BEFORE improvising anything that sounds like an established procedure — a matching skill gives you the exact steps. Returns candidate skills (name + description); load the best one with `skill(name)` to get its full instructions. An empty query lists everything available."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "query": {"type": "string", "description": "What you're trying to do — keywords or a short phrase. Empty lists all skills."}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SearchSkillsArgs = expect_object("search_skills", arguments)?;
        let skills: Vec<Value> = crate::skills::search(&args.query)
            .into_iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect();
        let note = if skills.is_empty() {
            "no skills installed"
        } else {
            "call skill(name) to load a skill's full instructions"
        };
        Ok(ToolResult::success(json!({
            "query": args.query,
            "count": skills.len(),
            "skills": skills,
            "note": note,
        })))
    }
}

pub struct SkillTool;

#[derive(Debug, Deserialize)]
struct SkillArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "skill".to_string(),
            description:
                "Load an Agent Skill by name (find one first with search_skills) — returns its full instructions (SKILL.md) plus the absolute paths of its bundled files. After loading, follow the instructions; read referenced files with read_file and run bundled scripts with bash (their contents stay out of context until you do)."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "name": {"type": "string", "description": "The skill name, exactly as listed under [skills]."}
                }),
                &["name"],
            ),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SkillArgs = expect_object("skill", arguments)?;
        match crate::skills::load(&args.name) {
            Some((sk, body, files)) => Ok(ToolResult::success(json!({
                "name": sk.name,
                "dir": sk.dir.display().to_string(),
                "instructions": body,
                "bundled_files": files,
            }))),
            None => Err(ToolError::msg(format!(
                "no such skill: {} (see the [skills] list)",
                args.name
            ))),
        }
    }
}
