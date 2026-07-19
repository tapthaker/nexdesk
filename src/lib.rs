pub mod app;
pub mod cli;
mod clipboard;
mod config;
mod cursor;
mod daemon;
mod filetransfer;
mod input;
mod net;
mod setup;
mod status;
pub mod testing;

use std::io::{IsTerminal, Write};

use cli::{Cli, Command, DaemonCommand};
use color_eyre::eyre::Result;

fn direction_from_cli_edge(edge: cli::Edge) -> net::protocol::Direction {
    match edge {
        cli::Edge::Left => net::protocol::Direction::Left,
        cli::Edge::Right => net::protocol::Direction::Right,
        cli::Edge::Up => net::protocol::Direction::Up,
        cli::Edge::Down => net::protocol::Direction::Down,
    }
}

fn normalize_config_edge(edge: &str) -> String {
    edge.trim().to_ascii_lowercase()
}

fn direction_from_config_edge(edge: &str) -> Option<net::protocol::Direction> {
    match normalize_config_edge(edge).as_str() {
        "left" => Some(net::protocol::Direction::Left),
        "right" => Some(net::protocol::Direction::Right),
        "top" | "up" => Some(net::protocol::Direction::Up),
        "bottom" | "down" => Some(net::protocol::Direction::Down),
        _ => None,
    }
}

fn direction_from_config_edge_or_error(edge: &str) -> Result<net::protocol::Direction> {
    direction_from_config_edge(edge).ok_or_else(|| {
        let edge = status::terminal_safe(edge, status::MAX_STATUS_DISPLAY_BYTES);
        color_eyre::eyre::eyre!(
            "Invalid configured switch edge {:?}. Run `nexdesk daemon setup` or start with `nexdesk serve --edge <left|right|up|down>`.",
            edge
        )
    })
}

/// Dispatch a parsed command using the production Nexdesk adapters.
pub async fn run(cli: Cli) -> Result<()> {
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
            let dir = if let Some(edge) = edge {
                direction_from_cli_edge(edge)
            } else {
                let cfg = config::NexdeskConfig::load()?;
                if let Some(ref edge_str) = cfg.switch_edge {
                    direction_from_config_edge_or_error(edge_str)?
                } else {
                    if !std::io::stdin().is_terminal() {
                        return Err(color_eyre::eyre::eyre!(
                            "No switch edge configured and no interactive terminal is available. Run `nexdesk daemon setup` or start with `nexdesk serve --edge <left|right|up|down>`."
                        ));
                    }
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
            input::ensure_accessibility()?;
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
        Command::Daemon { command } => match command {
            DaemonCommand::Setup => setup::run_setup().await?,
            DaemonCommand::Start => daemon::start_service()?,
            DaemonCommand::Stop => daemon::stop_service()?,
            DaemonCommand::Status => daemon::print_status()?,
            DaemonCommand::Log => daemon::print_log()?,
        },
        Command::Log => daemon::print_log()?,
        Command::TestInput => {
            input::ensure_accessibility()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_configured_switch_edges() {
        assert_eq!(
            direction_from_config_edge("left"),
            Some(net::protocol::Direction::Left)
        );
        assert_eq!(
            direction_from_config_edge("right"),
            Some(net::protocol::Direction::Right)
        );
        assert_eq!(
            direction_from_config_edge("top"),
            Some(net::protocol::Direction::Up)
        );
        assert_eq!(
            direction_from_config_edge("up"),
            Some(net::protocol::Direction::Up)
        );
        assert_eq!(
            direction_from_config_edge("bottom"),
            Some(net::protocol::Direction::Down)
        );
        assert_eq!(
            direction_from_config_edge("down"),
            Some(net::protocol::Direction::Down)
        );
        assert_eq!(
            direction_from_config_edge("  RIGHT\n"),
            Some(net::protocol::Direction::Right)
        );
        assert_eq!(direction_from_config_edge("invalid"), None);
    }

    #[test]
    fn invalid_configured_switch_edge_is_an_error() {
        let err = direction_from_config_edge_or_error("sideways").unwrap_err();
        assert!(err.to_string().contains("Invalid configured switch edge"));
        assert!(err.to_string().contains("nexdesk daemon setup"));
    }

    #[test]
    fn invalid_configured_switch_edge_error_is_terminal_safe_and_bounded() {
        let err = direction_from_config_edge_or_error(&format!("{}\x1b[31m", "x".repeat(2048)))
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Invalid configured switch edge"));
        assert!(!message.contains('\u{1b}'));
        assert!(message.len() < 1300);
    }
}
