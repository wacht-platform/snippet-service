use std::path::PathBuf;

use clap::{Parser, Subcommand};
use snippet::config::SnippetConfig;
use snippet::serve::{self};
use snippet::tui::{TuiOptions, run_tui};

#[derive(Debug, Parser)]
#[command(name = "snippet", version)]
#[command(about = "A Rust coding-agent harness with a durable TUI runtime.")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Resume a specific conversation by id (the command is printed when a session closes).
    #[arg(long)]
    resume: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the installed Snippet version.
    Version,
    /// Run headless in the background and serve the agent for remote control (mobile
    /// app). Additive — the TUI is unchanged; both use the same on-disk sessions.
    Serve {
        /// Local port to bind (a tunnel exposes it; the token is the auth gate).
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Auth token; generated if omitted.
        #[arg(long)]
        token: Option<String>,
        /// Bind localhost only, no tunnel (testing — serve is otherwise remote-only).
        #[arg(long)]
        no_tunnel: bool,
        /// Advertise this public URL and launch no tunnel — bring-your-own (run your
        /// own cloudflared/named tunnel as a service, pointed at the local port).
        #[arg(long)]
        public_url: Option<String>,
        /// How to reach the daemon remotely: auto (default: Tailscale if connected,
        /// else cloudflared if installed, else local only), tailscale (tailnet only),
        /// funnel (Tailscale Funnel, public), cloudflare (quick tunnel), or none.
        /// Snippet never installs these; set one up yourself.
        #[arg(long)]
        tunnel: Option<String>,
        /// Bind address. Use 0.0.0.0 to expose on the box's public IP for a fixed,
        /// tunnel-less URL (pair with --public-url http://<ip>:<port>). Default localhost.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Stop the running background daemon.
        #[arg(long)]
        stop: bool,
        /// Show the running daemon's status + its QR / connection string.
        #[arg(long)]
        status: bool,
        /// Restart the running daemon (stop then start on the new binary).
        #[arg(long)]
        restart: bool,
        /// Install as an OS service that auto-starts on boot/login (launchd on macOS,
        /// systemd --user on Linux), baking in the other flags you pass here.
        #[arg(long)]
        enable: bool,
        /// Remove the auto-start service installed by --enable.
        #[arg(long)]
        disable: bool,
        /// Internal: kept for backward compat with existing service files
        /// (systemd/launchd pass this). Ignored — serve is always foreground now.
        #[arg(long, hide = true)]
        supervised: bool,
    },
    /// Manage the secret vault (~/.snippet/vault.json). Secrets are usable by the
    /// agent as $NAME in shell commands; values are injected into the child
    /// process and redacted from everything the model sees.
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// Search and read the archived history of an agent session — the messages
    /// compaction moved out of its context. Scoped to $SNIPPET_SESSION_ID (set in
    /// the agent's shell) unless --session is given.
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Message internal agents and give one work in a session, through the
    /// running serve daemon. The daemon owns delivery, so a message accepted here
    /// is still delivered after a restart.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Check external tools and voice assets, and show how to install what is missing.
    Doctor {
        /// Machine output: one tab-separated line per missing item (id, required|optional, purpose, command).
        #[arg(long)]
        tsv: bool,
        /// Package manager to give install commands for (detected when omitted).
        #[arg(long, value_enum)]
        pm: Option<snippet::doctor::Pm>,
        /// Download and verify the voice runtime and model now.
        #[arg(long)]
        prefetch: bool,
    },
    #[command(hide = true)]
    AgyHook,
}

#[derive(Debug, Subcommand)]
enum AgentAction {
    /// List the agents in the coordination directory.
    List {
        /// Print the response as raw JSON.
        /// Print the response as raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Send a direct message to an agent.
    Message {
        /// Recipient agent id, as shown by `snippet agent list`.
        agent_id: String,
        /// The message body. Omit to read it from stdin.
        body: Vec<String>,
        /// Send as this agent instead of the local human.
        #[arg(long)]
        from_agent: Option<String>,
        /// A retry with the same key does not duplicate the message.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Read a direct conversation, oldest first.
    Thread {
        /// The peer agent id (or `human` for the local human).
        peer: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Show only messages after this sequence.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// The local participant: `human` (default) or an agent id.
        #[arg(long)]
        as_actor: Option<String>,
        /// Print the response as raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// List your direct conversations with unread counts.
    Inbox {
        /// The local participant: `human` (default) or an agent id.
        #[arg(long)]
        as_actor: Option<String>,
        /// Print the response as raw JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HistoryAction {
    /// Full-text search, best matches first.
    Search {
        /// Words to look for.
        query: String,
        /// Maximum number of matches.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Session to search (defaults to $SNIPPET_SESSION_ID).
        #[arg(long)]
        session: Option<String>,
    },
    /// Print archived messages in full, by id or as an id range.
    Show {
        /// Message ids from `snippet history search`.
        ids: Vec<i64>,
        /// First id of a range.
        #[arg(long)]
        from: Option<i64>,
        /// Last id of a range.
        #[arg(long)]
        to: Option<i64>,
        /// Session to read (defaults to $SNIPPET_SESSION_ID).
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum VaultAction {
    /// Store a secret. The value is read from stdin (piped, or typed with echo
    /// off on a TTY) so it never lands in shell history or process args.
    Set { name: String },
    /// Remove a secret.
    Rm { name: String },
    /// List secret names (never values).
    Ls,
}

fn vault_cli(action: VaultAction) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, IsTerminal, Write};
    let mut vault = snippet::vault::Vault::load();
    match action {
        VaultAction::Set { name } => {
            let value = if std::io::stdin().is_terminal() {
                print!("value for {name} (input hidden): ");
                std::io::stdout().flush()?;
                let value = rpassword_read()?;
                println!();
                value
            } else {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                line
            };
            vault.set(&name, value.trim())?;
            println!(
                "✓ stored `{name}` ({} secret{} in vault)",
                vault.names().len(),
                if vault.names().len() == 1 { "" } else { "s" }
            );
        }
        VaultAction::Rm { name } => {
            if vault.remove(&name)? {
                println!("✓ removed `{name}`");
            } else {
                println!("no secret named `{name}`");
            }
        }
        VaultAction::Ls => {
            let names = vault.names();
            if names.is_empty() {
                println!("vault is empty — add one with: snippet vault set NAME");
            } else {
                for n in names {
                    println!("{n}");
                }
            }
        }
    }
    Ok(())
}

/// Create and inspect the snippet store.
///
/// Opening the database is what creates it — `Store::open` runs the
/// schema migration — so `init` is deliberately just an open plus a report, not a
/// separate creation path that could drift from what the daemon does at startup.
fn history_cli(action: HistoryAction) -> Result<(), Box<dyn std::error::Error>> {
    use snippet::history_archive::{recall_turn_range, recall_turns, render_turn, search_history};
    let session = |explicit: Option<String>| {
        explicit
            .or_else(|| std::env::var("SNIPPET_SESSION_ID").ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or("no session: pass --session <id> (inside an agent shell $SNIPPET_SESSION_ID is set)")
    };
    let store = snippet::store::Store::open(snippet::store::default_db_path())?;
    match action {
        HistoryAction::Search {
            query,
            limit,
            session: explicit,
        } => {
            let hits = search_history(&store, &session(explicit)?, &query, limit.clamp(1, 50))?;
            if hits.is_empty() {
                println!("no matches for {query:?}");
            }
            for hit in hits {
                let who = hit.tool_name.as_deref().unwrap_or(&hit.role);
                println!("#{}  {who}  {}", hit.archive_id, hit.summary);
                let snippet = hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                if !snippet.is_empty() {
                    println!("    {snippet}");
                }
            }
        }
        HistoryAction::Show {
            ids,
            from,
            to,
            session: explicit,
        } => {
            let session = session(explicit)?;
            let turns = match (from, to) {
                (Some(from), Some(to)) => recall_turn_range(&store, &session, from, to, 50)?,
                (Some(_), None) | (None, Some(_)) => {
                    return Err("pass both --from and --to for a range".into());
                }
                (None, None) if ids.is_empty() => {
                    return Err("pass message ids, or --from and --to".into());
                }
                (None, None) => recall_turns(&store, &session, &ids)?,
            };
            if turns.is_empty() {
                println!("no archived messages with those ids in this session");
            }
            for turn in turns {
                println!("{}", render_turn(&turn));
            }
        }
    }
    Ok(())
}

/// The CLI's acting participant: the local human unless an agent is named.
///
/// The daemon refuses an unknown sender, so naming an agent here can only speak
/// as an identity the directory already knows.
fn cli_actor(as_actor: Option<&str>) -> (String, String) {
    match as_actor.map(str::trim).filter(|actor| !actor.is_empty()) {
        None | Some("human") | Some("local") => ("human".to_string(), "local".to_string()),
        Some(agent) => ("agent".to_string(), agent.to_string()),
    }
}

/// Resolve a `thread <peer>` argument to a participant reference. `human` is the
/// local human, which is what an agent uses to read its conversation with you.
fn peer_actor(peer: &str) -> (String, String) {
    match peer.trim() {
        "human" | "local" => ("human".to_string(), "local".to_string()),
        other => ("agent".to_string(), other.to_string()),
    }
}

fn print_json(value: &serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Call one daemon route, authenticating with the published token.
async fn daemon_http(
    state: &ServeState,
    method: &str,
    route: &str,
    query: &[(&str, String)],
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let mut url = serve_route_url(state.base_url(), route)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("token", &state.token);
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    let request = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        other => return Err(format!("unsupported daemon method `{other}`").into()),
    };
    let request = match body {
        Some(value) => request.json(&value),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    let value = serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|_| serde_json::json!({ "error": text }));
    if !status.is_success() {
        // An error body is a bare string, not an object, so it is read directly
        // before falling back to an `error` field.
        let detail = value
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(text);
        return Err(format!("daemon returned {status}: {detail}").into());
    }
    Ok(value)
}

/// Message internal agents and dispatch work, over the running daemon.
///
/// These commands go through the daemon rather than writing SQLite directly: the
/// daemon owns delivery, wake-up, and the retry ledger, so a message this CLI
/// accepts is still delivered after a restart.
async fn agent_cli(action: AgentAction) -> Result<(), Box<dyn std::error::Error>> {
    let state = serve_connection().map_err(|error| {
        format!(
            "{error}\n\nis the daemon running? start it with `snippet serve`, then check `snippet serve --status`"
        )
    })?;
    match action {
        AgentAction::List { json } => {
            let value = daemon_http(&state, "GET", "agents", &[], None).await?;
            if json {
                return print_json(&value);
            }
            let agents = value.as_array().cloned().unwrap_or_default();
            if agents.is_empty() {
                println!("no agents in the directory");
            }
            for agent in agents {
                let id = agent.get("id").and_then(serde_json::Value::as_str).unwrap_or("?");
                let name = agent
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(id);
                let kind = agent.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
                let role = agent.get("role").and_then(serde_json::Value::as_str).unwrap_or("");
                let status = agent
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                println!("{id:<24} {name:<20} {kind:<14} {role:<12} {status}");
            }
            Ok(())
        }
        AgentAction::Message {
            agent_id,
            body,
            from_agent,
            idempotency_key,
        } => {
            let body = if body.is_empty() {
                use std::io::Read;
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                text.trim().to_string()
            } else {
                body.join(" ")
            };
            if body.trim().is_empty() {
                return Err("message body must not be empty".into());
            }
            let (from_kind, from_id) = cli_actor(from_agent.as_deref());
            let mut payload = serde_json::json!({
                "from_kind": from_kind,
                "from_id": from_id,
                "to_kind": "agent",
                "to_id": agent_id,
                "body": body.trim(),
            });
            if let Some(key) = idempotency_key {
                payload["idempotency_key"] = serde_json::json!(key);
            }
            let value = daemon_http(
                &state,
                "POST",
                "coordination/direct/messages",
                &[],
                Some(payload),
            )
            .await?;
            let thread = value
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let sequence = value
                .get("sequence")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            println!("✓ sent to {agent_id} (thread {thread}, seq {sequence})");
            Ok(())
        }
        AgentAction::Thread {
            peer,
            limit,
            after,
            as_actor,
            json,
        } => {
            let (actor_kind, actor_id) = cli_actor(as_actor.as_deref());
            let (peer_kind, peer_id) = peer_actor(&peer);
            let value = daemon_http(
                &state,
                "GET",
                "coordination/direct/messages",
                &[
                    ("actor_kind", actor_kind),
                    ("actor_id", actor_id),
                    ("peer_kind", peer_kind),
                    ("peer_id", peer_id),
                    ("after_sequence", after.to_string()),
                    ("limit", limit.to_string()),
                ],
                None,
            )
            .await?;
            if json {
                return print_json(&value);
            }
            let events = value
                .get("events")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            if events.is_empty() {
                println!("no messages yet");
            }
            for event in events {
                let sequence = event
                    .get("sequence")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let kind = event
                    .get("actor_kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let id = event
                    .get("actor_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let at = event
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let body = event
                    .pointer("/payload/body")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                println!("[{sequence}] {kind}:{id}  {at}\n  {body}");
            }
            Ok(())
        }
        AgentAction::Inbox { as_actor, json } => {
            let (actor_kind, actor_id) = cli_actor(as_actor.as_deref());
            let value = daemon_http(
                &state,
                "GET",
                "coordination/direct/threads",
                &[("actor_kind", actor_kind), ("actor_id", actor_id)],
                None,
            )
            .await?;
            if json {
                return print_json(&value);
            }
            let threads = value.as_array().cloned().unwrap_or_default();
            if threads.is_empty() {
                println!("no conversations yet");
            }
            for thread in threads {
                let title = thread
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let peer = thread
                    .get("peer_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let unread = thread
                    .get("unread")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let badge = if unread > 0 {
                    format!("{unread} unread")
                } else {
                    "read".to_string()
                };
                println!("{title:<40} peer={peer:<16} {badge}");
            }
            Ok(())
        }
    }
}

/// Read a line from the TTY with echo disabled (crossterm raw mode) — no extra
/// password-prompt dependency needed.
fn rpassword_read() -> Result<String, Box<dyn std::error::Error>> {
    use crossterm::event::{Event, KeyCode, KeyModifiers, read};
    crossterm::terminal::enable_raw_mode()?;
    let mut value = String::new();
    let result = loop {
        match read()? {
            Event::Key(k) => match k.code {
                KeyCode::Enter => break Ok(value.clone()),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("aborted".into());
                }
                KeyCode::Char(c) => value.push(c),
                _ => {}
            },
            _ => {}
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    result
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".snippet/config.toml")
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, Box<dyn std::error::Error>> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

#[derive(Debug, serde::Deserialize)]
struct ServeState {
    url: String,
    token: String,
    /// The daemon's local address. Preferred by the CLI: a CLI on the same
    /// machine should not reach its own daemon through the public tunnel.
    #[serde(default)]
    api_url: Option<String>,
}

impl ServeState {
    /// The base URL the CLI talks to: the local address when published, else the
    /// public one.
    fn base_url(&self) -> &str {
        self.api_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(&self.url)
    }
}

fn serve_state_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".snippet/serve.json")
}

fn serve_connection() -> Result<ServeState, Box<dyn std::error::Error>> {
    let path = serve_state_path();
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let state: ServeState =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if state.url.trim().is_empty() || state.token.trim().is_empty() {
        return Err(format!("{} has no usable daemon connection", path.display()).into());
    }
    Ok(state)
}

fn serve_route_url(
    base_url: &str,
    route: &str,
) -> Result<reqwest::Url, Box<dyn std::error::Error>> {
    let mut url = reqwest::Url::parse(base_url.trim())?;
    url.set_query(None);
    url.set_fragment(None);
    let prefix = url.path().trim_end_matches('/');
    let path = if prefix.is_empty() {
        format!("/{route}")
    } else {
        format!("{prefix}/{route}")
    };
    url.set_path(&path);
    Ok(url)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = config_path(&cli);

    match cli.command {
        Some(Command::Version) => {
            println!("{}", snippet::update::CURRENT_VERSION);
            Ok(())
        }
        Some(Command::Vault { action }) => return vault_cli(action),
        Some(Command::History { action }) => return history_cli(action),
        Some(Command::Agent { action }) => return runtime()?.block_on(agent_cli(action)),
        Some(Command::AgyHook) => return Ok(snippet::antigravity::run_hook()?),
        Some(Command::Doctor { tsv, pm, prefetch }) => {
            snippet::doctor::run(tsv, pm, prefetch);
            Ok(())
        }
        Some(Command::Serve {
            port,
            token,
            no_tunnel,
            public_url,
            tunnel,
            host,
            stop,
            status,
            restart,
            enable,
            disable,
            supervised: _,
        }) => {
            if stop {
                return serve::stop().map_err(Into::into);
            }
            if restart {
                serve::stop().ok(); // best-effort: may not be running
                // Fall through to launch a fresh daemon below.
            }
            if status {
                return serve::status().map_err(Into::into);
            }
            if disable {
                return serve::uninstall_service().map_err(Into::into);
            }
            if enable {
                return serve::install_service(
                    &host,
                    port,
                    token.as_deref(),
                    no_tunnel,
                    public_url.as_deref(),
                    tunnel.as_deref(),
                    &config_path,
                )
                .map_err(Into::into);
            }
            let token = serve::resolve_token(token);
            // Always run foreground. Background via `nohup snippet serve &` or
            // `snippet serve --enable` (systemd/launchd). Settings from serve.toml
            // (written by --enable) override CLI defaults when present.
            let s = serve::ServeSettings::load();
            let host = s.host.unwrap_or(host);
            let port = s.port.unwrap_or(port);
            let no_tunnel = no_tunnel || s.no_tunnel;
            let public_url = public_url.or(s.public_url);
            let tunnel = serve::resolve_tunnel(
                tunnel.or(s.tunnel).as_deref(),
                no_tunnel,
                public_url.clone(),
            )?;
            serve::write_own_pidfile();
            let supervised = true; // treat all foreground as supervised
            runtime()?.block_on(async {
                let config = SnippetConfig::load(&config_path).await?;
                serve::run_serve(config, config_path, &host, port, token, tunnel, supervised)
                    .await
                    .map_err::<Box<dyn std::error::Error>, _>(Into::into)
            })
        }
        None => runtime()?.block_on(async {
            let config = SnippetConfig::load(&config_path).await?;
            run_tui(TuiOptions {
                config_path,
                config,
                resume: cli.resume,
            })
            .await?;
            Ok(())
        }),
    }
}
