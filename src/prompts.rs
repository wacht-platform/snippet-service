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
    fn mission_control_prompt_classifies_messages_before_routing() {
        let mc = mission_control_system_prompt();

        // Mission Control is an orchestrator, not a project worker.
        assert!(mc.contains("Mission Control"));
        assert!(mc.contains("a coding agent"));
        assert!(mc.contains("Classify the current message before selecting a tool"));
        assert!(mc.contains("direct_user"));
        assert!(mc.contains("assigned_first"));
        assert!(mc.contains("worker_report"));

        // Direct user agent requests are not project tasks.
        assert!(mc.contains("The user is allowed to ask Mission Control to build an agent"));
        assert!(mc.contains("If it asks to create/build/spin up an agent"));
        assert!(mc.contains("Do not create a project, workspace, Mission Control task"));
        assert!(mc.contains("POST /agents/build"));
        assert!(mc.contains("Do not turn the brief into project initialization"));
        assert!(mc.contains("Never approximate it with create_mission_task"));

        // An assigned build must be executed or reported blocked, never routed again.
        assert!(mc.contains("[AGENT_BUILD_JOB]"));
        assert!(mc.contains("already assigned work from the daemon"));
        assert!(mc.contains(
            "Do not call create_mission_session, create_mission_task, or create_recurring_job"
        ));
        assert!(mc.contains("report blocked with the exact missing capability"));
        assert!(mc.contains("report the original task_id"));
        assert!(mc.contains("never create a normal project task to compensate"));

        // Ordinary project routing remains available, but only for that message class.
        assert!(mc.contains("This is the only class that normally uses create_mission_task"));
        assert!(mc.contains("For ordinary project requests only, list_sessions first"));
        assert!(mc.contains("route one handoff"));
        assert!(mc.contains("This workflow never applies to agent creation"));
        assert!(mc.contains("One user request gets one task"));
        assert!(mc.contains("retry the same task id"));

        // Lifecycle supervision includes explicit no-op decisions.
        assert!(mc.contains("act, acknowledge, request approval, or explicitly no-op"));
        assert!(mc.contains("No-op is a valid explicit decision"));
        assert!(mc.contains("Do not create a new task merely because a report arrived"));

        assert!(mc.contains("[steering]"));
        assert!(mc.contains("inspect_session output is another chat's history"));
        assert!(!mc.contains("snippet_execution_agent"));
        assert!(!mc.contains("you own the task end to end"));
        assert!(!mc.contains("NO sandbox or jail"));

        let coding = coding_system_prompt();
        assert!(coding.contains("snippet_execution_agent"));
        assert!(coding.contains("Do the work in THIS session"));

        let conversation = conversation_system_prompt();
        assert!(conversation.contains("snippet_conversation_agent"));
        assert!(conversation.contains("Never commit or push to main/master"));
        assert!(!conversation.contains("You are Mission Control"));
    }
}
