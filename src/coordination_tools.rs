use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::coordination::{
    CoordinationDb,
    types::{ContextMode, CoordinationEvent, Handoff},
};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

/// Tools every coordination-aware session (Mission Control and workers) can use
/// to discover peers and post to the board.
pub fn add_coordination_tools(registry: &mut ToolRegistry) {
    registry.insert(ListCoordinationAgents);
    registry.insert(PostCoordinationMessage);
    registry.insert(ReadCoordinationThread);
}

/// Tools only Mission Control may use to offer new work.
pub fn add_coordination_dispatch_tools(registry: &mut ToolRegistry) {
    registry.insert(CreateCoordinationAssignment);
}

/// Turn-taking tools: accept an assignment (acquire the fenced lease), renew it
/// while working, and release it when done or handing off.
pub fn add_coordination_lease_tools(registry: &mut ToolRegistry) {
    registry.insert(AcceptCoordinationAssignment);
    registry.insert(RenewCoordinationLease);
    registry.insert(ReleaseCoordinationLease);
    registry.insert(CoordinationLeaseStatus);
}

/// Handoff tools: the current holder records an immutable transfer for a
/// successor, who must acknowledge the exact record before taking the turn.
pub fn add_coordination_handoff_tools(registry: &mut ToolRegistry) {
    registry.insert(CreateCoordinationHandoff);
    registry.insert(AcknowledgeCoordinationHandoff);
}

/// Open the coordination database the daemon owns. Prefers the path bound to this
/// session; falls back to the Mission Control root for older call sites.
fn db(ctx: &ToolContext) -> Result<CoordinationDb, ToolError> {
    let path = ctx
        .coordination_db_path()
        .or_else(|| {
            ctx.mission_control_root()
                .map(|root| root.join("coordination.sqlite3"))
        })
        .ok_or_else(|| {
            ToolError::msg("coordination tools require a coordination database context")
        })?;
    CoordinationDb::open(path)
        .map_err(|e| ToolError::msg(format!("open coordination database: {e}")))
}

/// The board identity of the current session. The model never supplies this:
/// a session posts as itself, so it cannot impersonate another agent.
fn actor(ctx: &ToolContext) -> Result<(&'static str, String), ToolError> {
    ctx.durable_session_id()
        .map(|id| ("agent", id.to_string()))
        .ok_or_else(|| {
            ToolError::msg("posting to the coordination board requires a durable session identity")
        })
}

/// Best-effort git facts for the handoff's `workspace_ref`. The successor needs
/// the repository/branch/revision it will inherit; a non-git workspace yields an
/// empty object rather than failing the handoff.
fn derive_workspace_ref(workspace: &std::path::Path) -> Value {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let mut map = serde_json::Map::new();
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        map.insert("repository".into(), json!(root));
    }
    if let Some(branch) = git(&["rev-parse", "--abbrev-ref", "HEAD"]) {
        map.insert("branch".into(), json!(branch));
    }
    if let Some(revision) = git(&["rev-parse", "HEAD"]) {
        map.insert("revision".into(), json!(revision));
    }
    Value::Object(map)
}

/// The handoff's `context_manifest`: what the successor should read to rebuild
/// context, derived from live state rather than supplied by the model.
fn derive_context_manifest(workspace: &std::path::Path) -> Value {
    json!({
        "workspace": workspace.display().to_string(),
        "session_scope": "the session transcript holds the full history; this manifest is only retrieval pointers",
    })
}

pub struct ListCoordinationAgents;
#[async_trait]
impl Tool for ListCoordinationAgents {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_coordination_agents".into(),
            description: "List specialized agents in the coordination directory. Use it to pick a direct recipient; Mission Control does not relay their messages.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let agents = db(ctx)?
            .list_agents()
            .map_err(|e| ToolError::msg(format!("list agents: {e}")))?;
        Ok(ToolResult::success(json!({"agents": agents})))
    }
}

#[derive(Deserialize)]
struct PostArgs {
    thread_id: String,
    body: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

pub struct PostCoordinationMessage;
#[async_trait]
impl Tool for PostCoordinationMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "post_coordination_message".into(),
            description: "Post a message or update to a coordination board thread. This talks to other permitted agents directly; it does not transfer ownership or acquire a session lease. The post is attributed to this session automatically.".into(),
            input_schema: schema(
                json!({
                    "thread_id":{"type":"string"},
                    "body":{"type":"string"},
                    "idempotency_key":{"type":"string"}
                }),
                &["thread_id", "body"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PostArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.thread_id.trim().is_empty() {
            return Err(ToolError::msg("thread_id must not be empty"));
        }
        if args.body.trim().is_empty() {
            return Err(ToolError::msg("body must not be empty"));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        let key = args
            .idempotency_key
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: args.thread_id.clone(),
            partition_key: format!("thread:{}", args.thread_id),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: actor_kind.to_string(),
            actor_id,
            payload_version: 1,
            payload: json!({"body": args.body}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: key,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let saved = db(ctx)?
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        Ok(ToolResult::success(json!({"event": saved})))
    }
}

/// Default slice of room history handed over when a participant is woken. Small
/// enough to keep the wake cheap, large enough to hold the current exchange.
pub const BOARD_HISTORY_ON_WAKE: u32 = 10;

#[derive(Deserialize)]
struct ReadThreadArgs {
    #[serde(default)]
    thread_id: Option<String>,
    /// Page forward from just after this sequence. Omit to get the most recent
    /// messages instead (the usual first call).
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// One board message, flattened for the model: who said what, when.
fn flatten_event(event: &CoordinationEvent) -> Value {
    json!({
        "sequence": event.sequence,
        "from": event.actor_id,
        "kind": event.actor_kind,
        "at": event.created_at,
        "body": event.payload.get("body").and_then(|v| v.as_str()).unwrap_or_default(),
    })
}

pub struct ReadCoordinationThread;
#[async_trait]
impl Tool for ReadCoordinationThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_coordination_thread".into(),
            description: "Read the coordination room's messages, oldest first. Call with no arguments for the most recent messages; pass after_sequence to page further back (use the returned oldest_sequence) or forward. The wake message already includes the recent history, so use this only to see more.".into(),
            input_schema: schema(
                json!({
                    "thread_id":{"type":"string","description":"defaults to the shared room"},
                    "after_sequence":{"type":"integer","minimum":0,"description":"omit for the most recent messages"},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadThreadArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let thread_id = args
            .thread_id
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| crate::serve::COORDINATION_THREAD.to_string());
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let db = db(ctx)?;

        // No cursor → the most recent messages. Cursor → the next page forward.
        // `after_sequence: 0` deliberately means "from the beginning".
        let (events, page_forward) = match args.after_sequence {
            Some(after) => (
                db.events_for_thread(&thread_id, after, limit)
                    .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
                true,
            ),
            None => (
                db.recent_events_for_thread(&thread_id, limit)
                    .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
                false,
            ),
        };

        let oldest = events.first().map(|e| e.sequence).unwrap_or(0);
        let newest = events.last().map(|e| e.sequence).unwrap_or(0);
        // A full page is a hint there may be more; the caller pages with the
        // oldest sequence to go back, or the newest to move forward.
        let full_page = events.len() as u32 >= limit;
        Ok(ToolResult::success(json!({
            "thread_id": thread_id,
            "messages": events.iter().map(flatten_event).collect::<Vec<_>>(),
            "oldest_sequence": oldest,
            "newest_sequence": newest,
            "may_have_more": full_page,
            "direction": if page_forward { "forward" } else { "recent" },
        })))
    }
}

#[derive(Deserialize)]
struct AssignmentArgs {
    #[serde(default)]
    id: Option<String>,
    goal_id: String,
    session_id: String,
    agent_id: String,
    scope: String,
    definition_of_done: String,
}

pub struct CreateCoordinationAssignment;
#[async_trait]
impl Tool for CreateCoordinationAssignment {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "create_coordination_assignment".into(),
            description: "Offer an assignment to a specialized agent. This records ownership intent only; the agent must accept it and acquire a session lease before executing. A board message alone never grants ownership.".into(),
            input_schema: schema(json!({
                "id":{"type":"string","description":"optional; generated when omitted"},
                "goal_id":{"type":"string"},
                "session_id":{"type":"string"},
                "agent_id":{"type":"string"},
                "scope":{"type":"string"},
                "definition_of_done":{"type":"string"}
            }), &["goal_id","session_id","agent_id","scope","definition_of_done"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: AssignmentArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        for (field, value) in [
            ("goal_id", &args.goal_id),
            ("session_id", &args.session_id),
            ("agent_id", &args.agent_id),
            ("scope", &args.scope),
            ("definition_of_done", &args.definition_of_done),
        ] {
            if value.trim().is_empty() {
                return Err(ToolError::msg(format!("{field} must not be empty")));
            }
        }
        let now = chrono::Utc::now().to_rfc3339();
        let assignment = crate::coordination::Assignment {
            id: args
                .id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            goal_id: args.goal_id,
            session_id: args.session_id,
            agent_id: args.agent_id,
            status: crate::coordination::AssignmentStatus::Offered,
            scope: args.scope,
            definition_of_done: args.definition_of_done,
            created_at: now.clone(),
            updated_at: now,
        };
        db(ctx)?
            .create_assignment(&assignment)
            .map_err(|e| ToolError::msg(format!("create assignment: {e}")))?;
        Ok(ToolResult::success(json!({"assignment": assignment})))
    }
}

/// Lease TTL: long enough to cover a turn, short enough that a dead holder stops
/// blocking the session. Renewal extends from now.
const LEASE_TTL_SECS: i64 = 90;

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn expiry_from_now() -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(LEASE_TTL_SECS)).to_rfc3339()
}

/// The agent id this session acts as: the bound directory id when present,
/// otherwise the durable session id acts on its own behalf.
fn lease_agent_id(ctx: &ToolContext) -> Result<String, ToolError> {
    ctx.agent_id()
        .map(str::to_string)
        .or_else(|| ctx.durable_session_id().map(str::to_string))
        .ok_or_else(|| ToolError::msg("lease tools require a durable session identity"))
}

/// Enforce this session's turn fence before a workspace mutation.
///
/// A session holding no coordination lease is unconstrained, so ordinary
/// sessions stay free. A session that holds one must still own the turn: an
/// expired, released, or superseded lease is refused, so a replaced agent can
/// never corrupt the successor's turn. The claim is kept on refusal (the session
/// must stop mutating); recovery is to release, then accept again.
pub fn enforce_turn_fence(ctx: &ToolContext) -> Result<(), ToolError> {
    let Some(claim) = ctx.lease_claim() else {
        return Ok(());
    };
    let valid = db(ctx)?
        .check_fence(
            &claim.session_id,
            &claim.lease_id,
            claim.fencing_token,
            &now_rfc3339(),
        )
        .map_err(|e| ToolError::msg(format!("check turn lease: {e}")))?;
    if !valid {
        return Err(ToolError::msg(
            "this session no longer holds the turn lease (expired, released, or replaced by another agent). Stop mutating shared work; release the lease and re-accept the assignment to continue.",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct AcceptArgs {
    assignment_id: String,
}

pub struct AcceptCoordinationAssignment;
#[async_trait]
impl Tool for AcceptCoordinationAssignment {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "accept_coordination_assignment".into(),
            description: "Accept an offered assignment and acquire its fenced session turn lease. Only the offered agent may accept. The lease is the exclusive right to mutate the session; renew it while working and release it when done or handing off.".into(),
            input_schema: schema(
                json!({"assignment_id":{"type":"string"}}),
                &["assignment_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: AcceptArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.assignment_id.trim().is_empty() {
            return Err(ToolError::msg("assignment_id must not be empty"));
        }
        let db = db(ctx)?;
        let agent_id = lease_agent_id(ctx)?;
        let assignment = db
            .get_assignment(&args.assignment_id)
            .map_err(|e| ToolError::msg(format!("load assignment: {e}")))?
            .ok_or_else(|| ToolError::msg("unknown assignment"))?;
        if assignment.agent_id != agent_id {
            return Err(ToolError::msg(format!(
                "assignment is offered to `{}`, not `{agent_id}`",
                assignment.agent_id
            )));
        }
        // Gate: a handoff targeting this assignment must be acknowledged before
        // the successor takes the turn, so ownership never changes without the
        // successor accepting the exact recorded context.
        if db
            .has_unacknowledged_handoff(&assignment.id)
            .map_err(|e| ToolError::msg(format!("check handoff: {e}")))?
        {
            return Err(ToolError::msg(
                "a handoff targets this assignment and has not been acknowledged — read it and call acknowledge_coordination_handoff before accepting the turn",
            ));
        }
        let now = now_rfc3339();
        // Take the fenced lease FIRST: it fails cleanly when the session already
        // has a holder, so a refused accept never leaves the assignment stranded
        // in `accepted`.
        let lease = crate::coordination::SessionLease {
            session_id: assignment.session_id.clone(),
            lease_id: Uuid::new_v4().to_string(),
            assignment_id: assignment.id.clone(),
            agent_id,
            fencing_token: 0,
            acquired_at: now.clone(),
            renewed_at: now.clone(),
            expires_at: expiry_from_now(),
        };
        let acquired = match db
            .acquire_lease(&lease)
            .map_err(|e| ToolError::msg(format!("acquire lease: {e}")))?
        {
            Some(acquired) => acquired,
            None => {
                return Err(ToolError::msg(
                    "session already has an active turn holder — wait for it to release or expire",
                ));
            }
        };
        // Reserve the work. If this races (already accepted/terminal), release the
        // lease we just took so the turn is not left held by a failed accept.
        if !db
            .transition_assignment(
                &assignment.id,
                crate::coordination::AssignmentStatus::Offered,
                crate::coordination::AssignmentStatus::Accepted,
                &now,
            )
            .map_err(|e| ToolError::msg(format!("accept assignment: {e}")))?
        {
            let _ = db.release_lease(&acquired.lease_id, &now_rfc3339(), "accept_failed");
            return Err(ToolError::msg(
                "assignment is not in the offered state (already accepted or terminal)",
            ));
        }
        // From here this session owns the turn: the harness enforces this fence
        // before every workspace mutation.
        ctx.set_lease_claim(Some(crate::tools::LeaseClaim {
            session_id: acquired.session_id.clone(),
            lease_id: acquired.lease_id.clone(),
            assignment_id: acquired.assignment_id.clone(),
            fencing_token: acquired.fencing_token,
        }));
        Ok(ToolResult::success(json!({
            "assignment": assignment.id,
            "session_id": acquired.session_id,
            "lease_id": acquired.lease_id,
            "fencing_token": acquired.fencing_token,
            "expires_at": acquired.expires_at,
        })))
    }
}

#[derive(Deserialize)]
struct RenewArgs {
    lease_id: String,
    fencing_token: u64,
}

pub struct RenewCoordinationLease;
#[async_trait]
impl Tool for RenewCoordinationLease {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "renew_coordination_lease".into(),
            description: "Extend the current turn lease while still working. Requires the exact lease id and fencing token. A refused renewal means this holder no longer owns the turn.".into(),
            input_schema: schema(
                json!({"lease_id":{"type":"string"},"fencing_token":{"type":"integer","minimum":1}}),
                &["lease_id", "fencing_token"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: RenewArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.lease_id.trim().is_empty() || args.fencing_token == 0 {
            return Err(ToolError::msg(
                "lease_id and a positive fencing_token are required",
            ));
        }
        let renewed_at = now_rfc3339();
        let expires_at = expiry_from_now();
        let ok = db(ctx)?
            .renew_lease(&args.lease_id, args.fencing_token, &renewed_at, &expires_at)
            .map_err(|e| ToolError::msg(format!("renew lease: {e}")))?;
        if !ok {
            return Err(ToolError::msg(
                "lease is not active for this id and token — the turn was released, expired, or replaced",
            ));
        }
        Ok(ToolResult::success(json!({
            "lease_id": args.lease_id,
            "fencing_token": args.fencing_token,
            "renewed_at": renewed_at,
            "expires_at": expires_at,
        })))
    }
}

#[derive(Deserialize)]
struct ReleaseArgs {
    lease_id: String,
    #[serde(default)]
    reason: Option<String>,
}

pub struct ReleaseCoordinationLease;
#[async_trait]
impl Tool for ReleaseCoordinationLease {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "release_coordination_lease".into(),
            description: "Release the turn lease when the work is done or handed off, so the session can accept its next holder. Provide the reason (e.g. done, handoff, blocked).".into(),
            input_schema: schema(
                json!({"lease_id":{"type":"string"},"reason":{"type":"string"}}),
                &["lease_id"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReleaseArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.lease_id.trim().is_empty() {
            return Err(ToolError::msg("lease_id must not be empty"));
        }
        let reason = args
            .reason
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "released".to_string());
        let released = db(ctx)?
            .release_lease(&args.lease_id, &now_rfc3339(), &reason)
            .map_err(|e| ToolError::msg(format!("release lease: {e}")))?;
        if released {
            // Ownership is gone: stop enforcing the fence and let the session
            // mutate freely again (or accept its next assignment).
            ctx.set_lease_claim(None);
        }
        Ok(ToolResult::success(json!({
            "lease_id": args.lease_id,
            "released": released,
            "reason": reason,
        })))
    }
}

#[derive(Deserialize)]
struct LeaseStatusArgs {
    session_id: String,
}

pub struct CoordinationLeaseStatus;
#[async_trait]
impl Tool for CoordinationLeaseStatus {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "coordination_lease_status".into(),
            description: "Report the current turn-lease holder of a session (or that it is free). Use before mutating shared work to confirm no other agent owns the turn.".into(),
            input_schema: schema(json!({"session_id":{"type":"string"}}), &["session_id"]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: LeaseStatusArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.session_id.trim().is_empty() {
            return Err(ToolError::msg("session_id must not be empty"));
        }
        let active = db(ctx)?
            .active_lease(&args.session_id, &now_rfc3339())
            .map_err(|e| ToolError::msg(format!("lease status: {e}")))?;
        Ok(ToolResult::success(match active {
            Some(lease) => json!({
                "held": true,
                "agent_id": lease.agent_id,
                "lease_id": lease.lease_id,
                "fencing_token": lease.fencing_token,
                "expires_at": lease.expires_at,
            }),
            None => json!({"held": false}),
        }))
    }
}

#[derive(Deserialize)]
struct HandoffArgs {
    goal_id: String,
    session_id: String,
    target_assignment_id: String,
    /// `resume_informed` (successor has context) or `fresh_needs_context`.
    context_mode: String,
    objective: String,
    definition_of_done: String,
    scope: String,
    #[serde(default)]
    non_goals: Vec<String>,
    #[serde(default)]
    completed_summary: String,
    #[serde(default)]
    next_action: String,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    risks: Vec<String>,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    artifacts: Vec<Value>,
    #[serde(default)]
    verification: Vec<Value>,
}

pub struct CreateCoordinationHandoff;
#[async_trait]
impl Tool for CreateCoordinationHandoff {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "create_coordination_handoff".into(),
            description: "Record an immutable handoff transferring work to a successor assignment. The successor cannot take the turn until it acknowledges this exact record. A successor cannot see this conversation, so include objective, scope, workspace revision, what's done, the next action, and any risks/blockers/artifacts.".into(),
            input_schema: schema(
                json!({
                    "goal_id":{"type":"string"},
                    "session_id":{"type":"string"},
                    "target_assignment_id":{"type":"string"},
                    "context_mode":{"type":"string","enum":["resume_informed","fresh_needs_context"]},
                    "objective":{"type":"string"},
                    "definition_of_done":{"type":"string"},
                    "scope":{"type":"string"},
                    "non_goals":{"type":"array","items":{"type":"string"}},
                    "completed_summary":{"type":"string"},
                    "next_action":{"type":"string"},
                    "decisions":{"type":"array","items":{"type":"string"}},
                    "risks":{"type":"array","items":{"type":"string"}},
                    "blockers":{"type":"array","items":{"type":"string"}},
                    "dependencies":{"type":"array","items":{"type":"string"}},
                    "artifacts":{"type":"array","items":{"type":"object"}},
                    "verification":{"type":"array","items":{"type":"object"}}
                }),
                &[
                    "goal_id",
                    "session_id",
                    "target_assignment_id",
                    "context_mode",
                    "objective",
                    "definition_of_done",
                    "scope",
                ],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: HandoffArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        for (field, value) in [
            ("goal_id", &args.goal_id),
            ("session_id", &args.session_id),
            ("target_assignment_id", &args.target_assignment_id),
            ("objective", &args.objective),
            ("definition_of_done", &args.definition_of_done),
            ("scope", &args.scope),
        ] {
            if value.trim().is_empty() {
                return Err(ToolError::msg(format!("{field} must not be empty")));
            }
        }
        let context_mode = match args.context_mode.as_str() {
            "resume_informed" => ContextMode::ResumeInformed,
            "fresh_needs_context" => ContextMode::FreshNeedsContext,
            other => {
                return Err(ToolError::msg(format!(
                    "context_mode must be resume_informed or fresh_needs_context, got `{other}`"
                )));
            }
        };
        let db = db(ctx)?;
        // The source is the assignment this session currently holds the turn for.
        let source = ctx
            .lease_claim()
            .map(|claim| claim.assignment_id)
            .ok_or_else(|| {
                ToolError::msg("only the current turn holder may create a handoff — accept an assignment first")
            })?;
        // The target must be a real assignment, or the handoff references nothing.
        if db
            .get_assignment(&args.target_assignment_id)
            .map_err(|e| ToolError::msg(format!("load target assignment: {e}")))?
            .is_none()
        {
            return Err(ToolError::msg("unknown target assignment"));
        }
        let mut handoff = Handoff {
            id: Uuid::new_v4().to_string(),
            goal_id: args.goal_id,
            session_id: args.session_id,
            source_assignment_id: source,
            target_assignment_id: args.target_assignment_id,
            context_mode,
            objective: args.objective,
            definition_of_done: args.definition_of_done,
            scope: args.scope,
            non_goals: args.non_goals,
            workspace_ref: derive_workspace_ref(ctx.workspace_root()),
            completed_summary: args.completed_summary,
            next_action: args.next_action,
            decisions: args.decisions,
            risks: args.risks,
            blockers: args.blockers,
            dependencies: args.dependencies,
            artifacts: args.artifacts,
            verification: args.verification,
            context_manifest: derive_context_manifest(ctx.workspace_root()),
            created_at: now_rfc3339(),
            content_hash: String::new(),
        };
        handoff.content_hash = handoff.compute_content_hash();
        db.create_handoff(&handoff)
            .map_err(|e| ToolError::msg(format!("create handoff: {e}")))?;
        Ok(ToolResult::success(json!({
            "handoff_id": handoff.id,
            "content_hash": handoff.content_hash,
            "target_assignment_id": handoff.target_assignment_id,
        })))
    }
}

#[derive(Deserialize)]
struct AcknowledgeHandoffArgs {
    target_assignment_id: String,
}

pub struct AcknowledgeCoordinationHandoff;
#[async_trait]
impl Tool for AcknowledgeCoordinationHandoff {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "acknowledge_coordination_handoff".into(),
            description: "Acknowledge the handoff targeting an assignment, accepting its exact content (verified by hash). Required before the successor assignment can acquire the turn. Fails if the record changed since it was read.".into(),
            input_schema: schema(
                json!({"target_assignment_id":{"type":"string"}}),
                &["target_assignment_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: AcknowledgeHandoffArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.target_assignment_id.trim().is_empty() {
            return Err(ToolError::msg("target_assignment_id must not be empty"));
        }
        let db = db(ctx)?;
        let handoff = db
            .acknowledge_handoff_by_target(&args.target_assignment_id, &now_rfc3339())
            .map_err(|e| ToolError::msg(format!("acknowledge handoff: {e}")))?
            .ok_or_else(|| ToolError::msg("no unacknowledged handoff targets that assignment"))?;
        // The stored record must still match its own hash; a mismatch means the
        // payload was tampered with after it was written.
        if handoff.content_hash != handoff.compute_content_hash() {
            return Err(ToolError::msg(
                "handoff content does not match its recorded hash — refusing to acknowledge a mutated record",
            ));
        }
        Ok(ToolResult::success(json!({
            "handoff_id": handoff.id,
            "content_hash": handoff.content_hash,
            "objective": handoff.objective,
            "completed_summary": handoff.completed_summary,
            "next_action": handoff.next_action,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentRole, AgentStatus};

    fn context(root: &std::path::Path, session: &str) -> ToolContext {
        ToolContext::new(root)
            .unwrap()
            .with_durable_session_id(session)
            .with_coordination_db_path(root.join("coordination.sqlite3"))
    }

    fn migrate(root: &std::path::Path) -> CoordinationDb {
        CoordinationDb::open(root.join("coordination.sqlite3")).unwrap()
    }

    #[tokio::test]
    async fn post_message_is_attributed_to_the_session_not_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");

        let result = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"t1","body":"starting"}))
            .await
            .unwrap();

        let event_id = result.value["data"]["event"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        let events = db.events_for_thread("t1", 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, event_id);
        // Identity came from the session, not from caller arguments.
        assert_eq!(events[0].actor_id, "mission-control");
        assert_eq!(events[0].actor_kind, "agent");
    }

    #[tokio::test]
    async fn post_message_requires_a_durable_session_identity() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = ToolContext::new(dir.path()).unwrap();
        let error = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"t1","body":"hi"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("durable session identity"));
    }

    #[tokio::test]
    async fn post_message_is_idempotent_for_a_repeated_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        let args = json!({"thread_id":"t1","body":"once","idempotency_key":"k1"});

        PostCoordinationMessage
            .execute(&ctx, args.clone())
            .await
            .unwrap();
        PostCoordinationMessage.execute(&ctx, args).await.unwrap();

        assert_eq!(db.events_for_thread("t1", 0, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_agents_reports_registered_agents() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "web-researcher".into(),
            display_name: "Web Research Specialist".into(),
            handle: "web_researcher".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Researcher,
            capabilities: vec!["web_search".into()],
            max_concurrent_assignments: 3,
            version: 1,
        })
        .unwrap();
        let ctx = context(dir.path(), "mission-control");

        let result = ListCoordinationAgents
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let agents = result.value["data"]["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"], "web-researcher");
    }

    #[tokio::test]
    async fn create_assignment_generates_an_id_and_offers_work() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "w1".into(),
            display_name: "Worker".into(),
            handle: "worker".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 1,
            version: 1,
        })
        .unwrap();
        let ctx = context(dir.path(), "mission-control");

        let result = CreateCoordinationAssignment
            .execute(
                &ctx,
                json!({
                    "goal_id":"g1","session_id":"s1","agent_id":"w1",
                    "scope":"src","definition_of_done":"tests pass"
                }),
            )
            .await
            .unwrap();
        let id = result.value["data"]["assignment"]["id"].as_str().unwrap();
        assert!(!id.is_empty());
        let stored = db.get_assignment(id).unwrap().unwrap();
        assert_eq!(
            stored.status,
            crate::coordination::AssignmentStatus::Offered
        );
    }

    /// Register a worker agent and offer it an assignment in a fresh store.
    fn offered(dir: &std::path::Path, agent_id: &str, assignment_id: &str) -> CoordinationDb {
        let db = migrate(dir);
        db.create_agent(&Agent {
            id: agent_id.into(),
            display_name: "Worker".into(),
            handle: format!("worker-{agent_id}"),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
            max_concurrent_assignments: 1,
            version: 1,
        })
        .unwrap();
        db.create_assignment(&crate::coordination::Assignment {
            id: assignment_id.into(),
            goal_id: "g1".into(),
            session_id: "s1".into(),
            agent_id: agent_id.into(),
            status: crate::coordination::AssignmentStatus::Offered,
            scope: "src".into(),
            definition_of_done: "tests pass".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .unwrap();
        db
    }

    fn worker_ctx(root: &std::path::Path, session: &str, agent_id: &str) -> ToolContext {
        context(root, session).with_agent_id(agent_id)
    }

    #[tokio::test]
    async fn accept_acquires_the_lease_and_reports_a_fencing_token() {
        let dir = tempfile::tempdir().unwrap();
        let db = offered(dir.path(), "w1", "a1");
        let ctx = worker_ctx(dir.path(), "s1", "w1");

        let result = AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();
        let data = &result.value["data"];
        assert_eq!(data["session_id"], "s1");
        assert!(data["fencing_token"].as_u64().unwrap() >= 1);

        let stored = db.get_assignment("a1").unwrap().unwrap();
        assert_eq!(
            stored.status,
            crate::coordination::AssignmentStatus::Accepted
        );
        assert!(db.active_lease("s1", &now_rfc3339()).unwrap().is_some());
    }

    #[tokio::test]
    async fn only_the_offered_agent_may_accept() {
        let dir = tempfile::tempdir().unwrap();
        offered(dir.path(), "w1", "a1");
        let intruder = worker_ctx(dir.path(), "s9", "w9");
        let error = AcceptCoordinationAssignment
            .execute(&intruder, json!({"assignment_id":"a1"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not `w9`"));
    }

    #[tokio::test]
    async fn a_second_agent_cannot_take_an_occupied_turn() {
        let dir = tempfile::tempdir().unwrap();
        offered(dir.path(), "w1", "a1");
        AcceptCoordinationAssignment
            .execute(
                &worker_ctx(dir.path(), "s1", "w1"),
                json!({"assignment_id":"a1"}),
            )
            .await
            .unwrap();

        // Same session, second worker: the lease is held.
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "w2".into(),
            display_name: "Worker 2".into(),
            handle: "worker-w2".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Reviewer,
            capabilities: vec![],
            max_concurrent_assignments: 1,
            version: 1,
        })
        .unwrap();
        db.create_assignment(&crate::coordination::Assignment {
            id: "a2".into(),
            goal_id: "g1".into(),
            session_id: "s1".into(),
            agent_id: "w2".into(),
            status: crate::coordination::AssignmentStatus::Offered,
            scope: "src".into(),
            definition_of_done: "review".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .unwrap();

        let error = AcceptCoordinationAssignment
            .execute(
                &worker_ctx(dir.path(), "s1", "w2"),
                json!({"assignment_id":"a2"}),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already has an active turn holder")
        );
        // The reservation rolled back: the offered assignment is still acceptable later.
        assert_eq!(
            db.get_assignment("a2").unwrap().unwrap().status,
            crate::coordination::AssignmentStatus::Offered
        );
    }

    #[tokio::test]
    async fn lease_status_reports_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        offered(dir.path(), "w1", "a1");
        let ctx = worker_ctx(dir.path(), "s1", "w1");
        assert_eq!(
            CoordinationLeaseStatus
                .execute(
                    &worker_ctx(dir.path(), "s1", "w1"),
                    json!({"session_id":"s1"})
                )
                .await
                .unwrap()
                .value["data"]["held"],
            false
        );

        AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();
        let status = CoordinationLeaseStatus
            .execute(&ctx, json!({"session_id":"s1"}))
            .await
            .unwrap();
        assert_eq!(status.value["data"]["held"], true);
        assert_eq!(status.value["data"]["agent_id"], "w1");
    }

    #[tokio::test]
    async fn renew_requires_the_current_token_and_release_frees_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let db = offered(dir.path(), "w1", "a1");
        let ctx = worker_ctx(dir.path(), "s1", "w1");
        let acquired = AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();
        let lease_id = acquired.value["data"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();
        let token = acquired.value["data"]["fencing_token"].as_u64().unwrap();

        // Wrong token is refused; the real one renews.
        assert!(
            RenewCoordinationLease
                .execute(&ctx, json!({"lease_id":lease_id,"fencing_token":token + 1}))
                .await
                .is_err()
        );
        RenewCoordinationLease
            .execute(&ctx, json!({"lease_id":lease_id,"fencing_token":token}))
            .await
            .unwrap();

        // Releasing frees the session for its next holder.
        ReleaseCoordinationLease
            .execute(&ctx, json!({"lease_id":lease_id,"reason":"done"}))
            .await
            .unwrap();
        assert!(db.active_lease("s1", &now_rfc3339()).unwrap().is_none());
        // ...and clears the local claim, so the session mutates freely again.
        assert!(ctx.lease_claim().is_none());
    }

    #[tokio::test]
    async fn a_session_without_a_lease_is_unconstrained() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "plain-session");
        // Ordinary sessions hold no coordination lease and must not be blocked.
        enforce_turn_fence(&ctx).unwrap();
    }

    #[tokio::test]
    async fn fence_allows_the_holder_and_blocks_a_replaced_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = offered(dir.path(), "w1", "a1");
        let ctx = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();

        // Holding the current lease: mutations are allowed.
        assert!(ctx.lease_claim().is_some());
        enforce_turn_fence(&ctx).unwrap();

        // Another agent replaces the holder out-of-band.
        let held = db.active_lease("s1", &now_rfc3339()).unwrap().unwrap();
        db.release_lease(&held.lease_id, &now_rfc3339(), "handoff")
            .unwrap();

        // The stale holder's next mutation is refused.
        let error = enforce_turn_fence(&ctx).unwrap_err();
        assert!(error.to_string().contains("no longer holds the turn lease"));
    }

    #[tokio::test]
    async fn fence_refuses_a_lease_that_expired() {
        let dir = tempfile::tempdir().unwrap();
        let db = offered(dir.path(), "w1", "a1");
        let ctx = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();

        // Force the stored lease past its expiry without an explicit release.
        let held = db.active_lease("s1", &now_rfc3339()).unwrap().unwrap();
        db.expire_lease_for_test(&held.lease_id).unwrap();

        let error = enforce_turn_fence(&ctx).unwrap_err();
        assert!(error.to_string().contains("no longer holds the turn lease"));
    }

    /// Two workers on one session: `w1` holds the source turn, `w2` is offered
    /// the successor assignment the handoff will target.
    fn two_workers(dir: &std::path::Path) -> CoordinationDb {
        let db = migrate(dir);
        for (id, role) in [("w1", AgentRole::Implementer), ("w2", AgentRole::Reviewer)] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                role,
                capabilities: vec![],
                max_concurrent_assignments: 1,
                version: 1,
            })
            .unwrap();
        }
        for (id, agent) in [("a1", "w1"), ("a2", "w2")] {
            db.create_assignment(&crate::coordination::Assignment {
                id: id.into(),
                goal_id: "g1".into(),
                session_id: "s1".into(),
                agent_id: agent.into(),
                status: crate::coordination::AssignmentStatus::Offered,
                scope: "src".into(),
                definition_of_done: "tests pass".into(),
                created_at: "1".into(),
                updated_at: "1".into(),
            })
            .unwrap();
        }
        db
    }

    fn handoff_args() -> Value {
        json!({
            "goal_id":"g1",
            "session_id":"s1",
            "target_assignment_id":"a2",
            "context_mode":"fresh_needs_context",
            "objective":"finish the review",
            "definition_of_done":"review signed off",
            "scope":"src",
            "completed_summary":"implementation done, tests pass",
            "next_action":"review the diff",
            "risks":["flaky test"]
        })
    }

    #[tokio::test]
    async fn a_handoff_carries_content_and_a_verifiable_hash() {
        let dir = tempfile::tempdir().unwrap();
        let db = two_workers(dir.path());
        let ctx = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&ctx, json!({"assignment_id":"a1"}))
            .await
            .unwrap();

        let result = CreateCoordinationHandoff
            .execute(&ctx, handoff_args())
            .await
            .unwrap();
        let id = result.value["data"]["handoff_id"].as_str().unwrap();
        let stored = db.get_handoff(id).unwrap().unwrap();
        // The source is the assignment the session actually held.
        assert_eq!(stored.source_assignment_id, "a1");
        assert_eq!(stored.target_assignment_id, "a2");
        assert_eq!(stored.completed_summary, "implementation done, tests pass");
        // The stored hash matches its own content.
        assert_eq!(stored.content_hash, stored.compute_content_hash());
    }

    #[tokio::test]
    async fn only_the_turn_holder_may_create_a_handoff() {
        let dir = tempfile::tempdir().unwrap();
        two_workers(dir.path());
        // No lease accepted yet.
        let error = CreateCoordinationHandoff
            .execute(&worker_ctx(dir.path(), "s1", "w1"), handoff_args())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only the current turn holder"));
    }

    #[tokio::test]
    async fn successor_cannot_take_the_turn_until_it_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        two_workers(dir.path());
        let holder = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&holder, json!({"assignment_id":"a1"}))
            .await
            .unwrap();
        CreateCoordinationHandoff
            .execute(&holder, handoff_args())
            .await
            .unwrap();
        // The holder steps aside, freeing the session.
        let lease_id = holder.lease_claim().unwrap().lease_id;
        ReleaseCoordinationLease
            .execute(&holder, json!({"lease_id":lease_id,"reason":"handoff"}))
            .await
            .unwrap();

        // The successor is blocked: the handoff is not acknowledged.
        let successor = worker_ctx(dir.path(), "s1", "w2");
        let error = AcceptCoordinationAssignment
            .execute(&successor, json!({"assignment_id":"a2"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not been acknowledged"));

        // Acknowledging returns the recorded context and unblocks the turn.
        let ack = AcknowledgeCoordinationHandoff
            .execute(&successor, json!({"target_assignment_id":"a2"}))
            .await
            .unwrap();
        assert_eq!(ack.value["data"]["next_action"], "review the diff");
        AcceptCoordinationAssignment
            .execute(&successor, json!({"assignment_id":"a2"}))
            .await
            .unwrap();
        assert!(successor.lease_claim().is_some());
    }

    #[tokio::test]
    async fn ack_fails_when_the_record_no_longer_matches_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let db = two_workers(dir.path());
        let holder = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&holder, json!({"assignment_id":"a1"}))
            .await
            .unwrap();
        let created = CreateCoordinationHandoff
            .execute(&holder, handoff_args())
            .await
            .unwrap();
        let id = created.value["data"]["handoff_id"].as_str().unwrap();

        // Tamper with the stored payload, leaving the recorded hash stale.
        let mut stored = db.get_handoff(id).unwrap().unwrap();
        stored.objective = "do something else entirely".into();
        let tampered = serde_json::to_string(&stored).unwrap();
        db.set_handoff_content_for_test(id, &tampered).unwrap();

        let error = AcknowledgeCoordinationHandoff
            .execute(
                &worker_ctx(dir.path(), "s1", "w2"),
                json!({"target_assignment_id":"a2"}),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its recorded hash")
        );
    }

    #[tokio::test]
    async fn handoff_workspace_ref_and_manifest_are_derived_not_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let db = two_workers(dir.path());
        let holder = worker_ctx(dir.path(), "s1", "w1");
        AcceptCoordinationAssignment
            .execute(&holder, json!({"assignment_id":"a1"}))
            .await
            .unwrap();

        // Caller-supplied workspace_ref/context_manifest are not part of the schema
        // any more; the tool derives both from live state.
        let created = CreateCoordinationHandoff
            .execute(&holder, handoff_args())
            .await
            .unwrap();
        let id = created.value["data"]["handoff_id"].as_str().unwrap();
        let stored = db.get_handoff(id).unwrap().unwrap();

        // A non-git workspace yields an empty ref rather than a bogus one.
        assert_eq!(stored.workspace_ref, json!({}));
        // The manifest records the real workspace the successor inherits.
        assert_eq!(
            stored.context_manifest["workspace"].as_str().unwrap(),
            dir.path().display().to_string()
        );
    }

    #[test]
    fn workspace_ref_reports_branch_and_revision_in_a_git_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(path.join("f.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        let workspace_ref = derive_workspace_ref(path);
        assert!(workspace_ref["branch"].is_string());
        let revision = workspace_ref["revision"].as_str().unwrap();
        assert_eq!(revision.len(), 40, "expected a full sha, got {revision}");
    }

    #[tokio::test]
    async fn read_thread_defaults_to_the_shared_room_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        for body in ["one", "two", "three"] {
            PostCoordinationMessage
                .execute(
                    &ctx,
                    json!({"thread_id": crate::serve::COORDINATION_THREAD, "body": body}),
                )
                .await
                .unwrap();
        }

        // No arguments: the most recent messages, oldest first, defaults to the
        // shared room.
        let read = ReadCoordinationThread
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let data = &read.value["data"];
        assert_eq!(data["thread_id"], crate::serve::COORDINATION_THREAD);
        let bodies: Vec<&str> = data["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, ["one", "two", "three"]);
        assert_eq!(data["oldest_sequence"], 1);
        assert_eq!(data["newest_sequence"], 3);
    }

    #[tokio::test]
    async fn read_thread_pages_forward_and_back_by_cursor() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        for body in ["m1", "m2", "m3", "m4"] {
            PostCoordinationMessage
                .execute(&ctx, json!({"thread_id": "t", "body": body}))
                .await
                .unwrap();
        }

        // Recent window, then page further back using the returned oldest.
        let recent = ReadCoordinationThread
            .execute(&ctx, json!({"thread_id": "t", "limit": 2}))
            .await
            .unwrap();
        let data = &recent.value["data"];
        assert_eq!(
            data["messages"].as_array().unwrap().len(),
            2,
            "limit bounds the window"
        );
        assert_eq!(data["newest_sequence"], 4);
        assert_eq!(data["oldest_sequence"], 3);
        assert_eq!(data["may_have_more"], true);

        // after_sequence pages forward from a cursor.
        let forward = ReadCoordinationThread
            .execute(&ctx, json!({"thread_id": "t", "after_sequence": 2}))
            .await
            .unwrap();
        let fwd: Vec<&str> = forward.value["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(fwd, ["m3", "m4"]);
        assert_eq!(forward.value["data"]["direction"], "forward");
    }

    #[tokio::test]
    async fn read_thread_reports_an_empty_room_without_error() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        let read = ReadCoordinationThread
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let data = &read.value["data"];
        assert!(data["messages"].as_array().unwrap().is_empty());
        assert_eq!(data["may_have_more"], false);
        assert_eq!(data["newest_sequence"], 0);
    }
}
