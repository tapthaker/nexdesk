use serde::{Deserialize, Serialize};

/// Screen layout information for a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenLayout {
    pub width: u32,
    pub height: u32,
}

/// Direction for screen switching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Scroll gesture phase (maps to macOS CGScrollEventScrollPhase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollPhase {
    /// Discrete scroll (mouse wheel) — no phase tracking.
    None,
    /// Continuous gesture started.
    Began,
    /// Continuous gesture ongoing.
    Changed,
    /// Continuous finger gesture ended.
    Ended,
    /// Inertial scrolling started after the fingers lifted.
    MomentumBegan,
    /// Inertial scrolling is continuing.
    MomentumChanged,
    /// Inertial scrolling ended.
    MomentumEnded,
}

/// Clipboard content types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
}

/// File metadata for file transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
}

/// All protocol messages sent over QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    // Phase 2: Control
    Heartbeat {
        timestamp: u64,
    },
    HeartbeatAck {
        timestamp: u64,
    },
    Hello {
        version: u32,
        hostname: String,
        screen: ScreenLayout,
        fingerprint: String,
        build_version: Option<String>,
    },
    HelloAck {
        accepted: bool,
        otp: Option<String>,
        screen: Option<ScreenLayout>,
        build_version: Option<String>,
    },
    PairingResult {
        success: bool,
    },

    // Phase 3: Mouse
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    MouseScroll {
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
    },
    SwitchScreen {
        direction: Direction,
    },

    // Phase 4a: Keyboard
    KeyEvent {
        keycode: u32,
        pressed: bool,
        modifiers: u16,
    },

    // Screen resize notification
    ScreenResize {
        screen: ScreenLayout,
    },

    // Phase 4b: Clipboard
    ClipboardUpdate {
        content: ClipboardContent,
    },

    // Phase 4c: File transfer
    FileTransferOffer {
        transfer_id: u64,
        files: Vec<FileInfo>,
        total_size: u64,
    },
    FileTransferAccept {
        transfer_id: u64,
    },
    FileTransferChunk {
        transfer_id: u64,
        file_index: u32,
        offset: u64,
        data: Vec<u8>,
    },
    FileTransferComplete {
        transfer_id: u64,
        file_index: u32,
        checksum: String,
    },
    FileTransferDone {
        transfer_id: u64,
    },
    FileTransferCancel {
        transfer_id: u64,
    },

    // Peer user activity: ask the OS to treat the display/session as active.
    WakeDisplay,

    // Server-side session lock or policy release: return control to the server.
    ReleaseControl,
}

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 6;

/// Build version string burned in at compile time (e.g. "v0.1.2" or "v0.1.2-3-gabcdef").
pub const BUILD_VERSION: &str = env!("NEXDESK_VERSION");
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
pub const MAX_SCROLL_DELTA: f64 = 10_000.0;
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_KEYCODE: u32 = 255;
pub const OTP_DIGITS: usize = 6;

pub fn local_build_version() -> String {
    BUILD_VERSION.to_string()
}

pub fn message_summary(msg: &Message) -> String {
    match msg {
        Message::Heartbeat { .. } => "Heartbeat".into(),
        Message::HeartbeatAck { .. } => "HeartbeatAck".into(),
        Message::Hello { .. } => "Hello".into(),
        Message::HelloAck { .. } => "HelloAck".into(),
        Message::PairingResult { .. } => "PairingResult".into(),
        Message::MouseMove { .. } => "MouseMove".into(),
        Message::MouseButton { .. } => "MouseButton".into(),
        Message::MouseScroll { .. } => "MouseScroll".into(),
        Message::SwitchScreen { .. } => "SwitchScreen".into(),
        Message::KeyEvent { .. } => "KeyEvent".into(),
        Message::ScreenResize { .. } => "ScreenResize".into(),
        Message::ClipboardUpdate { .. } => "ClipboardUpdate".into(),
        Message::FileTransferOffer { .. } => "FileTransferOffer".into(),
        Message::FileTransferAccept { .. } => "FileTransferAccept".into(),
        Message::FileTransferChunk { .. } => "FileTransferChunk".into(),
        Message::FileTransferComplete { .. } => "FileTransferComplete".into(),
        Message::FileTransferDone { .. } => "FileTransferDone".into(),
        Message::FileTransferCancel { .. } => "FileTransferCancel".into(),
        Message::WakeDisplay => "WakeDisplay".into(),
        Message::ReleaseControl => "ReleaseControl".into(),
    }
}

pub fn validate_message(msg: &Message) -> color_eyre::eyre::Result<()> {
    match msg {
        Message::MouseScroll { dx, dy, .. }
            if !dx.is_finite()
                || !dy.is_finite()
                || dx.abs() > MAX_SCROLL_DELTA
                || dy.abs() > MAX_SCROLL_DELTA =>
        {
            Err(color_eyre::eyre::eyre!("Invalid scroll delta"))
        }
        Message::KeyEvent { keycode, .. } if *keycode > MAX_KEYCODE => {
            Err(color_eyre::eyre::eyre!("Invalid keycode: {}", keycode))
        }
        Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        } if text.len() > MAX_CLIPBOARD_TEXT_BYTES => {
            Err(color_eyre::eyre::eyre!("Clipboard text too large"))
        }
        _ => Ok(()),
    }
}

/// Encode a message to bytes.
pub fn encode(msg: &Message) -> color_eyre::eyre::Result<Vec<u8>> {
    validate_message(msg)?;
    let bytes = bincode::serialize(msg)?;
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(color_eyre::eyre::eyre!(
            "Message too large: {} bytes",
            bytes.len()
        ));
    }
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
    if len > MAX_MESSAGE_SIZE {
        return Err(color_eyre::eyre::eyre!("Message too large: {} bytes", len));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let msg: Message = bincode::deserialize(&buf[4..4 + len])?;
    validate_message(&msg)?;
    Ok(Some((msg, 4 + len)))
}
