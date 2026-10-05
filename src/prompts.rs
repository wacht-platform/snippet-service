//! System-prompt composition.
//!
//! A stable execution/conversation core is always present. Capability- and
//! environment-specific guidance (git worktree, browser, skills, vault, memory)
//! is appended only when it applies, so an absent capability costs no tokens.

pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");
pub const MISSION_CONTROL_LAYER: &str = include_str!("../prompts/mission_control_layer.md");
pub const COORDINATION_LAYER: &str = include_str!("../prompts/coordination_layer.md");
pub const DELEGATION_LAYER: &str = include_str!("../prompts/delegation_layer.md");
pub const TASK_LAYER: &str = include_str!("../prompts/task_layer.md");
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
    /// This is an agent's COORDINATION session: it answers direct messages and
    /// dispatches work, and holds no workspace tools. The layer is what tells it
    /// how to behave, which is not inferable from the tool list alone.
    pub coordination: bool,
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
            // Not inferable from the environment; the role's own constructor sets it.
            coordination: false,
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
        if self.coordination {
            layers.push(COORDINATION_LAYER.trim());
        }
        layers
    }
}

pub fn coding_prompt(context: &PromptContext) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim()];
    parts.extend(context.conditional_layers());
    parts.join("\n\n")
}

pub fn conversation_prompt(context: &PromptContext) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim()];
    parts.extend(context.conditional_layers());
    parts.push(CONVERSATION_AGENT_LAYER.trim());
    parts.push(DELEGATION_LAYER.trim());
    parts.push(TASK_LAYER.trim());
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

/// The prompt for an agent's COORDINATION session.
///
/// Deliberately does NOT include [`CODING_AGENT_LAYER`]: that layer states the
/// session has real bash and full filesystem access, which is false here and
/// would instruct the model to reach for tools it was not given. The
/// coordination layer states the real capability set instead, and the
/// per-workspace memory layers are excluded for the same reason — this session
/// has no workspace to hold memory about.
pub fn coordination_prompt(_context: &PromptContext) -> String {
    // No browser or vault layers: both are about commands this session has no
    // shell to run. COORDINATION_LAYER is last so its statements about what
    // this session can and cannot do are the final word.
    [CONVERSATION_AGENT_LAYER.trim(), COORDINATION_LAYER.trim()].join("\n\n")
}

pub fn mission_control_system_prompt() -> String {
    // Orchestrator only. Do not stack CODING_AGENT_LAYER — its
    // identities ("full filesystem", "own the task end to end") made Mission
    // Control advertise as a general engineer and skip list_sessions.
    let brief = crate::mission_duty::read_brief();
    if brief.trim().is_empty() {
        MISSION_CONTROL_LAYER.to_string()
    } else {
        format!("{}\n\n## Your brief (as of this session's start)\n\n{}", MISSION_CONTROL_LAYER.trim_end(), brief.trim())
    }
}

/// The normal shared session contract plus a bounded researched identity.
/// The role overlay changes judgment and specialization, not the session's
/// execution, safety, or conversation rules.
pub struct SpecializedAgentPromptContext<'a> {
    pub agent_id: &'a str,
    pub identity: &'a str,
    pub context: &'a PromptContext,
}

/// The identity overlay appended to whichever base a session runs on.
///
/// One definition so a specialized coding session and a coordination session
/// cannot disagree about how the agent is introduced to itself.
fn identity_overlay(agent_id: &str, identity: &str) -> String {
    format!(
        "## Your identity: {agent_id}\n\nThe identity below sets your expertise, judgment and voice. Everything above still governs how you work, talk and finish.\n\n{identity}\n",
        identity = identity.trim(),
    )
}

/// A delegated lane's prompt: the execution contract, plus the identity of the
/// agent it was assigned, when there is one.
pub fn lane_prompt(context: &PromptContext, identity: Option<(&str, &str)>) -> String {
    let base = coding_prompt(context);
    match identity {
        Some((agent_id, body)) => format!("{base}\n\n{}", identity_overlay(agent_id, body)),
        None => base,
    }
}

pub fn specialized_agent_system_prompt(context: SpecializedAgentPromptContext<'_>) -> String {
    format!(
        "{base}\n\n{overlay}",
        base = conversation_prompt(context.context),
        overlay = identity_overlay(context.agent_id, context.identity),
    )
}

/// The prompt for an agent's coordination session: the restricted coordination
/// contract plus the agent's own identity.
///
/// Uses [`coordination_prompt`] rather than the conversation prompt so the
/// session is not told it has bash and full filesystem access it was never
/// given.
pub fn specialized_coordination_prompt(context: SpecializedAgentPromptContext<'_>) -> String {
    format!(
        "{base}\n\n{overlay}",
        base = coordination_prompt(context.context),
        overlay = identity_overlay(context.agent_id, context.identity),
    )
}
