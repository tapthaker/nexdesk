use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result, WrapErr};
use quinn::Endpoint;
use rand::Rng;
use tokio::sync::Notify;
use tokio::time::{self, Instant};
use tracing::{debug, error, info, warn};

use crate::app::{
    client_channel_disposition, client_pairing_decision, complete_client_pairing,
    decide_server_handshake, execute_update, require_handshake_message, server_channel_disposition,
    validate_client_server_hello, CancellationToken, ClientChannelDisposition, HandshakeMessage,
    PairingCompletion, PairingDecision, RestartReason, RetryPolicy, ServerChannelDisposition,
    ServerHandshakeDecision, ServerHelloAck, ServerPairingMethod, SessionExit, UpdateExecution,
    UpdatePolicy, UpdateSource,
};
use crate::clipboard::PlatformClipboard;
use crate::input::capture::{InputCapture, InputCaptureFactory, PlatformInputCaptureFactory};
use crate::input::inject::{InputInjector, InputInjectorFactory, PlatformInputInjectorFactory};
use crate::input::wake::PlatformDisplaySessionControl;
use crate::net::discovery;
use crate::net::framing::{recv_message, send_message};
use crate::net::pairing::{self, TerminalPairingPrompt};
use crate::net::protocol::{self, Message, ScreenLayout, BUILD_VERSION, PROTOCOL_VERSION};
use crate::net::quinn_client::QuinnClientPeerLink;
use crate::net::quinn_server::QuinnServerPeerLink;
use crate::net::tls;
use crate::net::tls::ConfigTrustStore;
use crate::net::transition::{ClientOutput, ClientTransition, ServerOutput, ServerTransition};
use crate::net::update::{ExecutableUpdateInstaller, GithubReleaseRepository};
use crate::ports::{
    ClientChannel, ClientClipboardCommand, ClientClipboardEvent, ClientControlCommand,
    ClientControlEvent, ClientInputEvent, ClientPeerLink, ClientTransportEvent, Clipboard,
    DisplaySessionControl, LocalSessionLockSource, PairingPrompt, PeerDirection, PeerScreen,
    PeerScrollPhase, Release, ReleaseRepository, ServerClipboardCommand, ServerClipboardEvent,
    ServerControlCommand, ServerControlEvent, ServerInputCommand, ServerPeerLink,
    ServerTransportEvent, StatusSink, TrustStore, UpdateInstaller,
};
use crate::status::{self, FileStatusSink, RuntimeStatus};

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

fn inject_late_release(
    injector: &mut dyn InputInjector,
    message: &Message,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) -> Result<bool> {
    let release = match message {
        Message::KeyEvent {
            keycode,
            pressed: false,
            ..
        } if injected_keys.contains(keycode) => Message::KeyEvent {
            keycode: *keycode,
            pressed: false,
            modifiers: 0,
        },
        Message::MouseButton {
            button,
            pressed: false,
        } if injected_buttons.contains(button) => Message::MouseButton {
            button: *button,
            pressed: false,
        },
        _ => return Ok(false),
    };

    injector.inject(&release)?;
    track_injected_input(&release, injected_keys, injected_buttons);
    Ok(true)
}

fn request_server_display_wake(display_control: Arc<dyn DisplaySessionControl>) {
    std::thread::spawn(move || {
        if let Err(error) = display_control.wake_display() {
            warn!("Failed to wake server display: {}", error);
        }
    });
}

async fn notify_after_local_input_release<T>(
    release: impl FnOnce(),
    notification: impl std::future::Future<Output = T>,
) -> T {
    release();
    notification.await
}

fn restore_server_input_state(
    capturer: &mut dyn InputCapture,
    keyboard_only: bool,
    transition: &mut ServerTransition,
) -> Vec<Message> {
    let releases = transition.release_remote_inputs();
    transition.deactivate();
    if keyboard_only {
        capturer.set_keyboard_grab(false).ok();
    } else {
        capturer.set_grab(false).ok();
    }
    releases
}

fn server_session_is_locked(lock_source: &dyn LocalSessionLockSource) -> bool {
    match lock_source.is_locked() {
        Ok(locked) => locked,
        Err(error) => {
            warn!("Failed to query local session lock state: {}", error);
            false
        }
    }
}

fn require_layer_shell_event(
    event: Option<crate::input::wayland_layer_shell::LayerShellEvent>,
) -> Result<crate::input::wayland_layer_shell::LayerShellEvent> {
    event.ok_or_else(|| eyre!("layer-shell capture stopped"))
}

fn validate_discovered_fingerprint(expected: Option<&str>, actual: &str) -> Result<String> {
    let actual = actual.to_uppercase();
    if let Some(expected) = expected {
        let expected = expected.to_uppercase();
        if expected != actual {
            return Err(eyre!(
                "Discovered server fingerprint mismatch: expected {}, received {}",
                expected,
                actual
            ));
        }
    }
    Ok(actual)
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

type SharedClipboardSync = Arc<std::sync::Mutex<crate::clipboard::sync::ClipboardSync>>;

async fn poll_clipboard_on_worker(clipboard: SharedClipboardSync) -> Result<Option<Message>> {
    tokio::task::spawn_blocking(move || {
        let mut clipboard = lock_recover(&clipboard, "clipboard");
        clipboard.poll_change()
    })
    .await
    .wrap_err("Clipboard polling worker failed")?
}

async fn apply_clipboard_on_worker(
    clipboard: SharedClipboardSync,
    content: protocol::ClipboardContent,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut clipboard = lock_recover(&clipboard, "clipboard");
        clipboard.apply_update(&content)
    })
    .await
    .wrap_err("Clipboard update worker failed")?
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

fn restore_client_input_state(
    injector: &mut dyn InputInjector,
    display_control: &dyn DisplaySessionControl,
    injected_keys: &mut HashSet<u32>,
    injected_buttons: &mut HashSet<u8>,
) {
    release_injected_inputs(injector, display_control, injected_keys, injected_buttons);
    release_defensive_keyups(injector);
    if let Err(error) = injector.set_cursor_visible(true) {
        warn!("Failed to restore client cursor visibility: {}", error);
    }
}

async fn terminate_server_tasks(tasks: &mut tokio::task::JoinSet<()>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn shutdown_server_connection_tasks(
    tasks: &mut tokio::task::JoinSet<()>,
    peer: &dyn ServerPeerLink,
) {
    terminate_server_tasks(tasks).await;
    peer.shutdown().await;
}

async fn terminate_client_tasks(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
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

async fn send_user_activity(peer: &dyn ServerPeerLink, last_sent: &mut Instant) {
    if last_sent.elapsed() < USER_ACTIVITY_INTERVAL {
        return;
    }

    if peer
        .send_control(ServerControlCommand::WakePeerDisplay)
        .await
        .is_ok()
    {
        *last_sent = Instant::now();
    }
}

fn server_input_command(message: Message) -> Result<ServerInputCommand> {
    let command = match message {
        Message::MouseMove { x, y } => ServerInputCommand::MouseMoved { x, y },
        Message::MouseButton { button, pressed } => {
            ServerInputCommand::MouseButtonChanged { button, pressed }
        }
        Message::MouseScroll { dx, dy, phase } => ServerInputCommand::MouseScrolled {
            dx,
            dy,
            phase: match phase {
                protocol::ScrollPhase::None => PeerScrollPhase::None,
                protocol::ScrollPhase::Began => PeerScrollPhase::Began,
                protocol::ScrollPhase::Changed => PeerScrollPhase::Changed,
                protocol::ScrollPhase::Ended => PeerScrollPhase::Ended,
            },
        },
        Message::KeyEvent {
            keycode,
            pressed,
            modifiers,
        } => ServerInputCommand::KeyChanged {
            keycode,
            pressed,
            modifiers,
        },
        Message::SwitchScreen { direction } => ServerInputCommand::SwitchToPeer {
            direction: peer_direction(direction),
        },
        other => {
            return Err(eyre!(
                "Message is not a server input command: {}",
                protocol::message_summary(&other)
            ));
        }
    };
    Ok(command)
}

async fn send_server_input_messages(
    peer: &dyn ServerPeerLink,
    messages: impl IntoIterator<Item = Message>,
) -> Result<()> {
    for message in messages {
        peer.send_input(server_input_command(message)?).await?;
    }
    Ok(())
}

#[derive(Clone)]
struct TypedServerInputSender {
    peer: Arc<dyn ServerPeerLink>,
}

impl TypedServerInputSender {
    async fn lock(&self) -> TypedServerInputGuard {
        TypedServerInputGuard {
            peer: self.peer.clone(),
        }
    }
}

struct TypedServerInputGuard {
    peer: Arc<dyn ServerPeerLink>,
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

fn validated_peer_screen(screen: PeerScreen) -> Option<ScreenLayout> {
    (screen.width > 0 && screen.height > 0).then_some(ScreenLayout {
        width: screen.width,
        height: screen.height,
    })
}

fn protocol_direction(direction: PeerDirection) -> protocol::Direction {
    match direction {
        PeerDirection::Left => protocol::Direction::Left,
        PeerDirection::Right => protocol::Direction::Right,
        PeerDirection::Up => protocol::Direction::Up,
        PeerDirection::Down => protocol::Direction::Down,
    }
}

fn peer_direction(direction: protocol::Direction) -> PeerDirection {
    match direction {
        protocol::Direction::Left => PeerDirection::Left,
        protocol::Direction::Right => PeerDirection::Right,
        protocol::Direction::Up => PeerDirection::Up,
        protocol::Direction::Down => PeerDirection::Down,
    }
}

fn protocol_input_message(event: ClientInputEvent) -> Message {
    match event {
        ClientInputEvent::MouseMoved { x, y } => Message::MouseMove { x, y },
        ClientInputEvent::MouseButtonChanged { button, pressed } => {
            Message::MouseButton { button, pressed }
        }
        ClientInputEvent::MouseScrolled { dx, dy, phase } => Message::MouseScroll {
            dx,
            dy,
            phase: match phase {
                PeerScrollPhase::None => protocol::ScrollPhase::None,
                PeerScrollPhase::Began => protocol::ScrollPhase::Began,
                PeerScrollPhase::Changed => protocol::ScrollPhase::Changed,
                PeerScrollPhase::Ended => protocol::ScrollPhase::Ended,
            },
        },
        ClientInputEvent::KeyChanged {
            keycode,
            pressed,
            modifiers,
        } => Message::KeyEvent {
            keycode,
            pressed,
            modifiers,
        },
        ClientInputEvent::SwitchToClient { direction } => Message::SwitchScreen {
            direction: protocol_direction(direction),
        },
    }
}

async fn attempt_client_update(
    server_build: &str,
    source: UpdateSource,
    repository: &dyn ReleaseRepository,
    installer: &dyn UpdateInstaller,
) -> Result<UpdateExecution> {
    execute_update(
        &UpdatePolicy::new(BUILD_VERSION),
        Release::new(server_build),
        source,
        repository,
        installer,
    )
    .await
}

#[derive(Debug, Default)]
struct ClientLatencyWatchdog {
    pending: Option<(u64, Instant)>,
    strikes: u8,
}

impl ClientLatencyWatchdog {
    fn record_ping(&mut self, timestamp: u64, now: Instant) {
        self.pending = Some((timestamp, now));
    }

    fn acknowledge(&mut self, timestamp: u64, now: Instant) -> Option<Duration> {
        let (pending_timestamp, sent_at) = self.pending?;
        if timestamp != pending_timestamp {
            return None;
        }
        let rtt = now.saturating_duration_since(sent_at);
        self.pending = None;
        if rtt > CLIENT_LATENCY_RESTART_THRESHOLD {
            self.strikes = self.strikes.saturating_add(1);
        } else {
            self.strikes = 0;
        }
        Some(rtt)
    }

    fn expire_pending(&mut self, now: Instant) -> Option<Duration> {
        let (_, sent_at) = self.pending?;
        let elapsed = now.saturating_duration_since(sent_at);
        if elapsed <= CLIENT_LATENCY_RESTART_THRESHOLD {
            return None;
        }
        self.strikes = self.strikes.saturating_add(1);
        self.pending = None;
        Some(elapsed)
    }

    fn needs_ping(&self) -> bool {
        self.pending.is_none()
    }

    fn should_restart(&self) -> bool {
        self.strikes >= CLIENT_LATENCY_RESTART_STRIKES
    }
}

fn client_shutdown_exit(restart_for_latency: bool) -> SessionExit {
    if restart_for_latency {
        SessionExit::RestartRequested(RestartReason::LatencyWatchdog)
    } else {
        SessionExit::Disconnected
    }
}

fn create_server_capturer(
    capture_factory: &dyn InputCaptureFactory,
) -> Result<(Box<dyn InputCapture>, (u32, u32))> {
    let capturer = capture_factory.create()?;
    let screen_size = capturer.screen_size()?;
    Ok((capturer, screen_size))
}

fn poll_server_capture(
    capturer: &mut dyn InputCapture,
) -> Result<(i32, i32, u32, u32, u8, Vec<Message>)> {
    let key_events = capturer.poll_key_events()?;
    let (x, y) = capturer.mouse_position()?;
    let (width, height) = capturer.screen_size()?;
    let buttons = capturer.mouse_buttons()?;
    Ok((x, y, width, height, buttons, key_events))
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
    serve_with_dependencies(
        port,
        trigger_edge,
        Arc::new(PlatformInputCaptureFactory),
        Arc::new(crate::input::session::PlatformLocalSessionLockSource),
        Arc::new(PlatformDisplaySessionControl),
    )
    .await
}

async fn serve_with_dependencies(
    port: u16,
    trigger_edge: Option<crate::net::protocol::Direction>,
    capture_factory: Arc<dyn InputCaptureFactory>,
    lock_source: Arc<dyn LocalSessionLockSource>,
    display_control: Arc<dyn DisplaySessionControl>,
) -> Result<()> {
    validate_listen_port(port)?;
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
        let capture_factory = capture_factory.clone();
        let lock_source = lock_source.clone();
        let display_control = display_control.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_server_connection(
                connection,
                edge,
                &otp,
                &fp,
                capture_factory.as_ref(),
                lock_source.as_ref(),
                display_control,
            )
            .await
            {
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
    capture_factory: &dyn InputCaptureFactory,
    lock_source: &dyn LocalSessionLockSource,
    display_control: Arc<dyn DisplaySessionControl>,
) -> Result<()> {
    let remote = connection.remote_address();

    // Create input capturer
    let (capturer, (screen_w, screen_h)) = create_server_capturer(capture_factory)?;

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

    // Receive HelloAck with optional OTP. Stream decoding stays here while
    // the handshake policy is decided independently of Quinn.
    let response = match recv_message(&mut control_recv).await? {
        Some(Message::HelloAck {
            accepted,
            otp,
            screen,
            build_version,
        }) => HandshakeMessage::Expected(ServerHelloAck {
            accepted,
            otp,
            screen: screen.map(|screen| PeerScreen {
                width: screen.width,
                height: screen.height,
            }),
            build_version,
        }),
        Some(other) => HandshakeMessage::Unexpected(protocol::message_summary(&other)),
        None => HandshakeMessage::StreamClosed,
    };
    let decision = decide_server_handshake(server_otp, BUILD_VERSION, response);
    let pairing_result = match &decision {
        ServerHandshakeDecision::Accept { pairing_result, .. } => Some(*pairing_result),
        ServerHandshakeDecision::Reject { pairing_result, .. } => *pairing_result,
    };
    if let Some(success) = pairing_result {
        send_message(&mut control_send, &Message::PairingResult { success }).await?;
    }
    let outcome = decision.into_result()?;
    match outcome.pairing_method {
        ServerPairingMethod::Otp => info!("Peer {} paired successfully via OTP", remote),
        ServerPairingMethod::TrustedCertificate => {
            info!("Peer {} reconnected (already trusts certificate)", remote)
        }
    }
    info!(
        "Peer {} build version: {}",
        remote, outcome.peer_build_version
    );
    if outcome.version_mismatch {
        warn!(
            "Version mismatch: server={}, client={}",
            BUILD_VERSION, outcome.peer_build_version
        );
    }
    let peer_screen = ScreenLayout {
        width: outcome.peer_screen.width,
        height: outcome.peer_screen.height,
    };

    let peer: Arc<dyn ServerPeerLink> =
        Arc::new(QuinnServerPeerLink::open(&connection, control_send, control_recv).await?);
    info!("Typed server channels opened");
    let input_send = TypedServerInputSender { peer: peer.clone() };
    let mut connection_tasks = tokio::task::JoinSet::new();

    // Keep all clipboard commands on a dedicated worker path. Linux clipboard
    // owners may daemonize or stall, so the server control/input loop must never
    // await a clipboard command (heartbeats and disconnects still need service).
    let clipboard = Arc::new(std::sync::Mutex::new(
        crate::clipboard::sync::ClipboardSync::new(Arc::new(PlatformClipboard)),
    ));
    let clipboard_worker = clipboard.clone();
    let clipboard_peer = peer.clone();
    // A watch channel keeps only the newest peer clipboard while an OS command
    // is in flight, preventing stale updates from queuing behind a slow owner.
    let (clipboard_update_tx, mut clipboard_updates) =
        tokio::sync::watch::channel(None::<protocol::ClipboardContent>);
    connection_tasks.spawn(async move {
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let msg = poll_clipboard_on_worker(clipboard_worker.clone()).await;
                    if let Ok(Some(Message::ClipboardUpdate {
                        content: protocol::ClipboardContent::Text(text),
                    })) = msg
                    {
                        if clipboard_peer
                            .send_clipboard(ServerClipboardCommand::SetPeerText(text))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                result = clipboard_updates.changed() => {
                    if result.is_err() {
                        break;
                    }
                    let content = { clipboard_updates.borrow_and_update().clone() };
                    if let Some(content) = content {
                        if let Err(error) = apply_clipboard_on_worker(
                            clipboard_worker.clone(),
                            content,
                        ).await {
                            warn!("Failed to apply clipboard update: {}", error);
                        }
                    }
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
    runtime.peer_build = Some("unknown".to_string());
    status::write_status(runtime).ok();
    let mut transition = ServerTransition::new(trigger_edge, peer_screen);

    // Spawn file transfer acceptor (receives files from client via new bi-streams)
    let ft_conn = connection.clone();
    connection_tasks.spawn(async move {
        let mut transfers = crate::filetransfer::supervisor::TransferTaskSet::default();
        loop {
            tokio::select! {
                result = ft_conn.accept_bi() => match result {
                Ok((send, recv)) => {
                    if !transfers.try_spawn(async move {
                        match crate::filetransfer::recv::receive_files(send, recv).await {
                            Ok(paths) if !paths.is_empty() => {
                                info!("Received {} file(s) from client", paths.len());
                                tokio::task::spawn_blocking(move || {
                                    PlatformClipboard.write_files(&paths).ok();
                                })
                                .await
                                .ok();
                            }
                            Ok(_) => {}
                            Err(e) => warn!("File transfer receive error: {}", e),
                        }
                    }) {
                        warn!("Rejecting file transfer: concurrent transfer limit reached");
                    }
                }
                Err(_) => break,
                },
                _ = transfers.join_next(), if !transfers.is_empty() => {}
            }
        }
        transfers.shutdown().await;
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
                let capture = {
                    let mut cap = capturer.lock().unwrap();
                    poll_server_capture(&mut **cap)
                };
                let (mx, my, sw, sh, buttons, key_events) = match capture {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        warn!("Input capture failed: {}", error);
                        break;
                    }
                };

                let has_input = (mx, my) != prev_mouse_pos || !key_events.is_empty() || buttons != 0;
                if has_input {
                    send_user_activity(peer.as_ref(), &mut last_user_activity_sent).await;
                    prev_mouse_pos = (mx, my);
                }

                // Log position every 500 polls (~1 second)
                debug_counter += 1;
                if debug_counter.is_multiple_of(500) {
                    let clamped_x = mx.clamp(0, sw as i32 - 1);
                    let clamped_y = my.clamp(0, sh as i32 - 1);
                    debug!("Mouse: ({}, {}) raw: ({}, {}) screen: {}x{}", clamped_x, clamped_y, mx, my, sw, sh);
                }

                match transition.poll(mx, my, sw, sh, buttons, key_events) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { messages, grab } => {
                        info!("Edge detected — switching to remote");
                        pending_layer_shell_motion = (0.0, 0.0);
                        capturer.lock().unwrap().set_grab(grab).ok();
                        let mut sender = input_send.lock().await;
                        for msg in messages {
                            send_message_uni(&mut sender, &msg).await.ok();
                        }
                        // Check clipboard for files and transfer them
                        let ft_conn = connection.clone();
                        connection_tasks.spawn(async move {
                            let files = tokio::task::spawn_blocking(|| {
                                PlatformClipboard.read_files().ok()
                            }).await.ok().flatten();
                            if let Some(files) = files.filter(|files| !files.is_empty()) {
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
                let key_events = {
                    let mut cap = capturer.lock().unwrap();
                    cap.poll_key_events().map(|events| {
                        events
                            .into_iter()
                            .filter(|msg| matches!(msg, Message::KeyEvent { .. }))
                            .collect::<Vec<_>>()
                    })
                };
                let key_events = match key_events {
                    Ok(events) => events,
                    Err(error) => {
                        warn!("Input key capture failed: {}", error);
                        break;
                    }
                };

                if !key_events.is_empty() {
                    send_user_activity(peer.as_ref(), &mut last_user_activity_sent).await;
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
            event = async {
                match &mut capture_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if use_layer_shell => {
                use crate::input::wayland_layer_shell::{LayerShellEvent, LayerShellCommand};
                let event = match require_layer_shell_event(event) {
                    Ok(event) => event,
                    Err(error) => {
                        warn!("{}; ending the connection so capture can be recreated", error);
                        break;
                    }
                };
                send_user_activity(peer.as_ref(), &mut last_user_activity_sent).await;

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
                        connection_tasks.spawn(async move {
                            let files = tokio::task::spawn_blocking(|| {
                                PlatformClipboard.read_files().ok()
                            }).await.ok().flatten();
                            if let Some(files) = files.filter(|files| !files.is_empty()) {
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
                            let btn_id = layer_shell_button_to_protocol(button)
                                .unwrap_or(button as u8);
                            transition.update_button(btn_id, pressed);
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
                    LayerShellEvent::KeyModifiers => {
                        // Modifier state is tracked via KeyEvent; modifiers event is informational
                    }
                }
            }
            // Branch: typed peer events
            event = peer.next_event() => {
                match event {
                    Some(ServerTransportEvent::Control(ServerControlEvent::Heartbeat { timestamp })) => {
                        peer.send_control(ServerControlCommand::AcknowledgeHeartbeat { timestamp }).await?;
                    }
                    Some(ServerTransportEvent::Control(ServerControlEvent::SwitchBackRequested { direction })) => {
                        request_server_display_wake(display_control.clone());
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
                    Some(ServerTransportEvent::Control(ServerControlEvent::PeerScreenChanged(screen))) => {
                        info!("Peer screen updated: {}x{}", screen.width, screen.height);
                        transition.update_peer_screen(ScreenLayout {
                            width: screen.width,
                            height: screen.height,
                        });
                    }
                    Some(ServerTransportEvent::Clipboard(ServerClipboardEvent::TextChanged(text))) => {
                        if clipboard_update_tx
                            .send(Some(protocol::ClipboardContent::Text(text)))
                            .is_err()
                        {
                            warn!("Clipboard worker stopped before peer update was applied");
                        }
                    }
                    Some(ServerTransportEvent::Closed(channel)) => {
                        info!("Peer {} {:?} channel closed", remote, channel);
                        if server_channel_disposition(channel) == ServerChannelDisposition::Disconnect {
                            break;
                        }
                    }
                    Some(ServerTransportEvent::Failed(failure)) => {
                        warn!("Peer {} {:?} channel failed: {}", remote, failure.channel, failure.message);
                        if server_channel_disposition(failure.channel) == ServerChannelDisposition::Disconnect {
                            break;
                        }
                    }
                    None => {
                        info!("Peer {} transport closed", remote);
                        break;
                    }
                }
            }
            _ = local_lock_check.tick(), if transition.is_active() => {
                if server_session_is_locked(lock_source) {
                    warn!("Local session locked while sharing — releasing remote control so Linux can be unlocked locally");
                    let messages = transition.deactivate_for_shortcut();
                    notify_after_local_input_release(
                        || {
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
                        },
                        async {
                            if !messages.is_empty() {
                                let mut sender = input_send.lock().await;
                                for msg in messages {
                                    send_message_uni(&mut sender, &msg).await.ok();
                                }
                            }
                        },
                    )
                    .await;
                }
            }
            _ = screen_check.tick() => {
                let size = capturer.lock().unwrap().screen_size();
                let size = match size {
                    Ok(size) => size,
                    Err(error) => {
                        warn!("Screen capture failed: {}", error);
                        break;
                    }
                };
                if size.0 != last_screen_w || size.1 != last_screen_h {
                    info!("Screen size changed: {}x{} -> {}x{}", last_screen_w, last_screen_h, size.0, size.1);
                    last_screen_w = size.0;
                    last_screen_h = size.1;
                    let resize = ServerControlCommand::LocalScreenChanged(PeerScreen {
                        width: size.0,
                        height: size.1,
                    });
                    if let Err(e) = peer.send_control(resize).await {
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
    }
    let release_messages = {
        let mut capturer = capturer.lock().unwrap();
        restore_server_input_state(&mut **capturer, use_layer_shell, &mut transition)
    };
    if !release_messages.is_empty() {
        match time::timeout(
            Duration::from_millis(250),
            send_server_input_messages(peer.as_ref(), release_messages),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("Failed to release remote input during cleanup: {}", error),
            Err(_) => warn!("Timed out releasing remote input during cleanup"),
        }
    }

    shutdown_server_connection_tasks(&mut connection_tasks, peer.as_ref()).await;
    Ok(())
}

fn normalize_connect_addr_input(addr: &str) -> Result<&str> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err(eyre!("Connect address cannot be empty"));
    }
    Ok(addr)
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
    expected_fingerprint: Option<String>,
    injector_factory: Arc<dyn InputInjectorFactory>,
    display_control: Arc<dyn DisplaySessionControl>,
    trust_store: Arc<dyn TrustStore>,
    pairing_prompt: Arc<dyn PairingPrompt>,
    release_repository: Arc<dyn ReleaseRepository>,
    update_installer: Arc<dyn UpdateInstaller>,
    clipboard: Arc<dyn Clipboard>,
    status_sink: Arc<dyn StatusSink>,
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
            None => {
                let fingerprint = self.expected_fingerprint.as_deref().ok_or_else(|| {
                    eyre!("No server fingerprint configured; run `nexdesk setup`")
                })?;
                discovery::discover_one(fingerprint, Duration::from_secs(10)).await
            }
        }
    }

    async fn connect_target(&mut self, addr: SocketAddr) -> Result<Self::Connected> {
        info!("Connecting to nexdesk server at {}", addr);
        let endpoint = make_client_endpoint()?;
        let mut runtime = RuntimeStatus::new("client", "connecting");
        runtime.peer_addr = Some(addr.to_string());
        self.status_sink.write(runtime).ok();
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
            self.pairing_prompt.as_ref(),
            self.release_repository.as_ref(),
            self.update_installer.as_ref(),
            self.clipboard.clone(),
            self.status_sink.as_ref(),
        )
        .await
    }

    fn record_disconnected(&mut self, addr: SocketAddr) {
        let mut runtime = RuntimeStatus::new("client", "disconnected");
        runtime.peer_addr = Some(addr.to_string());
        self.status_sink.write(runtime).ok();
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
    let expected_fingerprint = if addr.is_none() {
        crate::config::NexdeskConfig::load()?
            .server_fingerprint
            .map(|fingerprint| fingerprint.to_uppercase())
    } else {
        None
    };
    connect_with_dependencies(
        addr,
        expected_fingerprint,
        cancellation,
        Arc::new(PlatformInputInjectorFactory),
        Arc::new(PlatformDisplaySessionControl),
        Arc::new(ConfigTrustStore),
        Arc::new(TerminalPairingPrompt::new()),
        Arc::new(GithubReleaseRepository),
        Arc::new(ExecutableUpdateInstaller),
        Arc::new(PlatformClipboard),
        Arc::new(FileStatusSink),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "composition root wires explicit client capability ports"
)]
async fn connect_with_dependencies(
    addr: Option<&str>,
    expected_fingerprint: Option<String>,
    cancellation: CancellationToken,
    injector_factory: Arc<dyn InputInjectorFactory>,
    display_control: Arc<dyn DisplaySessionControl>,
    trust_store: Arc<dyn TrustStore>,
    pairing_prompt: Arc<dyn PairingPrompt>,
    release_repository: Arc<dyn ReleaseRepository>,
    update_installer: Arc<dyn UpdateInstaller>,
    clipboard: Arc<dyn Clipboard>,
    status_sink: Arc<dyn StatusSink>,
) -> Result<SessionExit> {
    let explicit_addr = explicit_connect_addr_arg(addr)?;
    let _idle_sleep_inhibitor = display_control.inhibit_idle_sleep()?;
    let mut driver = ProductionClientDriver {
        explicit_addr,
        expected_fingerprint,
        injector_factory,
        display_control,
        trust_store,
        pairing_prompt,
        release_repository,
        update_installer,
        clipboard,
        status_sink,
    };
    run_client_reconnect_loop(&mut driver, RetryPolicy::default(), cancellation).await
}

/// Handle one established client connection, including handshake and session loops.
#[allow(
    clippy::too_many_arguments,
    reason = "session boundary receives explicit transport and platform capabilities"
)]
async fn run_client_session(
    connection: quinn::Connection,
    addr: SocketAddr,
    injector_factory: &dyn InputInjectorFactory,
    display_control: &dyn DisplaySessionControl,
    trust_store: &dyn TrustStore,
    pairing_prompt: &dyn PairingPrompt,
    release_repository: &dyn ReleaseRepository,
    update_installer: &dyn UpdateInstaller,
    clipboard_port: Arc<dyn Clipboard>,
    status_sink: &dyn StatusSink,
) -> Result<SessionExit> {
    let tls_fingerprint = tls::peer_fingerprint(&connection)
        .ok_or_else(|| eyre!("Server did not present a certificate"))?;

    info!("Connected to {}", addr);
    let mut runtime = RuntimeStatus::new("client", "connected");
    runtime.peer_addr = Some(addr.to_string());
    status_sink.write(runtime).ok();

    // Create input injector early so we can send screen size in handshake
    let mut injector = injector_factory.create()?;
    let (my_w, my_h) = injector.screen_size()?;
    let my_screen = ScreenLayout {
        width: my_w,
        height: my_h,
    };
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

    if version != PROTOCOL_VERSION {
        let source = if fingerprint == tls_fingerprint && trust_store.is_trusted(&fingerprint)? {
            UpdateSource::TrustedPeer
        } else {
            UpdateSource::UntrustedPeer
        };
        let candidate = build_version.as_deref().unwrap_or("unknown");
        match attempt_client_update(candidate, source, release_repository, update_installer).await {
            Ok(UpdateExecution::RestartRequested(reason)) => {
                info!(
                    "Updated to {} after protocol mismatch. Restarting...",
                    candidate
                );
                connection.close(0u32.into(), b"updating");
                return Ok(SessionExit::RestartRequested(reason));
            }
            Ok(UpdateExecution::Ignored(_)) => {}
            Err(error) => warn!(
                "Self-update after protocol mismatch failed: {}. Rejecting incompatible peer.",
                error
            ),
        }
    }

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
        PairingDecision::PromptForOtp => Some(pairing_prompt.prompt(addr).await?),
    };

    let ack = Message::HelloAck {
        accepted: true,
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
    status_sink.write(runtime).ok();
    let mut server_screen = screen;

    match attempt_client_update(
        &server_build_version,
        UpdateSource::TrustedPeer,
        release_repository,
        update_installer,
    )
    .await
    {
        Ok(UpdateExecution::RestartRequested(reason)) => {
            info!("Updated to {}. Restarting...", server_build_version);
            connection.close(0u32.into(), b"updating");
            return Ok(SessionExit::RestartRequested(reason));
        }
        Ok(UpdateExecution::Ignored(_)) => {}
        Err(error) => warn!(
            "Self-update failed: {}. Continuing with current version.",
            error
        ),
    }

    let mut transition = ClientTransition::new(my_w, my_h);
    let peer: Arc<dyn ClientPeerLink> =
        Arc::new(QuinnClientPeerLink::open(&connection, control_send, control_recv).await?);
    info!("Typed control, input, and clipboard channels accepted");

    // Shutdown signal for background tasks
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);

    // Serialize local polling and peer writes through one clipboard task. The
    // OS calls run on blocking workers, while the session loop only enqueues
    // updates and remains responsive.
    let clipboard_sync = Arc::new(std::sync::Mutex::new(
        crate::clipboard::sync::ClipboardSync::new(clipboard_port.clone()),
    ));
    // Coalesce peer clipboard updates while the platform clipboard command is
    // running instead of replaying an unbounded backlog afterward.
    let (clipboard_update_tx, mut clipboard_updates) =
        tokio::sync::watch::channel(None::<protocol::ClipboardContent>);
    let clipboard_worker = clipboard_sync.clone();
    let clipboard_peer = peer.clone();
    let mut shutdown_rx1 = shutdown_tx.subscribe();
    let clipboard_task = tokio::spawn(async move {
        let interval = crate::clipboard::sync::ClipboardSync::poll_interval();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let msg = poll_clipboard_on_worker(clipboard_worker.clone()).await;
                    if let Ok(Some(Message::ClipboardUpdate {
                        content: protocol::ClipboardContent::Text(text),
                    })) = msg {
                        if clipboard_peer
                            .send_clipboard(ClientClipboardCommand::SetPeerText(text))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                result = clipboard_updates.changed() => {
                    if result.is_err() {
                        break;
                    }
                    let content = { clipboard_updates.borrow_and_update().clone() };
                    if let Some(content) = content {
                        if let Err(error) = apply_clipboard_on_worker(
                            clipboard_worker.clone(),
                            content,
                        ).await {
                            warn!("Failed to apply clipboard update: {}", error);
                        }
                    }
                }
                _ = shutdown_rx1.changed() => {
                    break;
                }
            }
        }
    });

    // Spawn file transfer acceptor (receives files from server via new bi-streams)
    let ft_conn = connection.clone();
    let receive_clipboard = clipboard_port.clone();
    let mut shutdown_rx3 = shutdown_tx.subscribe();
    let file_acceptor_task = tokio::spawn(async move {
        let mut transfer_tasks = crate::filetransfer::supervisor::TransferTaskSet::default();
        loop {
            tokio::select! {
                result = ft_conn.accept_bi() => {
                    match result {
                        Ok((send, recv)) => {
                            let receive_clipboard = receive_clipboard.clone();
                            if !transfer_tasks.try_spawn(async move {
                                match crate::filetransfer::recv::receive_files(send, recv).await {
                                    Ok(paths) if !paths.is_empty() => {
                                        info!("Received {} file(s) from server", paths.len());
                                        tokio::task::spawn_blocking(move || {
                                            receive_clipboard.write_files(&paths).ok();
                                        }).await.ok();
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        warn!("File transfer receive error: {}", e);
                                    }
                                }
                            }) {
                                warn!("Rejecting file transfer: concurrent transfer limit reached");
                            }
                        }
                        Err(_) => break,
                    }
                }
                _ = transfer_tasks.join_next(), if !transfer_tasks.is_empty() => {}
                _ = shutdown_rx3.changed() => {
                    break;
                }
            }
        }
        transfer_tasks.shutdown().await;
    });

    info!("Client ready. Waiting for server to share mouse...");

    // Drain typed input events independently so slow OS injection cannot stop
    // QUIC reads or replay stale pointer frames.
    let input_queue = InputMessageQueue::default();
    let input_reader_queue = input_queue.clone();
    let (peer_event_send, mut peer_events) = tokio::sync::mpsc::channel(64);
    let peer_reader = peer.clone();
    let peer_reader_task = tokio::spawn(async move {
        loop {
            match peer_reader.next_event().await {
                Some(ClientTransportEvent::Input(event)) => {
                    input_reader_queue.push(protocol_input_message(event))
                }
                Some(ClientTransportEvent::Closed(ClientChannel::Input)) => {
                    input_reader_queue.close();
                    break;
                }
                Some(ClientTransportEvent::Failed(failure))
                    if failure.channel == ClientChannel::Input =>
                {
                    input_reader_queue.fail(failure.message);
                    break;
                }
                Some(event) => {
                    if peer_event_send.send(event).await.is_err() {
                        break;
                    }
                }
                None => {
                    input_reader_queue.close();
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
    let mut latency_watchdog = ClientLatencyWatchdog::default();
    let mut restart_for_latency = false;
    let mut injected_keys: HashSet<u32> = HashSet::new();
    let mut injected_buttons: HashSet<u8> = HashSet::new();
    let mut activation_started: Option<Instant> = None;
    let mut activation_input_messages: u64 = 0;
    let mut activation_inject_moves: u64 = 0;
    let mut activation_superseded_moves: u64 = 0;
    let mut activation_first_inject_logged = false;
    let mut pending_mouse_move: Option<(i32, i32)> = None;
    let mut file_send_tasks = Vec::new();

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
                                if let Err(error) = inject_late_release(
                                    &mut *injector,
                                    &original_message,
                                    &mut injected_keys,
                                    &mut injected_buttons,
                                ) {
                                    warn!("Inject late input release error: {}", error);
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
                                restore_client_input_state(
                                    &mut *injector,
                                    display_control,
                                    &mut injected_keys,
                                    &mut injected_buttons,
                                );
                                info!("Edge on client: {:?} — requesting switch back", direction);
                                peer.send_control(ClientControlCommand::RequestSwitchBack {
                                    direction: peer_direction(direction),
                                })
                                .await
                                .ok();
                                // Check clipboard for files and transfer them
                                let ft_conn = connection.clone();
                                let send_clipboard = clipboard_port.clone();
                                file_send_tasks.push(tokio::spawn(async move {
                                    let files = tokio::task::spawn_blocking(move || {
                                        send_clipboard.read_files().ok()
                                    }).await.ok().flatten();
                                    if let Some(files) = files.filter(|files| !files.is_empty()) {
                                        info!("Transferring {} clipboard file(s) to server", files.len());
                                        if let Err(e) = crate::filetransfer::send::send_files(&ft_conn, files).await {
                                            warn!("File transfer error: {}", e);
                                        }
                                    }
                                }));
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
            event = peer_events.recv() => {
                match event {
                    Some(ClientTransportEvent::Control(ClientControlEvent::Heartbeat { timestamp })) => {
                        if let Err(error) = peer
                            .send_control(ClientControlCommand::AcknowledgeHeartbeat { timestamp })
                            .await
                        {
                            warn!("Failed to send heartbeat ack: {}", error);
                            break;
                        }
                    }
                    Some(ClientTransportEvent::Control(
                        ClientControlEvent::HeartbeatAcknowledged { timestamp },
                    )) => {
                        if let Some(rtt) = latency_watchdog.acknowledge(timestamp, Instant::now()) {
                            if rtt > CLIENT_LATENCY_RESTART_THRESHOLD {
                                warn!(
                                    "Client latency watchdog: RTT {:.0}ms (strike {}/{})",
                                    rtt.as_secs_f64() * 1000.0,
                                    latency_watchdog.strikes,
                                    CLIENT_LATENCY_RESTART_STRIKES
                                );
                            }
                        }
                    }
                    Some(ClientTransportEvent::Control(
                        ClientControlEvent::PeerScreenChanged(screen),
                    )) => {
                        if let Some(wire_screen) = validated_peer_screen(screen) {
                            info!("Server screen changed: {}x{}", screen.width, screen.height);
                            server_screen = wire_screen;
                        } else {
                            warn!("Ignoring invalid server screen resize: {}x{}", screen.width, screen.height);
                        }
                    }
                    Some(ClientTransportEvent::Control(ClientControlEvent::WakeDisplay)) => {
                        debug!("Peer user active — keeping this system awake");
                        display_control.wake_display().ok();
                    }
                    Some(ClientTransportEvent::Clipboard(ClientClipboardEvent::TextChanged(text))) => {
                        let content = protocol::ClipboardContent::Text(text);
                        if clipboard_update_tx.send(Some(content)).is_err() {
                            warn!("Clipboard worker stopped before peer update was applied");
                        }
                    }
                    Some(ClientTransportEvent::Closed(channel)) => {
                        info!("Server {:?} channel closed", channel);
                        if matches!(
                            client_channel_disposition(channel),
                            ClientChannelDisposition::Disconnect
                        ) {
                            break;
                        }
                    }
                    Some(ClientTransportEvent::Failed(failure)) => {
                        warn!("{:?} channel error: {}", failure.channel, failure.message);
                        if matches!(
                            client_channel_disposition(failure.channel),
                            ClientChannelDisposition::Disconnect
                        ) {
                            break;
                        }
                    }
                    Some(ClientTransportEvent::Input(event)) => {
                        // The reader task normally routes input to the coalescing queue.
                        input_queue.push(protocol_input_message(event));
                    }
                    None => {
                        info!("Client peer link closed");
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

                if let Some(elapsed) = latency_watchdog.expire_pending(Instant::now()) {
                    warn!(
                        "Client latency watchdog: heartbeat pending for {:.0}ms (strike {}/{})",
                        elapsed.as_secs_f64() * 1000.0,
                        latency_watchdog.strikes,
                        CLIENT_LATENCY_RESTART_STRIKES
                    );
                }

                if latency_watchdog.should_restart() {
                    warn!("Client latency watchdog: sustained lag detected; restarting client process");
                    restart_for_latency = true;
                    break;
                }

                if latency_watchdog.needs_ping() {
                    let timestamp = unix_millis();
                    if let Err(error) = peer
                        .send_control(ClientControlCommand::Heartbeat { timestamp })
                        .await
                    {
                        warn!("Client latency watchdog failed to send heartbeat: {}", error);
                        break;
                    }
                    latency_watchdog.record_ping(timestamp, Instant::now());
                }
            }
            _ = screen_check.tick() => {
                if let Ok((w, h)) = injector.screen_size() {
                    if w != last_screen_w || h != last_screen_h {
                        info!("Screen size changed: {}x{} -> {}x{}", last_screen_w, last_screen_h, w, h);
                        last_screen_w = w;
                        last_screen_h = h;
                        transition.update_screen_size(w, h);
                        if let Err(error) = peer
                            .send_control(ClientControlCommand::LocalScreenChanged(PeerScreen {
                                width: w,
                                height: h,
                            }))
                            .await
                        {
                            warn!("Failed to send screen resize: {}", error);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Release any synthetic input that may still be down if the stream ended
    // before key-up/button-up events were processed (for example during display sleep).
    restore_client_input_state(
        &mut *injector,
        display_control,
        &mut injected_keys,
        &mut injected_buttons,
    );

    // Signal clipboard tasks to shut down
    shutdown_tx.send(true).ok();

    // Stop and join transport readers, then close the connection to release
    // any remaining transport waits. Abort blocking adapter tasks afterward.
    peer.shutdown().await;
    connection.close(0u32.into(), b"disconnected");
    let mut session_tasks = vec![clipboard_task, file_acceptor_task, peer_reader_task];
    session_tasks.append(&mut file_send_tasks);
    terminate_client_tasks(session_tasks).await;

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
                Some(pairing::prompt_pairing_code(addr).await?)
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

/// Pair with a server once, verifying its advertised identity before storing trust.
pub async fn pair(addr: &str, expected_fingerprint: Option<&str>) -> Result<String> {
    let addr = resolve_addr(addr)?;
    let endpoint = make_client_endpoint()?;

    info!("Pairing with {}...", addr);
    let connection = connect_with_retry(&endpoint, addr).await?;

    let (mut send, mut recv) = connection.accept_bi().await?;
    let hello = recv_message(&mut recv).await?;

    let paired_fingerprint = match hello {
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
            let verified_fingerprint =
                validate_discovered_fingerprint(expected_fingerprint, &fingerprint)?;

            let otp = if tls::is_fingerprint_trusted(&fingerprint) {
                info!("Server fingerprint already trusted");
                None
            } else {
                Some(pairing::prompt_pairing_code(addr).await?)
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
            verified_fingerprint
        }
        other => {
            return Err(eyre!("Expected Hello, got: {:?}", other));
        }
    };

    connection.close(0u32.into(), b"paired");
    endpoint.wait_idle().await;

    Ok(paired_fingerprint)
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

async fn send_message_uni(send: &mut TypedServerInputGuard, msg: &Message) -> Result<()> {
    send_server_input_messages(send.peer.as_ref(), [msg.clone()]).await
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
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn peer_direction_strategy() -> impl Strategy<Value = PeerDirection> {
        prop_oneof![
            Just(PeerDirection::Left),
            Just(PeerDirection::Right),
            Just(PeerDirection::Up),
            Just(PeerDirection::Down),
        ]
    }

    fn client_session_event_strategy() -> impl Strategy<Value = ClientInputEvent> {
        prop_oneof![
            peer_direction_strategy()
                .prop_map(|direction| ClientInputEvent::SwitchToClient { direction }),
            (-2_000i32..=2_000, -2_000i32..=2_000)
                .prop_map(|(x, y)| ClientInputEvent::MouseMoved { x, y }),
            (0u8..=7, any::<bool>()).prop_map(|(button, pressed)| {
                ClientInputEvent::MouseButtonChanged { button, pressed }
            }),
            (0u32..=255, any::<bool>(), any::<u16>()).prop_map(|(keycode, pressed, modifiers)| {
                ClientInputEvent::KeyChanged {
                    keycode,
                    pressed,
                    modifiers,
                }
            }),
            (-100i16..=100, -100i16..=100).prop_map(|(dx, dy)| {
                ClientInputEvent::MouseScrolled {
                    dx: f64::from(dx),
                    dy: f64::from(dy),
                    phase: PeerScrollPhase::None,
                }
            }),
        ]
    }

    #[derive(Clone, Debug)]
    enum GeneratedServerSessionEvent {
        Poll {
            x: i32,
            y: i32,
            buttons: u8,
            keys: Vec<Message>,
        },
        Activate(PeerDirection),
        SwitchBack(PeerDirection),
        Resize {
            width: u32,
            height: u32,
        },
    }

    fn generated_key_strategy() -> impl Strategy<Value = Message> {
        (0u32..=255, any::<bool>(), any::<u16>()).prop_map(|(keycode, pressed, modifiers)| {
            Message::KeyEvent {
                keycode,
                pressed,
                modifiers,
            }
        })
    }

    fn server_session_event_strategy() -> impl Strategy<Value = GeneratedServerSessionEvent> {
        prop_oneof![
            (
                -100i32..=2_100,
                -100i32..=1_200,
                0u8..=7,
                prop::collection::vec(generated_key_strategy(), 0..5),
            )
                .prop_map(|(x, y, buttons, keys)| GeneratedServerSessionEvent::Poll {
                    x,
                    y,
                    buttons,
                    keys,
                }),
            peer_direction_strategy().prop_map(GeneratedServerSessionEvent::Activate),
            peer_direction_strategy().prop_map(GeneratedServerSessionEvent::SwitchBack),
            (1u32..=16_384, 1u32..=16_384).prop_map(|(width, height)| {
                GeneratedServerSessionEvent::Resize { width, height }
            }),
        ]
    }

    async fn run_generated_client_session(
        rig: &crate::testing::ClientRig,
        events: Vec<ClientInputEvent>,
    ) {
        for _ in 0..events.len() {
            rig.peer.succeed_next_control_send();
        }
        for event in events {
            rig.peer.push_event(ClientTransportEvent::Input(event));
        }
        rig.peer.push_channel_close(ClientChannel::Input);

        let mut injector = rig.injector_factory.create().unwrap();
        let mut transition = ClientTransition::new(1920, 1080);
        let mut injected_keys = HashSet::new();
        let mut injected_buttons = HashSet::new();

        while let Some(event) = rig.peer.next_event().await {
            match event {
                ClientTransportEvent::Input(event) => {
                    let original = protocol_input_message(event);
                    match transition.handle(original.clone()) {
                        ClientOutput::Ignore => {
                            inject_late_release(
                                &mut *injector,
                                &original,
                                &mut injected_keys,
                                &mut injected_buttons,
                            )
                            .unwrap();
                        }
                        ClientOutput::Activate => {
                            rig.display.wake_display().unwrap();
                        }
                        ClientOutput::InjectMove { x, y } => {
                            injector.inject(&Message::MouseMove { x, y }).unwrap();
                        }
                        ClientOutput::Forward(message) => {
                            injector.inject(&message).unwrap();
                            track_injected_input(
                                &message,
                                &mut injected_keys,
                                &mut injected_buttons,
                            );
                        }
                        ClientOutput::SwitchBack { direction, inject } => {
                            if let Some((x, y)) = inject {
                                injector.inject(&Message::MouseMove { x, y }).unwrap();
                            }
                            restore_client_input_state(
                                &mut *injector,
                                &rig.display,
                                &mut injected_keys,
                                &mut injected_buttons,
                            );
                            rig.peer
                                .send_control(ClientControlCommand::RequestSwitchBack {
                                    direction: peer_direction(direction),
                                })
                                .await
                                .unwrap();
                        }
                    }
                }
                ClientTransportEvent::Closed(ClientChannel::Input) => break,
                other => panic!("unexpected generated client event: {other:?}"),
            }
        }

        restore_client_input_state(
            &mut *injector,
            &rig.display,
            &mut injected_keys,
            &mut injected_buttons,
        );
        rig.peer.shutdown().await;
    }

    async fn send_generated_server_messages(
        rig: &crate::testing::ServerRig,
        messages: Vec<Message>,
    ) {
        for _ in &messages {
            rig.peer
                .succeed_next_send(crate::testing::ServerSendOperation::Input);
        }
        send_server_input_messages(&rig.peer, messages)
            .await
            .unwrap();
    }

    async fn run_generated_server_session(
        rig: &crate::testing::ServerRig,
        events: Vec<GeneratedServerSessionEvent>,
    ) {
        let mut capture = rig.capture.clone();
        let mut transition = ServerTransition::new(
            None,
            ScreenLayout {
                width: 1920,
                height: 1080,
            },
        );

        for event in events {
            match event {
                GeneratedServerSessionEvent::Poll {
                    x,
                    y,
                    buttons,
                    keys,
                } => match transition.poll(x, y, 1920, 1080, buttons, keys) {
                    ServerOutput::Idle => {}
                    ServerOutput::Activate { messages, grab } => {
                        capture.set_grab(grab).unwrap();
                        send_generated_server_messages(rig, messages).await;
                    }
                    ServerOutput::Forward { messages } => {
                        send_generated_server_messages(rig, messages).await;
                    }
                    ServerOutput::ShortcutRelease { messages }
                    | ServerOutput::ForceRelease { messages } => {
                        capture.set_grab(false).unwrap();
                        send_generated_server_messages(rig, messages).await;
                    }
                },
                GeneratedServerSessionEvent::Activate(direction) => {
                    if !transition.is_active() {
                        capture.set_grab(true).unwrap();
                        let messages = transition.activate_instant(protocol_direction(direction));
                        send_generated_server_messages(rig, messages).await;
                    }
                }
                GeneratedServerSessionEvent::SwitchBack(direction) => {
                    rig.peer.push_event(ServerTransportEvent::Control(
                        ServerControlEvent::SwitchBackRequested { direction },
                    ));
                    let event = rig.peer.next_event().await.unwrap();
                    assert!(matches!(
                        event,
                        ServerTransportEvent::Control(
                            ServerControlEvent::SwitchBackRequested { .. }
                        )
                    ));
                    let messages = transition.on_switch_back();
                    capture.set_grab(false).unwrap();
                    send_generated_server_messages(rig, messages).await;
                }
                GeneratedServerSessionEvent::Resize { width, height } => {
                    rig.peer.push_event(ServerTransportEvent::Control(
                        ServerControlEvent::PeerScreenChanged(PeerScreen { width, height }),
                    ));
                    let event = rig.peer.next_event().await.unwrap();
                    let ServerTransportEvent::Control(ServerControlEvent::PeerScreenChanged(
                        screen,
                    )) = event
                    else {
                        panic!("unexpected generated server event: {event:?}");
                    };
                    transition.update_peer_screen(ScreenLayout {
                        width: screen.width,
                        height: screen.height,
                    });
                }
            }
        }

        let releases = restore_server_input_state(&mut capture, false, &mut transition);
        send_generated_server_messages(rig, releases).await;
        rig.peer.shutdown().await;
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn generated_client_sessions_restore_inputs_cursor_and_tasks(
            events in prop::collection::vec(client_session_event_strategy(), 0..96),
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let rig = crate::testing::ClientRig::new();
                run_generated_client_session(&rig, events).await;
                rig.assert_pressed_inputs(&[], &[]);
                rig.assert_cursor_visible(true);
                assert_eq!(rig.peer.pending_events(), 0);
                rig.assert_tasks_completed();
            });
        }

        #[test]
        fn generated_server_sessions_release_grabs_inputs_and_tasks(
            events in prop::collection::vec(server_session_event_strategy(), 0..96),
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let rig = crate::testing::ServerRig::new();
                run_generated_server_session(&rig, events).await;

                let mut remote_keys = HashSet::new();
                let mut remote_buttons = HashSet::new();
                for observation in rig.peer.observations().snapshot() {
                    if let crate::testing::ServerPeerObservation::InputSend(command) = observation.event {
                        match command {
                            ServerInputCommand::KeyChanged { keycode, pressed, .. } => {
                                if pressed {
                                    remote_keys.insert(keycode);
                                } else {
                                    remote_keys.remove(&keycode);
                                }
                            }
                            ServerInputCommand::MouseButtonChanged { button, pressed } => {
                                if pressed {
                                    remote_buttons.insert(button);
                                } else {
                                    remote_buttons.remove(&button);
                                }
                            }
                            _ => {}
                        }
                    }
                }

                assert!(remote_keys.is_empty(), "server exit left remote keys held");
                assert!(remote_buttons.is_empty(), "server exit left remote buttons held");
                assert!(matches!(
                    rig.capture.grab_history().last(),
                    Some(crate::testing::GrabChange::All(false))
                ));
                assert_eq!(rig.peer.pending_events(), 0);
                assert!(rig.peer.is_shutdown());
                rig.assert_tasks_completed();
            });
        }
    }

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

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_runs_under_virtual_time() {
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move { wait_for_retry(&cancellation, Duration::from_secs(60)).await }
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(task.await.unwrap());
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
    fn disconnect_while_key_held_releases_the_client_key() {
        let rig = crate::testing::ClientRig::new();
        let mut injector = rig.injector_factory.create().unwrap();
        injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        let mut keys = HashSet::from([30]);
        let mut buttons = HashSet::new();

        release_injected_inputs(&mut *injector, &rig.display, &mut keys, &mut buttons);

        rig.assert_pressed_inputs(&[], &[]);
        assert!(keys.is_empty());
    }

    #[test]
    fn disconnect_while_button_held_releases_the_client_button() {
        let rig = crate::testing::ClientRig::new();
        let mut injector = rig.injector_factory.create().unwrap();
        injector
            .inject(&Message::MouseButton {
                button: 0,
                pressed: true,
            })
            .unwrap();
        let mut keys = HashSet::new();
        let mut buttons = HashSet::from([0]);

        release_injected_inputs(&mut *injector, &rig.display, &mut keys, &mut buttons);

        rig.assert_pressed_inputs(&[], &[]);
        assert!(buttons.is_empty());
    }

    #[test]
    fn switch_back_then_late_key_up_does_not_inject_a_duplicate_release() {
        let rig = crate::testing::ClientRig::new();
        let mut injector = rig.injector_factory.create().unwrap();
        injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        let mut keys = HashSet::from([30]);
        let mut buttons = HashSet::new();

        // Switch-back performs eager cleanup before the server's key-up can arrive.
        release_injected_inputs(&mut *injector, &rig.display, &mut keys, &mut buttons);
        let injected = inject_late_release(
            &mut *injector,
            &Message::KeyEvent {
                keycode: 30,
                pressed: false,
                modifiers: 0,
            },
            &mut keys,
            &mut buttons,
        )
        .unwrap();

        assert!(!injected);
        let releases = rig
            .injector
            .injected()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    crate::testing::RecordedInput::KeyEvent {
                        keycode: 30,
                        pressed: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(releases, 1);
    }

    #[test]
    fn duplicate_key_release_is_injected_only_once() {
        let rig = crate::testing::ClientRig::new();
        let mut injector = rig.injector_factory.create().unwrap();
        injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        let mut keys = HashSet::from([30]);
        let mut buttons = HashSet::new();
        let release = Message::KeyEvent {
            keycode: 30,
            pressed: false,
            modifiers: 0,
        };

        assert!(inject_late_release(&mut *injector, &release, &mut keys, &mut buttons,).unwrap());
        assert!(!inject_late_release(&mut *injector, &release, &mut keys, &mut buttons,).unwrap());
        rig.assert_pressed_inputs(&[], &[]);
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

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_clipboard_work_does_not_stall_async_session_progress() {
        let clipboard = Arc::new(crate::testing::MemoryClipboard::new());
        clipboard.set_text(Some("ready".to_string()));
        let gate = clipboard.block_next(crate::testing::ClipboardOperation::ReadText);
        let sync = Arc::new(std::sync::Mutex::new(
            crate::clipboard::sync::ClipboardSync::new(clipboard),
        ));

        let poll = tokio::spawn(poll_clipboard_on_worker(sync));
        tokio::task::yield_now().await;
        assert!(gate.wait_until_entered(Duration::from_secs(1)));

        // This marker represents unrelated session-loop work. On a
        // current-thread runtime it can only run while the clipboard call is
        // blocked if that call was moved to a blocking worker.
        let marker = tokio::spawn(async { "session progressed" });
        assert_eq!(marker.await.unwrap(), "session progressed");

        gate.release();
        assert!(matches!(
            poll.await.unwrap().unwrap(),
            Some(Message::ClipboardUpdate { .. })
        ));
    }

    #[test]
    fn discovered_fingerprint_must_match_the_connected_certificate() {
        assert_eq!(
            validate_discovered_fingerprint(Some("aa:bb"), "AA:BB").unwrap(),
            "AA:BB"
        );
        let error = validate_discovered_fingerprint(Some("AA:BB"), "CC:DD").unwrap_err();
        assert!(error.to_string().contains("expected AA:BB, received CC:DD"));
    }

    #[test]
    fn closed_layer_shell_capture_ends_the_server_session() {
        let error = require_layer_shell_event(None).unwrap_err();

        assert_eq!(error.to_string(), "layer-shell capture stopped");
    }

    #[tokio::test]
    async fn server_task_shutdown_aborts_and_joins_owned_tasks() {
        let tracker = crate::testing::TaskTracker::new();
        let tracked = tracker.clone();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            tracked
                .run("server background", std::future::pending::<()>())
                .await;
        });
        tokio::task::yield_now().await;
        assert!(!tracker.is_idle());

        terminate_server_tasks(&mut tasks).await;

        tracker.ensure_idle().unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn server_connection_shutdown_terminates_every_owned_task() {
        let rig = crate::testing::ServerRig::new();
        let mut tasks = tokio::task::JoinSet::new();
        for name in ["clipboard poll", "file acceptor", "file send"] {
            let tracker = rig.tasks.clone();
            tasks.spawn(async move {
                tracker.run(name, std::future::pending::<()>()).await;
            });
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if rig.tasks.running_tasks().len() == 3 {
                break;
            }
        }
        assert_eq!(rig.tasks.running_tasks().len(), 3);

        rig.shutdown();
        shutdown_server_connection_tasks(&mut tasks, &rig.peer).await;

        assert!(rig.is_shutdown());
        assert!(rig.peer.is_shutdown());
        assert!(tasks.is_empty());
        rig.assert_tasks_completed();
    }

    #[tokio::test]
    async fn every_client_exit_restores_input_cursor_and_tasks() {
        for restart_for_latency in [false, true] {
            let rig = crate::testing::ClientRig::new();
            let mut injector = rig.injector_factory.create().unwrap();
            injector.set_cursor_visible(false).unwrap();
            injector
                .inject(&Message::KeyEvent {
                    keycode: 30,
                    pressed: true,
                    modifiers: 0,
                })
                .unwrap();
            injector
                .inject(&Message::MouseButton {
                    button: 0,
                    pressed: true,
                })
                .unwrap();
            let mut keys = HashSet::from([30]);
            let mut buttons = HashSet::from([0]);

            let tasks = rig.tasks.clone();
            let background = tokio::spawn(async move {
                tasks
                    .run("client background", std::future::pending::<()>())
                    .await;
            });
            tokio::task::yield_now().await;

            restore_client_input_state(&mut *injector, &rig.display, &mut keys, &mut buttons);
            terminate_client_tasks(vec![background]).await;

            rig.assert_pressed_inputs(&[], &[]);
            rig.assert_cursor_visible(true);
            rig.assert_tasks_completed();
            assert!(keys.is_empty());
            assert!(buttons.is_empty());
            match (
                restart_for_latency,
                client_shutdown_exit(restart_for_latency),
            ) {
                (false, SessionExit::Disconnected)
                | (true, SessionExit::RestartRequested(RestartReason::LatencyWatchdog)) => {}
                (_, exit) => panic!("unexpected client exit: {exit:?}"),
            }
        }
    }

    struct FixedLockSource(std::result::Result<bool, &'static str>);

    impl crate::ports::LocalSessionLockSource for FixedLockSource {
        fn is_locked(&self) -> Result<bool> {
            self.0.map_err(|message| eyre!(message))
        }
    }

    #[tokio::test]
    async fn server_edge_activation_scenario() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        for index in 0..50 {
            capture.push_position(1919, 540);
            if index > 0 {
                capture.push_screen_size(1920, 1080);
            }
            capture.push_buttons(0);
            capture.push_key_events(Vec::new());
        }
        let mut transition = ServerTransition::new(
            Some(protocol::Direction::Right),
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );
        let mut activation = None;

        for _ in 0..50 {
            let keys = capture.poll_key_events().unwrap();
            let (x, y) = capture.mouse_position().unwrap();
            let (width, height) = capture.screen_size().unwrap();
            let buttons = capture.mouse_buttons().unwrap();
            if let ServerOutput::Activate { messages, .. } =
                transition.poll(x, y, width, height, buttons, keys)
            {
                activation = Some(messages);
                break;
            }
        }
        let messages = activation.expect("edge dwell should activate sharing");
        capture.set_grab(true).unwrap();
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);
        send_server_input_messages(&rig.peer, messages)
            .await
            .unwrap();

        rig.assert_grab_history(&[crate::testing::GrabChange::All(true)]);
        rig.assert_outbound_peer_messages(&[
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::SwitchToPeer {
                direction: PeerDirection::Right,
            }),
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::MouseMoved {
                x: 20,
                y: 540,
            }),
        ]);
    }

    #[tokio::test]
    async fn server_shortcut_activation_scenario() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        capture.push_position(400, 300);
        capture.push_buttons(0);
        capture.push_key_events(
            [29, 56, 42, 106]
                .into_iter()
                .map(|keycode| Message::KeyEvent {
                    keycode,
                    pressed: true,
                    modifiers: 0,
                })
                .collect(),
        );
        let keys = capture.poll_key_events().unwrap();
        let (x, y) = capture.mouse_position().unwrap();
        let (width, height) = capture.screen_size().unwrap();
        let buttons = capture.mouse_buttons().unwrap();
        let mut transition = ServerTransition::new(
            Some(protocol::Direction::Right),
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );
        let ServerOutput::Activate { messages, .. } =
            transition.poll(x, y, width, height, buttons, keys)
        else {
            panic!("shortcut should activate sharing");
        };
        capture.set_grab(true).unwrap();
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);

        send_server_input_messages(&rig.peer, messages)
            .await
            .unwrap();

        assert!(transition.is_active());
        rig.assert_grab_history(&[crate::testing::GrabChange::All(true)]);
        rig.assert_outbound_peer_messages(&[
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::SwitchToPeer {
                direction: PeerDirection::Right,
            }),
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::MouseMoved {
                x: 20,
                y: 720,
            }),
        ]);
    }

    #[tokio::test]
    async fn server_safety_escape_releases_grab_and_remote_keys() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        let mut transition = ServerTransition::new(
            None,
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );
        transition.activate_instant(protocol::Direction::Right);
        capture.set_grab(true).unwrap();
        let escape_keys = [29, 56, 1]
            .into_iter()
            .map(|keycode| Message::KeyEvent {
                keycode,
                pressed: true,
                modifiers: 0,
            })
            .collect();
        let ServerOutput::ForceRelease { messages } =
            transition.poll(100, 100, 1920, 1080, 0, escape_keys)
        else {
            panic!("safety escape should force release");
        };
        capture.set_grab(false).unwrap();
        for _ in &messages {
            rig.peer
                .succeed_next_send(crate::testing::ServerSendOperation::Input);
        }

        send_server_input_messages(&rig.peer, messages)
            .await
            .unwrap();

        assert!(!transition.is_active());
        rig.assert_grab_history(&[
            crate::testing::GrabChange::All(true),
            crate::testing::GrabChange::All(false),
        ]);
        rig.assert_outbound_peer_messages(&[
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::KeyChanged {
                keycode: 1,
                pressed: false,
                modifiers: 0,
            }),
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::KeyChanged {
                keycode: 29,
                pressed: false,
                modifiers: 0,
            }),
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::KeyChanged {
                keycode: 56,
                pressed: false,
                modifiers: 0,
            }),
        ]);
    }

    #[tokio::test]
    async fn server_switch_back_releases_held_keys_and_local_grab() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        let mut transition = ServerTransition::new(
            None,
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );
        transition.activate_instant(protocol::Direction::Right);
        capture.set_grab(true).unwrap();
        let key_down = Message::KeyEvent {
            keycode: 30,
            pressed: true,
            modifiers: 0,
        };
        let ServerOutput::Forward { messages } =
            transition.poll(0, 0, 1920, 1080, 0, vec![key_down])
        else {
            panic!("active input should be forwarded");
        };
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);
        send_server_input_messages(&rig.peer, messages)
            .await
            .unwrap();

        let releases = transition.on_switch_back();
        capture.set_grab(false).unwrap();
        rig.peer
            .succeed_next_send(crate::testing::ServerSendOperation::Input);
        send_server_input_messages(&rig.peer, releases)
            .await
            .unwrap();

        assert!(!transition.is_active());
        rig.assert_grab_history(&[
            crate::testing::GrabChange::All(true),
            crate::testing::GrabChange::All(false),
        ]);
        rig.assert_outbound_peer_messages(&[
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::KeyChanged {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            }),
            crate::testing::ServerPeerObservation::InputSend(ServerInputCommand::KeyChanged {
                keycode: 30,
                pressed: false,
                modifiers: 0,
            }),
        ]);
    }

    #[tokio::test(start_paused = true)]
    async fn local_lock_releases_sharing_in_polling_and_layer_shell_modes() {
        for layer_shell in [false, true] {
            let rig = crate::testing::ServerRig::new();
            let mut capture = rig.capture.clone();
            let mut transition = ServerTransition::new(
                None,
                ScreenLayout {
                    width: 2560,
                    height: 1440,
                },
            );
            transition.activate_instant(protocol::Direction::Right);
            transition.update_key(30, true);
            if layer_shell {
                capture.set_keyboard_grab(true).unwrap();
            } else {
                capture.set_grab(true).unwrap();
            }
            rig.lock.push_state(true);
            rig.advance_time(LOCAL_LOCK_CHECK_INTERVAL).await;
            assert!(server_session_is_locked(&rig.lock));

            let messages = transition.deactivate_for_shortcut();
            rig.peer
                .succeed_next_send(crate::testing::ServerSendOperation::Input);
            notify_after_local_input_release(
                || {
                    if layer_shell {
                        capture.set_keyboard_grab(false).unwrap();
                    } else {
                        capture.set_grab(false).unwrap();
                    }
                },
                send_server_input_messages(&rig.peer, messages),
            )
            .await
            .unwrap();

            assert!(!transition.is_active());
            let expected = if layer_shell {
                vec![
                    crate::testing::GrabChange::Keyboard(true),
                    crate::testing::GrabChange::Keyboard(false),
                ]
            } else {
                vec![
                    crate::testing::GrabChange::All(true),
                    crate::testing::GrabChange::All(false),
                ]
            };
            rig.assert_grab_history(&expected);
            rig.assert_outbound_peer_messages(&[crate::testing::ServerPeerObservation::InputSend(
                ServerInputCommand::KeyChanged {
                    keycode: 30,
                    pressed: false,
                    modifiers: 0,
                },
            )]);
        }
    }

    #[tokio::test]
    async fn blocked_server_send_does_not_delay_local_release() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        capture.set_grab(true).unwrap();
        let gate = rig
            .peer
            .block_next_send(crate::testing::ServerSendOperation::Input);
        let peer = rig.peer.clone();
        let mut release_capture = rig.capture.clone();
        let task = tokio::spawn(async move {
            notify_after_local_input_release(
                move || release_capture.set_grab(false).unwrap(),
                send_server_input_messages(
                    &peer,
                    [Message::KeyEvent {
                        keycode: 30,
                        pressed: false,
                        modifiers: 0,
                    }],
                ),
            )
            .await
        });
        gate.wait_until_entered().await;

        rig.assert_grab_history(&[
            crate::testing::GrabChange::All(true),
            crate::testing::GrabChange::All(false),
        ]);
        assert!(!task.is_finished());

        gate.release();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn input_capture_failure_is_observable_and_triggers_cleanup() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        capture.set_grab(true).unwrap();
        capture.push_key_events(Vec::new());
        capture.fail_next(
            crate::testing::CaptureOperation::MousePosition,
            "capture device disappeared",
        );

        let error = poll_server_capture(&mut capture).unwrap_err();
        capture.set_grab(false).unwrap();

        assert!(error.to_string().contains("capture device disappeared"));
        rig.assert_grab_history(&[
            crate::testing::GrabChange::All(true),
            crate::testing::GrabChange::All(false),
        ]);
    }

    #[tokio::test]
    async fn peer_resize_updates_server_transition_geometry() {
        let rig = crate::testing::ServerRig::new();
        rig.peer.push_event(ServerTransportEvent::Control(
            ServerControlEvent::PeerScreenChanged(PeerScreen {
                width: 1600,
                height: 900,
            }),
        ));
        let mut transition = ServerTransition::new(
            None,
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );

        let event = rig.peer.next_event().await.unwrap();
        let ServerTransportEvent::Control(ServerControlEvent::PeerScreenChanged(screen)) = event
        else {
            panic!("expected peer resize");
        };
        transition.update_peer_screen(ScreenLayout {
            width: screen.width,
            height: screen.height,
        });

        assert!(matches!(
            transition
                .activate_instant(protocol::Direction::Down)
                .as_slice(),
            [
                Message::SwitchScreen {
                    direction: protocol::Direction::Down,
                },
                Message::MouseMove { x: 800, y: 20 },
            ]
        ));
    }

    #[tokio::test]
    async fn server_disconnect_releases_active_local_grab() {
        let rig = crate::testing::ServerRig::new();
        let mut capture = rig.capture.clone();
        let mut transition = ServerTransition::new(
            None,
            ScreenLayout {
                width: 2560,
                height: 1440,
            },
        );
        transition.activate_instant(protocol::Direction::Right);
        capture.set_grab(true).unwrap();
        rig.peer
            .push_channel_close(crate::ports::ServerChannel::Control);

        let event = rig.peer.next_event().await.unwrap();
        assert_eq!(
            server_channel_disposition(event.channel()),
            ServerChannelDisposition::Disconnect
        );
        transition.deactivate();
        capture.set_grab(false).unwrap();

        assert!(!transition.is_active());
        rig.assert_grab_history(&[
            crate::testing::GrabChange::All(true),
            crate::testing::GrabChange::All(false),
        ]);
    }

    #[tokio::test]
    async fn every_server_exit_releases_grabs_and_remote_inputs() {
        for exit in ["disconnect", "capture failure", "send failure", "shutdown"] {
            for keyboard_only in [false, true] {
                let rig = crate::testing::ServerRig::new();
                let mut capture = rig.capture.clone();
                let mut transition = ServerTransition::new(
                    None,
                    ScreenLayout {
                        width: 2560,
                        height: 1440,
                    },
                );
                transition.activate_instant(protocol::Direction::Right);
                if keyboard_only {
                    capture.set_keyboard_grab(true).unwrap();
                } else {
                    capture.set_grab(true).unwrap();
                }
                let output = transition.poll(
                    0,
                    0,
                    1920,
                    1080,
                    1,
                    vec![Message::KeyEvent {
                        keycode: 30,
                        pressed: true,
                        modifiers: 0,
                    }],
                );
                assert!(matches!(output, ServerOutput::Forward { .. }), "{exit}");

                let releases =
                    restore_server_input_state(&mut capture, keyboard_only, &mut transition);
                for _ in &releases {
                    rig.peer
                        .succeed_next_send(crate::testing::ServerSendOperation::Input);
                }
                send_server_input_messages(&rig.peer, releases)
                    .await
                    .unwrap();

                let expected_grabs = if keyboard_only {
                    vec![
                        crate::testing::GrabChange::Keyboard(true),
                        crate::testing::GrabChange::Keyboard(false),
                    ]
                } else {
                    vec![
                        crate::testing::GrabChange::All(true),
                        crate::testing::GrabChange::All(false),
                    ]
                };
                rig.assert_grab_history(&expected_grabs);
                rig.assert_outbound_peer_messages(&[
                    crate::testing::ServerPeerObservation::InputSend(
                        ServerInputCommand::KeyChanged {
                            keycode: 30,
                            pressed: false,
                            modifiers: 0,
                        },
                    ),
                    crate::testing::ServerPeerObservation::InputSend(
                        ServerInputCommand::MouseButtonChanged {
                            button: 0,
                            pressed: false,
                        },
                    ),
                ]);
                assert!(!transition.is_active(), "{exit}");
            }
        }
    }

    #[test]
    fn server_wire_input_is_routed_through_typed_commands() {
        assert_eq!(
            server_input_command(Message::MouseButton {
                button: 2,
                pressed: true,
            })
            .unwrap(),
            ServerInputCommand::MouseButtonChanged {
                button: 2,
                pressed: true,
            }
        );
        assert_eq!(
            server_input_command(Message::SwitchScreen {
                direction: protocol::Direction::Right,
            })
            .unwrap(),
            ServerInputCommand::SwitchToPeer {
                direction: PeerDirection::Right,
            }
        );
        assert!(server_input_command(Message::WakeDisplay).is_err());
    }

    #[test]
    fn server_lock_checks_use_injected_source_and_fail_open() {
        assert!(server_session_is_locked(&FixedLockSource(Ok(true))));
        assert!(!server_session_is_locked(&FixedLockSource(Ok(false))));
        assert!(!server_session_is_locked(&FixedLockSource(Err(
            "query unavailable"
        ))));
    }

    #[tokio::test]
    async fn local_input_release_precedes_blocking_network_notification() {
        let capturer = crate::testing::ScriptedCapturer::new();
        let mut active_capture = capturer.clone();
        active_capture.set_grab(true).unwrap();
        let release_capture = capturer.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (unblock_tx, unblock_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(notify_after_local_input_release(
            move || {
                let mut capture = release_capture;
                capture.set_grab(false).unwrap();
            },
            async move {
                entered_tx.send(()).unwrap();
                let _ = unblock_rx.await;
            },
        ));
        entered_rx.await.unwrap();

        assert_eq!(
            capturer.grab_history(),
            vec![
                crate::testing::GrabChange::All(true),
                crate::testing::GrabChange::All(false),
            ]
        );
        assert!(!task.is_finished());

        unblock_tx.send(()).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn server_display_wake_uses_injected_port() {
        let display = crate::testing::FakeDisplaySessionControl::new();
        let gate = display.block_next(crate::testing::DisplayOperation::WakeDisplay);

        request_server_display_wake(Arc::new(display.clone()));

        assert!(gate.wait_until_entered(Duration::from_secs(1)));
        assert!(display
            .observations()
            .snapshot()
            .iter()
            .any(|entry| { entry.event == crate::testing::DisplayObservation::WakeRequested }));
        gate.release();
    }

    #[test]
    fn server_capture_creation_uses_injected_factory() {
        let capturer = crate::testing::ScriptedCapturer::new();
        capturer.push_screen_size(3440, 1440);
        let factory = crate::testing::ScriptedCaptureFactory::new(capturer.clone());

        let (_, screen_size) = create_server_capturer(&factory).unwrap();

        assert_eq!(screen_size, (3440, 1440));
        assert_eq!(
            capturer
                .observations()
                .snapshot()
                .into_iter()
                .map(|entry| entry.event)
                .collect::<Vec<_>>(),
            vec![
                crate::testing::CaptureObservation::Created,
                crate::testing::CaptureObservation::ScreenSize(3440, 1440),
            ]
        );
    }

    #[test]
    fn runtime_mutex_helper_locks_normal_state() {
        let mutex = std::sync::Mutex::new(1u32);
        *lock_recover(&mutex, "test") = 2;
        assert_eq!(*lock_recover(&mutex, "test"), 2);
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

    #[cfg(test)]
    fn should_retry_resolution(explicit_addr: Option<&str>) -> bool {
        explicit_addr.is_some()
    }

    #[test]
    fn explicit_connect_addresses_are_resolved_each_loop() {
        assert!(should_retry_resolution(Some("example.local")));
        assert!(!should_retry_resolution(None));
    }

    #[tokio::test]
    async fn client_update_uses_injected_repository_and_installer() {
        use crate::testing::{AssetStreamStep, FakeUpdateInstaller, ScriptedReleaseRepository};

        let repository = ScriptedReleaseRepository::new();
        let installer = FakeUpdateInstaller::new();
        repository.push_asset("v999.0.0", Some(3), [AssetStreamStep::bytes(b"bin")]);
        installer.succeed_next();

        assert_eq!(
            attempt_client_update(
                "v999.0.0",
                UpdateSource::TrustedPeer,
                &repository,
                &installer,
            )
            .await
            .unwrap(),
            UpdateExecution::RestartRequested(RestartReason::UpdateInstalled {
                version: "v999.0.0".to_string(),
            })
        );
        assert_eq!(installer.installed_updates().len(), 1);

        assert_eq!(
            attempt_client_update("v0.0.0", UpdateSource::TrustedPeer, &repository, &installer,)
                .await
                .unwrap(),
            UpdateExecution::Ignored(crate::app::UpdateRejection::NotNewer)
        );
    }

    #[tokio::test]
    async fn local_screen_resize_is_sent_to_the_peer() {
        let rig = crate::testing::ClientRig::new();
        rig.injector.set_screen_size((2560, 1440));
        let injector = rig.injector_factory.create().unwrap();
        let (width, height) = injector.screen_size().unwrap();
        rig.peer.succeed_next_control_send();

        rig.peer
            .send_control(ClientControlCommand::LocalScreenChanged(PeerScreen {
                width,
                height,
            }))
            .await
            .unwrap();

        rig.assert_outbound_peer_messages(&[crate::testing::PeerLinkObservation::ControlSend(
            ClientControlCommand::LocalScreenChanged(PeerScreen {
                width: 2560,
                height: 1440,
            }),
        )]);
    }

    #[test]
    fn invalid_peer_screen_resize_is_rejected() {
        assert!(validated_peer_screen(PeerScreen {
            width: 0,
            height: 1080,
        })
        .is_none());
        assert!(validated_peer_screen(PeerScreen {
            width: 1920,
            height: 0,
        })
        .is_none());
        assert_eq!(
            validated_peer_screen(PeerScreen {
                width: 1920,
                height: 1080,
            })
            .unwrap()
            .width,
            1920
        );
    }

    #[test]
    fn injector_failure_does_not_record_unapplied_input() {
        let rig = crate::testing::ClientRig::new();
        rig.injector.fail_next(
            crate::testing::InjectorOperation::Inject,
            "display disappeared",
        );
        let mut injector = rig.injector_factory.create().unwrap();

        let error = injector
            .inject(&Message::KeyEvent {
                keycode: 30,
                pressed: true,
                modifiers: 0,
            })
            .unwrap_err();

        assert!(error.to_string().contains("display disappeared"));
        rig.assert_pressed_inputs(&[], &[]);
    }

    #[tokio::test]
    async fn control_stream_failure_is_observable_during_resize() {
        let rig = crate::testing::ClientRig::new();
        rig.peer
            .fail_next_control_send("control stream unavailable");

        let error = rig
            .peer
            .send_control(ClientControlCommand::LocalScreenChanged(PeerScreen {
                width: 2560,
                height: 1440,
            }))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("control stream unavailable"));
        assert!(rig.peer.observations().snapshot().iter().any(|entry| {
            matches!(
                &entry.event,
                crate::testing::PeerLinkObservation::SendFailed {
                    operation: crate::testing::PeerSendOperation::Control,
                    ..
                }
            )
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_latency_below_threshold_is_healthy() {
        let rig = crate::testing::ClientRig::new();
        let mut watchdog = ClientLatencyWatchdog::default();
        watchdog.record_ping(1, Instant::now());

        rig.advance_time(Duration::from_secs(2)).await;
        let rtt = watchdog.acknowledge(1, Instant::now()).unwrap();

        assert_eq!(rtt, Duration::from_secs(2));
        assert_eq!(watchdog.strikes, 0);
        assert!(!watchdog.should_restart());
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_heartbeat_acknowledgement_adds_a_strike() {
        let rig = crate::testing::ClientRig::new();
        let mut watchdog = ClientLatencyWatchdog::default();
        watchdog.record_ping(1, Instant::now());

        rig.advance_time(Duration::from_secs(4)).await;
        watchdog.acknowledge(1, Instant::now()).unwrap();

        assert_eq!(watchdog.strikes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_acknowledgement_recovers_after_latency() {
        let rig = crate::testing::ClientRig::new();
        let mut watchdog = ClientLatencyWatchdog::default();
        watchdog.record_ping(1, Instant::now());
        rig.advance_time(Duration::from_secs(4)).await;
        watchdog.acknowledge(1, Instant::now()).unwrap();
        assert_eq!(watchdog.strikes, 1);

        watchdog.record_ping(2, Instant::now());
        rig.advance_time(Duration::from_secs(1)).await;
        watchdog.acknowledge(2, Instant::now()).unwrap();

        assert_eq!(watchdog.strikes, 0);
        assert!(!watchdog.should_restart());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_heartbeat_timeouts_trip_watchdog_restart() {
        let rig = crate::testing::ClientRig::new();
        let mut watchdog = ClientLatencyWatchdog::default();

        for timestamp in 0..CLIENT_LATENCY_RESTART_STRIKES as u64 {
            watchdog.record_ping(timestamp, Instant::now());
            rig.advance_time(Duration::from_secs(4)).await;
            assert!(watchdog.expire_pending(Instant::now()).is_some());
        }

        assert!(watchdog.should_restart());
        assert!(matches!(
            client_shutdown_exit(watchdog.should_restart()),
            SessionExit::RestartRequested(RestartReason::LatencyWatchdog)
        ));
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
