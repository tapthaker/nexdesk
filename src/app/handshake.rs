use color_eyre::eyre::{eyre, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairingDecision {
    UseTrustedIdentity,
    PromptForOtp,
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
    fn hello_validation_is_independent_of_transport_streams() {
        assert!(validate_client_server_hello(5, 6, FINGERPRINT, FINGERPRINT, 1920, 1080).is_err());
        assert!(validate_client_server_hello(6, 6, "hello", "tls", 1920, 1080).is_err());
        assert!(validate_client_server_hello(6, 6, FINGERPRINT, FINGERPRINT, 0, 1080).is_err());
    }
}
