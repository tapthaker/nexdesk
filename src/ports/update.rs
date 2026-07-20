use std::future::Future;
use std::pin::Pin;

use color_eyre::eyre::Result;
use tokio::io::AsyncRead;

pub type UpdateFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub type ReleaseAssetReader = Pin<Box<dyn AsyncRead + Send + 'static>>;

/// Release metadata used by update policy without exposing repository transport details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Release {
    pub version: String,
}

impl Release {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }
}

/// A bounded-by-policy stream for the current platform's release executable.
pub struct ReleaseAsset {
    pub declared_size: Option<u64>,
    reader: ReleaseAssetReader,
}

impl ReleaseAsset {
    pub fn new(declared_size: Option<u64>, reader: ReleaseAssetReader) -> Self {
        Self {
            declared_size,
            reader,
        }
    }

    pub fn into_reader(self) -> ReleaseAssetReader {
        self.reader
    }
}

/// Semantic source for release metadata and executable assets.
///
/// Implementations may use HTTP, local fixtures, or scripted in-memory data;
/// callers only depend on release lookup and asset-streaming behavior.
pub trait ReleaseRepository: Send + Sync {
    fn latest_release(&self) -> UpdateFuture<'_, Release>;

    fn stream_asset<'a>(&'a self, release: &'a Release) -> UpdateFuture<'a, ReleaseAsset>;
}

/// Semantic boundary for replacing the running executable with a release asset.
pub trait UpdateInstaller: Send + Sync {
    fn install<'a>(&'a self, release: &'a Release, asset: ReleaseAsset) -> UpdateFuture<'a, ()>;
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::eyre;

    use super::*;

    struct UnavailableRepository;

    impl ReleaseRepository for UnavailableRepository {
        fn latest_release(&self) -> UpdateFuture<'_, Release> {
            Box::pin(async { Err(eyre!("release lookup unavailable")) })
        }

        fn stream_asset<'a>(&'a self, _release: &'a Release) -> UpdateFuture<'a, ReleaseAsset> {
            Box::pin(async { Err(eyre!("asset unavailable")) })
        }
    }

    struct UnavailableInstaller;

    impl UpdateInstaller for UnavailableInstaller {
        fn install<'a>(
            &'a self,
            _release: &'a Release,
            _asset: ReleaseAsset,
        ) -> UpdateFuture<'a, ()> {
            Box::pin(async { Err(eyre!("installation unavailable")) })
        }
    }

    #[test]
    fn updater_ports_are_object_safe_and_keep_http_and_filesystem_details_hidden() {
        fn assert_repository_object_safe(_: &dyn ReleaseRepository) {}
        fn assert_installer_object_safe(_: &dyn UpdateInstaller) {}

        assert_repository_object_safe(&UnavailableRepository);
        assert_installer_object_safe(&UnavailableInstaller);
    }

    #[tokio::test]
    async fn release_asset_exposes_only_size_and_async_byte_stream() {
        let bytes = b"release bytes".to_vec();
        let mut asset = ReleaseAsset::new(
            Some(bytes.len() as u64),
            Box::pin(std::io::Cursor::new(bytes.clone())),
        )
        .into_reader();
        let mut received = Vec::new();

        tokio::io::AsyncReadExt::read_to_end(&mut asset, &mut received)
            .await
            .unwrap();

        assert_eq!(received, bytes);
    }
}
