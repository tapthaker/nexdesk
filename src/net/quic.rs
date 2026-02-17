use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::sync::Mutex;
use tokio::time;
use tracing::{info, debug, warn, error};

use crate::cursor::edge;
use crate::net::protocol::{self, Message, PROTOCOL_VERSION, ScreenLayout};
use crate::net::tls;

const DEFAULT_PORT: u16 = 4242;
const MOUSE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Run a QUIC server that captures local mouse and sends events to clients.
pub async fn serve(port: u16) -> Result<()> {
    let server_config = tls::server_config()?;
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    info!("QUIC server listening on {}", addr);

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

    // Create input capturer
    let capturer = crate::input::capture::create_capturer()?;
    let (screen_w, screen_h) = capturer.screen_size()?;

    // Open control stream (bidirectional) — handshake
    let (mut control_send, mut control_recv) = connection.open_bi().await?;
    debug!("Control stream opened with {}", remote);

    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    let hello = Message::Hello {
        version: PROTOCOL_VERSION,
        hostname: hostname.clone(),
        screen: ScreenLayout {
            width: screen_w,
            height: screen_h,
        },
    };
    send_message(&mut control_send, &hello).await?;

    let peer_screen = match recv_message(&mut control_recv).await? {
        Some(Message::HelloAck { accepted: true }) => {
            info!("Peer {} accepted connection", remote);
            // For now use a default; the client sends its screen info separately
            ScreenLayout { width: 1920, height: 1080 }
        }
        Some(Message::HelloAck { accepted: false }) => {
            return Err(eyre!("Peer rejected connection"));
        }
        other => {
            return Err(eyre!("Unexpected response: {:?}", other));
        }
    };

    // Open unidirectional input stream (server → client)
    let input_send = connection.open_uni().await?;
    let input_send = Arc::new(Mutex::new(input_send));
    debug!("Input stream opened");

    // Open clipboard stream (bidirectional)
    let (clip_send, mut clip_recv) = connection.open_bi().await?;
    debug!("Clipboard stream opened");

    // Spawn clipboard polling task
    let clip_send = Arc::new(Mutex::new(clip_send));
    let clip_send_clone = clip_send.clone();
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(Some(msg)) = clipboard.poll_change() {
                let mut sender = clip_send_clone.lock().await;
                if send_message(&mut sender, &msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Spawn clipboard receive task
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        loop {
            match recv_message(&mut clip_recv).await {
                Ok(Some(Message::ClipboardUpdate { content })) => {
                    clipboard.apply_update(&content).ok();
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    // Input polling + edge detection + forwarding
    let capturer = Arc::new(std::sync::Mutex::new(capturer));
    let mut active = false;
    let mut last_x: i32 = 0;
    let mut last_y: i32 = 0;
    let mut last_buttons: u8 = 0;

    info!("Server ready. Move mouse to screen edge to start sharing.");

    let mut poll_interval = time::interval(MOUSE_POLL_INTERVAL);

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                // Query input state while holding lock briefly
                let (mx, my, sw, sh, buttons, key_events) = {
                    let mut cap = capturer.lock().unwrap();
                    let pos = cap.mouse_position().unwrap_or((0, 0));
                    let size = cap.screen_size().unwrap_or((1920, 1080));
                    let btns = cap.mouse_buttons().unwrap_or(0);
                    let keys = cap.poll_key_events().unwrap_or_default();
                    (pos.0, pos.1, size.0, size.1, btns, keys)
                };

                if !active {
                    if let Some(dir) = edge::detect_edge(mx, my, sw, sh) {
                        info!("Edge detected: {:?} — switching to remote", dir);
                        active = true;
                        last_buttons = buttons;
                        let msg = Message::SwitchScreen { direction: dir };
                        let mut sender = input_send.lock().await;
                        send_message_uni(&mut sender, &msg).await.ok();

                        let (rx, ry) = match dir {
                            crate::net::protocol::Direction::Right => (0, my),
                            crate::net::protocol::Direction::Left => (peer_screen.width as i32 - 1, my),
                            crate::net::protocol::Direction::Down => (mx, 0),
                            crate::net::protocol::Direction::Up => (mx, peer_screen.height as i32 - 1),
                        };
                        let move_msg = Message::MouseMove { x: rx, y: ry };
                        send_message_uni(&mut sender, &move_msg).await.ok();
                        last_x = mx;
                        last_y = my;
                    }
                } else {
                    let mut sender = input_send.lock().await;

                    // Mouse movement
                    let dx = mx - last_x;
                    let dy = my - last_y;
                    if dx != 0 || dy != 0 {
                        let msg = Message::MouseMove { x: dx, y: dy };
                        if let Err(e) = send_message_uni(&mut sender, &msg).await {
                            warn!("Failed to send mouse move: {}", e);
                            active = false;
                            continue;
                        }
                        last_x = mx;
                        last_y = my;
                    }

                    // Mouse button changes
                    if buttons != last_buttons {
                        for bit in 0..3u8 {
                            let was = (last_buttons >> bit) & 1 != 0;
                            let now = (buttons >> bit) & 1 != 0;
                            if was != now {
                                let msg = Message::MouseButton { button: bit, pressed: now };
                                send_message_uni(&mut sender, &msg).await.ok();
                            }
                        }
                        last_buttons = buttons;
                    }

                    // Keyboard events
                    for key_msg in key_events {
                        send_message_uni(&mut sender, &key_msg).await.ok();
                    }

                    drop(sender);

                    // Edge detection to switch back
                    if let Some(dir) = edge::detect_edge(mx, my, sw, sh) {
                        info!("Edge detected while active: {:?} — switching back to local", dir);
                        active = false;
                    }
                }
            }
            msg = recv_message(&mut control_recv) => {
                match msg {
                    Ok(Some(Message::Heartbeat { timestamp })) => {
                        let ack = Message::HeartbeatAck { timestamp };
                        send_message(&mut control_send, &ack).await?;
                    }
                    Ok(Some(Message::SwitchScreen { direction })) => {
                        info!("Client requested switch back: {:?}", direction);
                        active = false;
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
        }
    }

    Ok(())
}

/// Connect to a QUIC server as a client (receives and injects input).
pub async fn connect(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint()?;
    let connection = connect_with_retry(&endpoint, addr).await?;

    info!("Connected to {}", addr);

    // Accept control stream and do handshake
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;

    let _server_screen = match recv_message(&mut control_recv).await? {
        Some(Message::Hello { version, hostname, screen }) => {
            info!(
                "Server: {} (v{}, screen: {}x{})",
                hostname, version, screen.width, screen.height
            );
            let ack = Message::HelloAck { accepted: true };
            send_message(&mut control_send, &ack).await?;
            screen
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    };

    // Create input injector
    let mut injector = crate::input::inject::create_injector()?;
    let (my_w, my_h) = injector.screen_size()?;
    info!("Local screen: {}x{}", my_w, my_h);

    // Track absolute cursor position on our screen
    let mut cursor_x: i32 = my_w as i32 / 2;
    let mut cursor_y: i32 = my_h as i32 / 2;
    let mut active = false;

    // Accept clipboard stream (bidirectional, second bi-stream from server)
    let (clip_send, mut clip_recv) = connection.accept_bi().await?;
    debug!("Clipboard stream accepted");

    // Spawn clipboard polling task (client → server)
    let clip_send = Arc::new(Mutex::new(clip_send));
    let clip_send_clone = clip_send.clone();
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(Some(msg)) = clipboard.poll_change() {
                let mut sender = clip_send_clone.lock().await;
                if send_message(&mut sender, &msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Spawn clipboard receive task (server → client)
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        loop {
            match recv_message(&mut clip_recv).await {
                Ok(Some(Message::ClipboardUpdate { content })) => {
                    clipboard.apply_update(&content).ok();
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    info!("Client ready. Waiting for server to share mouse...");

    // Accept the unidirectional input stream from the server
    let mut input_recv = connection.accept_uni().await?;
    debug!("Input stream accepted");

    loop {
        tokio::select! {
            msg = recv_message_uni(&mut input_recv) => {
                match msg {
                    Ok(Some(message)) => {
                        match &message {
                            Message::SwitchScreen { direction } => {
                                info!("Server sharing mouse (direction: {:?})", direction);
                                active = true;
                            }
                            Message::MouseMove { x, y } if active => {
                                if !active { continue; }
                                // Relative movement from server
                                cursor_x += x;
                                cursor_y += y;
                                cursor_x = cursor_x.clamp(0, my_w as i32 - 1);
                                cursor_y = cursor_y.clamp(0, my_h as i32 - 1);

                                // Check if cursor hit edge on client side (switch back)
                                if let Some(dir) = edge::detect_edge(cursor_x, cursor_y, my_w, my_h) {
                                    info!("Edge on client: {:?} — requesting switch back", dir);
                                    active = false;
                                    let switch_msg = Message::SwitchScreen { direction: dir };
                                    send_message(&mut control_send, &switch_msg).await.ok();
                                    continue;
                                }

                                let move_msg = Message::MouseMove { x: cursor_x, y: cursor_y };
                                injector.inject(&move_msg).ok();
                            }
                            Message::MouseButton { .. } if active => {
                                injector.inject(&message).ok();
                            }
                            Message::MouseScroll { .. } if active => {
                                injector.inject(&message).ok();
                            }
                            Message::KeyEvent { .. } if active => {
                                injector.inject(&message).ok();
                            }
                            _ => {
                                debug!("Received (inactive): {:?}", message);
                            }
                        }
                    }
                    Ok(None) => {
                        info!("Input stream closed");
                        break;
                    }
                    Err(e) => {
                        warn!("Input stream error: {}", e);
                        break;
                    }
                }
            }
            msg = recv_message(&mut control_recv) => {
                match msg {
                    Ok(Some(Message::Heartbeat { timestamp })) => {
                        let ack = Message::HeartbeatAck { timestamp };
                        send_message(&mut control_send, &ack).await?;
                    }
                    Ok(Some(other)) => {
                        debug!("Control message: {:?}", other);
                    }
                    Ok(None) => {
                        info!("Server disconnected");
                        break;
                    }
                    Err(e) => {
                        warn!("Control stream error: {}", e);
                        break;
                    }
                }
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

async fn send_message_uni(send: &mut quinn::SendStream, msg: &Message) -> Result<()> {
    let bytes = protocol::encode(msg)?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn recv_message(recv: &mut RecvStream) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => eyre!("Connection closed mid-message"),
        other => other.into(),
    })?;

    let msg: Message = bincode::deserialize(&body)?;
    Ok(Some(msg))
}

async fn recv_message_uni(recv: &mut quinn::RecvStream) -> Result<Option<Message>> {
    recv_message(recv).await
}
