mod cli;
mod clipboard;
mod config;
mod cursor;
mod daemon;
mod filetransfer;
mod input;
mod net;
mod setup;
mod status;

use clap::Parser;
use cli::{Cli, Command};
use color_eyre::eyre::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    // Skip tracing init for setup/status/log commands — log output corrupts the TUI/status output.
    if !matches!(cli.command, Command::Setup | Command::Status | Command::Log) {
        let filter = if cli.verbose {
            EnvFilter::new("nexdesk=debug")
        } else {
            EnvFilter::new("nexdesk=info")
        };

        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

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
            input::ensure_accessibility()?;
            let dir = if let Some(edge) = edge {
                match edge {
                    cli::Edge::Left => net::protocol::Direction::Left,
                    cli::Edge::Right => net::protocol::Direction::Right,
                    cli::Edge::Up => net::protocol::Direction::Up,
                    cli::Edge::Down => net::protocol::Direction::Down,
                }
            } else {
                let cfg = config::NexdeskConfig::load()?;
                if let Some(ref edge_str) = cfg.switch_edge {
                    match edge_str.as_str() {
                        "left" => net::protocol::Direction::Left,
                        "right" => net::protocol::Direction::Right,
                        "top" | "up" => net::protocol::Direction::Up,
                        "bottom" | "down" => net::protocol::Direction::Down,
                        _ => net::protocol::Direction::Right,
                    }
                } else {
                    let dir = setup::edge_picker::pick_edge()?;
                    let mut cfg = config::NexdeskConfig::load()?;
                    cfg.switch_edge = Some(
                        match dir {
                            net::protocol::Direction::Left => "left",
                            net::protocol::Direction::Right => "right",
                            net::protocol::Direction::Up => "top",
                            net::protocol::Direction::Down => "bottom",
                        }
                        .to_string(),
                    );
                    cfg.save()?;
                    dir
                }
            };
            net::quic::serve(port, Some(dir)).await?;
        }
        Command::Connect { addr } => {
            input::ensure_accessibility()?;
            net::quic::connect(addr.as_deref()).await?;
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
        Command::Status => {
            daemon::print_status()?;
        }
        Command::Log => {
            daemon::print_log()?;
        }
        Command::TestInput => {
            input::ensure_accessibility()?;
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
                    match k {
                        net::protocol::Message::KeyEvent {
                            keycode, pressed, ..
                        } => {
                            print!("  key:{} {}", keycode, if *pressed { "dn" } else { "up" });
                        }
                        net::protocol::Message::MouseScroll { dx, dy, .. } => {
                            print!("  scroll:({}, {})", dx, dy);
                        }
                        _ => {}
                    }
                }
                std::io::stdout().flush().ok();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    Ok(())
}
