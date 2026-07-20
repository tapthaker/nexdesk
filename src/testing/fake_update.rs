use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use color_eyre::eyre::eyre;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

use crate::app::RestartReason;
use crate::ports::{Release, ReleaseAsset, ReleaseRepository, UpdateFuture, UpdateInstaller};
use crate::testing::ObservationLog;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateOperation {
    LatestRelease,
    StreamAsset,
    Install,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateObservation {
    LatestReleaseRequested,
    LatestReleaseReturned {
        version: String,
    },
    AssetRequested {
        version: String,
    },
    AssetOpened {
        version: String,
        declared_size: Option<u64>,
    },
    AssetBytesRead {
        version: String,
        bytes: usize,
    },
    AssetStreamCompleted {
        version: String,
    },
    InstallRequested {
        version: String,
        declared_size: Option<u64>,
    },
    Installed {
        version: String,
        bytes: usize,
    },
    RestartRequested {
        reason: RestartReason,
    },
    Failed {
        operation: UpdateOperation,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetStreamStep {
    Bytes(Vec<u8>),
    Fail(String),
}

impl AssetStreamStep {
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(bytes.into())
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self::Fail(message.into())
    }
}

enum LatestAction {
    Return(Release),
    Fail(String),
}

enum AssetAction {
    Stream {
        version: String,
        declared_size: Option<u64>,
        steps: VecDeque<AssetStreamStep>,
    },
    Fail {
        version: String,
        message: String,
    },
}

#[derive(Default)]
struct RepositoryState {
    latest: VecDeque<LatestAction>,
    assets: VecDeque<AssetAction>,
}

/// Scripted release lookup and asset source. Every call consumes one FIFO action.
#[derive(Clone)]
pub struct ScriptedReleaseRepository {
    state: Arc<Mutex<RepositoryState>>,
    observations: ObservationLog<UpdateObservation>,
}

impl ScriptedReleaseRepository {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<UpdateObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RepositoryState::default())),
            observations,
        }
    }

    pub fn push_latest_release(&self, version: impl Into<String>) {
        lock_recover(&self.state)
            .latest
            .push_back(LatestAction::Return(Release::new(version)));
    }

    pub fn fail_next_latest(&self, message: impl Into<String>) {
        lock_recover(&self.state)
            .latest
            .push_back(LatestAction::Fail(message.into()));
    }

    pub fn push_asset(
        &self,
        version: impl Into<String>,
        declared_size: Option<u64>,
        steps: impl IntoIterator<Item = AssetStreamStep>,
    ) {
        lock_recover(&self.state)
            .assets
            .push_back(AssetAction::Stream {
                version: version.into(),
                declared_size,
                steps: steps.into_iter().collect(),
            });
    }

    pub fn fail_next_asset(&self, version: impl Into<String>, message: impl Into<String>) {
        lock_recover(&self.state)
            .assets
            .push_back(AssetAction::Fail {
                version: version.into(),
                message: message.into(),
            });
    }

    pub fn remaining_latest_actions(&self) -> usize {
        lock_recover(&self.state).latest.len()
    }

    pub fn remaining_asset_actions(&self) -> usize {
        lock_recover(&self.state).assets.len()
    }

    pub fn observations(&self) -> ObservationLog<UpdateObservation> {
        self.observations.clone()
    }

    fn failure(&self, operation: UpdateOperation, message: String) -> color_eyre::eyre::Report {
        self.observations.record(UpdateObservation::Failed {
            operation,
            message: message.clone(),
        });
        eyre!(message)
    }
}

impl Default for ScriptedReleaseRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseRepository for ScriptedReleaseRepository {
    fn latest_release(&self) -> UpdateFuture<'_, Release> {
        Box::pin(async move {
            self.observations
                .record(UpdateObservation::LatestReleaseRequested);
            let action = lock_recover(&self.state).latest.pop_front();
            match action {
                Some(LatestAction::Return(release)) => {
                    self.observations
                        .record(UpdateObservation::LatestReleaseReturned {
                            version: release.version.clone(),
                        });
                    Ok(release)
                }
                Some(LatestAction::Fail(message)) => {
                    Err(self.failure(UpdateOperation::LatestRelease, message))
                }
                None => Err(self.failure(
                    UpdateOperation::LatestRelease,
                    "unexpected release lookup: no scripted action".to_string(),
                )),
            }
        })
    }

    fn stream_asset<'a>(&'a self, release: &'a Release) -> UpdateFuture<'a, ReleaseAsset> {
        Box::pin(async move {
            self.observations.record(UpdateObservation::AssetRequested {
                version: release.version.clone(),
            });
            let action = lock_recover(&self.state).assets.pop_front();
            let (expected_version, declared_size, steps) = match action {
                Some(AssetAction::Stream {
                    version,
                    declared_size,
                    steps,
                }) => (version, declared_size, steps),
                Some(AssetAction::Fail { version, message }) => {
                    if version != release.version {
                        return Err(self.failure(
                            UpdateOperation::StreamAsset,
                            version_mismatch_message(&version, &release.version),
                        ));
                    }
                    return Err(self.failure(UpdateOperation::StreamAsset, message));
                }
                None => {
                    return Err(self.failure(
                        UpdateOperation::StreamAsset,
                        format!(
                            "unexpected asset request for {}: no scripted action",
                            release.version
                        ),
                    ));
                }
            };
            if expected_version != release.version {
                return Err(self.failure(
                    UpdateOperation::StreamAsset,
                    version_mismatch_message(&expected_version, &release.version),
                ));
            }

            self.observations.record(UpdateObservation::AssetOpened {
                version: release.version.clone(),
                declared_size,
            });
            Ok(ReleaseAsset::new(
                declared_size,
                Box::pin(ScriptedAssetReader {
                    version: release.version.clone(),
                    steps,
                    current: None,
                    completed: false,
                    observations: self.observations.clone(),
                }),
            ))
        })
    }
}

fn version_mismatch_message(expected: &str, actual: &str) -> String {
    format!("scripted asset expected release {expected}, got {actual}")
}

struct ScriptedAssetReader {
    version: String,
    steps: VecDeque<AssetStreamStep>,
    current: Option<(Vec<u8>, usize)>,
    completed: bool,
    observations: ObservationLog<UpdateObservation>,
}

impl AsyncRead for ScriptedAssetReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some((bytes, offset)) = &mut this.current {
                let count = buffer.remaining().min(bytes.len().saturating_sub(*offset));
                if count == 0 {
                    this.current = None;
                    continue;
                }
                buffer.put_slice(&bytes[*offset..*offset + count]);
                *offset += count;
                this.observations.record(UpdateObservation::AssetBytesRead {
                    version: this.version.clone(),
                    bytes: count,
                });
                if *offset == bytes.len() {
                    this.current = None;
                }
                return Poll::Ready(Ok(()));
            }

            match this.steps.pop_front() {
                Some(AssetStreamStep::Bytes(bytes)) if bytes.is_empty() => continue,
                Some(AssetStreamStep::Bytes(bytes)) => {
                    this.current = Some((bytes, 0));
                }
                Some(AssetStreamStep::Fail(message)) => {
                    this.observations.record(UpdateObservation::Failed {
                        operation: UpdateOperation::StreamAsset,
                        message: message.clone(),
                    });
                    return Poll::Ready(Err(io::Error::other(message)));
                }
                None => {
                    if !this.completed {
                        this.completed = true;
                        this.observations
                            .record(UpdateObservation::AssetStreamCompleted {
                                version: this.version.clone(),
                            });
                    }
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledUpdate {
    pub version: String,
    pub declared_size: Option<u64>,
    pub bytes: Vec<u8>,
}

enum InstallAction {
    Succeed,
    Fail(String),
}

#[derive(Default)]
struct InstallerState {
    actions: VecDeque<InstallAction>,
    installed: Vec<InstalledUpdate>,
}

/// Installer fake that consumes streamed bytes and records completed installs.
#[derive(Clone)]
pub struct FakeUpdateInstaller {
    state: Arc<Mutex<InstallerState>>,
    observations: ObservationLog<UpdateObservation>,
}

impl FakeUpdateInstaller {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<UpdateObservation>) -> Self {
        Self {
            state: Arc::new(Mutex::new(InstallerState::default())),
            observations,
        }
    }

    pub fn succeed_next(&self) {
        lock_recover(&self.state)
            .actions
            .push_back(InstallAction::Succeed);
    }

    pub fn fail_next(&self, message: impl Into<String>) {
        lock_recover(&self.state)
            .actions
            .push_back(InstallAction::Fail(message.into()));
    }

    pub fn installed_updates(&self) -> Vec<InstalledUpdate> {
        lock_recover(&self.state).installed.clone()
    }

    pub fn remaining_actions(&self) -> usize {
        lock_recover(&self.state).actions.len()
    }

    pub fn observations(&self) -> ObservationLog<UpdateObservation> {
        self.observations.clone()
    }

    fn record_failure(&self, message: String) -> color_eyre::eyre::Report {
        self.observations.record(UpdateObservation::Failed {
            operation: UpdateOperation::Install,
            message: message.clone(),
        });
        eyre!(message)
    }
}

impl Default for FakeUpdateInstaller {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateInstaller for FakeUpdateInstaller {
    fn install<'a>(&'a self, release: &'a Release, asset: ReleaseAsset) -> UpdateFuture<'a, ()> {
        Box::pin(async move {
            self.observations
                .record(UpdateObservation::InstallRequested {
                    version: release.version.clone(),
                    declared_size: asset.declared_size,
                });
            let action = lock_recover(&self.state).actions.pop_front();
            match action {
                Some(InstallAction::Fail(message)) => return Err(self.record_failure(message)),
                Some(InstallAction::Succeed) => {}
                None => {
                    return Err(self.record_failure(format!(
                        "unexpected install for {}: no scripted action",
                        release.version
                    )));
                }
            }

            let declared_size = asset.declared_size;
            let mut reader = asset.into_reader();
            let mut bytes = Vec::new();
            if let Err(error) = reader.read_to_end(&mut bytes).await {
                return Err(self.record_failure(format!("asset stream failed: {error}")));
            }
            lock_recover(&self.state).installed.push(InstalledUpdate {
                version: release.version.clone(),
                declared_size,
                bytes: bytes.clone(),
            });
            self.observations.record(UpdateObservation::Installed {
                version: release.version.clone(),
                bytes: bytes.len(),
            });
            Ok(())
        })
    }
}

/// Records restart intent returned by application/session orchestration.
#[derive(Clone)]
pub struct RestartRecorder {
    reasons: Arc<Mutex<Vec<RestartReason>>>,
    observations: ObservationLog<UpdateObservation>,
}

impl RestartRecorder {
    pub fn new() -> Self {
        Self::with_log(ObservationLog::new())
    }

    pub fn with_log(observations: ObservationLog<UpdateObservation>) -> Self {
        Self {
            reasons: Arc::new(Mutex::new(Vec::new())),
            observations,
        }
    }

    pub fn record(&self, reason: RestartReason) {
        self.observations
            .record(UpdateObservation::RestartRequested {
                reason: reason.clone(),
            });
        lock_recover(&self.reasons).push(reason);
    }

    pub fn reasons(&self) -> Vec<RestartReason> {
        lock_recover(&self.reasons).clone()
    }

    pub fn observations(&self) -> ObservationLog<UpdateObservation> {
        self.observations.clone()
    }
}

impl Default for RestartRecorder {
    fn default() -> Self {
        Self::new()
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lookup_and_asset_scripts_are_fifo_and_shared_across_clones() {
        let repository = ScriptedReleaseRepository::new();
        let clone = repository.clone();
        repository.push_latest_release("v1.2.3");
        repository.fail_next_latest("lookup offline");
        repository.push_asset(
            "v1.2.3",
            Some(6),
            [
                AssetStreamStep::bytes(b"abc"),
                AssetStreamStep::bytes(b"def"),
            ],
        );

        let release = clone.latest_release().await.unwrap();
        assert_eq!(release, Release::new("v1.2.3"));
        assert_eq!(
            repository.latest_release().await.unwrap_err().to_string(),
            "lookup offline"
        );
        let mut reader = repository
            .stream_asset(&release)
            .await
            .unwrap()
            .into_reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();

        assert_eq!(bytes, b"abcdef");
        assert_eq!(repository.remaining_latest_actions(), 0);
        assert_eq!(repository.remaining_asset_actions(), 0);
    }

    #[tokio::test]
    async fn asset_stream_can_fail_after_delivering_bytes() {
        let repository = ScriptedReleaseRepository::new();
        let release = Release::new("v1.2.3");
        repository.push_asset(
            "v1.2.3",
            None,
            [
                AssetStreamStep::bytes(b"partial"),
                AssetStreamStep::fail("connection reset"),
            ],
        );
        let mut reader = repository
            .stream_asset(&release)
            .await
            .unwrap()
            .into_reader();
        let mut bytes = Vec::new();

        let error = reader.read_to_end(&mut bytes).await.unwrap_err();
        assert_eq!(bytes, b"partial");
        assert_eq!(error.to_string(), "connection reset");
    }

    #[tokio::test]
    async fn installer_consumes_stream_and_supports_scripted_failures() {
        let installer = FakeUpdateInstaller::new();
        let release = Release::new("v2.0.0");
        installer.succeed_next();
        installer
            .install(
                &release,
                ReleaseAsset::new(Some(3), Box::pin(std::io::Cursor::new(b"bin".to_vec()))),
            )
            .await
            .unwrap();
        installer.fail_next("read-only filesystem");
        let error = installer
            .install(
                &release,
                ReleaseAsset::new(Some(3), Box::pin(std::io::Cursor::new(b"new".to_vec()))),
            )
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "read-only filesystem");
        assert_eq!(
            installer.installed_updates(),
            vec![InstalledUpdate {
                version: "v2.0.0".to_string(),
                declared_size: Some(3),
                bytes: b"bin".to_vec(),
            }]
        );
    }

    #[test]
    fn restart_recorder_is_cloneable_and_observable() {
        let log = ObservationLog::new();
        let recorder = RestartRecorder::with_log(log.clone());
        let clone = recorder.clone();
        let reason = RestartReason::UpdateInstalled {
            version: "v2.0.0".to_string(),
        };

        clone.record(reason.clone());

        assert_eq!(recorder.reasons(), vec![reason.clone()]);
        assert_eq!(
            log.snapshot().last().unwrap().event,
            UpdateObservation::RestartRequested { reason }
        );
    }

    #[tokio::test]
    async fn unconfigured_calls_fail_instead_of_returning_defaults() {
        let repository = ScriptedReleaseRepository::new();
        let installer = FakeUpdateInstaller::new();
        let release = Release::new("v1.0.0");

        assert!(repository
            .latest_release()
            .await
            .unwrap_err()
            .to_string()
            .contains("no scripted action"));
        assert!(repository
            .stream_asset(&release)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no scripted action"));
        assert!(installer
            .install(
                &release,
                ReleaseAsset::new(Some(0), Box::pin(std::io::Cursor::new(Vec::new()))),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("no scripted action"));
    }
}
