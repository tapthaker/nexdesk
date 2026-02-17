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
        Command::Serve { port, edge } => {
            let dir = match edge {
                cli::Edge::Left => net::protocol::Direction::Left,
                cli::Edge::Right => net::protocol::Direction::Right,
                cli::Edge::Up => net::protocol::Direction::Up,
                cli::Edge::Down => net::protocol::Direction::Down,
            };
            net::quic::serve(port, Some(dir)).await?;
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
        Command::TestInput => {
            use std::io::Write;
            let mut cap = input::capture::create_capturer()?;
            let (sw, sh) = cap.screen_size()?;
            println!("Screen: {}x{}", sw, sh);
            println!("Move your mouse around. Press Ctrl+C to stop.\n");
            loop {
                let keys = cap.poll_key_events()?;
                let (x, y) = cap.mouse_position()?;
                let btns = cap.mouse_buttons()?;
                print!("\rMouse: ({:5}, {:5})  buttons: {:03b}", x, y, btns);
                for k in &keys {
                    if let net::protocol::Message::KeyEvent { keycode, pressed, .. } = k {
                        print!("  key:{} {}", keycode, if *pressed { "dn" } else { "up" });
                    }
                }
                std::io::stdout().flush().ok();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    Ok(())
}
