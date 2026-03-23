use color_eyre::eyre::{Result, eyre};
use tracing::{info, warn};

/// Returns the platform slug used in GitHub release asset names.
fn platform_slug() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        _ => None,
    }
}

/// Returns true only for clean semver release tags (e.g. `v0.1.2`).
/// Rejects dirty builds (`v0.1.2-dirty`) and dev builds (`v0.1.2-3-gabcdef`).
pub fn is_release_version(version: &str) -> bool {
    if !version.starts_with('v') {
        return false;
    }
    if version.contains("-dirty") || version.contains("-g") {
        return false;
    }
    // Must be v followed by semver digits and dots only
    version[1..].chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Downloads the target version binary from GitHub releases and replaces the
/// current executable. Returns `Ok(())` on success — caller decides whether
/// to restart.
pub async fn self_update(target_version: &str) -> Result<()> {
    if !is_release_version(target_version) {
        return Err(eyre!("Not a clean release version: {}", target_version));
    }

    let platform = platform_slug()
        .ok_or_else(|| eyre!("Unsupported platform: {}-{}", std::env::consts::OS, std::env::consts::ARCH))?;

    let url = format!(
        "https://github.com/tapthaker/nexdesk/releases/download/{}/nexdesk-{}",
        target_version, platform
    );
    info!("Downloading update from {}", url);

    let response = reqwest::get(&url).await
        .map_err(|e| eyre!("Failed to download update: {}", e))?;

    if !response.status().is_success() {
        return Err(eyre!("Download failed with HTTP {}", response.status()));
    }

    let bytes = response.bytes().await
        .map_err(|e| eyre!("Failed to read response body: {}", e))?;

    if bytes.is_empty() {
        return Err(eyre!("Downloaded binary is empty"));
    }

    info!("Downloaded {} bytes", bytes.len());

    let exe_path = std::env::current_exe()
        .map_err(|e| eyre!("Failed to get current exe path: {}", e))?;

    let exe_dir = exe_path.parent()
        .ok_or_else(|| eyre!("Current exe has no parent directory"))?;

    let tmp_path = exe_dir.join(".nexdesk-update.tmp");

    // Write to temp file
    std::fs::write(&tmp_path, &bytes)
        .map_err(|e| eyre!("Failed to write temp file: {}", e))?;

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| eyre!("Failed to set permissions: {}", e))?;
    }

    // Atomic replace
    std::fs::rename(&tmp_path, &exe_path)
        .map_err(|e| eyre!("Failed to replace executable: {}", e))?;

    info!("Successfully updated to {}", target_version);
    Ok(())
}

/// Fetches the latest release tag from GitHub (e.g. "v0.1.8").
pub async fn check_latest_version() -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("nexdesk")
        .build()
        .map_err(|e| eyre!("Failed to build HTTP client: {}", e))?;

    let resp = client
        .get("https://api.github.com/repos/tapthaker/nexdesk/releases/latest")
        .send()
        .await
        .map_err(|e| eyre!("Failed to fetch latest release: {}", e))?;

    if !resp.status().is_success() {
        return Err(eyre!("GitHub API returned HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| eyre!("Failed to parse GitHub response: {}", e))?;

    body["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| eyre!("No tag_name in GitHub response"))
}

/// Periodically checks for new releases and self-updates. Exits the process
/// on successful update so the service manager (LaunchAgent/systemd) restarts
/// with the new binary.
pub async fn update_check_loop() {
    use std::time::Duration;
    use crate::net::protocol::BUILD_VERSION;

    // Skip update checks for dev builds
    if !is_release_version(BUILD_VERSION) {
        info!("Dev build ({}), skipping update checks", BUILD_VERSION);
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
    interval.tick().await; // skip the immediate first tick

    loop {
        interval.tick().await;

        let latest = match check_latest_version().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Update check failed: {}", e);
                continue;
            }
        };

        if latest == BUILD_VERSION {
            info!("Up to date ({})", BUILD_VERSION);
            continue;
        }

        if !is_release_version(&latest) {
            continue;
        }

        info!("New version available: {} (current: {})", latest, BUILD_VERSION);
        match self_update(&latest).await {
            Ok(()) => {
                info!("Updated to {}. Exiting for restart...", latest);
                std::process::exit(0);
            }
            Err(e) => {
                warn!("Self-update to {} failed: {}", latest, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_release_version() {
        assert!(is_release_version("v0.1.2"));
        assert!(is_release_version("v1.0.0"));
        assert!(is_release_version("v10.20.30"));

        assert!(!is_release_version("v0.1.2-dirty"));
        assert!(!is_release_version("v0.1.2-3-gabcdef"));
        assert!(!is_release_version("v0.1.2-dirty-3-gabcdef"));
        assert!(!is_release_version("0.1.2")); // no 'v' prefix
        assert!(!is_release_version("unknown"));
        assert!(!is_release_version(""));
    }
}
