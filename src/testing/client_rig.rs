use std::time::Duration;

use crate::app::CancellationToken;
use crate::net::protocol::BUILD_VERSION;
use crate::testing::{
    FakeDisplaySessionControl, FakeUpdateInstaller, MemoryClipboard, MemoryTrustStore,
    RecordingInjector, RecordingInjectorFactory, RestartRecorder, ScriptedPairingPrompt,
    ScriptedPeerLink, ScriptedReleaseRepository, TaskTracker,
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
    use crate::ports::TrustStore;

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
