use std::future::Future;
use std::pin::Pin;

use color_eyre::eyre::Result;

pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Logical client transport channels. Closure and failure are reported per
/// channel so one stream cannot silently masquerade as another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientChannel {
    Control,
    Input,
    Clipboard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerScrollPhase {
    None,
    Began,
    Changed,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerScreen {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientControlEvent {
    Heartbeat { timestamp: u64 },
    HeartbeatAcknowledged { timestamp: u64 },
    PeerScreenChanged(PeerScreen),
    WakeDisplay,
    ReleaseControl,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientInputEvent {
    MouseMoved {
        x: i32,
        y: i32,
    },
    MouseButtonChanged {
        button: u8,
        pressed: bool,
    },
    MouseScrolled {
        dx: f64,
        dy: f64,
        phase: PeerScrollPhase,
    },
    KeyChanged {
        keycode: u32,
        pressed: bool,
        modifiers: u16,
    },
    SwitchToClient {
        direction: PeerDirection,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientClipboardEvent {
    TextChanged(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientControlCommand {
    Heartbeat { timestamp: u64 },
    AcknowledgeHeartbeat { timestamp: u64 },
    RequestSwitchBack { direction: PeerDirection },
    LocalScreenChanged(PeerScreen),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientClipboardCommand {
    SetPeerText(String),
}

/// Logical server transport channels. The aliases deliberately share channel
/// identity with the client side while retaining role-specific API names.
pub type ServerChannel = ClientChannel;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerControlEvent {
    Heartbeat { timestamp: u64 },
    SwitchBackRequested { direction: PeerDirection },
    PeerScreenChanged(PeerScreen),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerClipboardEvent {
    TextChanged(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerControlCommand {
    AcknowledgeHeartbeat { timestamp: u64 },
    LocalScreenChanged(PeerScreen),
    WakePeerDisplay,
    ReleasePeerControl,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerInputCommand {
    MouseMoved {
        x: i32,
        y: i32,
    },
    MouseButtonChanged {
        button: u8,
        pressed: bool,
    },
    MouseScrolled {
        dx: f64,
        dy: f64,
        phase: PeerScrollPhase,
    },
    KeyChanged {
        keycode: u32,
        pressed: bool,
        modifiers: u16,
    },
    SwitchToPeer {
        direction: PeerDirection,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerClipboardCommand {
    SetPeerText(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportFailure {
    pub channel: ClientChannel,
    pub message: String,
}

impl TransportFailure {
    pub fn new(channel: ClientChannel, message: impl Into<String>) -> Self {
        Self {
            channel,
            message: message.into(),
        }
    }
}

/// One typed event delivered by a client peer link after handshake.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientTransportEvent {
    Control(ClientControlEvent),
    Input(ClientInputEvent),
    Clipboard(ClientClipboardEvent),
    Closed(ClientChannel),
    Failed(TransportFailure),
}

impl ClientTransportEvent {
    pub fn channel(&self) -> ClientChannel {
        match self {
            Self::Control(_) => ClientChannel::Control,
            Self::Input(_) => ClientChannel::Input,
            Self::Clipboard(_) => ClientChannel::Clipboard,
            Self::Closed(channel) => *channel,
            Self::Failed(failure) => failure.channel,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerTransportEvent {
    Control(ServerControlEvent),
    Clipboard(ServerClipboardEvent),
    Closed(ServerChannel),
    Failed(TransportFailure),
}

impl ServerTransportEvent {
    pub fn channel(&self) -> ServerChannel {
        match self {
            Self::Control(_) => ServerChannel::Control,
            Self::Clipboard(_) => ServerChannel::Clipboard,
            Self::Closed(channel) => *channel,
            Self::Failed(failure) => failure.channel,
        }
    }
}

/// Post-handshake client transport boundary.
pub trait ClientPeerLink: Send + Sync {
    fn next_event(&self) -> TransportFuture<'_, Option<ClientTransportEvent>>;

    fn send_control(&self, command: ClientControlCommand) -> TransportFuture<'_, Result<()>>;

    fn send_clipboard(&self, command: ClientClipboardCommand) -> TransportFuture<'_, Result<()>>;

    fn shutdown(&self) -> TransportFuture<'_, ()>;
}

/// Post-handshake server transport boundary.
pub trait ServerPeerLink: Send + Sync {
    fn next_event(&self) -> TransportFuture<'_, Option<ServerTransportEvent>>;

    fn send_control(&self, command: ServerControlCommand) -> TransportFuture<'_, Result<()>>;

    fn send_input(&self, command: ServerInputCommand) -> TransportFuture<'_, Result<()>>;

    fn send_clipboard(&self, command: ServerClipboardCommand) -> TransportFuture<'_, Result<()>>;

    fn shutdown(&self) -> TransportFuture<'_, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClosedLink;

    impl ClientPeerLink for ClosedLink {
        fn next_event(&self) -> TransportFuture<'_, Option<ClientTransportEvent>> {
            Box::pin(async { None })
        }

        fn send_control(&self, _command: ClientControlCommand) -> TransportFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn send_clipboard(
            &self,
            _command: ClientClipboardCommand,
        ) -> TransportFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown(&self) -> TransportFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    struct ClosedServerLink;

    impl ServerPeerLink for ClosedServerLink {
        fn next_event(&self) -> TransportFuture<'_, Option<ServerTransportEvent>> {
            Box::pin(async { None })
        }

        fn send_control(&self, _command: ServerControlCommand) -> TransportFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn send_input(&self, _command: ServerInputCommand) -> TransportFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn send_clipboard(
            &self,
            _command: ServerClipboardCommand,
        ) -> TransportFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown(&self) -> TransportFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn client_peer_link_is_object_safe() {
        fn assert_object_safe(_: &dyn ClientPeerLink) {}
        assert_object_safe(&ClosedLink);
    }

    #[test]
    fn server_peer_link_is_object_safe() {
        fn assert_object_safe(_: &dyn ServerPeerLink) {}
        assert_object_safe(&ClosedServerLink);
    }

    #[test]
    fn every_event_identifies_its_logical_channel_table() {
        let cases = [
            (
                ClientTransportEvent::Control(ClientControlEvent::WakeDisplay),
                ClientChannel::Control,
            ),
            (
                ClientTransportEvent::Input(ClientInputEvent::MouseMoved { x: 0, y: 0 }),
                ClientChannel::Input,
            ),
            (
                ClientTransportEvent::Clipboard(ClientClipboardEvent::TextChanged(
                    "text".to_string(),
                )),
                ClientChannel::Clipboard,
            ),
            (
                ClientTransportEvent::Closed(ClientChannel::Input),
                ClientChannel::Input,
            ),
            (
                ClientTransportEvent::Failed(TransportFailure::new(
                    ClientChannel::Control,
                    "connection lost",
                )),
                ClientChannel::Control,
            ),
        ];

        for (event, expected_channel) in cases {
            assert_eq!(event.channel(), expected_channel);
        }
    }

    #[test]
    fn closure_and_failure_are_distinct_typed_events() {
        let closed = ClientTransportEvent::Closed(ClientChannel::Clipboard);
        let failed = ClientTransportEvent::Failed(TransportFailure::new(
            ClientChannel::Clipboard,
            "malformed frame",
        ));

        assert_ne!(closed, failed);
        assert!(matches!(
            failed,
            ClientTransportEvent::Failed(TransportFailure {
                channel: ClientChannel::Clipboard,
                ..
            })
        ));
    }
}
