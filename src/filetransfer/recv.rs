use std::path::PathBuf;

use color_eyre::eyre::{eyre, Result};
use ring::digest;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

use super::stream::{FileTransferMessage, FileTransferMessageStream};

/// Receive files over a dedicated QUIC bi-stream.
///
/// Adapts the Quinn stream halves to the protocol-independent typed message
/// flow and returns the list of received file paths.
pub async fn receive_files(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
) -> Result<Vec<PathBuf>> {
    let mut stream = FileTransferMessageStream::new(send, recv);
    receive_files_over_stream(&mut stream).await
}

pub(crate) async fn receive_files_over_stream<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
) -> Result<Vec<PathBuf>>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let (transfer_id, files, total_size) = match stream.receive().await? {
        Some(FileTransferMessage::Offer {
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

    stream
        .send(FileTransferMessage::Accept { transfer_id })
        .await?;

    let staging_dir = tempfile::Builder::new().prefix("nexdesk-").tempdir()?;
    let output_paths: Vec<PathBuf> = files
        .iter()
        .map(|file_info| staging_dir.path().join(&file_info.name))
        .collect();
    let mut open_files: Vec<Option<tokio::fs::File>> = (0..files.len()).map(|_| None).collect();
    let mut hashers: Vec<Option<digest::Context>> = files
        .iter()
        .map(|_| Some(digest::Context::new(&digest::SHA256)))
        .collect();

    loop {
        match stream.receive().await? {
            Some(FileTransferMessage::Chunk {
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

                if open_files[idx].is_none() {
                    open_files[idx] = Some(tokio::fs::File::create(&output_paths[idx]).await?);
                }
                if let Some(file) = &mut open_files[idx] {
                    file.write_all(&data).await?;
                }
                if let Some(ctx) = &mut hashers[idx] {
                    ctx.update(&data);
                }
            }
            Some(FileTransferMessage::Complete {
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

                if let Some(mut file) = open_files[idx].take() {
                    file.flush().await.ok();
                }
                if let Some(ctx) = hashers[idx].take() {
                    let computed: String = ctx
                        .finish()
                        .as_ref()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
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
            Some(FileTransferMessage::Done { transfer_id: tid }) => {
                if tid != transfer_id {
                    continue;
                }
                info!(
                    "File transfer {} complete ({} files received)",
                    transfer_id,
                    files.len()
                );
                break;
            }
            Some(FileTransferMessage::Cancel { transfer_id: tid }) => {
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

    let _staging_path = staging_dir.keep();
    Ok(output_paths)
}
