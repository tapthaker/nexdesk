use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use color_eyre::eyre::{eyre, Result};
use ring::digest;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::net::protocol::{self, Message};

fn safe_file_name(name: &str) -> Result<String> {
    let path = std::path::Path::new(name);
    let mut components = path.components();
    let Some(Component::Normal(_)) = components.next() else {
        return Err(eyre!("Unsafe file name in transfer: {name:?}"));
    };
    if path.is_absolute()
        || components.next().is_some()
        || !protocol::is_portable_transfer_file_name(name)
    {
        return Err(eyre!("Unsafe file name in transfer: {name:?}"));
    }
    Ok(name.to_string())
}

async fn sync_directory(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(&path)?.sync_all())
        .await
        .map_err(|e| eyre!("Directory sync task failed: {}", e))??;
    Ok(())
}

#[cfg(unix)]
fn restrict_staging_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        eyre!(
            "Failed to restrict file-transfer staging directory permissions: {}: {}",
            path.display(),
            e
        )
    })
}

#[cfg(not(unix))]
fn restrict_staging_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn create_received_file_std(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

async fn create_received_file(path: &Path) -> Result<tokio::fs::File> {
    let path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || create_received_file_std(&path))
        .await
        .map_err(|e| eyre!("File create task failed: {}", e))??;
    Ok(tokio::fs::File::from_std(file))
}

fn validate_transfer_id(actual: u64, expected: u64, context: &str) -> Result<()> {
    if actual != expected {
        return Err(eyre!(
            "Mismatched transfer_id in {context}: got {}, expected {}",
            actual,
            expected
        ));
    }
    Ok(())
}

fn validate_file_index(file_index: u32, file_count: usize, context: &str) -> Result<usize> {
    let idx = file_index as usize;
    if idx >= file_count {
        return Err(eyre!(
            "Invalid file_index in {context}: {} (file count {})",
            file_index,
            file_count
        ));
    }
    Ok(idx)
}

fn checksum_matches(computed: &str, received: &str) -> bool {
    computed.eq_ignore_ascii_case(received)
}

fn transfer_message_summary(msg: &Message) -> String {
    match msg {
        Message::Heartbeat { .. } => "Heartbeat".to_string(),
        Message::HeartbeatAck { .. } => "HeartbeatAck".to_string(),
        Message::Hello { hostname, .. } => format!(
            "Hello(hostname={})",
            crate::status::terminal_safe(hostname, protocol::MAX_PEER_NAME_BYTES)
        ),
        Message::HelloAck { accepted, .. } => format!("HelloAck(accepted={accepted})"),
        Message::PairingResult { success } => format!("PairingResult(success={success})"),
        Message::MouseMove { .. } => "MouseMove".to_string(),
        Message::MouseButton { .. } => "MouseButton".to_string(),
        Message::MouseScroll { .. } => "MouseScroll".to_string(),
        Message::SwitchScreen { .. } => "SwitchScreen".to_string(),
        Message::ReleaseScreen => "ReleaseScreen".to_string(),
        Message::KeyEvent { .. } => "KeyEvent".to_string(),
        Message::ScreenResize { .. } => "ScreenResize".to_string(),
        Message::ClipboardUpdate { content } => match content {
            crate::net::protocol::ClipboardContent::Text(text) => {
                format!("ClipboardUpdate(text_bytes={})", text.len())
            }
        },
        Message::FileTransferOffer {
            transfer_id,
            files,
            total_size,
        } => format!(
            "FileTransferOffer(transfer_id={transfer_id}, files={}, total_size={total_size})",
            files.len()
        ),
        Message::FileTransferAccept { transfer_id } => {
            format!("FileTransferAccept(transfer_id={transfer_id})")
        }
        Message::FileTransferChunk {
            transfer_id,
            file_index,
            offset,
            data,
        } => format!(
            "FileTransferChunk(transfer_id={transfer_id}, file_index={file_index}, offset={offset}, bytes={})",
            data.len()
        ),
        Message::FileTransferComplete {
            transfer_id,
            file_index,
            ..
        } => format!("FileTransferComplete(transfer_id={transfer_id}, file_index={file_index})"),
        Message::FileTransferDone { transfer_id } => {
            format!("FileTransferDone(transfer_id={transfer_id})")
        }
        Message::FileTransferCancel { transfer_id } => {
            format!("FileTransferCancel(transfer_id={transfer_id})")
        }
        Message::WakeDisplay => "WakeDisplay".to_string(),
    }
}

fn unexpected_transfer_message(transfer_id: u64, msg: &Message) -> color_eyre::eyre::Error {
    eyre!(
        "Unexpected message during file transfer {}: {}",
        transfer_id,
        transfer_message_summary(msg)
    )
}

fn validate_offer(files: &[crate::net::protocol::FileInfo], total_size: u64) -> Result<()> {
    if files.is_empty() {
        return Err(eyre!("File transfer offer contains no files"));
    }
    if files.len() > protocol::MAX_FILE_COUNT {
        return Err(eyre!(
            "File transfer offer contains too many files: {} (max {})",
            files.len(),
            protocol::MAX_FILE_COUNT
        ));
    }

    let mut names = HashSet::new();
    let mut computed_total = 0u64;
    for file in files {
        let safe_name = safe_file_name(&file.name)?;
        if !names.insert(safe_name.to_lowercase()) {
            return Err(eyre!(
                "Duplicate file name in transfer (case-insensitive): {}",
                file.name
            ));
        }
        computed_total = computed_total
            .checked_add(file.size)
            .ok_or_else(|| eyre!("File transfer size overflow"))?;
    }

    if computed_total != total_size {
        return Err(eyre!(
            "File transfer total size mismatch: offer={}, files={}",
            total_size,
            computed_total
        ));
    }
    if total_size > protocol::MAX_TRANSFER_SIZE {
        return Err(eyre!(
            "File transfer too large: {} bytes (max {})",
            total_size,
            protocol::MAX_TRANSFER_SIZE
        ));
    }
    Ok(())
}

/// Receive files over a dedicated QUIC bi-stream.
///
/// Reads a `FileTransferOffer` from the stream, sends `FileTransferAccept`,
/// then receives chunks and writes them to a staging directory.
/// Returns the list of received file paths.
pub async fn receive_files(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<Vec<PathBuf>> {
    // Read the offer
    let (transfer_id, files, total_size) =
        match tokio::time::timeout(super::TRANSFER_IDLE_TIMEOUT, recv_msg(&mut recv))
            .await
            .map_err(|_| eyre!("Timed out waiting for file transfer offer"))??
        {
            Some(Message::FileTransferOffer {
                transfer_id,
                files,
                total_size,
            }) => (transfer_id, files, total_size),
            other => {
                return Err(eyre!(
                    "Expected FileTransferOffer, got: {}",
                    other
                        .as_ref()
                        .map(transfer_message_summary)
                        .unwrap_or_else(|| "end of stream".to_string())
                ));
            }
        };

    validate_offer(&files, total_size)?;

    info!(
        "File transfer offer {}: {} file(s), {} bytes",
        transfer_id,
        files.len(),
        total_size
    );

    // Accept the transfer
    let accept = Message::FileTransferAccept { transfer_id };
    send_msg(&mut send, &accept).await?;

    // Create staging directory
    let staging_dir = tempfile::Builder::new().prefix("nexdesk-").tempdir()?;
    restrict_staging_dir_permissions(staging_dir.path())?;

    // Pre-create output paths and hasher contexts
    let mut output_paths: Vec<PathBuf> = Vec::new();
    for file_info in &files {
        let safe_name = safe_file_name(&file_info.name)?;
        let path = staging_dir.path().join(safe_name);
        output_paths.push(path);
    }

    let mut open_files: Vec<Option<tokio::fs::File>> = (0..files.len()).map(|_| None).collect();
    let mut received_sizes = vec![0u64; files.len()];
    let mut completed = vec![false; files.len()];
    let mut hashers: Vec<Option<digest::Context>> = files
        .iter()
        .map(|_| Some(digest::Context::new(&digest::SHA256)))
        .collect();

    // Receive chunks
    loop {
        match tokio::time::timeout(super::TRANSFER_IDLE_TIMEOUT, recv_msg(&mut recv))
            .await
            .map_err(|_| {
                eyre!(
                    "File transfer {} timed out waiting for peer data",
                    transfer_id
                )
            })?? {
            Some(Message::FileTransferChunk {
                transfer_id: tid,
                file_index,
                offset,
                data,
            }) => {
                validate_transfer_id(tid, transfer_id, "chunk")?;
                let idx = validate_file_index(file_index, files.len(), "chunk")?;

                if completed[idx] {
                    return Err(eyre!(
                        "Received chunk after completion for {}",
                        files[idx].name
                    ));
                }
                if offset != received_sizes[idx] {
                    return Err(eyre!(
                        "Unexpected offset for {}: got {}, expected {}",
                        files[idx].name,
                        offset,
                        received_sizes[idx]
                    ));
                }
                let new_size = received_sizes[idx].saturating_add(data.len() as u64);
                if new_size > files[idx].size {
                    return Err(eyre!(
                        "Received more data than advertised for {}",
                        files[idx].name
                    ));
                }

                // Open file on first chunk
                if open_files[idx].is_none() {
                    let file = create_received_file(&output_paths[idx]).await?;
                    open_files[idx] = Some(file);
                }

                if let Some(ref mut file) = open_files[idx] {
                    file.write_all(&data).await?;
                }
                if let Some(ref mut ctx) = hashers[idx] {
                    ctx.update(&data);
                }
                received_sizes[idx] = new_size;
            }
            Some(Message::FileTransferComplete {
                transfer_id: tid,
                file_index,
                checksum,
            }) => {
                validate_transfer_id(tid, transfer_id, "completion")?;
                let idx = validate_file_index(file_index, files.len(), "completion")?;

                if completed[idx] {
                    return Err(eyre!("Duplicate completion for {}", files[idx].name));
                }
                if received_sizes[idx] != files[idx].size {
                    return Err(eyre!(
                        "File {} ended at {} bytes, expected {}",
                        files[idx].name,
                        received_sizes[idx],
                        files[idx].size
                    ));
                }

                // Flush and close the file. For empty files, create the file at completion time.
                if let Some(mut file) = open_files[idx].take() {
                    file.flush().await?;
                    file.sync_all().await?;
                } else if files[idx].size == 0 {
                    let file = create_received_file(&output_paths[idx]).await?;
                    file.sync_all().await?;
                }

                // Verify checksum
                if let Some(ctx) = hashers[idx].take() {
                    let hash = ctx.finish();
                    let computed: String =
                        hash.as_ref().iter().map(|b| format!("{:02x}", b)).collect();
                    if !checksum_matches(&computed, &checksum) {
                        return Err(eyre!(
                            "Checksum mismatch for file {} ({}): expected {}, got {}",
                            idx,
                            files[idx].name,
                            checksum,
                            computed
                        ));
                    }
                    completed[idx] = true;
                    debug!("Checksum verified for file {} ({})", idx, files[idx].name);
                }
            }
            Some(Message::FileTransferDone { transfer_id: tid }) => {
                validate_transfer_id(tid, transfer_id, "done")?;
                if completed.iter().any(|done| !done) {
                    return Err(eyre!(
                        "Transfer {} ended before all files completed",
                        transfer_id
                    ));
                }
                sync_directory(staging_dir.path()).await?;
                info!(
                    "File transfer {} complete ({} files received)",
                    transfer_id,
                    files.len()
                );
                break;
            }
            Some(Message::FileTransferCancel { transfer_id: tid }) => {
                validate_transfer_id(tid, transfer_id, "cancel")?;
                warn!("File transfer {} cancelled by sender", transfer_id);
                return Ok(vec![]);
            }
            None => {
                return Err(eyre!(
                    "Stream closed before file transfer {} completed",
                    transfer_id
                ));
            }
            Some(other) => {
                return Err(unexpected_transfer_message(transfer_id, &other));
            }
        }
    }

    // Prevent the staging directory from being deleted on drop
    let _staging_path = staging_dir.keep();

    Ok(output_paths)
}

async fn send_msg(send: &mut quinn::SendStream, msg: &Message) -> Result<()> {
    let bytes = protocol::encode(msg)?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn recv_msg(recv: &mut quinn::RecvStream) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > protocol::MAX_MESSAGE_SIZE {
        return Err(eyre!("Message too large: {} bytes", len));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => {
            color_eyre::eyre::eyre!("Connection closed mid-message")
        }
        other => other.into(),
    })?;
    let msg: Message = bincode::deserialize(&body)?;
    protocol::validate_message(&msg)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::protocol::FileInfo;

    #[test]
    fn rejects_unsafe_file_names() {
        assert!(safe_file_name("../evil").is_err());
        assert!(safe_file_name("dir/file.txt").is_err());
        assert!(safe_file_name("/tmp/file.txt").is_err());
        assert!(safe_file_name("bad\0name.txt").is_err());
        assert!(safe_file_name("dir\\file.txt").is_err());
        assert!(safe_file_name(&"a".repeat(protocol::MAX_TRANSFER_FILE_NAME_BYTES + 1)).is_err());
        assert!(safe_file_name("").is_err());
    }

    #[test]
    fn accepts_valid_posix_file_names() {
        for name in ["file.txt", "CON", "bad:name.txt", "trailing."] {
            assert_eq!(safe_file_name(name).unwrap(), name);
        }
    }

    #[cfg(unix)]
    #[test]
    fn received_files_are_created_private_and_without_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("received.txt");
        let file = create_received_file_std(&path).unwrap();
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(create_received_file_std(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn staging_directory_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        restrict_staging_dir_permissions(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn transfer_idle_timeout_is_bounded() {
        assert_eq!(super::super::TRANSFER_IDLE_TIMEOUT.as_secs(), 120);
    }

    #[test]
    fn rejects_mismatched_transfer_ids() {
        assert!(validate_transfer_id(1, 1, "chunk").is_ok());
        assert!(validate_transfer_id(2, 1, "chunk").is_err());
    }

    #[test]
    fn rejects_invalid_file_indices() {
        assert_eq!(validate_file_index(0, 1, "chunk").unwrap(), 0);
        assert!(validate_file_index(1, 1, "chunk").is_err());
        assert!(validate_file_index(u32::MAX, 1, "chunk").is_err());
    }

    #[test]
    fn checksum_comparison_accepts_protocol_valid_case_variants() {
        assert!(checksum_matches("abcdef012345", "ABCDEF012345"));
        assert!(!checksum_matches("abcdef012345", "abcdef012346"));
    }

    #[test]
    fn unexpected_transfer_messages_are_fatal() {
        let err = unexpected_transfer_message(
            7,
            &Message::ScreenResize {
                screen: crate::net::protocol::ScreenLayout {
                    width: 1,
                    height: 1,
                },
            },
        );
        assert!(err
            .to_string()
            .contains("Unexpected message during file transfer 7"));
    }

    #[test]
    fn transfer_message_summary_omits_large_or_hostile_payloads() {
        let message = Message::ClipboardUpdate {
            content: crate::net::protocol::ClipboardContent::Text("\x1b[31m".repeat(1024)),
        };
        assert_eq!(
            transfer_message_summary(&message),
            "ClipboardUpdate(text_bytes=5120)"
        );

        let chunk = Message::FileTransferChunk {
            transfer_id: 1,
            file_index: 2,
            offset: 3,
            data: vec![b'x'; protocol::MAX_FILE_CHUNK_SIZE],
        };
        assert_eq!(
            transfer_message_summary(&chunk),
            format!(
                "FileTransferChunk(transfer_id=1, file_index=2, offset=3, bytes={})",
                protocol::MAX_FILE_CHUNK_SIZE
            )
        );
    }

    #[test]
    fn validates_offer_totals_and_duplicates() {
        let files = vec![
            FileInfo {
                name: "a.txt".into(),
                size: 2,
            },
            FileInfo {
                name: "b.txt".into(),
                size: 3,
            },
        ];
        validate_offer(&files, 5).unwrap();
        assert!(validate_offer(&files, 4).is_err());

        let dupes = vec![
            FileInfo {
                name: "a.txt".into(),
                size: 1,
            },
            FileInfo {
                name: "a.txt".into(),
                size: 1,
            },
        ];
        assert!(validate_offer(&dupes, 2).is_err());

        let case_dupes = vec![
            FileInfo {
                name: "a.txt".into(),
                size: 1,
            },
            FileInfo {
                name: "A.TXT".into(),
                size: 1,
            },
        ];
        assert!(validate_offer(&case_dupes, 2).is_err());
    }
}
