use color_eyre::eyre::{eyre, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HandshakeMessage<T> {
    Expected(T),
    Unexpected(String),
    StreamClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairingDecision {
    UseTrustedIdentity,
    PromptForOtp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairingCompletion {
    Established,
    PersistTrust,
}

pub(crate) fn require_handshake_message<T>(
    expected: &str,
    message: HandshakeMessage<T>,
) -> Result<T> {
    match message {
        HandshakeMessage::Expected(value) => Ok(value),
        HandshakeMessage::Unexpected(actual) => {
            Err(eyre!("Expected {}, got: {}", expected, actual))
        }
        HandshakeMessage::StreamClosed => Err(eyre!("Expected {}, got: end of stream", expected)),
    }
}

pub(crate) fn validate_client_server_hello(
    server_protocol: u32,
    expected_protocol: u32,
    advertised_fingerprint: &str,
    tls_fingerprint: &str,
    screen_width: u32,
    screen_height: u32,
) -> Result<()> {
    if server_protocol != expected_protocol {
        return Err(eyre!(
            "Protocol version mismatch: server={}, client={}",
            server_protocol,
            expected_protocol
        ));
    }
    if advertised_fingerprint != tls_fingerprint {
        return Err(eyre!(
            "Server fingerprint mismatch: hello={}, tls={}",
            advertised_fingerprint,
            tls_fingerprint
        ));
    }
    if screen_width == 0 || screen_height == 0 {
        return Err(eyre!(
            "Invalid server screen size: {}x{}",
            screen_width,
            screen_height
        ));
    }
    Ok(())
}

pub(crate) fn client_pairing_decision(server_is_trusted: bool) -> PairingDecision {
    if server_is_trusted {
        PairingDecision::UseTrustedIdentity
    } else {
        PairingDecision::PromptForOtp
    }
}

pub(crate) fn complete_client_pairing(
    decision: PairingDecision,
    response: HandshakeMessage<bool>,
) -> Result<PairingCompletion> {
    let success = require_handshake_message("PairingResult", response)?;
    if !success {
        return Err(eyre!("Pairing failed: invalid code"));
    }
    Ok(match decision {
        PairingDecision::UseTrustedIdentity => PairingCompletion::Established,
        PairingDecision::PromptForOtp => PairingCompletion::PersistTrust,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINGERPRINT: &str = "fingerprint";

    #[test]
    fn valid_hello_selects_pairing_from_persisted_trust() {
        validate_client_server_hello(6, 6, FINGERPRINT, FINGERPRINT, 1920, 1080).unwrap();
        assert_eq!(
            client_pairing_decision(true),
            PairingDecision::UseTrustedIdentity
        );
        assert_eq!(
            client_pairing_decision(false),
            PairingDecision::PromptForOtp
        );
    }

    #[test]
    fn hello_validation_rejects_malformed_inputs_table() {
        let cases = [
            (
                5,
                FINGERPRINT,
                FINGERPRINT,
                1920,
                1080,
                "Protocol version mismatch",
            ),
            (6, "hello", "tls", 1920, 1080, "fingerprint mismatch"),
            (
                6,
                FINGERPRINT,
                FINGERPRINT,
                0,
                1080,
                "Invalid server screen size",
            ),
            (
                6,
                FINGERPRINT,
                FINGERPRINT,
                1920,
                0,
                "Invalid server screen size",
            ),
        ];

        for (protocol, advertised, tls, width, height, expected) in cases {
            let error = validate_client_server_hello(protocol, 6, advertised, tls, width, height)
                .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?} in {error}"
            );
        }
    }

    #[test]
    fn hello_stage_reports_unexpected_messages_and_stream_closure_table() {
        let cases = [
            (
                HandshakeMessage::<()>::Unexpected("Heartbeat".to_string()),
                "Expected Hello, got: Heartbeat",
            ),
            (
                HandshakeMessage::<()>::StreamClosed,
                "Expected Hello, got: end of stream",
            ),
        ];

        for (message, expected) in cases {
            let error = require_handshake_message("Hello", message).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn pairing_results_cover_trusted_otp_failure_malformed_and_closed_table() {
        let cases = [
            (
                PairingDecision::UseTrustedIdentity,
                HandshakeMessage::Expected(true),
                Some(PairingCompletion::Established),
                None,
            ),
            (
                PairingDecision::PromptForOtp,
                HandshakeMessage::Expected(true),
                Some(PairingCompletion::PersistTrust),
                None,
            ),
            (
                PairingDecision::PromptForOtp,
                HandshakeMessage::Expected(false),
                None,
                Some("Pairing failed: invalid code"),
            ),
            (
                PairingDecision::PromptForOtp,
                HandshakeMessage::Unexpected("Heartbeat".to_string()),
                None,
                Some("Expected PairingResult, got: Heartbeat"),
            ),
            (
                PairingDecision::PromptForOtp,
                HandshakeMessage::StreamClosed,
                None,
                Some("Expected PairingResult, got: end of stream"),
            ),
        ];

        for (decision, response, expected_completion, expected_error) in cases {
            let result = complete_client_pairing(decision, response);
            if let Some(expected) = expected_completion {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert_eq!(result.unwrap_err().to_string(), expected_error.unwrap());
            }
        }
    }
}
