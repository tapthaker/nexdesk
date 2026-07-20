use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{mpsc, Mutex};

use crate::net::framing;
use crate::net::protocol::{self, ClipboardContent, Direction, Message, ScrollPhase};
use crate::ports::{
    ClientChannel, ClientClipboardCommand, ClientClipboardEvent, ClientControlCommand,
    ClientControlEvent, ClientInputEvent, ClientPeerLink, ClientTransportEvent, PeerDirection,
    PeerScreen, PeerScrollPhase, TransportFailure, TransportFuture,
};

const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_BUFFER: usize = 64;

/// Quinn-backed post-handshake client link that translates wire messages into
/// semantic channel events.
pub(crate) struct QuinnClientPeerLink {
    control_send: Arc<Mutex<SendStream>>,
    clipboard_send: Arc<Mutex<SendStream>>,
    events: mpsc::Receiver<ClientTransportEvent>,
}

impl QuinnClientPeerLink {
    pub(crate) async fn open(
        connection: &Connection,
        control_send: SendStream,
        control_recv: RecvStream,
    ) -> Result<Self> {
        let (clipboard_send, mut clipboard_recv) =
            tokio::time::timeout(CHANNEL_OPEN_TIMEOUT, connection.accept_bi())
                .await
                .wrap_err("Timeout waiting for clipboard stream from server")?
                .wrap_err("Failed to accept clipboard stream")?;
        framing::recv_message(&mut clipboard_recv)
            .await?
            .ok_or_else(|| eyre!("Clipboard stream closed before ready marker"))?;

        let mut input_recv = tokio::time::timeout(CHANNEL_OPEN_TIMEOUT, connection.accept_uni())
            .await
            .wrap_err("Timeout waiting for input stream from server")?
            .wrap_err("Failed to accept input stream")?;
        framing::recv_message(&mut input_recv)
            .await?
            .ok_or_else(|| eyre!("Input stream closed before ready marker"))?;

        let (event_send, events) = mpsc::channel(EVENT_BUFFER);
        spawn_reader(
            control_recv,
            ClientChannel::Control,
            event_send.clone(),
            map_control_message,
        );
        spawn_reader(
            input_recv,
            ClientChannel::Input,
            event_send.clone(),
            map_input_message,
        );
        spawn_reader(
            clipboard_recv,
            ClientChannel::Clipboard,
            event_send,
            map_clipboard_message,
        );

        Ok(Self {
            control_send: Arc::new(Mutex::new(control_send)),
            clipboard_send: Arc::new(Mutex::new(clipboard_send)),
            events,
        })
    }
}

impl ClientPeerLink for QuinnClientPeerLink {
    fn next_event(&mut self) -> TransportFuture<'_, Option<ClientTransportEvent>> {
        Box::pin(self.events.recv())
    }

    fn send_control(&self, command: ClientControlCommand) -> TransportFuture<'_, Result<()>> {
        let sender = self.control_send.clone();
        Box::pin(async move {
            let message = control_command_message(command);
            framing::send_message(&mut *sender.lock().await, &message).await
        })
    }

    fn send_clipboard(&self, command: ClientClipboardCommand) -> TransportFuture<'_, Result<()>> {
        let sender = self.clipboard_send.clone();
        Box::pin(async move {
            let message = match command {
                ClientClipboardCommand::SetPeerText(text) => Message::ClipboardUpdate {
                    content: ClipboardContent::Text(text),
                },
            };
            framing::send_message(&mut *sender.lock().await, &message).await
        })
    }
}

fn spawn_reader(
    mut recv: RecvStream,
    channel: ClientChannel,
    sender: mpsc::Sender<ClientTransportEvent>,
    map: fn(Message) -> std::result::Result<ClientTransportEvent, String>,
) {
    tokio::spawn(async move {
        loop {
            let event = match framing::recv_message(&mut recv).await {
                Ok(Some(message)) => match map(message) {
                    Ok(event) => event,
                    Err(message) => {
                        ClientTransportEvent::Failed(TransportFailure::new(channel, message))
                    }
                },
                Ok(None) => ClientTransportEvent::Closed(channel),
                Err(error) => {
                    ClientTransportEvent::Failed(TransportFailure::new(channel, error.to_string()))
                }
            };
            let terminal = matches!(
                event,
                ClientTransportEvent::Closed(_) | ClientTransportEvent::Failed(_)
            );
            if sender.send(event).await.is_err() || terminal {
                break;
            }
        }
    });
}

fn map_control_message(message: Message) -> std::result::Result<ClientTransportEvent, String> {
    let event = match message {
        Message::Heartbeat { timestamp } => ClientControlEvent::Heartbeat { timestamp },
        Message::HeartbeatAck { timestamp } => {
            ClientControlEvent::HeartbeatAcknowledged { timestamp }
        }
        Message::ScreenResize { screen } => ClientControlEvent::PeerScreenChanged(PeerScreen {
            width: screen.width,
            height: screen.height,
        }),
        Message::WakeDisplay => ClientControlEvent::WakeDisplay,
        other => return Err(unexpected_message(ClientChannel::Control, &other)),
    };
    Ok(ClientTransportEvent::Control(event))
}

fn map_input_message(message: Message) -> std::result::Result<ClientTransportEvent, String> {
    let event = match message {
        Message::MouseMove { x, y } => ClientInputEvent::MouseMoved { x, y },
        Message::MouseButton { button, pressed } => {
            ClientInputEvent::MouseButtonChanged { button, pressed }
        }
        Message::MouseScroll { dx, dy, phase } => ClientInputEvent::MouseScrolled {
            dx,
            dy,
            phase: map_scroll_phase(phase),
        },
        Message::KeyEvent {
            keycode,
            pressed,
            modifiers,
        } => ClientInputEvent::KeyChanged {
            keycode,
            pressed,
            modifiers,
        },
        Message::SwitchScreen { direction } => ClientInputEvent::SwitchToClient {
            direction: map_direction(direction),
        },
        Message::ReleaseScreen => ClientInputEvent::ReleaseClient,
        other => return Err(unexpected_message(ClientChannel::Input, &other)),
    };
    Ok(ClientTransportEvent::Input(event))
}

fn map_clipboard_message(message: Message) -> std::result::Result<ClientTransportEvent, String> {
    match message {
        Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        } => Ok(ClientTransportEvent::Clipboard(
            ClientClipboardEvent::TextChanged(text),
        )),
        other => Err(unexpected_message(ClientChannel::Clipboard, &other)),
    }
}

fn unexpected_message(channel: ClientChannel, message: &Message) -> String {
    format!(
        "Unexpected message on {:?} channel: {}",
        channel,
        protocol::message_summary(message)
    )
}

fn control_command_message(command: ClientControlCommand) -> Message {
    match command {
        ClientControlCommand::Heartbeat { timestamp } => Message::Heartbeat { timestamp },
        ClientControlCommand::AcknowledgeHeartbeat { timestamp } => {
            Message::HeartbeatAck { timestamp }
        }
        ClientControlCommand::RequestSwitchBack { direction } => Message::SwitchScreen {
            direction: unmap_direction(direction),
        },
        ClientControlCommand::LocalScreenChanged(screen) => Message::ScreenResize {
            screen: protocol::ScreenLayout {
                width: screen.width,
                height: screen.height,
            },
        },
    }
}

fn map_direction(direction: Direction) -> PeerDirection {
    match direction {
        Direction::Left => PeerDirection::Left,
        Direction::Right => PeerDirection::Right,
        Direction::Up => PeerDirection::Up,
        Direction::Down => PeerDirection::Down,
    }
}

fn unmap_direction(direction: PeerDirection) -> Direction {
    match direction {
        PeerDirection::Left => Direction::Left,
        PeerDirection::Right => Direction::Right,
        PeerDirection::Up => Direction::Up,
        PeerDirection::Down => Direction::Down,
    }
}

fn map_scroll_phase(phase: ScrollPhase) -> PeerScrollPhase {
    match phase {
        ScrollPhase::None => PeerScrollPhase::None,
        ScrollPhase::Began => PeerScrollPhase::Began,
        ScrollPhase::Changed => PeerScrollPhase::Changed,
        ScrollPhase::Ended => PeerScrollPhase::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_messages_are_translated_to_their_typed_channels() {
        assert_eq!(
            map_control_message(Message::Heartbeat { timestamp: 7 }).unwrap(),
            ClientTransportEvent::Control(ClientControlEvent::Heartbeat { timestamp: 7 })
        );
        assert_eq!(
            map_input_message(Message::MouseButton {
                button: 1,
                pressed: true,
            })
            .unwrap(),
            ClientTransportEvent::Input(ClientInputEvent::MouseButtonChanged {
                button: 1,
                pressed: true,
            })
        );
        assert_eq!(
            map_clipboard_message(Message::ClipboardUpdate {
                content: ClipboardContent::Text("hello".to_string()),
            })
            .unwrap(),
            ClientTransportEvent::Clipboard(ClientClipboardEvent::TextChanged("hello".to_string()))
        );
    }

    #[test]
    fn unexpected_cross_channel_messages_become_transport_failures() {
        let error = map_control_message(Message::ReleaseScreen).unwrap_err();
        assert!(error.contains("Control channel"));
        assert!(error.contains("ReleaseScreen"));
    }

    #[test]
    fn outbound_control_commands_map_to_wire_messages() {
        assert!(matches!(
            control_command_message(ClientControlCommand::LocalScreenChanged(PeerScreen {
                width: 1920,
                height: 1080,
            })),
            Message::ScreenResize {
                screen: protocol::ScreenLayout {
                    width: 1920,
                    height: 1080,
                }
            }
        ));
        assert!(matches!(
            control_command_message(ClientControlCommand::RequestSwitchBack {
                direction: PeerDirection::Left,
            }),
            Message::SwitchScreen {
                direction: Direction::Left,
            }
        ));
    }
}
