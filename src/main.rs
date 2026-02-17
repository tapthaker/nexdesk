mod cli;
mod config;
mod net;
mod input;
mod clipboard;
mod cursor;
mod daemon;
mod setup;

use clap::Parser;
use cli::{Cli, Command};
use color_eyre::eyre::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("nexdesk=debug")
    } else {
        EnvFilter::new("nexdesk=info")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    match cli.command {
        Command::Advertise { port } => {
            net::discovery::advertise(port).await?;
        }
        Command::Discover => {
            net::discovery::discover().await?;
        }
        Command::Ping { addr } => {
            net::quic::ping(&addr).await?;
        }
        Command::Serve { port } => {
            net::quic::serve(port).await?;
        }
        Command::Connect { addr } => {
            net::quic::connect(&addr).await?;
        }
        Command::Fingerprint => {
            net::tls::show_fingerprint()?;
        }
        Command::Trust { fingerprint } => {
            net::tls::trust_fingerprint(&fingerprint)?;
        }
        Command::Setup => {
            setup::run_setup().await?;
        }
    }

    Ok(())
}
