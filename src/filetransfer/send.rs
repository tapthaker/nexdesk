use std::collections::HashSet;
use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use ring::digest;
use tokio::io::AsyncReadExt;
use tracing::{debug, info};

use crate::net::protocol::{self, FileInfo, Message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferResponse {
    Accepted,
    Cancelled,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity;

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity
}

fn validate_opened_file_metadata(
    metadata: &std::fs::Metadata,
    expected_size: u64,
    expected_identity: FileIdentity,
    path: &std::path::Path,
) -> Result<()> {
    if !metadata.is_file() {
        return Err(color_eyre::eyre::eyre!(
            "File changed during transfer: {} is no longer a regular file",
            path.display()
        ));
    }
    if metadata.len() != expected_size {
        return Err(color_eyre::eyre::eyre!(
            "File changed during transfer: {} is now {} bytes, expected {} bytes",
            path.display(),
            metadata.len(),
            expected_size
        ));
    }
    if file_identity(metadata) != expected_identity {
        return Err(color_eyre::eyre::eyre!(
            "File changed during transfer: {} is no longer the same file",
            path.display()
        ));
    }
    Ok(())
}

fn offer_response_summary(msg: &Message) -> String {
    match msg {
        Message::FileTransferAccept { transfer_id } => {
            format!("FileTransferAccept(transfer_id={transfer_id})")
        }
        Message::FileTransferCancel { transfer_id } => {
            format!("FileTransferCancel(transfer_id={transfer_id})")
        }
        Message::FileTransferOffer {
            transfer_id,
            files,
            total_size,
        } => format!(
            "FileTransferOffer(transfer_id={transfer_id}, files={}, total_size={total_size})",
            files.len()
        ),
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
        Message::ClipboardUpdate {
            content: crate::net::protocol::ClipboardContent::Text(text),
        } => format!("ClipboardUpdate(text_bytes={})", text.len()),
        Message::Hello { hostname, .. } => format!(
            "Hello(hostname={})",
            crate::status::terminal_safe(hostname, protocol::MAX_PEER_NAME_BYTES)
        ),
        Message::HelloAck { accepted, .. } => format!("HelloAck(accepted={accepted})"),
        Message::Heartbeat { .. } => "Heartbeat".to_string(),
        Message::HeartbeatAck { .. } => "HeartbeatAck".to_string(),
        Message::PairingResult { success } => format!("PairingResult(success={success})"),
        Message::MouseMove { .. } => "MouseMove".to_string(),
        Message::MouseButton { .. } => "MouseButton".to_string(),
        Message::MouseScroll { .. } => "MouseScroll".to_string(),
        Message::SwitchScreen { .. } => "SwitchScreen".to_string(),
        Message::ReleaseScreen => "ReleaseScreen".to_string(),
        Message::KeyEvent { .. } => "KeyEvent".to_string(),
        Message::ScreenResize { .. } => "ScreenResize".to_string(),
        Message::WakeDisplay => "WakeDisplay".to_string(),
    }
}

fn validate_offer_response(msg: Option<Message>, transfer_id: u64) -> Result<OfferResponse> {
    match msg {
        Some(Message::FileTransferAccept { transfer_id: tid }) if tid == transfer_id => {
            Ok(OfferResponse::Accepted)
        }
        Some(Message::FileTransferCancel { transfer_id: tid }) if tid == transfer_id => {
            Ok(OfferResponse::Cancelled)
        }
        Some(other) => Err(color_eyre::eyre::eyre!(
            "Unexpected response to file transfer offer: {}",
            offer_response_summary(&other)
        )),
        None => Err(color_eyre::eyre::eyre!(
            "Stream closed before file transfer offer was accepted"
        )),
    }
}

/// Send files over a dedicated QUIC bi-stream.
///
/// Opens a new bi-stream on the connection, sends a `FileTransferOffer`,
/// waits for `FileTransferAccept`, then streams file data in 64 KiB chunks
/// with SHA-256 checksums per file.
pub async fn send_files(connection: &quinn::Connection, files: Vec<PathBuf>) -> Result<()> {
    // Gather file metadata
    let mut file_infos = Vec::new();
    let mut regular_files = Vec::new();
    let mut seen_names = HashSet::new();
    let mut expected_identities = Vec::new();
    let mut total_size = 0u64;
    for path in &files {
        let metadata = tokio::fs::metadata(path).await?;
        if !metadata.is_file() {
            debug!("Skipping non-file: {}", path.display());
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| color_eyre::eyre::eyre!("Invalid file name: {}", path.display()))?
            .to_string();
        if name.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "Invalid empty file name: {}",
                path.display()
            ));
        }
        if !protocol::is_portable_transfer_file_name(&name) {
            return Err(color_eyre::eyre::eyre!(
                "Unsafe file name in transfer: {:?}",
                name
            ));
        }
        if !seen_names.insert(name.to_lowercase()) {
            return Err(color_eyre::eyre::eyre!(
                "Duplicate file name in transfer (case-insensitive): {}. Rename one of the files before transferring.",
                name
            ));
        }
        let size = metadata.len();
        let identity = file_identity(&metadata);
        total_size = total_size
            .checked_add(size)
            .ok_or_else(|| color_eyre::eyre::eyre!("File transfer size overflow"))?;
        file_infos.push(FileInfo { name, size });
        regular_files.push(path.clone());
        expected_identities.push(identity);
    }

    let files = regular_files;

    if files.is_empty() {
        debug!("No regular files to transfer");
        return Ok(());
    }
    if files.len() > protocol::MAX_FILE_COUNT {
        return Err(color_eyre::eyre::eyre!(
            "Too many files to transfer: {} (max {})",
            files.len(),
            protocol::MAX_FILE_COUNT
        ));
    }
    if total_size > protocol::MAX_TRANSFER_SIZE {
        return Err(color_eyre::eyre::eyre!(
            "File transfer too large: {} bytes (max {})",
            total_size,
            protocol::MAX_TRANSFER_SIZE
        ));
    }

    let expected_sizes: Vec<u64> = file_infos.iter().map(|info| info.size).collect();
    let transfer_id: u64 = rand::random();

    info!(
        "Starting file transfer {}: {} file(s), {} bytes total",
        transfer_id,
        files.len(),
        total_size
    );

    // Open a dedicated bi-stream for this transfer
    let (mut send, mut recv) = connection.open_bi().await?;

    // Send offer
    let offer = Message::FileTransferOffer {
        transfer_id,
        files: file_infos,
        total_size,
    };
    send_msg(&mut send, &offer).await?;

    // Wait for accept
    match validate_offer_response(
        tokio::time::timeout(super::TRANSFER_IDLE_TIMEOUT, recv_msg(&mut recv))
            .await
            .map_err(|_| {
                color_eyre::eyre::eyre!(
                    "File transfer {} timed out waiting for accept/cancel",
                    transfer_id
                )
            })??,
        transfer_id,
    )? {
        OfferResponse::Accepted => {
            debug!("Transfer {} accepted by peer", transfer_id);
        }
        OfferResponse::Cancelled => {
            info!("Transfer {} cancelled by peer", transfer_id);
            return Ok(());
        }
    }

    // Stream each file
    for (file_index, path) in files.iter().enumerate() {
        let expected_size = expected_sizes[file_index];
        let mut file = tokio::fs::File::open(path).await?;
        let opened_metadata = file.metadata().await?;
        validate_opened_file_metadata(
            &opened_metadata,
            expected_size,
            expected_identities[file_index],
            path,
        )?;
        let mut offset = 0u64;
        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0u8; super::CHUNK_SIZE];

        while offset < expected_size {
            let remaining = expected_size - offset;
            let read_len = (remaining as usize).min(buf.len());
            let n = file.read(&mut buf[..read_len]).await?;
            if n == 0 {
                return Err(color_eyre::eyre::eyre!(
                    "File changed during transfer: {} ended at {} bytes, expected {} bytes",
                    path.display(),
                    offset,
                    expected_size
                ));
            }
            ctx.update(&buf[..n]);

            let chunk = Message::FileTransferChunk {
                transfer_id,
                file_index: file_index as u32,
                offset,
                data: buf[..n].to_vec(),
            };
            send_msg(&mut send, &chunk).await?;
            offset = offset
                .checked_add(n as u64)
                .ok_or_else(|| color_eyre::eyre::eyre!("File transfer offset overflow"))?;
        }

        let mut extra = [0u8; 1];
        if file.read(&mut extra).await? != 0 {
            return Err(color_eyre::eyre::eyre!(
                "File changed during transfer: {} grew beyond advertised size of {} bytes",
                path.display(),
                expected_size
            ));
        }

        let hash = ctx.finish();
        let checksum = hash
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        let complete = Message::FileTransferComplete {
            transfer_id,
            file_index: file_index as u32,
            checksum,
        };
        send_msg(&mut send, &complete).await?;
        debug!(
            "File {}/{} sent: {} ({} bytes)",
            file_index + 1,
            files.len(),
            path.file_name().unwrap_or_default().to_string_lossy(),
            offset
        );
    }

    // Signal transfer complete
    let done = Message::FileTransferDone { transfer_id };
    send_msg(&mut send, &done).await?;

    info!("File transfer {} complete", transfer_id);

    // Gracefully close the send side so the receiver observes end-of-stream
    // only after the FileTransferDone message has been fully queued.
    send.finish()
        .wrap_err("Failed to finish file transfer send stream")?;

    Ok(())
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
        return Err(color_eyre::eyre::eyre!("Message too large: {} bytes", len));
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

    #[test]
    fn offer_response_requires_matching_transfer_id() {
        assert_eq!(
            validate_offer_response(Some(Message::FileTransferAccept { transfer_id: 7 }), 7)
                .unwrap(),
            OfferResponse::Accepted
        );
        assert_eq!(
            validate_offer_response(Some(Message::FileTransferCancel { transfer_id: 7 }), 7)
                .unwrap(),
            OfferResponse::Cancelled
        );
        assert!(
            validate_offer_response(Some(Message::FileTransferAccept { transfer_id: 8 }), 7)
                .is_err()
        );
        assert!(
            validate_offer_response(Some(Message::FileTransferCancel { transfer_id: 8 }), 7)
                .is_err()
        );
        assert!(validate_offer_response(None, 7).is_err());
    }

    #[test]
    fn opened_file_metadata_must_still_match_offer() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, b"abc").unwrap();
        let metadata = std::fs::metadata(&file_path).unwrap();
        let identity = file_identity(&metadata);
        validate_opened_file_metadata(&metadata, 3, identity, &file_path).unwrap();
        assert!(validate_opened_file_metadata(&metadata, 2, identity, &file_path).is_err());

        let dir_metadata = std::fs::metadata(dir.path()).unwrap();
        assert!(validate_opened_file_metadata(
            &dir_metadata,
            0,
            file_identity(&dir_metadata),
            dir.path()
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn opened_file_identity_must_still_match_offer() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.txt");
        let second_path = dir.path().join("second.txt");
        std::fs::write(&first_path, b"abc").unwrap();
        std::fs::write(&second_path, b"abc").unwrap();
        let first_metadata = std::fs::metadata(&first_path).unwrap();
        let second_metadata = std::fs::metadata(&second_path).unwrap();
        assert!(validate_opened_file_metadata(
            &second_metadata,
            3,
            file_identity(&first_metadata),
            &first_path,
        )
        .is_err());
    }

    #[test]
    fn unexpected_offer_response_omits_large_or_hostile_payloads() {
        let err = validate_offer_response(
            Some(Message::ClipboardUpdate {
                content: crate::net::protocol::ClipboardContent::Text("\x1b[31m".repeat(1024)),
            }),
            7,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Unexpected response to file transfer offer"));
        assert!(message.contains("ClipboardUpdate(text_bytes=5120)"));
        assert!(!message.contains("\x1b"));

        let err = validate_offer_response(
            Some(Message::FileTransferChunk {
                transfer_id: 1,
                file_index: 2,
                offset: 3,
                data: vec![b'x'; protocol::MAX_FILE_CHUNK_SIZE],
            }),
            7,
        )
        .unwrap_err();
        assert!(err.to_string().contains(&format!(
            "FileTransferChunk(transfer_id=1, file_index=2, offset=3, bytes={})",
            protocol::MAX_FILE_CHUNK_SIZE
        )));
    }
}
