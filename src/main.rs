use std::path::PathBuf;

use clap::{Parser, Subcommand};
use snippet::config::SnippetConfig;
use snippet::serve::{self, Tunnel};
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
        /// Stop the running background daemon.
        #[arg(long)]
        stop: bool,
        /// Show the running daemon's status + its QR / connection string.
        #[arg(long)]
        status: bool,
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
            stop,
            status,
        }) => {
            // Lifecycle commands are synchronous and need no config/runtime.
            if stop {
                return serve::stop().map_err(Into::into);
            }
            if status {
                return serve::status().map_err(Into::into);
            }
            let token = token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let tunnel = if no_tunnel {
                Tunnel::None
            } else if let Some(t) = tunnel_token {
                let url = public_url.ok_or_else(|| "--tunnel-token requires --public-url".to_string())?;
                Tunnel::Named { token: t, url }
            } else if let Some(u) = public_url {
                Tunnel::Url(u)
            } else {
                Tunnel::Cloudflared
            };
            // Fetch cloudflared in the foreground (visible progress bar) before we
            // detach — the backgrounded child can't draw to this terminal.
            if matches!(tunnel, Tunnel::Cloudflared | Tunnel::Named { .. }) {
                serve::ensure_cloudflared_foreground()?;
            }
            // Fork into the background BEFORE the async runtime exists; returns only
            // in the detached child.
            serve::detach()?;
            runtime()?.block_on(async {
                let config = SnippetConfig::load(&config_path).await?;
                serve::run_serve(config, port, token, tunnel)
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
