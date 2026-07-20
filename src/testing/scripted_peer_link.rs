use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::ports::{
    ClientChannel, ClientClipboardCommand, ClientControlCommand, ClientPeerLink,
    ClientTransportEvent, TransportFailure, TransportFuture,
};
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerSendOperation {
    Control,
    Clipboard,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PeerLinkObservation {
    EventDelivered(ClientTransportEvent),
    ControlSend(ClientControlCommand),
    ClipboardSend(ClientClipboardCommand),
    SendFailed {
        operation: PeerSendOperation,
        message: String,
    },
}

#[derive(Default)]
struct SendScripts {
    control: VecDeque<std::result::Result<(), String>>,
    clipboard: VecDeque<std::result::Result<(), String>>,
}

struct SharedState {
    sender: mpsc::UnboundedSender<ClientTransportEvent>,
    sends: Mutex<SendScripts>,
    pending_events: AtomicUsize,
    observations: ObservationLog<PeerLinkObservation>,
}

/// In-memory peer link with independent delayed/blocked channel events and
/// FIFO-scripted outbound results.
#[derive(Clone)]
pub struct ScriptedPeerLink {
    state: Arc<SharedState>,
    receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<ClientTransportEvent>>>,
}

impl ScriptedPeerLink {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<PeerLinkObservation>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            state: Arc::new(SharedState {
                sender,
                sends: Mutex::new(SendScripts::default()),
                pending_events: AtomicUsize::new(0),
                observations,
            }),
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
        }
    }

    pub fn push_event(&self, event: ClientTransportEvent) {
        self.state.pending_events.fetch_add(1, Ordering::SeqCst);
        if self.state.sender.send(event).is_err() {
            self.state.pending_events.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Schedule an event on its own task so a delayed channel does not block
    /// ready events from other channels. Tokio virtual time controls the delay.
    pub fn push_delayed_event(&self, delay: Duration, event: ClientTransportEvent) {
        self.state.pending_events.fetch_add(1, Ordering::SeqCst);
        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if state.sender.send(event).is_err() {
                state.pending_events.fetch_sub(1, Ordering::SeqCst);
            }
        });
    }

    pub fn push_channel_failure(&self, channel: ClientChannel, message: impl Into<String>) {
        self.push_event(ClientTransportEvent::Failed(TransportFailure::new(
            channel, message,
        )));
    }

    pub fn push_channel_close(&self, channel: ClientChannel) {
        self.push_event(ClientTransportEvent::Closed(channel));
    }

    pub fn block_event(&self, event: ClientTransportEvent) -> BlockingPeerEvent {
        self.state.pending_events.fetch_add(1, Ordering::SeqCst);
        let (completion, completed) = oneshot::channel();
        let entered = Arc::new(GateEntered {
            value: AtomicBool::new(false),
            changed: Notify::new(),
        });
        let task_entered = entered.clone();
        let state = self.state.clone();
        let channel = event.channel();
        tokio::spawn(async move {
            task_entered.value.store(true, Ordering::SeqCst);
            task_entered.changed.notify_waiters();
            let event = match completed.await {
                Ok(Ok(())) => event,
                Ok(Err(message)) => {
                    ClientTransportEvent::Failed(TransportFailure::new(channel, message))
                }
                Err(_) => ClientTransportEvent::Failed(TransportFailure::new(
                    channel,
                    "blocking peer event controller dropped",
                )),
            };
            if state.sender.send(event).is_err() {
                state.pending_events.fetch_sub(1, Ordering::SeqCst);
            }
        });
        BlockingPeerEvent {
            entered,
            completion: Mutex::new(Some(completion)),
        }
    }

    pub fn succeed_next_control_send(&self) {
        lock_recover(&self.state.sends).control.push_back(Ok(()));
    }

    pub fn fail_next_control_send(&self, message: impl Into<String>) {
        lock_recover(&self.state.sends)
            .control
            .push_back(Err(message.into()));
    }

    pub fn succeed_next_clipboard_send(&self) {
        lock_recover(&self.state.sends).clipboard.push_back(Ok(()));
    }

    pub fn fail_next_clipboard_send(&self, message: impl Into<String>) {
        lock_recover(&self.state.sends)
            .clipboard
            .push_back(Err(message.into()));
    }

    pub fn pending_events(&self) -> usize {
        self.state.pending_events.load(Ordering::SeqCst)
    }

    pub fn remaining_control_sends(&self) -> usize {
        lock_recover(&self.state.sends).control.len()
    }

    pub fn remaining_clipboard_sends(&self) -> usize {
        lock_recover(&self.state.sends).clipboard.len()
    }

    pub fn observations(&self) -> ObservationLog<PeerLinkObservation> {
        self.state.observations.clone()
    }

    fn take_send_result(&self, operation: PeerSendOperation) -> Result<()> {
        let result = {
            let mut scripts = lock_recover(&self.state.sends);
            match operation {
                PeerSendOperation::Control => scripts.control.pop_front(),
                PeerSendOperation::Clipboard => scripts.clipboard.pop_front(),
            }
        };
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => {
                self.state
                    .observations
                    .record(PeerLinkObservation::SendFailed {
                        operation,
                        message: message.clone(),
                    });
                Err(eyre!(message))
            }
            None => {
                let message = format!("unexpected {operation:?} send: no scripted action");
                self.state
                    .observations
                    .record(PeerLinkObservation::SendFailed {
                        operation,
                        message: message.clone(),
                    });
                Err(eyre!(message))
            }
        }
    }
}

impl Default for ScriptedPeerLink {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPeerLink for ScriptedPeerLink {
    fn next_event(&self) -> TransportFuture<'_, Option<ClientTransportEvent>> {
        Box::pin(async move {
            let event = self.receiver.lock().await.recv().await;
            if let Some(event) = &event {
                self.state.pending_events.fetch_sub(1, Ordering::SeqCst);
                self.state
                    .observations
                    .record(PeerLinkObservation::EventDelivered(event.clone()));
            }
            event
        })
    }

    fn send_control(&self, command: ClientControlCommand) -> TransportFuture<'_, Result<()>> {
        self.state
            .observations
            .record(PeerLinkObservation::ControlSend(command));
        let result = self.take_send_result(PeerSendOperation::Control);
        Box::pin(async move { result })
    }

    fn send_clipboard(&self, command: ClientClipboardCommand) -> TransportFuture<'_, Result<()>> {
        self.state
            .observations
            .record(PeerLinkObservation::ClipboardSend(command));
        let result = self.take_send_result(PeerSendOperation::Clipboard);
        Box::pin(async move { result })
    }
}

struct GateEntered {
    value: AtomicBool,
    changed: Notify,
}

/// Controller for one blocked inbound peer event.
pub struct BlockingPeerEvent {
    entered: Arc<GateEntered>,
    completion: Mutex<Option<oneshot::Sender<std::result::Result<(), String>>>>,
}

impl BlockingPeerEvent {
    pub async fn wait_until_entered(&self) {
        while !self.entered.value.load(Ordering::SeqCst) {
            self.entered.changed.notified().await;
        }
    }

    pub fn release(&self) {
        self.complete(Ok(()));
    }

    pub fn fail(&self, message: impl Into<String>) {
        self.complete(Err(message.into()));
    }

    fn complete(&self, outcome: std::result::Result<(), String>) {
        if let Some(completion) = lock_recover(&self.completion).take() {
            let _ = completion.send(outcome);
        }
    }
}

impl Drop for BlockingPeerEvent {
    fn drop(&mut self) {
        self.complete(Err("blocking peer event controller dropped".to_string()));
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use crate::ports::{ClientClipboardEvent, ClientControlEvent, ClientInputEvent, PeerScreen};

    use super::*;

    #[tokio::test]
    async fn closure_and_failure_are_injected_as_typed_channel_events() {
        let peer = ScriptedPeerLink::new();
        peer.push_channel_close(ClientChannel::Input);
        peer.push_channel_failure(ClientChannel::Clipboard, "clipboard reset");

        assert_eq!(
            peer.next_event().await,
            Some(ClientTransportEvent::Closed(ClientChannel::Input))
        );
        assert!(matches!(
            peer.next_event().await,
            Some(ClientTransportEvent::Failed(TransportFailure {
                channel: ClientChannel::Clipboard,
                ..
            }))
        ));
        assert_eq!(peer.pending_events(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn per_channel_delays_do_not_block_ready_channels() {
        let peer = ScriptedPeerLink::new();
        peer.push_delayed_event(
            Duration::from_secs(10),
            ClientTransportEvent::Clipboard(ClientClipboardEvent::TextChanged("later".to_string())),
        );
        peer.push_delayed_event(
            Duration::from_secs(1),
            ClientTransportEvent::Control(ClientControlEvent::PeerScreenChanged(PeerScreen {
                width: 1920,
                height: 1080,
            })),
        );
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            peer.next_event().await,
            Some(ClientTransportEvent::Control(_))
        ));
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(matches!(
            peer.next_event().await,
            Some(ClientTransportEvent::Clipboard(_))
        ));
    }

    #[tokio::test]
    async fn blocked_event_waits_for_explicit_release() {
        let peer = ScriptedPeerLink::new();
        let gate = peer.block_event(ClientTransportEvent::Input(ClientInputEvent::MouseMoved {
            x: 10,
            y: 20,
        }));
        gate.wait_until_entered().await;

        let receiver = peer.clone();
        let waiting = tokio::spawn(async move { receiver.next_event().await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        gate.release();
        assert_eq!(
            waiting.await.unwrap(),
            Some(ClientTransportEvent::Input(ClientInputEvent::MouseMoved {
                x: 10,
                y: 20,
            }))
        );
    }

    #[tokio::test]
    async fn outbound_calls_are_fifo_scripted_and_observed() {
        let peer = ScriptedPeerLink::new();
        peer.succeed_next_control_send();
        peer.fail_next_clipboard_send("clipboard unavailable");

        peer.send_control(ClientControlCommand::Heartbeat { timestamp: 1 })
            .await
            .unwrap();
        assert_eq!(
            peer.send_clipboard(ClientClipboardCommand::SetPeerText("x".to_string()))
                .await
                .unwrap_err()
                .to_string(),
            "clipboard unavailable"
        );
        assert!(peer
            .send_control(ClientControlCommand::Heartbeat { timestamp: 2 })
            .await
            .unwrap_err()
            .to_string()
            .contains("no scripted action"));
        assert_eq!(peer.observations().len(), 5);
    }
}
