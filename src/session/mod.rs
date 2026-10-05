//! The single seam between a frontend and the agent: build the model + tools +
//! harness for a config and spawn the resident `run_interactive` loop. Drive it by
//! sending `LoopInput` on `input_tx`; observe it via the persisted `HarnessState`
//! (and, optionally, a live `StreamHandle`). Shared by the TUI and headless `serve`.

use std::path::{Path, PathBuf};
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
    /// Whether the project memory block and reflection apply. Only a session
    /// that works in a project has a project to remember.
    memory: bool,
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
    base_model: InferenceProfileConfig,
    setups: Option<std::collections::BTreeMap<String, InferenceProfileConfig>>,
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
        tools.insert(crate::builtins::ViewImageTool);
        // Mission Control may research current docs and issues while routing work.
        if let Some(key) = i.exa_api_key.clone().filter(|k| !k.trim().is_empty()) {
            tools.insert(crate::builtins::WebSearchTool { api_key: key.clone() });
            tools.insert(crate::builtins::WebReadTool { api_key: key });
        }
        crate::mission_tools::add_mission_control_tools(&mut tools);
        crate::coordination_tools::add_coordination_tools(&mut tools);

        Ok(Self {
            factory: None,
            context,
            tools,
            prompt: mission_control_system_prompt(),
            allow_lane_control: false,
            memory: false,
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

        let custom_dir = crate::agent_tools::tools_dir(identity.map_or("snippet", |(agent_id, _)| agent_id));
        if let Some(dir) = custom_dir.as_ref() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut tools = coding_tools(i.exa_api_key.clone()).with_custom_dir(custom_dir.clone());
        crate::mission_tools::add_worker_report_tool(&mut tools);
        tools.insert(crate::mission_tools::CreateRecurringJob);
        crate::coordination_tools::add_coordination_tools(&mut tools);

        let prompt_ctx = i.prompt_ctx;
        let custom_section = custom_dir.as_deref().map(crate::agent_tools::prompt_section);
        let prompt = match identity {
            Some((agent_id, body)) => crate::prompts::specialized_agent_system_prompt(
                crate::prompts::SpecializedAgentPromptContext {
                    agent_id,
                    identity: body,
                    context: &prompt_ctx,
                },
            ),
            None => conversation_prompt(&prompt_ctx),
        };
        let prompt = match custom_section {
            Some(section) => format!("{prompt}\n\n{section}"),
            None => prompt,
        };

        let base_model = i.base_model;
        let setups = i.setups;
        let lane_session_id = i.durable_id;
        Ok(Self {
            factory: Some(Arc::new(move |profile: Option<&str>| {
                if let Some(name) = profile {
                    if let Some(cfg) = setups.as_ref().and_then(|s| s.get(name)) {
                        return Ok(cfg.build_model_for_session(lane_session_id.clone()));
                    } else {
                        let known = setups
                            .as_ref()
                            .map(|s| s.keys().cloned().collect::<Vec<_>>())
                            .unwrap_or_default();
                        return Err(format!(
                            "unknown inference profile `{name}`. Available setups: [{}]",
                            known.join(", ")
                        ));
                    }
                }
                Ok(base_model.build_model_for_session(lane_session_id.clone()))
            })),
            context,
            tools,
            prompt,
            allow_lane_control: true,
            memory: true,
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
            memory: false,
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
    let setups = config.setups.clone();
    let exa_api_key = config.exa_api_key.clone();
    let manual_approval = config.manual_approval;
    let context_window_tokens = model_config.context_window;
    let compact_at_pct = model_config.compact_at_pct;
    let memory_enabled = config.memory_enabled;
    let memory_reflect = config.memory_reflect;
    let sp = state_path.clone();
    let stream_out = stream.clone();

    let join = tokio::spawn(async move {
        let durable_id = Some(session_id_for_state_path(&sp));
        let mut model = model_config.build_model_for_session(durable_id.clone());
        let cli_profile = model.cli_agent_profile();
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
                base_model: model_config,
                setups,
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
                memory_enabled: memory_enabled && runtime.memory,
                memory_reflect,
                allow_lane_control: runtime.allow_lane_control,
                ..HarnessConfig::default()
            },
            runtime.tools,
            runtime.context,
        );
        if let Some(profile) = cli_profile {
            return harness
                .run_cli_agent(profile, initial, rx, runtime.factory, stream)
                .await
                .map_err(|e| e.to_string());
        }
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
    /// For a session in a git worktree: the folder in the main checkout it was
    /// made from, which is what a person recognises the session by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_folder: Option<String>,
    /// The worktree's branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
pub fn is_inbox_session_id(id: &str) -> bool {
    id.strip_prefix("inbox-")
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
        let session = crate::conversations::canonical_session_id(&task.session_id).0;
        let Ok(members) = store.list_task_agents(&task.id) else {
            continue;
        };
        if let Some(agent) = members
            .iter()
            .find(|member| member.removed_at.is_none() && member.status == "active")
            .or_else(|| members.iter().find(|member| member.removed_at.is_none()))
            .map(|member| member.agent_id.clone())
        {
            out.entry(session).or_insert(agent);
        }
    }
    out
}

/// Every session the store holds, unfiltered — INCLUDING agent inboxes.
///
/// Only usage accounting wants this. An inbox runs a real model, so its tokens
/// are real spend; dropping it from the totals would under-report what the
/// device actually used. Everything that presents sessions to a person uses
/// [`list_device_sessions`] instead.
pub fn all_device_sessions() -> Vec<SessionInfo> {
    let workers = working_agents_by_session();
    let mut out: Vec<SessionInfo> = store_for_sessions()
        .and_then(|store| store.list_all_sessions().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let origin = worktree_origin(Path::new(&row.workspace));
            SessionInfo {
                conversation: conversation_name_from_id(&row.id).to_string(),
                // Read before `row.id` is moved out below — struct-literal
                // fields evaluate in the order written.
                worker_agent_id: workers.get(&row.id).cloned(),
                id: row.id,
                folder: row.workspace,
                title: row.title.unwrap_or_default(),
                status: row.status,
                last_active: row.last_active.unwrap_or(0),
                agent_id: row.agent_id,
                origin_folder: origin.as_ref().map(|o| o.folder.display().to_string()),
                branch: origin.and_then(|o| o.branch),
            }
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

/// The session catalog as a PERSON sees it: no agent inboxes.
///
/// An inbox is an agent's private mailbox — the session it answers direct
/// messages in. It is not a chat anyone chose, resumed, renamed or deleted, and
/// listing it put the agent's private correspondence in the user's chat list and
/// made the app's per-folder badge count a folder the user never opened. It
/// stays reachable where it belongs: addressing the agent by name, and
/// `state_path_for_id`, which is how delivery and `/attach` resolve it.
pub fn list_device_sessions() -> Vec<SessionInfo> {
    all_device_sessions()
        .into_iter()
        .filter(|s| !is_inbox_session_id(&s.id))
        .collect()
}

/// The conversation name a session id encodes.
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


pub mod fork;
pub use fork::*;

/// Settle the work a worker session stopped doing without reporting.
///
/// A dispatched session that dies, or finishes its turn, without calling
/// `report_mission_task` leaves its task looking active forever, so this is the
/// one place that notices: a failure parks the task, an idle stop tells
/// Mission Control once.
pub fn park_failed_session_work(id: &str, prev_status: &str, state: &HarnessState) {
    let status = status_str(state.status);
    let lanes_running = state
        .lanes
        .iter()
        .any(|lane| lane.status == crate::lanes::LaneStatus::Running);
    let waiting_on_watch = !state.watches.is_empty();
    if status == "idle" && prev_status == "running" && !lanes_running && !waiting_on_watch {
        if let Some(store) = store_for_sessions() {
            let _ = store.flag_unreported_tasks(id, &chrono::Utc::now().to_rfc3339());
        }
        return;
    }
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

/// A session's scalar state (status, workspace, queue, lanes, …) without its
/// message and event logs — for callers that only need metadata.
pub fn read_session_meta(state_path: &Path) -> Option<HarnessState> {
    let id = session_id_for_state_path(state_path);
    if let Some(store) = store_for_sessions() {
        if store.has_conversation(&id).ok()? {
            let scalar = store.load_session_scalar(&id).ok()??;
            return crate::harness::state_from_scalar(&scalar, Vec::new(), Vec::new()).ok();
        }
    }
    read_session_state(state_path)
}

pub fn read_session_state_tail(state_path: &Path, from: usize) -> Option<(HarnessState, usize)> {
    let id = session_id_for_state_path(state_path);
    let Some(store) = store_for_sessions() else {
        return read_session_state(state_path).map(|mut s| {
            let count = s.events.len();
            s.messages.clear();
            s.events.drain(..from.min(count));
            (s, count)
        });
    };
    if !store.has_conversation(&id).ok()? {
        return read_session_state(state_path).map(|mut s| {
            let count = s.events.len();
            s.messages.clear();
            s.events.drain(..from.min(count));
            (s, count)
        });
    }
    let scalar = store.load_session_scalar(&id).ok()??;
    let (events, count) = store.load_conversation_events_from(&id, from).ok()?;
    let state = crate::harness::state_from_scalar(&scalar, Vec::new(), events).ok()?;
    Some((state, count))
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

#[cfg(test)]
mod notification_tests;
#[cfg(test)]
mod notification_wire_tests;
#[cfg(test)]
mod notification_paging_tests;
#[cfg(test)]
mod notification_attention_tests;
#[cfg(test)]
mod notification_stop_tests;

const NOTIFICATION_RETENTION_SECS: i64 = 24 * 60 * 60;

pub fn emit_device_event(event: serde_json::Value) {
    emit_device_event_with_store(event, store_for_sessions().as_ref(), device_events());
}

fn emit_device_event_with_store(
    event: serde_json::Value,
    store: Option<&crate::store::Store>,
    events: &broadcast::Sender<serde_json::Value>,
) {
    if let Some(store) = store {
        if let Some(mut notification) = notification_candidate(&event) {
            if notification["kind"] == "waiting" {
                let attention = notification["session"].as_str()
                    .and_then(|id| store.current_attention(id).ok().flatten());
                let Some(attention) = attention else {
                    let _ = events.send(event);
                    return;
                };
                notification["attention"] = attention;
            }
            if notification["kind"] == "idle" {
                let identity = notification["session"].as_str()
                    .and_then(|id| store.current_stop_identity(id).ok().flatten());
                let Some(identity) = identity else {
                    let _ = events.send(event);
                    return;
                };
                notification["stop_identity"] = identity.into();
            }
            if let Ok(settings) = store.load_control_settings() {
                if notification_allowed(&settings, &notification) {
                    match store.append_notification_event(notification, NOTIFICATION_RETENTION_SECS) {
                        Ok(saved) => { let _ = events.send(serde_json::json!({
                            "kind": "notification", "notification": saved
                        })); }
                        Err(error) => eprintln!("persist notification: {error}"),
                    }
                }
            }
        }
    }
    let _ = events.send(event);
}

fn notification_candidate(event: &serde_json::Value) -> Option<serde_json::Value> {
    let kind = event.get("kind")?.as_str()?;
    let mut value = event.clone();
    if matches!(kind, "done" | "idle")
        && event.get("session").and_then(|s| s.as_str()) == Some(crate::mission_control::SESSION_ID)
        && crate::mission_autonomy::is_on()
    {
        return None;
    }
    if matches!(kind, "waiting" | "done" | "error" | "idle" | "ping") {
        let session = event.get("session")?.as_str()?;
        value["destination"] = serde_json::json!({"type": "session", "id": session});
    } else if kind == "coordination_event" {
        let source = event.get("event")?;
        let event_type = source.get("event_type")?.as_str()?;
        let destination = match event_type {
            "direct_message.sent" => {
                let recipient = source["payload"]["recipient"].as_str()?;
                let (recipient_type, id) = recipient.split_once(':')?;
                if recipient_type != "session" {
                    return None;
                }
                serde_json::json!({"type": "session", "id": id})
            }
            "task.message" => {
                let id = source.get("correlation_id")?.as_str()?;
                serde_json::json!({"type": "task", "id": id, "session": crate::mission_control::SESSION_ID})
            }
            _ => return None,
        };
        value = serde_json::json!({
            "kind": event_type, "destination": destination,
            "source_key": source["event_id"], "payload": source["payload"]
        });
        if event_type == "direct_message.sent" {
            value["thread_id"] = source["thread_id"].clone();
        }
    } else {
        return None;
    }
    value.as_object_mut()?.remove("notify");
    Some(value)
}

fn notification_attention_current(store: &crate::store::Store, event: &serde_json::Value) -> bool {
    if event["kind"] == "idle" {
        let Some(session) = event["session"].as_str() else { return false; };
        return store.current_stop_identity(session).ok().flatten()
            .is_some_and(|identity| event["stop_identity"].as_str() == Some(identity.as_str()));
    }
    if event["kind"] != "waiting" { return true; }
    let Some(session) = event["session"].as_str() else { return false; };
    store.current_attention(session).ok().flatten()
        .is_some_and(|attention| event["attention"] == attention)
}

fn notification_allowed(settings: &crate::mission_control::ControlSettings, event: &serde_json::Value) -> bool {
    let destination = &event["destination"];
    let session = if destination["type"] == "session" {
        destination["id"].as_str()
    } else { destination["session"].as_str() };
    match settings.notification_policy.as_str() {
        "all_sessions" => true,
        "mission_control_only" => session.is_some_and(|id| {
            id == crate::mission_control::SESSION_ID
                || settings.mission_control_session_id.as_deref() == Some(id)
        }),
        _ => false,
    }
}

fn notify_kind(prev: &str, status: &str) -> Option<&'static str> {
    if prev == status {
        return None;
    }
    match status {
        "running" => Some("running"),
        "waiting_for_input" => Some("waiting"),
        "failed" => Some("error"),
        "completed" => Some("done"),
        "idle" if prev == "running" => Some("idle"),
        _ => None,
    }
}

pub fn emit_status_transition(
    session_id: &str,
    prev_status: &str,
    status: &str,
    title: Option<&str>,
    workspace: &str,
) {
    if let Some(kind) = notify_kind(prev_status, status) {
        emit_device_event(serde_json::json!({
            "session": session_id,
            "title": title.unwrap_or_default(),
            "workspace": workspace,
            "kind": kind,
            "status": status,
        }));
    }
}

pub fn replay_notification_events_after_cursor(created_at: i64, event_id: u64, limit: usize) -> Result<serde_json::Value, String> {
    let store = store_for_sessions().ok_or("notification store unavailable")?;
    let settings = store.load_control_settings().map_err(|e| e.to_string())?;
    notification_tuple_page(&store, &settings, created_at, event_id, limit)
}

fn notification_tuple_page(store: &crate::store::Store, settings: &crate::mission_control::ControlSettings, created_at: i64, event_id: u64, limit: usize) -> Result<serde_json::Value, String> {
    let mut rows = store.notification_events_after_cursor(created_at, event_id).map_err(|e| e.to_string())?;
    let more_rows = rows.len() > 500;
    rows.truncate(500);
    let mut cursor = serde_json::json!({"created_at": created_at, "event_id": event_id});
    let now = chrono::Utc::now().timestamp();
    let mut events = Vec::new();
    let mut has_more = more_rows;
    for row in rows {
        let eligible = row.get("notification_id").is_some()
            && row["created_at"].as_i64().is_some_and(|ts| ts >= now - NOTIFICATION_RETENTION_SECS)
            && row["expires_at"].as_i64().is_some_and(|expiry| expiry > now)
            && notification_allowed(settings, &row)
            && notification_attention_current(store, &row);
        if eligible && events.len() == limit.clamp(1, 500) {
            has_more = true;
            break;
        }
        cursor = serde_json::json!({"created_at": row["created_at"], "event_id": row["event_id"]});
        if eligible { events.push(row); }
    }
    Ok(serde_json::json!({"events": events, "next_cursor": cursor, "has_more": has_more}))
}

pub fn replay_notification_events(since: u64, limit: usize) -> Result<serde_json::Value, String> {
    let store = store_for_sessions().ok_or("notification store unavailable")?;
    let settings = store.load_control_settings().map_err(|e| e.to_string())?;
    notification_page_from_store(&store, &settings, since, limit)
}

fn notification_page_from_store(
    store: &crate::store::Store,
    settings: &crate::mission_control::ControlSettings,
    since: u64,
    limit: usize,
) -> Result<serde_json::Value, String> {
    let mut rows = store.notification_events_since(since, 501).map_err(|e| e.to_string())?;
    let more_rows = rows.len() > 500;
    rows.truncate(500);
    let rows = rows.into_iter().map(|mut row| {
        if !notification_attention_current(store, &row) {
            row.as_object_mut().map(|object| object.remove("notification_id"));
        }
        row
    }).collect();
    let mut page = notification_page(rows, settings, since, limit);
    if more_rows {
        page["has_more"] = serde_json::json!(true);
    }
    Ok(page)
}

fn notification_page(rows: Vec<serde_json::Value>, settings: &crate::mission_control::ControlSettings, since: u64, limit: usize) -> serde_json::Value {
    let limit = limit.clamp(1, 500);
    let now = chrono::Utc::now().timestamp();
    let mut events = Vec::new();
    let mut cursor = since;
    let mut has_more = false;
    for row in rows {
        let id = row["event_id"].as_u64().unwrap_or(cursor);
        let eligible = row.get("notification_id").is_some()
            && row["expires_at"].as_i64().is_some_and(|expiry| expiry > now)
            && notification_allowed(settings, &row);
        if eligible && events.len() == limit { has_more = true; break; }
        cursor = id;
        if eligible { events.push(row); }
    }
    serde_json::json!({"events": events, "next_cursor": cursor, "has_more": has_more})
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

pub mod worktree;
pub use worktree::*;

#[cfg(test)]
mod tests;

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
    mode: WorkspaceMode,
) -> Result<SessionInfo, String> {
    let store =
        store_for_sessions().ok_or_else(|| "the session store is unavailable".to_string())?;
    create_blank_session_in(&store, folder, title, new_conversation, mode)
}

/// [`create_blank_session`] against an explicit store, so tests can use an
/// in-memory one instead of writing fixtures into the real database.
pub fn create_blank_session_in(
    store: &crate::store::Store,
    folder: &Path,
    title: &str,
    new_conversation: bool,
    mode: WorkspaceMode,
) -> Result<SessionInfo, String> {
    let mut folder = folder
        .canonicalize()
        .map_err(|e| format!("folder is not a directory: {e}"))?;
    if !folder.is_dir() {
        return Err("folder is not a directory".into());
    }
    // The workspace, not the session's storage — the two are independent.
    folder = session_workspace(&folder, mode);
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

    let origin = worktree_origin(&folder);
    Ok(SessionInfo {
        id,
        folder: folder.display().to_string(),
        origin_folder: origin.as_ref().map(|o| o.folder.display().to_string()),
        branch: origin.and_then(|o| o.branch),
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
        // A worktree can hold several sessions once one is started in an
        // existing worktree; it goes with the last of them.
        let shared = store.is_some_and(|s| worktree_has_other_sessions(s, &id, &folder));
        if !shared {
            drop_session_worktree(&folder);
        }
    }
    if let Some(store) = store {
        let _ = store.delete_conversation(&id);
    }
}

/// Whether a session other than `id` works inside the worktree holding `folder`.
fn worktree_has_other_sessions(store: &crate::store::Store, id: &str, folder: &Path) -> bool {
    let Some(worktree) = linked_worktree_root(folder) else {
        return false;
    };
    store.list_all_sessions().is_ok_and(|rows| {
        rows.iter().any(|row| {
            row.id != id
                && linked_worktree_root(Path::new(&row.workspace)).as_deref()
                    == Some(worktree.as_path())
        })
    })
}

fn workspace_from_state_file(state_path: &Path) -> Option<PathBuf> {
    let state = read_session_state(state_path)?;
    if state.workspace.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(state.workspace))
    }
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
