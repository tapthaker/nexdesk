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
    ReleaseClient,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_identifies_its_logical_channel_table() {
        let cases = [
            (
                ClientTransportEvent::Control(ClientControlEvent::WakeDisplay),
                ClientChannel::Control,
            ),
            (
                ClientTransportEvent::Input(ClientInputEvent::ReleaseClient),
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
