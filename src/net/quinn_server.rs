use std::sync::Arc;

use color_eyre::eyre::Result;
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;

use crate::net::framing;
use crate::net::protocol::{self, ClipboardContent, Direction, Message, ScrollPhase};
use crate::ports::{
    PeerDirection, PeerScrollPhase, ServerChannel, ServerClipboardCommand, ServerClipboardEvent,
    ServerControlCommand, ServerControlEvent, ServerInputCommand, ServerPeerLink,
    ServerTransportEvent, TransportFailure, TransportFuture,
};

const EVENT_BUFFER: usize = 64;

/// Quinn-backed post-handshake server link that translates wire messages into
/// semantic channel events and commands.
pub(crate) struct QuinnServerPeerLink {
    control_send: Arc<Mutex<SendStream>>,
    input_send: Arc<Mutex<SendStream>>,
    clipboard_send: Arc<Mutex<SendStream>>,
    events: Mutex<mpsc::Receiver<ServerTransportEvent>>,
    tasks: Mutex<Option<JoinSet<()>>>,
}

impl QuinnServerPeerLink {
    pub(crate) async fn open(
        connection: &Connection,
        control_send: SendStream,
        control_recv: RecvStream,
    ) -> Result<Self> {
        let (mut clipboard_send, clipboard_recv) = connection.open_bi().await?;
        framing::send_message(&mut clipboard_send, &Message::Heartbeat { timestamp: 0 }).await?;

        let mut input_send = connection.open_uni().await?;
        framing::send_message(&mut input_send, &Message::Heartbeat { timestamp: 0 }).await?;

        let (event_send, events) = mpsc::channel(EVENT_BUFFER);
        let mut tasks = JoinSet::new();
        spawn_reader(
            &mut tasks,
            control_recv,
            ServerChannel::Control,
            event_send.clone(),
            map_control_message,
        );
        spawn_reader(
            &mut tasks,
            clipboard_recv,
            ServerChannel::Clipboard,
            event_send,
            map_clipboard_message,
        );

        Ok(Self {
            control_send: Arc::new(Mutex::new(control_send)),
            input_send: Arc::new(Mutex::new(input_send)),
            clipboard_send: Arc::new(Mutex::new(clipboard_send)),
            events: Mutex::new(events),
            tasks: Mutex::new(Some(tasks)),
        })
    }

    #[cfg(test)]
    pub(crate) async fn finish_control_stream(&self) -> Result<()> {
        self.control_send.lock().await.finish()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn reader_tasks_are_idle(&self) -> bool {
        self.tasks.lock().await.is_none()
    }
}

impl ServerPeerLink for QuinnServerPeerLink {
    fn next_event(&self) -> TransportFuture<'_, Option<ServerTransportEvent>> {
        Box::pin(async move { self.events.lock().await.recv().await })
    }

    fn send_control(&self, command: ServerControlCommand) -> TransportFuture<'_, Result<()>> {
        let sender = self.control_send.clone();
        Box::pin(async move {
            framing::send_message(&mut *sender.lock().await, &control_command_message(command))
                .await
        })
    }

    fn send_input(&self, command: ServerInputCommand) -> TransportFuture<'_, Result<()>> {
        let sender = self.input_send.clone();
        Box::pin(async move {
            framing::send_message(&mut *sender.lock().await, &input_command_message(command)).await
        })
    }

    fn send_clipboard(&self, command: ServerClipboardCommand) -> TransportFuture<'_, Result<()>> {
        let sender = self.clipboard_send.clone();
        Box::pin(async move {
            let message = match command {
                ServerClipboardCommand::SetPeerText(text) => Message::ClipboardUpdate {
                    content: ClipboardContent::Text(text),
                },
            };
            framing::send_message(&mut *sender.lock().await, &message).await
        })
    }

    fn shutdown(&self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            if let Some(mut tasks) = self.tasks.lock().await.take() {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            }
        })
    }
}

fn spawn_reader(
    tasks: &mut JoinSet<()>,
    mut recv: RecvStream,
    channel: ServerChannel,
    sender: mpsc::Sender<ServerTransportEvent>,
    map: fn(Message) -> std::result::Result<ServerTransportEvent, String>,
) {
    tasks.spawn(async move {
        loop {
            let event = match framing::recv_message(&mut recv).await {
                Ok(Some(message)) => match map(message) {
                    Ok(event) => event,
                    Err(message) => {
                        ServerTransportEvent::Failed(TransportFailure::new(channel, message))
                    }
                },
                Ok(None) => ServerTransportEvent::Closed(channel),
                Err(error) => {
                    ServerTransportEvent::Failed(TransportFailure::new(channel, error.to_string()))
                }
            };
            let terminal = matches!(
                event,
                ServerTransportEvent::Closed(_) | ServerTransportEvent::Failed(_)
            );
            if sender.send(event).await.is_err() || terminal {
                break;
            }
        }
    });
}

fn map_control_message(message: Message) -> std::result::Result<ServerTransportEvent, String> {
    let event = match message {
        Message::Heartbeat { timestamp } => ServerControlEvent::Heartbeat { timestamp },
        Message::SwitchScreen { direction } => ServerControlEvent::SwitchBackRequested {
            direction: map_direction(direction),
        },
        Message::ScreenResize { screen } => {
            ServerControlEvent::PeerScreenChanged(crate::ports::PeerScreen {
                width: screen.width,
                height: screen.height,
            })
        }
        other => return Err(unexpected_message(ServerChannel::Control, &other)),
    };
    Ok(ServerTransportEvent::Control(event))
}

fn map_clipboard_message(message: Message) -> std::result::Result<ServerTransportEvent, String> {
    match message {
        Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        } => Ok(ServerTransportEvent::Clipboard(
            ServerClipboardEvent::TextChanged(text),
        )),
        other => Err(unexpected_message(ServerChannel::Clipboard, &other)),
    }
}

fn unexpected_message(channel: ServerChannel, message: &Message) -> String {
    format!(
        "Unexpected message on {:?} channel: {}",
        channel,
        protocol::message_summary(message)
    )
}

fn control_command_message(command: ServerControlCommand) -> Message {
    match command {
        ServerControlCommand::AcknowledgeHeartbeat { timestamp } => {
            Message::HeartbeatAck { timestamp }
        }
        ServerControlCommand::LocalScreenChanged(screen) => Message::ScreenResize {
            screen: protocol::ScreenLayout {
                width: screen.width,
                height: screen.height,
            },
        },
        ServerControlCommand::WakePeerDisplay => Message::WakeDisplay,
        ServerControlCommand::ReleasePeerControl => Message::ReleaseControl,
    }
}

fn input_command_message(command: ServerInputCommand) -> Message {
    match command {
        ServerInputCommand::MouseMoved { x, y } => Message::MouseMove { x, y },
        ServerInputCommand::MouseButtonChanged { button, pressed } => {
            Message::MouseButton { button, pressed }
        }
        ServerInputCommand::MouseScrolled { dx, dy, phase } => Message::MouseScroll {
            dx,
            dy,
            phase: unmap_scroll_phase(phase),
        },
        ServerInputCommand::KeyChanged {
            keycode,
            pressed,
            modifiers,
        } => Message::KeyEvent {
            keycode,
            pressed,
            modifiers,
        },
        ServerInputCommand::SwitchToPeer { direction } => Message::SwitchScreen {
            direction: unmap_direction(direction),
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

fn unmap_scroll_phase(phase: PeerScrollPhase) -> ScrollPhase {
    match phase {
        PeerScrollPhase::None => ScrollPhase::None,
        PeerScrollPhase::Began => ScrollPhase::Began,
        PeerScrollPhase::Changed => ScrollPhase::Changed,
        PeerScrollPhase::Ended => ScrollPhase::Ended,
        PeerScrollPhase::MomentumBegan => ScrollPhase::MomentumBegan,
        PeerScrollPhase::MomentumChanged => ScrollPhase::MomentumChanged,
        PeerScrollPhase::MomentumEnded => ScrollPhase::MomentumEnded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::PeerScreen;

    #[test]
    fn wire_messages_are_translated_to_server_events() {
        assert_eq!(
            map_control_message(Message::SwitchScreen {
                direction: Direction::Left,
            })
            .unwrap(),
            ServerTransportEvent::Control(ServerControlEvent::SwitchBackRequested {
                direction: PeerDirection::Left,
            })
        );
        assert_eq!(
            map_control_message(Message::ScreenResize {
                screen: protocol::ScreenLayout {
                    width: 1920,
                    height: 1080,
                },
            })
            .unwrap(),
            ServerTransportEvent::Control(ServerControlEvent::PeerScreenChanged(PeerScreen {
                width: 1920,
                height: 1080,
            }))
        );
        assert_eq!(
            map_clipboard_message(Message::ClipboardUpdate {
                content: ClipboardContent::Text("hello".to_string()),
            })
            .unwrap(),
            ServerTransportEvent::Clipboard(ServerClipboardEvent::TextChanged("hello".to_string()))
        );
    }

    #[test]
    fn server_commands_are_translated_to_wire_messages() {
        assert!(matches!(
            control_command_message(ServerControlCommand::AcknowledgeHeartbeat { timestamp: 7 }),
            Message::HeartbeatAck { timestamp: 7 }
        ));
        assert!(matches!(
            input_command_message(ServerInputCommand::MouseScrolled {
                dx: 1.0,
                dy: -2.0,
                phase: PeerScrollPhase::Changed,
            }),
            Message::MouseScroll {
                phase: ScrollPhase::Changed,
                ..
            }
        ));
        assert!(matches!(
            control_command_message(ServerControlCommand::WakePeerDisplay),
            Message::WakeDisplay
        ));
        assert!(matches!(
            control_command_message(ServerControlCommand::ReleasePeerControl),
            Message::ReleaseControl
        ));
    }

    #[test]
    fn unexpected_server_channel_messages_are_failures() {
        let error = map_control_message(Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        })
        .unwrap_err();
        assert!(error.contains("Control channel"));
        assert!(error.contains("KeyEvent"));
    }
}
