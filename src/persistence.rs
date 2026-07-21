use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use color_eyre::eyre::{Result, WrapErr};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceStage {
    Create,
    Write,
    Flush,
    Sync,
    Persist,
    DirectorySync,
}

pub(crate) trait PersistenceFaults {
    fn check(&self, _stage: PersistenceStage, _path: &Path) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct NoPersistenceFaults;

impl PersistenceFaults for NoPersistenceFaults {}

pub(crate) fn atomic_replace(
    path: &Path,
    contents: &[u8],
    faults: &dyn PersistenceFaults,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("Persistence path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = temporary_path(path, sequence);
    let mut cleanup = TempCleanup(Some(temp_path.clone()));

    faults.check(PersistenceStage::Create, path)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .wrap_err("Failed to create atomic-write temporary file")?;
    faults.check(PersistenceStage::Write, path)?;
    file.write_all(contents)
        .wrap_err("Failed to write atomic-write temporary file")?;
    faults.check(PersistenceStage::Flush, path)?;
    file.flush()
        .wrap_err("Failed to flush atomic-write temporary file")?;
    faults.check(PersistenceStage::Sync, path)?;
    file.sync_all()
        .wrap_err("Failed to sync atomic-write temporary file")?;
    faults.check(PersistenceStage::Persist, path)?;
    std::fs::rename(&temp_path, path).wrap_err("Failed to persist atomic-write temporary file")?;
    cleanup.0 = None;
    faults.check(PersistenceStage::DirectorySync, path)?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .wrap_err("Failed to sync persistence directory")?;
    Ok(())
}

fn temporary_path(path: &Path, sequence: u64) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    path.with_file_name(format!(".{name}.tmp-{}-{sequence}", std::process::id()))
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailAt(PersistenceStage);

    impl PersistenceFaults for FailAt {
        fn check(&self, stage: PersistenceStage, _path: &Path) -> Result<()> {
            if stage == self.0 {
                Err(color_eyre::eyre::eyre!("injected {stage:?} failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn atomic_write_failure_stages_use_real_temporary_files() {
        let stages = [
            PersistenceStage::Create,
            PersistenceStage::Write,
            PersistenceStage::Flush,
            PersistenceStage::Sync,
            PersistenceStage::Persist,
            PersistenceStage::DirectorySync,
        ];

        for stage in stages {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state.json");
            std::fs::write(&path, b"old").unwrap();

            let error = atomic_replace(&path, b"new", &FailAt(stage)).unwrap_err();

            assert!(error.to_string().contains(&format!("{stage:?}")));
            let expected = if stage == PersistenceStage::DirectorySync {
                b"new".as_slice()
            } else {
                b"old".as_slice()
            };
            assert_eq!(std::fs::read(&path).unwrap(), expected, "{stage:?}");
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        }
    }
}
