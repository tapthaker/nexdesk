use std::sync::Arc;

use color_eyre::eyre::{Result, WrapErr};
use rcgen::{CertificateParams, KeyPair};
use ring::digest;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::info;

use crate::config::NexdeskConfig;

/// Generate a self-signed certificate and return (cert_der, key_der).
pub fn generate_self_signed() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(vec!["nexdesk".to_string()])?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("nexdesk".to_string()),
    );

    let cert = params.self_signed(&key_pair)?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    Ok((cert_der, key_der))
}

/// Load or generate certificates. Stores them in the config certs directory.
pub fn load_or_generate_certs() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let certs_dir = NexdeskConfig::certs_dir()?;
    let cert_path = certs_dir.join("cert.der");
    let key_path = certs_dir.join("key.der");

    if cert_path.exists() && key_path.exists() {
        let cert_bytes = std::fs::read(&cert_path)?;
        let key_bytes = std::fs::read(&key_path)?;
        let cert_der = CertificateDer::from(cert_bytes);
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes));
        Ok((cert_der, key_der))
    } else {
        let (cert_der, key_der) = generate_self_signed()?;
        std::fs::write(&cert_path, cert_der.as_ref())?;
        match &key_der {
            PrivateKeyDer::Pkcs8(k) => std::fs::write(&key_path, k.secret_pkcs8_der())?,
            _ => unreachable!(),
        }
        info!("Generated new self-signed certificate");
        Ok((cert_der, key_der))
    }
}

/// Compute SHA-256 fingerprint of a certificate.
pub fn fingerprint(cert_der: &CertificateDer<'_>) -> String {
    let hash = digest::digest(&digest::SHA256, cert_der.as_ref());
    hash.as_ref()
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

/// Show this machine's certificate fingerprint.
pub fn show_fingerprint() -> Result<()> {
    let (cert_der, _) = load_or_generate_certs()?;
    let fp = fingerprint(&cert_der);
    println!("Certificate fingerprint:\n  {}", fp);
    Ok(())
}

/// Trust a peer fingerprint by adding it to the config.
pub fn trust_fingerprint(fp: &str) -> Result<()> {
    let mut config = NexdeskConfig::load()?;
    let normalized = fp.to_uppercase();
    if config.trusted_fingerprints.contains(&normalized) {
        println!("Fingerprint already trusted.");
    } else {
        config.trusted_fingerprints.push(normalized.clone());
        config.save()?;
        println!("Trusted fingerprint: {}", normalized);
    }
    Ok(())
}

/// Check if a fingerprint is trusted.
pub fn is_fingerprint_trusted(fp: &str) -> bool {
    if let Ok(config) = NexdeskConfig::load() {
        config.trusted_fingerprints.contains(&fp.to_uppercase())
    } else {
        false
    }
}

/// Build a quinn server config with our self-signed cert.
pub fn server_config() -> Result<quinn::ServerConfig> {
    let (cert_der, key_der) = load_or_generate_certs()?;

    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .wrap_err("Failed to build TLS server config")?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));

    let transport = keep_alive_transport();
    server_config.transport_config(Arc::new(transport));

    Ok(server_config)
}

/// Build a quinn client config with TOFU certificate verification.
pub fn client_config() -> Result<quinn::ClientConfig> {
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TofuVerifier))
        .with_no_client_auth();

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)?,
    ));

    let transport = keep_alive_transport();
    client_config.transport_config(Arc::new(transport));

    Ok(client_config)
}

/// Build transport config with keep-alive to prevent idle timeouts.
fn keep_alive_transport() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.max_idle_timeout(None);
    transport
}

/// Certificate verifier that always accepts.
/// Trust decisions are handled at the application level via OTP pairing.
#[derive(Debug)]
struct TofuVerifier;

impl rustls::client::danger::ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Always accept at TLS level; trust is verified via OTP pairing handshake
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
