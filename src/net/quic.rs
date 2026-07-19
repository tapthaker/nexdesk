use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Endpoint, RecvStream, SendStream};
use rand::Rng;
use tokio::sync::{Mutex, Semaphore};
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::input::inject::InputInjector;
use crate::net::discovery;
use crate::net::protocol::{self, Message, ScreenLayout, BUILD_VERSION, PROTOCOL_VERSION};
use crate::net::tls;
use crate::net::transition::{ClientOutput, ClientTransition, ServerOutput, ServerTransition};
use crate::status::{self, RuntimeStatus};

const DEFAULT_PORT: u16 = 4242;
const MOUSE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const USER_ACTIVITY_INTERVAL: Duration = Duration::from_secs(20);
const LOCAL_LOCK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CLIENT_LATENCY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const CLIENT_SCREEN_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CLIENT_LATENCY_RESTART_THRESHOLD: Duration = Duration::from_secs(3);
const CLIENT_LATENCY_RESTART_STRIKES: u8 = 3;
const MAX_CONCURRENT_FILE_TRANSFERS: usize = 2;
const MAX_CONNECT_ADDR_BYTES: usize = 512;
const SERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVE_SESSION_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const INPUT_SEND_TIMEOUT: Duration = Duration::from_secs(1);

// Only modifiers are safe to release defensively. Synthesizing an orphaned
// key-up for an ordinary key such as Space can trigger application behavior
// (for example, pausing a focused YouTube player) during a screen transition.
const DEFENSIVE_MODIFIER_KEYS: &[u32] = &[
    29,  // KEY_LEFTCTRL
    42,  // KEY_LEFTSHIFT
    54,  // KEY_RIGHTSHIFT
    56,  // KEY_LEFTALT
    97,  // KEY_RIGHTCTRL
    100, // KEY_RIGHTALT
    125, // KEY_LEFTMETA
    126, // KEY_RIGHTMETA
];

#[cfg(any(target_os = "linux", target_os = "macos"))]
static WAKE_DISPLAY_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn mark_wake_display_in_flight(flag: &std::sync::atomic::AtomicBool) -> bool {
    !flag.swap(true, std::sync::atomic::Ordering::AcqRel)
}

fn layer_shell_button_to_protocol(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0), // BTN_LEFT
        0x111 => Some(1), // BTN_RIGHT
        0x112 => Some(2), // BTN_MIDDLE
        _ => None,
    }
}

fn layer_shell_key_to_protocol(keycode: u32) -> Option<u32> {
    (keycode <= protocol::MAX_KEYCODE).then_some(keycode)
}

fn layer_shell_motion_delta(delta: f64) -> i32 {
    if !delta.is_finite() {
        0
    } else {
        delta.clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

fn layer_shell_scroll_delta(delta: f64) -> f64 {
    if !delta.is_finite() {
        0.0
    } else {
        delta.clamp(-protocol::MAX_SCROLL_DELTA, protocol::MAX_SCROLL_DELTA)
    }
}

fn track_injected_input(
    message: &Message,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) {
    match message {
        Message::KeyEvent {
            keycode, pressed, ..
        } => {
            if *pressed {
                injected_keys.insert(*keycode);
            } else {
                injected_keys.remove(keycode);
            }
        }
        Message::MouseButton { button, pressed } => {
            if *pressed {
                injected_buttons.insert(*button);
            } else {
                injected_buttons.remove(button);
            }
        }
        _ => {}
    }
}

fn request_wake_display() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::sync::atomic::Ordering;

        if !mark_wake_display_in_flight(&WAKE_DISPLAY_IN_FLIGHT) {
            return;
        }
        std::thread::spawn(|| {
            crate::input::wake::wake_display();
            WAKE_DISPLAY_IN_FLIGHT.store(false, Ordering::Release);
        });
    }
}

fn inject_with_timing(
    injector: &mut dyn InputInjector,
    msg: &Message,
    context: &str,
) -> Result<()> {
    let started = Instant::now();
    let result = injector.inject(msg);
    let elapsed = started.elapsed();
    if elapsed > Duration::from_millis(100) {
        warn!(
            "Slow input injection during {}: {:.0}ms ({})",
            context,
            elapsed.as_secs_f64() * 1000.0,
            protocol::message_summary(msg)
        );
    }
    result
}

struct ClientScreenRefresh {
    resize: Option<ScreenLayout>,
}

fn refresh_client_screen_snapshot(
    injector: &mut dyn InputInjector,
    transition: &mut ClientTransition,
    last_screen_w: &mut u32,
    last_screen_h: &mut u32,
    context: &str,
) -> Result<ClientScreenRefresh> {
    let (w, h) = injector.refresh_screen_size()?;
    let screen = nonzero_screen_layout(w, h)
        .ok_or_else(|| eyre!("Invalid refreshed client screen size: {}x{}", w, h))?;
    let size_changed = (w, h) != (*last_screen_w, *last_screen_h);
    let topology_changed = size_changed || injector.take_screen_geometry_changed();
    transition.update_screen_size(w, h);

    // A display reconfiguration can move the real pointer independently of
    // Nexdesk's modeled cursor. Reconcile immediately so a closed laptop lid
    // cannot leave edge detection operating on the removed display.
    if let Ok(Some((x, y))) = injector.cursor_position() {
        transition.sync_cursor_position(x, y);
    }

    let resize = size_changed.then_some(screen);
    let (cursor_x, cursor_y, _, _, active, return_edge, dwell, cooldown) = transition.diagnostics();
    if size_changed {
        info!(
            "Screen size refreshed {}: {}x{} -> {}x{}, active={}, cursor=({}, {}), return_edge={:?}, dwell={}, cooldown={}",
            context,
            *last_screen_w,
            *last_screen_h,
            w,
            h,
            active,
            cursor_x,
            cursor_y,
            return_edge,
            dwell,
            cooldown
        );
        *last_screen_w = w;
        *last_screen_h = h;
    } else if topology_changed {
        info!(
            "Screen topology refreshed {} without a size change: active={}, cursor=({}, {}), return_edge={:?}, dwell={}, cooldown={}",
            context, active, cursor_x, cursor_y, return_edge, dwell, cooldown
        );
    }

    Ok(ClientScreenRefresh { resize })
}

async fn announce_client_screen_refresh(
    control_send: &mut SendStream,
    refresh: &ClientScreenRefresh,
) -> Result<()> {
    if let Some(ref screen) = refresh.resize {
        send_message(
            control_send,
            &Message::ScreenResize {
                screen: screen.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

fn lock_recover<'a, T>(
    mutex: &'a std::sync::Mutex<T>,
    context: &str,
) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Recovering from poisoned {} mutex", context);
            poisoned.into_inner()
        }
    }
}

fn cleanup_keycodes(injected_keys: &HashSet<u32>) -> Vec<u32> {
    let mut keys: Vec<u32> = injected_keys
        .iter()
        .copied()
        .chain(DEFENSIVE_MODIFIER_KEYS.iter().copied())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn release_injected_inputs(
    injector: &mut dyn InputInjector,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) {
    if !injected_keys.is_empty() || !injected_buttons.is_empty() {
        request_wake_display();
    }

    let mut buttons: Vec<u8> = injected_buttons.iter().copied().collect();
    buttons.sort_unstable();
    for button in buttons {
        let msg = Message::MouseButton {
            button,
            pressed: false,
        };
        if let Err(e) = injector.inject(&msg) {
            warn!("Failed to release injected mouse button {}: {}", button, e);
        } else {
            injected_buttons.remove(&button);
        }
    }

    // Include modifiers defensively because a missed modifier release is very
    // disruptive. Ordinary keys are released only when their key-down was
    // tracked; orphaned ordinary key-ups are observable by some applications.
    for keycode in cleanup_keycodes(injected_keys) {
        let msg = Message::KeyEvent {
            keycode,
            pressed: false,
            modifiers: 0,
        };
        if let Err(e) = injector.inject(&msg) {
            warn!("Failed to release injected key {}: {}", keycode, e);
        }
        injected_keys.remove(&keycode);
    }
}

async fn send_user_activity(send: &mut SendStream, last_sent: &mut Instant) {
    if last_sent.elapsed() < USER_ACTIVITY_INTERVAL {
        return;
    }

    if send_message(send, &Message::WakeDisplay).await.is_ok() {
        *last_sent = Instant::now();
    }
}

fn validate_screen_layout(screen: &ScreenLayout, context: &str) -> Result<()> {
    if screen.width == 0 || screen.height == 0 {
        return Err(eyre!(
            "Invalid {} screen size: {}x{}",
            context,
            screen.width,
            screen.height
        ));
    }
    Ok(())
}

fn nonzero_screen_layout(width: u32, height: u32) -> Option<ScreenLayout> {
    if width == 0 || height == 0 {
        None
    } else {
        Some(ScreenLayout { width, height })
    }
}

fn sanitize_peer_hostname(value: &str) -> String {
    protocol::sanitize_display_string(value, protocol::MAX_PEER_NAME_BYTES, "nexdesk")
}

fn local_peer_hostname() -> String {
    sanitize_peer_hostname(&gethostname::gethostname().to_string_lossy())
}

fn unix_millis() -> u64 {
    system_time_millis_u64(std::time::SystemTime::now())
}

fn system_time_millis_u64(time: std::time::SystemTime) -> u64 {
    let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn normalize_pairing_input(input: &str) -> Result<String> {
    let code = input.trim().to_string();
    if code.is_empty() {
        return Err(eyre!("Pairing code cannot be empty"));
    }
    if code.len() != protocol::OTP_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(eyre!(
            "Invalid pairing code: expected {} decimal digits",
            protocol::OTP_DIGITS
        ));
    }
    Ok(code)
}

fn write_pairing_prompt(mut writer: impl Write) -> Result<()> {
    write!(writer, "Enter pairing code: ").wrap_err("Failed to write pairing prompt")?;
    writer.flush().wrap_err("Failed to flush pairing prompt")
}

async fn prompt_pairing_code(addr: SocketAddr) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(eyre!(
            "Server fingerprint is not trusted and no interactive terminal is available for pairing. Run `nexdesk connect {}` from a terminal once, enter the pairing code, then restart the background service.",
            addr
        ));
    }

    tokio::task::spawn_blocking(|| -> Result<String> {
        write_pairing_prompt(std::io::stderr())?;
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .wrap_err("Failed to read pairing code from stdin")?;
        normalize_pairing_input(&input)
    })
    .await
    .wrap_err("Pairing prompt task failed")?
}

fn restart_current_process() -> Result<()> {
    // Do not use exec(). macOS IOPM assertions are process-scoped and exec keeps
    // the same PID alive, so assertions created by the old image can leak across
    // self-updates/watchdog restarts. Exit cleanly and let LaunchAgent/systemd
    // restart the service with a fresh process.
    info!("Exiting for service-manager restart");
    std::process::exit(0);
}

fn validate_listen_port(port: u16) -> Result<()> {
    if port == 0 {
        return Err(eyre!(
            "Cannot serve on port 0 because peers and mDNS need a stable UDP port"
        ));
    }
    Ok(())
}

/// Run a QUIC server that captures local mouse and sends events to clients.
pub async fn serve(port: u16, trigger_edge: Option<crate::net::protocol::Direction>) -> Result<()> {
    validate_listen_port(port)?;
    let _idle_sleep_inhibitor = crate::input::wake::inhibit_idle_system_sleep();
    let server_config = tls::server_config()?;
    let (endpoint, addr) = make_server_endpoint(server_config, port)?;

    info!("QUIC server listening on {}", addr);
    let mut runtime = RuntimeStatus::new("server", "listening");
    runtime.local_addr = Some(addr.to_string());
    status::write_status(runtime).ok();

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

    // Input sharing is single-session, but authentication and diagnostic
    // connections must not consume this permit.
    let active_connection = Arc::new(Semaphore::new(1));

    while let Some(incoming) = endpoint.accept().await {
        let connection = match incoming.await {
            Ok(connection) => connection,
            Err(e) => {
                warn!("Incoming connection failed before handshake: {}", e);
                continue;
            }
        };
        let remote = connection.remote_address();
        info!("New connection from {}", remote);

        let edge = trigger_edge;
        let otp = otp.clone();
        let fp = server_fingerprint.clone();
        let active_connection = active_connection.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_server_connection(connection, edge, &otp, &fp, active_connection).await
            {
                error!("Connection from {} error: {}", remote, e);
            }
        });
    }

    Ok(())
}

async fn handle_diagnostic_connection(
    connection: quinn::Connection,
    mut control_send: SendStream,
    mut control_recv: RecvStream,
) -> Result<()> {
    loop {
        match recv_message(&mut control_recv).await? {
            Some(Message::Heartbeat { timestamp }) => {
                send_message(&mut control_send, &Message::HeartbeatAck { timestamp }).await?;
            }
            Some(other) => {
                debug!(
                    "Ignoring non-diagnostic control message: {}",
                    protocol::message_summary(&other)
                );
            }
            None => break,
        }
    }
    connection.close(0u32.into(), b"diagnostic complete");
    Ok(())
}

async fn handle_server_connection(
    connection: quinn::Connection,
    trigger_edge: Option<crate::net::protocol::Direction>,
    server_otp: &str,
    server_fingerprint: &str,
    active_connection: Arc<Semaphore>,
) -> Result<()> {
    let remote = connection.remote_address();
    let client_fingerprint = tls::peer_fingerprint(&connection);

    // Create input capturer
    let capturer = crate::input::capture::create_capturer()?;
    let (screen_w, screen_h) = capturer.screen_size()?;
    let local_screen = nonzero_screen_layout(screen_w, screen_h).ok_or_else(|| {
        eyre!(
            "Invalid local server screen size during handshake: {}x{}",
            screen_w,
            screen_h
        )
    })?;

    // Open control stream (bidirectional) — handshake
    let (mut control_send, mut control_recv) = connection.open_bi().await?;
    debug!("Control stream opened with {}", remote);

    let hostname = local_peer_hostname();
    let hello = Message::Hello {
        version: PROTOCOL_VERSION,
        hostname: hostname.clone(),
        screen: local_screen,
        fingerprint: server_fingerprint.to_string(),
        build_version: Some(protocol::local_build_version()),
    };
    send_message(&mut control_send, &hello).await?;

    // Receive HelloAck with optional OTP. A peer that opens QUIC but never
    // completes the application handshake must not hold resources forever.
    let hello_ack = time::timeout(SERVER_HANDSHAKE_TIMEOUT, recv_message(&mut control_recv))
        .await
        .wrap_err("Timed out waiting for client handshake")??;
    let (peer_screen, peer_build_version, input_capable) = match hello_ack {
        Some(Message::HelloAck {
            accepted: true,
            version,
            otp,
            screen,
            build_version,
        }) => {
            if version != PROTOCOL_VERSION {
                let result = Message::PairingResult { success: false };
                send_message(&mut control_send, &result).await?;
                return Err(eyre!(
                    "Protocol version mismatch: server={}, client={}",
                    PROTOCOL_VERSION,
                    version
                ));
            }
            // Validate OTP if provided
            match otp {
                Some(code) => {
                    if code == server_otp {
                        if let Some(ref fp) = client_fingerprint {
                            tls::trust_fingerprint(fp)?;
                            info!(
                                "Peer {} paired successfully via OTP; trusted client certificate",
                                remote
                            );
                        } else {
                            warn!("Peer {} paired without a client certificate; future reconnects will require OTP", remote);
                        }
                        let result = Message::PairingResult { success: true };
                        send_message(&mut control_send, &result).await?;
                    } else {
                        warn!("Peer {} provided invalid OTP", remote);
                        let result = Message::PairingResult { success: false };
                        send_message(&mut control_send, &result).await?;
                        return Err(eyre!("Invalid pairing code from {}", remote));
                    }
                }
                None => match client_fingerprint.as_deref() {
                    Some(fp) if tls::is_fingerprint_trusted(fp) => {
                        info!(
                            "Peer {} reconnected with trusted client certificate",
                            remote
                        );
                        let result = Message::PairingResult { success: true };
                        send_message(&mut control_send, &result).await?;
                    }
                    Some(fp) => {
                        warn!(
                            "Peer {} omitted OTP and has untrusted client certificate {}",
                            remote, fp
                        );
                        let result = Message::PairingResult { success: false };
                        send_message(&mut control_send, &result).await?;
                        return Err(eyre!("Untrusted client certificate from {}", remote));
                    }
                    None => {
                        warn!(
                            "Peer {} omitted OTP and did not present a client certificate",
                            remote
                        );
                        let result = Message::PairingResult { success: false };
                        send_message(&mut control_send, &result).await?;
                        return Err(eyre!("Missing OTP from untrusted peer {}", remote));
                    }
                },
            }
            if let Some(ref screen) = screen {
                validate_screen_layout(screen, "peer")?;
            }
            let peer_version = build_version.as_deref().unwrap_or("unknown");
            info!("Peer {} build version: {}", remote, peer_version);
            if peer_version != BUILD_VERSION {
                warn!(
                    "Version mismatch: server={}, client={}",
                    BUILD_VERSION, peer_version
                );
            }
            let input_capable = screen.is_some();
            (
                screen.unwrap_or(ScreenLayout {
                    width: 1920,
                    height: 1080,
                }),
                peer_version.to_string(),
                input_capable,
            )
        }
        Some(Message::HelloAck {
            accepted: false, ..
        }) => {
            return Err(eyre!("Peer rejected connection"));
        }
        other => {
            return Err(eyre!(
                "Unexpected response: {}",
                protocol::optional_message_summary(other.as_ref())
            ));
        }
    };

    // Pairing and ping connections intentionally omit a client screen. They
    // can authenticate and use the control stream without competing for the
    // single input-sharing session.
    if !input_capable {
        return handle_diagnostic_connection(connection, control_send, control_recv).await;
    }

    // Wait for a previous input session to unwind during reconnect rather than
    // rejecting the new authenticated peer immediately. The bounded wait keeps
    // stale sessions from blocking service indefinitely.
    let _connection_permit = time::timeout(
        ACTIVE_SESSION_WAIT_TIMEOUT,
        active_connection.acquire_owned(),
    )
    .await
    .wrap_err("Timed out waiting for the previous input session to close")?
    .map_err(|_| eyre!("Input session gate closed"))?;

    // Open clipboard stream (bidirectional) — must be before uni stream
    // so client accept_bi() picks it up in order
    let (mut clip_send, mut clip_recv) = connection.open_bi().await?;
    // Send a Heartbeat as a "stream ready" marker so the client's accept_bi()
    // actually sees this stream (QUIC may not push an empty stream).
    let marker = Message::Heartbeat { timestamp: 0 };
    send_message(&mut clip_send, &marker).await?;
    info!("Clipboard stream opened");

    // Open unidirectional input stream (server → client)
    let mut input_send = connection.open_uni().await?;
    // Send a marker so QUIC pushes the stream creation to the client
    let input_marker = Message::Heartbeat { timestamp: 0 };
    send_message(&mut input_send, &input_marker).await?;
    let input_send = Arc::new(Mutex::new(input_send));
    debug!("Input stream opened and marker sent");

    // Keep framed reads in a dedicated task. recv_message() uses read_exact(),
    // which must not be cancelled by the 2ms input polling branch below after
    // it has consumed part of a frame.
    let mut control_messages = spawn_message_reader(control_recv);

    // Shutdown signal for background tasks tied to this server connection.
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);

    // Spawn clipboard polling task
    let clip_send = Arc::new(Mutex::new(clip_send));
    let clip_send_clone = clip_send.clone();
    let clipboard = Arc::new(std::sync::Mutex::new(
        crate::clipboard::sync::ClipboardSync::new(),
    ));
    let clipboard_poll = clipboard.clone();
    let mut shutdown_rx1 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let msg = {
                        let mut clipboard = lock_recover(&clipboard_poll, "clipboard");
                        clipboard.poll_change()
                    };
                    if let Ok(Some(msg)) = msg {
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

    // Spawn clipboard receive task
    let clipboard_recv = clipboard.clone();
    let mut shutdown_rx2 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = recv_message(&mut clip_recv) => {
                    match result {
                        Ok(Some(Message::ClipboardUpdate { content })) => {
                            let mut clipboard = lock_recover(&clipboard_recv, "clipboard");
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

    // Try layer-shell capture first (wlroots compositors only)
    #[cfg(target_os = "linux")]
    let layer_shell = crate::input::wayland_layer_shell::try_create(
        trigger_edge.unwrap_or(crate::net::protocol::Direction::Right),
    )
    .ok()
    .flatten();
    #[cfg(not(target_os = "linux"))]
    let layer_shell: Option<(
        tokio::sync::mpsc::UnboundedReceiver<crate::input::wayland_layer_shell::LayerShellEvent>,
        tokio::sync::mpsc::UnboundedSender<crate::input::wayland_layer_shell::LayerShellCommand>,
        u32,
        u32,
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
    let mut runtime = RuntimeStatus::new("server", "connected");
    runtime.peer_addr = Some(remote.to_string());
    runtime.peer_screen = Some(format!("{}x{}", peer_screen.width, peer_screen.height));
    runtime.peer_build = Some(peer_build_version);
    status::write_status(runtime).ok();
    let mut transition = ServerTransition::new(trigger_edge, peer_screen);

    // Spawn file transfer acceptor (receives files from client via new bi-streams)
    let ft_conn = connection.clone();
    let ft_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILE_TRANSFERS));
    let mut shutdown_rx3 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = ft_conn.accept_bi() => {
                    match result {
                        Ok((send, recv)) => {
                            let Ok(permit) = ft_semaphore.clone().try_acquire_owned() else {
                                warn!(
                                    "Rejecting incoming file transfer: too many concurrent transfers (max {})",
                                    MAX_CONCURRENT_FILE_TRANSFERS
                                );
                                continue;
                            };
                            tokio::spawn(async move {
                                let _permit = permit;
                                match crate::filetransfer::recv::receive_files(send, recv).await {
                                    Ok(paths) if !paths.is_empty() => {
                                        info!("Received {} file(s) from client", paths.len());
                                        tokio::task::spawn_blocking(move || {
                                            crate::filetransfer::clipboard_files::set_clipboard_files(&paths).ok();
                                        })
                                        .await
                                        .ok();
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

    info!("Server ready. Move mouse to screen edge to start sharing.");
    info!("Screen size: {}x{}", screen_w, screen_h);

    let mut poll_interval = time::interval(MOUSE_POLL_INTERVAL);
    let mut layer_shell_key_poll_interval = time::interval(MOUSE_POLL_INTERVAL);
    let mut debug_counter: u64 = 0;
    let mut last_screen_w = screen_w;
    let mut last_screen_h = screen_h;
    let mut screen_check = time::interval(Duration::from_secs(5));
    let mut local_lock_check = time::interval(LOCAL_LOCK_CHECK_INTERVAL);

    let mut prev_mouse_pos: (i32, i32) = (0, 0);
    let mut last_user_activity_sent = Instant::now() - USER_ACTIVITY_INTERVAL;

    // Track scroll gesture state for proper Began/Changed/Ended phases.
    let mut scroll_active = false;
    let mut layer_shell_keyboard_grabbed = false;

    loop {
        tokio::select! {
            // Branch: evdev polling (disabled when layer-shell is active)
            _ = poll_interval.tick(), if !use_layer_shell => {
                // Query input state while holding lock briefly.
                // poll_key_events() is called first because on Wayland (evdev)
                // it drains pending events and updates the cursor position.
                let (mx, my, sw, sh, buttons, key_events) = {
                    let mut cap = lock_recover(&capturer, "input capturer");
                    let keys = cap.poll_key_events().unwrap_or_default();
                    let pos = cap.mouse_position().unwrap_or((0, 0));
                    let size = cap.screen_size().unwrap_or((1920, 1080));
                    let btns = cap.mouse_buttons().unwrap_or(0);
                    (pos.0, pos.1, size.0, size.1, btns, keys)
                };

                let has_input = (mx, my) != prev_mouse_pos || !key_events.is_empty() || buttons != 0;
                if has_input {
                    send_user_activity(&mut control_send, &mut last_user_activity_sent).await;
                    prev_mouse_pos = (mx, my);
                }

                // Log position every 500 polls (~1 second)
                debug_counter += 1;
                if debug_counter.is_multiple_of(500) {
                    if sw == 0 || sh == 0 {
                        debug!("Mouse: raw: ({}, {}) invalid screen: {}x{}", mx, my, sw, sh);
                    } else {
                        let clamped_x = mx.clamp(0, sw as i32 - 1);
                        let clamped_y = my.clamp(0, sh as i32 - 1);
                        debug!("Mouse: ({}, {}) raw: ({}, {}) screen: {}x{}", clamped_x, clamped_y, mx, my, sw, sh);
                    }
                }

                match transition.poll(mx, my, sw, sh, buttons, key_events) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { messages, grab } => {
                        info!("Edge detected — switching to remote");
                        lock_recover(&capturer, "input capturer").set_grab(grab).ok();
                        let mut sender = input_send.lock().await;
                        let mut send_failed = None;
                        for msg in messages {
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                send_failed = Some(e);
                                break;
                            }
                        }
                        drop(sender);
                        if let Some(e) = send_failed {
                            warn!("Failed to activate remote screen: {}; releasing local input", e);
                            transition.deactivate();
                            lock_recover(&capturer, "input capturer").set_grab(false).ok();
                            connection.close(0u32.into(), b"input stream stalled");
                            continue;
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
                                lock_recover(&capturer, "input capturer").set_grab(false).ok();
                                connection.close(0u32.into(), b"input stream stalled");
                                break;
                            }
                        }
                    }
                    ServerOutput::ShortcutRelease { messages } => {
                        info!("Shortcut switch back — releasing grab");
                        // Local recovery must precede network I/O: a sleeping
                        // Mac may stop reading while Linux still owns the grab.
                        lock_recover(&capturer, "input capturer").set_grab(false).ok();
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    ServerOutput::ForceRelease { messages } => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing grab");
                        lock_recover(&capturer, "input capturer").set_grab(false).ok();
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                }
            }
            // Branch: keyboard polling for layer-shell mode. Wayland compositors
            // can consume global shortcuts and media keys before wl_keyboard sees
            // them, so use evdev while the remote screen is active.
            _ = layer_shell_key_poll_interval.tick(), if use_layer_shell && layer_shell_keyboard_grabbed && transition.is_active() => {
                let key_events: Vec<Message> = {
                    let mut cap = lock_recover(&capturer, "input capturer");
                    cap.poll_key_events_only()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|msg| matches!(msg, Message::KeyEvent { .. }))
                        .collect()
                };

                if !key_events.is_empty() {
                    send_user_activity(&mut control_send, &mut last_user_activity_sent).await;
                }

                match transition.poll_active_keys(key_events) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { .. } => {}
                    ServerOutput::ShortcutRelease { messages } => {
                        info!("Shortcut switch back — releasing layer-shell grab");
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    ServerOutput::Forward { messages } => {
                        if !messages.is_empty() {
                            let mut sender = input_send.lock().await;
                            for msg in messages {
                                if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                    warn!("Failed to send key event: {}", e);
                                    transition.deactivate();
                                    if let Some(ref tx) = capture_tx {
                                        use crate::input::wayland_layer_shell::LayerShellCommand;
                                        tx.send(LayerShellCommand::Release).ok();
                                    }
                                    lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                                    layer_shell_keyboard_grabbed = false;
                                    connection.close(0u32.into(), b"input stream stalled");
                                    break;
                                }
                            }
                        }
                    }
                    ServerOutput::ForceRelease { messages } => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
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
                send_user_activity(&mut control_send, &mut last_user_activity_sent).await;

                match event {
                    LayerShellEvent::EdgeEnter { direction } => {
                        if !transition.edge_is_armed() {
                            info!(
                                "Ignoring layer-shell edge enter ({:?}) until pointer leaves the reclaim edge",
                                direction
                            );
                            if let Some(ref tx) = capture_tx {
                                tx.send(LayerShellCommand::Release).ok();
                            }
                            continue;
                        }
                        let messages = transition.activate_instant(direction);
                        info!("Layer-shell edge enter ({:?}) — switching to remote", direction);
                        match lock_recover(&capturer, "input capturer").set_keyboard_grab(true) {
                            Ok(()) => {
                                layer_shell_keyboard_grabbed = true;
                            }
                            Err(e) => {
                                warn!("Failed to grab keyboard devices for layer-shell capture: {}; falling back to layer-shell keyboard focus", e);
                                layer_shell_keyboard_grabbed = false;
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::CaptureKeyboard).ok();
                                }
                            }
                        }
                        let mut sender = input_send.lock().await;
                        let mut send_failed = None;
                        for msg in messages {
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                send_failed = Some(e);
                                break;
                            }
                        }
                        drop(sender);
                        if let Some(e) = send_failed {
                            warn!("Failed to activate layer-shell remote screen: {}; releasing local input", e);
                            transition.deactivate();
                            if let Some(ref tx) = capture_tx {
                                tx.send(LayerShellCommand::Release).ok();
                            }
                            lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                            layer_shell_keyboard_grabbed = false;
                            connection.close(0u32.into(), b"input stream stalled");
                            continue;
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
                    LayerShellEvent::EdgeLeave => {
                        let was_armed = transition.edge_is_armed();
                        transition.rearm_edge();
                        if !was_armed && transition.edge_is_armed() {
                            info!("Layer-shell transfer edge rearmed after pointer left it");
                        }
                    }
                    LayerShellEvent::GrabLost => {
                        warn!("Layer-shell pointer grab was lost — reclaiming Linux input");
                        if transition.is_active() {
                            let messages = transition.reset_to_local();
                            // Recover local keyboard ownership before any network await.
                            lock_recover(&capturer, "input capturer")
                                .set_keyboard_grab(false)
                                .ok();
                            layer_shell_keyboard_grabbed = false;

                            let mut sender = input_send.lock().await;
                            for msg in messages {
                                if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                    warn!("Failed to notify client after pointer grab loss: {}", e);
                                    connection.close(0u32.into(), b"pointer grab lost");
                                    break;
                                }
                            }
                        }
                    }
                    LayerShellEvent::MouseMove { dx, dy } => {
                        if transition.is_active() {
                            let msg = Message::MouseMove {
                                x: layer_shell_motion_delta(dx),
                                y: layer_shell_motion_delta(dy),
                            };
                            let mut sender = input_send.lock().await;
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                warn!("Failed to send mouse move: {}", e);
                                transition.deactivate();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                                lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                                layer_shell_keyboard_grabbed = false;
                                connection.close(0u32.into(), b"input stream stalled");
                            }
                        }
                    }
                    LayerShellEvent::MouseButton { button, pressed } => {
                        if transition.is_active() {
                            let Some(btn_id) = layer_shell_button_to_protocol(button) else {
                                debug!("Ignoring unsupported layer-shell pointer button: {}", button);
                                continue;
                            };
                            let msg = Message::MouseButton { button: btn_id, pressed };
                            let mut sender = input_send.lock().await;
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::MouseScroll { dx, dy } => {
                        if transition.is_active() {
                            let dx = layer_shell_scroll_delta(dx);
                            let dy = layer_shell_scroll_delta(dy);
                            let phase = if scroll_active {
                                crate::net::protocol::ScrollPhase::Changed
                            } else {
                                scroll_active = true;
                                crate::net::protocol::ScrollPhase::Began
                            };
                            let msg = Message::MouseScroll { dx, dy, phase };
                            let mut sender = input_send.lock().await;
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::ScrollEnd => {
                        if transition.is_active() && scroll_active {
                            scroll_active = false;
                            let msg = Message::MouseScroll {
                                dx: 0.0, dy: 0.0,
                                phase: crate::net::protocol::ScrollPhase::Ended,
                            };
                            let mut sender = input_send.lock().await;
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::KeyEvent { keycode, pressed } => {
                        if transition.is_active() && !layer_shell_keyboard_grabbed {
                            let Some(keycode) = layer_shell_key_to_protocol(keycode) else {
                                debug!("Ignoring unsupported layer-shell keycode: {}", keycode);
                                continue;
                            };
                            transition.update_key(keycode, pressed);
                            if transition.is_escape_combo() {
                                warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                                let messages = transition.deactivate_for_shortcut();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                                let mut sender = input_send.lock().await;
                                for msg in messages {
                                    send_message_uni(&mut sender, &msg).await.ok();
                                }
                            } else if transition.shortcut_direction().is_some() {
                                info!("Shortcut switch back — releasing layer-shell grab");
                                let messages = transition.deactivate_for_shortcut();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                                lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                                layer_shell_keyboard_grabbed = false;
                                let mut sender = input_send.lock().await;
                                for msg in messages {
                                    send_message_uni(&mut sender, &msg).await.ok();
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
            msg = control_messages.recv() => {
                match msg.unwrap_or(Ok(None)) {
                    Ok(Some(Message::Heartbeat { timestamp })) => {
                        let ack = Message::HeartbeatAck { timestamp };
                        send_message(&mut control_send, &ack).await?;
                    }
                    Ok(Some(control @ Message::SwitchScreen { .. }))
                    | Ok(Some(control @ Message::ReleaseScreen)) => {
                        request_wake_display();
                        match control {
                            Message::SwitchScreen { direction } => {
                                info!("Client requested switch back: {:?}", direction);
                            }
                            Message::ReleaseScreen => {
                                warn!("Client requested fail-safe ownership reset");
                            }
                            _ => unreachable!(),
                        }
                        // Echo ReleaseScreen back to the client as an ownership
                        // reset acknowledgement, followed by held-key releases.
                        let messages = transition.reset_to_local();

                        // Restore Linux input ownership before waiting on the
                        // stream back to the Mac. During lid/display changes
                        // that stream can stall, but local input must recover.
                        if use_layer_shell {
                            #[cfg(target_os = "linux")]
                            if let Some(ref tx) = capture_tx {
                                use crate::input::wayland_layer_shell::LayerShellCommand;
                                tx.send(LayerShellCommand::Release).ok();
                            }
                            lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                            layer_shell_keyboard_grabbed = false;
                        } else {
                            lock_recover(&capturer, "input capturer").set_grab(false).ok();
                        }

                        info!("Local input ownership restored; acknowledging client reset");
                        if !messages.is_empty() {
                            let mut sender = input_send.lock().await;
                            for msg in messages {
                                send_message_uni(&mut sender, &msg).await.ok();
                            }
                        }
                    }
                    Ok(Some(Message::ScreenResize { screen })) => {
                        if validate_screen_layout(&screen, "peer resize").is_ok() {
                            info!("Peer screen updated: {}x{}", screen.width, screen.height);
                            // A resize updates future entry placement but does not
                            // transfer ownership. The active client reconciles its
                            // real cursor against the new geometry itself.
                            transition.update_peer_screen(screen);
                        } else {
                            warn!("Ignoring invalid peer screen resize: {}x{}", screen.width, screen.height);
                        }
                    }
                    Ok(Some(other)) => {
                        debug!("Received message: {}", protocol::message_summary(&other));
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
            _ = local_lock_check.tick(), if transition.is_active() => {
                if crate::input::session::is_session_locked() {
                    warn!("Linux session locked while sharing — switching control back to the server");
                    let messages = transition.reset_to_local();

                    // Local recovery must happen before any network await. If the
                    // sleeping/locked peer has stopped reading QUIC, notifying it
                    // can block, but the Linux keyboard must already be available
                    // for password entry.
                    if use_layer_shell {
                        #[cfg(target_os = "linux")]
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        lock_recover(&capturer, "input capturer").set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                    } else {
                        lock_recover(&capturer, "input capturer").set_grab(false).ok();
                    }
                    request_wake_display();

                    let mut sender = input_send.lock().await;
                    for msg in messages {
                        send_message_uni(&mut sender, &msg).await.ok();
                    }
                }
            }
            _ = screen_check.tick() => {
                let size = lock_recover(&capturer, "input capturer").screen_size().unwrap_or((last_screen_w, last_screen_h));
                if size.0 != last_screen_w || size.1 != last_screen_h {
                    let Some(screen) = nonzero_screen_layout(size.0, size.1) else {
                        warn!("Ignoring invalid local screen resize: {}x{}", size.0, size.1);
                        continue;
                    };
                    info!("Screen size changed: {}x{} -> {}x{}", last_screen_w, last_screen_h, size.0, size.1);
                    last_screen_w = size.0;
                    last_screen_h = size.1;
                    let resize_msg = Message::ScreenResize { screen };
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
    let mut runtime = RuntimeStatus::new("server", "listening");
    runtime.peer_addr = Some(format!("last disconnected: {}", remote));
    status::write_status(runtime).ok();
    if use_layer_shell {
        #[cfg(target_os = "linux")]
        if let Some(ref tx) = capture_tx {
            use crate::input::wayland_layer_shell::LayerShellCommand;
            tx.send(LayerShellCommand::Shutdown).ok();
        }
        lock_recover(&capturer, "input capturer")
            .set_keyboard_grab(false)
            .ok();
    } else {
        lock_recover(&capturer, "input capturer")
            .set_grab(false)
            .ok();
    }

    shutdown_tx.send(true).ok();
    connection.close(0u32.into(), b"disconnected");

    Ok(())
}

fn explicit_connect_addr_arg(addr: Option<&str>) -> Result<Option<String>> {
    addr.map(normalize_connect_addr_input)
        .transpose()
        .map(|addr| addr.map(str::to_string))
}

/// Connect to a QUIC server as a client (receives and injects input).
/// If `addr` is None, discovers the server via mDNS.
/// Automatically reconnects with exponential backoff on disconnection.
pub async fn connect(addr: Option<&str>) -> Result<()> {
    let explicit_addr = explicit_connect_addr_arg(addr)?;
    let _idle_sleep_inhibitor = crate::input::wake::inhibit_idle_system_sleep();

    loop {
        let target_addr = match explicit_addr.as_deref() {
            Some(a) => match resolve_addr(a) {
                Ok(addr) => addr,
                Err(e) => {
                    warn!(
                        "Failed to resolve {}: {}. Retrying in 2s...",
                        safe_connect_addr_for_error(a),
                        e
                    );
                    time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            None => match discovery::discover_one(Duration::from_secs(10)).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("Discovery failed: {}. Retrying in 2s...", e);
                    time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
        };

        info!("Connecting to nexdesk server at {}", target_addr);
        let endpoint = match make_client_endpoint(target_addr) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                warn!("Failed to create client endpoint: {}. Retrying in 2s...", e);
                time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut runtime = RuntimeStatus::new("client", "connecting");
        runtime.peer_addr = Some(target_addr.to_string());
        status::write_status(runtime).ok();
        let ep = endpoint.clone();
        let handle = tokio::spawn(async move { connect_once(&ep, target_addr).await });
        match handle.await {
            Ok(Ok(())) => info!("Connection closed cleanly"),
            Ok(Err(e)) => warn!("Connection error: {}", e),
            Err(join_err) => error!("Connection panicked: {}", join_err),
        }
        let mut runtime = RuntimeStatus::new("client", "disconnected");
        runtime.peer_addr = Some(target_addr.to_string());
        status::write_status(runtime).ok();

        info!("Reconnecting in 2s...");
        time::sleep(Duration::from_secs(2)).await;
    }
}

/// Perform a single client connection to the server, handling handshake with
/// OTP pairing and then running the input/clipboard loop until disconnection.
async fn connect_once(endpoint: &Endpoint, addr: SocketAddr) -> Result<()> {
    let connection = connect_with_retry(endpoint, addr).await?;
    let tls_fingerprint = tls::peer_fingerprint(&connection)
        .ok_or_else(|| eyre!("Server did not present a certificate"))?;

    info!("Connected to {}", addr);
    let mut runtime = RuntimeStatus::new("client", "connected");
    runtime.peer_addr = Some(addr.to_string());
    status::write_status(runtime).ok();

    // Create input injector early so we can send screen size in handshake
    let mut injector = crate::input::inject::create_injector()?;
    let (my_w, my_h) = injector.refresh_screen_size()?;
    let my_screen = nonzero_screen_layout(my_w, my_h).ok_or_else(|| {
        eyre!(
            "Invalid local client screen size during handshake: {}x{}",
            my_w,
            my_h
        )
    })?;
    info!("Local screen: {}x{}", my_w, my_h);

    // Accept control stream and do handshake
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;

    let (mut server_screen, server_build_version) = match recv_message(&mut control_recv).await? {
        Some(Message::Hello {
            version,
            hostname,
            screen,
            fingerprint,
            build_version,
        }) => {
            if version != PROTOCOL_VERSION {
                return Err(eyre!(
                    "Protocol version mismatch: server={}, client={}",
                    version,
                    PROTOCOL_VERSION
                ));
            }
            if fingerprint != tls_fingerprint {
                return Err(eyre!(
                    "Server fingerprint mismatch: hello={}, tls={}",
                    fingerprint,
                    tls_fingerprint
                ));
            }
            validate_screen_layout(&screen, "server")?;

            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );
            if server_ver != BUILD_VERSION {
                warn!(
                    "Version mismatch: server={}, client={}",
                    server_ver, BUILD_VERSION
                );
            }

            // Check if we already trust this server's fingerprint
            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                info!("Server fingerprint already trusted");
                None
            } else {
                Some(prompt_pairing_code(addr).await?)
            };

            let ack = Message::HelloAck {
                accepted: true,
                version: PROTOCOL_VERSION,
                otp: otp.clone(),
                screen: Some(my_screen),
                build_version: Some(protocol::local_build_version()),
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
                    return Err(eyre!(
                        "Expected PairingResult, got: {}",
                        protocol::optional_message_summary(other.as_ref())
                    ));
                }
            }

            let ver = build_version.unwrap_or_else(|| "unknown".to_string());
            let mut runtime = RuntimeStatus::new("client", "connected");
            runtime.peer_addr = Some(addr.to_string());
            runtime.peer_name = Some(hostname);
            runtime.peer_screen = Some(format!("{}x{}", screen.width, screen.height));
            runtime.peer_build = Some(ver.clone());
            status::write_status(runtime).ok();
            (screen, ver)
        }
        other => {
            return Err(eyre!(
                "Expected Hello, got: {}",
                protocol::optional_message_summary(other.as_ref())
            ));
        }
    };

    // Auto-update only if server has a strictly newer clean release version
    if server_build_version != BUILD_VERSION
        && crate::net::update::is_release_version(&server_build_version)
        && crate::net::update::is_newer(&server_build_version, BUILD_VERSION)
    {
        info!(
            "Server has newer version {}, attempting self-update...",
            server_build_version
        );
        match crate::net::update::self_update(&server_build_version).await {
            Ok(()) => {
                info!("Updated to {}. Restarting...", server_build_version);
                connection.close(0u32.into(), b"updating");
                return restart_current_process();
            }
            Err(e) => {
                warn!(
                    "Self-update failed: {}. Continuing with current version.",
                    e
                );
            }
        }
    }

    let mut transition = ClientTransition::new(my_w, my_h);
    let mut control_messages = spawn_message_reader(control_recv);

    // Shutdown signal for background tasks
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);

    // Accept clipboard stream (bidirectional, second bi-stream from server)
    let (clip_send, mut clip_recv) =
        tokio::time::timeout(Duration::from_secs(10), connection.accept_bi())
            .await
            .wrap_err("Timeout waiting for clipboard stream from server")?
            .wrap_err("Failed to accept clipboard stream")?;
    // Read and discard the stream-ready marker
    let _marker = recv_message(&mut clip_recv).await?;
    info!("Clipboard stream accepted");

    // Spawn clipboard polling task (client → server)
    let clip_send = Arc::new(Mutex::new(clip_send));
    let clip_send_clone = clip_send.clone();
    let clipboard = Arc::new(std::sync::Mutex::new(
        crate::clipboard::sync::ClipboardSync::new(),
    ));
    let clipboard_poll = clipboard.clone();
    let mut shutdown_rx1 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let msg = {
                        let mut clipboard = lock_recover(&clipboard_poll, "clipboard");
                        clipboard.poll_change()
                    };
                    if let Ok(Some(msg)) = msg {
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
    let clipboard_recv = clipboard.clone();
    let mut shutdown_rx2 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = recv_message(&mut clip_recv) => {
                    match result {
                        Ok(Some(Message::ClipboardUpdate { content })) => {
                            let mut clipboard = lock_recover(&clipboard_recv, "clipboard");
                            if let Err(e) = clipboard.apply_update(&content) {
                                warn!("Failed to apply clipboard update: {}", e);
                            }
                        }
                        Ok(Some(other)) => {
                            debug!(
                                "Unexpected message on clipboard stream: {}",
                                protocol::message_summary(&other)
                            );
                        }
                        Ok(None) => {
                            info!("Clipboard stream closed by server");
                            break;
                        }
                        Err(e) => {
                            warn!("Clipboard stream error: {}", e);
                            break;
                        }
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
    let ft_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILE_TRANSFERS));
    let mut shutdown_rx3 = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = ft_conn.accept_bi() => {
                    match result {
                        Ok((send, recv)) => {
                            let Ok(permit) = ft_semaphore.clone().try_acquire_owned() else {
                                warn!(
                                    "Rejecting incoming file transfer: too many concurrent transfers (max {})",
                                    MAX_CONCURRENT_FILE_TRANSFERS
                                );
                                continue;
                            };
                            tokio::spawn(async move {
                                let _permit = permit;
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
    let mut input_recv = tokio::time::timeout(Duration::from_secs(10), connection.accept_uni())
        .await
        .wrap_err("Timeout waiting for input stream from server")?
        .wrap_err("Failed to accept input stream")?;
    // Read and discard the stream-ready marker
    let _marker = recv_message_uni(&mut input_recv).await?;
    debug!("Input stream accepted");
    let mut input_messages = spawn_message_reader(input_recv);

    let mut last_screen_w = my_w;
    let mut last_screen_h = my_h;
    let mut screen_check = time::interval(CLIENT_SCREEN_CHECK_INTERVAL);
    let mut latency_check = time::interval(CLIENT_LATENCY_CHECK_INTERVAL);
    let mut pending_latency_ping: Option<(u64, Instant)> = None;
    let mut latency_strikes: u8 = 0;
    let mut restart_for_latency = false;
    let mut injected_keys: HashSet<u32> = HashSet::new();
    let mut injected_buttons: HashSet<u8> = HashSet::new();
    let mut activation_started: Option<Instant> = None;
    let mut activation_input_messages: u64 = 0;
    let mut activation_inject_moves: u64 = 0;
    let mut activation_first_inject_logged = false;
    let mut last_edge_diagnostic: Option<(protocol::Direction, Instant)> = None;

    loop {
        tokio::select! {
            msg = input_messages.recv() => {
                match msg.unwrap_or(Ok(None)) {
                    Ok(Some(message)) => {
                        if activation_started.is_some() {
                            activation_input_messages += 1;
                        }
                        if matches!(message, Message::MouseMove { .. })
                            && transition.needs_cursor_sync()
                        {
                            // Reconcile against the real pointer while retaining
                            // the geometry snapshot taken at activation.
                            if let Ok(Some((x, y))) = injector.cursor_position() {
                                transition.sync_cursor_position(x, y);
                            }
                        }
                        let original_message = message.clone();
                        match transition.handle(message) {
                            ClientOutput::Ignore => {
                                // A switch-back can deactivate the client before cleanup
                                // releases arrive. Still inject releases for inputs we
                                // previously pressed, or the client OS can be left sticky.
                                match original_message {
                                    Message::KeyEvent { keycode, pressed: false, .. }
                                        if injected_keys.contains(&keycode) =>
                                    {
                                        let release = Message::KeyEvent { keycode, pressed: false, modifiers: 0 };
                                        if let Err(e) = injector.inject(&release) {
                                            warn!("Inject key release error: {}", e);
                                        } else {
                                            injected_keys.remove(&keycode);
                                        }
                                    }
                                    Message::MouseButton { button, pressed: false }
                                        if injected_buttons.contains(&button) =>
                                    {
                                        let release = Message::MouseButton { button, pressed: false };
                                        if let Err(e) = injector.inject(&release) {
                                            warn!("Inject mouse button release error: {}", e);
                                        } else {
                                            injected_buttons.remove(&button);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            ClientOutput::Activate => {
                                // Snapshot fresh geometry at every handoff and use
                                // that same snapshot for movement and edge checks.
                                match refresh_client_screen_snapshot(
                                    &mut *injector,
                                    &mut transition,
                                    &mut last_screen_w,
                                    &mut last_screen_h,
                                    "on remote activation",
                                ) {
                                    Ok(refresh) => {
                                        if let Err(e) = announce_client_screen_refresh(
                                            &mut control_send,
                                            &refresh,
                                        )
                                        .await
                                        {
                                            warn!("Failed to announce activation screen size: {}", e);
                                        }
                                    }
                                    Err(e) => warn!("Failed to refresh screen size on activation: {}", e),
                                }
                                if let Err(e) = injector.set_cursor_visible(true) {
                                    warn!("Failed to show cursor on remote activation: {}", e);
                                }
                                let direction = match &original_message {
                                    Message::SwitchScreen { direction } => Some(*direction),
                                    _ => None,
                                };
                                let (x, y, w, h, _, return_edge, dwell, cooldown) =
                                    transition.diagnostics();
                                info!(
                                    "Server sharing mouse: entry={:?}, return_edge={:?}, cursor=({}, {}), screen={}x{}, dwell={}, cooldown={}",
                                    direction, return_edge, x, y, w, h, dwell, cooldown
                                );
                                activation_started = Some(Instant::now());
                                activation_input_messages = 0;
                                activation_inject_moves = 0;
                                activation_first_inject_logged = false;
                                request_wake_display();
                            }
                            ClientOutput::Deactivate => {
                                // Synthetic input cleanup is more important than
                                // geometry refresh and must never wait behind it.
                                release_injected_inputs(&mut *injector, &mut injected_keys, &mut injected_buttons);
                                if let Err(e) = injector.set_cursor_visible(false) {
                                    warn!("Failed to hide cursor after server reclaimed control: {}", e);
                                }
                                activation_started = None;
                                info!("Server reclaimed control");
                                match refresh_client_screen_snapshot(
                                    &mut *injector,
                                    &mut transition,
                                    &mut last_screen_w,
                                    &mut last_screen_h,
                                    "on deactivation",
                                ) {
                                    Ok(refresh) => {
                                        if let Err(e) = announce_client_screen_refresh(
                                            &mut control_send,
                                            &refresh,
                                        )
                                        .await
                                        {
                                            warn!("Failed to announce deactivation screen size: {}", e);
                                        }
                                    }
                                    Err(e) => warn!("Failed to refresh screen size on deactivation: {}", e),
                                }
                            }
                            ClientOutput::InjectMove { x, y } => {
                                activation_inject_moves += 1;
                                let (_, _, w, h, _, return_edge, dwell, cooldown) =
                                    transition.diagnostics();
                                if let Some(edge) = crate::cursor::edge::detect_edge(x, y, w, h) {
                                    let should_log = last_edge_diagnostic.is_none_or(
                                        |(last_edge, at)| {
                                            last_edge != edge
                                                || at.elapsed() >= Duration::from_secs(1)
                                        },
                                    );
                                    if should_log {
                                        let actual = injector.cursor_position().ok().flatten();
                                        info!(
                                            "Client edge diagnostic: modeled=({}, {}), actual={:?}, screen={}x{}, edge={:?}, return_edge={:?}, dwell={}, cooldown={}",
                                            x, y, actual, w, h, edge, return_edge, dwell, cooldown
                                        );
                                        last_edge_diagnostic = Some((edge, Instant::now()));
                                    }
                                } else {
                                    last_edge_diagnostic = None;
                                }
                                if let Some(started) = activation_started {
                                    if !activation_first_inject_logged {
                                        activation_first_inject_logged = true;
                                        info!(
                                            "Activation diagnostics: first injected mouse move after {:.0}ms",
                                            started.elapsed().as_secs_f64() * 1000.0
                                        );
                                    }
                                }
                                let msg = Message::MouseMove { x, y };
                                if let Err(e) = inject_with_timing(&mut *injector, &msg, "mouse move") {
                                    warn!("Inject mouse move error: {}", e);
                                }
                            }
                            ClientOutput::Forward(msg) => {
                                if let Err(e) = inject_with_timing(&mut *injector, &msg, "forward") {
                                    warn!("Inject error: {}", e);
                                } else {
                                    track_injected_input(&msg, &mut injected_keys, &mut injected_buttons);
                                }
                            }
                            ClientOutput::SwitchBack { direction, inject } => {
                                if let Some((x, y)) = inject {
                                    let msg = Message::MouseMove { x, y };
                                    inject_with_timing(&mut *injector, &msg, "switch back").ok();
                                }
                                // Stop synthetic input and tell Linux to release
                                // its devices before any display query. CoreGraphics
                                // can be transiently unavailable during lid close.
                                release_injected_inputs(&mut *injector, &mut injected_keys, &mut injected_buttons);
                                if let Err(e) = injector.set_cursor_visible(false) {
                                    warn!("Failed to hide cursor after switching back: {}", e);
                                }
                                info!("Edge on client: {:?} — requesting switch back", direction);
                                let switch_msg = Message::SwitchScreen { direction };
                                if let Err(e) = send_message(&mut control_send, &switch_msg).await {
                                    warn!("Failed to request switch back: {}", e);
                                }
                                match refresh_client_screen_snapshot(
                                    &mut *injector,
                                    &mut transition,
                                    &mut last_screen_w,
                                    &mut last_screen_h,
                                    "on switch back",
                                ) {
                                    Ok(refresh) => {
                                        if let Err(e) = announce_client_screen_refresh(
                                            &mut control_send,
                                            &refresh,
                                        )
                                        .await
                                        {
                                            warn!("Failed to announce switch-back screen size: {}", e);
                                        }
                                    }
                                    Err(e) => warn!("Failed to refresh screen size on switch back: {}", e),
                                }
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
            msg = control_messages.recv() => {
                match msg.unwrap_or(Ok(None)) {
                    Ok(Some(Message::Heartbeat { timestamp })) => {
                        let ack = Message::HeartbeatAck { timestamp };
                        if let Err(e) = send_message(&mut control_send, &ack).await {
                            warn!("Failed to send heartbeat ack: {}", e);
                            break;
                        }
                    }
                    Ok(Some(Message::HeartbeatAck { timestamp })) => {
                        if let Some((pending_timestamp, sent_at)) = pending_latency_ping {
                            if timestamp == pending_timestamp {
                                let rtt = sent_at.elapsed();
                                pending_latency_ping = None;
                                if rtt > CLIENT_LATENCY_RESTART_THRESHOLD {
                                    latency_strikes = latency_strikes.saturating_add(1);
                                    warn!(
                                        "Client latency watchdog: RTT {:.0}ms (strike {}/{})",
                                        rtt.as_secs_f64() * 1000.0,
                                        latency_strikes,
                                        CLIENT_LATENCY_RESTART_STRIKES
                                    );
                                } else {
                                    latency_strikes = 0;
                                }
                            }
                        }
                    }
                    Ok(Some(Message::ScreenResize { screen })) => {
                        if validate_screen_layout(&screen, "server resize").is_ok() {
                            info!("Server screen changed: {}x{}", screen.width, screen.height);
                            server_screen = screen;
                        } else {
                            warn!("Ignoring invalid server screen resize: {}x{}", screen.width, screen.height);
                        }
                    }
                    Ok(Some(Message::WakeDisplay)) => {
                        debug!("Peer user active — keeping this system awake");
                        request_wake_display();
                    }
                    Ok(Some(other)) => {
                        debug!("Control message: {}", protocol::message_summary(&other));
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
            _ = latency_check.tick() => {
                if activation_started.is_some_and(|started| started.elapsed() >= Duration::from_secs(10)) {
                    let elapsed = activation_started.unwrap().elapsed();
                    info!(
                        "Activation diagnostics: {:.0}s summary: input_messages={}, injected_mouse_moves={}",
                        elapsed.as_secs_f64(),
                        activation_input_messages,
                        activation_inject_moves
                    );
                    activation_started = None;
                }

                if let Some((_, sent_at)) = pending_latency_ping {
                    if sent_at.elapsed() > CLIENT_LATENCY_RESTART_THRESHOLD {
                        latency_strikes = latency_strikes.saturating_add(1);
                        warn!(
                            "Client latency watchdog: heartbeat pending for {:.0}ms (strike {}/{})",
                            sent_at.elapsed().as_secs_f64() * 1000.0,
                            latency_strikes,
                            CLIENT_LATENCY_RESTART_STRIKES
                        );
                        pending_latency_ping = None;
                    }
                }

                if latency_strikes >= CLIENT_LATENCY_RESTART_STRIKES {
                    warn!("Client latency watchdog: sustained lag detected; restarting client process");
                    restart_for_latency = true;
                    break;
                }

                if pending_latency_ping.is_none() {
                    let timestamp = unix_millis();
                    if let Err(e) = send_message(&mut control_send, &Message::Heartbeat { timestamp }).await {
                        warn!("Client latency watchdog failed to send heartbeat: {}", e);
                        break;
                    }
                    pending_latency_ping = Some((timestamp, Instant::now()));
                }
            }
            _ = screen_check.tick() => {
                // Refresh geometry without changing ownership. CoreGraphics can
                // relocate the pointer when a display disappears; the snapshot
                // refresh reconciles the model to that real position and resets
                // stale edge dwell before processing more relative movement.
                let context = if transition.is_active() {
                    "while active"
                } else {
                    "while inactive"
                };
                match refresh_client_screen_snapshot(
                    &mut *injector,
                    &mut transition,
                    &mut last_screen_w,
                    &mut last_screen_h,
                    context,
                ) {
                    Ok(refresh) => {
                        if let Err(e) = announce_client_screen_refresh(
                            &mut control_send,
                            &refresh,
                        )
                        .await
                        {
                            warn!("Failed to announce refreshed screen size: {}", e);
                            break;
                        }
                    }
                    Err(e) => warn!("Failed to refresh {} screen size: {}", context, e),
                }
            }
        }
    }

    // Release any synthetic input that may still be down if the stream ended
    // before key-up/button-up events were processed (for example during display sleep).
    release_injected_inputs(&mut *injector, &mut injected_keys, &mut injected_buttons);
    if let Err(e) = injector.set_cursor_visible(true) {
        warn!("Failed to restore cursor after disconnect: {}", e);
    }

    // Signal clipboard tasks to shut down
    shutdown_tx.send(true).ok();

    // Gracefully close the connection
    connection.close(0u32.into(), b"disconnected");

    if restart_for_latency {
        return restart_current_process();
    }

    // Suppress unused variable warning
    let _ = server_screen;

    Ok(())
}

/// Ping a peer to measure QUIC RTT.
pub async fn ping(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint(addr)?;

    info!("Connecting to {}...", addr);
    let connection = connect_with_retry(&endpoint, addr).await?;
    let tls_fingerprint = tls::peer_fingerprint(&connection)
        .ok_or_else(|| eyre!("Server did not present a certificate"))?;

    // Accept the server's control stream and do the handshake
    let (mut send, mut recv) = connection.accept_bi().await?;

    let hello = recv_message(&mut recv).await?;
    match hello {
        Some(Message::Hello {
            version,
            hostname,
            screen,
            fingerprint,
            build_version,
        }) => {
            if version != PROTOCOL_VERSION {
                return Err(eyre!(
                    "Protocol version mismatch: server={}, client={}",
                    version,
                    PROTOCOL_VERSION
                ));
            }
            if fingerprint != tls_fingerprint {
                return Err(eyre!(
                    "Server fingerprint mismatch: hello={}, tls={}",
                    fingerprint,
                    tls_fingerprint
                ));
            }
            validate_screen_layout(&screen, "server")?;

            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );

            // Check if we already trust this server
            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                None
            } else {
                Some(prompt_pairing_code(addr).await?)
            };

            let ack = Message::HelloAck {
                accepted: true,
                version: PROTOCOL_VERSION,
                otp: otp.clone(),
                screen: None,
                build_version: Some(protocol::local_build_version()),
            };
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
                    return Err(eyre!(
                        "Expected PairingResult, got: {}",
                        protocol::optional_message_summary(other.as_ref())
                    ));
                }
            }
        }
        other => {
            return Err(eyre!(
                "Expected Hello, got: {}",
                protocol::optional_message_summary(other.as_ref())
            ));
        }
    }

    info!("Sending pings...\n");

    for seq in 0..10 {
        let ts = unix_millis();

        let start = Instant::now();
        let msg = Message::Heartbeat { timestamp: ts };
        send_message(&mut send, &msg).await?;

        match recv_message(&mut recv).await? {
            Some(Message::HeartbeatAck { timestamp: _ }) => {
                let rtt = start.elapsed();
                println!("  seq={} rtt={:.3}ms", seq, rtt.as_secs_f64() * 1000.0);
            }
            other => {
                warn!(
                    "Unexpected response: {}",
                    protocol::optional_message_summary(other.as_ref())
                );
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

/// Pair with a server once, storing its fingerprint if the OTP succeeds.
pub async fn pair(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint(addr)?;

    info!("Pairing with {}...", addr);
    let connection = connect_with_retry(&endpoint, addr).await?;
    let tls_fingerprint = tls::peer_fingerprint(&connection)
        .ok_or_else(|| eyre!("Server did not present a certificate"))?;

    let (mut send, mut recv) = connection.accept_bi().await?;
    let hello = recv_message(&mut recv).await?;

    match hello {
        Some(Message::Hello {
            version,
            hostname,
            screen,
            fingerprint,
            build_version,
        }) => {
            if version != PROTOCOL_VERSION {
                return Err(eyre!(
                    "Protocol version mismatch: server={}, client={}",
                    version,
                    PROTOCOL_VERSION
                ));
            }
            if fingerprint != tls_fingerprint {
                return Err(eyre!(
                    "Server fingerprint mismatch: hello={}, tls={}",
                    fingerprint,
                    tls_fingerprint
                ));
            }
            validate_screen_layout(&screen, "server")?;

            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );

            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                info!("Server fingerprint already trusted");
                None
            } else {
                Some(prompt_pairing_code(addr).await?)
            };

            let ack = Message::HelloAck {
                accepted: true,
                version: PROTOCOL_VERSION,
                otp: otp.clone(),
                screen: None,
                build_version: Some(protocol::local_build_version()),
            };
            send_message(&mut send, &ack).await?;

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
                    return Err(eyre!(
                        "Expected PairingResult, got: {}",
                        protocol::optional_message_summary(other.as_ref())
                    ));
                }
            }
        }
        other => {
            return Err(eyre!(
                "Expected Hello, got: {}",
                protocol::optional_message_summary(other.as_ref())
            ));
        }
    }

    connection.close(0u32.into(), b"paired");
    endpoint.wait_idle().await;

    Ok(())
}

fn normalize_connect_addr_input(addr: &str) -> Result<&str> {
    if addr.chars().any(char::is_control) {
        return Err(eyre!("Connect address contains control characters"));
    }
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return Err(eyre!("Connect address cannot be empty"));
    }
    if trimmed.len() > MAX_CONNECT_ADDR_BYTES {
        return Err(eyre!(
            "Connect address too large: {} bytes (max {})",
            trimmed.len(),
            MAX_CONNECT_ADDR_BYTES
        ));
    }
    Ok(trimmed)
}

fn safe_connect_addr_for_error(addr: &str) -> String {
    status::terminal_safe(addr, MAX_CONNECT_ADDR_BYTES)
}

fn resolve_addr(addr: &str) -> Result<SocketAddr> {
    let addr = normalize_connect_addr_input(addr)?;
    if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
        return validate_connect_addr(socket_addr, addr);
    }
    if let Ok(ip_addr) = addr.parse::<IpAddr>() {
        return validate_connect_addr(SocketAddr::new(ip_addr, DEFAULT_PORT), addr);
    }

    let host_port = if addr.starts_with('[') && addr.ends_with(']') {
        format!("{}:{}", addr, DEFAULT_PORT)
    } else if addr.matches(':').count() == 1
        && addr
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        addr.to_string()
    } else {
        format!("{}:{}", addr, DEFAULT_PORT)
    };

    let mut unusable_link_local = false;
    for socket_addr in host_port.to_socket_addrs().wrap_err_with(|| {
        format!(
            "Invalid or unresolvable address: {}",
            safe_connect_addr_for_error(addr)
        )
    })? {
        if is_unscoped_ipv6_link_local(&socket_addr) {
            unusable_link_local = true;
            continue;
        }
        return validate_connect_addr(socket_addr, addr);
    }

    let addr = safe_connect_addr_for_error(addr);
    if unusable_link_local {
        Err(eyre!(
            "Resolved only unscoped IPv6 link-local addresses for {addr}; use a scoped address or a routable address"
        ))
    } else {
        Err(eyre!("No addresses resolved for {addr}"))
    }
}

fn validate_connect_addr(socket_addr: SocketAddr, original: &str) -> Result<SocketAddr> {
    if socket_addr.port() == 0 {
        return Err(eyre!(
            "Port 0 is not usable for {original}; choose the server's fixed UDP port"
        ));
    }
    if is_unscoped_ipv6_link_local(&socket_addr) {
        return Err(eyre!(
            "Unscoped IPv6 link-local address is not usable for {original}; use a scoped address or a routable address"
        ));
    }
    Ok(socket_addr)
}

fn is_unscoped_ipv6_link_local(addr: &SocketAddr) -> bool {
    match addr {
        SocketAddr::V6(v6) => (v6.ip().segments()[0] & 0xffc0) == 0xfe80 && v6.scope_id() == 0,
        SocketAddr::V4(_) => false,
    }
}

fn make_server_endpoint(
    server_config: quinn::ServerConfig,
    port: u16,
) -> Result<(Endpoint, SocketAddr)> {
    let ipv6_addr: SocketAddr = format!("[::]:{port}").parse()?;
    match Endpoint::server(server_config.clone(), ipv6_addr) {
        Ok(endpoint) => Ok((endpoint, ipv6_addr)),
        Err(ipv6_err) => {
            let ipv4_addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
            let endpoint = Endpoint::server(server_config, ipv4_addr).wrap_err_with(|| {
                format!(
                    "Failed to bind QUIC server on {} (IPv6 attempt on {} failed: {})",
                    ipv4_addr, ipv6_addr, ipv6_err
                )
            })?;
            Ok((endpoint, ipv4_addr))
        }
    }
}

fn make_client_endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let client_config = tls::client_config()?;
    let bind_addr: SocketAddr = if addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let mut endpoint = Endpoint::client(bind_addr)
        .wrap_err_with(|| format!("Failed to bind QUIC client endpoint on {bind_addr}"))?;
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
    time::timeout(INPUT_SEND_TIMEOUT, send.write_all(&bytes))
        .await
        .wrap_err("Timed out sending input to peer")??;
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
    if len > protocol::MAX_MESSAGE_SIZE {
        return Err(eyre!("Message too large: {} bytes", len));
    }

    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(_) => eyre!("Connection closed mid-message"),
        other => other.into(),
    })?;

    let msg: Message = bincode::deserialize(&body)?;
    protocol::validate_message(&msg)?;
    Ok(Some(msg))
}

async fn recv_message_uni(recv: &mut quinn::RecvStream) -> Result<Option<Message>> {
    recv_message(recv).await
}

/// Read framed messages without exposing `read_exact` to cancellation from a
/// surrounding `tokio::select!`. Cancelling a partially completed framed read
/// consumes stream bytes and causes subsequent reads to lose frame alignment.
fn spawn_message_reader(
    mut recv: RecvStream,
) -> tokio::sync::mpsc::Receiver<Result<Option<Message>>> {
    // Keep the channel bounded so an abusive peer cannot queue messages without
    // applying QUIC stream backpressure.
    let (send, receive) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let result = recv_message(&mut recv).await;
            let finished = !matches!(result, Ok(Some(_)));
            if send.send(result).await.is_err() || finished {
                break;
            }
        }
    });
    receive
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    struct GeometryInjector {
        size: (u32, u32),
        changed: bool,
        cursor: (i32, i32),
    }

    impl InputInjector for GeometryInjector {
        fn inject(&mut self, _event: &Message) -> Result<()> {
            Ok(())
        }

        fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }

        fn screen_size(&self) -> Result<(u32, u32)> {
            Ok(self.size)
        }

        fn take_screen_geometry_changed(&mut self) -> bool {
            std::mem::take(&mut self.changed)
        }

        fn cursor_position(&self) -> Result<Option<(i32, i32)>> {
            Ok(Some(self.cursor))
        }
    }

    #[test]
    fn client_refresh_preserves_active_handoff_across_topology_changes() {
        let mut injector = GeometryInjector {
            size: (1920, 1080),
            changed: true,
            cursor: (400, 300),
        };
        let mut transition = ClientTransition::new(1920, 1080);
        transition.handle(Message::SwitchScreen {
            direction: protocol::Direction::Right,
        });
        let mut width = 1920;
        let mut height = 1080;

        let refresh = refresh_client_screen_snapshot(
            &mut injector,
            &mut transition,
            &mut width,
            &mut height,
            "during test",
        )
        .unwrap();

        assert!(refresh.resize.is_none());
        assert!(!injector.changed);
        assert!(transition.is_active());
    }

    #[test]
    fn listen_port_rejects_zero() {
        assert!(validate_listen_port(0).is_err());
        assert!(validate_listen_port(DEFAULT_PORT).is_ok());
    }

    #[test]
    fn file_transfer_concurrency_limit_is_small() {
        assert_eq!(MAX_CONCURRENT_FILE_TRANSFERS, 2);
    }

    #[test]
    fn transition_cleanup_does_not_release_untracked_ordinary_keys() {
        let keys = cleanup_keycodes(&HashSet::new());

        assert!(!keys.contains(&57), "orphaned Space key-up can pause media");
        assert!(!keys.contains(&164), "orphaned play/pause key-up is unsafe");
        assert_eq!(keys, DEFENSIVE_MODIFIER_KEYS);
    }

    #[test]
    fn transition_cleanup_releases_tracked_ordinary_keys() {
        let keys = cleanup_keycodes(&HashSet::from([57, 164]));

        assert!(keys.contains(&57));
        assert!(keys.contains(&164));
        for modifier in DEFENSIVE_MODIFIER_KEYS {
            assert!(keys.contains(modifier));
        }
    }

    #[test]
    fn wake_display_requests_are_coalesced_while_in_flight() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(mark_wake_display_in_flight(&flag));
        assert!(!mark_wake_display_in_flight(&flag));
        flag.store(false, std::sync::atomic::Ordering::Release);
        assert!(mark_wake_display_in_flight(&flag));
    }

    #[test]
    fn runtime_mutex_helper_locks_normal_state() {
        let mutex = std::sync::Mutex::new(1u32);
        *lock_recover(&mutex, "test") = 2;
        assert_eq!(*lock_recover(&mutex, "test"), 2);
    }

    #[test]
    fn pairing_prompt_is_flushed_and_user_visible() {
        let mut output = Vec::new();
        write_pairing_prompt(&mut output).unwrap();
        assert_eq!(output, b"Enter pairing code: ");
    }

    #[test]
    fn invalid_pairing_codes_are_rejected_before_send() {
        assert_eq!(normalize_pairing_input(" 123456\n").unwrap(), "123456");
        assert!(normalize_pairing_input("\n").is_err());
        assert!(normalize_pairing_input("12345").is_err());
        assert!(normalize_pairing_input("1234567").is_err());
        assert!(normalize_pairing_input("12a456").is_err());
    }

    #[test]
    fn local_peer_hostname_metadata_is_protocol_safe() {
        assert_eq!(sanitize_peer_hostname("host\nname"), "host�name");
        assert_eq!(sanitize_peer_hostname(""), "nexdesk");
        assert_eq!(
            sanitize_peer_hostname(&"h".repeat(protocol::MAX_PEER_NAME_BYTES + 1)).len(),
            protocol::MAX_PEER_NAME_BYTES
        );
    }

    #[test]
    fn system_time_millis_saturates_to_protocol_timestamp_range() {
        assert_eq!(
            system_time_millis_u64(std::time::UNIX_EPOCH - Duration::from_millis(1)),
            0
        );
        assert_eq!(
            system_time_millis_u64(std::time::UNIX_EPOCH + Duration::from_millis(42)),
            42
        );
        assert_eq!(
            system_time_millis_u64(
                std::time::UNIX_EPOCH + Duration::from_millis(u64::MAX) + Duration::from_millis(1)
            ),
            u64::MAX
        );
    }

    #[test]
    fn layer_shell_button_mapping_rejects_unsupported_buttons() {
        assert_eq!(layer_shell_button_to_protocol(0x110), Some(0));
        assert_eq!(layer_shell_button_to_protocol(0x111), Some(1));
        assert_eq!(layer_shell_button_to_protocol(0x112), Some(2));
        assert_eq!(layer_shell_button_to_protocol(0x113), None);
        assert_eq!(layer_shell_button_to_protocol(256), None);
    }

    #[test]
    fn layer_shell_key_mapping_matches_protocol_range() {
        assert_eq!(
            layer_shell_key_to_protocol(protocol::MAX_KEYCODE),
            Some(protocol::MAX_KEYCODE)
        );
        assert_eq!(layer_shell_key_to_protocol(protocol::MAX_KEYCODE + 1), None);
    }

    #[test]
    fn layer_shell_float_inputs_are_normalized_to_protocol_safe_values() {
        assert_eq!(layer_shell_motion_delta(f64::NAN), 0);
        assert_eq!(layer_shell_motion_delta(f64::INFINITY), 0);
        assert_eq!(layer_shell_motion_delta(i32::MAX as f64 * 2.0), i32::MAX);
        assert_eq!(layer_shell_scroll_delta(f64::NAN), 0.0);
        assert_eq!(
            layer_shell_scroll_delta(protocol::MAX_SCROLL_DELTA * 2.0),
            protocol::MAX_SCROLL_DELTA
        );
    }

    #[test]
    fn nonzero_screen_layout_rejects_zero_dimensions() {
        assert!(nonzero_screen_layout(0, 1080).is_none());
        assert!(nonzero_screen_layout(1920, 0).is_none());
        assert_eq!(nonzero_screen_layout(1920, 1080).unwrap().width, 1920);
    }

    #[cfg(test)]
    fn should_retry_resolution(explicit_addr: Option<&str>) -> bool {
        explicit_addr.is_some()
    }

    #[test]
    fn explicit_connect_addresses_are_resolved_each_loop() {
        assert!(should_retry_resolution(Some("example.local")));
        assert!(!should_retry_resolution(None));
    }

    #[test]
    fn explicit_connect_addr_is_validated_before_reconnect_loop() {
        assert_eq!(
            explicit_connect_addr_arg(Some(" example.local "))
                .unwrap()
                .as_deref(),
            Some("example.local")
        );
        assert!(explicit_connect_addr_arg(None).unwrap().is_none());
        assert!(explicit_connect_addr_arg(Some("")).is_err());
        assert!(explicit_connect_addr_arg(Some("example.local\n")).is_err());
        assert!(explicit_connect_addr_arg(Some(&"x".repeat(MAX_CONNECT_ADDR_BYTES + 1))).is_err());
    }

    #[test]
    fn resolve_addr_adds_default_port_to_ipv4_literal() {
        assert_eq!(
            resolve_addr("127.0.0.1").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
        );
    }

    #[test]
    fn resolve_addr_trims_and_bounds_user_input() {
        assert_eq!(
            resolve_addr(" 127.0.0.1 ").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
        );
        assert!(resolve_addr(" ").is_err());
        assert!(resolve_addr("127.0.0.1\n").is_err());
        assert!(resolve_addr("127.0.\n0.1").is_err());
        assert!(resolve_addr(&"a".repeat(MAX_CONNECT_ADDR_BYTES + 1)).is_err());
        let safe = safe_connect_addr_for_error(&format!("{}\x1b[31m", "x".repeat(2048)));
        assert!(!safe.contains('\u{1b}'));
        assert_eq!(safe.len(), MAX_CONNECT_ADDR_BYTES);
    }

    #[test]
    fn resolve_addr_preserves_explicit_ipv4_port() {
        assert_eq!(
            resolve_addr("127.0.0.1:5555").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5555)
        );
    }

    #[test]
    fn resolve_addr_adds_default_port_to_ipv6_literal() {
        assert_eq!(
            resolve_addr("::1").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT)
        );
    }

    #[test]
    fn resolve_addr_preserves_bracketed_ipv6_port() {
        assert_eq!(
            resolve_addr("[::1]:5555").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5555)
        );
    }

    #[test]
    fn resolve_addr_adds_default_port_to_bracketed_ipv6_literal() {
        assert_eq!(
            resolve_addr("[::1]").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), DEFAULT_PORT)
        );
    }

    #[test]
    fn resolve_addr_rejects_unscoped_ipv6_link_local_literals() {
        assert!(resolve_addr("fe80::1").is_err());
        assert!(resolve_addr("[fe80::1]:4242").is_err());
        assert!(is_unscoped_ipv6_link_local(
            &"[fe80::1]:4242".parse().unwrap()
        ));
        assert!(!is_unscoped_ipv6_link_local(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            DEFAULT_PORT
        )));
    }

    #[test]
    fn resolve_addr_rejects_zero_connect_ports() {
        assert!(resolve_addr("127.0.0.1:0").is_err());
        assert!(resolve_addr("[::1]:0").is_err());
        assert!(resolve_addr("localhost:0").is_err());
    }
}
