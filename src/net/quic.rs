use std::collections::HashSet;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::{Endpoint, RecvStream, SendStream};
use rand::Rng;
use tokio::sync::Mutex;
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

fn release_injected_inputs(
    injector: &mut dyn InputInjector,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) {
    if injected_keys.is_empty() && injected_buttons.is_empty() {
        return;
    }

    crate::input::wake::wake_display();

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
                    ServerOutput::ShortcutRelease { messages } => {
                        info!("Shortcut switch back — releasing grab");
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        capturer.lock().unwrap().set_grab(false).ok();
                    }
                    ServerOutput::ForceRelease { messages } => {
                        warn!("Safety escape (Ctrl+Alt+Escape) — releasing grab");
                        let mut sender = input_send.lock().await;
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
                            let msg = Message::MouseMove { x: dx as i32, y: dy as i32 };
                            let mut sender = input_send.lock().await;
                            if let Err(e) = send_message_uni(&mut sender, &msg).await {
                                warn!("Failed to send mouse move: {}", e);
                                transition.deactivate();
                                if let Some(ref tx) = capture_tx {
                                    tx.send(LayerShellCommand::Release).ok();
                                }
                                capturer.lock().unwrap().set_keyboard_grab(false).ok();
                                layer_shell_keyboard_grabbed = false;
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
                            transition.update_key(keycode, pressed);
                            if transition.is_escape_combo() {
                                warn!("Safety escape (Ctrl+Alt+Escape) — releasing layer-shell grab");
                                let messages = transition.deactivate_for_shortcut();
                                let mut sender = input_send.lock().await;
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
                        crate::input::wake::wake_display();
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

/// Connect to a QUIC server as a client (receives and injects input).
/// If `addr` is None, discovers the server via mDNS.
/// Automatically reconnects with exponential backoff on disconnection.
pub async fn connect(addr: Option<&str>) -> Result<()> {
    let resolved_addr = match addr {
        Some(a) => Some(resolve_addr(a)?),
        None => None,
    };

    let endpoint = make_client_endpoint()?;

    loop {
        let target_addr = match resolved_addr {
            Some(a) => a,
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

    info!("Connected to {}", addr);
    let mut runtime = RuntimeStatus::new("client", "connected");
    runtime.peer_addr = Some(addr.to_string());
    status::write_status(runtime).ok();

    // Create input injector early so we can send screen size in handshake
    let mut injector = crate::input::inject::create_injector()?;
    let (my_w, my_h) = injector.screen_size()?;
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
                if !std::io::stdin().is_terminal() {
                    return Err(eyre!(
                        "Server fingerprint is not trusted and no interactive terminal is available for pairing. Run `nexdesk connect {}` from a terminal once, enter the pairing code, then restart the background service.",
                        addr
                    ));
                }
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
                screen: Some(ScreenLayout {
                    width: my_w,
                    height: my_h,
                }),
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
            let mut runtime = RuntimeStatus::new("client", "connected");
            runtime.peer_addr = Some(addr.to_string());
            runtime.peer_name = Some(hostname);
            runtime.peer_screen = Some(format!("{}x{}", screen.width, screen.height));
            runtime.peer_build = Some(ver.clone());
            status::write_status(runtime).ok();
            (screen, ver)
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    };

    // Auto-update only if server has a strictly newer clean release version
    if server_build_version != BUILD_VERSION {
        if crate::net::update::is_release_version(&server_build_version)
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
                    let exe = std::env::current_exe()?;
                    let args: Vec<String> = std::env::args().collect();
                    use std::os::unix::process::CommandExt;
                    let err = std::process::Command::new(exe).args(&args[1..]).exec();
                    return Err(eyre!("Failed to restart after update: {}", err));
                }
                Err(e) => {
                    warn!(
                        "Self-update failed: {}. Continuing with current version.",
                        e
                    );
                }
            }
        }
    }

    let mut transition = ClientTransition::new(my_w, my_h);

    // Shutdown signal for clipboard tasks
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

    let mut last_screen_w = my_w;
    let mut last_screen_h = my_h;
    let mut screen_check = time::interval(Duration::from_secs(5));
    let mut injected_keys: HashSet<u32> = HashSet::new();
    let mut injected_buttons: HashSet<u8> = HashSet::new();

    loop {
        tokio::select! {
            msg = recv_message_uni(&mut input_recv) => {
                match msg {
                    Ok(Some(message)) => {
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
                                } else {
                                    track_injected_input(&msg, &mut injected_keys, &mut injected_buttons);
                                }
                            }
                            ClientOutput::SwitchBack { direction, inject } => {
                                if let Some((x, y)) = inject {
                                    let msg = Message::MouseMove { x, y };
                                    injector.inject(&msg).ok();
                                }
                                // Release any remote-held keys immediately on switch-back. The
                                // server also sends cleanup releases, but those can arrive after
                                // this transition has become inactive.
                                release_injected_inputs(&mut *injector, &mut injected_keys, &mut injected_buttons);
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
                        debug!("Peer user active — keeping this system awake");
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

    // Release any synthetic input that may still be down if the stream ended
    // before key-up/button-up events were processed (for example during display sleep).
    release_injected_inputs(&mut *injector, &mut injected_keys, &mut injected_buttons);
    release_defensive_keyups(&mut *injector);

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
