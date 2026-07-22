use std::path::PathBuf;

use color_eyre::eyre::Result;
use ring::digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{debug, info, warn};

use super::stream::{FileTransferMessage, FileTransferMessageStream};
use crate::net::protocol::FileInfo;

struct PreparedTransfer {
    files: Vec<PathBuf>,
    file_infos: Vec<FileInfo>,
    total_size: u64,
}

/// Send files over a dedicated QUIC bi-stream.
///
/// Opens a new bi-stream on the connection, sends a `FileTransferOffer`,
/// waits for `FileTransferAccept`, then streams file data in 64 KiB chunks
/// with SHA-256 checksums per file.
pub async fn send_files(connection: &quinn::Connection, files: Vec<PathBuf>) -> Result<()> {
    let Some(transfer) = prepare_transfer(files).await? else {
        return Ok(());
    };

    let (send, recv) = connection.open_bi().await?;
    let mut stream = FileTransferMessageStream::new(send, recv);
    send_prepared(&mut stream, transfer).await?;

    // Gracefully close the Quinn send side after the protocol-independent flow.
    let (mut send, _) = stream.into_inner();
    send.finish().ok();
    Ok(())
}

async fn prepare_transfer(files: Vec<PathBuf>) -> Result<Option<PreparedTransfer>> {
    let mut regular_files = Vec::new();
    let mut file_infos = Vec::new();
    let mut total_size = 0u64;

    for path in files {
        let metadata = tokio::fs::metadata(&path).await?;
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
        regular_files.push(path);
        file_infos.push(FileInfo { name, size });
    }

    if regular_files.is_empty() {
        debug!("No regular files to transfer");
        return Ok(None);
    }

    Ok(Some(PreparedTransfer {
        files: regular_files,
        file_infos,
        total_size,
    }))
}

async fn send_prepared<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    transfer: PreparedTransfer,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let PreparedTransfer {
        files,
        file_infos,
        total_size,
    } = transfer;
    let transfer_id: u64 = rand::random();

    info!(
        "Starting file transfer {}: {} file(s), {} bytes total",
        transfer_id,
        files.len(),
        total_size
    );

    stream
        .send(FileTransferMessage::Offer {
            transfer_id,
            files: file_infos,
            total_size,
        })
        .await?;

    match stream.receive().await? {
        Some(FileTransferMessage::Accept { transfer_id: tid }) if tid == transfer_id => {
            debug!("Transfer {} accepted by peer", transfer_id);
        }
        Some(FileTransferMessage::Cancel { .. }) => {
            info!("Transfer {} cancelled by peer", transfer_id);
            return Ok(());
        }
        other => {
            warn!("Unexpected response to file transfer offer: {:?}", other);
            return Ok(());
        }
    }

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

            stream
                .send(FileTransferMessage::Chunk {
                    transfer_id,
                    file_index: file_index as u32,
                    offset,
                    data: buf[..n].to_vec(),
                })
                .await?;
            offset += n as u64;
        }

        let checksum = ctx
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        stream
            .send(FileTransferMessage::Complete {
                transfer_id,
                file_index: file_index as u32,
                checksum,
            })
            .await?;
        debug!(
            "File {}/{} sent: {} ({} bytes)",
            file_index + 1,
            files.len(),
            path.file_name().unwrap_or_default().to_string_lossy(),
            offset
        );
    }

    stream
        .send(FileTransferMessage::Done { transfer_id })
        .await?;
    info!("File transfer {} complete", transfer_id);
    Ok(())
}
