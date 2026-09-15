//! The single seam between a frontend and the agent: build the model + tools +
//! harness for a config and spawn the resident `run_interactive` loop. Drive it by
//! sending `LoopInput` on `input_tx`; observe it via the persisted `HarnessState`
//! (and, optionally, a live `StreamHandle`). Shared by the TUI and headless `serve`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::builtins::coding_tools;
use crate::config::{InferenceProfileConfig, SnippetConfig, workspaces_root};
use crate::coordination::{AgentHome, IdentityError};
use crate::harness::{CodingHarness, HarnessConfig, HarnessState, LoopInput};
use crate::lanes::ModelFactory;
use crate::llm::StreamHandle;
use crate::prompts::{conversation_prompt, mission_control_system_prompt};
use crate::tools::{BrowserSummaryProvider, ToolContext, ToolError, ToolRegistry};

pub struct SessionHandle {
    pub input_tx: mpsc::UnboundedSender<LoopInput>,
    pub join: tokio::task::JoinHandle<Result<HarnessState, String>>,
    pub state_path: PathBuf,
    /// Live token stream shared with attached UIs (TUI / mobile via WS).
    pub stream: Option<StreamHandle>,
}

/// Spawn a resident conversation session for `config`, persisting to `state_path`.
/// `stream` carries live text deltas to a UI sink; pass `None` for headless callers
/// that only read committed `HarnessState`.
pub fn start_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
) -> SessionHandle {
    start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        None,
        SessionConfig::standard(),
    )
}

/// Spawn a durable session worked by a directory agent. It keeps the same
/// predefined session contract as a normal conversation, then appends the
/// researched, versioned identity stored in the agent home.
///
/// The session's KIND is unchanged — this is still a standard session. The
/// agent is who is working it, not what it is.
pub fn start_specialized_agent_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
    agent_home: AgentHome,
) -> Result<SessionHandle, IdentityError> {
    let identity = agent_home.read_identity()?;
    Ok(start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        SessionConfig::for_agent(AgentBinding {
            agent_id: agent_home.agent_id().to_string(),
            identity,
        }),
    ))
}

/// Spawn an agent's COORDINATION session: it answers direct messages and
/// dispatches work, and has no workspace tools.
///
/// Separate from [`start_specialized_agent_session`] because the two differ in
/// TOOL SET and prompt, not just identity — the same agent runs a full coding
/// session when it is working and this restricted one in its inbox.
pub fn start_specialized_coordination_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
    agent_home: AgentHome,
) -> Result<SessionHandle, IdentityError> {
    let identity = agent_home.read_identity()?;
    Ok(start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        SessionConfig::coordination(AgentBinding {
            agent_id: agent_home.agent_id().to_string(),
            identity,
        }),
    ))
}

pub fn start_mission_control_session(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
) -> SessionHandle {
    start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        SessionConfig::mission_control(),
    )
}

pub fn start_session_with_browser_summary(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
) -> SessionHandle {
    start_session_with_role(
        config,
        state_path,
        initial,
        resume,
        stream,
        browser_summary,
        SessionConfig::standard(),
    )
}

/// The two kinds of session. Not three.
///
/// An AGENT is a separate entity — who is working — not a kind of session. A
/// session is either Mission Control or it is not; a plain conversation and one
/// worked by a specialist are the SAME kind, differing only by their agent. The
/// previous three-variant enum fused the two axes, so "which agent" could only
/// be expressed by claiming a different kind of session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    MissionControl,
    Standard,
    /// An agent's coordination session: it answers direct messages and dispatches
    /// work, and deliberately has no workspace tools.
    Coordination,
}

/// The directory agent working this session, if any.
///
/// `identity` is loaded for the runtime; it is NOT persisted (see
/// [`SessionSidecar`]) because a resume must pick up the agent's CURRENT
/// researched identity, not the one captured when the session first started.
struct AgentBinding {
    agent_id: String,
    identity: String,
}

/// How a session runs: its kind, and optionally the agent working it.
///
/// The two travel together — persisted in one sidecar, resolved in one step —
/// but they are independent: an agent can be rebound without the session
/// changing kind.
struct SessionConfig {
    role: SessionRole,
    agent: Option<AgentBinding>,
}

impl SessionConfig {
    fn standard() -> Self {
        Self {
            role: SessionRole::Standard,
            agent: None,
        }
    }

    fn mission_control() -> Self {
        Self {
            role: SessionRole::MissionControl,
            agent: None,
        }
    }

    fn for_agent(agent: AgentBinding) -> Self {
        Self {
            role: SessionRole::Standard,
            agent: Some(agent),
        }
    }

    /// An agent's coordination session. Requires the agent: the board it writes
    /// to is per agent, so a coordination session with no identity has nowhere to
    /// record anything.
    fn coordination(agent: AgentBinding) -> Self {
        Self {
            role: SessionRole::Coordination,
            agent: Some(agent),
        }
    }
}

/// The persisted half of a [`SessionConfig`]: the kind, and the agent's ID.
///
/// `agent_id` alone is enough. The identity text is re-read from the agent home
/// on every resume, so an agent whose guidance was updated comes back with the
/// new guidance instead of a stale copy frozen here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSidecar {
    pub role: SessionRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Persist a session's kind and agent. THE one writer.
///
/// Takes the persisted shape rather than a full [`SessionConfig`] on purpose:
/// the runtime carries an identity the sidecar must NOT hold (it is re-read from
/// the agent home on resume), so accepting a config here would invite writing a
/// stale copy of it.
///
/// Writes to the STORE when the session has a row. [`read_session_sidecar`] is
/// store-first, so a file-only write here would be shadowed by the row's older
/// values and the change would appear to do nothing.
pub fn write_session_sidecar(state_path: &Path, sidecar: &SessionSidecar) {
    if let Some(store) = store_for_sessions() {
        let id = session_id_for_state_path(state_path);
        let _ = store.set_session_role(&id, role_str(sidecar.role), sidecar.agent_id.as_deref());
    }
}

/// Read a session's persisted kind and agent. THE one parser.
///
/// Every reader goes through here — `start_role_aware` and `read_session` both
/// used to parse this file independently, and they disagreed about the grammar,
/// so a role one of them understood was invisible to the other. Nothing outside
/// this function should know the sidecar's name or shape.
///
/// Store first: a migrated session has no `.role`, so reading only the file would
/// report every session as a plain standard session — the routing agent would
/// then treat Mission Control as ordinary work.
pub fn read_session_sidecar(state_path: &Path) -> Option<SessionSidecar> {
    let row = store_session_row(state_path)?;
    let role = if row.role == "mission_control" {
        SessionRole::MissionControl
    } else {
        SessionRole::Standard
    };
    Some(SessionSidecar {
        role,
        agent_id: row.agent_id,
    })
}

/// Everything that distinguishes one kind of session from another, resolved once.
///
/// Replaces a single `mission_control: bool` that drove five separate branches
/// in `start_session_with_role` — model factory, tool context, tool registry,
/// system prompt, and lane control. Adding a kind meant finding all five, and
/// nothing said when one was missed: the two places that read a role back off
/// disk drifted, so a specialized agent was silently rebuilt as an ordinary
/// session on a config reload.
///
/// There are two RUNTIMES, not three. `Standard` and `Specialized` differ only
/// by an identity overlay appended to the SAME prompt and the SAME tool set,
/// which is what `specialized_agent_system_prompt` already did.
struct AgentRuntime {
    factory: Option<ModelFactory>,
    context: ToolContext,
    tools: ToolRegistry,
    prompt: String,
    /// Mission Control coordinates durable sessions; it does not spawn lanes.
    allow_lane_control: bool,
}

/// The config-derived inputs every runtime shares.
///
/// Bundled rather than passed positionally: they are mostly `Option`s and `Arc`s
/// of similar shape, and a long positional list of those transposes silently at
/// a call site.
struct RuntimeInputs {
    workspace: PathBuf,
    durable_id: Option<String>,
    prompt_ctx: crate::prompts::PromptContext,
    browser_summary: Option<BrowserSummaryProvider>,
    exa_api_key: Option<String>,
    memory: crate::memory::MemoryLimits,
    delegate: InferenceProfileConfig,
}

/// A researched, versioned identity overlaid on the snippet runtime.
type AgentIdentity<'a> = (&'a str, &'a str);

impl AgentRuntime {
    /// Mission Control: routing and orchestration only.
    ///
    /// No model factory (the daemon drives it rather than delegating to it), a
    /// routing-only tool set, and the orchestrator prompt — which deliberately
    /// does NOT stack the coding-agent layer, because that identity ("own the
    /// task end to end") made Mission Control advertise as a general engineer
    /// and skip `list_sessions`.
    fn mission_control(i: RuntimeInputs) -> Result<Self, ToolError> {
        let context = ToolContext::mission_control(i.workspace)?
            .with_durable_session_id_opt(i.durable_id)
            .with_store_path(crate::store::default_db_path())
            // Mission Control is an ADDRESSABLE agent: it holds a directory row
            // and an agent id, so a peer can name it and a lease can attribute a
            // turn to it. Without this it could post to the board but nothing
            // could address it back.
            .with_agent_id(crate::mission_control::SESSION_ID);

        let mut tools = ToolRegistry::new();
        tools.insert(crate::builtins::BashTool);
        tools.insert(crate::builtins::ReadFileTool);
        tools.insert(crate::builtins::ReadImageTool);
        crate::mission_tools::add_mission_control_tools(&mut tools);
        crate::coordination_tools::add_coordination_tools(&mut tools);

        Ok(Self {
            factory: None,
            context,
            tools,
            prompt: mission_control_system_prompt(),
            allow_lane_control: false,
        })
    }

    /// The snippet agent: a coding conversation, optionally wearing an identity.
    ///
    /// `identity` is the ONLY difference between `Standard` and `Specialized`.
    /// The base prompt and the tool set are shared either way; the overlay
    /// changes judgment and specialization, not the session's execution, safety
    /// or conversation rules — and not its executable permissions.
    fn snippet(i: RuntimeInputs, identity: Option<AgentIdentity<'_>>) -> Result<Self, ToolError> {
        let context = match i.browser_summary {
            Some(provider) => ToolContext::with_browser_summary(i.workspace, provider)?,
            None => ToolContext::new(i.workspace)?,
        }
        .with_durable_session_id_opt(i.durable_id.clone())
        // Every session's coordination tools open the same database the daemon
        // owns, so board posts and tasks share one store.
        .with_store_path(crate::store::default_db_path())
        // A specialized session acts as its directory agent, so lease tools can
        // attribute turn ownership to the right identity.
        .with_agent_id_opt(identity.map(|(agent_id, _)| agent_id.to_string()));

        let mut tools = coding_tools(i.exa_api_key.clone(), i.memory);
        crate::mission_tools::add_worker_report_tool(&mut tools);
        tools.insert(crate::mission_tools::CreateRecurringJob);
        // Workers discover peers, post to the board, and message Mission Control.
        // They do NOT dispatch: creating and routing work is Mission Control's
        // job, so a worker asks instead of acting.
        crate::coordination_tools::add_coordination_tools(&mut tools);

        let prompt = match identity {
            Some((agent_id, body)) => crate::prompts::specialized_agent_system_prompt(
                crate::prompts::SpecializedAgentPromptContext {
                    agent_id,
                    identity: body,
                    context: &i.prompt_ctx,
                },
            ),
            None => conversation_prompt(&i.prompt_ctx),
        };

        let delegate = i.delegate;
        let lane_session_id = i.durable_id;
        Ok(Self {
            factory: Some(Arc::new(move || {
                delegate.build_model_for_session(lane_session_id.clone())
            })),
            context,
            tools,
            prompt,
            allow_lane_control: true,
        })
    }

    /// An agent's coordination session: it answers direct messages and dispatches
    /// work, and does not touch a workspace.
    ///
    /// The tool set is the point of this runtime. It deliberately has NO bash and
    /// no file tools, because a session that receives messages must never turn one
    /// into an unrequested edit to someone's repository — that is the boundary a
    /// conversation is supposed to respect. It also has no lease or handoff tools:
    /// those exist for a session HOLDING a work turn, and a coordinator never does,
    /// so carrying them would be dead weight that invites misuse.
    ///
    /// It keeps read-only session awareness so it can dispatch into the right
    /// place, but not Mission Control's task tools — the runtime owns task state,
    /// and a peer managing it would be a second owner.
    fn coordination(
        i: RuntimeInputs,
        identity: AgentIdentity<'_>,
    ) -> Result<Self, ToolError> {
        let (agent_id, body) = identity;
        let context = match i.browser_summary {
            Some(provider) => ToolContext::with_browser_summary(i.workspace, provider)?,
            None => ToolContext::new(i.workspace)?,
        }
        .with_durable_session_id_opt(i.durable_id)
        .with_store_path(crate::store::default_db_path())
        // The board is per agent, so the identity is what makes every board read
        // and write attributable.
        .with_agent_id(agent_id.to_string());

        let mut tools = ToolRegistry::new();
        // Messaging, peer discovery, and the agent's own board.
        crate::coordination_tools::add_coordination_tools(&mut tools);
        // Read-only view of what exists, so a dispatch targets a real session.
        crate::mission_tools::add_coordination_session_tools(&mut tools);

        // The environment layer states this session has bash and full filesystem
        // access. It does not, so the flag both selects the coordination layer and
        // keeps that claim out of the prompt.
        let mut prompt_ctx = i.prompt_ctx;
        prompt_ctx.coordination = true;

        Ok(Self {
            factory: None,
            context,
            tools,
            prompt: crate::prompts::specialized_coordination_prompt(
                crate::prompts::SpecializedAgentPromptContext {
                    agent_id,
                    identity: body,
                    context: &prompt_ctx,
                },
            ),
            // Nothing to delegate: no lane control in a coordination session.
            allow_lane_control: false,
        })
    }

    /// Resolve a session's config to its runtime. THE one place a kind is defined.
    ///
    /// The kind has two arms and the agent is passed THROUGH, not matched on:
    /// a session worked by a specialist resolves to the same runtime as a plain
    /// conversation, differing only by the identity overlay. That is what makes
    /// "an agent is a separate entity" true in the types rather than in a
    /// comment.
    fn resolve(config: SessionConfig, inputs: RuntimeInputs) -> Result<Self, ToolError> {
        match config.role {
            SessionRole::MissionControl => Self::mission_control(inputs),
            SessionRole::Standard => Self::snippet(
                inputs,
                config
                    .agent
                    .as_ref()
                    .map(|a| (a.agent_id.as_str(), a.identity.as_str())),
            ),
            SessionRole::Coordination => {
                let identity = config.agent.as_ref().ok_or_else(|| {
                    ToolError::msg(
                        "a coordination session requires an agent identity — its board is per agent",
                    )
                })?;
                Self::coordination(inputs, (identity.agent_id.as_str(), identity.identity.as_str()))
            }
        }
    }
}

fn start_session_with_role(
    config: &SnippetConfig,
    state_path: PathBuf,
    initial: Option<String>,
    resume: bool,
    stream: Option<StreamHandle>,
    browser_summary: Option<BrowserSummaryProvider>,
    session: SessionConfig,
) -> SessionHandle {
    let (input_tx, rx) = mpsc::unbounded_channel();

    // Config-derived values, snapshotted here because `config` is a borrow and
    // the session task must be `'static`.
    let workspace = config.workspace.clone();
    let model_config = config.model.clone();
    let delegate = config.delegate_profile();
    let exa_api_key = config.exa_api_key.clone();
    let manual_approval = config.manual_approval;
    let context_window_tokens = model_config.context_window;
    let compact_at_pct = model_config.compact_at_pct;
    let memory = crate::memory::MemoryLimits {
        enabled: config.memory_enabled,
        writable: true,
        index_budget_chars: config.memory_index_budget_chars,
        entry_budget_chars: config.memory_entry_budget_chars,
        max_entries: config.memory_max_entries,
    };
    let memory_enabled = config.memory_enabled;
    let memory_index_budget_chars = config.memory_index_budget_chars;
    let memory_entry_budget_chars = config.memory_entry_budget_chars;
    let memory_max_entries = config.memory_max_entries;
    let memory_reflect_on_compaction = config.memory_reflect_on_compaction;
    let sp = state_path.clone();
    let stream_out = stream.clone();

    let join = tokio::spawn(async move {
        let durable_id = Some(session_id_for_state_path(&sp));
        let mut model = model_config.build_model_for_session(durable_id.clone());
        // Session-start capability snapshot for conditional prompt layers. This
        // is computed once and stays fixed for the session (cache-stable).
        let prompt_ctx = crate::prompts::PromptContext {
            worktree: workspace_is_worktree(&workspace),
            memory: memory_enabled,
            memory_writable: memory_enabled,
            skills: !crate::skills::discover().is_empty(),
            vault: !crate::vault::Vault::load().is_empty(),
            browser: browser_summary
                .as_ref()
                .map(|provider| browser_summary_is_connected(&provider()))
                .unwrap_or(false),
            // Set by the role, once it is known: only a coordination runtime
            // wants that layer, and it is the reason the environment layer is
            // dropped from its prompt.
            coordination: false,
        };
        let runtime = AgentRuntime::resolve(
            session,
            RuntimeInputs {
                workspace,
                durable_id,
                prompt_ctx,
                browser_summary,
                exa_api_key: exa_api_key.clone(),
                memory,
                delegate,
            },
        )
        .map_err(|e| e.to_string())?;

        let harness = CodingHarness::new(
            HarnessConfig {
                system_prompt: runtime.prompt,
                state_path: Some(sp),
                resume,
                exa_api_key: exa_api_key.clone(),
                context_window_tokens,
                compact_at_pct,
                manual_approval,
                memory_enabled,
                memory_index_budget_chars,
                memory_entry_budget_chars,
                memory_max_entries,
                memory_reflect_on_compaction,
                allow_lane_control: runtime.allow_lane_control,
                ..HarnessConfig::default()
            },
            runtime.tools,
            runtime.context,
        );
        harness
            .run_interactive(&mut model, initial, rx, runtime.factory, stream)
            .await
            .map_err(|e| e.to_string())
    });

    SessionHandle {
        input_tx,
        join,
        state_path,
        stream: stream_out,
    }
}

/// One session as seen on disk, for the serve daemon's device-wide list.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// Stable id = the state file's path relative to the workspaces root
    /// (e.g. `snipett-2a3f/state.json`). Used to resolve the session for /attach.
    pub id: String,
    /// Absolute workspace folder.
    pub folder: String,
    /// Conversation name (`default` for the active state, else the saved name).
    pub conversation: String,
    /// First user request (truncated), for a list label.
    pub title: String,
    pub status: String,
    /// Last-active time, unix seconds.
    pub last_active: i64,
    /// The agent this session IS, if any — its inbox, or a specialized session.
    /// A durable property of the session: it survives restarts and does not
    /// change just because work was routed here.
    pub agent_id: Option<String>,
    /// The agent dispatched to work here RIGHT NOW, from the task board's
    /// roster. Different question from [`Self::agent_id`]: a plain project
    /// session is nobody's, yet an agent can be working in it. Without this the
    /// session list showed nothing for exactly the case the badge exists for —
    /// the user asked to see "the agent when it is working in a session".
    pub worker_agent_id: Option<String>,
}

/// The inbox session id for an agent: `inbox-<agent-id>`.
///
/// A dedicated inbox per agent is what makes direct messaging predictable — a
/// message never lands in whichever task session happens to be running. The
/// `inbox-` prefix cannot collide with a workspace directory, because
/// `config::workspace_dir_name` always appends `-<hex key>`.
pub fn inbox_session_id(agent_id: &str) -> String {
    format!("inbox-{agent_id}")
}

/// Whether a session id names an agent inbox rather than a project session.
///
/// Accepts the pre-canonical `inbox-<agent>/state.json` form too, so a stored
/// reference or a client cache from before the id rewrite still classifies.
pub fn is_inbox_session_id(id: &str) -> bool {
    id.strip_prefix("inbox-")
        .and_then(|rest| rest.strip_suffix("/state.json").or(Some(rest)))
        .is_some_and(|agent_id| {
            !agent_id.is_empty()
                && agent_id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
}

/// Resolve a session id to its state path, rejecting any id that escapes the root.
///
/// The path is only a KEY: a store-backed session has no file behind it, so
/// requiring the filesystem to back it would make every session unresolvable.
/// Traversal is rejected lexically for that reason — canonicalizing the parent
/// returns `None` when the directory does not exist.
pub fn state_path_for_id(id: &str) -> Option<PathBuf> {
    if crate::mission_control::is_session_id(id) {
        return Some(crate::mission_control::session_state_path());
    }
    resolve_session_path(&workspaces_root(), id)
}

/// Resolve a session id to its path under `root`.
///
/// Split out from [`state_path_for_id`] so the completion rule below can be
/// tested against a tempdir instead of the caller's real workspaces.
pub(crate) fn resolve_session_path(root: &Path, id: &str) -> Option<PathBuf> {
    let rel = Path::new(id);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    // A session id IS the path, minus any filename it used to carry. Nothing is
    // appended: the id names the session, and `session_id_for_state_path` walks
    // the same path back, so the two are exact inverses. They used to disagree
    // — the id carried `/state.json` while callers routinely dropped it — which
    // is why a reply could be rejected for naming a session that plainly existed.
    let resolved = root.join(rel);
    // A symlink inside the root can still point out of it; the lexical check
    // above cannot see that.
    if let (Ok(real), Ok(canon_root)) = (
        std::fs::canonicalize(&resolved),
        std::fs::canonicalize(root),
    ) && !real.starts_with(&canon_root)
    {
        return None;
    }
    Some(resolved)
}

/// The store's row for a state path, if the session has been migrated.
///
/// THE lookup the sidecar readers use. Each of them (`profile`, `role`,
/// `last_active`) reads a file that a migrated session does not have, so without
/// this a store-backed session would silently report no model override, no role,
/// and no activity. Returning the row rather than one field keeps the four
/// readers on a single query shape.
fn store_session_row(state_path: &Path) -> Option<crate::conversations::SessionRow> {
    let store = store_for_sessions()?;
    let id = session_id_for_state_path(state_path);
    store.get_session_row(&id).ok()?
}

/// Sidecar file holding a session's per-conversation model override (the profile
/// name), kept next to its state file so it survives daemon restarts.
/// Read a session's persisted model override, if one was set.
///
/// Store first: a migrated session has no `.profile`, so reading only the file
/// would drop the override and silently run the conversation on the default
/// model.
pub fn read_session_profile(state_path: &std::path::Path) -> Option<String> {
    let profile = store_session_row(state_path).and_then(|row| row.profile)?;
    let t = profile.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Persist a session's model override (or clear it when `profile` is empty).
///
/// Writes to the STORE when the session has a row. [`read_session_profile`] is
/// store-first, so a file-only write here would be shadowed by the row's older
/// value — the switch would appear to revert on the next read.
pub fn write_session_profile(state_path: &std::path::Path, profile: &str) {
    if let Some(store) = store_for_sessions() {
        let id = session_id_for_state_path(state_path);
        let _ = store.set_session_profile(&id, Some(profile.trim()));
    }
}

/// Merge the two session sources into the final catalog.
///
/// Store rows WIN on conflict: a migrated session's state file is frozen, so its
/// title and last-active stop advancing, and letting the file's copy through would
/// shadow the authoritative row.
/// Enumerate every session persisted on the device (across all workspaces).
///
/// The store IS the inventory. There is no filesystem walk: a session's row is
/// the only thing that makes it a session, so a walk could only ever rediscover
/// rows or resurrect sessions nothing owns.
/// Which agent is dispatched to work in each session right now.
///
/// The TASK BOARD is the source, not `sessions.agent_id`. They answer different
/// questions: `sessions.agent_id` is what a session IS (an inbox belongs to an
/// agent, a specialized session runs as one), while this is who was sent to work
/// there. A plain project session is nobody's — yet Mission Control can dispatch
/// an agent into it, and that is precisely the case the session list needs to
/// show. Reading only the binding made the badge invisible for the work the user
/// actually dispatches.
///
/// Built in one pass over the board so the session list stays a single query per
/// table, rather than one per row.
fn working_agents_by_session() -> std::collections::HashMap<String, String> {
    match store_for_sessions() {
        Some(store) => working_agents_in(&store),
        None => std::collections::HashMap::new(),
    }
}

/// The mapping itself, against a given store — so it can be asserted directly
/// rather than only through the global session path.
fn working_agents_in(store: &crate::store::Store) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(tasks) = store.list_tasks(None, None) else {
        return out;
    };
    for task in tasks.iter().filter(|task| !task.status.is_terminal()) {
        if task.session_id.trim().is_empty() {
            continue;
        }
        // Canonical, so the key matches the id this catalog stores: a task may
        // carry the pre-canonicalisation form (`x/state.json`).
        let session = crate::conversations::canonical_session_id(&task.session_id).0;
        let Ok(members) = store.list_task_agents(&task.id) else {
            continue;
        };
        if let Some(agent) = members
            .into_iter()
            .find(|member| member.removed_at.is_none())
            .map(|member| member.agent_id)
        {
            // Newest wins: `list_tasks` returns newest first, and the most
            // recent dispatch is the one actually working.
            out.entry(session).or_insert(agent);
        }
    }
    out
}

pub fn list_device_sessions() -> Vec<SessionInfo> {
    let workers = working_agents_by_session();
    let mut out: Vec<SessionInfo> = store_for_sessions()
        .and_then(|store| store.list_all_sessions().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|row| SessionInfo {
            conversation: conversation_name_from_id(&row.id).to_string(),
            // Read before `row.id` is moved out below — struct-literal fields
            // evaluate in the order written.
            worker_agent_id: workers.get(&row.id).cloned(),
            id: row.id,
            folder: row.workspace,
            title: row.title.unwrap_or_default(),
            status: row.status,
            last_active: row.last_active.unwrap_or(0),
            agent_id: row.agent_id,
        })
        .collect();

    out.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    // Mission Control is always the first entry.
    if let Some(idx) = out
        .iter()
        .position(|s| crate::mission_control::is_session_id(&s.id))
    {
        let mc = out.remove(idx);
        out.insert(0, mc);
    }
    out
}

/// The conversation name a session id encodes.
///
/// A session that lives under `conversations/` is a SAVED conversation, and its
/// name is that file's stem. Everything else is the workspace's single default
/// session — the root directory, an agent inbox, Mission Control, or a
/// pre-canonical id that still carries a `/state.json` suffix. Determining this
/// from the presence of a `conversations/` component (rather than from a filename
/// suffix) is what keeps it correct now that a canonical id has no filename: the
/// root id would otherwise report the workspace directory as its conversation
/// name, and the TUI picker would list a workspace as if it were a saved
/// conversation.
fn conversation_name_from_id(id: &str) -> &str {
    if let Some(rest) = id.rsplit_once("/conversations/") {
        return rest.1.strip_suffix(".json").unwrap_or(rest.1);
    }
    "default"
}

/// Whether a session is a valid target for dispatched work.
///
/// Two exclusions, both structural rather than stylistic:
///
/// - Mission Control coordinates; routing work to itself is a loop.
/// - An agent's INBOX is a mailbox. It runs the coordination runtime — no file
///   or shell tools, and no `report_mission_task` — so work sent there can be
///   neither done nor reported, and the task sits InProgress forever.
///
/// Named so the rule has one home and can be asserted directly; when this was
/// an inline filter, the inbox case was simply missing.
pub fn is_routable_target(id: &str) -> bool {
    !crate::mission_control::is_session_id(id) && !is_inbox_session_id(id)
}

/// Same catalog as [`list_device_sessions`], without Mission Control itself and
/// without agent inboxes — neither is a place work runs.
pub fn list_routable_sessions() -> Vec<SessionInfo> {
    list_device_sessions()
        .into_iter()
        .filter(|s| is_routable_target(&s.id))
        .collect()
}

/// The session-list label. New state stores this in `title`; the initial request
/// fallback is only for old states while they are being migrated.
fn effective_title(state: &HarnessState) -> String {
    state
        .title
        .as_deref()
        .or_else(|| state.initial_request())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(120).collect())
        .unwrap_or_default()
}

/// The status string exposed on the session list / events APIs. Uses the enum's
/// serde (snake_case) name.
pub fn status_str(status: crate::harness::HarnessStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The persisted spelling of a session's role.
///
/// Explicit rather than derived from serde: this string is stored in the
/// `sessions.role` column and compared against literal `'mission_control'`, so a
/// silent rename under it would strand every mission-control session as
/// "standard" — routing would then treat the coordinator as ordinary work.
pub fn role_str(role: SessionRole) -> &'static str {
    match role {
        SessionRole::MissionControl => "mission_control",
        SessionRole::Standard => "standard",
        SessionRole::Coordination => "coordination",
    }
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Set (or clear, if empty) a saved session's title override.
///
/// Writes through the dual writer: a session that lives in the store has no
/// state file, so writing one here would be ignored on the next read and the
/// rename would silently do nothing. For sessions that aren't currently live —
/// the daemon routes live ones through the loop so its in-memory state stays in
/// sync.
pub fn set_session_title(state_path: &Path, title: &str) -> Result<(), String> {
    let mut state = read_session_state(state_path)
        .ok_or_else(|| format!("session state unreadable: {}", state_path.display()))?;
    let t = title.trim();
    state.title = if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    };
    write_session_state(state_path, &state)
}

/// Where a fork cuts the source conversation. Both ends are exclusive lengths
/// (`events[..event_end]`, `messages[..message_end]`).
#[derive(Debug, Clone, Copy)]
pub struct ForkPoint {
    pub event_end: usize,
    pub message_end: usize,
}

/// Resolve a fork cut from a checkpoint id and/or event index.
///
/// - **checkpoint**: same boundary as `/rewind` (state *before* that turn).
/// - **event_index**: keep through that event (inclusive), then snap back to a
///   provider-safe boundary (no orphan tool_call / tool_result pairs).
/// - both: checkpoint wins for the cut; event_index is ignored.
pub fn resolve_fork_point(
    state: &HarnessState,
    checkpoint: Option<&str>,
    event_index: Option<usize>,
) -> Result<ForkPoint, String> {
    if let Some(id) = checkpoint.map(str::trim).filter(|s| !s.is_empty()) {
        let cp = state
            .checkpoints
            .iter()
            .rev()
            .find(|c| c.id == id || c.id.starts_with(id))
            .ok_or_else(|| format!("no checkpoint matching `{id}`"))?;
        return Ok(ForkPoint {
            event_end: cp.event_index.min(state.events.len()),
            message_end: cp.message_index.min(state.messages.len()),
        });
    }
    let Some(idx) = event_index else {
        return Err("fork requires `checkpoint` or `event_index`".into());
    };
    if state.events.is_empty() {
        return Err("nothing to fork — session has no events".into());
    }
    if idx >= state.events.len() {
        return Err(format!(
            "event_index {idx} out of range (0..{})",
            state.events.len().saturating_sub(1)
        ));
    }
    // Keep through idx (inclusive), then walk back to a safe tool-pairing boundary.
    let mut event_end = idx + 1;
    event_end = snap_event_end_safe(&state.events, event_end);
    let message_end = message_end_for_events(state, event_end);
    Ok(ForkPoint {
        event_end,
        message_end,
    })
}

/// Walk exclusive `event_end` backward so we don't strand a tool_call without its
/// result (or a trailing tool_result without its call) — providers 400 on that.
fn snap_event_end_safe(events: &[crate::harness::HarnessEvent], mut end: usize) -> usize {
    use crate::harness::HarnessEvent;
    end = end.min(events.len());
    while end > 0 {
        match &events[end - 1] {
            HarnessEvent::ToolResult { .. } => {
                // Ensure a ToolCall exists earlier in the kept prefix for pairing
                // at the tail; if the tail is ToolResult after ToolCall we're fine.
                break;
            }
            HarnessEvent::ToolCall { .. } => {
                // Orphan call at end — drop it.
                end -= 1;
            }
            HarnessEvent::ApprovalRequest { .. } | HarnessEvent::InvalidToolCall { .. } => {
                end -= 1;
            }
            _ => break,
        }
    }
    end
}

/// Best-effort message length matching a kept event prefix.
/// Prefer a checkpoint on the same boundary; otherwise count user/assistant/tool
/// events and consume messages in order until those counts are met.
fn message_end_for_events(state: &HarnessState, event_end: usize) -> usize {
    use crate::harness::HarnessEvent;
    use crate::llm::HarnessMessage;

    if let Some(cp) = state
        .checkpoints
        .iter()
        .filter(|c| c.event_index == event_end)
        .last()
    {
        return cp.message_index.min(state.messages.len());
    }
    // Nearest checkpoint at or before the cut — start counts from there.
    let (mut base_event, mut base_msg) = state
        .checkpoints
        .iter()
        .filter(|c| c.event_index <= event_end)
        .max_by_key(|c| c.event_index)
        .map(|c| (c.event_index, c.message_index))
        .unwrap_or((0, 0));
    base_event = base_event.min(event_end);
    base_msg = base_msg.min(state.messages.len());

    let mut need_user = 0usize;
    let mut need_assistant = 0usize;
    let mut need_tool = 0usize;
    for ev in state
        .events
        .get(base_event..event_end)
        .into_iter()
        .flatten()
    {
        match ev {
            HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. } => need_user += 1,
            HarnessEvent::AssistantText { .. } => need_assistant += 1,
            HarnessEvent::ToolCall { .. } | HarnessEvent::ToolResult { .. } => need_tool += 1,
            _ => {}
        }
    }

    let mut i = base_msg;
    let mut got_user = 0usize;
    let mut got_assistant = 0usize;
    let mut got_tool = 0usize;
    while i < state.messages.len() {
        if got_user >= need_user && got_assistant >= need_assistant && got_tool >= need_tool {
            break;
        }
        match &state.messages[i] {
            HarnessMessage::User { .. } => {
                if got_user >= need_user {
                    break;
                }
                got_user += 1;
            }
            HarnessMessage::Assistant { .. } => {
                if got_assistant >= need_assistant && got_tool >= need_tool && got_user >= need_user
                {
                    // Extra assistant after targets met — stop before it.
                    break;
                }
                got_assistant += 1;
            }
            HarnessMessage::ToolResult { .. } => {
                got_tool += 1;
            }
            HarnessMessage::System { .. } | HarnessMessage::Summary { .. } => {}
        }
        i += 1;
    }
    i
}

/// Build a forked [`HarnessState`]: history truncated to `point`, idle, no live
/// lanes/watches/questions. Workspace path is unchanged (shared files on disk).
pub fn build_forked_state(source: &HarnessState, point: ForkPoint) -> HarnessState {
    use crate::harness::{ApprovalMode, HarnessStatus};

    let now = chrono::Utc::now().to_rfc3339();
    let event_end = point.event_end.min(source.events.len());
    let message_end = point.message_end.min(source.messages.len());

    let mut forked = source.clone();
    forked.events.truncate(event_end);
    forked.messages.truncate(message_end);
    forked
        .checkpoints
        .retain(|c| c.event_index <= event_end && c.message_index <= message_end);
    forked.lanes.clear();
    forked.watches.clear();
    forked.pending_question = None;
    forked.goal = None;
    forked.compacting = false;
    forked.compacting_started_at = None;
    forked.turn_started_at = None;
    forked.final_text = None;
    forked.status = HarnessStatus::Idle;
    forked.approval_mode = ApprovalMode::Auto;
    // Fresh usage accounting for the branch (history is what matters).
    forked.total_tokens = 0;
    forked.prompt_tokens = 0;
    forked.completion_tokens = 0;
    forked.cache_read_tokens = 0;
    forked.tool_payloads_pruned = false;
    forked.queued_inputs.clear();
    // Keep last_prompt_tokens / context_window as hints; model will refresh.
    forked.created_at = now.clone();
    forked.updated_at = now;
    forked.iterations = 0;

    let base_title = source
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("fork");
    let short: String = base_title.chars().take(60).collect();
    forked.title = Some(format!("fork · {short}"));
    forked
}

/// Result of writing a forked conversation next to the source session.
#[derive(Debug, Clone)]
pub struct ForkedConversation {
    /// Session id relative to the workspaces root (same form as `/sessions`).
    pub id: String,
    pub state_path: PathBuf,
    pub title: String,
    pub event_end: usize,
    pub message_end: usize,
}

/// Fork `source_state_path` at `point` into a new `conversations/<uuid>.json`.
/// Copies the model-profile sidecar when present. Does **not** start a live loop.
pub fn write_forked_conversation(
    source_state_path: &Path,
    source: &HarnessState,
    point: ForkPoint,
) -> Result<ForkedConversation, String> {
    let store =
        store_for_sessions().ok_or_else(|| "the session store is unavailable".to_string())?;
    write_forked_conversation_in(&store, source_state_path, source, point)
}

/// [`write_forked_conversation`] against an explicit store, so tests use an
/// in-memory one instead of writing into the real database.
pub fn write_forked_conversation_in(
    store: &crate::store::Store,
    source_state_path: &Path,
    source: &HarnessState,
    point: ForkPoint,
) -> Result<ForkedConversation, String> {
    let forked = build_forked_state(source, point);
    let title = forked.title.clone().unwrap_or_else(|| "fork".to_string());

    let parent = source_state_path
        .parent()
        .ok_or_else(|| "source state path has no parent".to_string())?;
    // Source may be `state.json` or `conversations/<id>.json` — forks always land
    // in `conversations/` beside the workspace state root.
    let conv_dir = if parent.file_name().and_then(|s| s.to_str()) == Some("conversations") {
        parent.to_path_buf()
    } else {
        parent.join("conversations")
    };

    let name = uuid::Uuid::new_v4().to_string();
    let dest = conv_dir.join(format!("{name}.json"));
    let root = workspaces_root();
    let id = dest
        .strip_prefix(&root)
        .unwrap_or(&dest)
        .display()
        .to_string();

    // The branch is a store row, like every other session — writing only a file
    // would make it unopenable, since reads are store-only.
    let extras = crate::conversations::SessionExtras {
        // Creating a branch is a user action: put it at the top of the list.
        last_active: Some(now_unix_secs()),
        // Carry the per-conversation model override onto the branch.
        profile: read_session_profile(source_state_path),
        ..Default::default()
    };
    store
        .import_session(
            &id,
            &crate::config::workspace_key(Path::new(&forked.workspace)),
            &forked,
            &extras,
        )
        .map_err(|e| format!("write fork: {e}"))?;

    Ok(ForkedConversation {
        id,
        state_path: dest,
        title,
        event_end: point.event_end.min(source.events.len()),
        message_end: point.message_end.min(source.messages.len()),
    })
}

/// Park the work a dead worker session was doing.
///
/// A dispatched session that dies without calling `report_mission_task` leaves
/// its task looking active forever, so this is the one place that notices.
pub fn park_failed_session_work(id: &str, prev_status: &str, state: &HarnessState) {
    let status = status_str(state.status);
    if status != "failed" || prev_status == "failed" {
        return;
    }
    let detail = state
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            crate::harness::HarnessEvent::ModelError { message } => Some(message.as_str()),
            _ => None,
        })
        .unwrap_or("");
    if let Some(store) = store_for_sessions() {
        let _ = store.block_tasks_for_failed_session(
            id,
            detail,
            &chrono::Utc::now().to_rfc3339(),
        );
    }
}

pub fn session_id_for_state_path(state_path: &Path) -> String {
    if state_path == crate::mission_control::session_state_path() {
        return crate::mission_control::SESSION_ID.to_string();
    }
    let root = workspaces_root();
    let relative = state_path
        .strip_prefix(&root)
        .unwrap_or(state_path)
        .display()
        .to_string();
    // The one place a path becomes an id, so a leftover filename is dropped here
    // rather than at each of the ~27 callers. A path carrying `/state.json` (a
    // value captured before ids were canonicalised) names the same session as
    // the bare form.
    crate::conversations::canonical_session_id(&relative).0
}

/// Read a session's state from the store.
///
/// This is THE session reader. A session lives in the store; there is no file
/// fallback, because no session has a state file any more.
pub fn read_session_state(state_path: &Path) -> Option<HarnessState> {
    read_session_state_from_store(state_path)
}

/// The store-backed read: resolve the id from the path, then load the scalar
/// plus both logs.
fn read_session_state_from_store(state_path: &Path) -> Option<HarnessState> {
    let id = session_id_for_state_path(state_path);
    let store = store_for_sessions()?;
    if !store.has_conversation(&id).ok()? {
        return None;
    }
    let scalar = store.load_session_scalar(&id).ok()??;
    let messages = store.load_conversation_messages(&id).ok()?;
    let events = store.load_conversation_events(&id).ok()?;
    crate::harness::state_from_scalar(&scalar, messages, events).ok()
}

/// Open the session store, if one exists at the canonical path.
pub(crate) fn store_for_sessions() -> Option<crate::store::Store> {
    crate::store::Store::open_cached(crate::store::default_db_path()).ok()
}

/// The workspace's default session id, `<dir>`, if the store holds ANY
/// session for this workspace.
///
/// Matched on the recorded `workspace` column, NOT on a freshly derived key: the
/// key algorithm changed once, so a workspace migrated under an older key still
/// owns its history. Deriving the key again would miss that row, and the caller
/// would start a blank session (and a new worktree) beside the real one.
///
/// The directory prefix comes from whichever row exists, because most migrated
/// workspaces have only conversation rows — they never had a default session.
pub fn store_default_session_id(workspace: &Path) -> Option<String> {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let want = canonical.to_string_lossy().to_string();
    store_for_sessions()?
        .list_all_sessions()
        .ok()?
        .into_iter()
        .find(|row| row.workspace == want)
        // A conversation id is `<dir>/conversations/<uuid>`; the workspace is the
        // first component either way. Return the dir itself, which is the
        // default session's canonical id.
        .and_then(|row| row.id.split('/').next().map(str::to_string))
}

/// Write a session's state back to wherever it lives.
///
/// The counterpart to [`read_session_state`], for the writers OUTSIDE the harness
/// (checkpoint rewind, title/mode edits). Those have no harness instance and no
/// counters, so they replace the history rather than appending to it — which is
/// correct for them, since each one has just rewritten the transcript.
pub fn write_session_state(state_path: &Path, state: &HarnessState) -> Result<(), String> {
    let id = session_id_for_state_path(state_path);
    let store =
        store_for_sessions().ok_or_else(|| "the session store is unavailable".to_string())?;
    let scalar = crate::harness::scalar_json(state)?;
    let status = status_str(state.status);
    store
        .save_session_scalar(
            &id,
            &crate::config::workspace_key(Path::new(&state.workspace)),
            &state.workspace,
            state.title.as_deref(),
            &status,
            &scalar,
            &state.created_at,
            &state.updated_at,
        )
        .map_err(|e| format!("save session: {e}"))?;
    store
        .replace_conversation_messages(&id, &state.messages, &state.updated_at)
        .map_err(|e| format!("save messages: {e}"))?;
    store
        .replace_conversation_events(&id, &state.events, &state.updated_at)
        .map_err(|e| format!("save events: {e}"))?;
    Ok(())
}

const DEVICE_EVENTS_CAP: usize = 64;

static DEVICE_EVENTS: OnceLock<broadcast::Sender<serde_json::Value>> = OnceLock::new();

fn device_events() -> &'static broadcast::Sender<serde_json::Value> {
    DEVICE_EVENTS.get_or_init(|| broadcast::channel(DEVICE_EVENTS_CAP).0)
}

/// Subscribe to the device-wide `/events` firehose (status + terminal bells).
pub fn subscribe_device_events() -> broadcast::Receiver<serde_json::Value> {
    device_events().subscribe()
}

pub fn replay_notification_events(since: u64) -> Vec<serde_json::Value> {
    store_for_sessions()
        .and_then(|store| store.notification_events_since(since).ok())
        .unwrap_or_default()
}

/// Last-active unix seconds for a state file: the store's stamp if the session
/// has been migrated, else the sidecar, else the file mtime (legacy sessions that
/// have never been rewritten).
///
/// Store first, because a migrated session has neither sidecar nor state file —
/// and the session list sorts on this, so returning 0 for every migrated session
/// would collapse the ordering.
/// Last-active unix seconds for a session. Reads the store row; the list sorts on
/// this, so a file fallback would mis-order every migrated session.
pub fn session_last_active(state_path: &Path) -> i64 {
    store_session_row(state_path)
        .and_then(|row| row.last_active)
        .unwrap_or(0)
}

/// Record that the user sent a message in this session. List sort uses this
/// stamp, not state-file mtime.
///
/// Writes to the STORE when the session has a row. The list reads
/// `last_active` from the row, so a file-only write here would leave a migrated
/// session frozen at its migration timestamp — the chat would sink down the list
/// even while actively being used. (Caught live: the running session showed
/// 86 minutes stale against a 3-minute-old file.)
pub fn bump_session_activity(state_path: &Path) {
    if let Some(store) = store_for_sessions() {
        let id = session_id_for_state_path(state_path);
        let _ = store.set_session_last_active(&id, now_unix_secs());
    }
}

/// If `folder` is a git work tree, add a detached worktree under
/// `~/.snippet/worktrees/{repo}/{id}` and return that path (preserving a
/// subfolder relative to the repo root). Non-git folders, nested worktrees
/// already under that root, and any git failure fall back to `folder`.
pub fn prepare_new_session_workspace(folder: &Path) -> PathBuf {
    try_session_worktree(folder).unwrap_or_else(|| folder.to_path_buf())
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn sanitize_repo_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "repo".into() } else { s }
}

fn unique_worktree_path(parent: &Path) -> Option<PathBuf> {
    for _ in 0..8 {
        let id = uuid::Uuid::new_v4().to_string();
        let dest = parent.join(&id[..8]);
        if !dest.exists() {
            return Some(dest);
        }
    }
    Some(parent.join(uuid::Uuid::new_v4().to_string()))
}

fn try_session_worktree(folder: &Path) -> Option<PathBuf> {
    let inside = git_stdout(folder, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let toplevel = PathBuf::from(git_stdout(folder, &["rev-parse", "--show-toplevel"])?);
    let root = crate::config::worktrees_root();
    if folder.starts_with(&root) || toplevel.starts_with(&root) {
        return None;
    }
    let repo = sanitize_repo_name(toplevel.file_name()?.to_str()?);
    let parent = root.join(&repo);
    std::fs::create_dir_all(&parent).ok()?;
    // Unique per session so one repo can host many parallel worktrees.
    // Named branch (not --detach) so the agent can commit/push without
    // fighting detached HEAD, and never lands on main.
    let dest = unique_worktree_path(&parent)?;
    let branch = dest
        .file_name()
        .and_then(|s| s.to_str())
        .map(|id| format!("snippet/{id}"))
        .unwrap_or_else(|| "snippet/session".into());
    let status = Command::new("git")
        .arg("-C")
        .arg(&toplevel)
        .args(["worktree", "add", "-b", &branch])
        .arg(&dest)
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dest);
        return None;
    }
    let workspace = match folder.strip_prefix(&toplevel) {
        Ok(rel) if !rel.as_os_str().is_empty() => dest.join(rel),
        _ => dest,
    };
    Some(workspace)
}

/// Persist a brand-new idle conversation in `folder` so Mission Control can
/// dispatch to it. `new_conversation=false` uses the folder's default session id
/// (refuses if one already exists). `true` always mints a fresh
/// `conversations/<uuid>.json` id. Git repos always get an isolated worktree;
/// non-git folders and already-isolated worktrees stay put.
///
/// Writes a store row, not a state file — a new session has no file at all.
pub fn create_blank_session(
    folder: &Path,
    title: &str,
    new_conversation: bool,
) -> Result<SessionInfo, String> {
    let store =
        store_for_sessions().ok_or_else(|| "the session store is unavailable".to_string())?;
    create_blank_session_in(&store, folder, title, new_conversation)
}

/// [`create_blank_session`] against an explicit store, so tests can use an
/// in-memory one instead of writing fixtures into the real database.
pub fn create_blank_session_in(
    store: &crate::store::Store,
    folder: &Path,
    title: &str,
    new_conversation: bool,
) -> Result<SessionInfo, String> {
    let mut folder = folder
        .canonicalize()
        .map_err(|e| format!("folder is not a directory: {e}"))?;
    if !folder.is_dir() {
        return Err("folder is not a directory".into());
    }
    // A git repo gets an isolated worktree for the new session. That is the
    // WORKSPACE, not the session's storage — the two are independent now.
    folder = prepare_new_session_workspace(&folder);
    if let Ok(canonical) = folder.canonicalize() {
        folder = canonical;
    }
    let base = crate::config::state_path_for_workspace(&folder);
    let dest = if new_conversation {
        base.join("conversations")
            .join(uuid::Uuid::new_v4().to_string())
    } else {
        base
    };
    // The path is only an ID now — nothing is written to it. Canonicalised, so a
    // session created today has the same id shape as one migrated from the old
    // path-shaped ids; otherwise `session_id_for_state_path` (which strips the
    // filename) would not match what this function stored.
    let id = crate::conversations::canonical_session_id(
        &dest
            .strip_prefix(workspaces_root())
            .unwrap_or(&dest)
            .display()
            .to_string(),
    )
    .0;
    // Uniqueness is the STORE's question: a folder's default session is taken if a
    // row exists, whether or not any file was ever written for it.
    if !new_conversation && store.has_conversation(&id).unwrap_or(false) {
        return Err(
            "this folder already has a default session — pass new_conversation=true or route to the existing id".into(),
        );
    }

    let label = title.trim();
    let state = HarnessState::blank(
        folder.display().to_string(),
        (!label.is_empty()).then(|| label.to_string()),
    );
    // Brand-new chats belong at the top until the user opens something else and
    // sends a message there. Opening this chat later must not re-bump.
    let extras = crate::conversations::SessionExtras {
        last_active: Some(now_unix_secs()),
        ..Default::default()
    };
    store
        .import_session(&id, &crate::config::workspace_key(&folder), &state, &extras)
        .map_err(|e| format!("create session: {e}"))?;

    Ok(SessionInfo {
        id,
        folder: folder.display().to_string(),
        conversation: if new_conversation {
            dest.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("new")
                .to_string()
        } else {
            "default".into()
        },
        title: effective_title(&state),
        status: status_str(state.status),
        last_active: extras.last_active.unwrap_or_else(now_unix_secs),
        // No agent is bound at creation: an agent binds a session afterwards
        // (an inbox by `ensure_agent_inbox`, a specialized session at start).
        agent_id: None,
        // Nor is anyone working in it yet — a dispatch sets this, not creation.
        worker_agent_id: None,
    })
}

/// Delete a session: its worktree, its store row, and any legacy sidecar files.
///
/// The `.profile` sidecar matters even after the move — a leftover would silently
/// re-apply the deleted session's model to the next session on this path.
pub fn remove_session_files(state_path: &Path) {
    remove_session_files_with(store_for_sessions().as_ref(), state_path);
}

/// [`remove_session_files`] against an explicit store, so tests use an in-memory
/// one instead of deleting from the real database.
pub fn remove_session_files_with(store: Option<&crate::store::Store>, state_path: &Path) {
    let id = session_id_for_state_path(state_path);
    // The row knows the workspace when the session lives in the store; a
    // store-backed session has no sidecar to read it from.
    let folder = store
        .and_then(|s| s.get_session_row(&id).ok().flatten())
        .map(|r| PathBuf::from(r.workspace))
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| workspace_from_state_file(state_path));
    if let Some(folder) = folder {
        drop_session_worktree(&folder);
    }
    if let Some(store) = store {
        let _ = store.delete_conversation(&id);
    }
}

fn workspace_from_state_file(state_path: &Path) -> Option<PathBuf> {
    let state = read_session_state(state_path)?;
    if state.workspace.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(state.workspace))
    }
}

/// True when `folder` sits inside an isolated linked git worktree (created for
/// this session) rather than the user's main checkout. Drives whether the
/// worktree-specific prompt layer renders.
pub(crate) fn workspace_is_worktree(folder: &Path) -> bool {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    linked_worktree_root(&folder).is_some()
}

/// Parse the `connected = N` count out of a rendered browser summary. Drives both
/// the conditional browser prompt layer (session start) and whether the live
/// context shows the [browsers] section at all.
pub(crate) fn browser_summary_is_connected(summary: &str) -> bool {
    summary
        .lines()
        .find_map(|line| line.strip_prefix("connected = "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .is_some_and(|count| count > 0)
}

/// Best-effort: drop a linked git worktree created for this session.
/// A linked worktree has a `.git` *file* (not a directory). Never touches
/// the original clone.
fn drop_session_worktree(folder: &Path) {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());
    let Some(worktree) = linked_worktree_root(&folder) else {
        return;
    };
    if let Some(common) = git_stdout(&worktree, &["rev-parse", "--git-common-dir"]) {
        let common_path = PathBuf::from(&common);
        let common_path = if common_path.is_absolute() {
            common_path
        } else {
            worktree.join(common_path)
        };
        if let Some(main) = common_path.parent() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(main)
                .args(["worktree", "remove", "--force"])
                .arg(&worktree)
                .status();
        }
    }
    if worktree.exists() {
        let _ = std::fs::remove_dir_all(&worktree);
    }
}

/// Walk up from `folder` until we find a `.git` file — the linked-worktree
/// marker. A `.git` directory is the original clone and is left alone.
fn linked_worktree_root(folder: &Path) -> Option<PathBuf> {
    let mut cur = folder.to_path_buf();
    loop {
        let git = cur.join(".git");
        if git.is_file() {
            return Some(cur);
        }
        if git.is_dir() {
            return None;
        }
        if !cur.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod fork_tests {
    use super::*;
    use crate::harness::{HarnessEvent, HarnessState, HarnessStatus};
    use crate::llm::HarnessMessage;

    fn sample_state() -> HarnessState {
        // Build via JSON so private migration fields stay internal.
        let mut s: HarnessState = serde_json::from_value(serde_json::json!({
            "version": 1,
            "status": "idle",
            "created_at": "t0",
            "updated_at": "t0",
            "workspace": "/tmp/ws",
            "title": "original title",
            "messages": [],
            "events": [],
            "iterations": 3,
            "total_tokens": 100,
            "prompt_tokens": 80,
            "completion_tokens": 20,
            "last_prompt_tokens": 50,
            "context_window": 128000
        }))
        .expect("sample state");
        s.messages = vec![
            HarnessMessage::User {
                content: "hi".into(),
            },
            HarnessMessage::Assistant {
                content: "hello".into(),
                tool_calls: Vec::new(),
            },
            HarnessMessage::User {
                content: "again".into(),
            },
        ];
        s.events = vec![
            HarnessEvent::UserInput { text: "hi".into() },
            HarnessEvent::AssistantText {
                text: "hello".into(),
            },
            HarnessEvent::UserInput {
                text: "again".into(),
            },
        ];
        s.checkpoints = vec![crate::harness::CheckpointRecord {
            id: "abc12345deadbeef".into(),
            label: "hi".into(),
            created_at: "t0".into(),
            event_index: 0,
            message_index: 0,
        }];
        s
    }

    #[test]
    fn resolve_checkpoint_cut() {
        let s = sample_state();
        let p = resolve_fork_point(&s, Some("abc12345"), None).unwrap();
        assert_eq!(p.event_end, 0);
        assert_eq!(p.message_end, 0);
    }

    #[test]
    fn resolve_event_index_inclusive() {
        let s = sample_state();
        let p = resolve_fork_point(&s, None, Some(1)).unwrap();
        assert_eq!(p.event_end, 2); // keep through index 1
    }

    #[test]
    fn build_fork_truncates_and_idles() {
        let s = sample_state();
        let p = ForkPoint {
            event_end: 2,
            message_end: 2,
        };
        let f = build_forked_state(&s, p);
        assert_eq!(f.events.len(), 2);
        assert_eq!(f.messages.len(), 2);
        assert_eq!(f.status, HarnessStatus::Idle);
        assert!(f.lanes.is_empty());
        assert!(f.title.as_deref().unwrap_or("").starts_with("fork ·"));
        assert_eq!(f.total_tokens, 0);
        assert!(!f.tool_payloads_pruned);
    }

    #[test]
    fn snaps_orphan_tool_call_at_end() {
        let mut s = sample_state();
        s.events.push(HarnessEvent::ToolCall {
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        });
        s.messages.push(HarnessMessage::Assistant {
            content: String::new(),
            tool_calls: Vec::new(),
        });
        let last = s.events.len() - 1;
        let p = resolve_fork_point(&s, None, Some(last)).unwrap();
        // Exclusive end must not leave a trailing ToolCall.
        if p.event_end > 0 {
            assert!(!matches!(
                s.events[p.event_end - 1],
                HarnessEvent::ToolCall { .. }
            ));
        }
    }

    #[test]
    fn a_fork_is_written_to_the_store() {
        // The bug this guards: fork wrote only a file, so the branch got no store
        // row — invisible in the list and unopenable, since reads are store-only.
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let s = sample_state();
        let p = ForkPoint {
            event_end: 2,
            message_end: 2,
        };
        let dir = std::env::temp_dir().join(format!("snippet-fork-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        let source = dir.join("conversations").join("source.json");

        let fork = write_forked_conversation_in(&store, &source, &s, p).unwrap();

        let row = store
            .get_session_row(&fork.id)
            .unwrap()
            .expect("the branch must have a store row");
        assert_eq!(row.title.as_deref(), Some(fork.title.as_str()));
        assert!(row.last_active.is_some(), "a new branch sorts to the top");
        // The truncated transcript, not the whole conversation.
        assert_eq!(store.conversation_message_count(&fork.id).unwrap(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
#[cfg(test)]
mod create_blank_tests {
    use super::*;
    use crate::store::Store;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_folder(stamp: u128, suffix: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("snippet-mc-{suffix}-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn create_blank_session_writes_a_store_row() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = temp_folder(stamp, "blank");
        let store = Store::open_in_memory().unwrap();

        let info = create_blank_session_in(&store, &folder, "Odd request", true).unwrap();
        assert_eq!(info.title, "Odd request");
        assert_eq!(info.status, "idle");
        assert_eq!(
            info.folder,
            folder.canonicalize().unwrap().display().to_string()
        );

        // No state file: the row IS the session.
        assert!(!state_path_for_id(&info.id).unwrap().exists());
        let row = store
            .get_session_row(&info.id)
            .unwrap()
            .expect("row exists");
        assert_eq!(row.title.as_deref(), Some("Odd request"));
        assert_eq!(row.status, "idle");
        assert!(row.last_active.is_some(), "a new chat sorts to the top");

        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_second_default_session_in_the_same_folder_is_refused() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = temp_folder(stamp, "dup");
        let store = Store::open_in_memory().unwrap();

        create_blank_session_in(&store, &folder, "first", false).unwrap();
        let second = create_blank_session_in(&store, &folder, "second", false);
        assert!(second.is_err(), "uniqueness is the store's question now");

        let _ = fs::remove_dir_all(&folder);
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(stamp: u128, suffix: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!("snippet-wt-{suffix}-{stamp}"));
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q"]);
        git_ok(&repo, &["config", "user.email", "snippet@test"]);
        git_ok(&repo, &["config", "user.name", "snippet"]);
        fs::write(repo.join("README"), "hi\n").unwrap();
        git_ok(&repo, &["add", "README"]);
        git_ok(&repo, &["commit", "-qm", "init"]);
        repo.canonicalize().unwrap()
    }

    fn drop_worktree(repo: &Path, workspace: &Path) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(workspace)
            .status();
        if workspace.exists() {
            let _ = fs::remove_dir_all(workspace);
        }
    }

    #[test]
    fn new_session_in_git_repo_uses_isolated_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "repo");
        let workspace = prepare_new_session_workspace(&repo);
        let root = crate::config::worktrees_root();
        assert_ne!(workspace, repo);
        assert!(workspace.starts_with(&root));
        assert!(workspace.join(".git").is_file());
        assert!(workspace.join("README").exists());
        let branch = git_stdout(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert!(
            branch.starts_with("snippet/"),
            "session worktree should be on snippet/{{id}}, got {branch}"
        );
        assert_ne!(branch, "HEAD", "must not be detached");
        drop_worktree(&repo, &workspace);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_git_folder_stays_put() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!("snippet-wt-plain-{stamp}"));
        fs::create_dir_all(&folder).unwrap();
        let got = prepare_new_session_workspace(&folder);
        assert_eq!(got, folder);
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn parallel_sessions_get_unique_worktrees() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "parallel");
        let a = prepare_new_session_workspace(&repo);
        let b = prepare_new_session_workspace(&repo);
        let root = crate::config::worktrees_root();
        assert_ne!(a, b);
        assert!(a.starts_with(&root) && b.starts_with(&root));
        assert!(a.join("README").exists() && b.join("README").exists());
        drop_worktree(&repo, &a);
        drop_worktree(&repo, &b);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn blank_session_in_git_repo_always_gets_a_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "mc-blank");
        let store = Store::open_in_memory().unwrap();
        // Mission Control often creates with new_conversation=false.
        let info = create_blank_session_in(&store, &repo, "from mc", false).unwrap();
        let root = crate::config::worktrees_root();
        let folder = PathBuf::from(&info.folder);
        assert_ne!(folder, repo);
        assert!(
            folder.starts_with(&root),
            "expected isolated worktree, got {}",
            folder.display()
        );
        assert!(folder.join(".git").is_file());
        let path = state_path_for_id(&info.id).expect("created session is resolvable");
        remove_session_files_with(Some(&store), &path);
        assert!(!folder.exists(), "isolated worktree should be gone");
        assert!(store.get_session_row(&info.id).unwrap().is_none());
        assert!(repo.exists(), "original clone must stay");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn deleting_a_session_drops_its_worktree() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = init_repo(stamp, "drop");
        let workspace = prepare_new_session_workspace(&repo);
        assert!(workspace.exists());
        let store = Store::open_in_memory().unwrap();
        let info = create_blank_session_in(&store, &workspace, "wt-drop", false).unwrap();
        let path = state_path_for_id(&info.id).expect("created session is resolvable");
        remove_session_files_with(Some(&store), &path);
        assert!(!workspace.exists(), "isolated worktree should be gone");
        assert!(repo.exists(), "original clone must stay");
        let _ = fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
mod resolve_session_path_tests {
    use super::*;
    use std::fs;

    /// A model handed `session:<ws>/state.json` routinely sends the bare
    /// The id IS the workspace directory — no filename. A caller that still
    /// appends `/state.json` (a value captured before ids were canonicalised)
    /// names the same session, and a bare name matches too: they all resolve to
    /// the one path, which is what makes the three forms interchangeable instead
    /// of three ways to be rejected.
    #[test]
    fn a_workspace_directory_is_the_default_session() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws-1");
        fs::create_dir_all(&ws).unwrap();

        let bare = resolve_session_path(root.path(), "ws-1").expect("resolves");
        assert_eq!(bare, ws, "got {}", bare.display());

        // The pre-canonical form names the same session rather than failing.
        assert_eq!(
            crate::conversations::canonical_session_id("ws-1/state.json").0,
            "ws-1"
        );
    }

    /// The explicit form must keep working — it is what the envelope sends.
    #[test]
    fn the_full_id_resolves_to_itself() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws-2");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("state.json"), b"{}").unwrap();

        let resolved = resolve_session_path(root.path(), "ws-2/state.json").expect("resolves");
        assert!(
            resolved.ends_with("ws-2/state.json"),
            "got {}",
            resolved.display()
        );
    }

    /// A saved conversation is not a directory, so the completion must not touch
    /// it. Regression guard: completing every id would rewrite this path.
    #[test]
    fn a_saved_conversation_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let conv = root.path().join("ws-3/conversations");
        fs::create_dir_all(&conv).unwrap();
        let file = conv.join("abc.json");
        fs::write(&file, b"{}").unwrap();

        let resolved =
            resolve_session_path(root.path(), "ws-3/conversations/abc.json").expect("resolves");
        assert!(
            resolved.ends_with("ws-3/conversations/abc.json"),
            "got {}",
            resolved.display()
        );
    }

    #[test]
    fn traversal_is_refused() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_session_path(root.path(), "../escape").is_none());
        assert!(resolve_session_path(root.path(), "/abs/path").is_none());
    }
}

#[cfg(test)]
mod conversation_name_tests {
    use super::conversation_name_from_id;

    /// A session under `conversations/` is a saved conversation; its name is the
    /// file stem.
    #[test]
    fn a_saved_conversation_is_named_by_its_file_stem() {
        assert_eq!(
            conversation_name_from_id("ws-1/conversations/41e5e18b-5478-4737-863f-750de39e025d"),
            "41e5e18b-5478-4737-863f-750de39e025d"
        );
        // The pre-canonical form (with `.json`) named the same conversation.
        assert_eq!(
            conversation_name_from_id("ws-1/conversations/41e5e18b.json"),
            "41e5e18b"
        );
    }

    /// Everything else is the workspace's one default session.
    ///
    /// This is the regression: a canonical id is the workspace DIRECTORY, with no
    /// `state.json` suffix. Deriving "default" from that suffix meant a root
    /// session reported the directory name as its conversation, so the TUI picker
    /// listed a workspace as though it were a saved conversation.
    #[test]
    fn a_root_session_is_the_default_conversation() {
        assert_eq!(
            conversation_name_from_id("snippet-service-61c2d836aee8dc5b"),
            "default"
        );
        assert_eq!(conversation_name_from_id("inbox-snippet"), "default");
        assert_eq!(conversation_name_from_id("mission-control"), "default");
        // And a value captured before the id change still names the same session.
        assert_eq!(
            conversation_name_from_id("snippet-service-61c2d836aee8dc5b/state.json"),
            "default"
        );
        assert_eq!(
            conversation_name_from_id("inbox-snippet/state.json"),
            "default"
        );
    }
}

#[cfg(test)]
mod routable_target_tests {
    use super::is_routable_target;

    /// An ordinary project session is the only thing work routes to.
    #[test]
    fn a_project_session_is_routable() {
        assert!(is_routable_target("snippet-service-61c2d836aee8dc5b"));
        assert!(is_routable_target(
            "wacht-480461c289235d72/conversations/e000736e-da39-4bd0-a307-52f52fc71241"
        ));
        // A pre-canonical id names the same session and must stay routable.
        assert!(is_routable_target("snippet-service-61c2d836aee8dc5b/state.json"));
    }

    /// The bug this predicate exists for: MC routed a real task into an agent's
    /// inbox, which runs the coordination runtime. It has no workspace tools and
    /// no `report_mission_task`, so the task could be neither done nor reported
    /// and sat InProgress forever while the inbox spun on it.
    #[test]
    fn an_agent_inbox_is_not_routable() {
        assert!(!is_routable_target("inbox-snippet"));
        assert!(!is_routable_target("inbox-snippet/state.json"));
        assert!(!is_routable_target("inbox-rust-pr-reviewer"));
    }

    /// Mission Control coordinates; routing work to itself is a loop.
    #[test]
    fn mission_control_is_not_routable() {
        assert!(!is_routable_target("mission-control"));
        assert!(!is_routable_target("mission-control/session.json"));
    }

    /// The two exclusions must not swallow a workspace that merely starts with
    /// the same letters — `inboxing-app-1234` is a real project folder.
    #[test]
    fn a_folder_named_like_an_inbox_stays_routable() {
        assert!(is_routable_target("inboxing-app-1234abcd"));
        assert!(is_routable_target("mission-control-ui-5678ef90"));
    }
}

#[cfg(test)]
mod working_agents_tests {
    use super::working_agents_in;
    use crate::coordination::{HandoffMode, Task, TaskStatus};
    use crate::store::Store;

    /// The roster row has a real FK to `agents(id)`, so the agent must exist.
    fn worker(id: &str) -> crate::coordination::types::Agent {
        crate::coordination::types::Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: crate::coordination::types::AgentKind::Worker,
            status: crate::coordination::types::AgentStatus::Active,
            role: crate::coordination::types::AgentRole::Implementer,
            capabilities: vec![],
        }
    }

    fn dispatched(id: &str, session: &str, now: &str) -> Task {
        let mut task = Task::dispatched_to(
            id.into(),
            session.into(),
            format!("task {id}"),
            "scope".into(),
            vec![],
            HandoffMode::Resume,
            "agent",
            crate::mission_control::SESSION_ID,
            now.into(),
        );
        task.profile = None;
        task
    }

    /// A plain project session is nobody's — `sessions.agent_id` is NULL — yet an
    /// agent dispatched into it is exactly who the session list must name. This
    /// is the case the badge missed: it read the session's own binding, which
    /// dispatch never writes.
    #[test]
    fn a_dispatched_agent_is_reported_for_its_target_session() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        let map = working_agents_in(&db);
        assert_eq!(
            map.get("proj-1").map(String::as_str),
            Some("snippet"),
            "the dispatched agent must be reported for the target session"
        );
    }

    /// Finished work is not someone working. A terminal task must drop out, or
    /// every session an agent ever touched would keep claiming a worker.
    #[test]
    fn a_finished_task_stops_reporting_a_worker() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();
        assert!(working_agents_in(&db).contains_key("proj-1"));

        for terminal in [TaskStatus::Done, TaskStatus::Failed, TaskStatus::Cancelled] {
            db.update_task_in("t1", "2026-01-02T00:00:00Z", |t| {
                t.status = terminal.clone()
            })
            .unwrap();
            assert!(
                !working_agents_in(&db).contains_key("proj-1"),
                "a {terminal:?} task must not report a worker"
            );
        }
    }

    /// The task row may carry the pre-canonicalisation id (`x/state.json`) while
    /// the catalog keys on `x`. Without canonicalising the key the lookup misses
    /// and the badge silently disappears for those sessions.
    #[test]
    fn a_legacy_shaped_target_still_matches_its_session() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "proj-1/state.json", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        assert_eq!(
            working_agents_in(&db).get("proj-1").map(String::as_str),
            Some("snippet"),
            "a legacy-shaped target must key to the canonical session id"
        );
    }

    /// A task with no target cannot be delivered, so it must not claim a worker.
    #[test]
    fn a_targetless_task_reports_nothing() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("snippet")).unwrap();
        let task = dispatched("t1", "", "2026-01-01T00:00:00Z");
        db.create_task(&task).unwrap();
        db.add_task_agent("t1", "snippet", "implementer", "2026-01-01T00:00:00Z")
            .unwrap();

        assert!(working_agents_in(&db).is_empty());
    }
}
