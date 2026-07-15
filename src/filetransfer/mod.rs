pub mod clipboard_files;
pub mod recv;
pub mod send;

/// Chunk size for file transfer (64 KiB).
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Maximum time to wait for peer progress on a file-transfer stream.
pub const TRANSFER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
