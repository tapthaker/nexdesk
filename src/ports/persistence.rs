use std::path::Path;

use color_eyre::eyre::Result;

/// Semantic boundary for durable replacement of one persisted document.
/// Reads and path semantics continue to use real files and temporary roots.
pub trait AtomicFileStore: Send + Sync {
    fn replace(&self, path: &Path, contents: &[u8]) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealAtomicFileStore;

impl AtomicFileStore for RealAtomicFileStore {
    fn replace(&self, path: &Path, contents: &[u8]) -> Result<()> {
        crate::persistence::atomic_replace(path, contents, &crate::persistence::NoPersistenceFaults)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_store_uses_real_temporary_filesystem_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("document");
        let store: &dyn AtomicFileStore = &RealAtomicFileStore;

        store.replace(&path, b"first").unwrap();
        store.replace(&path, b"second").unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"second");
    }
}
