use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use color_eyre::eyre::Result;

pub type PairingPromptFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// Boundary for obtaining a pairing code from a user or test scenario.
pub trait PairingPrompt: Send + Sync {
    fn prompt(&self, addr: SocketAddr) -> PairingPromptFuture<'_>;
}
