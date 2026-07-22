pub mod clipboard_files;
pub mod recv;
pub mod send;
pub(crate) mod stream;

/// Chunk size for file transfer (64 KiB).
pub const CHUNK_SIZE: usize = 64 * 1024;
