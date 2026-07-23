//! Narrow entry points used by out-of-package fuzz targets.
//!
//! Keeping the harness adapters here lets fuzz crates exercise private protocol
//! implementation without widening the normal public API of those modules.

use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

struct ChunkedReader<'a> {
    input: &'a [u8],
    chunks: &'a [u8],
    position: usize,
    chunk_index: usize,
}

impl AsyncRead for ChunkedReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.position == self.input.len() {
            return Poll::Ready(Ok(()));
        }
        let requested = if self.chunks.is_empty() {
            1
        } else {
            self.chunks[self.chunk_index % self.chunks.len()].max(1) as usize
        };
        self.chunk_index += 1;
        let count = requested
            .min(buffer.remaining())
            .min(self.input.len() - self.position);
        buffer.put_slice(&self.input[self.position..self.position + count]);
        self.position += count;
        Poll::Ready(Ok(()))
    }
}

fn fuzz_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create fuzz runtime")
    })
}

/// Decode arbitrary framed protocol input and verify every successful result
/// satisfies the decoder's framing and semantic-validation contracts.
pub fn exercise_protocol_decode(input: &[u8]) {
    if let Ok(Some((message, consumed))) = crate::net::protocol::decode(input) {
        assert!(consumed <= input.len());
        crate::net::protocol::validate_message(&message)
            .expect("decoder returned a semantically invalid message");
        crate::net::protocol::encode(&message)
            .expect("decoder returned a message that cannot be encoded");
    }
}

/// Read arbitrary frame bytes through adversarial async chunk boundaries and
/// require the stream decoder to agree with the slice decoder.
pub fn exercise_framed_chunks(input: &[u8]) {
    let (chunks, frame) = match input.split_first() {
        Some((&chunk_count, remaining)) => {
            let chunk_count = usize::from(chunk_count).min(remaining.len()).min(32);
            remaining.split_at(chunk_count)
        }
        None => (&[][..], &[][..]),
    };
    let mut reader = ChunkedReader {
        input: frame,
        chunks,
        position: 0,
        chunk_index: 0,
    };
    let streamed = fuzz_runtime().block_on(crate::net::framing::recv_message(&mut reader));
    let sliced = crate::net::protocol::decode(frame);

    match sliced {
        Ok(Some((expected, _))) => {
            let actual = streamed
                .expect("slice decoder accepted a frame rejected by stream decoder")
                .expect("stream decoder reported clean EOF for a complete frame");
            assert_eq!(
                crate::net::protocol::encode(&actual).expect("re-encode streamed message"),
                crate::net::protocol::encode(&expected).expect("re-encode sliced message")
            );
        }
        Ok(None) if frame.is_empty() => {
            assert!(matches!(streamed, Ok(None)));
        }
        Ok(None) => {
            assert!(streamed.is_err(), "partial frame was treated as clean EOF");
        }
        Err(_) => {
            assert!(
                streamed.is_err(),
                "invalid slice frame passed stream decoding"
            );
        }
    }
}

/// Drive the production receiver with a bounded, byte-derived sequence of
/// semantic file-transfer messages over in-memory framed streams.
pub fn exercise_file_transfer_sequence(input: &[u8]) {
    use crate::filetransfer::stream::{FileTransferMessage, FileTransferMessageStream};
    use crate::net::protocol::FileInfo;

    const TRANSFER_ID: u64 = 7;
    let bytes = &input[..input.len().min(256)];
    let size = u64::from(bytes.first().copied().unwrap_or(0) % 16);
    let name = match bytes.get(1).copied().unwrap_or(0) % 4 {
        0 => "fuzz.bin",
        1 => "../escape",
        2 => "",
        _ => "nested/file",
    };
    let total_size = if bytes.get(2).copied().unwrap_or(0) & 1 == 0 {
        size
    } else {
        size.saturating_add(1)
    };
    let mut messages = vec![FileTransferMessage::Offer {
        transfer_id: TRANSFER_ID,
        files: vec![FileInfo {
            name: name.to_string(),
            size,
        }],
        total_size,
    }];

    for chunk in bytes.get(3..).unwrap_or_default().chunks(5).take(48) {
        let byte = |index: usize| chunk.get(index).copied().unwrap_or(0);
        let transfer_id = if byte(1) & 1 == 0 {
            TRANSFER_ID
        } else {
            TRANSFER_ID + 1
        };
        match byte(0) % 6 {
            0 => messages.push(FileTransferMessage::Chunk {
                transfer_id,
                file_index: u32::from(byte(2) % 3),
                offset: u64::from(byte(3) % 20),
                data: if byte(4) & 1 == 0 {
                    vec![byte(4)]
                } else {
                    Vec::new()
                },
            }),
            1 => messages.push(FileTransferMessage::Complete {
                transfer_id,
                file_index: u32::from(byte(2) % 3),
                checksum: if byte(3) & 1 == 0 {
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()
                } else {
                    format!("{:02x}", byte(4))
                },
            }),
            2 => messages.push(FileTransferMessage::Done { transfer_id }),
            3 => messages.push(FileTransferMessage::Cancel { transfer_id }),
            4 => messages.push(FileTransferMessage::Offer {
                transfer_id,
                files: Vec::new(),
                total_size: 0,
            }),
            _ => messages.push(FileTransferMessage::Accept { transfer_id }),
        }
    }
    messages.push(FileTransferMessage::Cancel {
        transfer_id: TRANSFER_ID,
    });

    fuzz_runtime().block_on(async move {
        let root = tempfile::tempdir().expect("create fuzz transfer root");
        let (receiver_writer, peer_reader) = tokio::io::duplex(64 * 1024);
        let (peer_writer, receiver_reader) = tokio::io::duplex(64 * 1024);
        let mut receiver = FileTransferMessageStream::new(receiver_writer, receiver_reader);
        let mut peer = FileTransferMessageStream::new(peer_writer, peer_reader);

        let receive = crate::filetransfer::recv::receive_files_into(&mut receiver, root.path());
        let send = async {
            for message in messages {
                if peer.send(message).await.is_err() {
                    break;
                }
            }
        };
        let (result, ()) = tokio::join!(receive, send);
        if let Ok(paths) = result {
            for path in paths {
                assert!(path.starts_with(root.path()));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_chunk_harness_accepts_complete_partial_and_empty_inputs() {
        let frame = crate::net::protocol::encode(&crate::net::protocol::Message::Heartbeat {
            timestamp: 42,
        })
        .unwrap();
        let mut chunked = vec![3, 1, 2, 7];
        chunked.extend_from_slice(&frame);

        exercise_framed_chunks(&chunked);
        exercise_framed_chunks(&[0]);
        exercise_framed_chunks(&[0, 0, 0]);
    }

    #[test]
    fn file_transfer_sequence_harness_terminates_for_valid_and_invalid_flows() {
        exercise_file_transfer_sequence(&[]);
        exercise_file_transfer_sequence(&[0, 0, 0, 3, 0, 0, 0, 0]);
        exercise_file_transfer_sequence(&[8, 1, 1, 0, 1, 2, 3, 4]);
    }
}
