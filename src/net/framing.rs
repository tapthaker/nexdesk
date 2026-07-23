use color_eyre::eyre::{eyre, Result, WrapErr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::net::protocol::{self, Message};

/// Write one validated length-prefixed protocol message.
pub async fn send_message<W>(writer: &mut W, message: &Message) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let frame = protocol::encode(message)?;
    writer
        .write_all(&frame)
        .await
        .wrap_err("Failed to write framed message")?;
    Ok(())
}

/// Read one validated length-prefixed protocol message.
///
/// End-of-stream before a new frame returns `None`; closure after any frame
/// bytes have arrived is an error.
pub async fn recv_message<R>(reader: &mut R) -> Result<Option<Message>>
where
    R: AsyncRead + Unpin,
{
    let mut length = [0u8; 4];
    if reader
        .read(&mut length[..1])
        .await
        .wrap_err("Failed to read message frame")?
        == 0
    {
        return Ok(None);
    }
    reader
        .read_exact(&mut length[1..])
        .await
        .wrap_err("Connection closed mid-message length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length > protocol::MAX_MESSAGE_SIZE {
        return Err(eyre!("Message too large: {} bytes", length));
    }

    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .wrap_err("Connection closed mid-message body")?;
    let message: Message = bincode::deserialize(&body)?;
    protocol::validate_message(&message)?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_codec_round_trips_over_generic_async_streams() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let message = Message::Heartbeat { timestamp: 42 };

        send_message(&mut writer, &message).await.unwrap();
        assert!(matches!(
            recv_message(&mut reader).await.unwrap(),
            Some(Message::Heartbeat { timestamp: 42 })
        ));
    }

    #[tokio::test]
    async fn invalid_outbound_message_is_rejected_before_writing_bytes() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        let invalid = Message::KeyEvent {
            keycode: protocol::MAX_KEYCODE + 1,
            pressed: true,
            modifiers: 0,
        };

        let error = send_message(&mut writer, &invalid).await.unwrap_err();
        assert!(error.to_string().contains("Invalid keycode"));

        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn clean_close_and_partial_frame_close_are_distinct() {
        let (writer, mut reader) = tokio::io::duplex(16);
        drop(writer);
        assert!(recv_message(&mut reader).await.unwrap().is_none());

        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(&[0, 0]).await.unwrap();
        drop(writer);
        assert!(recv_message(&mut reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("mid-message length"));
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_reading_a_body() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        let length = (protocol::MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes();
        writer.write_all(&length).await.unwrap();

        assert!(recv_message(&mut reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("Message too large"));
    }
}
