//! System-prompt composition.
//!
//! A stable execution/conversation core is always present. Capability- and
//! environment-specific guidance (git worktree, browser, skills, vault, memory)
//! is appended only when it applies, so an absent capability costs no tokens.

pub const RUNTIME_SANDBOX_ENVIRONMENT: &str = include_str!("../prompts/sandbox_environment.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");
pub const MISSION_CONTROL_LAYER: &str = include_str!("../prompts/mission_control_layer.md");
pub const GIT_WORKTREE_LAYER: &str = include_str!("../prompts/git_worktree_layer.md");
pub const MEMORY_GUIDANCE_LAYER: &str = include_str!("../prompts/memory_layer.md");
pub const MEMORY_WRITE_LAYER: &str = include_str!("../prompts/memory_write_layer.md");
pub const SKILLS_LAYER: &str = include_str!("../prompts/skills_layer.md");
pub const VAULT_LAYER: &str = include_str!("../prompts/vault_layer.md");
pub const BROWSER_LAYER: &str = include_str!("../prompts/browser_command_layer.md");

/// Capability/environment facts that decide which optional layers render.
/// These are a session-start snapshot, so the assembled prompt stays stable
/// across a session's turns (prompt-cache friendly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptContext {
    /// The session workspace is a linked git worktree (not the main checkout).
    pub worktree: bool,
    /// Per-workspace memory is enabled; the index/patterns/rules block is
    /// appended separately at session start.
    pub memory: bool,
    /// This session may write durable memory (main session, not read-only lanes).
    pub memory_writable: bool,
    /// At least one skill is installed.
    pub skills: bool,
    /// The vault holds at least one secret.
    pub vault: bool,
    /// This session can reach connected browsers.
    pub browser: bool,
}

impl PromptContext {
    /// Detect the capability/environment snapshot for `workspace`. Browser
    /// reachability and memory enablement are supplied by the caller (it owns
    /// those facts); skills and vault are probed here.
    pub fn detect(
        workspace: &std::path::Path,
        memory: bool,
        memory_writable: bool,
        browser: bool,
    ) -> Self {
        Self {
            worktree: crate::session::workspace_is_worktree(workspace),
            memory,
            memory_writable: memory && memory_writable,
            skills: !crate::skills::discover().is_empty(),
            vault: !crate::vault::Vault::load().is_empty(),
            browser,
        }
    }

    fn conditional_layers(&self) -> Vec<&'static str> {
        let mut layers = Vec::new();
        if self.worktree {
            layers.push(GIT_WORKTREE_LAYER.trim());
        }
        if self.browser {
            layers.push(BROWSER_LAYER.trim());
        }
        if self.skills {
            layers.push(SKILLS_LAYER.trim());
        }
        if self.vault {
            layers.push(VAULT_LAYER.trim());
        }
        if self.memory {
            layers.push(MEMORY_GUIDANCE_LAYER.trim());
        }
        if self.memory && self.memory_writable {
            layers.push(MEMORY_WRITE_LAYER.trim());
        }
        layers
    }
}

pub fn coding_prompt(context: &PromptContext) -> String {
    let mut parts = vec![
        RUNTIME_SANDBOX_ENVIRONMENT.trim(),
        CODING_AGENT_LAYER.trim(),
    ];
    parts.extend(context.conditional_layers());
    parts.join("\n\n")
}

pub fn conversation_prompt(context: &PromptContext) -> String {
    let mut parts = vec![
        RUNTIME_SANDBOX_ENVIRONMENT.trim(),
        CODING_AGENT_LAYER.trim(),
    ];
    parts.extend(context.conditional_layers());
    parts.push(CONVERSATION_AGENT_LAYER.trim());
    parts.join("\n\n")
}

/// Base execution prompt with no optional capabilities — used by tests and any
/// caller that has not computed a `PromptContext`.
pub fn coding_system_prompt() -> String {
    coding_prompt(&PromptContext::default())
}

/// Base conversation prompt with no optional capabilities.
pub fn conversation_system_prompt() -> String {
    conversation_prompt(&PromptContext::default())
}

pub fn mission_control_system_prompt() -> String {
    // Orchestrator only. Do not stack sandbox or CODING_AGENT_LAYER — those
    // identities ("full filesystem", "own the task end to end") made Mission
    // Control advertise as a general engineer and skip list_sessions.
    MISSION_CONTROL_LAYER.to_string()
}

/// The normal shared session contract plus a bounded researched identity.
/// The role overlay changes judgment and specialization, not the session's
/// execution, safety, or conversation rules.
pub struct SpecializedAgentPromptContext<'a> {
    pub agent_id: &'a str,
    pub identity_revision: u64,
    pub identity: &'a str,
    pub context: &'a PromptContext,
}

pub fn specialized_agent_system_prompt(context: SpecializedAgentPromptContext<'_>) -> String {
    format!(
        "{base}\n\n[agent_identity]\nid = \"{id}\"\nrevision = {revision}\nidentity = \"\"\"\n{identity}\n\"\"\"\n",
        base = conversation_prompt(context.context),
        id = context.agent_id,
        revision = context.identity_revision,
        identity = context.identity.trim(),
    )
}
