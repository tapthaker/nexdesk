use color_eyre::eyre::{eyre, Result};
use tracing::{info, warn};

use crate::app::MAX_RELEASE_VERSION_BYTES;
pub use crate::app::{is_newer, is_release_version};

const MAX_UPDATE_SIZE: u64 = 100 * 1024 * 1024;
const MAX_RELEASE_API_RESPONSE_SIZE: u64 = 64 * 1024;
const MAX_RELEASE_TAG_BYTES: usize = MAX_RELEASE_VERSION_BYTES;
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(60);

fn checked_downloaded_size(current: u64, chunk_len: usize) -> Result<u64> {
    current
        .checked_add(chunk_len as u64)
        .ok_or_else(|| eyre!("Downloaded binary size overflow"))
}

/// Returns the platform slug used in GitHub release asset names.
fn platform_slug() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        _ => None,
    }
}

fn validate_release_tag_text(version: &str) -> Result<()> {
    if version.len() > MAX_RELEASE_TAG_BYTES {
        return Err(eyre!(
            "Release tag too large: {} bytes (max {})",
            version.len(),
            MAX_RELEASE_TAG_BYTES
        ));
    }
    if version.chars().any(char::is_control) {
        return Err(eyre!("Release tag contains control characters"));
    }
    Ok(())
}

fn safe_release_tag_for_error(version: &str) -> String {
    crate::status::terminal_safe(version, MAX_RELEASE_TAG_BYTES)
}

/// Downloads the target version binary from GitHub releases and replaces the
/// current executable. Returns `Ok(())` on success — caller decides whether
/// to restart.
pub async fn self_update(target_version: &str) -> Result<()> {
    if !is_release_version(target_version) {
        return Err(eyre!("Not a clean release version: {}", target_version));
    }

    let platform = platform_slug().ok_or_else(|| {
        eyre!(
            "Unsupported platform: {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let url = format!(
        "https://github.com/tapthaker/nexdesk/releases/download/{}/nexdesk-{}",
        target_version, platform
    );
    info!("Downloading update from {}", url);

    let response = reqwest::get(&url)
        .await
        .map_err(|e| eyre!("Failed to download update: {}", e))?;

    if !response.status().is_success() {
        return Err(eyre!("Download failed with HTTP {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| eyre!("Failed to read response body: {}", e))?;

    if bytes.is_empty() {
        return Err(eyre!("Downloaded binary is empty"));
    }

    info!("Downloaded {} bytes", bytes.len());

    let exe_path =
        std::env::current_exe().map_err(|e| eyre!("Failed to get current exe path: {}", e))?;

    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| eyre!("Current exe has no parent directory"))?;

    let tmp_path = exe_dir.join(".nexdesk-update.tmp");

    // Write to temp file
    std::fs::write(&tmp_path, &bytes).map_err(|e| eyre!("Failed to write temp file: {}", e))?;

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
    use crate::net::protocol::BUILD_VERSION;
    use std::time::Duration;

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

        if !is_release_version(&latest) || !is_newer(&latest, BUILD_VERSION) {
            continue;
        }

        info!(
            "New version available: {} (current: {})",
            latest, BUILD_VERSION
        );
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
    fn downloaded_size_accounting_rejects_overflow() {
        assert_eq!(checked_downloaded_size(10, 5).unwrap(), 15);
        assert!(checked_downloaded_size(u64::MAX, 1).is_err());
    }

    #[tokio::test]
    async fn update_temp_file_is_open_private_and_writable() {
        let dir = tempfile::tempdir().unwrap();
        let (mut file, path) = create_update_temp_file(dir.path()).unwrap();
        file.write_all(b"abc").await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn release_tag_text_is_bounded_and_control_free() {
        validate_release_tag_text("v1.2.3").unwrap();
        assert!(validate_release_tag_text("v1.2.3\n").is_err());
        assert!(validate_release_tag_text(&"v".repeat(MAX_RELEASE_TAG_BYTES + 1)).is_err());
        let safe = safe_release_tag_for_error(&format!("{}\x1b[31m", "v".repeat(128)));
        assert!(!safe.contains('\u{1b}'));
        assert_eq!(safe.len(), MAX_RELEASE_TAG_BYTES);
    }

    #[test]
    fn release_api_response_limit_is_small() {
        assert_eq!(MAX_RELEASE_API_RESPONSE_SIZE, 64 * 1024);
    }
}
