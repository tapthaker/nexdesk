use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Result, WrapErr};
use rcgen::{CertificateParams, KeyPair};
use ring::digest;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tracing::info;

use crate::config::NexdeskConfig;

static TRUST_CONFIG_LOCK: Mutex<()> = Mutex::new(());
static CERT_FILE_LOCK: Mutex<()> = Mutex::new(());

const MAX_CERT_DER_BYTES: usize = 1024 * 1024;
const MAX_KEY_DER_BYTES: usize = 1024 * 1024;

fn normalize_fingerprint(fp: &str) -> Result<String> {
    let hex: String = fp
        .trim()
        .chars()
        .filter(|c| *c != ':')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(color_eyre::eyre::eyre!(
            "Invalid SHA-256 fingerprint format: expected 64 hex digits with optional ':' separators"
        ));
    }

    Ok(hex
        .as_bytes()
        .chunks(2)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(":"))
}

fn trusted_fingerprints_contains(fingerprints: &[String], normalized: &str) -> bool {
    fingerprints
        .iter()
        .filter_map(|stored| normalize_fingerprint(stored).ok())
        .any(|stored| stored == normalized)
}

#[cfg(unix)]
fn restrict_private_key_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(|| {
        format!(
            "Failed to restrict private key permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_private_key_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
struct CertDirLock(std::fs::File);

#[cfg(unix)]
impl Drop for CertDirLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn lock_cert_dir(certs_dir: &Path) -> Result<CertDirLock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let lock_path = certs_dir.join(".nexdesk-certs.lock");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .wrap_err_with(|| format!("Failed to open certificate lock: {}", lock_path.display()))?;
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(
        || {
            format!(
                "Failed to restrict certificate lock: {}",
                lock_path.display()
            )
        },
    )?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        return Err(color_eyre::eyre::eyre!(
            "Failed to lock certificate directory: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(CertDirLock(file))
}

#[cfg(not(unix))]
struct CertDirLock;

#[cfg(not(unix))]
fn lock_cert_dir(_certs_dir: &Path) -> Result<CertDirLock> {
    Ok(CertDirLock)
}

#[cfg(unix)]
struct TrustConfigFileLock(std::fs::File);

#[cfg(unix)]
impl Drop for TrustConfigFileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn lock_trust_config() -> Result<TrustConfigFileLock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let config_dir = NexdeskConfig::config_dir()?;
    let lock_path = config_dir.join(".nexdesk-trust.lock");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .wrap_err_with(|| format!("Failed to open trust config lock: {}", lock_path.display()))?;
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).wrap_err_with(
        || {
            format!(
                "Failed to restrict trust config lock: {}",
                lock_path.display()
            )
        },
    )?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        return Err(color_eyre::eyre::eyre!(
            "Failed to lock trust config: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(TrustConfigFileLock(file))
}

#[cfg(not(unix))]
struct TrustConfigFileLock;

#[cfg(not(unix))]
fn lock_trust_config() -> Result<TrustConfigFileLock> {
    Ok(TrustConfigFileLock)
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn read_bounded_bytes(path: &Path, max_bytes: usize, context: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .wrap_err_with(|| format!("Failed to open {context}: {}", path.display()))?;
    let mut limited = file.take(max_bytes as u64 + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("Failed to read {context}: {}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(color_eyre::eyre::eyre!(
            "{} too large: {} bytes (max {})",
            context,
            bytes.len(),
            max_bytes
        ));
    }
    Ok(bytes)
}

fn write_cert_file_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid certificate path: {}", path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".nexdesk-cert.")
        .tempfile_in(dir)
        .wrap_err_with(|| {
            format!(
                "Failed to create temporary certificate file in {}",
                dir.display()
            )
        })?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .wrap_err_with(|| {
                format!(
                    "Failed to restrict temporary private key permissions: {}",
                    tmp.path().display()
                )
            })?;
    }

    let _ = private;
    tmp.write_all(bytes)
        .wrap_err_with(|| format!("Failed to write certificate file: {}", tmp.path().display()))?;
    tmp.as_file_mut()
        .sync_all()
        .wrap_err("Failed to sync certificate file")?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .wrap_err_with(|| format!("Failed to replace certificate file: {}", path.display()))?;

    if private {
        restrict_private_key_permissions(path)?;
        sync_file(path)
            .wrap_err_with(|| format!("Failed to sync private key metadata: {}", path.display()))?;
    }
    sync_directory(dir).wrap_err_with(|| {
        format!(
            "Failed to sync certificate directory after replace: {}",
            dir.display()
        )
    })?;
    Ok(())
}

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

fn certificate_key_pair_valid(
    cert: &CertificateDer<'static>,
    key: &PrivateKeyDer<'static>,
) -> bool {
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.clone_key())
        .is_ok()
}

fn read_stored_cert_pair(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_bytes = read_bounded_bytes(cert_path, MAX_CERT_DER_BYTES, "certificate DER")?;
    let key_bytes = read_bounded_bytes(key_path, MAX_KEY_DER_BYTES, "private key DER")?;
    Ok((
        CertificateDer::from(cert_bytes),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
    ))
}

/// Load or generate certificates. Stores them in the config certs directory.
pub fn load_or_generate_certs() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let _guard = CERT_FILE_LOCK
        .lock()
        .map_err(|_| color_eyre::eyre::eyre!("Certificate file lock poisoned"))?;
    let certs_dir = NexdeskConfig::certs_dir()?;
    let _dir_lock = lock_cert_dir(&certs_dir)?;
    let cert_path = certs_dir.join("cert.der");
    let key_path = certs_dir.join("key.der");

    if cert_path.exists() && key_path.exists() {
        restrict_private_key_permissions(&key_path)?;
        match read_stored_cert_pair(&cert_path, &key_path) {
            Ok((cert_der, key_der)) if certificate_key_pair_valid(&cert_der, &key_der) => {
                return Ok((cert_der, key_der));
            }
            Ok(_) => {
                tracing::warn!(
                    "Stored nexdesk certificate and private key do not match or are invalid; regenerating"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Stored nexdesk certificate/key could not be read safely; regenerating: {e}"
                );
            }
        }
    }

    let (cert_der, key_der) = generate_self_signed()?;
    write_cert_file_atomic(&cert_path, cert_der.as_ref(), false)?;
    match &key_der {
        PrivateKeyDer::Pkcs8(k) => write_cert_file_atomic(&key_path, k.secret_pkcs8_der(), true)?,
        _ => unreachable!(),
    }
    info!("Generated new self-signed certificate");
    Ok((cert_der, key_der))
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
    let normalized = normalize_fingerprint(fp)?;
    let _guard = TRUST_CONFIG_LOCK
        .lock()
        .map_err(|_| color_eyre::eyre::eyre!("Trust config lock poisoned"))?;
    let _file_guard = lock_trust_config()?;
    let mut config = NexdeskConfig::load()?;
    if trusted_fingerprints_contains(&config.trusted_fingerprints, &normalized) {
        println!("Fingerprint already trusted.");
    } else {
        let valid_count = config
            .trusted_fingerprints
            .iter()
            .filter_map(|stored| normalize_fingerprint(stored).ok())
            .collect::<std::collections::HashSet<_>>()
            .len();
        if valid_count >= crate::config::MAX_TRUSTED_FINGERPRINTS {
            return Err(color_eyre::eyre::eyre!(
                "Too many trusted fingerprints: {} (max {})",
                valid_count,
                crate::config::MAX_TRUSTED_FINGERPRINTS
            ));
        }
        config.trusted_fingerprints.push(normalized.clone());
        config.save()?;
        println!("Trusted fingerprint: {}", normalized);
    }
    Ok(())
}

/// Check if a fingerprint is trusted.
pub fn is_fingerprint_trusted(fp: &str) -> bool {
    let Ok(normalized) = normalize_fingerprint(fp) else {
        return false;
    };
    if let Ok(config) = NexdeskConfig::load() {
        trusted_fingerprints_contains(&config.trusted_fingerprints, &normalized)
    } else {
        false
    }
}

/// Return the SHA-256 fingerprint of the peer certificate used for this QUIC connection.
pub fn peer_fingerprint(connection: &quinn::Connection) -> Option<String> {
    let identity = connection.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(fingerprint)
}

/// Build a quinn server config with our self-signed cert.
pub fn server_config() -> Result<quinn::ServerConfig> {
    let (cert_der, key_der) = load_or_generate_certs()?;

    let server_crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(TofuClientVerifier))
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
    let (cert_der, key_der) = load_or_generate_certs()?;
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TofuVerifier))
        .with_client_auth_cert(vec![cert_der], key_der.clone_key())
        .wrap_err("Failed to build TLS client config")?;

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
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(15).try_into().unwrap()));
    transport
}

fn supported_verify_schemes() -> Vec<SignatureScheme> {
    vec![
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ED25519,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::RSA_PKCS1_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512,
    ]
}

fn verify_tls12_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    let provider = rustls::crypto::ring::default_provider();
    rustls::crypto::verify_tls12_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

fn verify_tls13_signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
    let provider = rustls::crypto::ring::default_provider();
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

/// Certificate verifier that accepts self-signed peers at the chain level.
/// Identity trust is handled by comparing the TLS peer certificate fingerprint
/// with the OTP-paired trust store, but TLS handshake signatures are still
/// verified so a trusted fingerprint cannot be spoofed without its private key.
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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_verify_schemes()
    }
}

/// Optional client-certificate verifier for mutual TOFU. Certificates are
/// accepted at chain level because nexdesk uses self-signed local certificates,
/// but proof-of-possession signatures are verified by rustls/webpki.
#[derive(Debug)]
struct TofuClientVerifier;

impl rustls::server::danger::ClientCertVerifier for TofuClientVerifier {
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_colonless_fingerprint() {
        let fp = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        assert_eq!(
            normalize_fingerprint(fp).unwrap(),
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn bounded_der_reader_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cert.der");
        std::fs::write(&path, vec![b'a'; 17]).unwrap();
        assert_eq!(
            read_bounded_bytes(&path, 17, "certificate DER")
                .unwrap()
                .len(),
            17
        );
        assert!(read_bounded_bytes(&path, 16, "certificate DER").is_err());
    }

    #[test]
    fn stored_cert_pair_reader_rejects_oversized_der_before_validation() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.der");
        let key_path = dir.path().join("key.der");
        std::fs::write(&cert_path, vec![b'a'; MAX_CERT_DER_BYTES + 1]).unwrap();
        std::fs::write(&key_path, b"not-a-key").unwrap();
        assert!(read_stored_cert_pair(&cert_path, &key_path).is_err());
    }

    #[test]
    fn rejects_invalid_fingerprints() {
        assert!(normalize_fingerprint("not a fingerprint").is_err());
        assert!(normalize_fingerprint("00:11").is_err());
        assert!(normalize_fingerprint(
            "GG112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF"
        )
        .is_err());
    }

    #[test]
    fn trusted_fingerprint_lookup_accepts_legacy_format_variants() {
        let stored = vec![
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_string(),
            "not-a-fingerprint".to_string(),
        ];
        assert!(trusted_fingerprints_contains(
            &stored,
            "00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF"
        ));
        assert!(!trusted_fingerprints_contains(
            &stored,
            "FF:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn certificate_lock_file_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".nexdesk-certs.lock");
        std::fs::write(&lock_path, b"").unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let _guard = lock_cert_dir(dir.path()).unwrap();
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn certificate_key_pair_validation_detects_mismatches() {
        let (cert_a, key_a) = generate_self_signed().unwrap();
        let (cert_b, _key_b) = generate_self_signed().unwrap();
        assert!(certificate_key_pair_valid(&cert_a, &key_a));
        assert!(!certificate_key_pair_valid(&cert_b, &key_a));
    }
}
