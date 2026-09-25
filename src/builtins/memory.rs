use super::*;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
pub struct MemoryReadTool;

#[derive(Debug, Deserialize)]
struct MemoryReadArgs {
    id: String,
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_read".to_string(),
            description: "Load the full content of a workspace memory entry by id (entry ids are listed in the [workspace_memory] index).".to_string(),
            input_schema: object_schema(
                json!({ "id": {"type": "string", "description": "entry id, e.g. build-and-test"} }),
                &["id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: MemoryReadArgs = expect_object("memory_read", arguments)?;
        let content = memory_store(ctx)
            .read_entry(&args.id)
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({ "id": args.id, "content": content }),
        ))
    }
}

pub struct MemoryWriteTool {
    pub entry_budget: usize,
    pub max_entries: usize,
}

#[derive(Debug, Deserialize)]
struct MemoryWriteArgs {
    id: String,
    content: String,
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_write".to_string(),
            description:
                "Create or replace a workspace memory ENTRY — a durable fact, pointer, or \
                how-to playbook for THIS folder that should help future sessions. Use a short \
                kebab-case id. After writing, add or update a one-line pointer to it via \
                memory_index so it stays discoverable."
                    .to_string(),
            input_schema: object_schema(
                json!({
                    "id": {"type": "string", "description": "kebab-case slug, e.g. build-and-test"},
                    "content": {"type": "string"}
                }),
                &["id", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryWriteArgs = expect_object("memory_write", arguments)?;
        memory_store(ctx)
            .write_entry(&args.id, &args.content, self.entry_budget, self.max_entries)
            .map_err(ToolError::msg)?;
        ctx.note_memory_write(&args.id);
        Ok(ToolResult::success(json!({ "id": args.id, "saved": true })))
    }
}

pub struct MemoryIndexTool {
    pub index_budget: usize,
}

#[derive(Debug, Deserialize)]
struct MemoryIndexArgs {
    content: String,
}

#[async_trait]
impl Tool for MemoryIndexTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_index".to_string(),
            description:
                "Replace the always-loaded workspace memory INDEX. Keep it lean: one short \
                line per entry — a label, a one-line summary, and the entry id to load with \
                memory_read. Must fit the index budget (oversize writes are rejected)."
                    .to_string(),
            input_schema: object_schema(json!({ "content": {"type": "string"} }), &["content"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryIndexArgs = expect_object("memory_index", arguments)?;
        memory_store(ctx)
            .write_index(&args.content, self.index_budget)
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(json!({ "saved": true })))
    }
}

pub struct MemoryDeleteTool;

#[derive(Debug, Deserialize)]
struct MemoryDeleteArgs {
    id: String,
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_delete".to_string(),
            description: "Delete a workspace memory entry by id. Also remove its line from the index with memory_index.".to_string(),
            input_schema: object_schema(
                json!({ "id": {"type": "string"} }),
                &["id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryDeleteArgs = expect_object("memory_delete", arguments)?;
        memory_store(ctx)
            .delete_entry(&args.id)
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({ "id": args.id, "deleted": true }),
        ))
    }
}

pub struct MemoryPatternTool;

#[derive(Debug, Deserialize)]
struct MemoryPatternArgs {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl Tool for MemoryPatternTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_pattern".to_string(),
            description: "Save a REUSABLE PATTERN — a generalizable technique that transfers to ANY \
                project, not a fact about this one. A pattern is ONE tight line: the SITUATION it \
                applies to → the APPROACH → briefly WHY. Example: 'stuck async app (browser/REPL) → \
                run it as one resident bg process, drive it step by step, tear down after — one-shot \
                scripts lose all state on failure'. Patterns are GLOBAL and always loaded into every \
                session (yours are under REUSABLE PATTERNS). action='add' (default) appends one new \
                pattern — other sessions also write here, so never rewrite the list just to add. \
                Refining or merging existing patterns → action='replace' with the full curated list. \
                action='clear' wipes them. Use memory_rule for always-obey directives and \
                memory_write for facts/playbooks about THIS workspace."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "action": {"type": "string", "enum": ["add", "replace", "clear"], "description": "add (default): append one pattern line; replace: rewrite the full list (consolidation only); clear: remove all"},
                    "content": {"type": "string", "description": "add: one pattern line (situation → approach → why); replace: the full pattern list"}
                }),
                &["content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryPatternArgs = expect_object("memory_pattern", arguments)?;
        let store = crate::memory::MemoryStore::global();
        let budget = crate::memory::patterns_budget();
        match args.action.as_deref().unwrap_or("add") {
            "add" => {
                let added = store
                    .add_pattern(&args.content, budget)
                    .map_err(ToolError::msg)?;
                Ok(ToolResult::success(json!({
                    "saved": added,
                    "note": if added { "pattern added" } else { "identical pattern already stored" },
                })))
            }
            "replace" => {
                store
                    .write_patterns(&args.content, budget)
                    .map_err(ToolError::msg)?;
                Ok(ToolResult::success(
                    json!({ "saved": true, "note": "pattern list replaced" }),
                ))
            }
            "clear" => {
                store.write_patterns("", budget).map_err(ToolError::msg)?;
                Ok(ToolResult::success(
                    json!({ "saved": true, "note": "patterns cleared" }),
                ))
            }
            other => Err(ToolError::msg(format!(
                "action must be add, replace, or clear — got '{other}'"
            ))),
        }
    }
}

pub struct MemoryRuleTool;

#[derive(Debug, Deserialize)]
struct MemoryRuleArgs {
    scope: String,
    content: String,
}

#[async_trait]
impl Tool for MemoryRuleTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "memory_rule".to_string(),
            description: "Set the STANDING RULES that are always loaded into context and must be \
                followed. `scope`='global' applies in EVERY workspace — use it for cross-cutting \
                user preferences (e.g. \"when writing emails, don't use dashes\"); 'workspace' \
                applies to THIS folder only. Replaces the rule list at that scope, so include all \
                rules you want kept; pass empty content to clear. Keep them short and imperative; \
                never store secrets here."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "scope": {"type": "string", "enum": ["global", "workspace"]},
                    "content": {"type": "string", "description": "the full rule list for this scope (e.g. markdown bullets)"}
                }),
                &["scope", "content"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        require_main_owner(ctx)?;
        let args: MemoryRuleArgs = expect_object("memory_rule", arguments)?;
        let store = match args.scope.as_str() {
            "global" => crate::memory::MemoryStore::global(),
            "workspace" => memory_store(ctx),
            other => {
                return Err(ToolError::msg(format!(
                    "scope must be 'global' or 'workspace', got '{other}'"
                )));
            }
        };
        store
            .write_rules(&args.content, crate::memory::rules_budget())
            .map_err(ToolError::msg)?;
        Ok(ToolResult::success(
            json!({ "scope": args.scope, "saved": true }),
        ))
    }
}

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


fn memory_store(ctx: &ToolContext) -> crate::memory::MemoryStore {
    crate::memory::MemoryStore::for_workspace(ctx.workspace_root())
}

fn require_main_owner(ctx: &ToolContext) -> Result<(), ToolError> {
    if ctx.owner() != "main" {
        return Err(ToolError::msg(
            "workspace memory is read-only in delegated lanes — only the main session can write it",
        ));
    }
    Ok(())
}


