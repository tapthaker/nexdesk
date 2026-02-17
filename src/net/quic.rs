use std::net::SocketAddr;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::time;
use tracing::{info, debug, warn, error};

use crate::net::protocol::{self, Message, PROTOCOL_VERSION, ScreenLayout};
use crate::net::tls;

const DEFAULT_PORT: u16 = 4242;

/// Run a QUIC server that accepts connections.
pub async fn serve(port: u16) -> Result<()> {
    let server_config = tls::server_config()?;
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    info!("QUIC server listening on {}", addr);

    // Show fingerprint
    let (cert, _) = tls::load_or_generate_certs()?;
    info!("Fingerprint: {}", tls::fingerprint(&cert));

    while let Some(incoming) = endpoint.accept().await {
        let connection = incoming.await?;
        let remote = connection.remote_address();
        info!("New connection from {}", remote);

        tokio::spawn(async move {
            if let Err(e) = handle_server_connection(connection).await {
                error!("Connection from {} error: {}", remote, e);
            }
        });
    }

    Ok(())
}

async fn handle_server_connection(connection: quinn::Connection) -> Result<()> {
    let remote = connection.remote_address();

    // Open control stream (bidirectional)
    let (mut control_send, mut control_recv) = connection.open_bi().await?;
    debug!("Control stream opened with {}", remote);

    // Send Hello
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    let hello = Message::Hello {
        version: PROTOCOL_VERSION,
        hostname: hostname.clone(),
        screen: ScreenLayout {
            width: 1920,
            height: 1080,
        },
    };
    send_message(&mut control_send, &hello).await?;

    // Read HelloAck
    let response = recv_message(&mut control_recv).await?;
    match response {
        Some(Message::HelloAck { accepted: true }) => {
            info!("Peer {} accepted connection", remote);
        }
        Some(Message::HelloAck { accepted: false }) => {
            return Err(eyre!("Peer rejected connection"));
        }
        other => {
            return Err(eyre!("Unexpected response: {:?}", other));
        }
    }

    // Message loop: respond to heartbeats, handle other messages
    loop {
        match recv_message(&mut control_recv).await {
            Ok(Some(Message::Heartbeat { timestamp })) => {
                let ack = Message::HeartbeatAck { timestamp };
                send_message(&mut control_send, &ack).await?;
            }
            Ok(Some(other)) => {
                debug!("Received message: {:?}", other);
            }
            Ok(None) => {
                info!("Peer {} disconnected", remote);
                break;
            }
            Err(e) => {
                warn!("Error reading from {}: {}", remote, e);
                break;
            }
        }
    }

    Ok(())
}

/// Connect to a QUIC server.
pub async fn connect(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint()?;
    let connection = connect_with_retry(&endpoint, addr).await?;

    info!("Connected to {}", addr);

    // Accept control stream
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;

    // Read Hello
    let hello = recv_message(&mut control_recv).await?;
    match hello {
        Some(Message::Hello { version, hostname, screen }) => {
            info!(
                "Server: {} (v{}, screen: {}x{})",
                hostname, version, screen.width, screen.height
            );
            // Send HelloAck
            let ack = Message::HelloAck { accepted: true };
            send_message(&mut control_send, &ack).await?;
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    }

    // Heartbeat response loop
    loop {
        match recv_message(&mut control_recv).await {
            Ok(Some(Message::Heartbeat { timestamp })) => {
                let ack = Message::HeartbeatAck { timestamp };
                send_message(&mut control_send, &ack).await?;
                debug!("Responded to heartbeat");
            }
            Ok(Some(Message::HeartbeatAck { timestamp })) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis() as u64;
                debug!("Heartbeat RTT: {}ms", now.saturating_sub(timestamp));
            }
            Ok(Some(other)) => {
                debug!("Received: {:?}", other);
            }
            Ok(None) => {
                info!("Server disconnected");
                break;
            }
            Err(e) => {
                warn!("Connection error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Ping a peer to measure QUIC RTT.
pub async fn ping(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint()?;

    info!("Connecting to {}...", addr);
    let connection = connect_with_retry(&endpoint, addr).await?;

    // Accept the server's control stream and do the handshake
    let (mut send, mut recv) = connection.accept_bi().await?;

    let hello = recv_message(&mut recv).await?;
    match hello {
        Some(Message::Hello { version, hostname, screen }) => {
            info!(
                "Server: {} (v{}, screen: {}x{})",
                hostname, version, screen.width, screen.height
            );
            let ack = Message::HelloAck { accepted: true };
            send_message(&mut send, &ack).await?;
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    }

    info!("Sending pings...\n");

    for seq in 0..10 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;

        let start = Instant::now();
        let msg = Message::Heartbeat { timestamp: ts };
        send_message(&mut send, &msg).await?;

        match recv_message(&mut recv).await? {
            Some(Message::HeartbeatAck { timestamp: _ }) => {
                let rtt = start.elapsed();
                println!(
                    "  seq={} rtt={:.3}ms",
                    seq,
                    rtt.as_secs_f64() * 1000.0
                );
            }
            other => {
                warn!("Unexpected response: {:?}", other);
            }
        }

        if seq < 9 {
            time::sleep(Duration::from_millis(500)).await;
        }
    }

    println!();
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;

    Ok(())
}

fn resolve_addr(addr: &str) -> Result<SocketAddr> {
    if addr.contains(':') {
        addr.parse().wrap_err("Invalid socket address")
    } else {
        format!("{}:{}", addr, DEFAULT_PORT)
            .parse()
            .wrap_err("Invalid address")
    }
}

fn make_client_endpoint() -> Result<Endpoint> {
    let client_config = tls::client_config()?;
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

async fn connect_with_retry(endpoint: &Endpoint, addr: SocketAddr) -> Result<quinn::Connection> {
    let mut delay = Duration::from_millis(100);
    let max_delay = Duration::from_secs(10);
    let max_attempts = 5;

    for attempt in 1..=max_attempts {
        match endpoint.connect(addr, "nexdesk") {
            Ok(connecting) => match connecting.await {
                Ok(connection) => return Ok(connection),
                Err(e) => {
                    warn!(
                        "Connection attempt {}/{} failed: {}",
                        attempt, max_attempts, e
                    );
                }
            },
            Err(e) => {
                warn!(
                    "Connection attempt {}/{} failed: {}",
                    attempt, max_attempts, e
                );
            }
        }

        if attempt < max_attempts {
            info!("Retrying in {:?}...", delay);
            time::sleep(delay).await;
            delay = (delay * 2).min(max_delay);
        }
    }

    Err(eyre!("Failed to connect after {} attempts", max_attempts))
}

async fn send_message(send: &mut SendStream, msg: &Message) -> Result<()> {
    let bytes = protocol::encode(msg)?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn recv_message(recv: &mut RecvStream) -> Result<Option<Message>> {
    // Read length prefix
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;

    // Read message body
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => eyre!("Connection closed mid-message"),
        other => other.into(),
    })?;

    let msg: Message = bincode::deserialize(&body)?;
    Ok(Some(msg))
}
