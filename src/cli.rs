use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "nexdesk", version = env!("NEXDESK_VERSION"), about = "Cross-platform KVM sharing tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Advertise this machine on the local network
    Advertise {
        /// Port to advertise (for future QUIC server)
        #[arg(short, long, default_value_t = 4242)]
        port: u16,
    },

    /// Discover peers on the local network
    Discover,

    /// Ping a peer to measure latency
    Ping {
        /// Peer address (IP:port or hostname)
        addr: String,
    },

    /// Run as server (capture input, send to clients)
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value_t = 4242)]
        port: u16,

        /// Screen edge where the remote machine is located
        #[arg(short, long)]
        edge: Option<Edge>,
    },

    /// Connect to a server as a client (discovers via mDNS if no address given)
    Connect {
        /// Server address (IP:port). If omitted, discovers via mDNS.
        addr: Option<String>,
    },

    /// Show this machine's certificate fingerprint
    Fingerprint,

    /// Trust a peer by fingerprint
    Trust {
        /// SHA-256 fingerprint to trust
        fingerprint: String,
    },

    /// Launch the TUI setup wizard
    Setup,

    /// Manage the background service
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },

    /// Test input capture (prints mouse position for 10 seconds)
    TestInput,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum DaemonCommand {
    /// Show service, process, listener, and connection status
    Status,

    /// Start the background service
    Start,

    /// Stop the background service
    Stop,

    /// Show service diagnostics and recent logs
    Logs,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Edge {
    Left,
    Right,
    Up,
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_daemon_action(name: &str) -> DaemonCommand {
        let cli = Cli::try_parse_from(["nexdesk", "daemon", name]).unwrap();
        match cli.command {
            Command::Daemon { action } => action,
            _ => panic!("expected daemon command"),
        }
    }

    #[test]
    fn parses_all_daemon_actions() {
        assert!(matches!(
            parse_daemon_action("status"),
            DaemonCommand::Status
        ));
        assert!(matches!(parse_daemon_action("start"), DaemonCommand::Start));
        assert!(matches!(parse_daemon_action("stop"), DaemonCommand::Stop));
        assert!(matches!(parse_daemon_action("logs"), DaemonCommand::Logs));
    }

    #[test]
    fn rejects_removed_legacy_daemon_commands() {
        assert!(Cli::try_parse_from(["nexdesk", "status"]).is_err());
        assert!(Cli::try_parse_from(["nexdesk", "log"]).is_err());
    }
}
