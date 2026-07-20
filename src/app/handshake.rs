use color_eyre::eyre::{eyre, Result};

use crate::ports::PeerScreen;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerHelloAck {
    pub accepted: bool,
    pub otp: Option<String>,
    pub screen: Option<PeerScreen>,
    pub build_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerPairingMethod {
    Otp,
    TrustedCertificate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerHandshakeOutcome {
    pub peer_screen: PeerScreen,
    pub peer_build_version: String,
    pub pairing_method: ServerPairingMethod,
    pub version_mismatch: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServerHandshakeDecision {
    Accept {
        pairing_result: bool,
        outcome: ServerHandshakeOutcome,
    },
    Reject {
        pairing_result: Option<bool>,
        reason: String,
    },
}

impl ServerHandshakeDecision {
    pub(crate) fn into_result(self) -> Result<ServerHandshakeOutcome> {
        match self {
            Self::Accept { outcome, .. } => Ok(outcome),
            Self::Reject { reason, .. } => Err(eyre!(reason)),
        }
    }
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

pub(crate) fn require_server_certificate_fingerprint(
    fingerprint: Option<String>,
) -> Result<String> {
    fingerprint.ok_or_else(|| eyre!("Server certificate is absent"))
}

pub(crate) fn decide_server_handshake(
    expected_otp: &str,
    local_build_version: &str,
    response: HandshakeMessage<ServerHelloAck>,
) -> ServerHandshakeDecision {
    let ack = match require_handshake_message("HelloAck", response) {
        Ok(ack) => ack,
        Err(error) => {
            return ServerHandshakeDecision::Reject {
                pairing_result: None,
                reason: error.to_string(),
            };
        }
    };
    if !ack.accepted {
        return ServerHandshakeDecision::Reject {
            pairing_result: None,
            reason: "Peer rejected connection".to_string(),
        };
    }

    let pairing_method = match ack.otp.as_deref() {
        Some(code) if code == expected_otp => ServerPairingMethod::Otp,
        Some(_) => {
            return ServerHandshakeDecision::Reject {
                pairing_result: Some(false),
                reason: "Invalid pairing code".to_string(),
            };
        }
        None => ServerPairingMethod::TrustedCertificate,
    };
    let peer_build_version = ack.build_version.unwrap_or_else(|| "unknown".to_string());
    let peer_screen = ack.screen.unwrap_or(PeerScreen {
        width: 1920,
        height: 1080,
    });

    ServerHandshakeDecision::Accept {
        pairing_result: true,
        outcome: ServerHandshakeOutcome {
            peer_screen,
            version_mismatch: peer_build_version != local_build_version,
            peer_build_version,
            pairing_method,
        },
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
    fn server_handshake_decision_is_independent_of_wire_streams() {
        let decision = decide_server_handshake(
            "123456",
            "v1",
            HandshakeMessage::Expected(ServerHelloAck {
                accepted: true,
                otp: Some("123456".to_string()),
                screen: Some(PeerScreen {
                    width: 2560,
                    height: 1440,
                }),
                build_version: Some("v1".to_string()),
            }),
        );

        assert_eq!(
            decision,
            ServerHandshakeDecision::Accept {
                pairing_result: true,
                outcome: ServerHandshakeOutcome {
                    peer_screen: PeerScreen {
                        width: 2560,
                        height: 1440,
                    },
                    peer_build_version: "v1".to_string(),
                    pairing_method: ServerPairingMethod::Otp,
                    version_mismatch: false,
                },
            }
        );
    }

    #[test]
    fn server_handshake_rejection_describes_response_policy() {
        assert_eq!(
            decide_server_handshake(
                "123456",
                "v1",
                HandshakeMessage::Expected(ServerHelloAck {
                    accepted: true,
                    otp: Some("000000".to_string()),
                    screen: None,
                    build_version: None,
                }),
            ),
            ServerHandshakeDecision::Reject {
                pairing_result: Some(false),
                reason: "Invalid pairing code".to_string(),
            }
        );
        assert!(require_server_certificate_fingerprint(None)
            .unwrap_err()
            .to_string()
            .contains("absent"));
    }

    #[test]
    fn server_handshake_scenarios_cover_otp_trust_and_version_mismatch() {
        let cases = [
            (Some("123456"), Some("v2"), ServerPairingMethod::Otp, false),
            (
                None,
                Some("v2"),
                ServerPairingMethod::TrustedCertificate,
                false,
            ),
            (
                None,
                Some("v1"),
                ServerPairingMethod::TrustedCertificate,
                true,
            ),
        ];

        for (otp, peer_build, pairing_method, version_mismatch) in cases {
            let decision = decide_server_handshake(
                "123456",
                "v2",
                HandshakeMessage::Expected(ServerHelloAck {
                    accepted: true,
                    otp: otp.map(str::to_string),
                    screen: None,
                    build_version: peer_build.map(str::to_string),
                }),
            );
            let outcome = decision.into_result().unwrap();
            assert_eq!(outcome.pairing_method, pairing_method);
            assert_eq!(outcome.version_mismatch, version_mismatch);
            assert_eq!(
                outcome.peer_screen,
                PeerScreen {
                    width: 1920,
                    height: 1080,
                }
            );
        }
    }

    #[test]
    fn server_handshake_scenarios_cover_invalid_malformed_and_disconnects() {
        let cases = [
            (
                HandshakeMessage::Expected(ServerHelloAck {
                    accepted: true,
                    otp: Some("000000".to_string()),
                    screen: None,
                    build_version: None,
                }),
                Some(false),
                "Invalid pairing code",
            ),
            (
                HandshakeMessage::Unexpected("Heartbeat".to_string()),
                None,
                "Expected HelloAck, got: Heartbeat",
            ),
            (
                HandshakeMessage::StreamClosed,
                None,
                "Expected HelloAck, got: end of stream",
            ),
        ];

        for (response, pairing_result, reason) in cases {
            assert_eq!(
                decide_server_handshake("123456", "v2", response),
                ServerHandshakeDecision::Reject {
                    pairing_result,
                    reason: reason.to_string(),
                }
            );
        }
    }

    #[test]
    fn absent_server_certificate_is_rejected_before_handshake() {
        assert_eq!(
            require_server_certificate_fingerprint(Some("trusted-fingerprint".to_string()))
                .unwrap(),
            "trusted-fingerprint"
        );
        assert_eq!(
            require_server_certificate_fingerprint(None)
                .unwrap_err()
                .to_string(),
            "Server certificate is absent"
        );
    }

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
