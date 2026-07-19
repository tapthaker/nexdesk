use std::io::IsTerminal;

use clap::Parser;
use color_eyre::eyre::Result;
use nexdesk::cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    // Skip tracing init for setup/service commands — log output corrupts their user-facing output.
    if !matches!(cli.command, Command::Daemon { .. } | Command::Log) {
        let filter = if cli.verbose {
            EnvFilter::new("nexdesk=debug")
        } else {
            EnvFilter::new("nexdesk=info")
        };

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(std::io::stderr().is_terminal())
            .init();
    }

    nexdesk::run(cli).await
}
