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
    /// Internal: dump a sample transcript to stdout for design review.
    #[command(hide = true)]
    RenderPreview,
    /// Manage the secret vault (~/.snippet/vault.json). Secrets are usable by the
    /// agent as $NAME in shell commands; values are injected into the child
    /// process and redacted from everything the model sees.
    Vault {
        #[command(subcommand)]
        action: VaultAction,
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
            println!("✓ stored `{name}` ({} secret{} in vault)", vault.names().len(), if vault.names().len() == 1 { "" } else { "s" });
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
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

// Not `#[tokio::main]`: `serve` daemonizes (fork) before any runtime threads exist,
// so the runtime is built explicitly *after* the fork.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = config_path(&cli);

    match cli.command {
        Some(Command::RenderPreview) => {
            println!("{}", snippet::tui::transcript::preview_transcript(64));
            return Ok(());
        }
        Some(Command::Vault { action }) => return vault_cli(action),
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
