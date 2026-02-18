pub mod send;
pub mod recv;
pub mod clipboard_files;

/// Chunk size for file transfer (64 KiB).
pub const CHUNK_SIZE: usize = 64 * 1024;
