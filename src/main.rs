use std::path::PathBuf;

use clap::{Parser, Subcommand};
use snippet::config::SnippetConfig;
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
    /// Run headless and serve the agent for remote control (mobile app). The TUI is
    /// unaffected — this is an additional frontend over the same on-disk sessions.
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
        /// Run a pre-created named cloudflared tunnel by token (needs --public-url for its hostname).
        #[arg(long)]
        tunnel_token: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".snippet/config.toml")
    });

    let config = SnippetConfig::load(&config_path).await?;

    match cli.command {
        Some(Command::Serve { port, token, no_tunnel, public_url, tunnel_token }) => {
            let token = token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let tunnel = if no_tunnel {
                snippet::serve::Tunnel::None
            } else if let Some(t) = tunnel_token {
                let url = public_url
                    .ok_or_else(|| "--tunnel-token requires --public-url".to_string())?;
                snippet::serve::Tunnel::Named { token: t, url }
            } else if let Some(u) = public_url {
                snippet::serve::Tunnel::Url(u)
            } else {
                snippet::serve::Tunnel::Cloudflared
            };
            if let Err(e) = snippet::serve::run_serve(config, port, token, tunnel).await {
                return Err(e.into());
            }
            Ok(())
        }
        None => {
            run_tui(TuiOptions {
                config_path,
                config,
                resume: cli.resume,
            })
            .await?;
            Ok(())
        }
    }
}
