use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use color_eyre::eyre::{eyre, Result, WrapErr};
use ring::digest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::id::{RandomTransferIdSource, TransferIdSource};
use super::stream::{FileTransferMessage, FileTransferMessageStream};
use crate::net::protocol::FileInfo;

const TRANSFER_IO_TIMEOUT: Duration = Duration::from_secs(30);

struct PreparedFile {
    path: PathBuf,
    file: tokio::fs::File,
    size: u64,
    modified: Option<SystemTime>,
}

struct PreparedTransfer {
    files: Vec<PreparedFile>,
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
    let mut prepared_files = Vec::new();
    let mut file_infos = Vec::new();
    let mut total_size = 0u64;

    for path in files {
        let file = tokio::fs::File::open(&path).await?;
        let metadata = file.metadata().await?;
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
        prepared_files.push(PreparedFile {
            path,
            file,
            size,
            modified: metadata.modified().ok(),
        });
    }

    if prepared_files.is_empty() {
        debug!("No regular files to transfer");
        return Ok(None);
    }

    Ok(Some(PreparedTransfer {
        files: prepared_files,
        file_infos,
        total_size,
    }))
}

async fn send_prepared<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    mut transfer: PreparedTransfer,
    transfer_id: u64,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    info!(
        "Starting file transfer {}: {} file(s), {} bytes total",
        transfer_id,
        transfer.files.len(),
        transfer.total_size
    );

    send_with_timeout(
        stream,
        FileTransferMessage::Offer {
            transfer_id,
            files: transfer.file_infos,
            total_size: transfer.total_size,
        },
    )
    .await?;

    match receive_with_timeout(stream).await? {
        Some(FileTransferMessage::Accept { transfer_id: tid }) if tid == transfer_id => {
            debug!("Transfer {} accepted by peer", transfer_id);
        }
        Some(FileTransferMessage::Cancel { transfer_id: tid }) if tid == transfer_id => {
            info!("Transfer {} cancelled by peer", transfer_id);
            return Ok(());
        }
        other => {
            warn!("Unexpected response to file transfer offer: {:?}", other);
            return Ok(());
        }
    }

    let file_count = transfer.files.len();
    for (file_index, prepared) in transfer.files.iter_mut().enumerate() {
        let mut offset = 0u64;
        let mut ctx = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0u8; super::CHUNK_SIZE];

        while offset < prepared.size {
            let remaining = (prepared.size - offset) as usize;
            let read_limit = remaining.min(buf.len());
            let n = prepared.file.read(&mut buf[..read_limit]).await?;
            if n == 0 {
                return cancel_with_error(
                    stream,
                    transfer_id,
                    eyre!(
                        "File truncated during transfer: {} (expected {} bytes, read {})",
                        prepared.path.display(),
                        prepared.size,
                        offset
                    ),
                )
                .await;
            }
            ctx.update(&buf[..n]);
            send_with_timeout(
                stream,
                FileTransferMessage::Chunk {
                    transfer_id,
                    file_index: file_index as u32,
                    offset,
                    data: buf[..n].to_vec(),
                },
            )
            .await?;
            offset += n as u64;
        }

        let mut extra = [0u8; 1];
        if prepared.file.read(&mut extra).await? != 0 {
            return cancel_with_error(
                stream,
                transfer_id,
                eyre!("File grew during transfer: {}", prepared.path.display()),
            )
            .await;
        }

        let final_metadata = prepared.file.metadata().await?;
        if final_metadata.len() != prepared.size
            || prepared.modified != final_metadata.modified().ok()
        {
            return cancel_with_error(
                stream,
                transfer_id,
                eyre!("File mutated during transfer: {}", prepared.path.display()),
            )
            .await;
        }

        let checksum = ctx
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        send_with_timeout(
            stream,
            FileTransferMessage::Complete {
                transfer_id,
                file_index: file_index as u32,
                checksum,
            },
        )
        .await?;
        debug!(
            "File {}/{} sent: {} ({} bytes)",
            file_index + 1,
            file_count,
            prepared
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            offset
        );
    }

    send_with_timeout(stream, FileTransferMessage::Done { transfer_id }).await?;
    info!("File transfer {} complete", transfer_id);
    Ok(())
}

async fn send_with_timeout<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    message: FileTransferMessage,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    timeout(TRANSFER_IO_TIMEOUT, stream.send(message))
        .await
        .map_err(|_| eyre!("Timed out sending file-transfer message"))??;
    Ok(())
}

async fn receive_with_timeout<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
) -> Result<Option<FileTransferMessage>>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    timeout(TRANSFER_IO_TIMEOUT, stream.receive())
        .await
        .map_err(|_| eyre!("Timed out waiting for file-transfer response"))?
}

async fn cancel_with_error<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    transfer_id: u64,
    error: color_eyre::Report,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    send_with_timeout(stream, FileTransferMessage::Cancel { transfer_id })
        .await
        .wrap_err_with(|| format!("Failed to cancel transfer after: {error}"))?;
    Err(error)
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

    fn memory_stream_pair(
        capacity: usize,
    ) -> (
        FileTransferMessageStream<tokio::io::DuplexStream, tokio::io::DuplexStream>,
        FileTransferMessageStream<tokio::io::DuplexStream, tokio::io::DuplexStream>,
    ) {
        let (sender_writer, peer_reader) = tokio::io::duplex(capacity);
        let (peer_writer, sender_reader) = tokio::io::duplex(capacity);
        (
            FileTransferMessageStream::new(sender_writer, sender_reader),
            FileTransferMessageStream::new(peer_writer, peer_reader),
        )
    }

    async fn receive_offer(
        peer: &mut FileTransferMessageStream<tokio::io::DuplexStream, tokio::io::DuplexStream>,
    ) -> Result<u64> {
        let Some(FileTransferMessage::Offer { transfer_id, .. }) = peer.receive().await? else {
            return Err(eyre!("expected offer"));
        };
        Ok(transfer_id)
    }

    #[tokio::test]
    async fn injected_transfer_identifier_is_used_for_every_outgoing_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("note.txt");
        tokio::fs::write(&path, b"hello").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(4242);

        let send = send_files_over_stream(&mut sender, vec![path], &ids);
        let receive = async {
            let transfer_id = receive_offer(&mut peer).await?;
            assert_eq!(transfer_id, 4242);
            peer.send(FileTransferMessage::Accept { transfer_id })
                .await?;

            loop {
                let message = peer
                    .receive()
                    .await?
                    .ok_or_else(|| eyre!("sender closed before done"))?;
                match message {
                    FileTransferMessage::Chunk { transfer_id, .. }
                    | FileTransferMessage::Complete { transfer_id, .. } => {
                        assert_eq!(transfer_id, 4242);
                    }
                    FileTransferMessage::Done { transfer_id } => {
                        assert_eq!(transfer_id, 4242);
                        break;
                    }
                    other => return Err(eyre!("unexpected sender message: {other:?}")),
                }
            }
            Ok::<(), color_eyre::Report>(())
        };

        tokio::try_join!(send, receive).unwrap();
    }

    async fn peer_accepts_then_waits_for_cancel(
        peer: &mut FileTransferMessageStream<tokio::io::DuplexStream, tokio::io::DuplexStream>,
        transfer_id: u64,
    ) -> Result<()> {
        peer.send(FileTransferMessage::Accept { transfer_id })
            .await?;
        loop {
            match peer.receive().await? {
                Some(FileTransferMessage::Cancel { transfer_id: id }) if id == transfer_id => {
                    return Ok(());
                }
                Some(FileTransferMessage::Chunk { .. }) => {}
                other => return Err(eyre!("expected sender cancellation, got {other:?}")),
            }
        }
    }

    #[tokio::test]
    async fn peer_cancellation_stops_sender_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cancel.txt");
        tokio::fs::write(&path, b"cancel me").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(1);

        let send = send_files_over_stream(&mut sender, vec![path], &ids);
        let cancel = async {
            let transfer_id = receive_offer(&mut peer).await?;
            peer.send(FileTransferMessage::Cancel { transfer_id }).await
        };

        let (send_result, cancel_result) = tokio::join!(send, cancel);
        send_result.unwrap();
        cancel_result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn sender_times_out_waiting_for_offer_response() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("timeout.txt");
        tokio::fs::write(&path, b"timeout").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(2);

        let send = send_files_over_stream(&mut sender, vec![path], &ids);
        let stall = async {
            receive_offer(&mut peer).await.unwrap();
            tokio::time::advance(TRANSFER_IO_TIMEOUT).await;
        };
        let (result, ()) = tokio::join!(send, stall);

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Timed out waiting for file-transfer response"));
    }

    #[tokio::test]
    async fn sender_reports_mid_frame_disconnect() {
        use tokio::io::AsyncReadExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("disconnect.txt");
        tokio::fs::write(&path, vec![b'x'; 256]).await.unwrap();
        let (mut sender, peer) = memory_stream_pair(8);
        let (_, mut peer_reader) = peer.into_inner();
        let ids = FixedTransferId(3);

        let send = send_files_over_stream(&mut sender, vec![path], &ids);
        let disconnect = async move {
            let mut partial_frame = [0u8; 5];
            peer_reader.read_exact(&mut partial_frame).await.unwrap();
        };
        let (result, ()) = tokio::join!(send, disconnect);

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to write framed message"));
    }

    #[tokio::test]
    async fn sender_cancels_if_file_is_truncated_after_offer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("truncate.txt");
        tokio::fs::write(&path, b"original content").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(4);

        let send = send_files_over_stream(&mut sender, vec![path.clone()], &ids);
        let truncate = async {
            let transfer_id = receive_offer(&mut peer).await?;
            tokio::fs::write(&path, b"short").await?;
            peer_accepts_then_waits_for_cancel(&mut peer, transfer_id).await
        };
        let (result, peer_result) = tokio::join!(send, truncate);

        peer_result.unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("File truncated during transfer"));
    }

    #[tokio::test]
    async fn sender_cancels_if_file_grows_after_offer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("grow.txt");
        tokio::fs::write(&path, b"small").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(5);

        let send = send_files_over_stream(&mut sender, vec![path.clone()], &ids);
        let grow = async {
            let transfer_id = receive_offer(&mut peer).await?;
            tokio::fs::write(&path, b"now much larger").await?;
            peer_accepts_then_waits_for_cancel(&mut peer, transfer_id).await
        };
        let (result, peer_result) = tokio::join!(send, grow);

        peer_result.unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("File grew during transfer"));
    }

    #[tokio::test]
    async fn sender_cancels_if_file_mutates_after_offer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mutate.txt");
        tokio::fs::write(&path, b"before").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(6);

        let send = send_files_over_stream(&mut sender, vec![path.clone()], &ids);
        let mutate = async {
            let transfer_id = receive_offer(&mut peer).await?;
            tokio::fs::write(&path, b"after!").await?;
            peer_accepts_then_waits_for_cancel(&mut peer, transfer_id).await
        };
        let (result, peer_result) = tokio::join!(send, mutate);

        peer_result.unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("File mutated during transfer"));
    }

    #[tokio::test]
    async fn open_file_identity_survives_path_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("identity.txt");
        let moved = temp.path().join("identity.original.txt");
        tokio::fs::write(&path, b"original").await.unwrap();
        let (mut sender, mut peer) = memory_stream_pair(4096);
        let ids = FixedTransferId(7);

        let send = send_files_over_stream(&mut sender, vec![path.clone()], &ids);
        let replace = async {
            let transfer_id = receive_offer(&mut peer).await?;
            tokio::fs::rename(&path, &moved).await?;
            tokio::fs::write(&path, b"replaced").await?;
            peer.send(FileTransferMessage::Accept { transfer_id })
                .await?;

            let mut received = Vec::new();
            loop {
                match peer.receive().await? {
                    Some(FileTransferMessage::Chunk { data, .. }) => received.extend(data),
                    Some(FileTransferMessage::Complete { .. }) => {}
                    Some(FileTransferMessage::Done { .. }) => break,
                    other => return Err(eyre!("unexpected sender message: {other:?}")),
                }
            }
            Ok::<Vec<u8>, color_eyre::Report>(received)
        };
        let (result, received) = tokio::join!(send, replace);

        result.unwrap();
        assert_eq!(received.unwrap(), b"original");
    }
}
