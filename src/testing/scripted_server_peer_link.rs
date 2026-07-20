use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use color_eyre::eyre::{eyre, Result};
use tokio::sync::mpsc;

use crate::ports::{
    ServerChannel, ServerClipboardCommand, ServerControlCommand, ServerInputCommand,
    ServerPeerLink, ServerTransportEvent, TransportFailure, TransportFuture,
};
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerSendOperation {
    Control,
    Input,
    Clipboard,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerPeerObservation {
    EventDelivered(ServerTransportEvent),
    ControlSend(ServerControlCommand),
    InputSend(ServerInputCommand),
    ClipboardSend(ServerClipboardCommand),
    SendFailed {
        operation: ServerSendOperation,
        message: String,
    },
    Shutdown,
}

#[derive(Default)]
struct SendScripts {
    control: VecDeque<std::result::Result<(), String>>,
    input: VecDeque<std::result::Result<(), String>>,
    clipboard: VecDeque<std::result::Result<(), String>>,
}

struct SharedState {
    sender: mpsc::UnboundedSender<ServerTransportEvent>,
    sends: Mutex<SendScripts>,
    pending_events: AtomicUsize,
    shutdown: AtomicBool,
    observations: ObservationLog<ServerPeerObservation>,
}

/// In-memory server peer with FIFO channel events and outbound results.
#[derive(Clone)]
pub struct ScriptedServerPeerLink {
    state: Arc<SharedState>,
    receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<ServerTransportEvent>>>,
}

impl ScriptedServerPeerLink {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<ServerPeerObservation>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            state: Arc::new(SharedState {
                sender,
                sends: Mutex::new(SendScripts::default()),
                pending_events: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
                observations,
            }),
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
        }
    }

    pub fn push_event(&self, event: ServerTransportEvent) {
        self.state.pending_events.fetch_add(1, Ordering::SeqCst);
        if self.state.sender.send(event).is_err() {
            self.state.pending_events.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn push_channel_close(&self, channel: ServerChannel) {
        self.push_event(ServerTransportEvent::Closed(channel));
    }

    pub fn push_channel_failure(&self, channel: ServerChannel, message: impl Into<String>) {
        self.push_event(ServerTransportEvent::Failed(TransportFailure::new(
            channel, message,
        )));
    }

    pub fn succeed_next_send(&self, operation: ServerSendOperation) {
        self.push_send(operation, Ok(()));
    }

    pub fn fail_next_send(&self, operation: ServerSendOperation, message: impl Into<String>) {
        self.push_send(operation, Err(message.into()));
    }

    pub fn pending_events(&self) -> usize {
        self.state.pending_events.load(Ordering::SeqCst)
    }

    pub fn is_shutdown(&self) -> bool {
        self.state.shutdown.load(Ordering::SeqCst)
    }

    pub fn observations(&self) -> ObservationLog<ServerPeerObservation> {
        self.state.observations.clone()
    }

    fn push_send(&self, operation: ServerSendOperation, result: std::result::Result<(), String>) {
        let mut sends = lock_recover(&self.state.sends);
        match operation {
            ServerSendOperation::Control => sends.control.push_back(result),
            ServerSendOperation::Input => sends.input.push_back(result),
            ServerSendOperation::Clipboard => sends.clipboard.push_back(result),
        }
    }

    fn take_send(&self, operation: ServerSendOperation) -> Result<()> {
        let result = {
            let mut sends = lock_recover(&self.state.sends);
            match operation {
                ServerSendOperation::Control => sends.control.pop_front(),
                ServerSendOperation::Input => sends.input.pop_front(),
                ServerSendOperation::Clipboard => sends.clipboard.pop_front(),
            }
        };
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => {
                self.state
                    .observations
                    .record(ServerPeerObservation::SendFailed {
                        operation,
                        message: message.clone(),
                    });
                Err(eyre!(message))
            }
            None => {
                let message = format!("unexpected {operation:?} send: no scripted action");
                self.state
                    .observations
                    .record(ServerPeerObservation::SendFailed {
                        operation,
                        message: message.clone(),
                    });
                Err(eyre!(message))
            }
        }
    }
}

impl Default for ScriptedServerPeerLink {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerPeerLink for ScriptedServerPeerLink {
    fn next_event(&self) -> TransportFuture<'_, Option<ServerTransportEvent>> {
        Box::pin(async move {
            let event = self.receiver.lock().await.recv().await;
            if let Some(event) = &event {
                self.state.pending_events.fetch_sub(1, Ordering::SeqCst);
                self.state
                    .observations
                    .record(ServerPeerObservation::EventDelivered(event.clone()));
            }
            event
        })
    }

    fn send_control(&self, command: ServerControlCommand) -> TransportFuture<'_, Result<()>> {
        self.state
            .observations
            .record(ServerPeerObservation::ControlSend(command));
        let result = self.take_send(ServerSendOperation::Control);
        Box::pin(async move { result })
    }

    fn send_input(&self, command: ServerInputCommand) -> TransportFuture<'_, Result<()>> {
        self.state
            .observations
            .record(ServerPeerObservation::InputSend(command));
        let result = self.take_send(ServerSendOperation::Input);
        Box::pin(async move { result })
    }

    fn send_clipboard(&self, command: ServerClipboardCommand) -> TransportFuture<'_, Result<()>> {
        self.state
            .observations
            .record(ServerPeerObservation::ClipboardSend(command));
        let result = self.take_send(ServerSendOperation::Clipboard);
        Box::pin(async move { result })
    }

    fn shutdown(&self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            self.state.shutdown.store(true, Ordering::SeqCst);
            self.state
                .observations
                .record(ServerPeerObservation::Shutdown);
        })
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
