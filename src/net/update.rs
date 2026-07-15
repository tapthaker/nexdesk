use std::path::Path;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

const MAX_UPDATE_SIZE: u64 = 100 * 1024 * 1024;
const MAX_RELEASE_API_RESPONSE_SIZE: u64 = 64 * 1024;
const MAX_RELEASE_TAG_BYTES: usize = 64;
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

/// Parse a version string like "v0.1.2" into (major, minor, patch).
fn parse_semver(version: &str) -> Option<(u32, u32, u32)> {
    let v = version.strip_prefix('v')?;
    // Take only the semver part before any suffix like "-dirty" or "-3-gabcdef"
    let v = v.split('-').next()?;
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

/// Returns true if `a` is a newer version than `b`.
pub fn is_newer(a: &str, b: &str) -> bool {
    match (parse_semver(a), parse_semver(b)) {
        (Some(va), Some(vb)) => va > vb,
        _ => false,
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

/// Returns true only for clean semver release tags (e.g. `v0.1.2`).
/// Rejects dirty builds (`v0.1.2-dirty`) and dev builds (`v0.1.2-3-gabcdef`).
pub fn is_release_version(version: &str) -> bool {
    if validate_release_tag_text(version).is_err() {
        return false;
    }
    let Some(v) = version.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u32>().is_ok()
        })
}

/// Downloads the target version binary from GitHub releases and replaces the
/// current executable. Returns `Ok(())` on success — caller decides whether
/// to restart.
pub async fn self_update(target_version: &str) -> Result<()> {
    if !is_release_version(target_version) {
        return Err(eyre!(
            "Not a clean release version: {}",
            safe_release_tag_for_error(target_version)
        ));
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

    let client = reqwest::Client::builder()
        .user_agent("nexdesk")
        .timeout(UPDATE_HTTP_TIMEOUT)
        .build()
        .map_err(|e| eyre!("Failed to build HTTP client: {}", e))?;
    let mut response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| eyre!("Failed to download update: {}", e))?;

    if !response.status().is_success() {
        return Err(eyre!("Download failed with HTTP {}", response.status()));
    }
    if let Some(length) = response.content_length() {
        if length > MAX_UPDATE_SIZE {
            return Err(eyre!(
                "Downloaded binary is too large: {} bytes (max {})",
                length,
                MAX_UPDATE_SIZE
            ));
        }
    }

    let exe_path =
        std::env::current_exe().map_err(|e| eyre!("Failed to get current exe path: {}", e))?;

    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| eyre!("Current exe has no parent directory"))?;

    let (mut file, tmp_path) = create_update_temp_file(exe_dir)?;
    let mut downloaded = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| eyre!("Failed to read response body: {}", e))?
    {
        downloaded = checked_downloaded_size(downloaded, chunk.len())?;
        if downloaded > MAX_UPDATE_SIZE {
            return Err(eyre!(
                "Downloaded binary exceeded max size: {} bytes (max {})",
                downloaded,
                MAX_UPDATE_SIZE
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| eyre!("Failed to write temp file: {}", e))?;
    }
    file.flush()
        .await
        .map_err(|e| eyre!("Failed to flush temp file: {}", e))?;
    file.sync_all()
        .await
        .map_err(|e| eyre!("Failed to sync temp update file: {}", e))?;
    drop(file);

    if downloaded == 0 {
        return Err(eyre!("Downloaded binary is empty"));
    }

    info!("Downloaded {} bytes", downloaded);

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| eyre!("Failed to set permissions: {}", e))?;
        sync_file_path(&tmp_path)
            .map_err(|e| eyre!("Failed to sync executable permissions: {}", e))?;
    }

    // Atomic replace
    tmp_path
        .persist(&exe_path)
        .map_err(|e| eyre!("Failed to replace executable: {}", e.error))?;
    sync_directory(exe_dir).map_err(|e| {
        eyre!(
            "Failed to sync executable directory after update ({}): {}",
            exe_dir.display(),
            e
        )
    })?;

    info!("Successfully updated to {}", target_version);
    Ok(())
}

fn create_update_temp_file(dir: &Path) -> Result<(tokio::fs::File, tempfile::TempPath)> {
    let mut tmp_file = tempfile::Builder::new()
        .prefix(".nexdesk-update.")
        .tempfile_in(dir)
        .map_err(|e| eyre!("Failed to create temp update file: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp_file
            .as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| eyre!("Failed to restrict temp update file permissions: {}", e))?;
    }

    let (file, path) = tmp_file.into_parts();
    Ok((tokio::fs::File::from_std(file), path))
}

fn sync_file_path(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Fetches the latest release tag from GitHub (e.g. "v0.1.8").
pub async fn check_latest_version() -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("nexdesk")
        .timeout(UPDATE_HTTP_TIMEOUT)
        .build()
        .map_err(|e| eyre!("Failed to build HTTP client: {}", e))?;

    let mut resp = client
        .get("https://api.github.com/repos/tapthaker/nexdesk/releases/latest")
        .send()
        .await
        .map_err(|e| eyre!("Failed to fetch latest release: {}", e))?;

    if !resp.status().is_success() {
        return Err(eyre!("GitHub API returned HTTP {}", resp.status()));
    }
    if let Some(length) = resp.content_length() {
        if length > MAX_RELEASE_API_RESPONSE_SIZE {
            return Err(eyre!(
                "GitHub API response too large: {} bytes (max {})",
                length,
                MAX_RELEASE_API_RESPONSE_SIZE
            ));
        }
    }

    let mut body_bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| eyre!("Failed to read GitHub response: {}", e))?
    {
        if (body_bytes.len() as u64).saturating_add(chunk.len() as u64)
            > MAX_RELEASE_API_RESPONSE_SIZE
        {
            return Err(eyre!(
                "GitHub API response exceeded max size: {} bytes",
                MAX_RELEASE_API_RESPONSE_SIZE
            ));
        }
        body_bytes.extend_from_slice(&chunk);
    }

    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| eyre!("Failed to parse GitHub response: {}", e))?;

    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| eyre!("No tag_name in GitHub response"))?;
    validate_release_tag_text(tag)?;
    Ok(tag.to_string())
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

    #[test]
    fn test_is_release_version() {
        assert!(is_release_version("v0.1.2"));
        assert!(is_release_version("v1.0.0"));
        assert!(is_release_version("v10.20.30"));

        assert!(!is_release_version("v0.1.2\n"));
        assert!(!is_release_version(&format!(
            "v{}.1.2",
            "9".repeat(MAX_RELEASE_TAG_BYTES)
        )));
        assert!(!is_release_version("v0.1.2-dirty"));
        assert!(!is_release_version("v0.1.2-3-gabcdef"));
        assert!(!is_release_version("v0.1.2-dirty-3-gabcdef"));
        assert!(!is_release_version("v0.1"));
        assert!(!is_release_version("v0.1.2.3"));
        assert!(!is_release_version("v0..2"));
        assert!(!is_release_version("v0.1.x"));
        assert!(!is_release_version("v999999999999999999999.1.2"));
        assert!(!is_release_version("0.1.2")); // no 'v' prefix
        assert!(!is_release_version("unknown"));
        assert!(!is_release_version(""));
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("v0.1.10", "v0.1.9"));
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(is_newer("v1.0.0", "v0.99.99"));

        assert!(!is_newer("v0.1.9", "v0.1.10"));
        assert!(!is_newer("v0.1.9", "v0.1.9"));

        // Handles dirty/dev versions by comparing base semver
        assert!(is_newer("v0.1.10", "v0.1.9-dirty"));
        assert!(!is_newer("v0.1.9", "v0.1.10-dirty"));
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
