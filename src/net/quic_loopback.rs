use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Connection, Endpoint};
use rustls::pki_types::CertificateDer;

use super::tls;
use super::{framing, protocol, quinn_client, quinn_server};

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

    pub(super) async fn connect_peer_links(
        &self,
    ) -> Result<(
        Connection,
        Connection,
        quinn_server::QuinnServerPeerLink,
        quinn_client::QuinnClientPeerLink,
    )> {
        let (server_connection, client_connection) = self.connect().await?;
        let fingerprint = self.certificate_fingerprint();

        let server_handshake = async {
            let (mut send, mut recv) = server_connection.open_bi().await?;
            framing::send_message(
                &mut send,
                &protocol::Message::Hello {
                    version: protocol::PROTOCOL_VERSION,
                    hostname: "loopback-server".to_string(),
                    screen: protocol::ScreenLayout {
                        width: 1920,
                        height: 1080,
                    },
                    fingerprint,
                    build_version: Some(protocol::local_build_version()),
                },
            )
            .await?;
            match framing::recv_message(&mut recv).await? {
                Some(protocol::Message::HelloAck { accepted: true, .. }) => {}
                other => return Err(eyre!("Expected accepted HelloAck, got {other:?}")),
            }
            framing::send_message(
                &mut send,
                &protocol::Message::PairingResult { success: true },
            )
            .await?;
            Ok::<_, color_eyre::Report>((send, recv))
        };
        let client_handshake = async {
            let (mut send, mut recv) = client_connection.accept_bi().await?;
            match framing::recv_message(&mut recv).await? {
                Some(protocol::Message::Hello {
                    version,
                    fingerprint,
                    ..
                }) if version == protocol::PROTOCOL_VERSION
                    && fingerprint == tls::peer_fingerprint(&client_connection).unwrap() => {}
                other => return Err(eyre!("Expected valid Hello, got {other:?}")),
            }
            framing::send_message(
                &mut send,
                &protocol::Message::HelloAck {
                    accepted: true,
                    otp: None,
                    screen: Some(protocol::ScreenLayout {
                        width: 1280,
                        height: 720,
                    }),
                    build_version: Some(protocol::local_build_version()),
                },
            )
            .await?;
            match framing::recv_message(&mut recv).await? {
                Some(protocol::Message::PairingResult { success: true }) => {}
                other => return Err(eyre!("Expected successful PairingResult, got {other:?}")),
            }
            Ok::<_, color_eyre::Report>((send, recv))
        };
        let ((server_send, server_recv), (client_send, client_recv)) =
            tokio::try_join!(server_handshake, client_handshake)?;

        let server_link =
            quinn_server::QuinnServerPeerLink::open(&server_connection, server_send, server_recv);
        let client_link =
            quinn_client::QuinnClientPeerLink::open(&client_connection, client_send, client_recv);
        let (server_link, client_link) = tokio::try_join!(server_link, client_link)?;
        Ok((
            server_connection,
            client_connection,
            server_link,
            client_link,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        complete_client_pairing, decide_server_handshake, validate_client_server_hello,
        HandshakeMessage, PairingDecision, ServerHandshakeDecision, ServerHelloAck,
    };
    use crate::ports::{
        ClientClipboardCommand, ClientClipboardEvent, ClientControlCommand, ClientControlEvent,
        ClientInputEvent, ClientPeerLink, ClientTransportEvent, ServerClipboardCommand,
        ServerClipboardEvent, ServerControlCommand, ServerControlEvent, ServerInputCommand,
        ServerPeerLink, ServerTransportEvent,
    };

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

    #[tokio::test]
    async fn successful_handshake_opens_every_logical_stream() {
        let fixture = QuicLoopback::new().unwrap();
        let (server_connection, client_connection, server, client) =
            fixture.connect_peer_links().await.unwrap();

        server
            .send_control(ServerControlCommand::AcknowledgeHeartbeat { timestamp: 11 })
            .await
            .unwrap();
        assert_eq!(
            client.next_event().await,
            Some(ClientTransportEvent::Control(
                ClientControlEvent::HeartbeatAcknowledged { timestamp: 11 }
            ))
        );

        server
            .send_input(ServerInputCommand::MouseMoved { x: 10, y: -4 })
            .await
            .unwrap();
        assert_eq!(
            client.next_event().await,
            Some(ClientTransportEvent::Input(ClientInputEvent::MouseMoved {
                x: 10,
                y: -4,
            }))
        );

        server
            .send_clipboard(ServerClipboardCommand::SetPeerText(
                "server text".to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            client.next_event().await,
            Some(ClientTransportEvent::Clipboard(
                ClientClipboardEvent::TextChanged("server text".to_string())
            ))
        );

        client
            .send_control(ClientControlCommand::Heartbeat { timestamp: 22 })
            .await
            .unwrap();
        assert_eq!(
            server.next_event().await,
            Some(ServerTransportEvent::Control(
                ServerControlEvent::Heartbeat { timestamp: 22 }
            ))
        );

        client
            .send_clipboard(ClientClipboardCommand::SetPeerText(
                "client text".to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            server.next_event().await,
            Some(ServerTransportEvent::Clipboard(
                ServerClipboardEvent::TextChanged("client text".to_string())
            ))
        );

        server.shutdown().await;
        client_connection.close(0u32.into(), b"test complete");
        server_connection.closed().await;
    }

    #[tokio::test]
    async fn strict_tls_rejects_an_invalid_server_identity() {
        let fixture = QuicLoopback::new().unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(fixture.certificate.clone()).unwrap();
        let client_crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).unwrap(),
        ));
        let mut endpoint = Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        endpoint.set_default_client_config(client_config);

        let server = async {
            fixture
                .server
                .accept()
                .await
                .expect("incoming connection")
                .await
        };
        let client = endpoint
            .connect(fixture.server_addr(), "wrong-server-name")
            .unwrap();
        let (server_result, client_result) = tokio::join!(server, client);

        assert!(server_result.is_err());
        assert!(client_result.is_err());
        endpoint.close(0u32.into(), b"test complete");
    }

    #[tokio::test]
    async fn advertised_fingerprint_must_match_real_tls_identity() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();

        let server_hello = async {
            let (mut send, _recv) = server.open_bi().await?;
            framing::send_message(
                &mut send,
                &protocol::Message::Hello {
                    version: protocol::PROTOCOL_VERSION,
                    hostname: "impostor".to_string(),
                    screen: protocol::ScreenLayout {
                        width: 1920,
                        height: 1080,
                    },
                    fingerprint: "00:11:22".to_string(),
                    build_version: None,
                },
            )
            .await
        };
        let client_validation = async {
            let (_send, mut recv) = client.accept_bi().await?;
            let Some(protocol::Message::Hello {
                version,
                fingerprint,
                screen,
                ..
            }) = framing::recv_message(&mut recv).await?
            else {
                return Err(eyre!("expected Hello"));
            };
            validate_client_server_hello(
                version,
                protocol::PROTOCOL_VERSION,
                &fingerprint,
                &tls::peer_fingerprint(&client).unwrap(),
                screen.width,
                screen.height,
            )
        };
        let (send_result, validation_result) = tokio::join!(server_hello, client_validation);

        send_result.unwrap();
        assert!(validation_result
            .unwrap_err()
            .to_string()
            .contains("fingerprint mismatch"));
        client.close(0u32.into(), b"fingerprint mismatch");
        server.closed().await;
    }

    #[tokio::test]
    async fn untrusted_peer_with_wrong_otp_is_rejected_on_wire() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();
        let fingerprint = fixture.certificate_fingerprint();

        let server_handshake = async {
            let (mut send, mut recv) = server.open_bi().await?;
            framing::send_message(
                &mut send,
                &protocol::Message::Hello {
                    version: protocol::PROTOCOL_VERSION,
                    hostname: "loopback-server".to_string(),
                    screen: protocol::ScreenLayout {
                        width: 1920,
                        height: 1080,
                    },
                    fingerprint,
                    build_version: Some(protocol::local_build_version()),
                },
            )
            .await?;
            let response = match framing::recv_message(&mut recv).await? {
                Some(protocol::Message::HelloAck {
                    accepted,
                    otp,
                    screen,
                    build_version,
                }) => HandshakeMessage::Expected(ServerHelloAck {
                    accepted,
                    otp,
                    screen: screen.map(|screen| crate::ports::PeerScreen {
                        width: screen.width,
                        height: screen.height,
                    }),
                    build_version,
                }),
                other => HandshakeMessage::Unexpected(format!("{other:?}")),
            };
            let decision = decide_server_handshake("123456", "v1", response);
            assert!(matches!(
                decision,
                ServerHandshakeDecision::Reject {
                    pairing_result: Some(false),
                    ..
                }
            ));
            framing::send_message(
                &mut send,
                &protocol::Message::PairingResult { success: false },
            )
            .await
        };
        let client_handshake = async {
            let (mut send, mut recv) = client.accept_bi().await?;
            assert!(matches!(
                framing::recv_message(&mut recv).await?,
                Some(protocol::Message::Hello { .. })
            ));
            framing::send_message(
                &mut send,
                &protocol::Message::HelloAck {
                    accepted: true,
                    otp: Some("000000".to_string()),
                    screen: None,
                    build_version: Some("v1".to_string()),
                },
            )
            .await?;
            let result = match framing::recv_message(&mut recv).await? {
                Some(protocol::Message::PairingResult { success }) => {
                    HandshakeMessage::Expected(success)
                }
                other => HandshakeMessage::Unexpected(format!("{other:?}")),
            };
            complete_client_pairing(PairingDecision::PromptForOtp, result)
        };
        let (server_result, client_result) = tokio::join!(server_handshake, client_handshake);

        server_result.unwrap();
        assert_eq!(
            client_result.unwrap_err().to_string(),
            "Pairing failed: invalid code"
        );
        client.close(0u32.into(), b"pairing rejected");
        server.closed().await;
    }

    #[tokio::test]
    async fn framed_message_survives_single_byte_quic_splits() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();
        let frame = protocol::encode(&protocol::Message::Heartbeat { timestamp: 99 }).unwrap();

        let send = async {
            let mut stream = server.open_uni().await?;
            for byte in frame {
                stream.write_all(&[byte]).await?;
                tokio::task::yield_now().await;
            }
            stream.finish()?;
            Ok::<(), color_eyre::Report>(())
        };
        let receive = async {
            let mut stream = client.accept_uni().await?;
            framing::recv_message(&mut stream).await
        };
        let ((), message) = tokio::try_join!(send, receive).unwrap();

        assert!(matches!(
            message,
            Some(protocol::Message::Heartbeat { timestamp: 99 })
        ));
        client.close(0u32.into(), b"test complete");
        server.closed().await;
    }

    #[tokio::test]
    async fn quic_closure_mid_frame_is_not_a_clean_end_of_stream() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();

        let send = async {
            let mut stream = server.open_uni().await?;
            stream.write_all(&10u32.to_be_bytes()).await?;
            stream.write_all(&[1, 2, 3]).await?;
            stream.finish()?;
            Ok::<(), color_eyre::Report>(())
        };
        let receive = async {
            let mut stream = client.accept_uni().await?;
            framing::recv_message(&mut stream).await
        };
        let (send_result, result) = tokio::join!(send, receive);
        send_result.unwrap();

        assert!(result.unwrap_err().to_string().contains("mid-message body"));
        client.close(0u32.into(), b"test complete");
        server.closed().await;
    }

    #[tokio::test]
    async fn oversized_quic_frame_is_rejected_before_its_body() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();

        let send = async {
            let mut stream = server.open_uni().await?;
            stream
                .write_all(&(protocol::MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes())
                .await?;
            stream.finish()?;
            Ok::<(), color_eyre::Report>(())
        };
        let receive = async {
            let mut stream = client.accept_uni().await?;
            framing::recv_message(&mut stream).await
        };
        let (send_result, result) = tokio::join!(send, receive);
        send_result.unwrap();

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Message too large"));
        client.close(0u32.into(), b"test complete");
        server.closed().await;
    }

    #[tokio::test]
    async fn malformed_quic_payload_is_rejected_by_protocol_decode() {
        let fixture = QuicLoopback::new().unwrap();
        let (server, client) = fixture.connect().await.unwrap();

        let send = async {
            let mut stream = server.open_uni().await?;
            stream.write_all(&4u32.to_be_bytes()).await?;
            stream.write_all(&[0xff; 4]).await?;
            stream.finish()?;
            Ok::<(), color_eyre::Report>(())
        };
        let receive = async {
            let mut stream = client.accept_uni().await?;
            framing::recv_message(&mut stream).await
        };
        let (send_result, result) = tokio::join!(send, receive);
        send_result.unwrap();

        assert!(result.is_err());
        client.close(0u32.into(), b"test complete");
        server.closed().await;
    }
}
