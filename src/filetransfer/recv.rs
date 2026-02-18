use std::path::PathBuf;

use color_eyre::eyre::{Result, eyre};
use ring::digest;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::net::protocol::{self, Message};

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
    let (transfer_id, files, total_size) = match recv_msg(&mut recv).await? {
        Some(Message::FileTransferOffer {
            transfer_id,
            files,
            total_size,
        }) => (transfer_id, files, total_size),
        other => {
            return Err(eyre!("Expected FileTransferOffer, got: {:?}", other));
        }
    };

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
    let staging_dir = tempfile::Builder::new()
        .prefix("nexdesk-")
        .tempdir()?;

    // Pre-create output paths and hasher contexts
    let mut output_paths: Vec<PathBuf> = Vec::new();
    for file_info in &files {
        let path = staging_dir.path().join(&file_info.name);
        output_paths.push(path);
    }

    let mut open_files: Vec<Option<tokio::fs::File>> = (0..files.len()).map(|_| None).collect();
    let mut hashers: Vec<Option<digest::Context>> = files
        .iter()
        .map(|_| Some(digest::Context::new(&digest::SHA256)))
        .collect();

    // Receive chunks
    loop {
        match recv_msg(&mut recv).await? {
            Some(Message::FileTransferChunk {
                transfer_id: tid,
                file_index,
                data,
                ..
            }) => {
                if tid != transfer_id {
                    continue;
                }
                let idx = file_index as usize;
                if idx >= files.len() {
                    warn!("Invalid file_index: {}", file_index);
                    continue;
                }

                // Open file on first chunk
                if open_files[idx].is_none() {
                    let file = tokio::fs::File::create(&output_paths[idx]).await?;
                    open_files[idx] = Some(file);
                }

                if let Some(ref mut file) = open_files[idx] {
                    file.write_all(&data).await?;
                }
                if let Some(ref mut ctx) = hashers[idx] {
                    ctx.update(&data);
                }
            }
            Some(Message::FileTransferComplete {
                transfer_id: tid,
                file_index,
                checksum,
            }) => {
                if tid != transfer_id {
                    continue;
                }
                let idx = file_index as usize;
                if idx >= files.len() {
                    continue;
                }

                // Flush and close the file
                if let Some(mut file) = open_files[idx].take() {
                    file.flush().await.ok();
                }

                // Verify checksum
                if let Some(ctx) = hashers[idx].take() {
                    let hash = ctx.finish();
                    let computed: String = hash
                        .as_ref()
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    if computed != checksum {
                        warn!(
                            "Checksum mismatch for file {} ({}): expected {}, got {}",
                            idx, files[idx].name, checksum, computed
                        );
                    } else {
                        debug!("Checksum verified for file {} ({})", idx, files[idx].name);
                    }
                }
            }
            Some(Message::FileTransferDone { transfer_id: tid }) => {
                if tid != transfer_id {
                    continue;
                }
                info!("File transfer {} complete ({} files received)", transfer_id, files.len());
                break;
            }
            Some(Message::FileTransferCancel { transfer_id: tid }) => {
                if tid != transfer_id {
                    continue;
                }
                warn!("File transfer {} cancelled by sender", transfer_id);
                return Ok(vec![]);
            }
            None => {
                warn!("Stream closed during file transfer {}", transfer_id);
                return Ok(vec![]);
            }
            Some(other) => {
                debug!("Unexpected message during file transfer: {:?}", other);
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
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => {
            color_eyre::eyre::eyre!("Connection closed mid-message")
        }
        other => other.into(),
    })?;
    let msg: Message = bincode::deserialize(&body)?;
    Ok(Some(msg))
}
