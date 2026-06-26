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
        /// Advertise this public URL instead of auto-launching a tunnel (bring-your-own).
        #[arg(long)]
        public_url: Option<String>,
        /// Run a pre-created named cloudflared tunnel by token (needs --public-url).
        #[arg(long)]
        tunnel_token: Option<String>,
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
        Some(Command::Serve {
            port,
            token,
            no_tunnel,
            public_url,
            tunnel_token,
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
                    tunnel_token.as_deref(),
                    &config_path,
                )
                .map_err(Into::into);
            }
            let token = serve::resolve_token(token);
            if supervised {
                // Foreground under a service manager (launchd/systemd): skip the
                // launcher/worker split and the daemonize — the manager supervises us.
                let tunnel = serve::resolve_tunnel(no_tunnel, tunnel_token, public_url)?;
                serve::write_own_pidfile();
                return runtime()?.block_on(async {
                    let config = SnippetConfig::load(&config_path).await?;
                    serve::run_serve(config, config_path, &host, port, token, tunnel)
                        .await
                        .map_err::<Box<dyn std::error::Error>, _>(Into::into)
                });
            }
            if std::env::var_os("__SNIPPET_SERVE_WORKER").is_some() {
                // Detached worker: become a daemon, then run the server.
                let tunnel = serve::resolve_tunnel(no_tunnel, tunnel_token, public_url)?;
                serve::daemonize_self()?;
                runtime()?.block_on(async {
                    let config = SnippetConfig::load(&config_path).await?;
                    serve::run_serve(config, config_path, &host, port, token, tunnel)
                        .await
                        .map_err::<Box<dyn std::error::Error>, _>(Into::into)
                })
            } else {
                // Launcher: fetch cloudflared (visible), spawn the worker, print the QR.
                let needs_cf = !no_tunnel && (tunnel_token.is_some() || public_url.is_none());
                if needs_cf {
                    serve::ensure_cloudflared_foreground()?;
                }
                serve::launch_and_show(&host, port, &token, no_tunnel, public_url, tunnel_token, &config_path)
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
