use std::path::PathBuf;

use color_eyre::eyre::Result;
use ring::digest;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use crate::net::framing;
use crate::net::protocol::{self, FileInfo, Message};

/// Send files over a dedicated QUIC bi-stream.
///
/// Opens a new bi-stream on the connection, sends a `FileTransferOffer`,
/// waits for `FileTransferAccept`, then streams file data in 64 KiB chunks
/// with SHA-256 checksums per file.
pub async fn send_files(connection: &quinn::Connection, files: Vec<PathBuf>) -> Result<()> {
    // Gather file metadata
    let mut file_infos = Vec::new();
    let mut total_size = 0u64;
    for path in &files {
        let metadata = tokio::fs::metadata(path).await?;
        if !metadata.is_file() {
            debug!("Skipping non-file: {}", path.display());
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let size = metadata.len();
        total_size += size;
        file_infos.push(FileInfo { name, size });
    }

    // Filter to only regular files
    let files: Vec<PathBuf> = files.into_iter().filter(|p| p.is_file()).collect();

    if files.is_empty() {
        debug!("No regular files to transfer");
        return Ok(());
    }

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
    match recv_msg(&mut recv).await? {
        Some(Message::FileTransferAccept { transfer_id: tid }) if tid == transfer_id => {
            debug!("Transfer {} accepted by peer", transfer_id);
        }
        Some(Message::FileTransferCancel { .. }) => {
            info!("Transfer {} cancelled by peer", transfer_id);
            return Ok(());
        }
        other => {
            warn!("Unexpected response to file transfer offer: {:?}", other);
            return Ok(());
        }
    }

    // Stream each file
    for (file_index, path) in files.iter().enumerate() {
        let mut file = tokio::fs::File::open(path).await?;
        let mut offset = 0u64;
        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0u8; super::CHUNK_SIZE];

        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);

            let chunk = Message::FileTransferChunk {
                transfer_id,
                file_index: file_index as u32,
                offset,
                data: buf[..n].to_vec(),
            };
            send_msg(&mut send, &chunk).await?;
            offset += n as u64;
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

    // Gracefully close the send side
    send.finish().ok();

    Ok(())
}

async fn send_msg(send: &mut quinn::SendStream, msg: &Message) -> Result<()> {
    framing::send_message(send, msg).await
}

async fn recv_msg(recv: &mut quinn::RecvStream) -> Result<Option<Message>> {
    framing::recv_message(recv).await
}
