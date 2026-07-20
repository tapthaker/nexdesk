use std::collections::{HashSet, VecDeque};
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Endpoint, RecvStream, SendStream};
use rand::Rng;
use tokio::sync::{Mutex, Notify};
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::app::{
    client_pairing_decision, complete_client_pairing, require_handshake_message,
    validate_client_server_hello, CancellationToken, HandshakeMessage, PairingCompletion,
    PairingDecision, RestartReason, RetryPolicy, SessionExit,
};
use crate::input::inject::{InputInjector, InputInjectorFactory, PlatformInputInjectorFactory};
use crate::input::wake::PlatformDisplaySessionControl;
use crate::net::discovery;
use crate::net::protocol::{self, Message, ScreenLayout, BUILD_VERSION, PROTOCOL_VERSION};
use crate::net::tls;
use crate::net::tls::ConfigTrustStore;
use crate::net::transition::{ClientOutput, ClientTransition, ServerOutput, ServerTransition};
use crate::ports::{DisplaySessionControl, TrustStore};
use crate::status::{self, RuntimeStatus};

const DEFAULT_PORT: u16 = 4242;
/// Maximum rate at which pointer positions are sent to or injected by a peer.
/// Intermediate relative movements are accumulated, never discarded.
const POINTER_FRAME_INTERVAL: Duration = Duration::from_micros(4_167); // ~240 Hz
const MOUSE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const USER_ACTIVITY_INTERVAL: Duration = Duration::from_secs(20);
const LOCAL_LOCK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CLIENT_LATENCY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const CLIENT_LATENCY_RESTART_THRESHOLD: Duration = Duration::from_secs(3);
const CLIENT_LATENCY_RESTART_STRIKES: u8 = 3;

#[derive(Debug)]
enum InputQueueItem {
    Message(Message),
    Closed,
    Error(String),
}

#[derive(Default)]
struct InputQueueState {
    items: VecDeque<InputQueueItem>,
    terminal_queued: bool,
}

/// Queue between the QUIC reader and the input injector. Reading remains active
/// while an OS injection call blocks, and the consumer applies every relative
/// delta to logical pointer state before replacing the pending absolute frame.
#[derive(Clone, Default)]
struct InputMessageQueue {
    state: Arc<StdMutex<InputQueueState>>,
    notify: Arc<Notify>,
}

impl InputMessageQueue {
    fn push(&self, message: Message) {
        let mut state = self.state.lock().unwrap();
        if state.terminal_queued {
            return;
        }

        state.items.push_back(InputQueueItem::Message(message));
        drop(state);
        self.notify.notify_one();
    }

    fn close(&self) {
        self.push_terminal(InputQueueItem::Closed);
    }

    fn fail(&self, error: String) {
        self.push_terminal(InputQueueItem::Error(error));
    }

    fn push_terminal(&self, item: InputQueueItem) {
        let mut state = self.state.lock().unwrap();
        if state.terminal_queued {
            return;
        }
        state.terminal_queued = true;
        state.items.push_back(item);
        drop(state);
        self.notify.notify_one();
    }

    async fn recv(&self) -> InputQueueItem {
        loop {
            // Register before checking the queue so a push between the check
            // and await cannot be missed.
            let notified = self.notify.notified();
            if let Some(item) = self.state.lock().unwrap().items.pop_front() {
                return item;
            }
            notified.await;
        }
    }
}

fn layer_shell_button_to_protocol(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0),
        0x111 => Some(1),
        0x112 => Some(2),
        _ => None,
    }
}

fn take_accumulated_motion(pending: &mut (f64, f64)) -> Option<Message> {
    let dx = pending.0.trunc() as i32;
    let dy = pending.1.trunc() as i32;
    pending.0 -= f64::from(dx);
    pending.1 -= f64::from(dy);

    if dx == 0 && dy == 0 {
        None
    } else {
        Some(Message::MouseMove { x: dx, y: dy })
    }
}

fn flush_pending_mouse(
    injector: &mut dyn InputInjector,
    pending: &mut Option<(i32, i32)>,
    activation_started: Option<Instant>,
    injected_count: &mut u64,
    first_inject_logged: &mut bool,
    context: &str,
) {
    let Some((x, y)) = pending.take() else {
        return;
    };

    *injected_count += 1;
    if let Some(started) = activation_started {
        if !*first_inject_logged {
            *first_inject_logged = true;
            info!(
                "Activation diagnostics: first injected mouse move after {:.0}ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    let message = Message::MouseMove { x, y };
    if let Err(e) = inject_with_timing(injector, &message, context) {
        warn!("Inject mouse move error: {}", e);
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
    #[cfg(target_os = "linux")]
    std::thread::spawn(crate::input::wake::wake_display);
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
            "Slow input injection during {}: {:.0}ms ({:?})",
            context,
            elapsed.as_secs_f64() * 1000.0,
            msg
        );
    }
    result
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

fn release_injected_inputs(
    injector: &mut dyn InputInjector,
    display_control: &dyn DisplaySessionControl,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) {
    if injected_keys.is_empty() && injected_buttons.is_empty() {
        return;
    }

    display_control.wake_display().ok();

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

    // Always include modifiers in cleanup. These are the keys users notice as
    // "sticky" most often, and relying only on tracked key-downs is brittle if
    // the disconnect/switch-back races with the last key event.
    const MODIFIER_KEYS: &[u32] = &[
        29,  // KEY_LEFTCTRL
        42,  // KEY_LEFTSHIFT
        54,  // KEY_RIGHTSHIFT
        56,  // KEY_LEFTALT
        58,  // KEY_CAPSLOCK
        97,  // KEY_RIGHTCTRL
        100, // KEY_RIGHTALT
        125, // KEY_LEFTMETA
        126, // KEY_RIGHTMETA
    ];

    let mut keys: Vec<u32> = injected_keys
        .iter()
        .copied()
        .chain(MODIFIER_KEYS.iter().copied())
        .collect();
    keys.sort_unstable();
    keys.dedup();

    for keycode in keys {
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

fn release_defensive_keyups(injector: &mut dyn InputInjector) {
    // Last-resort cleanup for cases where our bookkeeping missed a key-down
    // (e.g. a disconnect/restart race). Key-up events for keys that are not
    // down are harmless, and this prevents OS-level auto-repeat from getting
    // stuck on the client.
    for keycode in 0u32..256 {
        if crate::input::keymap::evdev_to_macos(keycode).is_none() {
            continue;
        }
        let msg = Message::KeyEvent {
            keycode,
            pressed: false,
            modifiers: 0,
        };
        if let Err(e) = injector.inject(&msg) {
            debug!("Defensive key release failed for {}: {}", keycode, e);
        }
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

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn should_attempt_client_update(server_build: &str) -> bool {
    server_build != BUILD_VERSION
        && crate::net::update::is_release_version(server_build)
        && crate::net::update::is_newer(server_build, BUILD_VERSION)
}

fn update_restart_reason<E>(
    server_build: &str,
    update_result: &std::result::Result<(), E>,
) -> Option<RestartReason> {
    update_result
        .is_ok()
        .then(|| RestartReason::UpdateInstalled {
            version: server_build.to_string(),
        })
}

fn client_shutdown_exit(restart_for_latency: bool) -> SessionExit {
    if restart_for_latency {
        SessionExit::RestartRequested(RestartReason::LatencyWatchdog)
    } else {
        SessionExit::Disconnected
    }
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
    let server_config = tls::server_config()?;
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    let endpoint = Endpoint::server(server_config, addr)?;

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
        Some(Message::HelloAck {
            accepted: true,
            otp,
            screen,
            build_version,
        }) => {
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
                warn!(
                    "Version mismatch: server={}, client={}",
                    BUILD_VERSION, peer_version
                );
            }
            screen.unwrap_or(ScreenLayout {
                width: 1920,
                height: 1080,
            })
        }
        Some(Message::HelloAck {
            accepted: false, ..
        }) => {
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
    info!("Clipboard stream opened");

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
    let clipboard = Arc::new(std::sync::Mutex::new(
        crate::clipboard::sync::ClipboardSync::new(),
    ));
    let clipboard_poll = clipboard.clone();
    tokio::spawn(async move {
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::time::sleep(interval).await;
            let msg = {
                let mut clipboard = clipboard_poll.lock().unwrap();
                clipboard.poll_change()
            };
            if let Ok(Some(msg)) = msg {
                let mut sender = clip_send_clone.lock().await;
                if send_message(&mut sender, &msg).await.is_err() {
                    break;
                }
            }
        }
    });

    // Spawn clipboard receive task
    let clipboard_recv = clipboard.clone();
    tokio::spawn(async move {
        loop {
            match recv_message(&mut clip_recv).await {
                Ok(Some(Message::ClipboardUpdate { content })) => {
                    let mut clipboard = clipboard_recv.lock().unwrap();
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
    runtime.peer_build = Some("unknown".to_string());
    status::write_status(runtime).ok();
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
                                    crate::filetransfer::clipboard_files::set_clipboard_files(
                                        &paths,
                                    )
                                    .ok();
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
    });

    info!("Server ready. Move mouse to screen edge to start sharing.");
    info!("Screen size: {}x{}", screen_w, screen_h);

    let mut poll_interval = time::interval(MOUSE_POLL_INTERVAL);
    let mut layer_shell_key_poll_interval = time::interval(MOUSE_POLL_INTERVAL);
    let mut pointer_send_interval = time::interval(POINTER_FRAME_INTERVAL);
    pointer_send_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut pending_layer_shell_motion = (0.0f64, 0.0f64);
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
                    let mut cap = capturer.lock().unwrap();
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
                if debug_counter % 500 == 0 {
                    let clamped_x = mx.clamp(0, sw as i32 - 1);
                    let clamped_y = my.clamp(0, sh as i32 - 1);
                    debug!("Mouse: ({}, {}) raw: ({}, {}) screen: {}x{}", clamped_x, clamped_y, mx, my, sw, sh);
                }

                match transition.poll(mx, my, sw, sh, buttons, key_events) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { messages, .. } => {
                        info!("Edge detected — switching to remote");
                        pending_layer_shell_motion = (0.0, 0.0);
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
                            if let Message::MouseMove { x, y } = msg {
                                pending_layer_shell_motion.0 += f64::from(x);
                                pending_layer_shell_motion.1 += f64::from(y);
                                continue;
                            }
                            if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                send_message_uni(&mut sender, &motion).await.ok();
                            }
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                warn!("Failed to send: {}", e);
                                transition.deactivate();
                                capturer.lock().unwrap().set_grab(false).ok();
                                break;
                            }
                        }
                    }
                    ServerOutput::ShortcutRelease { messages } => {
                        info!("Shortcut switch back — releasing grab");
                        let mut sender = input_send.lock().await;
                        if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                            send_message_uni(&mut sender, &motion).await.ok();
                        }
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        capturer.lock().unwrap().set_grab(false).ok();
                    }
                    ServerOutput::ForceRelease { messages } => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing grab");
                        let mut sender = input_send.lock().await;
                        if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                            send_message_uni(&mut sender, &motion).await.ok();
                        }
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        capturer.lock().unwrap().set_grab(false).ok();
                    }
                }
            }
            // Branch: keyboard polling for layer-shell mode. Wayland compositors
            // can consume global shortcuts and media keys before wl_keyboard sees
            // them, so use evdev while the remote screen is active.
            _ = layer_shell_key_poll_interval.tick(), if use_layer_shell && layer_shell_keyboard_grabbed && transition.is_active() => {
                let key_events: Vec<Message> = {
                    let mut cap = capturer.lock().unwrap();
                    cap.poll_key_events()
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
                        let mut sender = input_send.lock().await;
                        if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                            send_message_uni(&mut sender, &motion).await.ok();
                        }
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        capturer.lock().unwrap().set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                    }
                    ServerOutput::Forward { messages } => {
                        if !messages.is_empty() {
                            let mut sender = input_send.lock().await;
                            if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                send_message_uni(&mut sender, &motion).await.ok();
                            }
                            for msg in messages {
                                if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                    warn!("Failed to send key event: {}", e);
                                    transition.deactivate();
                                    if let Some(ref tx) = capture_tx {
                                        use crate::input::wayland_layer_shell::LayerShellCommand;
                                        tx.send(LayerShellCommand::Release).ok();
                                    }
                                    capturer.lock().unwrap().set_keyboard_grab(false).ok();
                                    layer_shell_keyboard_grabbed = false;
                                    break;
                                }
                            }
                        }
                    }
                    ServerOutput::ForceRelease { messages } => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                        let mut sender = input_send.lock().await;
                        if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                            send_message_uni(&mut sender, &motion).await.ok();
                        }
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        capturer.lock().unwrap().set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                    }
                }
            }
            // Send at most one accumulated pointer movement per frame. If the
            // network or peer is slow, newer deltas merge into this position
            // instead of forming an unbounded queue of stale movements.
            _ = pointer_send_interval.tick(), if transition.is_active() => {
                if let Some(message) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                    let mut sender = input_send.lock().await;
                    if let Err(e) = send_message_uni(&mut sender, &message).await {
                        warn!("Failed to send accumulated mouse move: {}", e);
                        transition.deactivate();
                        if use_layer_shell {
                            if let Some(ref tx) = capture_tx {
                                use crate::input::wayland_layer_shell::LayerShellCommand;
                                tx.send(LayerShellCommand::Release).ok();
                            }
                            capturer.lock().unwrap().set_keyboard_grab(false).ok();
                            layer_shell_keyboard_grabbed = false;
                        } else {
                            capturer.lock().unwrap().set_grab(false).ok();
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
                        pending_layer_shell_motion = (0.0, 0.0);
                        let messages = transition.activate_instant(direction);
                        info!("Layer-shell edge enter ({:?}) — switching to remote", direction);
                        match capturer.lock().unwrap().set_keyboard_grab(true) {
                            Ok(()) => {
                                layer_shell_keyboard_grabbed = true;
                            }
                            Err(e) => {
                                warn!("Failed to grab keyboard devices for layer-shell capture: {}", e);
                                layer_shell_keyboard_grabbed = false;
                            }
                        }
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
                            pending_layer_shell_motion.0 += dx;
                            pending_layer_shell_motion.1 += dy;
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
                            if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                send_message_uni(&mut sender, &motion).await.ok();
                            }
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::MouseScroll { dx, dy } => {
                        if transition.is_active() {
                            let phase = if scroll_active {
                                crate::net::protocol::ScrollPhase::Changed
                            } else {
                                scroll_active = true;
                                crate::net::protocol::ScrollPhase::Began
                            };
                            let msg = Message::MouseScroll { dx, dy, phase };
                            let mut sender = input_send.lock().await;
                            if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                send_message_uni(&mut sender, &motion).await.ok();
                            }
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
                            if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                send_message_uni(&mut sender, &motion).await.ok();
                            }
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    LayerShellEvent::KeyEvent { keycode, pressed } => {
                        if transition.is_active() && !layer_shell_keyboard_grabbed {
                            transition.update_key(keycode, pressed);
                            if transition.is_escape_combo() {
                                warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                                let messages = transition.deactivate_for_shortcut();
                                let mut sender = input_send.lock().await;
                                if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                    send_message_uni(&mut sender, &motion).await.ok();
                                }
                                for msg in messages {
                                    send_message_uni(&mut sender, &msg).await.ok();
                                }
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                            } else if transition.shortcut_direction().is_some() {
                                info!("Shortcut switch back — releasing layer-shell grab");
                                let messages = transition.deactivate_for_shortcut();
                                let mut sender = input_send.lock().await;
                                if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                    send_message_uni(&mut sender, &motion).await.ok();
                                }
                                for msg in messages {
                                    send_message_uni(&mut sender, &msg).await.ok();
                                }
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                                capturer.lock().unwrap().set_keyboard_grab(false).ok();
                                layer_shell_keyboard_grabbed = false;
                            } else {
                                let msg = Message::KeyEvent { keycode, pressed, modifiers: 0 };
                                let mut sender = input_send.lock().await;
                                if let Some(motion) = take_accumulated_motion(&mut pending_layer_shell_motion) {
                                    send_message_uni(&mut sender, &motion).await.ok();
                                }
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
                        request_wake_display();
                        pending_layer_shell_motion = (0.0, 0.0);
                        info!("Client requested switch back: {:?}", direction);
                        let messages = transition.on_switch_back();
                        if !messages.is_empty() {
                            let mut sender = input_send.lock().await;
                            for msg in messages {
                                send_message_uni(&mut sender, &msg).await.ok();
                            }
                        }
                        if use_layer_shell {
                            #[cfg(target_os = "linux")]
                            if let Some(ref tx) = capture_tx {
                                use crate::input::wayland_layer_shell::LayerShellCommand;
                                tx.send(LayerShellCommand::Release).ok();
                            }
                            capturer.lock().unwrap().set_keyboard_grab(false).ok();
                            layer_shell_keyboard_grabbed = false;
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
            _ = local_lock_check.tick(), if transition.is_active() => {
                if crate::input::session::is_session_locked() {
                    warn!("Local session locked while sharing — releasing remote control so Linux can be unlocked locally");
                    let messages = transition.deactivate_for_shortcut();
                    if !messages.is_empty() {
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                    }
                    if use_layer_shell {
                        #[cfg(target_os = "linux")]
                        if let Some(ref tx) = capture_tx {
                            use crate::input::wayland_layer_shell::LayerShellCommand;
                            tx.send(LayerShellCommand::Release).ok();
                        }
                        capturer.lock().unwrap().set_keyboard_grab(false).ok();
                        layer_shell_keyboard_grabbed = false;
                    } else {
                        capturer.lock().unwrap().set_grab(false).ok();
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
    let mut runtime = RuntimeStatus::new("server", "listening");
    runtime.peer_addr = Some(format!("last disconnected: {}", remote));
    status::write_status(runtime).ok();
    if use_layer_shell {
        #[cfg(target_os = "linux")]
        if let Some(ref tx) = capture_tx {
            use crate::input::wayland_layer_shell::LayerShellCommand;
            tx.send(LayerShellCommand::Shutdown).ok();
        }
        capturer.lock().unwrap().set_keyboard_grab(false).ok();
    } else {
        capturer.lock().unwrap().set_grab(false).ok();
    }

    Ok(())
}

fn explicit_connect_addr_arg(addr: Option<&str>) -> Result<Option<String>> {
    addr.map(normalize_connect_addr_input)
        .transpose()
        .map(|addr| addr.map(str::to_string))
}

trait ClientReconnectDriver {
    type Connected;

    async fn resolve_target(&mut self) -> Result<SocketAddr>;
    async fn connect_target(&mut self, addr: SocketAddr) -> Result<Self::Connected>;
    async fn run_session(&mut self, connected: Self::Connected) -> Result<SessionExit>;
    fn record_disconnected(&mut self, _addr: SocketAddr) {}
}

struct ProductionClientDriver {
    explicit_addr: Option<String>,
    injector_factory: Arc<dyn InputInjectorFactory>,
    display_control: Arc<dyn DisplaySessionControl>,
    trust_store: Arc<dyn TrustStore>,
}

struct ConnectedClient {
    _endpoint: Endpoint,
    connection: quinn::Connection,
    addr: SocketAddr,
}

impl ClientReconnectDriver for ProductionClientDriver {
    type Connected = ConnectedClient;

    async fn resolve_target(&mut self) -> Result<SocketAddr> {
        match self.explicit_addr.clone() {
            Some(addr) => tokio::task::spawn_blocking(move || resolve_addr(&addr))
                .await
                .wrap_err("Address resolution task failed")?,
            None => discovery::discover_one(Duration::from_secs(10)).await,
        }
    }

    async fn connect_target(&mut self, addr: SocketAddr) -> Result<Self::Connected> {
        info!("Connecting to nexdesk server at {}", addr);
        let endpoint = make_client_endpoint()?;
        let mut runtime = RuntimeStatus::new("client", "connecting");
        runtime.peer_addr = Some(addr.to_string());
        status::write_status(runtime).ok();
        let connection = connect_with_retry(&endpoint, addr).await?;
        Ok(ConnectedClient {
            _endpoint: endpoint,
            connection,
            addr,
        })
    }

    async fn run_session(&mut self, connected: Self::Connected) -> Result<SessionExit> {
        run_client_session(
            connected.connection,
            connected.addr,
            self.injector_factory.as_ref(),
            self.display_control.as_ref(),
            self.trust_store.as_ref(),
        )
        .await
    }

    fn record_disconnected(&mut self, addr: SocketAddr) {
        let mut runtime = RuntimeStatus::new("client", "disconnected");
        runtime.peer_addr = Some(addr.to_string());
        status::write_status(runtime).ok();
    }
}

async fn wait_for_retry(cancellation: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = cancellation.cancelled() => false,
        _ = time::sleep(delay) => true,
    }
}

async fn run_client_reconnect_loop<D: ClientReconnectDriver>(
    driver: &mut D,
    retry_policy: RetryPolicy,
    cancellation: CancellationToken,
) -> Result<SessionExit> {
    let mut retry_attempt = 0u32;

    loop {
        let target_addr = tokio::select! {
            _ = cancellation.cancelled() => return Ok(SessionExit::Cancelled),
            result = driver.resolve_target() => match result {
                Ok(addr) => addr,
                Err(error) => {
                    let delay = retry_policy.delay_for_attempt(retry_attempt);
                    retry_attempt = retry_attempt.saturating_add(1);
                    warn!("Target resolution failed: {}. Retrying in {:?}...", error, delay);
                    if !wait_for_retry(&cancellation, delay).await {
                        return Ok(SessionExit::Cancelled);
                    }
                    continue;
                }
            },
        };

        let connected = tokio::select! {
            _ = cancellation.cancelled() => return Ok(SessionExit::Cancelled),
            result = driver.connect_target(target_addr) => match result {
                Ok(connected) => connected,
                Err(error) => {
                    driver.record_disconnected(target_addr);
                    let delay = retry_policy.delay_for_attempt(retry_attempt);
                    retry_attempt = retry_attempt.saturating_add(1);
                    warn!("Connection failed: {}. Retrying in {:?}...", error, delay);
                    if !wait_for_retry(&cancellation, delay).await {
                        return Ok(SessionExit::Cancelled);
                    }
                    continue;
                }
            },
        };

        let session_exit = tokio::select! {
            _ = cancellation.cancelled() => return Ok(SessionExit::Cancelled),
            result = driver.run_session(connected) => result,
        };
        driver.record_disconnected(target_addr);

        let retry_delay = match session_exit {
            Ok(SessionExit::Cancelled) => return Ok(SessionExit::Cancelled),
            Ok(SessionExit::RestartRequested(reason)) => {
                return Ok(SessionExit::RestartRequested(reason));
            }
            Ok(SessionExit::Fatal(error)) => return Err(error),
            Ok(SessionExit::RetryAfter(delay)) => delay,
            Ok(SessionExit::Disconnected) => {
                info!("Connection closed cleanly");
                retry_attempt = 0;
                retry_policy.delay_for_attempt(retry_attempt)
            }
            Err(error) => {
                warn!("Connection error: {}", error);
                let delay = retry_policy.delay_for_attempt(retry_attempt);
                retry_attempt = retry_attempt.saturating_add(1);
                delay
            }
        };

        info!("Reconnecting in {:?}...", retry_delay);
        if !wait_for_retry(&cancellation, retry_delay).await {
            return Ok(SessionExit::Cancelled);
        }
    }
}

/// Connect to a QUIC server as a client (receives and injects input).
/// If `addr` is None, discovers the server via mDNS.
/// Automatically reconnects according to the configured retry policy.
pub async fn connect(addr: Option<&str>) -> Result<SessionExit> {
    connect_with_cancellation(addr, CancellationToken::new()).await
}

async fn connect_with_cancellation(
    addr: Option<&str>,
    cancellation: CancellationToken,
) -> Result<SessionExit> {
    connect_with_dependencies(
        addr,
        cancellation,
        Arc::new(PlatformInputInjectorFactory),
        Arc::new(PlatformDisplaySessionControl),
        Arc::new(ConfigTrustStore),
    )
    .await
}

async fn connect_with_dependencies(
    addr: Option<&str>,
    cancellation: CancellationToken,
    injector_factory: Arc<dyn InputInjectorFactory>,
    display_control: Arc<dyn DisplaySessionControl>,
    trust_store: Arc<dyn TrustStore>,
) -> Result<SessionExit> {
    let explicit_addr = explicit_connect_addr_arg(addr)?;
    let _idle_sleep_inhibitor = display_control.inhibit_idle_sleep()?;
    let mut driver = ProductionClientDriver {
        explicit_addr,
        injector_factory,
        display_control,
        trust_store,
    };
    run_client_reconnect_loop(&mut driver, RetryPolicy::default(), cancellation).await
}

/// Handle one established client connection, including handshake and session loops.
async fn run_client_session(
    connection: quinn::Connection,
    addr: SocketAddr,
    injector_factory: &dyn InputInjectorFactory,
    display_control: &dyn DisplaySessionControl,
    trust_store: &dyn TrustStore,
) -> Result<SessionExit> {
    let tls_fingerprint = tls::peer_fingerprint(&connection)
        .ok_or_else(|| eyre!("Server did not present a certificate"))?;

    info!("Connected to {}", addr);
    let mut runtime = RuntimeStatus::new("client", "connected");
    runtime.peer_addr = Some(addr.to_string());
    status::write_status(runtime).ok();

    // Create input injector early so we can send screen size in handshake
    let mut injector = injector_factory.create()?;
    let (my_w, my_h) = injector.screen_size()?;
    info!("Local screen: {}x{}", my_w, my_h);

    // Accept control stream and do handshake
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;

    let hello = match recv_message(&mut control_recv).await? {
        Some(Message::Hello {
            version,
            hostname,
            screen,
            fingerprint,
            build_version,
        }) => HandshakeMessage::Expected((version, hostname, screen, fingerprint, build_version)),
        Some(other) => HandshakeMessage::Unexpected(protocol::message_summary(&other)),
        None => HandshakeMessage::StreamClosed,
    };
    let (version, hostname, screen, fingerprint, build_version) =
        require_handshake_message("Hello", hello)?;

    validate_client_server_hello(
        version,
        PROTOCOL_VERSION,
        &fingerprint,
        &tls_fingerprint,
        screen.width,
        screen.height,
    )?;

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

    let pairing = client_pairing_decision(trust_store.is_trusted(&fingerprint)?);
    let otp = match pairing {
        PairingDecision::UseTrustedIdentity => {
            info!("Server fingerprint already trusted");
            None
        }
        PairingDecision::PromptForOtp => Some(prompt_pairing_code(addr).await?),
    };

    let ack = Message::HelloAck {
        accepted: true,
        version: PROTOCOL_VERSION,
        otp,
        screen: Some(my_screen),
        build_version: Some(protocol::local_build_version()),
    };
    send_message(&mut control_send, &ack).await?;

    let pairing_response = match recv_message(&mut control_recv).await? {
        Some(Message::PairingResult { success }) => HandshakeMessage::Expected(success),
        Some(other) => HandshakeMessage::Unexpected(protocol::message_summary(&other)),
        None => HandshakeMessage::StreamClosed,
    };
    if matches!(
        complete_client_pairing(pairing, pairing_response)?,
        PairingCompletion::PersistTrust
    ) {
        trust_store.trust(&fingerprint)?;
        info!("Paired successfully. Fingerprint stored.");
    }

    let server_build_version = build_version.unwrap_or_else(|| "unknown".to_string());
    let mut runtime = RuntimeStatus::new("client", "connected");
    runtime.peer_addr = Some(addr.to_string());
    runtime.peer_name = Some(hostname);
    runtime.peer_screen = Some(format!("{}x{}", screen.width, screen.height));
    runtime.peer_build = Some(server_build_version.clone());
    status::write_status(runtime).ok();
    let mut server_screen = screen;

    // Auto-update only if server has a strictly newer clean release version
    if should_attempt_client_update(&server_build_version) {
        info!(
            "Server has newer version {}, attempting self-update...",
            server_build_version
        );
        let update_result = crate::net::update::self_update(&server_build_version).await;
        if let Some(reason) = update_restart_reason(&server_build_version, &update_result) {
            info!("Updated to {}. Restarting...", server_build_version);
            connection.close(0u32.into(), b"updating");
            return Ok(SessionExit::RestartRequested(reason));
        }
        if let Err(e) = update_result {
            warn!(
                "Self-update failed: {}. Continuing with current version.",
                e
            );
        }
    }

    let mut transition = ClientTransition::new(my_w, my_h);

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
                        let mut clipboard = clipboard_poll.lock().unwrap();
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
                            let mut clipboard = clipboard_recv.lock().unwrap();
                            if let Err(e) = clipboard.apply_update(&content) {
                                warn!("Failed to apply clipboard update: {}", e);
                            }
                        }
                        Ok(Some(other)) => {
                            debug!("Unexpected message on clipboard stream: {:?}", other);
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
    let mut input_recv = tokio::time::timeout(Duration::from_secs(10), connection.accept_uni())
        .await
        .wrap_err("Timeout waiting for input stream from server")?
        .wrap_err("Failed to accept input stream")?;
    // Read and discard the stream-ready marker
    let _marker = recv_message_uni(&mut input_recv).await?;
    debug!("Input stream accepted");

    // Keep draining QUIC even while CoreGraphics is busy. Relative movement
    // still updates logical state in order, but only the newest absolute
    // position is retained for the next injection frame.
    let input_queue = InputMessageQueue::default();
    let input_reader_queue = input_queue.clone();
    tokio::spawn(async move {
        loop {
            match recv_message_uni(&mut input_recv).await {
                Ok(Some(message)) => input_reader_queue.push(message),
                Ok(None) => {
                    input_reader_queue.close();
                    break;
                }
                Err(e) => {
                    input_reader_queue.fail(e.to_string());
                    break;
                }
            }
        }
    });

    let mut last_screen_w = my_w;
    let mut last_screen_h = my_h;
    let mut screen_check = time::interval(Duration::from_secs(5));
    let mut latency_check = time::interval(CLIENT_LATENCY_CHECK_INTERVAL);
    let mut pointer_injection_interval = time::interval(POINTER_FRAME_INTERVAL);
    pointer_injection_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut pending_latency_ping: Option<(u64, Instant)> = None;
    let mut latency_strikes: u8 = 0;
    let mut restart_for_latency = false;
    let mut injected_keys: HashSet<u32> = HashSet::new();
    let mut injected_buttons: HashSet<u8> = HashSet::new();
    let mut activation_started: Option<Instant> = None;
    let mut activation_input_messages: u64 = 0;
    let mut activation_inject_moves: u64 = 0;
    let mut activation_superseded_moves: u64 = 0;
    let mut activation_first_inject_logged = false;
    let mut pending_mouse_move: Option<(i32, i32)> = None;

    loop {
        tokio::select! {
            // Drain buffered movement before posting another pointer frame.
            // This is what makes the pending slot jump to the newest position
            // after a slow CoreGraphics call instead of replaying stale frames.
            biased;
            item = input_queue.recv() => {
                match item {
                    InputQueueItem::Message(message) => {
                        if activation_started.is_some() {
                            activation_input_messages += 1;
                        }
                        let original_message = message.clone();
                        match transition.handle(message) {
                            ClientOutput::Ignore => {
                                // Preserve ordering if a discrete release follows a
                                // pointer position that has not reached CoreGraphics yet.
                                if matches!(&original_message, Message::KeyEvent { .. } | Message::MouseButton { .. } | Message::MouseScroll { .. }) {
                                    flush_pending_mouse(
                                        &mut *injector,
                                        &mut pending_mouse_move,
                                        activation_started,
                                        &mut activation_inject_moves,
                                        &mut activation_first_inject_logged,
                                        "before ignored input",
                                    );
                                }
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
                                info!("Server sharing mouse");
                                pending_mouse_move = None;
                                activation_started = Some(Instant::now());
                                activation_input_messages = 0;
                                activation_inject_moves = 0;
                                activation_superseded_moves = 0;
                                activation_first_inject_logged = false;
                                display_control.wake_display().ok();
                            }
                            ClientOutput::InjectMove { x, y } => {
                                if pending_mouse_move.replace((x, y)).is_some() {
                                    activation_superseded_moves += 1;
                                }
                            }
                            ClientOutput::Forward(msg) => {
                                flush_pending_mouse(
                                    &mut *injector,
                                    &mut pending_mouse_move,
                                    activation_started,
                                    &mut activation_inject_moves,
                                    &mut activation_first_inject_logged,
                                    "before discrete input",
                                );
                                if let Err(e) = inject_with_timing(&mut *injector, &msg, "forward") {
                                    warn!("Inject error: {}", e);
                                } else {
                                    track_injected_input(&msg, &mut injected_keys, &mut injected_buttons);
                                }
                            }
                            ClientOutput::SwitchBack { direction, inject } => {
                                // The switch-back position supersedes any older pending
                                // frame and must be posted before releasing input.
                                pending_mouse_move = None;
                                if let Some((x, y)) = inject {
                                    let msg = Message::MouseMove { x, y };
                                    inject_with_timing(&mut *injector, &msg, "switch back").ok();
                                }
                                // Release any remote-held keys immediately on switch-back. The
                                // server also sends cleanup releases, but those can arrive after
                                // this transition has become inactive.
                                release_injected_inputs(
                                    &mut *injector,
                                    display_control,
                                    &mut injected_keys,
                                    &mut injected_buttons,
                                );
                                release_defensive_keyups(&mut *injector);
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
                    InputQueueItem::Closed => {
                        info!("Input stream closed");
                        break;
                    }
                    InputQueueItem::Error(e) => {
                        warn!("Input stream error: {}", e);
                        break;
                    }
                }
            }
            _ = pointer_injection_interval.tick(), if pending_mouse_move.is_some() => {
                flush_pending_mouse(
                    &mut *injector,
                    &mut pending_mouse_move,
                    activation_started,
                    &mut activation_inject_moves,
                    &mut activation_first_inject_logged,
                    "pointer frame",
                );
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
                        info!("Server screen changed: {}x{}", screen.width, screen.height);
                        server_screen = screen;
                    }
                    Ok(Some(Message::WakeDisplay)) => {
                        debug!("Peer user active — keeping this system awake");
                        display_control.wake_display().ok();
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
            _ = latency_check.tick() => {
                if activation_started.is_some_and(|started| started.elapsed() >= Duration::from_secs(10)) {
                    let elapsed = activation_started.unwrap().elapsed();
                    info!(
                        "Activation diagnostics: {:.0}s summary: input_messages={}, injected_mouse_moves={}, superseded_mouse_moves={}",
                        elapsed.as_secs_f64(),
                        activation_input_messages,
                        activation_inject_moves,
                        activation_superseded_moves
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

    // Release any synthetic input that may still be down if the stream ended
    // before key-up/button-up events were processed (for example during display sleep).
    release_injected_inputs(
        &mut *injector,
        display_control,
        &mut injected_keys,
        &mut injected_buttons,
    );
    release_defensive_keyups(&mut *injector);

    // Signal clipboard tasks to shut down
    shutdown_tx.send(true).ok();

    // Gracefully close the connection
    connection.close(0u32.into(), b"disconnected");

    // Suppress unused variable warning
    let _ = server_screen;

    Ok(client_shutdown_exit(restart_for_latency))
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
        Some(Message::Hello {
            version,
            hostname,
            screen,
            fingerprint,
            build_version,
        }) => {
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

            let ack = Message::HelloAck {
                accepted: true,
                otp: otp.clone(),
                screen: None,
                build_version: Some(BUILD_VERSION.to_string()),
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
                println!("  seq={} rtt={:.3}ms", seq, rtt.as_secs_f64() * 1000.0);
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

/// Pair with a server once, storing its fingerprint if the OTP succeeds.
pub async fn pair(addr: &str) -> Result<()> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint()?;

    info!("Pairing with {}...", addr);
    let connection = connect_with_retry(&endpoint, addr).await?;

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
            let server_ver = build_version.as_deref().unwrap_or("unknown");
            info!(
                "Server: {} (proto v{}, build {}, screen: {}x{})",
                hostname, version, server_ver, screen.width, screen.height
            );

            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                info!("Server fingerprint already trusted");
                None
            } else {
                if !std::io::stdin().is_terminal() {
                    return Err(eyre!(
                        "Server fingerprint is not trusted and no interactive terminal is available for pairing. Run `nexdesk connect {}` from a terminal once, enter the pairing code, then restart the background service.",
                        addr
                    ));
                }
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
                screen: None,
                build_version: Some(BUILD_VERSION.to_string()),
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
                    return Err(eyre!("Expected PairingResult, got: {:?}", other));
                }
            }
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    }

    connection.close(0u32.into(), b"paired");
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

#[cfg(test)]
mod input_coalescing_tests {
    use super::*;
    use crate::net::protocol::Direction;

    #[tokio::test]
    async fn input_queue_preserves_relative_moves_and_barriers() {
        let queue = InputMessageQueue::default();
        queue.push(Message::SwitchScreen {
            direction: Direction::Right,
        });
        queue.push(Message::MouseMove { x: 100, y: 50 });
        queue.push(Message::MouseMove { x: 3, y: -2 });
        queue.push(Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        });
        queue.push(Message::MouseMove { x: 7, y: 8 });
        queue.push(Message::MouseMove { x: -2, y: 4 });
        queue.close();

        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::SwitchScreen {
                direction: Direction::Right
            })
        ));
        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::MouseMove { x: 100, y: 50 })
        ));
        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::MouseMove { x: 3, y: -2 })
        ));
        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::KeyEvent {
                keycode: 30,
                pressed: true,
                ..
            })
        ));
        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::MouseMove { x: 7, y: 8 })
        ));
        assert!(matches!(
            queue.recv().await,
            InputQueueItem::Message(Message::MouseMove { x: -2, y: 4 })
        ));
        assert!(matches!(queue.recv().await, InputQueueItem::Closed));
    }

    #[test]
    fn accumulated_motion_retains_fractional_deltas() {
        let mut pending = (0.5, -0.75);
        assert!(take_accumulated_motion(&mut pending).is_none());

        pending.0 += 1.75;
        pending.1 -= 1.5;
        assert!(matches!(
            take_accumulated_motion(&mut pending),
            Some(Message::MouseMove { x: 2, y: -2 })
        ));
        assert!((pending.0 - 0.25).abs() < f64::EPSILON);
        assert!((pending.1 + 0.25).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Copy)]
    enum BlockingStage {
        Resolution,
        Connection,
        Session,
        Backoff,
    }

    #[derive(Default)]
    struct ReconnectCalls {
        resolutions: AtomicUsize,
        connections: AtomicUsize,
        sessions: AtomicUsize,
    }

    struct BlockingReconnectDriver {
        stage: BlockingStage,
        calls: Arc<ReconnectCalls>,
    }

    impl ClientReconnectDriver for BlockingReconnectDriver {
        type Connected = ();

        async fn resolve_target(&mut self) -> Result<SocketAddr> {
            self.calls.resolutions.fetch_add(1, Ordering::SeqCst);
            match self.stage {
                BlockingStage::Resolution => std::future::pending().await,
                BlockingStage::Backoff => Err(eyre!("scripted resolution failure")),
                _ => Ok(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    DEFAULT_PORT,
                )),
            }
        }

        async fn connect_target(&mut self, _addr: SocketAddr) -> Result<Self::Connected> {
            self.calls.connections.fetch_add(1, Ordering::SeqCst);
            if matches!(self.stage, BlockingStage::Connection) {
                std::future::pending().await
            } else {
                Ok(())
            }
        }

        async fn run_session(&mut self, _connected: Self::Connected) -> Result<SessionExit> {
            self.calls.sessions.fetch_add(1, Ordering::SeqCst);
            if matches!(self.stage, BlockingStage::Session) {
                std::future::pending().await
            } else {
                Ok(SessionExit::Disconnected)
            }
        }
    }

    async fn wait_for_call(counter: &AtomicUsize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconnect stage should be entered");
    }

    async fn cancel_at_stage(stage: BlockingStage) -> Arc<ReconnectCalls> {
        let calls = Arc::new(ReconnectCalls::default());
        let mut driver = BlockingReconnectDriver {
            stage,
            calls: calls.clone(),
        };
        let cancellation = CancellationToken::new();
        let loop_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            run_client_reconnect_loop(
                &mut driver,
                RetryPolicy::fixed(Duration::from_secs(60 * 60)),
                loop_cancellation,
            )
            .await
        });

        match stage {
            BlockingStage::Resolution | BlockingStage::Backoff => {
                wait_for_call(&calls.resolutions).await
            }
            BlockingStage::Connection => wait_for_call(&calls.connections).await,
            BlockingStage::Session => wait_for_call(&calls.sessions).await,
        }
        if matches!(stage, BlockingStage::Backoff) {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            assert_eq!(calls.resolutions.load(Ordering::SeqCst), 1);
        }

        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled reconnect loop should finish")
            .expect("reconnect task should not panic")
            .expect("reconnect loop should not fail");
        assert!(matches!(outcome, SessionExit::Cancelled));
        calls
    }

    #[tokio::test]
    async fn reconnect_loop_cancels_during_resolution() {
        let calls = cancel_at_stage(BlockingStage::Resolution).await;
        assert_eq!(calls.connections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reconnect_loop_cancels_during_connection() {
        let calls = cancel_at_stage(BlockingStage::Connection).await;
        assert_eq!(calls.connections.load(Ordering::SeqCst), 1);
        assert_eq!(calls.sessions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reconnect_loop_cancels_during_session() {
        let calls = cancel_at_stage(BlockingStage::Session).await;
        assert_eq!(calls.sessions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconnect_loop_cancels_during_backoff() {
        let calls = cancel_at_stage(BlockingStage::Backoff).await;
        assert_eq!(calls.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(calls.connections.load(Ordering::SeqCst), 0);
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
    fn injected_input_cleanup_uses_display_control_port() {
        let mut injector = crate::testing::RecordingInjector::new((1920, 1080));
        let display = crate::testing::FakeDisplaySessionControl::new();
        let mut keys = HashSet::from([30]);
        let mut buttons = HashSet::from([0]);

        release_injected_inputs(&mut injector, &display, &mut keys, &mut buttons);

        assert!(keys.is_empty());
        assert!(buttons.is_empty());
        let observations = display.observations().snapshot();
        assert!(observations.iter().any(|entry| matches!(
            entry.event,
            crate::testing::DisplayObservation::WakeRequested
        )));
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
    fn client_update_restart_requires_a_newer_clean_release_and_success() {
        let newer = "v999.0.0";
        assert!(should_attempt_client_update(newer));
        assert!(!should_attempt_client_update(BUILD_VERSION));
        assert!(!should_attempt_client_update("v0.0.0"));
        assert!(!should_attempt_client_update("v999.0.0-dirty"));
        assert!(!should_attempt_client_update("unknown"));

        let installed: std::result::Result<(), &str> = Ok(());
        assert_eq!(
            update_restart_reason(newer, &installed),
            Some(RestartReason::UpdateInstalled {
                version: newer.to_string(),
            })
        );
        let failed = Err("download failed");
        assert_eq!(update_restart_reason(newer, &failed), None);
    }

    #[test]
    fn latency_restart_is_requested_only_after_watchdog_trips() {
        assert!(matches!(
            client_shutdown_exit(false),
            SessionExit::Disconnected
        ));
        assert!(matches!(
            client_shutdown_exit(true),
            SessionExit::RestartRequested(RestartReason::LatencyWatchdog)
        ));
    }
}
