use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::coordination::{NotificationMarker, Store, StoreError, types::CoordinationEvent};
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
    // Direct peer conversation: the human and any agent can address one named
    // agent, and every agent can always reach Mission Control.
    registry.insert(SendAgentMessage);
    registry.insert(ReadAgentThread);
    registry.insert(ReadAgentInbox);
    // Per-agent coordination memory: recall before acting, and record what is
    // worth keeping.
    registry.insert(ReadCoordinationBoard);
    registry.insert(RecordCoordinationNote);
    // Task-based coordination and session lease handoff
    registry.insert(MessageMissionControl);
    registry.insert(PostTaskCoordination);
    registry.insert(TransferTaskSessionLease);
    registry.insert(InspectTask);
    registry.insert(ClaimAndDispatchTask);
}


/// Open the store the daemon owns: the path bound to this session when present,
/// otherwise the canonical general-purpose store.
fn db(ctx: &ToolContext) -> Result<Store, ToolError> {
    let path = ctx
        .store_path()
        .unwrap_or_else(crate::store::default_db_path);
    Store::open(path).map_err(|e| ToolError::msg(format!("open store: {e}")))
}

/// The board identity of the current session. The model never supplies this: a
/// session posts as itself, so it cannot impersonate another agent.
///
/// The kind separates the two cases that used to be conflated: an agent working
/// the session posts as `agent`, a plain session posts as `session`. Both are
/// addressable, but only the former is an identity the directory knows.
fn actor(ctx: &ToolContext) -> Result<(&'static str, String), ToolError> {
    // The lease holder on work dispatched here comes first: a lease can move an
    // agent into a session that belongs to someone else, or to no one.
    if let Some(agent_id) = ctx.durable_session_id().and_then(|session| lease_holder(ctx, session)) {
        return Ok(("agent", agent_id));
    }
    if let Some(agent_id) = ctx.agent_id() {
        return Ok(("agent", agent_id.to_string()));
    }
    if let Some(session_id) = ctx.durable_session_id() {
        return Ok(("session", session_id.to_string()));
    }
    Err(ToolError::msg("posting to the coordination board requires a session identity"))
}

/// Refuse a board thread this session has no place in. Allowed: the shared
/// room, a room it is a member of, the room of a task dispatched to this
/// session, and every room for Mission Control, which coordinates them all.
fn require_thread_access(db: &Store, ctx: &ToolContext, actor_id: &str, thread_id: &str) -> Result<(), ToolError> {
    if thread_id == crate::serve::COORDINATION_THREAD {
        return Ok(());
    }
    let lookup = |e: StoreError| ToolError::msg(format!("look up thread: {e}"));
    if !db.thread_exists(thread_id).map_err(lookup)? {
        return Err(ToolError::msg(format!("no thread `{thread_id}`")));
    }
    if actor_id == crate::mission_control::SESSION_ID
        || db
            .list_thread_participants(thread_id)
            .map_err(lookup)?
            .iter()
            .any(|(member, _)| member == actor_id)
    {
        return Ok(());
    }
    let own_task = db
        .get_task_by_thread(thread_id)
        .map_err(lookup)?
        .is_some_and(|task| Some(task.session_id.as_str()) == ctx.durable_session_id());
    if own_task {
        return Ok(());
    }
    Err(ToolError::msg(format!("this session is not a member of `{thread_id}`")))
}

/// The specialized agent holding the lease on a task this session is running.
/// The default worker is not one: it has no identity beyond the session's own.
fn lease_holder(ctx: &ToolContext, session_id: &str) -> Option<String> {
    let db = db(ctx).ok()?;
    let tasks = db
        .list_tasks(Some(session_id), Some(&crate::coordination::TaskStatus::InProgress))
        .ok()?;
    tasks
        .iter()
        .filter(|task| task.reporting_session.as_deref() == Some(session_id))
        .find_map(|task| {
            db.list_task_agents(&task.id)
                .ok()?
                .into_iter()
                .find(|m| m.status == "active" && m.removed_at.is_none())
                .map(|m| m.agent_id)
                .filter(|agent| agent != crate::coordination::SNIPPET_AGENT_ID)
        })
}


pub struct ListCoordinationAgents;
#[async_trait]
impl Tool for ListCoordinationAgents {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_coordination_agents".into(),
            description: "The agent directory: every agent's id, role, status, capabilities and a one-line summary of what it is for. Use it to choose whom to message, invite onto a task (invite_task_agent) or run a lane as (delegate_task agent). Only active agents take new work.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let agents = db(ctx)?
            .list_agents()
            .map_err(|e| ToolError::msg(format!("list agents: {e}")))?;
        let root = crate::coordination::agents_root(
            &ctx.mission_control_root()
                .unwrap_or_else(|| crate::config::snippet_home().join("mission-control")),
        );
        let agents: Vec<Value> = agents
            .into_iter()
            .map(|agent| {
                let summary = crate::coordination::AgentHome::new(root.clone(), &agent.id)
                    .ok()
                    .and_then(|home| home.read_identity().ok())
                    .map(|text| identity_summary(&text))
                    .unwrap_or_default();
                let mut row = serde_json::to_value(&agent).unwrap_or(Value::Null);
                if !summary.is_empty() {
                    row["summary"] = json!(summary);
                }
                row
            })
            .collect();
        Ok(ToolResult::success(json!({"agents": agents})))
    }
}


fn identity_summary(identity: &str) -> String {
    let para = identity
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with('#'))
        .unwrap_or("");
    let line = para.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 240 {
        format!("{}…", line.chars().take(239).collect::<String>())
    } else {
        line
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
        let db = db(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &args.thread_id)?;
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
            payload: stamp_origin(ctx, json!({"body": args.body})),
            causation_id: None,
            correlation_id: None,
            idempotency_key: key,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(saved.clone());
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
        let (_, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &thread_id)?;

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
struct SendAgentMessageArgs {
    recipient_agent_id: String,
    body: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// The session id named by the most recent human message in a direct thread.
///
/// Pure selection, split from resolution so the rule is testable on its own: the
/// newest message from the human is the one being answered, so its origin wins.
fn latest_human_origin(events: &[CoordinationEvent]) -> Option<&str> {
    events
        .iter()
        .rev()
        .find(|event| event.actor_kind == "human")
        .and_then(|event| event.payload.get("origin_session"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|session| !session.is_empty())
}

/// The session an agent's reply to `human` should actually land in.
///
/// An agent answers where it was asked. When the last message it received from
/// the human came from a session, that session is where the human is reading —
/// so the reply belongs there, not in the human's own thread. The destination is
/// derived from the thread rather than taken from the model's argument, because a
/// model that names the wrong address loses the answer somewhere the asker is not
/// looking.
fn reply_session_for_agent(db: &Store, agent_id: &str, human_id: &str) -> Option<String> {
    let thread = crate::coordination::direct_thread_id(("agent", agent_id), ("human", human_id));
    let events = db.recent_direct_thread_events(&thread, 10).ok()?;
    let origin = latest_human_origin(&events)?;
    // Only redirect to a session the store actually has: a stale origin would
    // create a delivery that can never land — worse than the human thread.
    db.has_conversation(origin).ok()?.then(|| origin.to_string())
}

/// Send a direct message to a peer agent by id.
///
/// The recipient is an AGENT ID from the directory, never a raw thread id: the
/// thread is derived from the pair, so two agents exchanging messages cannot
/// each invent a different conversation. The message is written durably and the
/// daemon's delivery loop wakes the recipient — a tool call never delivers
/// inline, or a message sent to a stopped session would be lost.
pub struct SendAgentMessage;
#[async_trait]
impl Tool for SendAgentMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "send_agent_message".into(),
            description: "Send a direct message to another agent, or reply where you were asked. When the message you are answering came from a session, reply to `session:<id>` — the exact `reply_to` value from its envelope, copied as written — so the answer lands in the conversation where it was asked. Use `human` only when the message did not name a session. Otherwise address an agent by its agent id (list_coordination_agents shows the directory). It is a conversation, not a command: it never creates a task, transfers ownership, or authorises a workspace change. Task and status messages belong on a task room instead.".into(),
            input_schema: schema(
                json!({
                    "recipient_agent_id":{"type":"string","description":"agent id from list_coordination_agents"},
                    "body":{"type":"string"},
                    "idempotency_key":{"type":"string","description":"optional; a retry with the same key does not duplicate"}
                }),
                &["recipient_agent_id", "body"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SendAgentMessageArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let recipient = args.recipient_agent_id.trim();
        if recipient.is_empty() {
            return Err(ToolError::msg("recipient_agent_id must not be empty"));
        }
        if args.body.trim().is_empty() {
            return Err(ToolError::msg("body must not be empty"));
        }
        // `human` (or `local`) addresses the operator who is talking to this
        // agent. `session:<id>` answers a message in the session it was asked
        // from, which is where the asker is reading. Without these an agent could
        // receive a message and have no way to answer where it matters.
        let (requested_kind, requested_id) = match recipient {
            "human" | "local" => ("human", crate::coordination::LOCAL_HUMAN_ID),
            other => match other.strip_prefix("session:") {
                Some(session) if !session.trim().is_empty() => ("session", session.trim()),
                _ => ("agent", other),
            },
        };
        let db = db(ctx)?;
        // An agent always answers where it was asked. When the message it is
        // answering came from a SESSION, that session is where the human is
        // reading — so a reply addressed to the person is routed there instead of
        // to the human's own thread. Decided here rather than left to the
        // argument: a wrong address loses the answer in a thread nobody watches,
        // which is exactly what the guidance in the description failed to prevent.
        let redirect = if requested_kind == "human" {
            ctx.agent_id()
                .and_then(|agent_id| reply_session_for_agent(&db, agent_id, requested_id))
        } else {
            None
        };
        let (to_kind, to_id): (&str, &str) = match redirect.as_deref() {
            Some(session) => ("session", session),
            None => (requested_kind, requested_id),
        };
        if to_kind == "agent" {
            // A recipient that is not in the directory would sit undelivered
            // forever, so an unknown id is refused rather than accepted and dropped.
            // A paused agent's mail waits; a disabled agent takes none.
            match db
                .get_agent(to_id)
                .map_err(|e| ToolError::msg(format!("look up agent: {e}")))?
            {
                None => {
                    return Err(ToolError::msg(format!(
                        "unknown agent `{to_id}` — list_coordination_agents shows the directory. Use `human` to reply to the person."
                    )));
                }
                Some(agent) if agent.status == crate::coordination::types::AgentStatus::Disabled => {
                    return Err(ToolError::msg(format!("agent `{to_id}` is disabled")));
                }
                Some(_) => {}
            }
        } else if to_kind == "session" {
            // A session id that does not resolve would create a delivery row that
            // can never be delivered, so it is refused at send time rather than
            // retried forever.
            //
            // The message must NOT suggest an alternative address. The previous
            // wording ended with "Use `human` to reply to the person instead",
            // and a model read that as guidance: every later reply went to
            // `human`, so the asker's session never saw one. Point back at the
            // value it was given — that is the correct target — and nothing else.
            let resolves = crate::session::state_path_for_id(to_id)
                .map(|path| crate::session::read_session_meta(&path).is_some())
                .unwrap_or(false);
            if !resolves {
                return Err(ToolError::msg(format!(
                    "unknown session `{to_id}`. Copy the `reply_to` value from the envelope as written, in full: it names the session the sender is reading."
                )));
            }
        }
        let (sender_kind, sender_id) = actor(ctx)?;
        let key = args
            .idempotency_key
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        // The session this message is asked FROM, taken from the context rather
        // than the model's arguments: the model must not be able to claim a
        // different origin any more than it can claim a different sender.
        let origin = ctx.durable_session_id().map(str::to_string);
        let saved = db
            .send_direct_message_from(
                (&sender_kind, &sender_id),
                (to_kind, to_id),
                args.body.trim(),
                &key,
                &chrono::Utc::now().to_rfc3339(),
                origin.as_deref(),
            )
            .map_err(|e| ToolError::msg(format!("send message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved,
        }));
        Ok(ToolResult::success(json!({
            "thread_id": saved.thread_id,
            "sequence": saved.sequence,
            "to": format!("{to_kind}:{to_id}"),
            "note": "Accepted and queued for delivery. This is conversation only — it does not start work.",
        })))
    }
}

#[derive(Deserialize)]
struct ReadAgentThreadArgs {
    peer_agent_id: String,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Read the direct conversation between this session's actor and one peer.
pub struct ReadAgentThread;
#[async_trait]
impl Tool for ReadAgentThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_agent_thread".into(),
            description: "Read your direct conversation with one agent, oldest first. Omit after_sequence for the most recent messages; pass it to page forward. The wake message already includes the recent history, so use this only to see more.".into(),
            input_schema: schema(
                json!({
                    "peer_agent_id":{"type":"string"},
                    "after_sequence":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &["peer_agent_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadAgentThreadArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let peer = args.peer_agent_id.trim();
        if peer.is_empty() {
            return Err(ToolError::msg("peer_agent_id must not be empty"));
        }
        let (kind, id) = actor(ctx)?;
        let thread_id =
            crate::coordination::direct_thread_id((&kind, &id), ("agent", peer));
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let db = db(ctx)?;
        let events = match args.after_sequence {
            Some(after) => db
                .direct_thread_events(&thread_id, after, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
            None => db
                .recent_direct_thread_events(&thread_id, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
        };
        Ok(ToolResult::success(json!({
            "thread_id": thread_id,
            "peer": peer,
            "messages": events.iter().map(flatten_event).collect::<Vec<_>>(),
            "oldest_sequence": events.first().map(|e| e.sequence).unwrap_or(0),
            "newest_sequence": events.last().map(|e| e.sequence).unwrap_or(0),
        })))
    }
}

/// The conversations this actor is part of, with unread counts — the inbox.
pub struct ReadAgentInbox;
#[async_trait]
impl Tool for ReadAgentInbox {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_agent_inbox".into(),
            description: "List your direct conversations with unread counts, most recent first. Use it to find messages you have not answered; read one with read_agent_thread.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let (kind, id) = actor(ctx)?;
        let threads = db(ctx)?
            .list_direct_threads(&kind, &id)
            .map_err(|e| ToolError::msg(format!("list inbox: {e}")))?;
        Ok(ToolResult::success(json!({
            "actor": format!("{kind}:{id}"),
            "threads": threads,
        })))
    }
}

/// Read back this agent's own coordination memory.
///
/// The point of the board is recall: before dispatching, an agent asks "have I
/// sent work about this workspace before, and how did it go". Filtering by
/// workspace and searching the summaries are what make that a query instead of
/// a re-read of every transcript.
pub struct ReadCoordinationBoard;
#[async_trait]
impl Tool for ReadCoordinationBoard {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_coordination_board".into(),
            description: "Read your own coordination memory: work you dispatched, what came back, and notes you recorded. Newest first. Filter with workspace to scope to one folder, contains to search the summaries, and kind to narrow to dispatched/reported/noted. Use it before dispatching to see whether you have done something similar, and what worked. Set outstanding_only to list dispatches that have not reported back yet.".into(),
            input_schema: schema(
                json!({
                    "workspace":{"type":"string","description":"only rows about this folder"},
                    "contains":{"type":"string","description":"substring to find in summaries"},
                    "kind":{"type":"string","enum":["dispatched","reported","noted"]},
                    "outstanding_only":{"type":"boolean","description":"only dispatches with no report yet"},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            workspace: Option<String>,
            #[serde(default)]
            contains: Option<String>,
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            outstanding_only: bool,
            #[serde(default)]
            limit: Option<u32>,
        }
        let args: Args =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let agent_id = board_agent_id(ctx)?;
        let db = db(ctx)?;
        if args.outstanding_only {
            let outstanding = db
                .board_awaiting_report(&agent_id)
                .map_err(|e| ToolError::msg(format!("read board: {e}")))?;
            return Ok(ToolResult::success(json!({
                "agent_id": agent_id,
                "outstanding": outstanding,
                "note": "Dispatches with no report yet. Read a specific one by searching its correlation id.",
            })));
        }
        let kind = match args.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            None => None,
            Some(raw) => Some(
                crate::coordination::BoardEntryKind::parse(raw).ok_or_else(|| {
                    ToolError::msg(format!(
                        "unknown kind `{raw}` — use dispatched, reported, or noted"
                    ))
                })?,
            ),
        };
        let query = crate::coordination::BoardQuery {
            workspace: args.workspace.as_deref().filter(|w| !w.trim().is_empty()),
            contains: args.contains.as_deref().filter(|c| !c.trim().is_empty()),
            kind,
        };
        let entries = db
            .read_board(&agent_id, &query, args.limit.unwrap_or(30).clamp(1, 100))
            .map_err(|e| ToolError::msg(format!("read board: {e}")))?;
        Ok(ToolResult::success(json!({
            "agent_id": agent_id,
            "count": entries.len(),
            "entries": entries,
        })))
    }
}

#[derive(Deserialize)]
struct NoteArgs {
    summary: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
}

/// Record a note on this agent's board.
///
/// For conclusions the mechanical record cannot capture — which agent suited a
/// kind of work, what a workspace's conventions are, why an approach failed.
/// Dispatches and reports are recorded automatically; this is the agent's own
/// judgment, which is exactly the part worth keeping.
pub struct RecordCoordinationNote;
#[async_trait]
impl Tool for RecordCoordinationNote {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "record_coordination_note".into(),
            description: "Write a note to your own coordination memory, so a later turn can recall it. Use it for conclusions worth keeping: what a workspace needs, which agent fits a kind of work, why an approach failed. Keep it to one or two sentences — it is a memory row, not a report. Dispatches and their reports are recorded for you; do not duplicate those.".into(),
            input_schema: schema(
                json!({
                    "summary":{"type":"string","description":"one or two sentences"},
                    "workspace":{"type":"string","description":"the folder it concerns, when it concerns one"},
                    "session_id":{"type":"string","description":"the session it concerns, when it concerns one"},
                    "correlation_id":{"type":"string","description":"an assignment or goal id to link it to"}
                }),
                &["summary"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: NoteArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let summary = args.summary.trim();
        if summary.is_empty() {
            return Err(ToolError::msg("summary must not be empty"));
        }
        let agent_id = board_agent_id(ctx)?;
        let id = db(ctx)?
            .record_board_entry(
                &agent_id,
                crate::coordination::BoardEntryKind::Noted,
                &crate::coordination::NewBoardEntry {
                    session_id: args.session_id.as_deref().filter(|s| !s.trim().is_empty()),
                    workspace: args.workspace.as_deref().filter(|w| !w.trim().is_empty()),
                    summary,
                    correlation_id: args
                        .correlation_id
                        .as_deref()
                        .filter(|c| !c.trim().is_empty()),
                    created_at: &now_rfc3339(),
                },
            )
            .map_err(|e| ToolError::msg(format!("record note: {e}")))?;
        Ok(ToolResult::success(json!({"id": id, "recorded": true})))
    }
}

/// The agent identity a board row belongs to.
///
/// The board is PER AGENT, so a session with no agent identity has no board —
/// this is the same rule the turn lease uses, and for the same reason: a session
/// is not an agent.
fn board_agent_id(ctx: &ToolContext) -> Result<String, ToolError> {
    if let Ok((kind, id)) = actor(ctx) {
        if kind == "agent" {
            return Ok(id);
        }
    }
    ctx.agent_id().map(str::to_string).ok_or_else(|| {
        ToolError::msg(
            "this session is not working as an agent — coordination memory requires an agent identity",
        )
    })
}
#[derive(Deserialize)]
struct MessageMissionControlArgs {
    task_id: String,
    message: String,
}

pub struct MessageMissionControl;
#[async_trait]
impl Tool for MessageMissionControl {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "message_mission_control".into(),
            description: "Message Mission Control using the active task board item you are working on. This posts to the task thread, notifies Mission Control, and records the update on the task board so your progress or question is linked to the task.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID you are working on"},
                    "message": {"type": "string", "description": "Message, question, or status update for Mission Control"}
                }),
                &["task_id", "message"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: MessageMissionControlArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let message = args.message.trim();
        if task_id.is_empty() || message.is_empty() {
            return Err(ToolError::msg("task_id and message must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let (actor_kind, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &task.thread_id)?;
        let now = now_rfc3339();
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.message".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(ctx, json!({
                "body": message,
                "task_id": task_id,
                "to": "mission_control",
            })),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now.clone(),
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        let _ = db.update_task_in(task_id, &now, |t| {
            t.notifications.push(NotificationMarker {
                target: "mission_control".into(),
                kind: "message".into(),
                message: format!("{actor_id}: {message}"),
                delivered: false,
            });
        });
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(saved.clone());
        Ok(ToolResult::success(json!({
            "sent": true,
            "task_id": task_id,
            "event": saved,
        })))
    }
}

#[derive(Deserialize)]
struct PostTaskCoordinationArgs {
    task_id: String,
    body: String,
}

pub struct PostTaskCoordination;
#[async_trait]
impl Tool for PostTaskCoordination {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "post_task_coordination".into(),
            description: "Coordinate with fellow agents assigned to this task board item. Posts context, plan proposals, or status to the task thread. Everyone on the task and Mission Control see it; a post that needs nothing from them can simply be read.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID on the task board"},
                    "body": {"type": "string", "description": "The coordination message or update"}
                }),
                &["task_id", "body"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PostTaskCoordinationArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let body = args.body.trim();
        if task_id.is_empty() || body.is_empty() {
            return Err(ToolError::msg("task_id and body must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let (actor_kind, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &task.thread_id)?;
        let now = now_rfc3339();
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: actor_kind.to_string(),
            actor_id,
            payload_version: 1,
            payload: stamp_origin(ctx, json!({"body": body, "task_id": task_id})),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(saved.clone());
        Ok(ToolResult::success(json!({"event": saved})))
    }
}

#[derive(Deserialize)]
struct TransferTaskSessionLeaseArgs {
    task_id: String,
    to_agent_id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    handoff_context: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
}

pub struct TransferTaskSessionLease;
#[async_trait]
impl Tool for TransferTaskSessionLease {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "transfer_task_session_lease".into(),
            description: "Transfer the active workspace session lease for a task to another assigned agent (or back to snippet). Only ONE agent can work in the project session at a time; the others are waiting. Provide reason, handoff_context, and artifacts to give the next agent complete context of what you did and what they should do.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string"},
                    "to_agent_id": {"type": "string", "description": "Agent to hand lease to, e.g. an assigned agent or 'snippet'"},
                    "reason": {"type": "string", "description": "High-level reason for the handoff / phase transition"},
                    "handoff_context": {"type": "string", "description": "Detailed briefing and guidance for the incoming agent"},
                    "artifacts": {"type": "array", "items": {"type": "string"}, "description": "List of files or paths modified or created in this phase"}
                }),
                &["task_id", "to_agent_id"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: TransferTaskSessionLeaseArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let to_agent = args.to_agent_id.trim();
        if task_id.is_empty() || to_agent.is_empty() {
            return Err(ToolError::msg("task_id and to_agent_id must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let (_, actor_id) = actor(ctx)?;
        crate::mission_tools::require_working_agent(&db, to_agent)?;
        let now = now_rfc3339();

        let roster = db.list_task_agents(task_id).unwrap_or_default();
        let current_active = roster
            .iter()
            .find(|m| m.status == "active" && m.removed_at.is_none())
            .map(|m| m.agent_id.as_str())
            .unwrap_or(&actor_id);

        if !roster.iter().any(|m| m.agent_id == to_agent && m.removed_at.is_none()) {
            db.add_task_agent_full(task_id, to_agent, "collaborator", None, "", "waiting", &now)
                .map_err(|e| ToolError::msg(format!("add agent to roster: {e}")))?;
        }

        db.transfer_task_session_lease(task_id, current_active, to_agent)
            .map_err(|e| ToolError::msg(format!("transfer lease: {e}")))?;

        let reason_str = args.reason.as_deref().unwrap_or("Handoff to next agent");
        let mut body = format!("Session lease transferred from {current_active} to {to_agent}: {reason_str}");
        if let Some(ref h_ctx) = args.handoff_context {
            if !h_ctx.trim().is_empty() {
                body.push_str(&format!("\nHandoff Briefing:\n{}", h_ctx.trim()));
            }
        }
        if !args.artifacts.is_empty() {
            body.push_str(&format!("\nArtifacts / Modified Files:\n{}", args.artifacts.join(", ")));
        }

        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.lease_transferred".into(),
            actor_kind: "agent".into(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(ctx, json!({
                "body": body,
                "task_id": task_id,
                "from_agent_id": current_active,
                "to_agent_id": to_agent,
                "reason": reason_str,
                "handoff_context": args.handoff_context,
                "artifacts": args.artifacts,
            })),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("record event: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(saved.clone());
        Ok(ToolResult::success(json!({
            "transferred": true,
            "task_id": task_id,
            "active_agent": to_agent,
            "event": saved,
        })))
    }
}

#[derive(Deserialize)]
struct InspectTaskArgs {
    task_id: String,
}

pub struct InspectTask;
#[async_trait]
impl Tool for InspectTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "inspect_task".into(),
            description: "Inspect the full details of a task: its plan, description, roster of assigned agents with active lease holder, linked blockers, owned paths, and result. Use this whenever you need full visibility and context for a task.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID to inspect"}
                }),
                &["task_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: InspectTaskArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return Err(ToolError::msg("task_id must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let roster = db.list_task_agents(task_id).unwrap_or_default();
        let blockers = db.blockers_of(task_id).unwrap_or_default();
        let links = db.task_links(task_id).unwrap_or_default();

        Ok(ToolResult::success(json!({
            "task_id": task.id,
            "title": task.title,
            "description": task.description,
            "status": task.status,
            "plan": task.plan,
            "profile": task.profile,
            "session_id": task.session_id,
            "owned_paths": task.owned_paths,
            "roster": roster,
            "blockers": blockers,
            "links": links,
            "thread_id": task.thread_id,
            "result": task.result,
        })))
    }
}

#[derive(Deserialize)]
struct ClaimAndDispatchArgs {
    task_id: String,
    #[serde(default)]
    profile: Option<String>,
}

pub struct ClaimAndDispatchTask;
#[async_trait]
impl Tool for ClaimAndDispatchTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "claim_and_dispatch_task".into(),
            description: "Take the lease on a queued or blocked task and queue it for dispatch into its target session; the daemon delivers it within seconds, with your identity. Refuses finished work and work already running. Specify `profile` to select an inference profile, or omit it to keep the session's model and its prompt cache.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID to claim"},
                    "profile": {"type": "string", "description": "Optional inference profile name. Omit to preserve prompt cache affinity."}
                }),
                &["task_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ClaimAndDispatchArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return Err(ToolError::msg("task_id must not be empty"));
        }
        let profile = args
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        if let Some(name) = profile.as_deref() {
            crate::mission_tools::check_profile(name).await?;
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        if task.status.is_terminal() || task.status == crate::coordination::TaskStatus::InProgress {
            return Err(ToolError::msg(format!("task `{task_id}` is {}; only queued or blocked work can be claimed", task.status)));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        if actor_kind != "agent" {
            return Err(ToolError::msg("claiming a task needs an agent identity; this session has none"));
        }
        if actor_id == crate::mission_control::SESSION_ID {
            return Err(ToolError::msg(
                "Mission Control routes work and never takes a task's lease. To send a blocked or failed task to its session again, use retry_mission_task (pass profile to switch the worker's model).",
            ));
        }
        crate::mission_tools::require_working_agent(&db, &actor_id)?;
        let now = now_rfc3339();
        db.add_task_agent_full(task_id, &actor_id, "implementer", None, "", "active", &now)
            .map_err(|e| ToolError::msg(format!("take the lease: {e}")))?;
        db.retry_task(task_id, &now)
            .map_err(|e| ToolError::msg(format!("queue task: {e}")))?;
        let queued = db
            .update_task_in(task_id, &now, |t| {
                if profile.is_some() {
                    t.profile = profile.clone();
                }
            })
            .map_err(|e| ToolError::msg(format!("queue task: {e}")))?;

        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.dispatched".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(ctx, json!({
                "body": format!("{actor_id} claimed the task"),
                "task_id": task_id,
                "agent_id": actor_id,
                "profile": queued.profile,
            })),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let _ = db.append_event(&event);
        let _ = db.record_board_entry(
            &actor_id,
            crate::coordination::BoardEntryKind::Dispatched,
            &crate::coordination::NewBoardEntry {
                session_id: Some(&queued.session_id),
                workspace: None,
                summary: &format!("{task_id} — claimed: {}", task.title),
                correlation_id: Some(task_id),
                created_at: &event.created_at,
            },
        );

        Ok(ToolResult::success(json!({
            "queued": true,
            "task_id": task_id,
            "session_id": queued.session_id,
            "profile": queued.profile,
            "active_agent": actor_id,
            "note": "Lease taken; the daemon dispatches the task to its session within seconds.",
        })))
    }
}

#[derive(Deserialize)]
struct DeclineTaskArgs {
    task_id: String,
    reason: String,
}

pub struct DeclineTask;

#[async_trait]
impl Tool for DeclineTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "decline_task".into(),
            description: "Turn down a task you were offered, with your reason: it isn't your kind of work, the target session is wrong, the briefing can't be done as written. You leave the roster, the task waits as blocked, and Mission Control is told why so it can reassign or reshape it. Ask Mission Control first instead when a question would settle it.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The full task id from the offer."},
                    "reason": {"type": "string", "description": "Why, and what would make it work (another agent, session or scope)."}
                }),
                &["task_id", "reason"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: DeclineTaskArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let reason = args.reason.trim();
        if task_id.is_empty() || reason.is_empty() {
            return Err(ToolError::msg("task_id and reason must not be empty"));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        if actor_kind != "agent" {
            return Err(ToolError::msg("declining a task needs an agent identity; this session has none"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        if task.status.is_terminal() || task.status == crate::coordination::TaskStatus::InProgress {
            return Err(ToolError::msg(format!(
                "task `{task_id}` is {}; only work that hasn't started can be declined",
                task.status
            )));
        }
        let now = now_rfc3339();
        db.remove_task_agent(task_id, &actor_id, &now)
            .map_err(|e| ToolError::msg(format!("leave the roster: {e}")))?;
        let message = format!("{actor_id} declined the task: {reason}");
        db.move_task(task_id, crate::coordination::TaskStatus::Blocked, &message, &now)
            .map_err(|e| ToolError::msg(format!("park task: {e}")))?;
        let marker = crate::coordination::offer_marker(&actor_id);
        db.update_task_in(task_id, &now, |t| {
            t.notifications.retain(|n| n.target != marker);
            t.notify_once("declined", &message);
        })
            .map_err(|e| ToolError::msg(format!("notify Mission Control: {e}")))?;
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.declined".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(ctx, json!({"body": message, "task_id": task_id, "agent_id": actor_id})),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let _ = db.append_event(&event);
        Ok(ToolResult::success(json!({
            "declined": true,
            "task_id": task_id,
            "note": "Mission Control has been told why; the task waits as blocked until it is reassigned or reshaped.",
        })))
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}


/// Record the session an event was posted from, so the room's wake-up skips it.
pub(crate) fn stamp_origin(ctx: &ToolContext, mut payload: Value) -> Value {
    if let Some(session) = ctx.durable_session_id() {
        payload["origin_session"] = json!(session);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentRole, AgentStatus};

    fn context(root: &std::path::Path, session: &str) -> ToolContext {
        ToolContext::new(root)
            .unwrap()
            .with_durable_session_id(session)
            .with_store_path(root.join("snippet.db"))
    }

    fn migrate(root: &std::path::Path) -> Store {
        Store::open(root.join("snippet.db")).unwrap()
    }

    /// A session cannot read or post in a room it has no place in, such as
    /// another task's room or a direct thread between two others.
    #[tokio::test]
    async fn rooms_are_closed_to_outsiders() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_task(&crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Private".into(),
            "work".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        let room = crate::coordination::Task::thread_for("t1");

        let outsider = context(dir.path(), "work-2");
        assert!(PostCoordinationMessage
            .execute(&outsider, json!({"thread_id": room, "body": "hi"}))
            .await
            .is_err());
        assert!(ReadCoordinationThread
            .execute(&outsider, json!({"thread_id": room}))
            .await
            .is_err());
        assert!(PostCoordinationMessage
            .execute(&outsider, json!({"thread_id": "made-up", "body": "hi"}))
            .await
            .is_err());

        // The task's own session and Mission Control are let in.
        let worker = context(dir.path(), "work-1");
        PostCoordinationMessage
            .execute(&worker, json!({"thread_id": room, "body": "progress"}))
            .await
            .unwrap();
        let mc = context(dir.path(), "mc").with_agent_id("mission-control");
        ReadCoordinationThread
            .execute(&mc, json!({"thread_id": room}))
            .await
            .unwrap();
    }

    /// Claiming takes the lease (demoting the holder) and queues the task for
    /// the dispatcher; it used to mark the task running and deliver nothing.
    #[tokio::test]
    async fn claiming_takes_the_lease_and_queues_for_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["snippet", "reviewer"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                role: AgentRole::Reviewer,
                capabilities: vec![],
            })
            .unwrap();
        }
        let mut task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Review".into(),
            "look".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = crate::coordination::TaskStatus::Blocked;
        db.create_task(&task).unwrap();
        db.add_task_agent_full("t1", "snippet", "implementer", None, "", "active", "2026-01-01T00:00:00Z")
            .unwrap();

        let ctx = context(dir.path(), "inbox-reviewer").with_agent_id("reviewer");
        ClaimAndDispatchTask
            .execute(&ctx, json!({"task_id": "t1"}))
            .await
            .unwrap();

        let task = db.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, crate::coordination::TaskStatus::Todo);
        let roster = db.list_task_agents("t1").unwrap();
        let status = |id: &str| roster.iter().find(|m| m.agent_id == id).unwrap().status.clone();
        assert_eq!(status("reviewer"), "active");
        assert_eq!(status("snippet"), "waiting", "one lease holder at a time");

        db.claim_task_for_dispatch("t1", "2026-01-01T00:00:01Z").unwrap();
        assert!(ClaimAndDispatchTask.execute(&ctx, json!({"task_id": "t1"})).await.is_err());
    }

    /// A lease moves an agent into a session it does not own; posts from that
    /// session are the lease holder's, not the session owner's.
    #[tokio::test]
    async fn posts_are_attributed_to_the_lease_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "reviewer".into(),
            display_name: "Reviewer".into(),
            handle: "reviewer".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Reviewer,
            capabilities: vec![],
        })
        .unwrap();
        let mut task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Review".into(),
            "look".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = crate::coordination::TaskStatus::InProgress;
        task.reporting_session = Some("s1".into());
        db.create_task(&task).unwrap();
        db.add_task_agent_full("t1", "reviewer", "reviewer", None, "", "active", "2026-01-01T00:00:00Z")
            .unwrap();

        let ctx = context(dir.path(), "s1").with_agent_id("owner");
        let posted = PostTaskCoordination
            .execute(&ctx, json!({"task_id": "t1", "body": "looks good"}))
            .await
            .unwrap();
        assert_eq!(posted.value["data"]["event"]["actor_id"], "reviewer");
        assert_eq!(posted.value["data"]["event"]["payload"]["origin_session"], "s1");
    }

    #[tokio::test]
    async fn post_message_is_attributed_to_the_session_not_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");

        let result = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"system","body":"starting"}))
            .await
            .unwrap();

        let event_id = result.value["data"]["event"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        let events = db.events_for_thread("system", 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, event_id);
        // Identity came from the session, not from caller arguments. This session
        // has no agent bound, so it posts as the session — not as an "agent",
        // which is exactly the conflation this split removes.
        assert_eq!(events[0].actor_id, "mission-control");
        assert_eq!(events[0].actor_kind, "session");
    }


    #[tokio::test]
    async fn post_message_posts_as_the_bound_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "s1").with_agent_id("rust-pr-reviewer");

        PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"system","body":"reviewing"}))
            .await
            .unwrap();

        let events = db.events_for_thread("system", 0, 10).unwrap();
        // An agent-bound session posts as the AGENT, so the board names the
        // identity rather than the address it happens to run in.
        assert_eq!(events[0].actor_id, "rust-pr-reviewer");
        assert_eq!(events[0].actor_kind, "agent");
    }


    #[tokio::test]
    async fn post_message_requires_a_session_identity() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = ToolContext::new(dir.path()).unwrap();
        let error = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"t1","body":"hi"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("session identity"));
    }


    #[tokio::test]
    async fn post_message_is_idempotent_for_a_repeated_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        let args = json!({"thread_id":"system","body":"once","idempotency_key":"k1"});

        PostCoordinationMessage
            .execute(&ctx, args.clone())
            .await
            .unwrap();
        PostCoordinationMessage.execute(&ctx, args).await.unwrap();

        assert_eq!(db.events_for_thread("system", 0, 10).unwrap().len(), 1);
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


    /// The separation itself: a session is not an agent, so a session with no
    /// agent bound cannot take a turn. Before this split the session id stood in
    /// as the identity, which made every session an agent.


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
                .execute(&ctx, json!({"thread_id": "system", "body": body}))
                .await
                .unwrap();
        }

        // Recent window, then page further back using the returned oldest.
        let recent = ReadCoordinationThread
            .execute(&ctx, json!({"thread_id": "system", "limit": 2}))
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
            .execute(&ctx, json!({"thread_id": "system", "after_sequence": 2}))
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


    fn direct_event(actor_kind: &str, origin: Option<&str>) -> CoordinationEvent {
        let mut payload = json!({"body": "x", "recipient": "agent:snippet"});
        if let Some(origin) = origin {
            payload["origin_session"] = json!(origin);
        }
        CoordinationEvent {
            event_id: format!("e-{actor_kind}-{origin:?}"),
            thread_id: "direct:agent:snippet|human:local".into(),
            partition_key: "thread:x".into(),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: actor_kind.into(),
            actor_id: if actor_kind == "human" {
                "local".into()
            } else {
                "snippet".into()
            },
            payload_version: 1,
            payload,
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn the_newest_human_message_decides_where_a_reply_goes() {
        // The agent's own earlier reply sits between the two questions; it must
        // not be mistaken for the question being answered now.
        let events = vec![
            direct_event("human", Some("ws-a")),
            direct_event("agent", None),
            direct_event("human", Some("ws-b")),
        ];
        assert_eq!(latest_human_origin(&events), Some("ws-b"));
    }

    #[test]
    fn a_human_message_without_a_session_does_not_redirect() {
        let events = vec![direct_event("human", None)];
        assert_eq!(latest_human_origin(&events), None);
    }

    #[tokio::test]
    async fn a_reply_to_the_human_is_routed_to_the_session_that_asked() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let state = crate::harness::HarnessState::blank("/tmp/ws", Some("asking".into()));
        db.import_session(
            "ws-a",
            "key",
            &state,
            &crate::conversations::SessionExtras::default(),
        )
        .unwrap();
        db.send_direct_message_from(
            ("human", "local"),
            ("agent", "snippet"),
            "did the dispatch land?",
            "k1",
            "2026-01-01T00:00:00Z",
            Some("ws-a"),
        )
        .unwrap();

        assert_eq!(
            reply_session_for_agent(&db, "snippet", "local").as_deref(),
            Some("ws-a"),
            "a reply addressed to the person belongs in the session that asked"
        );
    }

    #[tokio::test]
    async fn a_reply_is_not_routed_to_a_session_the_store_does_not_have() {
        // A stale origin must fall back to the human thread: redirecting into a
        // session that no longer exists is a delivery that can never land.
        let db = Store::open_in_memory().unwrap();
        db.send_direct_message_from(
            ("human", "local"),
            ("agent", "snippet"),
            "hello",
            "k1",
            "2026-01-01T00:00:00Z",
            Some("gone"),
        )
        .unwrap();

        assert_eq!(reply_session_for_agent(&db, "snippet", "local"), None);
    }

    #[tokio::test]
    async fn task_coordination_and_lease_handoff_flow() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "agent-a".into(),
            display_name: "Agent A".into(),
            handle: "agent_a".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Implementer,
            capabilities: vec![],
        })
        .unwrap();
        db.create_agent(&Agent {
            id: "agent-b".into(),
            display_name: "Agent B".into(),
            handle: "agent_b".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            role: AgentRole::Reviewer,
            capabilities: vec![],
        })
        .unwrap();

        let task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Title".into(),
            "Desc".into(),
            "sess-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        db.create_task(&task).unwrap();
        db.add_task_agent_full("t1", "agent-a", "implementer", None, "", "active", "2026-01-01T00:00:00Z").unwrap();
        db.add_task_agent_full("t1", "agent-b", "reviewer", None, "", "waiting", "2026-01-01T00:00:00Z").unwrap();

        let ctx_a = ToolContext::new(dir.path())
            .unwrap()
            .with_store_path(dir.path().join("snippet.db"))
            .with_durable_session_id("sess-1")
            .with_agent_id("agent-a");

        let res = MessageMissionControl
            .execute(&ctx_a, json!({"task_id": "t1", "message": "Need clarification on auth"}))
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        let t = db.get_task("t1").unwrap().unwrap();
        assert_eq!(t.notifications.len(), 1);
        assert_eq!(t.notifications[0].target, "mission_control");
        assert!(t.notifications[0].message.contains("Need clarification on auth"));

        let res = PostTaskCoordination
            .execute(&ctx_a, json!({"task_id": "t1", "body": "Auth implementation ready for review"}))
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        let res = TransferTaskSessionLease
            .execute(&ctx_a, json!({
                "task_id": "t1",
                "to_agent_id": "agent-b",
                "reason": "Ready for PR review",
                "handoff_context": "Implemented OAuth2 token refresh in src/auth.rs",
                "artifacts": ["src/auth.rs", "tests/auth_test.rs"]
            }))
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        let agents = db.list_task_agents("t1").unwrap();
        let a = agents.iter().find(|m| m.agent_id == "agent-a").unwrap();
        let b = agents.iter().find(|m| m.agent_id == "agent-b").unwrap();
        assert_eq!(a.status, "waiting");
        assert_eq!(b.status, "active");

        // Verify InspectTask provides maximum visibility
        let ctx_b = ToolContext::new(dir.path())
            .unwrap()
            .with_store_path(dir.path().join("snippet.db"))
            .with_durable_session_id("sess-1")
            .with_agent_id("agent-b");

        let inspect_res = InspectTask
            .execute(&ctx_b, json!({"task_id": "t1"}))
            .await
            .unwrap();
        assert_eq!(inspect_res.value["status"], "success");
        let val = &inspect_res.value["data"];
        assert_eq!(val["task_id"], "t1");
        assert_eq!(val["title"], "Title");
        assert_eq!(val["roster"].as_array().unwrap().len(), 2);
    }
}
