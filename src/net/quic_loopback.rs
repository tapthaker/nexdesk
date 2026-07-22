use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Connection, Endpoint};
use rustls::pki_types::CertificateDer;

use super::tls;

pub(super) struct QuicLoopback {
    _certificate_root: tempfile::TempDir,
    certificate: CertificateDer<'static>,
    server: Endpoint,
    client: Endpoint,
    server_addr: SocketAddr,
}

impl QuicLoopback {
    pub(super) fn new() -> Result<Self> {
        let certificate_root = tempfile::tempdir()?;
        let (certificate, private_key) = tls::load_or_generate_certs_in(certificate_root.path())?;
        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .wrap_err("Failed to build loopback server TLS config")?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
        ));
        let server = Endpoint::server(server_config, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let server_addr = server.local_addr()?;

        let mut client = Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        client.set_default_client_config(tls::client_config()?);

        Ok(Self {
            _certificate_root: certificate_root,
            certificate,
            server,
            client,
            server_addr,
        })
    }

    pub(super) fn server_addr(&self) -> SocketAddr {
        self.server_addr
    }

    pub(super) fn certificate_fingerprint(&self) -> String {
        tls::fingerprint(&self.certificate)
    }

    pub(super) fn certificate_root(&self) -> &std::path::Path {
        self._certificate_root.path()
    }

    pub(super) async fn connect(&self) -> Result<(Connection, Connection)> {
        let server = async {
            self.server
                .accept()
                .await
                .ok_or_else(|| eyre!("loopback server endpoint closed"))?
                .await
                .wrap_err("Loopback server handshake failed")
        };
        let client = async {
            self.client
                .connect(self.server_addr, "nexdesk")?
                .await
                .wrap_err("Loopback client handshake failed")
        };
        let (server, client) = tokio::try_join!(server, client)?;
        Ok((server, client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_connects_ephemeral_endpoints_with_temporary_identity() {
        let fixture = QuicLoopback::new().unwrap();
        assert_eq!(fixture.server_addr().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(fixture.server_addr().port(), 0);
        assert!(fixture.certificate_root().join("cert.der").is_file());
        assert!(fixture.certificate_root().join("key.der").is_file());

        let expected_fingerprint = fixture.certificate_fingerprint();
        let (server, client) = fixture.connect().await.unwrap();
        assert_eq!(
            tls::peer_fingerprint(&client).unwrap(),
            expected_fingerprint
        );
        assert!(tls::peer_fingerprint(&server).is_none());

        client.close(0u32.into(), b"test complete");
        server.closed().await;
    }
}
