use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result, WrapErr};
use ring::digest;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::stream::{FileTransferMessage, FileTransferMessageStream};
use crate::net::protocol::FileInfo;

const TRANSFER_IO_TIMEOUT: Duration = Duration::from_secs(30);

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
    let staging_dir = tempfile::Builder::new().prefix("nexdesk-").tempdir()?;
    let output_paths = receive_files_into(stream, staging_dir.path()).await?;
    if !output_paths.is_empty() {
        let _staging_path = staging_dir.keep();
    }
    Ok(output_paths)
}

pub(crate) async fn receive_files_into<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    staging_dir: &Path,
) -> Result<Vec<PathBuf>>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let (transfer_id, files, total_size) = match receive_with_timeout(stream).await? {
        Some(FileTransferMessage::Offer {
            transfer_id,
            files,
            total_size,
        }) => (transfer_id, files, total_size),
        other => return Err(eyre!("Expected FileTransferOffer, got: {:?}", other)),
    };

    if let Err(error) = validate_offer(&files, total_size) {
        return reject_transfer(stream, transfer_id, error).await;
    }

    info!(
        "File transfer offer {}: {} file(s), {} bytes",
        transfer_id,
        files.len(),
        total_size
    );
    send_with_timeout(stream, FileTransferMessage::Accept { transfer_id }).await?;

    let output_paths: Vec<PathBuf> = files
        .iter()
        .map(|file_info| staging_dir.join(&file_info.name))
        .collect();
    let mut open_files: Vec<Option<tokio::fs::File>> = (0..files.len()).map(|_| None).collect();
    let mut hashers: Vec<Option<digest::Context>> = files
        .iter()
        .map(|_| Some(digest::Context::new(&digest::SHA256)))
        .collect();
    let mut offsets = vec![0u64; files.len()];
    let mut completed = vec![false; files.len()];

    loop {
        match receive_with_timeout(stream).await? {
            Some(FileTransferMessage::Chunk {
                transfer_id: tid,
                file_index,
                offset,
                data,
            }) => {
                let idx = file_index as usize;
                let validation = if tid != transfer_id {
                    Err(eyre!("Chunk transfer ID does not match active transfer"))
                } else if idx >= files.len() {
                    Err(eyre!("Invalid file index: {file_index}"))
                } else if completed[idx] {
                    Err(eyre!("Chunk received after file completion: {file_index}"))
                } else if data.is_empty() {
                    Err(eyre!("Empty file chunk: {file_index}"))
                } else if offset != offsets[idx] {
                    Err(eyre!(
                        "Invalid chunk offset for file {file_index}: expected {}, got {offset}",
                        offsets[idx]
                    ))
                } else if offset
                    .checked_add(data.len() as u64)
                    .is_none_or(|end| end > files[idx].size)
                {
                    Err(eyre!("Chunk exceeds offered file size: {file_index}"))
                } else {
                    Ok(())
                };
                if let Err(error) = validation {
                    return reject_transfer(stream, transfer_id, error).await;
                }

                if open_files[idx].is_none() {
                    match create_output_file(&output_paths[idx]).await {
                        Ok(file) => open_files[idx] = Some(file),
                        Err(error) => return reject_transfer(stream, transfer_id, error).await,
                    }
                }
                if let Some(file) = &mut open_files[idx] {
                    if let Err(error) = file.write_all(&data).await {
                        return reject_transfer(
                            stream,
                            transfer_id,
                            eyre!("Failed to write received file data: {error}"),
                        )
                        .await;
                    }
                }
                hashers[idx]
                    .as_mut()
                    .expect("validated active file")
                    .update(&data);
                offsets[idx] += data.len() as u64;
            }
            Some(FileTransferMessage::Complete {
                transfer_id: tid,
                file_index,
                checksum,
            }) => {
                let idx = file_index as usize;
                let validation = if tid != transfer_id {
                    Err(eyre!(
                        "Completion transfer ID does not match active transfer"
                    ))
                } else if idx >= files.len() {
                    Err(eyre!("Invalid completion file index: {file_index}"))
                } else if completed[idx] {
                    Err(eyre!("Duplicate file completion: {file_index}"))
                } else if offsets[idx] != files[idx].size {
                    Err(eyre!(
                        "File completed at wrong size: expected {}, got {}",
                        files[idx].size,
                        offsets[idx]
                    ))
                } else {
                    Ok(())
                };
                if let Err(error) = validation {
                    return reject_transfer(stream, transfer_id, error).await;
                }

                if open_files[idx].is_none() {
                    match create_output_file(&output_paths[idx]).await {
                        Ok(file) => open_files[idx] = Some(file),
                        Err(error) => return reject_transfer(stream, transfer_id, error).await,
                    }
                }
                if let Some(mut file) = open_files[idx].take() {
                    if let Err(error) = file.flush().await {
                        return reject_transfer(
                            stream,
                            transfer_id,
                            eyre!("Failed to flush received file: {error}"),
                        )
                        .await;
                    }
                }

                let computed: String = hashers[idx]
                    .take()
                    .expect("validated active checksum")
                    .finish()
                    .as_ref()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                if computed != checksum {
                    return reject_transfer(
                        stream,
                        transfer_id,
                        eyre!(
                            "Checksum mismatch for file {} ({}): expected {}, got {}",
                            idx,
                            files[idx].name,
                            checksum,
                            computed
                        ),
                    )
                    .await;
                }
                completed[idx] = true;
                debug!("Checksum verified for file {} ({})", idx, files[idx].name);
            }
            Some(FileTransferMessage::Done { transfer_id: tid }) => {
                if tid != transfer_id {
                    return reject_transfer(
                        stream,
                        transfer_id,
                        eyre!("Done transfer ID does not match active transfer"),
                    )
                    .await;
                }
                if !completed.iter().all(|done| *done) {
                    return reject_transfer(
                        stream,
                        transfer_id,
                        eyre!("Transfer finished before every file completed"),
                    )
                    .await;
                }
                info!(
                    "File transfer {} complete ({} files received)",
                    transfer_id,
                    files.len()
                );
                break;
            }
            Some(FileTransferMessage::Cancel { transfer_id: tid }) if tid == transfer_id => {
                warn!("File transfer {} cancelled by sender", transfer_id);
                return Ok(vec![]);
            }
            Some(other) => {
                return reject_transfer(
                    stream,
                    transfer_id,
                    eyre!("Unexpected message during file transfer: {other:?}"),
                )
                .await;
            }
            None => {
                warn!("Stream closed during file transfer {}", transfer_id);
                return Ok(vec![]);
            }
        }
    }

    Ok(output_paths)
}

fn validate_offer(files: &[FileInfo], total_size: u64) -> Result<()> {
    if files.is_empty() {
        return Err(eyre!("File transfer offer contains no files"));
    }

    let mut names = HashSet::new();
    let mut computed_total = 0u64;
    for file in files {
        let path = Path::new(&file.name);
        let mut components = path.components();
        let valid_name = matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none()
            && !file.name.is_empty();
        if !valid_name {
            return Err(eyre!("Unsafe file name in offer: {:?}", file.name));
        }
        if !names.insert(file.name.as_str()) {
            return Err(eyre!("Duplicate file name in offer: {:?}", file.name));
        }
        computed_total = computed_total
            .checked_add(file.size)
            .ok_or_else(|| eyre!("File transfer total size overflow"))?;
    }
    if computed_total != total_size {
        return Err(eyre!(
            "File transfer total size mismatch: expected {computed_total}, got {total_size}"
        ));
    }
    Ok(())
}

async fn create_output_file(path: &Path) -> Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .wrap_err_with(|| format!("Failed to create transfer destination {}", path.display()))
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
        .map_err(|_| eyre!("Timed out sending file-transfer response"))??;
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
        .map_err(|_| eyre!("Timed out waiting for file-transfer message"))?
}

async fn reject_transfer<W, R>(
    stream: &mut FileTransferMessageStream<W, R>,
    transfer_id: u64,
    error: color_eyre::Report,
) -> Result<Vec<PathBuf>>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    send_with_timeout(stream, FileTransferMessage::Cancel { transfer_id })
        .await
        .wrap_err_with(|| format!("Failed to reject transfer after: {error}"))?;
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    type MemoryStream = FileTransferMessageStream<tokio::io::DuplexStream, tokio::io::DuplexStream>;

    fn memory_stream_pair() -> (MemoryStream, MemoryStream) {
        let (receiver_writer, peer_reader) = tokio::io::duplex(4096);
        let (peer_writer, receiver_reader) = tokio::io::duplex(4096);
        (
            FileTransferMessageStream::new(receiver_writer, receiver_reader),
            FileTransferMessageStream::new(peer_writer, peer_reader),
        )
    }

    async fn send_offer(
        peer: &mut MemoryStream,
        files: Vec<FileInfo>,
        total_size: u64,
    ) -> Result<u64> {
        let transfer_id = 42;
        peer.send(FileTransferMessage::Offer {
            transfer_id,
            files,
            total_size,
        })
        .await?;
        assert!(matches!(
            peer.receive().await?,
            Some(FileTransferMessage::Accept { transfer_id: 42 })
        ));
        Ok(transfer_id)
    }

    async fn expect_cancel(peer: &mut MemoryStream) -> Result<()> {
        assert!(matches!(
            peer.receive().await?,
            Some(FileTransferMessage::Cancel { transfer_id: 42 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn malformed_offers_are_rejected_before_acceptance() {
        let cases = [
            (
                vec![FileInfo {
                    name: "../escape".to_string(),
                    size: 1,
                }],
                1,
                "Unsafe file name",
            ),
            (
                vec![
                    FileInfo {
                        name: "same.txt".to_string(),
                        size: 1,
                    },
                    FileInfo {
                        name: "same.txt".to_string(),
                        size: 1,
                    },
                ],
                2,
                "Duplicate file name",
            ),
            (
                vec![FileInfo {
                    name: "size.txt".to_string(),
                    size: 1,
                }],
                2,
                "total size mismatch",
            ),
        ];

        for (files, total_size, expected_error) in cases {
            let root = tempfile::tempdir().unwrap();
            let (mut receiver, mut peer) = memory_stream_pair();
            let receive = receive_files_into(&mut receiver, root.path());
            let send = async {
                peer.send(FileTransferMessage::Offer {
                    transfer_id: 42,
                    files,
                    total_size,
                })
                .await?;
                expect_cancel(&mut peer).await
            };
            let (result, peer_result) = tokio::join!(receive, send);
            peer_result.unwrap();
            assert!(result.unwrap_err().to_string().contains(expected_error));
        }
    }

    #[tokio::test]
    async fn invalid_and_duplicate_offsets_are_rejected() {
        for chunks in [
            vec![(1, b"x".to_vec())],
            vec![(0, b"x".to_vec()), (0, b"y".to_vec())],
        ] {
            let root = tempfile::tempdir().unwrap();
            let (mut receiver, mut peer) = memory_stream_pair();
            let receive = receive_files_into(&mut receiver, root.path());
            let send = async {
                let transfer_id = send_offer(
                    &mut peer,
                    vec![FileInfo {
                        name: "offset.txt".to_string(),
                        size: 3,
                    }],
                    3,
                )
                .await?;
                for (offset, data) in chunks {
                    peer.send(FileTransferMessage::Chunk {
                        transfer_id,
                        file_index: 0,
                        offset,
                        data,
                    })
                    .await?;
                }
                expect_cancel(&mut peer).await
            };
            let (result, peer_result) = tokio::join!(receive, send);
            peer_result.unwrap();
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Invalid chunk offset"));
        }
    }

    #[tokio::test]
    async fn checksum_mismatch_rejects_the_transfer() {
        let root = tempfile::tempdir().unwrap();
        let (mut receiver, mut peer) = memory_stream_pair();
        let receive = receive_files_into(&mut receiver, root.path());
        let send = async {
            let transfer_id = send_offer(
                &mut peer,
                vec![FileInfo {
                    name: "checksum.txt".to_string(),
                    size: 3,
                }],
                3,
            )
            .await?;
            peer.send(FileTransferMessage::Chunk {
                transfer_id,
                file_index: 0,
                offset: 0,
                data: b"abc".to_vec(),
            })
            .await?;
            peer.send(FileTransferMessage::Complete {
                transfer_id,
                file_index: 0,
                checksum: "not-a-checksum".to_string(),
            })
            .await?;
            expect_cancel(&mut peer).await
        };
        let (result, peer_result) = tokio::join!(receive, send);

        peer_result.unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Checksum mismatch"));
    }

    async fn assert_destination_failure(root: &Path, expected_error: &str) {
        let (mut receiver, mut peer) = memory_stream_pair();
        let receive = receive_files_into(&mut receiver, root);
        let send = async {
            let transfer_id = send_offer(
                &mut peer,
                vec![FileInfo {
                    name: "blocked.txt".to_string(),
                    size: 1,
                }],
                1,
            )
            .await?;
            peer.send(FileTransferMessage::Chunk {
                transfer_id,
                file_index: 0,
                offset: 0,
                data: vec![b'x'],
            })
            .await?;
            expect_cancel(&mut peer).await
        };
        let (result, peer_result) = tokio::join!(receive, send);
        peer_result.unwrap();
        assert!(result.unwrap_err().to_string().contains(expected_error));
    }

    #[tokio::test]
    async fn existing_destination_collision_is_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("blocked.txt");
        tokio::fs::write(&existing, b"keep me").await.unwrap();

        assert_destination_failure(root.path(), "Failed to create transfer destination").await;
        assert_eq!(tokio::fs::read(existing).await.unwrap(), b"keep me");
    }

    #[tokio::test]
    async fn destination_storage_failure_cancels_the_transfer() {
        let root = tempfile::tempdir().unwrap();
        let missing_directory = root.path().join("missing");
        assert_destination_failure(&missing_directory, "Failed to create transfer destination")
            .await;
    }

    #[tokio::test]
    async fn sender_cancellation_discards_staged_outputs() {
        let (mut receiver, mut peer) = memory_stream_pair();
        let receive = receive_files_over_stream(&mut receiver);
        let send = async {
            let transfer_id = send_offer(
                &mut peer,
                vec![FileInfo {
                    name: "cancel.txt".to_string(),
                    size: 1,
                }],
                1,
            )
            .await?;
            peer.send(FileTransferMessage::Chunk {
                transfer_id,
                file_index: 0,
                offset: 0,
                data: vec![b'x'],
            })
            .await?;
            peer.send(FileTransferMessage::Cancel { transfer_id }).await
        };
        let (result, peer_result) = tokio::join!(receive, send);

        peer_result.unwrap();
        assert!(result.unwrap().is_empty());
    }

    struct EndToEndTransferId;

    impl crate::filetransfer::id::TransferIdSource for EndToEndTransferId {
        fn next_transfer_id(&self) -> u64 {
            9001
        }
    }

    #[tokio::test]
    async fn sender_and_receiver_transfer_real_files_over_memory_streams() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let small_path = source.path().join("small.txt");
        let large_path = source.path().join("large.bin");
        let empty_path = source.path().join("empty.txt");
        let small = b"hello from nexdesk".to_vec();
        let large: Vec<u8> = (0..super::super::CHUNK_SIZE + 17)
            .map(|index| (index % 251) as u8)
            .collect();
        tokio::fs::write(&small_path, &small).await.unwrap();
        tokio::fs::write(&large_path, &large).await.unwrap();
        tokio::fs::write(&empty_path, []).await.unwrap();

        let (mut receiver, mut sender) = memory_stream_pair();
        let send = crate::filetransfer::send::send_files_over_stream(
            &mut sender,
            vec![small_path, large_path, empty_path],
            &EndToEndTransferId,
        );
        let receive = receive_files_into(&mut receiver, destination.path());
        let ((), output_paths) = tokio::try_join!(send, receive).unwrap();

        assert_eq!(output_paths.len(), 3);
        assert_eq!(tokio::fs::read(&output_paths[0]).await.unwrap(), small);
        assert_eq!(tokio::fs::read(&output_paths[1]).await.unwrap(), large);
        assert!(tokio::fs::read(&output_paths[2]).await.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_times_out_waiting_for_transfer_data() {
        let root = tempfile::tempdir().unwrap();
        let (mut receiver, mut peer) = memory_stream_pair();
        let receive = receive_files_into(&mut receiver, root.path());
        let stall = async {
            send_offer(
                &mut peer,
                vec![FileInfo {
                    name: "timeout.txt".to_string(),
                    size: 1,
                }],
                1,
            )
            .await
            .unwrap();
            tokio::time::advance(TRANSFER_IO_TIMEOUT).await;
        };
        let (result, ()) = tokio::join!(receive, stall);

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Timed out waiting for file-transfer message"));
    }
}
