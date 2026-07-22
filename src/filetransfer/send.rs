use std::path::PathBuf;

use color_eyre::eyre::Result;
use ring::digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{debug, info, warn};

use super::id::{RandomTransferIdSource, TransferIdSource};
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
    send_files_with_id_source(connection, files, &RandomTransferIdSource).await
}

pub(crate) async fn send_files_with_id_source<I>(
    connection: &quinn::Connection,
    files: Vec<PathBuf>,
    id_source: &I,
) -> Result<()>
where
    I: TransferIdSource,
{
    let Some(transfer) = prepare_transfer(files).await? else {
        return Ok(());
    };

    let (send, recv) = connection.open_bi().await?;
    let mut stream = FileTransferMessageStream::new(send, recv);
    send_prepared(&mut stream, transfer, id_source.next_transfer_id()).await?;

    // Gracefully close the Quinn send side after the protocol-independent flow.
    let (mut send, _) = stream.into_inner();
    send.finish().ok();
    Ok(())
}

#[cfg(test)]
pub(crate) async fn send_files_over_stream<W, R, I>(
    stream: &mut FileTransferMessageStream<W, R>,
    files: Vec<PathBuf>,
    id_source: &I,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    I: TransferIdSource,
{
    if let Some(transfer) = prepare_transfer(files).await? {
        send_prepared(stream, transfer, id_source.next_transfer_id()).await?;
    }
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
    transfer_id: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedTransferId(u64);

    impl TransferIdSource for FixedTransferId {
        fn next_transfer_id(&self) -> u64 {
            self.0
        }
    }

    #[tokio::test]
    async fn injected_transfer_identifier_is_used_for_every_outgoing_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("note.txt");
        tokio::fs::write(&path, b"hello").await.unwrap();

        let (sender_writer, peer_reader) = tokio::io::duplex(4096);
        let (peer_writer, sender_reader) = tokio::io::duplex(4096);
        let mut sender = FileTransferMessageStream::new(sender_writer, sender_reader);
        let mut peer = FileTransferMessageStream::new(peer_writer, peer_reader);
        let ids = FixedTransferId(4242);

        let send = send_files_over_stream(&mut sender, vec![path], &ids);
        let receive = async {
            let Some(FileTransferMessage::Offer { transfer_id, .. }) = peer.receive().await? else {
                return Err(color_eyre::eyre::eyre!("expected offer"));
            };
            assert_eq!(transfer_id, 4242);
            peer.send(FileTransferMessage::Accept { transfer_id })
                .await?;

            loop {
                let message = peer
                    .receive()
                    .await?
                    .ok_or_else(|| color_eyre::eyre::eyre!("sender closed before done"))?;
                match message {
                    FileTransferMessage::Chunk { transfer_id, .. }
                    | FileTransferMessage::Complete { transfer_id, .. } => {
                        assert_eq!(transfer_id, 4242);
                    }
                    FileTransferMessage::Done { transfer_id } => {
                        assert_eq!(transfer_id, 4242);
                        break;
                    }
                    other => {
                        return Err(color_eyre::eyre::eyre!(
                            "unexpected sender message: {other:?}"
                        ));
                    }
                }
            }
            Ok::<(), color_eyre::Report>(())
        };

        tokio::try_join!(send, receive).unwrap();
    }
}
