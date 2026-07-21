use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use color_eyre::eyre::{Result, WrapErr};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

pub(crate) trait PersistenceFaults {
    fn before_replace(&self, _path: &Path) -> Result<()> {
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

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .wrap_err("Failed to create atomic-write temporary file")?;
    file.write_all(contents)
        .wrap_err("Failed to write atomic-write temporary file")?;
    file.flush()
        .wrap_err("Failed to flush atomic-write temporary file")?;
    file.sync_all()
        .wrap_err("Failed to sync atomic-write temporary file")?;
    faults.before_replace(path)?;
    std::fs::rename(&temp_path, path).wrap_err("Failed to persist atomic-write temporary file")?;
    cleanup.0 = None;
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
