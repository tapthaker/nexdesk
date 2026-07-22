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
}
