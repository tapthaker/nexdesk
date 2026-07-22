use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::app::MAX_RELEASE_VERSION_BYTES;
pub use crate::app::{is_newer, is_release_version};
use crate::ports::{Release, ReleaseAsset, ReleaseRepository, UpdateFuture, UpdateInstaller};

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

/// GitHub-backed release metadata and executable asset repository.
#[derive(Clone, Copy, Debug, Default)]
pub struct GithubReleaseRepository;

impl ReleaseRepository for GithubReleaseRepository {
    fn latest_release(&self) -> UpdateFuture<'_, Release> {
        Box::pin(async { check_latest_version().await.map(Release::new) })
    }

    fn stream_asset<'a>(&'a self, release: &'a Release) -> UpdateFuture<'a, ReleaseAsset> {
        Box::pin(async move {
            if !is_release_version(&release.version) {
                return Err(eyre!(
                    "Not a clean release version: {}",
                    safe_release_tag_for_error(&release.version)
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
                release.version, platform
            );
            info!("Downloading update from {}", url);
            let client = update_http_client(UPDATE_HTTP_TIMEOUT)?;
            stream_asset_at(&client, &url, MAX_UPDATE_SIZE).await
        })
    }
}

async fn stream_asset_at(
    client: &reqwest::Client,
    url: &str,
    max_size: u64,
) -> Result<ReleaseAsset> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| eyre!("Failed to download update: {}", e))?;
    if !response.status().is_success() {
        return Err(eyre!("Download failed with HTTP {}", response.status()));
    }
    let declared_size = response.content_length();
    if declared_size.is_some_and(|length| length > max_size) {
        return Err(eyre!(
            "Downloaded binary is too large: {} bytes (max {})",
            declared_size.expect("declared size was checked"),
            max_size
        ));
    }

    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(pump_response(response, sender, max_size));
    Ok(ReleaseAsset::new(
        declared_size,
        Box::pin(HttpAssetReader {
            receiver,
            current: None,
        }),
    ))
}

async fn pump_response(
    mut response: reqwest::Response,
    sender: mpsc::Sender<io::Result<Vec<u8>>>,
    max_size: u64,
) {
    let mut downloaded = 0u64;
    loop {
        let item = match response.chunk().await {
            Ok(Some(chunk)) => match checked_downloaded_size(downloaded, chunk.len()) {
                Ok(total) if total <= max_size => {
                    downloaded = total;
                    Ok(chunk.to_vec())
                }
                Ok(total) => Err(io::Error::other(format!(
                    "Downloaded binary exceeded max size: {total} bytes (max {max_size})"
                ))),
                Err(error) => Err(io::Error::other(error.to_string())),
            },
            Ok(None) => break,
            Err(error) => Err(io::Error::other(format!(
                "Failed to read response body: {error}"
            ))),
        };
        let failed = item.is_err();
        if sender.send(item).await.is_err() || failed {
            break;
        }
    }
}

struct HttpAssetReader {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    current: Option<(Vec<u8>, usize)>,
}

impl AsyncRead for HttpAssetReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
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
                if *offset == bytes.len() {
                    this.current = None;
                }
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.receiver).poll_recv(cx) {
                Poll::Ready(Some(Ok(bytes))) if bytes.is_empty() => continue,
                Poll::Ready(Some(Ok(bytes))) => this.current = Some((bytes, 0)),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Production installer that atomically replaces the current executable.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutableUpdateInstaller;

impl UpdateInstaller for ExecutableUpdateInstaller {
    fn install<'a>(&'a self, release: &'a Release, asset: ReleaseAsset) -> UpdateFuture<'a, ()> {
        Box::pin(async move {
            if asset
                .declared_size
                .is_some_and(|length| length > MAX_UPDATE_SIZE)
            {
                return Err(eyre!(
                    "Downloaded binary is too large: {} bytes (max {})",
                    asset.declared_size.expect("declared size was checked"),
                    MAX_UPDATE_SIZE
                ));
            }
            let exe_path = std::env::current_exe()
                .map_err(|e| eyre!("Failed to get current exe path: {}", e))?;
            let exe_dir = exe_path
                .parent()
                .ok_or_else(|| eyre!("Current exe has no parent directory"))?;
            let (mut file, tmp_path) = create_update_temp_file(exe_dir)?;
            let mut reader = asset.into_reader();
            let mut buffer = vec![0u8; 64 * 1024];
            let mut downloaded = 0u64;
            loop {
                let read = reader
                    .read(&mut buffer)
                    .await
                    .map_err(|e| eyre!("Failed to read response body: {}", e))?;
                if read == 0 {
                    break;
                }
                downloaded = checked_downloaded_size(downloaded, read)?;
                if downloaded > MAX_UPDATE_SIZE {
                    return Err(eyre!(
                        "Downloaded binary exceeded max size: {} bytes (max {})",
                        downloaded,
                        MAX_UPDATE_SIZE
                    ));
                }
                file.write_all(&buffer[..read])
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

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| eyre!("Failed to set permissions: {}", e))?;
                sync_file_path(&tmp_path)
                    .map_err(|e| eyre!("Failed to sync executable permissions: {}", e))?;
            }

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
            info!("Successfully updated to {}", release.version);
            Ok(())
        })
    }
}

/// Downloads the target version and atomically replaces the current executable.
pub async fn self_update(target_version: &str) -> Result<()> {
    let release = Release::new(target_version);
    let repository = GithubReleaseRepository;
    let asset = repository.stream_asset(&release).await?;
    ExecutableUpdateInstaller.install(&release, asset).await
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

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/tapthaker/nexdesk/releases/latest";

fn update_http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("nexdesk")
        .timeout(timeout)
        .build()
        .map_err(|e| eyre!("Failed to build HTTP client: {}", e))
}

/// Fetches the latest release tag from GitHub (e.g. "v0.1.8").
pub async fn check_latest_version() -> Result<String> {
    let client = update_http_client(UPDATE_HTTP_TIMEOUT)?;
    check_latest_version_at(&client, LATEST_RELEASE_URL).await
}

async fn check_latest_version_at(client: &reqwest::Client, url: &str) -> Result<String> {
    let mut resp = client
        .get(url)
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

/// Periodically checks for new releases and self-updates. The first check runs
/// immediately so a restarted or manually installed stale daemon does not wait
/// 30 minutes. A successful update exits so the service manager restarts the
/// new binary.
pub async fn update_check_loop() {
    use crate::net::protocol::BUILD_VERSION;
    use std::time::Duration;

    if !is_release_version(BUILD_VERSION) {
        info!("Dev build ({}), skipping update checks", BUILD_VERSION);
        return;
    }

    loop {
        match check_latest_version().await {
            Ok(latest) if latest == BUILD_VERSION => {
                info!("Up to date ({})", BUILD_VERSION);
            }
            Ok(latest) if is_release_version(&latest) && is_newer(&latest, BUILD_VERSION) => {
                info!(
                    "New version available: {} (current: {})",
                    latest, BUILD_VERSION
                );
                match self_update(&latest).await {
                    Ok(()) => {
                        info!("Updated to {}. Exiting for restart...", latest);
                        std::process::exit(0);
                    }
                    Err(e) => warn!("Self-update to {} failed: {}", latest, e),
                }
            }
            Ok(_) => {}
            Err(e) => warn!("Update check failed: {}", e),
        }

        tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::update_http_fixture::{LocalHttpFixture, ScriptedHttpResponse};

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

    #[tokio::test]
    async fn release_lookup_rejects_http_statuses_and_malformed_json() {
        let fixture = LocalHttpFixture::start().await;
        let client = update_http_client(Duration::from_secs(1)).unwrap();
        for status in [404, 500] {
            fixture.push(ScriptedHttpResponse::bytes(status, "failure"));
            let error = check_latest_version_at(&client, &fixture.url("/latest"))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("HTTP {status}")), "{error}");
        }

        fixture.push(ScriptedHttpResponse::bytes(200, "not json"));
        assert!(check_latest_version_at(&client, &fixture.url("/latest"))
            .await
            .unwrap_err()
            .to_string()
            .contains("Failed to parse GitHub response"));
    }

    #[tokio::test]
    async fn release_lookup_accepts_chunked_json() {
        let fixture = LocalHttpFixture::start().await;
        fixture.push(ScriptedHttpResponse::chunked(
            200,
            vec![b"{\"tag_".to_vec(), b"name\":\"v1.2.3\"}".to_vec()],
        ));

        let version = check_latest_version_at(
            &update_http_client(Duration::from_secs(1)).unwrap(),
            &fixture.url("/latest"),
        )
        .await
        .unwrap();

        assert_eq!(version, "v1.2.3");
    }

    #[tokio::test]
    async fn release_lookup_enforces_declared_and_streamed_size_limits() {
        let fixture = LocalHttpFixture::start().await;
        let client = update_http_client(Duration::from_secs(1)).unwrap();

        fixture.push(
            ScriptedHttpResponse::bytes(200, "{}")
                .with_header("Content-Length", MAX_RELEASE_API_RESPONSE_SIZE + 1),
        );
        assert!(check_latest_version_at(&client, &fixture.url("/latest"))
            .await
            .unwrap_err()
            .to_string()
            .contains("response too large"));

        fixture.push(ScriptedHttpResponse::chunked(
            200,
            vec![vec![b'x'; MAX_RELEASE_API_RESPONSE_SIZE as usize + 1]],
        ));
        assert!(check_latest_version_at(&client, &fixture.url("/latest"))
            .await
            .unwrap_err()
            .to_string()
            .contains("response exceeded max size"));
    }

    async fn read_asset(asset: ReleaseAsset) -> io::Result<Vec<u8>> {
        let mut reader = asset.into_reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    #[tokio::test]
    async fn binary_download_rejects_http_statuses() {
        let fixture = LocalHttpFixture::start().await;
        let client = update_http_client(Duration::from_secs(1)).unwrap();
        for status in [404, 500] {
            fixture.push(ScriptedHttpResponse::bytes(status, "failure"));
            let error = stream_asset_at(&client, &fixture.url("/asset"), 1024)
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&format!("HTTP {status}")), "{error}");
        }
    }

    #[tokio::test]
    async fn binary_download_exposes_empty_and_chunked_bodies() {
        let fixture = LocalHttpFixture::start().await;
        let client = update_http_client(Duration::from_secs(1)).unwrap();

        fixture.push(ScriptedHttpResponse::bytes(200, Vec::new()));
        let empty = stream_asset_at(&client, &fixture.url("/asset"), 1024)
            .await
            .unwrap();
        assert_eq!(empty.declared_size, Some(0));
        assert!(read_asset(empty).await.unwrap().is_empty());

        fixture.push(ScriptedHttpResponse::chunked(
            200,
            vec![b"release ".to_vec(), b"binary".to_vec()],
        ));
        let chunked = stream_asset_at(&client, &fixture.url("/asset"), 1024)
            .await
            .unwrap();
        assert_eq!(chunked.declared_size, None);
        assert_eq!(read_asset(chunked).await.unwrap(), b"release binary");
    }

    #[tokio::test]
    async fn truncated_binary_body_fails_during_streaming() {
        let fixture = LocalHttpFixture::start().await;
        fixture.push(ScriptedHttpResponse::truncated(200, 10, b"short".to_vec()));
        let asset = stream_asset_at(
            &update_http_client(Duration::from_secs(1)).unwrap(),
            &fixture.url("/asset"),
            1024,
        )
        .await
        .unwrap();

        assert_eq!(asset.declared_size, Some(10));
        assert!(read_asset(asset).await.is_err());
    }

    #[tokio::test]
    async fn binary_download_enforces_declared_and_actual_size_limits() {
        let fixture = LocalHttpFixture::start().await;
        let client = update_http_client(Duration::from_secs(1)).unwrap();

        fixture.push(
            ScriptedHttpResponse::bytes(200, b"12345".to_vec()).with_header("Content-Length", 5),
        );
        assert!(stream_asset_at(&client, &fixture.url("/asset"), 4)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("too large"));

        fixture.push(ScriptedHttpResponse::chunked(
            200,
            vec![b"123".to_vec(), b"45".to_vec()],
        ));
        let asset = stream_asset_at(&client, &fixture.url("/asset"), 4)
            .await
            .unwrap();
        assert!(read_asset(asset)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeded max size"));
    }

    #[tokio::test]
    async fn binary_download_timeout_is_bounded() {
        let fixture = LocalHttpFixture::start().await;
        fixture
            .push(ScriptedHttpResponse::bytes(200, "late").with_delay(Duration::from_millis(200)));
        let client = update_http_client(Duration::from_millis(25)).unwrap();

        let error = stream_asset_at(&client, &fixture.url("/asset"), 1024)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("Failed to download update"), "{error}");
    }

    #[tokio::test]
    async fn release_lookup_timeout_is_bounded() {
        let fixture = LocalHttpFixture::start().await;
        fixture.push(ScriptedHttpResponse::stalled());
        let client = update_http_client(Duration::from_millis(25)).unwrap();

        let error = check_latest_version_at(&client, &fixture.url("/latest"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Failed to fetch latest release"), "{error}");
    }
}
