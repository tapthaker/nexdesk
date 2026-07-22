//! Narrow entry points used by out-of-package fuzz targets.
//!
//! Keeping the harness adapters here lets fuzz crates exercise private protocol
//! implementation without widening the normal public API of those modules.

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
