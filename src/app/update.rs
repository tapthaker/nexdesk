use color_eyre::eyre::Result;

use super::RestartReason;
use crate::ports::{Release, ReleaseRepository, UpdateInstaller};

pub const MAX_RELEASE_VERSION_BYTES: usize = 64;

/// Origin of an update suggestion. Only configured repositories and trusted
/// peers may cause executable replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateSource {
    TrustedRepository,
    TrustedPeer,
    UntrustedPeer,
}

impl UpdateSource {
    fn is_trusted(self) -> bool {
        matches!(self, Self::TrustedRepository | Self::TrustedPeer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRejection {
    UntrustedSource,
    InvalidReleaseVersion,
    NotNewer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateDecision {
    Install(Release),
    Ignore(UpdateRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateExecution {
    Ignored(UpdateRejection),
    RestartRequested(RestartReason),
}

/// Pure update eligibility policy, independent of release transport and local
/// executable installation mechanics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatePolicy {
    current_version: String,
}

impl UpdatePolicy {
    pub fn new(current_version: impl Into<String>) -> Self {
        Self {
            current_version: current_version.into(),
        }
    }

    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    pub fn evaluate(&self, release: Release, source: UpdateSource) -> UpdateDecision {
        if !source.is_trusted() {
            return UpdateDecision::Ignore(UpdateRejection::UntrustedSource);
        }
        if !is_release_version(&release.version) {
            return UpdateDecision::Ignore(UpdateRejection::InvalidReleaseVersion);
        }
        if !is_newer(&release.version, &self.current_version) {
            return UpdateDecision::Ignore(UpdateRejection::NotNewer);
        }
        UpdateDecision::Install(release)
    }
}

/// Apply policy and, only when eligible, stream and install the selected release.
pub async fn execute_update(
    policy: &UpdatePolicy,
    release: Release,
    source: UpdateSource,
    repository: &dyn ReleaseRepository,
    installer: &dyn UpdateInstaller,
) -> Result<UpdateExecution> {
    let release = match policy.evaluate(release, source) {
        UpdateDecision::Install(release) => release,
        UpdateDecision::Ignore(reason) => return Ok(UpdateExecution::Ignored(reason)),
    };
    let asset = repository.stream_asset(&release).await?;
    installer.install(&release, asset).await?;
    Ok(UpdateExecution::RestartRequested(
        RestartReason::UpdateInstalled {
            version: release.version,
        },
    ))
}

fn parse_semver(version: &str) -> Option<(u32, u32, u32)> {
    let version = version.strip_prefix('v')?;
    let version = version.split('-').next()?;
    let mut parts = version.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

/// Returns true if `candidate` has a newer semantic version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Returns true only for bounded, clean semver release tags such as `v1.2.3`.
pub fn is_release_version(version: &str) -> bool {
    if version.len() > MAX_RELEASE_VERSION_BYTES || version.chars().any(char::is_control) {
        return false;
    }
    let Some(version) = version.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u32>().is_ok()
        })
}

#[cfg(test)]
mod tests {
    use crate::testing::ObservationLog;
    use crate::testing::{
        AssetStreamStep, FakeUpdateInstaller, RestartRecorder, ScriptedReleaseRepository,
        UpdateObservation,
    };

    use super::*;

    fn release(version: &str) -> Release {
        Release::new(version)
    }

    #[test]
    fn eligibility_is_independent_of_download_and_installation_table() {
        let policy = UpdatePolicy::new("v1.2.3");
        let cases = [
            (
                "v1.2.4",
                UpdateSource::TrustedRepository,
                UpdateDecision::Install(release("v1.2.4")),
            ),
            (
                "v2.0.0",
                UpdateSource::TrustedPeer,
                UpdateDecision::Install(release("v2.0.0")),
            ),
            (
                "v1.2.4",
                UpdateSource::UntrustedPeer,
                UpdateDecision::Ignore(UpdateRejection::UntrustedSource),
            ),
            (
                "v1.2.3-dirty",
                UpdateSource::TrustedRepository,
                UpdateDecision::Ignore(UpdateRejection::InvalidReleaseVersion),
            ),
            (
                "v1.2.3",
                UpdateSource::TrustedRepository,
                UpdateDecision::Ignore(UpdateRejection::NotNewer),
            ),
            (
                "v1.2.2",
                UpdateSource::TrustedRepository,
                UpdateDecision::Ignore(UpdateRejection::NotNewer),
            ),
        ];

        for (version, source, expected) in cases {
            assert_eq!(policy.evaluate(release(version), source), expected);
        }
    }

    #[tokio::test]
    async fn rejected_update_scenarios_do_not_touch_download_or_install() {
        let cases = [
            (
                "v1.2.4",
                UpdateSource::UntrustedPeer,
                UpdateRejection::UntrustedSource,
            ),
            (
                "v1.2.4-dirty",
                UpdateSource::TrustedPeer,
                UpdateRejection::InvalidReleaseVersion,
            ),
            (
                "v1.2.3",
                UpdateSource::TrustedPeer,
                UpdateRejection::NotNewer,
            ),
            (
                "v1.2.2",
                UpdateSource::TrustedPeer,
                UpdateRejection::NotNewer,
            ),
        ];

        for (version, source, expected) in cases {
            let repository = ScriptedReleaseRepository::new();
            let installer = FakeUpdateInstaller::new();
            let outcome = execute_update(
                &UpdatePolicy::new("v1.2.3"),
                release(version),
                source,
                &repository,
                &installer,
            )
            .await
            .unwrap();

            assert_eq!(outcome, UpdateExecution::Ignored(expected));
            assert!(repository.observations().is_empty());
            assert!(installer.observations().is_empty());
        }
    }

    #[tokio::test]
    async fn download_and_install_failures_never_request_restart() {
        let policy = UpdatePolicy::new("v1.2.3");

        let repository = ScriptedReleaseRepository::new();
        let installer = FakeUpdateInstaller::new();
        repository.fail_next_asset("v1.2.4", "download unavailable");
        installer.succeed_next();
        assert_eq!(
            execute_update(
                &policy,
                release("v1.2.4"),
                UpdateSource::TrustedRepository,
                &repository,
                &installer,
            )
            .await
            .unwrap_err()
            .to_string(),
            "download unavailable"
        );
        assert_eq!(installer.remaining_actions(), 1);
        assert!(installer.installed_updates().is_empty());

        let repository = ScriptedReleaseRepository::new();
        let installer = FakeUpdateInstaller::new();
        repository.push_asset(
            "v1.2.4",
            None,
            [
                AssetStreamStep::bytes(b"partial"),
                AssetStreamStep::fail("download interrupted"),
            ],
        );
        installer.succeed_next();
        assert!(execute_update(
            &policy,
            release("v1.2.4"),
            UpdateSource::TrustedRepository,
            &repository,
            &installer,
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("download interrupted"));
        assert!(installer.installed_updates().is_empty());

        let repository = ScriptedReleaseRepository::new();
        let installer = FakeUpdateInstaller::new();
        repository.push_asset("v1.2.4", Some(3), [AssetStreamStep::bytes(b"bin")]);
        installer.fail_next("installation denied");
        assert_eq!(
            execute_update(
                &policy,
                release("v1.2.4"),
                UpdateSource::TrustedRepository,
                &repository,
                &installer,
            )
            .await
            .unwrap_err()
            .to_string(),
            "installation denied"
        );
        assert!(installer.installed_updates().is_empty());
    }

    #[tokio::test]
    async fn successful_install_returns_and_records_restart_intent() {
        let log = ObservationLog::new();
        let repository = ScriptedReleaseRepository::with_log(log.clone());
        let installer = FakeUpdateInstaller::with_log(log.clone());
        let restarts = RestartRecorder::with_log(log.clone());
        repository.push_asset("v1.2.4", Some(3), [AssetStreamStep::bytes(b"bin")]);
        installer.succeed_next();

        let outcome = execute_update(
            &UpdatePolicy::new("v1.2.3"),
            release("v1.2.4"),
            UpdateSource::TrustedPeer,
            &repository,
            &installer,
        )
        .await
        .unwrap();
        let UpdateExecution::RestartRequested(reason) = outcome else {
            panic!("successful update did not request restart");
        };
        restarts.record(reason.clone());

        assert_eq!(
            reason,
            RestartReason::UpdateInstalled {
                version: "v1.2.4".to_string(),
            }
        );
        assert_eq!(restarts.reasons(), vec![reason]);
        let events = log
            .snapshot()
            .into_iter()
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        assert!(matches!(
            events.last(),
            Some(UpdateObservation::RestartRequested { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            UpdateObservation::Installed { version, .. } if version == "v1.2.4"
        )));
    }

    #[test]
    fn clean_release_validation_is_bounded_and_strict() {
        assert!(is_release_version("v0.1.2"));
        assert!(is_release_version("v10.20.30"));
        assert!(!is_release_version("v0.1.2-dirty"));
        assert!(!is_release_version("v0.1.2-3-gabcdef"));
        assert!(!is_release_version("v0.1"));
        assert!(!is_release_version("v0.1.2.3"));
        assert!(!is_release_version("v0..2"));
        assert!(!is_release_version("v0.1.x"));
        assert!(!is_release_version("0.1.2"));
        assert!(!is_release_version("v0.1.2\n"));
        assert!(!is_release_version(&"v1".repeat(MAX_RELEASE_VERSION_BYTES)));
    }

    #[test]
    fn version_ordering_accepts_dirty_current_builds_but_not_invalid_versions() {
        assert!(is_newer("v0.1.10", "v0.1.9"));
        assert!(is_newer("v0.2.0", "v0.1.9-dirty"));
        assert!(!is_newer("v0.1.9", "v0.1.9"));
        assert!(!is_newer("v0.1.8", "v0.1.9"));
        assert!(!is_newer("unknown", "v0.1.9"));
        assert!(!is_newer("v0.1.10", "unknown"));
    }
}
