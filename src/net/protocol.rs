use serde::{Deserialize, Serialize};

/// Screen layout information for a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenLayout {
    pub width: u32,
    pub height: u32,
}

/// Direction for screen switching.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Clipboard content types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
}

/// All protocol messages sent over QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    // Phase 2: Control
    Heartbeat { timestamp: u64 },
    HeartbeatAck { timestamp: u64 },
    Hello {
        version: u32,
        hostname: String,
        screen: ScreenLayout,
    },
    HelloAck { accepted: bool },

    // Phase 3: Mouse
    MouseMove { x: i32, y: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseScroll { dx: i32, dy: i32 },
    SwitchScreen { direction: Direction },

    // Phase 4a: Keyboard
    KeyEvent {
        keycode: u32,
        pressed: bool,
        modifiers: u16,
    },

    // Phase 4b: Clipboard
    ClipboardUpdate { content: ClipboardContent },

    // Phase 4c: File transfer (stubs)
    FileTransferStart { name: String, size: u64 },
    FileTransferChunk { data: Vec<u8> },
    FileTransferEnd { checksum: String },
}

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Encode a message to bytes.
pub fn encode(msg: &Message) -> color_eyre::eyre::Result<Vec<u8>> {
    let bytes = bincode::serialize(msg)?;
    // Prefix with 4-byte length
    let len = (bytes.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + bytes.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&bytes);
    Ok(buf)
}

/// Decode a message from a length-prefixed byte stream.
/// Returns the message and the number of bytes consumed.
pub fn decode(buf: &[u8]) -> color_eyre::eyre::Result<Option<(Message, usize)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let msg: Message = bincode::deserialize(&buf[4..4 + len])?;
    Ok(Some((msg, 4 + len)))
}
