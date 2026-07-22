use color_eyre::eyre::{eyre, Result};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::net::framing;
use crate::net::protocol::{FileInfo, Message};

/// A semantic message carried by a dedicated file-transfer stream.
///
/// Keeping this type narrower than the complete wire protocol prevents file
/// transfer orchestration from depending on unrelated control/input messages.
#[derive(Debug, Clone)]
pub enum FileTransferMessage {
    Offer {
        transfer_id: u64,
        files: Vec<FileInfo>,
        total_size: u64,
    },
    Accept {
        transfer_id: u64,
    },
    Chunk {
        transfer_id: u64,
        file_index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Complete {
        transfer_id: u64,
        file_index: u32,
        checksum: String,
    },
    Done {
        transfer_id: u64,
    },
    Cancel {
        transfer_id: u64,
    },
}

impl From<FileTransferMessage> for Message {
    fn from(message: FileTransferMessage) -> Self {
        match message {
            FileTransferMessage::Offer {
                transfer_id,
                files,
                total_size,
            } => Self::FileTransferOffer {
                transfer_id,
                files,
                total_size,
            },
            FileTransferMessage::Accept { transfer_id } => Self::FileTransferAccept { transfer_id },
            FileTransferMessage::Chunk {
                transfer_id,
                file_index,
                offset,
                data,
            } => Self::FileTransferChunk {
                transfer_id,
                file_index,
                offset,
                data,
            },
            FileTransferMessage::Complete {
                transfer_id,
                file_index,
                checksum,
            } => Self::FileTransferComplete {
                transfer_id,
                file_index,
                checksum,
            },
            FileTransferMessage::Done { transfer_id } => Self::FileTransferDone { transfer_id },
            FileTransferMessage::Cancel { transfer_id } => Self::FileTransferCancel { transfer_id },
        }
    }
}

impl TryFrom<Message> for FileTransferMessage {
    type Error = color_eyre::Report;

    fn try_from(message: Message) -> Result<Self> {
        match message {
            Message::FileTransferOffer {
                transfer_id,
                files,
                total_size,
            } => Ok(Self::Offer {
                transfer_id,
                files,
                total_size,
            }),
            Message::FileTransferAccept { transfer_id } => Ok(Self::Accept { transfer_id }),
            Message::FileTransferChunk {
                transfer_id,
                file_index,
                offset,
                data,
            } => Ok(Self::Chunk {
                transfer_id,
                file_index,
                offset,
                data,
            }),
            Message::FileTransferComplete {
                transfer_id,
                file_index,
                checksum,
            } => Ok(Self::Complete {
                transfer_id,
                file_index,
                checksum,
            }),
            Message::FileTransferDone { transfer_id } => Ok(Self::Done { transfer_id }),
            Message::FileTransferCancel { transfer_id } => Ok(Self::Cancel { transfer_id }),
            other => Err(eyre!(
                "Unexpected message on file-transfer stream: {}",
                crate::net::protocol::message_summary(&other)
            )),
        }
    }
}

/// A framed, bidirectional stream of semantic file-transfer messages.
///
/// The reader and writer are generic asynchronous I/O halves, so orchestration
/// can use in-memory streams in tests while production supplies Quinn streams.
pub struct FileTransferMessageStream<W, R> {
    writer: W,
    reader: R,
}

impl<W, R> FileTransferMessageStream<W, R> {
    pub fn new(writer: W, reader: R) -> Self {
        Self { writer, reader }
    }

    pub fn into_inner(self) -> (W, R) {
        (self.writer, self.reader)
    }
}

impl<W, R> FileTransferMessageStream<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    pub async fn send(&mut self, message: FileTransferMessage) -> Result<()> {
        framing::send_message(&mut self.writer, &message.into()).await
    }

    pub async fn receive(&mut self) -> Result<Option<FileTransferMessage>> {
        framing::recv_message(&mut self.reader)
            .await?
            .map(FileTransferMessage::try_from)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn typed_file_transfer_stream_round_trips_over_memory_io() {
        let (writer, peer_reader) = tokio::io::duplex(1024);
        let (mut peer_writer, reader) = tokio::io::duplex(1024);
        let mut stream = FileTransferMessageStream::new(writer, reader);

        stream
            .send(FileTransferMessage::Offer {
                transfer_id: 42,
                files: vec![FileInfo {
                    name: "note.txt".to_string(),
                    size: 3,
                }],
                total_size: 3,
            })
            .await
            .unwrap();

        let mut peer_stream = FileTransferMessageStream::new(&mut peer_writer, peer_reader);
        assert!(matches!(
            peer_stream.receive().await.unwrap(),
            Some(FileTransferMessage::Offer {
                transfer_id: 42,
                total_size: 3,
                ..
            })
        ));

        peer_stream
            .send(FileTransferMessage::Accept { transfer_id: 42 })
            .await
            .unwrap();
        assert!(matches!(
            stream.receive().await.unwrap(),
            Some(FileTransferMessage::Accept { transfer_id: 42 })
        ));
    }

    #[tokio::test]
    async fn typed_file_transfer_stream_rejects_cross_channel_messages() {
        let (mut writer, reader) = tokio::io::duplex(128);
        framing::send_message(&mut writer, &Message::Heartbeat { timestamp: 1 })
            .await
            .unwrap();
        writer.shutdown().await.unwrap();

        let mut stream = FileTransferMessageStream::new(tokio::io::sink(), reader);
        let error = stream.receive().await.unwrap_err().to_string();
        assert!(error.contains("Unexpected message on file-transfer stream: Heartbeat"));
    }
}
