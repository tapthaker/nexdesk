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
}
