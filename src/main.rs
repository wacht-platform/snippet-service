use std::path::PathBuf;

use clap::{Parser, Subcommand};
use snippet::config::SnippetConfig;
use snippet::serve::{self};
use snippet::tui::{TuiOptions, run_tui};

#[derive(Debug, Parser)]
#[command(name = "snippet")]
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
        /// Install as an OS service that auto-starts on boot/login (launchd on macOS,
        /// systemd --user on Linux), baking in the other flags you pass here.
        #[arg(long)]
        enable: bool,
        /// Remove the auto-start service installed by --enable.
        #[arg(long)]
        disable: bool,
        /// Internal: run the server in the foreground under a service manager (no
        /// daemonize); the manager owns supervision/restart.
        #[arg(long, hide = true)]
        supervised: bool,
    },
    /// Manage connected browser extensions through the authenticated serve daemon.
    Browser {
        #[command(subcommand)]
        action: BrowserAction,
    },
    /// Manage the secret vault (~/.snippet/vault.json). Secrets are usable by the
    /// agent as $NAME in shell commands; values are injected into the child
    /// process and redacted from everything the model sees.
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
}

#[derive(Debug, Subcommand)]
enum BrowserAction {
    /// Print the complete authenticated WebSocket URI for a new browser extension.
    Uri {
        /// Print a JSON object instead of only the URI.
        #[arg(long)]
        json: bool,
    },
    /// List all connected browser extensions.
    List {
        /// Print the response as formatted JSON.
        #[arg(long)]
        json: bool,
    },
    /// Relay an arbitrary extension method through the daemon.
    Call {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        method: String,
        /// JSON object passed as the extension method arguments.
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Take a compact DOM/page snapshot.
    Snapshot {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        tab: i64,
        #[arg(long, default_value_t = 120)]
        max_elements: usize,
        #[arg(long, default_value_t = 8000)]
        max_text: usize,
    },
    /// Click a snapshot element reference.
    Click {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        tab: i64,
        #[arg(long = "ref")]
        element_ref: String,
        /// Use Chrome's real pointer path instead of DOM click.
        #[arg(long)]
        cdp: bool,
    },
    /// Type into a snapshot element reference.
    Type {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        tab: i64,
        #[arg(long = "ref")]
        element_ref: String,
        text: String,
        #[arg(long)]
        append: bool,
    },
    /// Drag from one snapshot element reference to another.
    Drag {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        tab: i64,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// Use Chrome's real pointer path instead of HTML5 drag events.
        #[arg(long)]
        cdp: bool,
    },
    /// Upload one local file into a file input element reference.
    Upload {
        /// Connected device name shown by `snippet browser list`.
        #[arg(long, alias = "browser")]
        device_name: String,
        #[arg(long)]
        tab: i64,
        #[arg(long = "ref")]
        element_ref: String,
        file: PathBuf,
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
                // No-echo prompt on a TTY so the value isn't visible on screen.
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
}

fn serve_state_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".snippet/serve.json")
}

fn browser_connection() -> Result<ServeState, Box<dyn std::error::Error>> {
    let path = serve_state_path();
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let state: ServeState =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if state.url.trim().is_empty() || state.token.trim().is_empty() {
        return Err(format!("{} has no usable browser connection", path.display()).into());
    }
    Ok(state)
}

fn browser_route_url(
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

fn browser_ws_uri(state: &ServeState) -> Result<String, Box<dyn std::error::Error>> {
    let mut url = browser_route_url(&state.url, "browser/ws")?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(format!("unsupported serve URL scheme `{other}`").into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "could not set WebSocket URL scheme")?;
    url.query_pairs_mut().append_pair("token", &state.token);
    Ok(url.to_string())
}

async fn browser_http(
    base_url: &str,
    method: &str,
    args: serde_json::Value,
    token: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let response = match method {
        "GET" => {
            client
                .get(browser_route_url(base_url, "browsers")?)
                .query(&[("token", token)])
                .send()
                .await?
        }
        "POST" => {
            client
                .post(browser_route_url(base_url, "browser/command")?)
                .query(&[("token", token)])
                .json(&args)
                .send()
                .await?
        }
        _ => return Err(format!("unsupported browser HTTP method `{method}`").into()),
    };
    let status = response.status();
    let body = response.text().await?;
    let value = serde_json::from_str::<serde_json::Value>(&body)
        .unwrap_or_else(|_| serde_json::json!({"error": body}));
    if !status.is_success() {
        return Err(format!("browser daemon returned {status}: {value}").into());
    }
    Ok(value)
}

fn print_browser_json(
    value: &serde_json::Value,
    pretty: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

fn mime_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

async fn browser_cli(action: BrowserAction) -> Result<(), Box<dyn std::error::Error>> {
    let state = browser_connection()?;
    match action {
        BrowserAction::Uri { json } => {
            let uri = browser_ws_uri(&state)?;
            if json {
                print_browser_json(
                    &serde_json::json!({"uri": uri, "url": state.url, "token": state.token}),
                    true,
                )
            } else {
                println!("{uri}");
                Ok(())
            }
        }
        BrowserAction::List { json } => {
            let value = browser_http(
                &state.url,
                "GET",
                serde_json::Value::Null,
                &state.token,
            )
            .await?;
            print_browser_json(&value, json)
        }
        BrowserAction::Call {
            device_name,
            method,
            args,
        } => {
            let args: serde_json::Value = serde_json::from_str(&args)
                .map_err(|e| format!("invalid --args JSON: {e}"))?;
            let value = browser_http(
                &state.url,
                "POST",
                serde_json::json!({"device_name": device_name, "method": method, "args": args}),
                &state.token,
            )
            .await?;
            print_browser_json(&value, true)
        }
        BrowserAction::Snapshot {
            device_name,
            tab,
            max_elements,
            max_text,
        } => browser_command_cli(
            &state.url,
            &state.token,
            device_name,
            "page.snapshot",
            serde_json::json!({"tabId": tab, "maxElements": max_elements, "maxText": max_text}),
        )
        .await,
        BrowserAction::Click {
            device_name,
            tab,
            element_ref,
            cdp,
        } => browser_command_cli(
            &state.url,
            &state.token,
            device_name,
            "page.click",
            serde_json::json!({"tabId": tab, "ref": element_ref, "mode": if cdp { "cdp" } else { "dom" }}),
        )
        .await,
        BrowserAction::Type {
            device_name,
            tab,
            element_ref,
            text,
            append,
        } => browser_command_cli(
            &state.url,
            &state.token,
            device_name,
            "page.type",
            serde_json::json!({"tabId": tab, "ref": element_ref, "text": text, "append": append}),
        )
        .await,
        BrowserAction::Drag {
            device_name,
            tab,
            from,
            to,
            cdp,
        } => browser_command_cli(
            &state.url,
            &state.token,
            device_name,
            "page.drag",
            serde_json::json!({"tabId": tab, "from": from, "to": to, "mode": if cdp { "cdp" } else { "html5" }}),
        )
        .await,
        BrowserAction::Upload {
            device_name,
            tab,
            element_ref,
            file,
        } => {
            let data = std::fs::read(&file)
                .map_err(|e| format!("read upload {}: {e}", file.display()))?;
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(data);
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("upload path has no valid file name: {}", file.display()))?;
            browser_command_cli(
                &state.url,
                &state.token,
                device_name,
                "page.upload",
                serde_json::json!({"tabId": tab, "ref": element_ref, "files": [{"name": name, "type": mime_type(&file), "data": encoded}]}),
            )
            .await
        }
    }
}

async fn browser_command_cli(
    base_url: &str,
    token: &str,
    device_name: String,
    method: &str,
    args: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = browser_http(
        base_url,
        "POST",
        serde_json::json!({"device_name": device_name, "method": method, "args": args}),
        token,
    )
    .await?;
    print_browser_json(&value, true)
}

// Not `#[tokio::main]`: `serve` daemonizes (fork) before any runtime threads exist,
// so the runtime is built explicitly *after* the fork.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = config_path(&cli);

    match cli.command {
        Some(Command::Vault { action }) => return vault_cli(action),
        Some(Command::Browser { action }) => return runtime()?.block_on(browser_cli(action)),
        Some(Command::Serve {
            port,
            token,
            no_tunnel,
            public_url,
            host,
            stop,
            status,
            enable,
            disable,
            supervised,
        }) => {
            // Lifecycle commands are synchronous and need no config/runtime.
            if stop {
                return serve::stop().map_err(Into::into);
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
                    &config_path,
                )
                .map_err(Into::into);
            }
            let token = serve::resolve_token(token);
            if supervised {
                // Foreground under a service manager (launchd/systemd): skip the
                // launcher/worker split and the daemonize — the manager supervises
                // us. Settings come from serve.toml (written by `--enable`); host/
                // port fall back to CLI defaults, an explicit url flag still wins,
                // and an absent file → plain defaults (a fresh serve).
                let s = serve::ServeSettings::load();
                let host = s.host.unwrap_or(host);
                let port = s.port.unwrap_or(port);
                let no_tunnel = no_tunnel || s.no_tunnel;
                let public_url = public_url.or(s.public_url);
                let tunnel = serve::resolve_tunnel(no_tunnel, public_url);
                serve::write_own_pidfile();
                return runtime()?.block_on(async {
                    let config = SnippetConfig::load(&config_path).await?;
                    // Supervised: a service manager can restart us onto a new binary.
                    serve::run_serve(config, config_path, &host, port, token, tunnel, true)
                        .await
                        .map_err::<Box<dyn std::error::Error>, _>(Into::into)
                });
            }
            if std::env::var_os("__SNIPPET_SERVE_WORKER").is_some() {
                // Detached worker: become a daemon, then run the server.
                let tunnel = serve::resolve_tunnel(no_tunnel, public_url);
                serve::daemonize_self()?;
                runtime()?.block_on(async {
                    let config = SnippetConfig::load(&config_path).await?;
                    // Daemonized without a supervisor: update in place, apply on next restart.
                    serve::run_serve(config, config_path, &host, port, token, tunnel, false)
                        .await
                        .map_err::<Box<dyn std::error::Error>, _>(Into::into)
                })
            } else {
                // Launcher: fetch cloudflared (visible), spawn the worker, print the QR.
                // Only the default quick-tunnel route needs cloudflared.
                let needs_cf = !no_tunnel && public_url.is_none();
                if needs_cf {
                    serve::ensure_cloudflared_foreground()?;
                }
                serve::launch_and_show(&host, port, &token, no_tunnel, public_url, &config_path)
                    .map_err(Into::into)
            }
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
