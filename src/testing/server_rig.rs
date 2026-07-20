use std::time::Duration;

use crate::app::CancellationToken;
use crate::testing::{
    FakeDisplaySessionControl, GrabChange, MemoryClipboard, RecordingStatusSink,
    ScriptedCaptureFactory, ScriptedCapturer, ScriptedServerPeerLink, ScriptedSessionLockSource,
    ServerPeerObservation, TaskTracker, VirtualClock,
};

pub const DEFAULT_SERVER_SCREEN: (u32, u32) = (1920, 1080);

/// Collection of stateful fakes used by deterministic server scenarios.
///
/// Defaults model an unlocked local session and a 1920x1080 local screen.
/// Peer events and outbound send outcomes remain explicit in each scenario.
pub struct ServerRig {
    pub capture: ScriptedCapturer,
    pub capture_factory: ScriptedCaptureFactory,
    pub peer: ScriptedServerPeerLink,
    pub clipboard: MemoryClipboard,
    pub lock: ScriptedSessionLockSource,
    pub display: FakeDisplaySessionControl,
    pub status: RecordingStatusSink,
    pub clock: VirtualClock,
    pub tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl ServerRig {
    pub fn new() -> Self {
        let capture = ScriptedCapturer::new();
        capture.push_screen_size(DEFAULT_SERVER_SCREEN.0, DEFAULT_SERVER_SCREEN.1);

        Self {
            capture_factory: ScriptedCaptureFactory::new(capture.clone()),
            capture,
            peer: ScriptedServerPeerLink::new(),
            clipboard: MemoryClipboard::new(),
            lock: ScriptedSessionLockSource::default(),
            display: FakeDisplaySessionControl::new(),
            status: RecordingStatusSink::new(),
            clock: VirtualClock,
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
        }
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub async fn run_until_idle(&self) {
        let mut quiet_turns = 0;
        for _ in 0..1024 {
            tokio::task::yield_now().await;
            if self.peer.pending_events() == 0 {
                quiet_turns += 1;
                if quiet_turns == 3 {
                    return;
                }
            } else {
                quiet_turns = 0;
            }
        }
        panic!(
            "server rig did not become idle: {} peer event(s) remain",
            self.peer.pending_events()
        );
    }

    pub async fn advance_time(&self, duration: Duration) {
        self.clock.advance(duration).await;
        self.run_until_idle().await;
    }

    pub fn assert_grab_history(&self, expected: &[GrabChange]) {
        assert_eq!(self.capture.grab_history(), expected, "input grab history");
    }

    pub fn assert_status_history(&self, expected_states: &[&str]) {
        assert_eq!(
            self.status.states(),
            expected_states
                .iter()
                .map(|state| (*state).to_string())
                .collect::<Vec<_>>(),
            "runtime status history"
        );
    }

    pub fn assert_outbound_peer_messages(&self, expected: &[ServerPeerObservation]) {
        let actual = self
            .peer
            .observations()
            .snapshot()
            .into_iter()
            .filter_map(|entry| match entry.event {
                event @ (ServerPeerObservation::ControlSend(_)
                | ServerPeerObservation::InputSend(_)
                | ServerPeerObservation::ClipboardSend(_)) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "outbound peer messages");
    }

    pub fn assert_tasks_completed(&self) {
        if let Err(error) = self.tasks.ensure_idle() {
            panic!("server rig tasks did not complete: {error}");
        }
    }
}

impl Default for ServerRig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::capture::{InputCapture, InputCaptureFactory};
    use crate::ports::{
        Clipboard, LocalSessionLockSource, ServerInputCommand, ServerPeerLink, StatusSink,
    };
    use crate::status::RuntimeStatus;
    use crate::testing::ServerSendOperation;

    #[test]
    fn defaults_expose_deterministic_server_fakes() {
        let rig = ServerRig::new();
        let capture = rig.capture_factory.create().unwrap();

        assert_eq!(capture.screen_size().unwrap(), DEFAULT_SERVER_SCREEN);
        assert!(!rig.lock.is_locked().unwrap());
        assert!(rig.clipboard.read_text().unwrap().is_none());
        assert!(rig.tasks.is_idle());
        assert!(!rig.is_shutdown());
    }

    #[tokio::test]
    async fn assertion_helpers_cover_server_visible_state() {
        let rig = ServerRig::new();
        let mut capture = rig.capture.clone();
        capture.set_grab(true).unwrap();
        rig.status
            .write(RuntimeStatus::new("server", "connected"))
            .unwrap();
        rig.peer.succeed_next_send(ServerSendOperation::Input);
        rig.peer
            .send_input(ServerInputCommand::MouseMoved { x: 4, y: -2 })
            .await
            .unwrap();

        rig.assert_grab_history(&[GrabChange::All(true)]);
        rig.assert_status_history(&["connected"]);
        rig.assert_outbound_peer_messages(&[ServerPeerObservation::InputSend(
            ServerInputCommand::MouseMoved { x: 4, y: -2 },
        )]);
        rig.assert_tasks_completed();
    }

    #[tokio::test(start_paused = true)]
    async fn virtual_clock_and_shutdown_are_explicit() {
        let rig = ServerRig::new();
        let started = rig.clock.now();
        rig.advance_time(Duration::from_secs(5)).await;
        assert_eq!(
            rig.clock.now().duration_since(started),
            Duration::from_secs(5)
        );

        rig.shutdown();
        assert!(rig.is_shutdown());
    }
}
