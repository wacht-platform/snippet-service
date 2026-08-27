//! System-prompt layers. Every prompt lives as a `.md` file in the `prompts/`
//! directory (repo root) and is embedded at compile time — none are inlined here.

pub const RUNTIME_SANDBOX_ENVIRONMENT: &str = include_str!("../prompts/sandbox_environment.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");
pub const MISSION_CONTROL_LAYER: &str = include_str!("../prompts/mission_control_layer.md");

pub fn coding_system_prompt() -> String {
    [RUNTIME_SANDBOX_ENVIRONMENT, CODING_AGENT_LAYER].join("\n\n")
}

pub fn conversation_system_prompt() -> String {
    [
        RUNTIME_SANDBOX_ENVIRONMENT,
        CODING_AGENT_LAYER,
        CONVERSATION_AGENT_LAYER,
    ]
    .join("\n\n")
}

pub fn mission_control_system_prompt() -> String {
    // Orchestrator only. Do not stack sandbox or CODING_AGENT_LAYER — those
    // identities ("full filesystem", "own the task end to end") made Mission
    // Control advertise as a general engineer and skip list_sessions.
    MISSION_CONTROL_LAYER.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_control_prompt_is_orchestrator_not_coder() {
        let mc = mission_control_system_prompt();
        assert!(mc.contains("Mission Control"));
        assert!(mc.contains("list_sessions"));
        assert!(mc.contains("inspect_session"));
        assert!(mc.contains("create_mission_session"));
        assert!(mc.contains("create_mission_task"));
        assert!(mc.contains("Expect messy, informal"));
        assert!(mc.contains("ask ONE question after intel"));
        assert!(mc.contains("retry_mission_task"));
        assert!(mc.contains("cancel_mission_task"));
        assert!(mc.contains("read_image"));
        assert!(mc.contains("present_file"));
        assert!(mc.contains("openable card"));
        assert!(mc.contains("bash is for inspection only"));
        assert!(mc.contains("never excessively"));
        assert!(mc.contains("Going idle IS waiting"));
        assert!(mc.contains("[mission_task_report]"));
        assert!(mc.contains("Gather first. Confirm second. Route third."));
        assert!(mc.contains("~/.snippet/mission-control"));
        assert!(mc.contains("last_active"));
        assert!(mc.contains("Do not ask other sessions what they are doing"));
        assert!(mc.contains("Do not do the review yourself because it looks small"));
        assert!(mc.contains(
            "do a status/review/diff yourself when a matching session already owns that repo"
        ));
        assert!(mc.contains("inspect_session output is another chat's history"));
        assert!(mc.contains("[steering] … [/steering]"));
        let coding = coding_system_prompt();
        assert!(coding.contains("[steering] … [/steering]"));
        let conversation = conversation_system_prompt();
        assert!(conversation.contains("[steering] … [/steering]"));
        assert!(mc.contains("never reply to, quote, acknowledge, or mention it"));
        assert!(!mc.contains("snippet_execution_agent"));
        assert!(!mc.contains("coding/execution agent"));
        assert!(!mc.contains("you own the task end to end"));
        assert!(!mc.contains("NO sandbox or jail"));
        assert!(!mc.contains("$SNIPPET_SHADOW_GIT"));
        let coding = coding_system_prompt();
        assert!(coding.contains("snippet_execution_agent"));
        assert!(coding.contains("Do the work in THIS session"));
        assert!(coding.contains("you MUST call report_mission_task"));
        assert!(coding.contains("redo the evaluation from current sources"));
        assert!(!coding.contains("You are Mission Control"));
        let conversation = conversation_system_prompt();
        assert!(conversation.contains("snippet_conversation_agent"));
        assert!(conversation.contains("a new lane will miss it"));
        assert!(!conversation.contains("[worker_envelope]"));
        assert!(!conversation.contains("You are Mission Control"));
    }
}
