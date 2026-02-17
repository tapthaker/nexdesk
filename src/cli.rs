use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nexdesk", version, about = "Cross-platform KVM sharing tool")]
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
    },

    /// Connect to a server as a client
    Connect {
        /// Server address (IP:port)
        addr: String,
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

    /// Test input capture (prints mouse position for 10 seconds)
    TestInput,
}
