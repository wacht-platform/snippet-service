use std::path::PathBuf;

use clap::Parser;
use snippet::config::SnippetConfig;
use snippet::tui::{TuiOptions, run_tui};

#[derive(Debug, Parser)]
#[command(name = "snippet")]
#[command(about = "A Rust coding-agent harness with a durable TUI runtime.")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
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
    run_tui(TuiOptions {
        config_path,
        config,
    })
    .await?;
    Ok(())
}
