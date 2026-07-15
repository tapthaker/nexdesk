use std::collections::HashSet;
use std::path::Component;

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
    /// Continuous gesture ended.
    Ended,
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
        version: u32,
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
}

/// Protocol version.
pub const PROTOCOL_VERSION: u32 = 5;

/// Build version string burned in at compile time (e.g. "v0.1.2" or "v0.1.2-3-gabcdef").
pub const BUILD_VERSION: &str = env!("NEXDESK_VERSION");

/// Maximum serialized message body size accepted by nexdesk streams.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
/// Maximum absolute scroll delta accepted in one protocol message.
pub const MAX_SCROLL_DELTA: f64 = 10_000.0;
/// Maximum total file-transfer payload size accepted by the protocol.
pub const MAX_TRANSFER_SIZE: u64 = 1024 * 1024 * 1024;
/// Maximum number of files accepted in one file-transfer offer.
pub const MAX_FILE_COUNT: usize = 1024;
/// Maximum UTF-8 byte length accepted for a single transferred file name.
pub const MAX_TRANSFER_FILE_NAME_BYTES: usize = 255;
/// Maximum payload size accepted in one file-transfer chunk message.
pub const MAX_FILE_CHUNK_SIZE: usize = 1024 * 1024;
/// Maximum UTF-8 clipboard text size accepted by the protocol.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum evdev keycode currently supported by nexdesk keymaps/injectors.
pub const MAX_KEYCODE: u32 = 255;
/// Maximum screen dimension accepted in protocol layouts.
pub const MAX_SCREEN_DIMENSION: u32 = 1_000_000;
/// Maximum peer hostname/display-name bytes accepted in handshake messages.
pub const MAX_PEER_NAME_BYTES: usize = 1024;
/// Maximum build-version string bytes accepted in handshake messages.
pub const MAX_BUILD_VERSION_BYTES: usize = 128;
/// Pairing OTP length in decimal digits.
pub const OTP_DIGITS: usize = 6;

fn validate_bounded_string(value: &str, max: usize, context: &str) -> color_eyre::eyre::Result<()> {
    if value.len() > max {
        return Err(color_eyre::eyre::eyre!(
            "{context} too large: {} bytes (max {})",
            value.len(),
            max
        ));
    }
    Ok(())
}

fn validate_display_string(value: &str, max: usize, context: &str) -> color_eyre::eyre::Result<()> {
    validate_bounded_string(value, max, context)?;
    if value.chars().any(char::is_control) {
        return Err(color_eyre::eyre::eyre!(
            "{context} contains control characters"
        ));
    }
    Ok(())
}

pub fn sanitize_display_string(value: &str, max: usize, default: &str) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { '�' } else { ch };
        let len = ch.len_utf8();
        if sanitized.len().saturating_add(len) > max {
            break;
        }
        sanitized.push(ch);
    }
    if sanitized.is_empty() {
        default.to_string()
    } else {
        sanitized
    }
}

pub fn local_build_version() -> String {
    sanitize_display_string(BUILD_VERSION, MAX_BUILD_VERSION_BYTES, "unknown")
}

pub fn message_summary(msg: &Message) -> String {
    match msg {
        Message::Heartbeat { .. } => "Heartbeat".to_string(),
        Message::HeartbeatAck { .. } => "HeartbeatAck".to_string(),
        Message::Hello { hostname, .. } => format!(
            "Hello(hostname={})",
            sanitize_display_string(hostname, MAX_PEER_NAME_BYTES, "nexdesk")
        ),
        Message::HelloAck { accepted, .. } => format!("HelloAck(accepted={accepted})"),
        Message::PairingResult { success } => format!("PairingResult(success={success})"),
        Message::MouseMove { .. } => "MouseMove".to_string(),
        Message::MouseButton { .. } => "MouseButton".to_string(),
        Message::MouseScroll { .. } => "MouseScroll".to_string(),
        Message::SwitchScreen { .. } => "SwitchScreen".to_string(),
        Message::KeyEvent { .. } => "KeyEvent".to_string(),
        Message::ScreenResize { .. } => "ScreenResize".to_string(),
        Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        } => format!("ClipboardUpdate(text_bytes={})", text.len()),
        Message::FileTransferOffer {
            transfer_id,
            files,
            total_size,
        } => format!(
            "FileTransferOffer(transfer_id={transfer_id}, files={}, total_size={total_size})",
            files.len()
        ),
        Message::FileTransferAccept { transfer_id } => {
            format!("FileTransferAccept(transfer_id={transfer_id})")
        }
        Message::FileTransferChunk {
            transfer_id,
            file_index,
            offset,
            data,
        } => format!(
            "FileTransferChunk(transfer_id={transfer_id}, file_index={file_index}, offset={offset}, bytes={})",
            data.len()
        ),
        Message::FileTransferComplete {
            transfer_id,
            file_index,
            ..
        } => format!("FileTransferComplete(transfer_id={transfer_id}, file_index={file_index})"),
        Message::FileTransferDone { transfer_id } => {
            format!("FileTransferDone(transfer_id={transfer_id})")
        }
        Message::FileTransferCancel { transfer_id } => {
            format!("FileTransferCancel(transfer_id={transfer_id})")
        }
        Message::WakeDisplay => "WakeDisplay".to_string(),
    }
}

pub fn optional_message_summary(msg: Option<&Message>) -> String {
    msg.map(message_summary)
        .unwrap_or_else(|| "end of stream".to_string())
}

fn validate_peer_name(value: &str) -> color_eyre::eyre::Result<()> {
    validate_display_string(value, MAX_PEER_NAME_BYTES, "Peer hostname")?;
    if value.is_empty() {
        return Err(color_eyre::eyre::eyre!("Peer hostname cannot be empty"));
    }
    Ok(())
}

fn validate_fingerprint(value: &str) -> color_eyre::eyre::Result<()> {
    let hex: String = value.chars().filter(|c| *c != ':').collect();
    if hex.len() != 64
        || !hex.chars().all(|c| c.is_ascii_hexdigit())
        || !value.chars().all(|c| c == ':' || c.is_ascii_hexdigit())
    {
        return Err(color_eyre::eyre::eyre!(
            "Invalid certificate fingerprint in handshake"
        ));
    }
    Ok(())
}

fn validate_build_version(value: &Option<String>) -> color_eyre::eyre::Result<()> {
    if let Some(value) = value {
        validate_display_string(value, MAX_BUILD_VERSION_BYTES, "Build version")?;
    }
    Ok(())
}

fn validate_otp(value: &Option<String>) -> color_eyre::eyre::Result<()> {
    if let Some(value) = value {
        if value.len() != OTP_DIGITS || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(color_eyre::eyre::eyre!(
                "Invalid pairing OTP format: expected {OTP_DIGITS} decimal digits"
            ));
        }
    }
    Ok(())
}

fn validate_screen_layout(screen: &ScreenLayout, context: &str) -> color_eyre::eyre::Result<()> {
    if screen.width == 0 || screen.height == 0 {
        return Err(color_eyre::eyre::eyre!(
            "Invalid {context} screen size: {}x{}",
            screen.width,
            screen.height
        ));
    }
    if screen.width > MAX_SCREEN_DIMENSION || screen.height > MAX_SCREEN_DIMENSION {
        return Err(color_eyre::eyre::eyre!(
            "Invalid {context} screen size: {}x{} exceeds maximum dimension {}",
            screen.width,
            screen.height,
            MAX_SCREEN_DIMENSION
        ));
    }
    Ok(())
}

pub fn is_portable_transfer_file_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_TRANSFER_FILE_NAME_BYTES
        || name.contains('\0')
        || name.contains('\\')
        || name.ends_with([' ', '.'])
        || name
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_file_name(name: &str) -> color_eyre::eyre::Result<()> {
    let path = std::path::Path::new(name);
    let mut components = path.components();
    let Some(Component::Normal(_)) = components.next() else {
        return Err(color_eyre::eyre::eyre!(
            "Unsafe file name in transfer: {name:?}"
        ));
    };
    if path.is_absolute() || components.next().is_some() || !is_portable_transfer_file_name(name) {
        return Err(color_eyre::eyre::eyre!(
            "Unsafe file name in transfer: {name:?}"
        ));
    }
    Ok(())
}

fn validate_file_offer(files: &[FileInfo], total_size: u64) -> color_eyre::eyre::Result<()> {
    if files.is_empty() {
        return Err(color_eyre::eyre::eyre!(
            "File transfer offer contains no files"
        ));
    }
    if files.len() > MAX_FILE_COUNT {
        return Err(color_eyre::eyre::eyre!(
            "File transfer offer contains too many files: {} (max {})",
            files.len(),
            MAX_FILE_COUNT
        ));
    }

    let mut names = HashSet::new();
    let mut computed_total = 0u64;
    for file in files {
        validate_file_name(&file.name)?;
        if !names.insert(file.name.to_lowercase()) {
            return Err(color_eyre::eyre::eyre!(
                "Duplicate file name in transfer (case-insensitive): {}",
                file.name
            ));
        }
        computed_total = computed_total
            .checked_add(file.size)
            .ok_or_else(|| color_eyre::eyre::eyre!("File transfer size overflow"))?;
    }

    if computed_total != total_size {
        return Err(color_eyre::eyre::eyre!(
            "File transfer total size mismatch: offer={}, files={}",
            total_size,
            computed_total
        ));
    }
    if total_size > MAX_TRANSFER_SIZE {
        return Err(color_eyre::eyre::eyre!(
            "File transfer too large: {} bytes (max {})",
            total_size,
            MAX_TRANSFER_SIZE
        ));
    }
    Ok(())
}

fn validate_sha256_hex(checksum: &str) -> color_eyre::eyre::Result<()> {
    if checksum.len() != 64 || !checksum.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(color_eyre::eyre::eyre!(
            "Invalid SHA-256 checksum in file transfer"
        ));
    }
    Ok(())
}

/// Validate semantic constraints that serde/bincode cannot express.
pub fn validate_message(msg: &Message) -> color_eyre::eyre::Result<()> {
    match msg {
        Message::Hello {
            hostname,
            screen,
            fingerprint,
            build_version,
            ..
        } => {
            validate_peer_name(hostname)?;
            validate_screen_layout(screen, "hello")?;
            validate_fingerprint(fingerprint)?;
            validate_build_version(build_version)?;
        }
        Message::HelloAck {
            otp,
            screen,
            build_version,
            ..
        } => {
            validate_otp(otp)?;
            if let Some(screen) = screen {
                validate_screen_layout(screen, "hello ack")?;
            }
            validate_build_version(build_version)?;
        }
        Message::ScreenResize { screen } => validate_screen_layout(screen, "resize")?,
        Message::MouseButton { button, .. } => {
            if *button > 2 {
                return Err(color_eyre::eyre::eyre!(
                    "Unsupported mouse button index: {}",
                    button
                ));
            }
        }
        Message::KeyEvent { keycode, .. } => {
            if *keycode > MAX_KEYCODE {
                return Err(color_eyre::eyre::eyre!(
                    "Unsupported keycode: {} (max {})",
                    keycode,
                    MAX_KEYCODE
                ));
            }
        }
        Message::ClipboardUpdate {
            content: ClipboardContent::Text(text),
        } => {
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                return Err(color_eyre::eyre::eyre!(
                    "Clipboard update too large: {} bytes (max {})",
                    text.len(),
                    MAX_CLIPBOARD_TEXT_BYTES
                ));
            }
        }
        Message::FileTransferOffer {
            files, total_size, ..
        } => validate_file_offer(files, *total_size)?,
        Message::FileTransferChunk { data, .. } => {
            if data.is_empty() {
                return Err(color_eyre::eyre::eyre!(
                    "File transfer chunk contains no data"
                ));
            }
            if data.len() > MAX_FILE_CHUNK_SIZE {
                return Err(color_eyre::eyre::eyre!(
                    "File transfer chunk too large: {} bytes (max {})",
                    data.len(),
                    MAX_FILE_CHUNK_SIZE
                ));
            }
        }
        Message::FileTransferComplete { checksum, .. } => validate_sha256_hex(checksum)?,
        Message::MouseScroll { dx, dy, .. } => {
            if !dx.is_finite() || !dy.is_finite() {
                return Err(color_eyre::eyre::eyre!(
                    "Invalid scroll delta: non-finite value ({dx}, {dy})"
                ));
            }
            if dx.abs() > MAX_SCROLL_DELTA || dy.abs() > MAX_SCROLL_DELTA {
                return Err(color_eyre::eyre::eyre!(
                    "Invalid scroll delta: ({dx}, {dy}) exceeds maximum {MAX_SCROLL_DELTA}"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Encode a message to bytes.
pub fn encode(msg: &Message) -> color_eyre::eyre::Result<Vec<u8>> {
    validate_message(msg)?;
    let bytes = bincode::serialize(msg)?;
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(color_eyre::eyre::eyre!(
            "Message too large: {} bytes (max {})",
            bytes.len(),
            MAX_MESSAGE_SIZE
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
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_scroll_deltas() {
        assert!(validate_message(&Message::MouseScroll {
            dx: 0.0,
            dy: MAX_SCROLL_DELTA,
            phase: ScrollPhase::None,
        })
        .is_ok());
        assert!(validate_message(&Message::MouseScroll {
            dx: f64::NAN,
            dy: 0.0,
            phase: ScrollPhase::None,
        })
        .is_err());
        assert!(validate_message(&Message::MouseScroll {
            dx: 0.0,
            dy: MAX_SCROLL_DELTA + 1.0,
            phase: ScrollPhase::None,
        })
        .is_err());
    }

    #[test]
    fn encode_rejects_invalid_scroll_delta() {
        assert!(encode(&Message::MouseScroll {
            dx: f64::INFINITY,
            dy: 0.0,
            phase: ScrollPhase::None,
        })
        .is_err());
    }

    #[test]
    fn validates_protocol_screen_layouts() {
        let valid_screen = ScreenLayout {
            width: 1,
            height: 1,
        };
        assert!(validate_message(&Message::ScreenResize {
            screen: valid_screen.clone(),
        })
        .is_ok());
        assert!(validate_message(&Message::ScreenResize {
            screen: ScreenLayout {
                width: 0,
                height: 1,
            },
        })
        .is_err());
        assert!(validate_message(&Message::ScreenResize {
            screen: ScreenLayout {
                width: MAX_SCREEN_DIMENSION + 1,
                height: 1,
            },
        })
        .is_err());
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: None,
            screen: None,
            build_version: None,
        })
        .is_ok());
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "host".into(),
            screen: ScreenLayout {
                width: 1,
                height: 0,
            },
            fingerprint: "fp".into(),
            build_version: None,
        })
        .is_err());
    }

    #[test]
    fn validates_pairing_otp_format() {
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: Some("123456".into()),
            screen: None,
            build_version: None,
        })
        .is_ok());
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: Some("12345".into()),
            screen: None,
            build_version: None,
        })
        .is_err());
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: Some("12345x".into()),
            screen: None,
            build_version: None,
        })
        .is_err());
    }

    #[test]
    fn local_build_version_metadata_is_protocol_safe() {
        assert_eq!(
            sanitize_display_string("v1\n", MAX_BUILD_VERSION_BYTES, "unknown"),
            "v1�"
        );
        assert_eq!(
            sanitize_display_string("", MAX_BUILD_VERSION_BYTES, "unknown"),
            "unknown"
        );
        assert_eq!(
            sanitize_display_string(
                &"v".repeat(MAX_BUILD_VERSION_BYTES + 1),
                MAX_BUILD_VERSION_BYTES,
                "unknown"
            )
            .len(),
            MAX_BUILD_VERSION_BYTES
        );
    }

    #[test]
    fn validates_handshake_string_bounds_and_fingerprint() {
        let fp = "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF";
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "host".into(),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: fp.into(),
            build_version: Some("v0.1.0".into()),
        })
        .is_ok());
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "h".repeat(MAX_PEER_NAME_BYTES + 1),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: fp.into(),
            build_version: None,
        })
        .is_err());
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "".into(),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: fp.into(),
            build_version: None,
        })
        .is_err());
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "host\nname".into(),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: fp.into(),
            build_version: None,
        })
        .is_err());
        assert!(validate_message(&Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "host".into(),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: "not-a-fingerprint".into(),
            build_version: None,
        })
        .is_err());
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: None,
            screen: None,
            build_version: Some("v".repeat(MAX_BUILD_VERSION_BYTES + 1)),
        })
        .is_err());
        assert!(validate_message(&Message::HelloAck {
            accepted: true,
            version: PROTOCOL_VERSION,
            otp: None,
            screen: None,
            build_version: Some("v0.1.0\n".into()),
        })
        .is_err());
    }

    #[test]
    fn validates_mouse_button_indices() {
        assert!(validate_message(&Message::MouseButton {
            button: 2,
            pressed: true,
        })
        .is_ok());
        assert!(validate_message(&Message::MouseButton {
            button: 3,
            pressed: true,
        })
        .is_err());
    }

    #[test]
    fn validates_keycode_range() {
        assert!(validate_message(&Message::KeyEvent {
            keycode: MAX_KEYCODE,
            pressed: true,
            modifiers: 0,
        })
        .is_ok());
        assert!(validate_message(&Message::KeyEvent {
            keycode: MAX_KEYCODE + 1,
            pressed: true,
            modifiers: 0,
        })
        .is_err());
    }

    #[test]
    fn validates_clipboard_text_size() {
        assert!(validate_message(&Message::ClipboardUpdate {
            content: ClipboardContent::Text("a".repeat(MAX_CLIPBOARD_TEXT_BYTES)),
        })
        .is_ok());
        assert!(validate_message(&Message::ClipboardUpdate {
            content: ClipboardContent::Text("a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1)),
        })
        .is_err());
    }

    #[test]
    fn message_summary_omits_payloads_and_sanitizes_display_fields() {
        let clipboard = Message::ClipboardUpdate {
            content: ClipboardContent::Text("\x1b[31m".repeat(1024)),
        };
        assert_eq!(
            message_summary(&clipboard),
            "ClipboardUpdate(text_bytes=5120)"
        );

        let hello = Message::Hello {
            version: PROTOCOL_VERSION,
            hostname: "host\x1b[31m".into(),
            screen: ScreenLayout {
                width: 1,
                height: 1,
            },
            fingerprint: "AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA".into(),
            build_version: None,
        };
        let summary = message_summary(&hello);
        assert!(summary.contains("Hello(hostname=host�[31m)"));
        assert!(!summary.contains('\x1b'));

        assert_eq!(optional_message_summary(None), "end of stream");
    }

    #[test]
    fn validates_file_transfer_offer_semantics() {
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "a.txt".into(),
                size: 3,
            }],
            total_size: 3,
        })
        .is_ok());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "../a.txt".into(),
                size: 3,
            }],
            total_size: 3,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "bad\0name.txt".into(),
                size: 3,
            }],
            total_size: 3,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "dir\\file.txt".into(),
                size: 3,
            }],
            total_size: 3,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "a".repeat(MAX_TRANSFER_FILE_NAME_BYTES + 1),
                size: 3,
            }],
            total_size: 3,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "a".repeat(MAX_TRANSFER_FILE_NAME_BYTES),
                size: 3,
            }],
            total_size: 3,
        })
        .is_ok());
        for name in [
            "CON",
            "nul.txt",
            "bad:name.txt",
            "bad?.txt",
            "trailing.",
            "trailing ",
        ] {
            assert!(
                validate_message(&Message::FileTransferOffer {
                    transfer_id: 1,
                    files: vec![FileInfo {
                        name: name.into(),
                        size: 3,
                    }],
                    total_size: 3,
                })
                .is_err(),
                "accepted non-portable file name {name:?}"
            );
        }
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![
                FileInfo {
                    name: "a.txt".into(),
                    size: 1,
                },
                FileInfo {
                    name: "a.txt".into(),
                    size: 1,
                },
            ],
            total_size: 2,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![
                FileInfo {
                    name: "a.txt".into(),
                    size: 1,
                },
                FileInfo {
                    name: "A.TXT".into(),
                    size: 1,
                },
            ],
            total_size: 2,
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferOffer {
            transfer_id: 1,
            files: vec![FileInfo {
                name: "a.txt".into(),
                size: 3,
            }],
            total_size: 2,
        })
        .is_err());
    }

    #[test]
    fn validates_file_transfer_chunk_and_checksum() {
        assert!(validate_message(&Message::FileTransferChunk {
            transfer_id: 1,
            file_index: 0,
            offset: 0,
            data: vec![1],
        })
        .is_ok());
        assert!(validate_message(&Message::FileTransferChunk {
            transfer_id: 1,
            file_index: 0,
            offset: 0,
            data: vec![],
        })
        .is_err());
        assert!(validate_message(&Message::FileTransferComplete {
            transfer_id: 1,
            file_index: 0,
            checksum: "a".repeat(64),
        })
        .is_ok());
        assert!(validate_message(&Message::FileTransferComplete {
            transfer_id: 1,
            file_index: 0,
            checksum: "not-a-checksum".into(),
        })
        .is_err());
    }
}
