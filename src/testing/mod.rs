mod client_rig;
mod fake_display_control;
mod fake_update;
mod memory_clipboard;
mod memory_trust_store;
mod observation;
mod recording_injector;
mod recording_status;
mod scripted_capturer;
mod scripted_pairing_prompt;
mod scripted_peer_link;
mod scripted_server_peer_link;
mod task_tracker;

pub use client_rig::{ClientRig, DEFAULT_CLIENT_PEER_FINGERPRINT, DEFAULT_CLIENT_SCREEN};
pub use fake_display_control::{
    BlockingDisplayCall, DisplayObservation, DisplayOperation, FakeDisplaySessionControl,
};
pub use fake_update::{
    AssetStreamStep, FakeUpdateInstaller, InstalledUpdate, RestartRecorder,
    ScriptedReleaseRepository, UpdateObservation, UpdateOperation,
};
pub use memory_clipboard::{
    BlockingClipboardCall, ClipboardChange, ClipboardObservation, ClipboardOperation,
    MemoryClipboard,
};
pub use memory_trust_store::{MemoryTrustStore, TrustObservation, TrustOperation};
pub use observation::{Observation, ObservationLog};
pub use recording_injector::{
    InjectorObservation, InjectorOperation, RecordedInput, RecordingInjector,
    RecordingInjectorFactory,
};
pub use recording_status::RecordingStatusSink;
pub use scripted_capturer::{
    CaptureObservation, CaptureOperation, GrabChange, ScriptedCaptureFactory, ScriptedCapturer,
};
pub use scripted_pairing_prompt::{PairingPromptObservation, ScriptedPairingPrompt};
pub use scripted_peer_link::{
    BlockingPeerEvent, PeerLinkObservation, PeerSendOperation, ScriptedPeerLink,
};
pub use scripted_server_peer_link::{
    ScriptedServerPeerLink, ServerPeerObservation, ServerSendOperation,
};
pub use task_tracker::{RunningTask, RunningTasksError, TaskGuard, TaskTracker};
