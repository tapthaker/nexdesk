use std::collections::BTreeSet;
use std::time::Duration;

use crate::app::CancellationToken;
use crate::net::protocol::BUILD_VERSION;
use crate::testing::{
    FakeDisplaySessionControl, FakeUpdateInstaller, MemoryClipboard, MemoryTrustStore,
    PeerLinkObservation, RecordingInjector, RecordingInjectorFactory, RecordingStatusSink,
    RestartRecorder, ScriptedPairingPrompt, ScriptedPeerLink, ScriptedReleaseRepository,
    TaskTracker,
};

pub const DEFAULT_CLIENT_PEER_FINGERPRINT: &str = "test-peer-fingerprint";
pub const DEFAULT_CLIENT_SCREEN: (u32, u32) = (1920, 1080);

/// Collection of stateful fakes used by deterministic client scenarios.
///
/// Defaults model an already-trusted peer on a 1920x1080 client. Exceptional
/// behavior and outbound peer sends remain explicitly scripted by each test.
pub struct ClientRig {
    pub peer: ScriptedPeerLink,
    pub injector: RecordingInjector,
    pub injector_factory: RecordingInjectorFactory,
    pub display: FakeDisplaySessionControl,
    pub trust: MemoryTrustStore,
    pub pairing: ScriptedPairingPrompt,
    pub releases: ScriptedReleaseRepository,
    pub installer: FakeUpdateInstaller,
    pub restart: RestartRecorder,
    pub clipboard: MemoryClipboard,
    pub status: RecordingStatusSink,
    pub tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl ClientRig {
    pub fn new() -> Self {
        let injector = RecordingInjector::new(DEFAULT_CLIENT_SCREEN);
        let releases = ScriptedReleaseRepository::new();
        // A same-version release is the normal no-update path.
        releases.push_latest_release(BUILD_VERSION);

        Self {
            peer: ScriptedPeerLink::new(),
            injector_factory: RecordingInjectorFactory::new(injector.clone()),
            injector,
            display: FakeDisplaySessionControl::new(),
            trust: MemoryTrustStore::with_trusted([DEFAULT_CLIENT_PEER_FINGERPRINT.to_string()]),
            pairing: ScriptedPairingPrompt::new(),
            releases,
            installer: FakeUpdateInstaller::new(),
            restart: RestartRecorder::new(),
            clipboard: MemoryClipboard::new(),
            status: RecordingStatusSink::new(),
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

    /// Yield until scripted peer events have been consumed and spawned work
    /// has had several scheduler turns with no new peer event.
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
            "client rig did not become idle: {} peer event(s) remain",
            self.peer.pending_events()
        );
    }

    /// Advance Tokio's virtual clock, then settle all newly-ready work.
    pub async fn advance_time(&self, duration: Duration) {
        tokio::time::advance(duration).await;
        self.run_until_idle().await;
    }

    pub fn assert_pressed_inputs(&self, keys: &[u32], buttons: &[u8]) {
        assert_eq!(
            self.injector.pressed_keys(),
            keys.iter().copied().collect::<BTreeSet<_>>(),
            "pressed key state"
        );
        assert_eq!(
            self.injector.pressed_buttons(),
            buttons.iter().copied().collect::<BTreeSet<_>>(),
            "pressed button state"
        );
    }

    pub fn assert_cursor_visible(&self, expected: bool) {
        assert_eq!(
            self.injector.cursor_visibility().last().copied(),
            Some(expected),
            "latest cursor visibility"
        );
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

    pub fn assert_outbound_peer_messages(&self, expected: &[PeerLinkObservation]) {
        let actual = self
            .peer
            .observations()
            .snapshot()
            .into_iter()
            .filter_map(|entry| match entry.event {
                event @ (PeerLinkObservation::ControlSend(_)
                | PeerLinkObservation::ClipboardSend(_)) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "outbound peer messages");
    }

    pub fn assert_tasks_completed(&self) {
        if let Err(error) = self.tasks.ensure_idle() {
            panic!("client rig tasks did not complete: {error}");
        }
    }
}

impl Default for ClientRig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::inject::InputInjectorFactory;
    use crate::net::protocol::Message;
    use crate::ports::{ClientControlCommand, ClientPeerLink, StatusSink, TrustStore};
    use crate::status::RuntimeStatus;

    #[test]
    fn defaults_model_a_trusted_peer_and_expose_shared_fakes() {
        let rig = ClientRig::new();

        assert!(rig
            .trust
            .is_trusted(DEFAULT_CLIENT_PEER_FINGERPRINT)
            .unwrap());
        assert_eq!(
            rig.injector_factory
                .create()
                .unwrap()
                .screen_size()
                .unwrap(),
            DEFAULT_CLIENT_SCREEN
        );
        assert_eq!(rig.releases.remaining_latest_actions(), 1);
        assert_eq!(rig.pairing.remaining_actions(), 0);
        assert!(rig.tasks.is_idle());
    }

    #[tokio::test]
    async fn explicit_shutdown_is_shared_with_scenario_tasks() {
        let rig = ClientRig::new();
        let token = rig.shutdown_token();
        let tasks = rig.tasks.clone();
        let session = tokio::spawn(async move {
            tasks.run("client session", token.cancelled()).await;
        });

        rig.shutdown();
        rig.run_until_idle().await;
        session.await.unwrap();

        assert!(rig.is_shutdown());
        assert!(rig.tasks.is_idle());
    }

    #[tokio::test]
    async fn assertion_helpers_cover_client_visible_state() {
        let rig = ClientRig::new();
        let mut injector = rig.injector_factory.create().unwrap();
        injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        injector.set_cursor_visible(false).unwrap();
        rig.status
            .write(RuntimeStatus::new("client", "connected"))
            .unwrap();
        rig.peer.succeed_next_control_send();
        rig.peer
            .send_control(ClientControlCommand::Heartbeat { timestamp: 7 })
            .await
            .unwrap();

        rig.assert_pressed_inputs(&[30], &[]);
        rig.assert_cursor_visible(false);
        rig.assert_status_history(&["connected"]);
        rig.assert_outbound_peer_messages(&[PeerLinkObservation::ControlSend(
            ClientControlCommand::Heartbeat { timestamp: 7 },
        )]);
        rig.assert_tasks_completed();
    }

    #[tokio::test(start_paused = true)]
    async fn virtual_time_advancement_settles_ready_work() {
        let rig = ClientRig::new();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_completed = completed.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            task_completed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::task::yield_now().await;

        rig.advance_time(Duration::from_secs(30)).await;

        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
    }
}
