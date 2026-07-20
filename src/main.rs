use std::io::IsTerminal;

use clap::Parser;
use color_eyre::eyre::Result;
use nexdesk::{
    app::RunOutcome,
    cli::{Cli, Command},
};
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

    match nexdesk::run(cli).await? {
        RunOutcome::Completed => Ok(()),
        RunOutcome::RestartRequested(reason) => {
            // Do not use exec(). macOS IOPM assertions are process-scoped and
            // exec keeps the same PID alive, so old assertions can leak across
            // self-updates and watchdog restarts. Exit and let the configured
            // service manager start a fresh process.
            tracing::info!(?reason, "Exiting for service-manager restart");
            std::process::exit(0);
        }
    }
}
