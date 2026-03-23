use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use quinn::{Endpoint, RecvStream, SendStream};
use rand::Rng;
use tokio::sync::Mutex;
use tokio::time;
use tracing::{info, debug, warn, error};

use crate::net::protocol::{self, Message, PROTOCOL_VERSION, BUILD_VERSION, ScreenLayout};
use crate::net::tls;
use crate::net::discovery;
use crate::net::transition::{ServerTransition, ServerOutput, ClientTransition, ClientOutput};

const DEFAULT_PORT: u16 = 4242;
const MOUSE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Run a QUIC server that captures local mouse and sends events to clients.
pub async fn serve(port: u16, trigger_edge: Option<crate::net::protocol::Direction>) -> Result<()> {
    let server_config = tls::server_config()?;
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    info!("QUIC server listening on {}", addr);

    let (cert, _) = tls::load_or_generate_certs()?;
    let server_fingerprint = tls::fingerprint(&cert);
    info!("Fingerprint: {}", server_fingerprint);

    // Start mDNS advertisement (held for server lifetime)
    let _mdns_handle = discovery::start_advertising(port)?;

    // Generate a 6-digit OTP for pairing
    let otp = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
    println!("\n  Pairing code: {}\n", otp);

    // Periodically check for new releases and self-update
    tokio::spawn(crate::net::update::update_check_loop());

    while let Some(incoming) = endpoint.accept().await {
        let connection = incoming.await?;
        let remote = connection.remote_address();
        info!("New connection from {}", remote);

        let edge = trigger_edge;
        let otp = otp.clone();
        let fp = server_fingerprint.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_server_connection(connection, edge, &otp, &fp).await {
                error!("Connection from {} error: {}", remote, e);
            }
        });
    }

    Ok(())
}

async fn handle_server_connection(
    connection: quinn::Connection,
    trigger_edge: Option<crate::net::protocol::Direction>,
    server_otp: &str,
    server_fingerprint: &str,
) -> Result<()> {
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
        fingerprint: server_fingerprint.to_string(),
        build_version: Some(BUILD_VERSION.to_string()),
    };
    send_message(&mut control_send, &hello).await?;

    // Receive HelloAck with optional OTP
    let peer_screen = match recv_message(&mut control_recv).await? {
        Some(Message::HelloAck { accepted: true, otp, screen, build_version }) => {
            // Validate OTP if provided
            match otp {
                Some(code) => {
                    if code == server_otp {
                        info!("Peer {} paired successfully via OTP", remote);
                        let result = Message::PairingResult { success: true };
                        send_message(&mut control_send, &result).await?;
                    } else {
                        warn!("Peer {} provided invalid OTP", remote);
                        let result = Message::PairingResult { success: false };
                        send_message(&mut control_send, &result).await?;
                        return Err(eyre!("Invalid pairing code from {}", remote));
                    }
                }
                None => {
                    // Client already trusts us (fingerprint stored from previous pairing)
                    info!("Peer {} reconnected (already paired)", remote);
                    let result = Message::PairingResult { success: true };
                    send_message(&mut control_send, &result).await?;
                }
            }
            let peer_version = build_version.as_deref().unwrap_or("unknown");
            info!("Peer {} build version: {}", remote, peer_version);
            if peer_version != BUILD_VERSION {
                warn!("Version mismatch: server={}, client={}", BUILD_VERSION, peer_version);
            }
            screen.unwrap_or(ScreenLayout { width: 1920, height: 1080 })
        }
        Some(Message::HelloAck { accepted: false, .. }) => {
            return Err(eyre!("Peer rejected connection"));
        }
        other => {
            return Err(eyre!("Unexpected response: {:?}", other));
        }
    };

    // Open clipboard stream (bidirectional) — must be before uni stream
    // so client accept_bi() picks it up in order
    let (mut clip_send, mut clip_recv) = connection.open_bi().await?;
    // Send a Heartbeat as a "stream ready" marker so the client's accept_bi()
    // actually sees this stream (QUIC may not push an empty stream).
    let marker = Message::Heartbeat { timestamp: 0 };
    send_message(&mut clip_send, &marker).await?;
    debug!("Clipboard stream opened and marker sent");

    // Open unidirectional input stream (server → client)
    let mut input_send = connection.open_uni().await?;
    // Send a marker so QUIC pushes the stream creation to the client
    let input_marker = Message::Heartbeat { timestamp: 0 };
    send_message(&mut input_send, &input_marker).await?;
    let input_send = Arc::new(Mutex::new(input_send));
    debug!("Input stream opened and marker sent");

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

    // Try layer-shell capture first (wlroots compositors only)
    #[cfg(target_os = "linux")]
    let layer_shell = crate::input::wayland_layer_shell::try_create(
        trigger_edge.unwrap_or(crate::net::protocol::Direction::Right),
    ).ok().flatten();
    #[cfg(not(target_os = "linux"))]
    let layer_shell: Option<(
        tokio::sync::mpsc::UnboundedReceiver<crate::input::wayland_layer_shell::LayerShellEvent>,
        tokio::sync::mpsc::UnboundedSender<crate::input::wayland_layer_shell::LayerShellCommand>,
        u32, u32,
    )> = None;

    let (mut capture_rx, capture_tx, use_layer_shell) = match layer_shell {
        Some((rx, tx, _sw, _sh)) => {
            info!("Using layer-shell capture (zero-latency edge detection)");
            (Some(rx), Some(tx), true)
        }
        None => {
            info!("Using evdev polling capture");
            (None, None, false)
        }
    };

    // Input polling + edge detection + forwarding
    let capturer = Arc::new(std::sync::Mutex::new(capturer));
    info!("Peer screen: {}x{}", peer_screen.width, peer_screen.height);
    let mut transition = ServerTransition::new(trigger_edge, peer_screen);

    // Spawn file transfer acceptor (receives files from client via new bi-streams)
    let ft_conn = connection.clone();
    tokio::spawn(async move {
        loop {
            match ft_conn.accept_bi().await {
                Ok((send, recv)) => {
                    tokio::spawn(async move {
                        match crate::filetransfer::recv::receive_files(send, recv).await {
                            Ok(paths) if !paths.is_empty() => {
                                info!("Received {} file(s) from client", paths.len());
                                tokio::task::spawn_blocking(move || {
                                    crate::filetransfer::clipboard_files::set_clipboard_files(&paths).ok();
                                }).await.ok();
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("File transfer receive error: {}", e);
                            }
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });

    info!("Server ready. Move mouse to screen edge to start sharing.");
    info!("Screen size: {}x{}", screen_w, screen_h);

    let mut poll_interval = time::interval(MOUSE_POLL_INTERVAL);
    let mut debug_counter: u64 = 0;
    let mut last_screen_w = screen_w;
    let mut last_screen_h = screen_h;
    let mut screen_check = time::interval(Duration::from_secs(5));

    // Track input activity so we can wake the client display when the
    // server user returns after being idle.
    let mut prev_mouse_pos: (i32, i32) = (0, 0);
    let mut last_input_time = Instant::now();
    const IDLE_THRESHOLD: Duration = Duration::from_secs(30);

    loop {
        tokio::select! {
            // Branch: evdev polling (disabled when layer-shell is active)
            _ = poll_interval.tick(), if !use_layer_shell => {
                // Query input state while holding lock briefly.
                // poll_key_events() is called first because on Wayland (evdev)
                // it drains pending events and updates the cursor position.
                let (mx, my, sw, sh, buttons, key_events) = {
                    let mut cap = capturer.lock().unwrap();
                    let keys = cap.poll_key_events().unwrap_or_default();
                    let pos = cap.mouse_position().unwrap_or((0, 0));
                    let size = cap.screen_size().unwrap_or((1920, 1080));
                    let btns = cap.mouse_buttons().unwrap_or(0);
                    (pos.0, pos.1, size.0, size.1, btns, keys)
                };

                // Detect input activity after idle period
                let has_input = (mx, my) != prev_mouse_pos || !key_events.is_empty() || buttons != 0;
                if has_input {
                    let idle_duration = last_input_time.elapsed();
                    if idle_duration >= IDLE_THRESHOLD {
                        info!("User active after {}s idle — waking client display", idle_duration.as_secs());
                        let wake_msg = Message::WakeDisplay;
                        send_message(&mut control_send, &wake_msg).await.ok();
                    }
                    prev_mouse_pos = (mx, my);
                    last_input_time = Instant::now();
                }

                // Log position every 500 polls (~1 second)
                debug_counter += 1;
                if debug_counter % 500 == 0 {
                    let clamped_x = mx.clamp(0, sw as i32 - 1);
                    let clamped_y = my.clamp(0, sh as i32 - 1);
                    debug!("Mouse: ({}, {}) raw: ({}, {}) screen: {}x{}", clamped_x, clamped_y, mx, my, sw, sh);
                }

                match transition.poll(mx, my, sw, sh, buttons, key_events) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { messages, .. } => {
                        info!("Edge detected — switching to remote");
                        capturer.lock().unwrap().set_grab(true).ok();
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        // Check clipboard for files and transfer them
                        let ft_conn = connection.clone();
                        tokio::spawn(async move {
                            let files = tokio::task::spawn_blocking(|| {
                                crate::filetransfer::clipboard_files::get_clipboard_files()
                            }).await.ok().flatten();
                            if let Some(files) = files {
                                info!("Transferring {} clipboard file(s) to client", files.len());
                                if let Err(e) = crate::filetransfer::send::send_files(&ft_conn, files).await {
                                    warn!("File transfer error: {}", e);
                                }
                            }
                        });
                    }
                    ServerOutput::Forward { messages } => {
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                warn!("Failed to send: {}", e);
                                transition.deactivate();
                                capturer.lock().unwrap().set_grab(false).ok();
                                break;
                            }
                        }
                    }
                    ServerOutput::ForceRelease => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing grab");
                        capturer.lock().unwrap().set_grab(false).ok();
                    }
                }
            }
            // Branch: Layer-shell events (only when layer-shell is active)
            Some(event) = async {
                match &mut capture_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if use_layer_shell => {
                use crate::input::wayland_layer_shell::{LayerShellEvent, LayerShellCommand};

                // Detect input activity after idle period (layer-shell path)
                let idle_duration = last_input_time.elapsed();
                if idle_duration >= IDLE_THRESHOLD {
                    info!("User active after {}s idle — waking client display", idle_duration.as_secs());
                    let wake_msg = Message::WakeDisplay;
                    send_message(&mut control_send, &wake_msg).await.ok();
                }
                last_input_time = Instant::now();

                match event {
                    LayerShellEvent::EdgeEnter { direction } => {
                        let messages = transition.activate_instant(direction);
                        info!("Layer-shell edge enter ({:?}) — switching to remote", direction);
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        // Check clipboard for files and transfer them
                        let ft_conn = connection.clone();
                        tokio::spawn(async move {
                            let files = tokio::task::spawn_blocking(|| {
                                crate::filetransfer::clipboard_files::get_clipboard_files()
                            }).await.ok().flatten();
                            if let Some(files) = files {
                                info!("Transferring {} clipboard file(s) to client", files.len());
                                if let Err(e) = crate::filetransfer::send::send_files(&ft_conn, files).await {
                                    warn!("File transfer error: {}", e);
                                }
                            }
                        });
                    }
                    LayerShellEvent::MouseMove { dx, dy } => {
                        if transition.is_active() {
                            let msg = Message::MouseMove { x: dx as i32, y: dy as i32 };
                            let mut sender = input_send.lock().await;
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                warn!("Failed to send mouse move: {}", e);
                                transition.deactivate();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                            }
                        }
                    }
                    LayerShellEvent::MouseButton { button, pressed } => {
                        if transition.is_active() {
                            // Map evdev button codes to protocol button IDs
                            // evdev: BTN_LEFT=0x110, BTN_RIGHT=0x111, BTN_MIDDLE=0x112
                            let btn_id = match button {
                                0x110 => 0u8, // left
                                0x111 => 1,   // right
                                0x112 => 2,   // middle
                                _ => button as u8,
                            };
                            let msg = Message::MouseButton { button: btn_id, pressed };
                            let mut sender = input_send.lock().await;
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::MouseScroll { dx, dy } => {
                        if transition.is_active() {
                            let msg = Message::MouseScroll { dx: dx as i32, dy: dy as i32 };
                            let mut sender = input_send.lock().await;
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::KeyEvent { keycode, pressed } => {
                        if transition.is_active() {
                            transition.update_key(keycode, pressed);
                            if transition.is_escape_combo() {
                                warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                                transition.deactivate();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                            } else {
                                let msg = Message::KeyEvent { keycode, pressed, modifiers: 0 };
                                let mut sender = input_send.lock().await;
                                send_message_uni(&mut sender, &msg).await.ok();
                            }
                        }
                    }
                    LayerShellEvent::KeyModifiers { .. } => {
                        // Modifier state is tracked via KeyEvent; modifiers event is informational
                    }
                }
            }
            // Branch: Control stream messages
            msg = recv_message(&mut control_recv) => {
                match msg {
                    Ok(Some(Message::Heartbeat { timestamp })) => {
                        let ack = Message::HeartbeatAck { timestamp };
                        send_message(&mut control_send, &ack).await?;
                    }
                    Ok(Some(Message::SwitchScreen { direction })) => {
                        info!("Client requested switch back: {:?}", direction);
                        transition.on_switch_back();
                        if use_layer_shell {
                            #[cfg(target_os = "linux")]
                            if let Some(ref tx) = capture_tx {
                                use crate::input::wayland_layer_shell::LayerShellCommand;
                                tx.send(LayerShellCommand::Release).ok();
                            }
                        } else {
                            capturer.lock().unwrap().set_grab(false).ok();
                        }
                    }
                    Ok(Some(Message::ScreenResize { screen })) => {
                        info!("Peer screen updated: {}x{}", screen.width, screen.height);
                        transition.update_peer_screen(screen);
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
            _ = screen_check.tick() => {
                let size = capturer.lock().unwrap().screen_size().unwrap_or((last_screen_w, last_screen_h));
                if size.0 != last_screen_w || size.1 != last_screen_h {
                    info!("Screen size changed: {}x{} -> {}x{}", last_screen_w, last_screen_h, size.0, size.1);
                    last_screen_w = size.0;
                    last_screen_h = size.1;
                    let resize_msg = Message::ScreenResize {
                        screen: ScreenLayout { width: size.0, height: size.1 },
                    };
                    if let Err(e) = send_message(&mut control_send, &resize_msg).await {
                        warn!("Failed to send screen resize: {}", e);
                        break;
                    }
                }
            }
        }
    }

    // Always release grab when connection ends (client crash, disconnect, etc.)
    info!("Connection ended, releasing input grab");
    if use_layer_shell {
        #[cfg(target_os = "linux")]
        if let Some(ref tx) = capture_tx {
            use crate::input::wayland_layer_shell::LayerShellCommand;
            tx.send(LayerShellCommand::Shutdown).ok();
        }
    } else {
        capturer.lock().unwrap().set_grab(false).ok();
    }

    Ok(())
}

/// Connect to a QUIC server as a client (receives and injects input).
/// If `addr` is None, discovers the server via mDNS.
/// Automatically reconnects with exponential backoff on disconnection.
pub async fn connect(addr: Option<&str>) -> Result<()> {
    let resolved_addr = match addr {
        Some(a) => Some(resolve_addr(a)?),
        None => None,
    };

    let endpoint = make_client_endpoint()?;

    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        let target_addr = match resolved_addr {
            Some(a) => a,
            None => {
                match discovery::discover_one(Duration::from_secs(10)).await {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("Discovery failed: {}. Retrying in {:?}...", e, backoff);
                        time::sleep(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }
            }
        };

        let ep = endpoint.clone();
        let handle = tokio::spawn(async move { connect_once(&ep, target_addr).await });
        match handle.await {
            Ok(Ok(())) => {
                info!("Connection closed cleanly");
                backoff = Duration::from_secs(1);
            }
            Ok(Err(e)) => {
                warn!("Connection error: {}", e);
            }
            Err(join_err) => {
                error!("Connection panicked: {}", join_err);
            }
        }

        info!("Reconnecting in {:?}...", backoff);
        time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Perform a single client connection to the server, handling handshake with
/// OTP pairing and then running the input/clipboard loop until disconnection.
async fn connect_once(endpoint: &Endpoint, addr: SocketAddr) -> Result<()> {
    let connection = connect_with_retry(endpoint, addr).await?;

    info!("Connected to {}", addr);

    // Create input injector early so we can send screen size in handshake
    let mut injector = crate::input::inject::create_injector()?;
    let (my_w, my_h) = injector.screen_size()?;
    info!("Local screen: {}x{}", my_w, my_h);

    // Accept control stream and do handshake
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;

    let (mut server_screen, server_build_version) = match recv_message(&mut control_recv).await? {
        Some(Message::Hello { version, hostname, screen, fingerprint, build_version }) => {
            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );
            if server_ver != BUILD_VERSION {
                warn!("Version mismatch: server={}, client={}", server_ver, BUILD_VERSION);
            }

            // Check if we already trust this server's fingerprint
            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                info!("Server fingerprint already trusted");
                None
            } else {
                // Prompt user for pairing code
                let code = tokio::task::spawn_blocking(|| {
                    eprint!("Enter pairing code: ");
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input).ok();
                    input.trim().to_string()
                })
                .await
                .wrap_err("Failed to read pairing code")?;
                Some(code)
            };

            let ack = Message::HelloAck {
                accepted: true,
                otp: otp.clone(),
                screen: Some(ScreenLayout { width: my_w, height: my_h }),
                build_version: Some(BUILD_VERSION.to_string()),
            };
            send_message(&mut control_send, &ack).await?;

            // Wait for PairingResult
            match recv_message(&mut control_recv).await? {
                Some(Message::PairingResult { success: true }) => {
                    if otp.is_some() {
                        // First time pairing succeeded — store fingerprint
                        tls::trust_fingerprint(&fingerprint)?;
                        info!("Paired successfully. Fingerprint stored.");
                    }
                }
                Some(Message::PairingResult { success: false }) => {
                    return Err(eyre!("Pairing failed: invalid code"));
                }
                other => {
                    return Err(eyre!("Expected PairingResult, got: {:?}", other));
                }
            }

            let ver = build_version.unwrap_or_else(|| "unknown".to_string());
            (screen, ver)
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    };

    // Auto-update if server has a newer clean release version
    if server_build_version != BUILD_VERSION {
        if crate::net::update::is_release_version(&server_build_version) {
            info!("Server has release version {}, attempting self-update...", server_build_version);
            match crate::net::update::self_update(&server_build_version).await {
                Ok(()) => {
                    info!("Updated to {}. Restarting...", server_build_version);
                    connection.close(0u32.into(), b"updating");
                    let exe = std::env::current_exe()?;
                    let args: Vec<String> = std::env::args().collect();
                    use std::os::unix::process::CommandExt;
                    let err = std::process::Command::new(exe).args(&args[1..]).exec();
                    return Err(eyre!("Failed to restart after update: {}", err));
                }
                Err(e) => {
                    warn!("Self-update failed: {}. Continuing with current version.", e);
                }
            }
        }
    }

    let mut transition = ClientTransition::new(my_w, my_h);

    // Shutdown signal for clipboard tasks
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);

    // Accept clipboard stream (bidirectional, second bi-stream from server)
    let (clip_send, mut clip_recv) = tokio::time::timeout(
        Duration::from_secs(10),
        connection.accept_bi(),
    )
    .await
    .wrap_err("Timeout waiting for clipboard stream from server")?
    .wrap_err("Failed to accept clipboard stream")?;
    // Read and discard the stream-ready marker
    let _marker = recv_message(&mut clip_recv).await?;
    debug!("Clipboard stream accepted");

    // Spawn clipboard polling task (client → server)
    let clip_send = Arc::new(Mutex::new(clip_send));
    let clip_send_clone = clip_send.clone();
    let mut shutdown_rx1 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    if let Ok(Some(msg)) = clipboard.poll_change() {
                        let mut sender = clip_send_clone.lock().await;
                        if send_message(&mut sender, &msg).await.is_err() {
                            break;
                        }
                    }
                }
                _ = shutdown_rx1.changed() => {
                    break;
                }
            }
        }
    });

    // Spawn clipboard receive task (server → client)
    let mut shutdown_rx2 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut clipboard = crate::clipboard::sync::ClipboardSync::new();
        loop {
            tokio::select! {
                result = recv_message(&mut clip_recv) => {
                    match result {
                        Ok(Some(Message::ClipboardUpdate { content })) => {
                            clipboard.apply_update(&content).ok();
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
                _ = shutdown_rx2.changed() => {
                    break;
                }
            }
        }
    });

    // Spawn file transfer acceptor (receives files from server via new bi-streams)
    let ft_conn = connection.clone();
    let mut shutdown_rx3 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = ft_conn.accept_bi() => {
                    match result {
                        Ok((send, recv)) => {
                            tokio::spawn(async move {
                                match crate::filetransfer::recv::receive_files(send, recv).await {
                                    Ok(paths) if !paths.is_empty() => {
                                        info!("Received {} file(s) from server", paths.len());
                                        tokio::task::spawn_blocking(move || {
                                            crate::filetransfer::clipboard_files::set_clipboard_files(&paths).ok();
                                        }).await.ok();
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        warn!("File transfer receive error: {}", e);
                                    }
                                }
                            });
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx3.changed() => {
                    break;
                }
            }
        }
    });

    info!("Client ready. Waiting for server to share mouse...");

    // Accept the unidirectional input stream from the server
    let mut input_recv = tokio::time::timeout(
        Duration::from_secs(10),
        connection.accept_uni(),
    )
    .await
    .wrap_err("Timeout waiting for input stream from server")?
    .wrap_err("Failed to accept input stream")?;
    // Read and discard the stream-ready marker
    let _marker = recv_message_uni(&mut input_recv).await?;
    debug!("Input stream accepted");

    let mut last_screen_w = my_w;
    let mut last_screen_h = my_h;
    let mut screen_check = time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            msg = recv_message_uni(&mut input_recv) => {
                match msg {
                    Ok(Some(message)) => {
                        match transition.handle(message) {
                            ClientOutput::Ignore => {}
                            ClientOutput::Activate => {
                                info!("Server sharing mouse");
                                crate::input::wake::wake_display();
                            }
                            ClientOutput::InjectMove { x, y } => {
                                let msg = Message::MouseMove { x, y };
                                if let Err(e) = injector.inject(&msg) {
                                    warn!("Inject mouse move error: {}", e);
                                }
                            }
                            ClientOutput::Forward(msg) => {
                                if let Err(e) = injector.inject(&msg) {
                                    warn!("Inject error: {}", e);
                                }
                            }
                            ClientOutput::SwitchBack { direction, inject } => {
                                if let Some((x, y)) = inject {
                                    let msg = Message::MouseMove { x, y };
                                    injector.inject(&msg).ok();
                                }
                                info!("Edge on client: {:?} — requesting switch back", direction);
                                let switch_msg = Message::SwitchScreen { direction };
                                send_message(&mut control_send, &switch_msg).await.ok();
                                // Check clipboard for files and transfer them
                                let ft_conn = connection.clone();
                                tokio::spawn(async move {
                                    let files = tokio::task::spawn_blocking(|| {
                                        crate::filetransfer::clipboard_files::get_clipboard_files()
                                    }).await.ok().flatten();
                                    if let Some(files) = files {
                                        info!("Transferring {} clipboard file(s) to server", files.len());
                                        if let Err(e) = crate::filetransfer::send::send_files(&ft_conn, files).await {
                                            warn!("File transfer error: {}", e);
                                        }
                                    }
                                });
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
                        if let Err(e) = send_message(&mut control_send, &ack).await {
                            warn!("Failed to send heartbeat ack: {}", e);
                            break;
                        }
                    }
                    Ok(Some(Message::ScreenResize { screen })) => {
                        info!("Server screen changed: {}x{}", screen.width, screen.height);
                        server_screen = screen;
                    }
                    Ok(Some(Message::WakeDisplay)) => {
                        info!("Server user active — waking display");
                        crate::input::wake::wake_display();
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
            _ = screen_check.tick() => {
                if let Ok((w, h)) = injector.screen_size() {
                    if w != last_screen_w || h != last_screen_h {
                        info!("Screen size changed: {}x{} -> {}x{}", last_screen_w, last_screen_h, w, h);
                        last_screen_w = w;
                        last_screen_h = h;
                        transition.update_screen_size(w, h);
                        let resize_msg = Message::ScreenResize {
                            screen: ScreenLayout { width: w, height: h },
                        };
                        if let Err(e) = send_message(&mut control_send, &resize_msg).await {
                            warn!("Failed to send screen resize: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Signal clipboard tasks to shut down
    shutdown_tx.send(true).ok();

    // Gracefully close the connection
    connection.close(0u32.into(), b"disconnected");

    // Suppress unused variable warning
    let _ = server_screen;

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
        Some(Message::Hello { version, hostname, screen, fingerprint, build_version }) => {
            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );

            // Check if we already trust this server
            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                None
            } else {
                let code = tokio::task::spawn_blocking(|| {
                    eprint!("Enter pairing code: ");
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input).ok();
                    input.trim().to_string()
                })
                .await
                .wrap_err("Failed to read pairing code")?;
                Some(code)
            };

            let ack = Message::HelloAck { accepted: true, otp: otp.clone(), screen: None, build_version: Some(BUILD_VERSION.to_string()) };
            send_message(&mut send, &ack).await?;

            // Wait for PairingResult
            match recv_message(&mut recv).await? {
                Some(Message::PairingResult { success: true }) => {
                    if otp.is_some() {
                        tls::trust_fingerprint(&fingerprint)?;
                        info!("Paired successfully. Fingerprint stored.");
                    }
                }
                Some(Message::PairingResult { success: false }) => {
                    return Err(eyre!("Pairing failed: invalid code"));
                }
                other => {
                    return Err(eyre!("Expected PairingResult, got: {:?}", other));
                }
            }
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
