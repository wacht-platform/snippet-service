//! System-prompt layers. Every prompt lives as a `.md` file in the `prompts/`
//! directory (repo root) and is embedded at compile time — none are inlined here.

pub const RUNTIME_OPERATING_STYLE: &str = include_str!("../prompts/operating_style.md");
pub const RUNTIME_SANDBOX_ENVIRONMENT: &str = include_str!("../prompts/sandbox_environment.md");
pub const RUNTIME_ARTIFACT_DISCIPLINE: &str = include_str!("../prompts/artifact_discipline.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");

pub fn coding_system_prompt() -> String {
    [
        RUNTIME_OPERATING_STYLE,
        RUNTIME_SANDBOX_ENVIRONMENT,
        RUNTIME_ARTIFACT_DISCIPLINE,
        CODING_AGENT_LAYER,
    ]
    .join("\n\n")
}

pub fn conversation_system_prompt() -> String {
    [
        RUNTIME_OPERATING_STYLE,
        RUNTIME_SANDBOX_ENVIRONMENT,
        RUNTIME_ARTIFACT_DISCIPLINE,
        CODING_AGENT_LAYER,
        CONVERSATION_AGENT_LAYER,
    ]
    .join("\n\n")
}
