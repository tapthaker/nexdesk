mod cancellation;
mod client_transport;
mod discovery;
mod handshake;
mod reconnect;
mod server_transport;
mod session;
mod update;

pub use cancellation::CancellationToken;
pub use client_transport::{client_channel_disposition, ClientChannelDisposition};
pub use discovery::{resolve_peer_with_retry, DiscoveredPeerSet};
pub(crate) use handshake::{
    client_pairing_decision, complete_client_pairing, decide_server_handshake,
    require_handshake_message, validate_client_server_hello, HandshakeMessage, PairingCompletion,
    PairingDecision, ServerHandshakeDecision, ServerHelloAck, ServerPairingMethod,
};
pub use reconnect::RetryPolicy;
pub use server_transport::{server_channel_disposition, ServerChannelDisposition};
pub use session::{RestartReason, RunOutcome, SessionExit};
pub use update::{
    execute_update, is_newer, is_release_version, UpdateDecision, UpdateExecution, UpdatePolicy,
    UpdateRejection, UpdateSource, MAX_RELEASE_VERSION_BYTES,
};
