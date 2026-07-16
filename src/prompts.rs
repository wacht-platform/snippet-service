//! System-prompt layers. Every prompt lives as a `.md` file in the `prompts/`
//! directory (repo root) and is embedded at compile time — none are inlined here.

pub const RUNTIME_SANDBOX_ENVIRONMENT: &str = include_str!("../prompts/sandbox_environment.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");

pub fn coding_system_prompt() -> String {
    [
        RUNTIME_SANDBOX_ENVIRONMENT,
        CODING_AGENT_LAYER,
    ]
    .join("\n\n")
}

pub fn conversation_system_prompt() -> String {
    [
        RUNTIME_SANDBOX_ENVIRONMENT,
        CODING_AGENT_LAYER,
        CONVERSATION_AGENT_LAYER,
    ]
    .join("\n\n")
}
