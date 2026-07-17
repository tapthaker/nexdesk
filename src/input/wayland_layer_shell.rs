//! Wayland layer-shell based input capture for wlroots compositors (Sway, Hyprland).
//!
//! Creates 1px invisible overlay windows at screen edges using `zwlr_layer_shell_v1`.
//! When the cursor hits the edge, we receive a `wl_pointer::Event::Enter`, lock the
//! pointer, and forward relative motion + keyboard events to the server loop.
//! This provides zero-latency edge detection compared to the evdev polling fallback.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::io::FromRawFd;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1, zwp_keyboard_shortcuts_inhibitor_v1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::net::protocol::Direction;

// --- Public API ---

/// Events sent from the Wayland capture task to the server loop.
#[derive(Debug)]
pub enum LayerShellEvent {
    EdgeEnter {
        direction: Direction,
    },
    MouseMove {
        dx: f64,
        dy: f64,
    },
    MouseButton {
        button: u32,
        pressed: bool,
    },
    MouseScroll {
        dx: f64,
        dy: f64,
    },
    ScrollEnd,
    KeyEvent {
        keycode: u32,
        pressed: bool,
    },
    KeyModifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

/// Commands from the server loop to the Wayland capture task.
#[derive(Debug)]
pub enum LayerShellCommand {
    /// Fall back to wl_keyboard capture when the evdev keyboard grab failed.
    CaptureKeyboard,
    Release,
    Shutdown,
}

/// Try to create a layer-shell capture. Returns None if the layer-shell
/// protocol is not available (GNOME, KDE, X11 session).
pub fn try_create(
    trigger_edge: Direction,
) -> Result<
    Option<(
        mpsc::UnboundedReceiver<LayerShellEvent>,
        mpsc::UnboundedSender<LayerShellCommand>,
        u32, // screen_width
        u32, // screen_height
    )>,
> {
    // Must have WAYLAND_DISPLAY to connect
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        debug!("No WAYLAND_DISPLAY set, layer-shell not available");
        return Ok(None);
    }

    let conn = Connection::connect_to_env().wrap_err("Failed to connect to Wayland display")?;

    let display = conn.display();

    let mut event_queue: EventQueue<WaylandState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = WaylandState::new(trigger_edge);

    // Initial registry roundtrip
    display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .wrap_err("Wayland registry roundtrip failed")?;

    // Check required protocols
    let compositor = match state.compositor.clone() {
        Some(c) => c,
        None => return Err(eyre!("wl_compositor not found")),
    };
    let layer_shell = match state.layer_shell.clone() {
        Some(ls) => ls,
        None => {
            info!("zwlr_layer_shell_v1 not available, falling back to evdev");
            return Ok(None);
        }
    };
    let seat = match state.seat.clone() {
        Some(s) => s,
        None => return Err(eyre!("wl_seat not found")),
    };
    let pointer_constraints = match state.pointer_constraints.clone() {
        Some(pc) => pc,
        None => {
            info!("zwp_pointer_constraints_v1 not available, falling back to evdev");
            return Ok(None);
        }
    };
    let relative_pointer_manager = match state.relative_pointer_manager.clone() {
        Some(rpm) => rpm,
        None => {
            info!("zwp_relative_pointer_manager_v1 not available, falling back to evdev");
            return Ok(None);
        }
    };
    let shm = match state.shm.clone() {
        Some(s) => s,
        None => return Err(eyre!("wl_shm not found")),
    };

    // Get pointer and keyboard from seat
    let pointer = seat.get_pointer(&qh, ());
    state.pointer = Some(pointer);
    let keyboard = seat.get_keyboard(&qh, ());
    state.keyboard = Some(keyboard);

    // Enumerate outputs
    if let Some(ref xdg_output_manager) = state.xdg_output_manager {
        for output_info in &state.outputs {
            let _xdg_output = xdg_output_manager.get_xdg_output(
                &output_info.wl_output,
                &qh,
                output_info.wl_output.clone(),
            );
        }
        event_queue
            .roundtrip(&mut state)
            .wrap_err("xdg_output roundtrip failed")?;
        // Need a second roundtrip to get the done events
        event_queue
            .roundtrip(&mut state)
            .wrap_err("xdg_output done roundtrip failed")?;
    }

    // Determine screen dimensions from outputs
    let (screen_w, screen_h) = if state.outputs.is_empty() {
        (1920u32, 1080u32)
    } else {
        // Use the first output's dimensions, or fallback
        let out = &state.outputs[0];
        if out.logical_width > 0 && out.logical_height > 0 {
            (out.logical_width as u32, out.logical_height as u32)
        } else {
            (1920, 1080)
        }
    };

    info!(
        "Layer-shell: screen {}x{}, {} output(s)",
        screen_w,
        screen_h,
        state.outputs.len()
    );

    // Create edge surfaces for each output (or one if no outputs)
    let outputs_to_use: Vec<_> = if state.outputs.is_empty() {
        vec![None]
    } else {
        state
            .outputs
            .iter()
            .map(|o| Some(o.wl_output.clone()))
            .collect()
    };

    for maybe_output in &outputs_to_use {
        let surface = compositor.create_surface(&qh, ());

        // Set input region to the entire surface (None = infinite, which is fine for 1px)
        // Actually, for layer surfaces, the default input region is the surface size, which works.

        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            maybe_output.as_ref(),
            zwlr_layer_shell_v1::Layer::Overlay,
            "nexdesk-edge".to_string(),
            &qh,
            (),
        );

        // Configure anchoring and size based on trigger edge
        let (anchor, w, h) = edge_anchor_size(trigger_edge);
        layer_surface.set_anchor(anchor);
        layer_surface.set_size(w, h);
        layer_surface.set_exclusive_zone(-1); // Don't reserve space
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

        surface.commit();

        state.edge_surfaces.push(EdgeSurface {
            surface: surface.clone(),
            layer_surface,
            direction: trigger_edge,
            configured: false,
            configured_width: 0,
            configured_height: 0,
        });
    }

    // Roundtrip to get configure events
    event_queue
        .roundtrip(&mut state)
        .wrap_err("layer surface configure roundtrip failed")?;

    // Create properly-sized transparent buffers and attach for configured surfaces
    for edge in &state.edge_surfaces {
        if edge.configured {
            let Some((width, height, stride, buf_size)) =
                layer_buffer_geometry(edge.configured_width, edge.configured_height)
            else {
                warn!(
                    "Skipping layer-shell edge buffer with invalid compositor size {}x{}",
                    edge.configured_width, edge.configured_height
                );
                continue;
            };

            let fd = create_shm_file(buf_size)?;
            let pool = shm.create_pool(fd.as_fd(), buf_size as i32, &qh, ());
            let buffer =
                pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &qh, ());
            edge.surface.attach(Some(&buffer), 0, 0);
            edge.surface.commit();
        }
    }

    event_queue
        .roundtrip(&mut state)
        .wrap_err("buffer attach roundtrip failed")?;

    // Store globals on state for the async loop
    state.pointer_constraints = Some(pointer_constraints);
    state.relative_pointer_manager = Some(relative_pointer_manager);

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    state.event_tx = Some(event_tx);

    // Spawn the async event loop
    // Get the raw fd before moving conn into the async task
    let wayland_fd = conn.as_fd().as_raw_fd();
    tokio::spawn(async move {
        if let Err(e) = run_event_loop(conn, event_queue, state, command_rx, wayland_fd).await {
            warn!("Layer-shell event loop ended: {}", e);
        }
    });

    info!("Layer-shell capture active for edge {:?}", trigger_edge);
    Ok(Some((event_rx, command_tx, screen_w, screen_h)))
}

// --- Internal Types ---

struct OutputInfo {
    wl_output: wl_output::WlOutput,
    logical_width: i32,
    logical_height: i32,
}

struct EdgeSurface {
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    direction: Direction,
    configured: bool,
    configured_width: u32,
    configured_height: u32,
}

struct GrabState {
    locked_pointer: zwp_locked_pointer_v1::ZwpLockedPointerV1,
    relative_pointer: zwp_relative_pointer_v1::ZwpRelativePointerV1,
    shortcuts_inhibitor:
        Option<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1>,
    surface: wl_surface::WlSurface,
    keyboard_interactive: bool,
}

/// Key repeat state for a single held key.
struct KeyRepeatState {
    keycode: u32,
    started: Instant,
    last_repeat: Instant,
}

struct WaylandState {
    trigger_edge: Direction,

    // Globals
    compositor: Option<wl_compositor::WlCompositor>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    shm: Option<wl_shm::WlShm>,
    pointer_constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    relative_pointer_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    shortcuts_inhibit_manager:
        Option<zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1>,

    // Input objects
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    // Outputs
    outputs: Vec<OutputInfo>,

    // Edge surfaces
    edge_surfaces: Vec<EdgeSurface>,

    // Grab state
    grab: Option<GrabState>,
    grabbed: bool,

    // Event channel
    event_tx: Option<mpsc::UnboundedSender<LayerShellEvent>>,

    // Pointer enter serial (needed for set_cursor and lock)
    pointer_serial: u32,

    // Key repeat
    repeat_rate: i32,  // keys per second (0 = no repeat)
    repeat_delay: i32, // ms before first repeat
    held_keys: Vec<KeyRepeatState>,
}

impl WaylandState {
    fn new(trigger_edge: Direction) -> Self {
        Self {
            trigger_edge,
            compositor: None,
            layer_shell: None,
            seat: None,
            shm: None,
            pointer_constraints: None,
            relative_pointer_manager: None,
            xdg_output_manager: None,
            shortcuts_inhibit_manager: None,
            pointer: None,
            keyboard: None,
            outputs: Vec::new(),
            edge_surfaces: Vec::new(),
            grab: None,
            grabbed: false,
            event_tx: None,
            pointer_serial: 0,
            repeat_rate: 25,
            repeat_delay: 600,
            held_keys: Vec::new(),
        }
    }

    fn send_event(&self, event: LayerShellEvent) {
        if let Some(ref tx) = self.event_tx {
            tx.send(event).ok();
        }
    }

    fn is_edge_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.edge_surfaces.iter().any(|e| e.surface == *surface)
    }

    fn do_grab(&mut self, qh: &QueueHandle<WaylandState>, surface: &wl_surface::WlSurface) {
        if self.grabbed {
            return;
        }

        let pointer = match &self.pointer {
            Some(p) => p,
            None => return,
        };
        let pc = match &self.pointer_constraints {
            Some(pc) => pc,
            None => return,
        };
        let rpm = match &self.relative_pointer_manager {
            Some(rpm) => rpm,
            None => return,
        };

        // Hide cursor
        pointer.set_cursor(self.pointer_serial, None, 0, 0);

        // Keep keyboard focus on the user's current window. The server normally
        // captures the keyboard directly through evdev; giving this 1px overlay
        // exclusive keyboard focus can make terminals emit focus/resize updates.
        // Keyboard interactivity is enabled later only if the evdev grab fails.

        // Lock pointer
        let locked_pointer = pc.lock_pointer(
            surface,
            pointer,
            None::<&wl_region::WlRegion>,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            qh,
            (),
        );

        // Get relative pointer
        let relative_pointer = rpm.get_relative_pointer(pointer, qh, ());

        // Inhibit shortcuts if both the protocol and seat are available. Global
        // discovery order is compositor-dependent, so do not assume a seat is
        // present just because the inhibitor manager was advertised.
        let shortcuts_inhibitor = match (&self.shortcuts_inhibit_manager, &self.seat) {
            (Some(mgr), Some(seat)) => Some(mgr.inhibit_shortcuts(surface, seat, qh, ())),
            (Some(_), None) => {
                debug!("Layer-shell: shortcuts inhibitor available before seat; skipping inhibit");
                None
            }
            _ => None,
        };

        self.grab = Some(GrabState {
            locked_pointer,
            relative_pointer,
            shortcuts_inhibitor,
            surface: surface.clone(),
            keyboard_interactive: false,
        });
        self.grabbed = true;

        debug!("Layer-shell: pointer grabbed");
    }

    fn capture_keyboard(&mut self) {
        let Some(grab) = self.grab.as_mut() else {
            return;
        };
        if grab.keyboard_interactive {
            return;
        }

        for edge in &self.edge_surfaces {
            if edge.surface == grab.surface {
                edge.layer_surface.set_keyboard_interactivity(
                    zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
                );
                edge.surface.commit();
                grab.keyboard_interactive = true;
                debug!("Layer-shell: using exclusive keyboard-focus fallback");
                break;
            }
        }
    }

    fn release_grab(&mut self) {
        if let Some(grab) = self.grab.take() {
            grab.locked_pointer.destroy();
            grab.relative_pointer.destroy();
            if let Some(inhibitor) = grab.shortcuts_inhibitor {
                inhibitor.destroy();
            }

            // Do not commit the layer surface on the normal evdev path. Avoiding
            // needless layer reconfiguration keeps terminal geometry stable.
            if grab.keyboard_interactive {
                for edge in &self.edge_surfaces {
                    if edge.surface == grab.surface {
                        edge.layer_surface.set_keyboard_interactivity(
                            zwlr_layer_surface_v1::KeyboardInteractivity::None,
                        );
                        edge.surface.commit();
                        break;
                    }
                }
            }
        }
        self.grabbed = false;
        self.held_keys.clear();
        debug!("Layer-shell: pointer released");
    }

    fn next_repeat_deadline(&self) -> Option<Instant> {
        let repeat_interval = repeat_interval(self.repeat_rate)?;
        if self.held_keys.is_empty() {
            return None;
        }
        let delay = repeat_delay_duration(self.repeat_delay);

        self.held_keys
            .iter()
            .map(|ks| {
                let elapsed = ks.started.elapsed();
                if elapsed < delay {
                    ks.started + delay
                } else {
                    ks.last_repeat + repeat_interval
                }
            })
            .min()
    }

    fn fire_repeats(&mut self) {
        let Some(repeat_interval) = repeat_interval(self.repeat_rate) else {
            return;
        };
        let now = Instant::now();
        let delay = repeat_delay_duration(self.repeat_delay);

        for ks in &mut self.held_keys {
            let elapsed = now.duration_since(ks.started);
            if elapsed >= delay && now >= ks.last_repeat + repeat_interval {
                self.event_tx.as_ref().map(|tx| {
                    tx.send(LayerShellEvent::KeyEvent {
                        keycode: ks.keycode,
                        pressed: true,
                    })
                    .ok()
                });
                ks.last_repeat = now;
            }
        }
    }
}

// --- Wayland Dispatch Implementations ---

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    );
                    state.compositor = Some(compositor);
                }
                "zwlr_layer_shell_v1" => {
                    let layer_shell = registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    );
                    state.layer_shell = Some(layer_shell);
                }
                "wl_seat" => {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ());
                    state.seat = Some(seat);
                }
                "wl_shm" => {
                    let shm = registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ());
                    state.shm = Some(shm);
                }
                "zwp_pointer_constraints_v1" => {
                    let pc = registry
                        .bind::<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        );
                    state.pointer_constraints = Some(pc);
                }
                "zwp_relative_pointer_manager_v1" => {
                    let rpm = registry
                        .bind::<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        );
                    state.relative_pointer_manager = Some(rpm);
                }
                "zxdg_output_manager_v1" => {
                    let mgr = registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    );
                    state.xdg_output_manager = Some(mgr);
                }
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    let mgr = registry.bind::<zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1, _, _>(
                        name, version.min(1), qh, (),
                    );
                    state.shortcuts_inhibit_manager = Some(mgr);
                }
                "wl_output" => {
                    let output =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    state.outputs.push(OutputInfo {
                        wl_output: output,
                        logical_width: 0,
                        logical_height: 0,
                    });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial, surface, ..
            } => {
                state.pointer_serial = serial;
                if state.is_edge_surface(&surface) && !state.grabbed {
                    debug!("Layer-shell: pointer entered edge surface");
                    state.do_grab(qh, &surface);
                    state.send_event(LayerShellEvent::EdgeEnter {
                        direction: state.trigger_edge,
                    });
                }
            }
            wl_pointer::Event::Leave { serial, .. } => {
                state.pointer_serial = serial;
            }
            wl_pointer::Event::Button {
                button,
                state: btn_state,
                ..
            } => {
                if state.grabbed {
                    let pressed = btn_state == WEnum::Value(wl_pointer::ButtonState::Pressed);
                    state.send_event(LayerShellEvent::MouseButton { button, pressed });
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if state.grabbed {
                    let (dx, dy) = match axis {
                        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (value, 0.0),
                        WEnum::Value(wl_pointer::Axis::VerticalScroll) => (0.0, value),
                        _ => (0.0, 0.0),
                    };
                    if dx != 0.0 || dy != 0.0 {
                        state.send_event(LayerShellEvent::MouseScroll { dx, dy });
                    }
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                if state.grabbed {
                    state.send_event(LayerShellEvent::ScrollEnd);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            if state.grabbed {
                state.send_event(LayerShellEvent::MouseMove {
                    dx: dx_unaccel,
                    dy: dy_unaccel,
                });
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if state.grabbed {
                    let pressed = key_state == WEnum::Value(wl_keyboard::KeyState::Pressed);
                    state.send_event(LayerShellEvent::KeyEvent {
                        keycode: key,
                        pressed,
                    });

                    // Track held keys for repeat
                    if pressed {
                        // Only repeat non-modifier keys
                        if !is_modifier(key) {
                            // Remove any existing entry for this key
                            state.held_keys.retain(|k| k.keycode != key);
                            let now = Instant::now();
                            state.held_keys.push(KeyRepeatState {
                                keycode: key,
                                started: now,
                                last_repeat: now,
                            });
                        }
                    } else {
                        state.held_keys.retain(|k| k.keycode != key);
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if state.grabbed {
                    state.send_event(LayerShellEvent::KeyModifiers {
                        depressed: mods_depressed,
                        latched: mods_latched,
                        locked: mods_locked,
                        group,
                    });
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_rate = rate;
                state.repeat_delay = delay;
                debug!("Layer-shell: key repeat rate={}/s delay={}ms", rate, delay);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer_surface.ack_configure(serial);
            debug!("Layer surface configured: {}x{}", width, height);
            for edge in &mut state.edge_surfaces {
                if edge.layer_surface == *layer_surface {
                    edge.configured = true;
                    edge.configured_width = width;
                    edge.configured_height = height;
                    break;
                }
            }
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, wl_output::WlOutput> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        output: &wl_output::WlOutput,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                for o in &mut state.outputs {
                    if o.wl_output == *output {
                        o.logical_width = width;
                        o.logical_height = height;
                        debug!("Output logical size: {}x{}", width, height);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

// No-op dispatches for protocols that don't need event handling
delegate_noop!(WaylandState: ignore wl_compositor::WlCompositor);
delegate_noop!(WaylandState: ignore wl_surface::WlSurface);
delegate_noop!(WaylandState: ignore wl_shm::WlShm);
delegate_noop!(WaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandState: ignore wl_buffer::WlBuffer);
delegate_noop!(WaylandState: ignore wl_region::WlRegion);
delegate_noop!(WaylandState: ignore wl_output::WlOutput);
delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore wl_callback::WlCallback);
delegate_noop!(WaylandState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(WaylandState: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(WaylandState: ignore zwp_locked_pointer_v1::ZwpLockedPointerV1);
delegate_noop!(WaylandState: ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(WaylandState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(WaylandState: ignore zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(WaylandState: ignore zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1);

// --- Helpers ---

fn repeat_interval(rate: i32) -> Option<Duration> {
    if rate <= 0 {
        return None;
    }
    let millis = (1000 / u64::try_from(rate).ok()?).max(1);
    Some(Duration::from_millis(millis))
}

fn repeat_delay_duration(delay: i32) -> Duration {
    Duration::from_millis(u64::try_from(delay.max(0)).unwrap_or(0))
}

fn edge_anchor_size(direction: Direction) -> (zwlr_layer_surface_v1::Anchor, u32, u32) {
    use zwlr_layer_surface_v1::Anchor;
    match direction {
        Direction::Right => (
            Anchor::Right | Anchor::Top | Anchor::Bottom,
            1,
            0, // 1px wide, fill height
        ),
        Direction::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, 1, 0),
        Direction::Up => (
            Anchor::Top | Anchor::Left | Anchor::Right,
            0,
            1, // fill width, 1px tall
        ),
        Direction::Down => (Anchor::Bottom | Anchor::Left | Anchor::Right, 0, 1),
    }
}

const MAX_LAYER_SHM_BUFFER_BYTES: usize = 64 * 1024 * 1024;

fn layer_buffer_geometry(width: u32, height: u32) -> Option<(i32, i32, i32, usize)> {
    if width == 0 || height == 0 {
        return None;
    }

    let stride = width.checked_mul(4)?;
    let size = usize::try_from(u64::from(stride).checked_mul(u64::from(height))?).ok()?;
    if size > MAX_LAYER_SHM_BUFFER_BYTES
        || width > i32::MAX as u32
        || height > i32::MAX as u32
        || stride > i32::MAX as u32
    {
        return None;
    }

    Some((width as i32, height as i32, stride as i32, size))
}

/// Create a shared memory file for the 1px transparent buffer.
fn create_shm_file(size: usize) -> Result<OwnedFd> {
    use std::io::Write;
    let file = tempfile::tempfile().wrap_err("Failed to create shm tempfile")?;
    // Write transparent pixels (ARGB8888: all zeros = fully transparent black)
    {
        let mut writer = std::io::BufWriter::new(&file);
        let zeros = vec![0u8; size];
        writer.write_all(&zeros)?;
        writer.flush()?;
    }
    let fd = file.into();
    Ok(fd)
}

/// Modifier keys that should not generate repeat events.
fn is_modifier(keycode: u32) -> bool {
    matches!(
        keycode,
        29 |   // KEY_LEFTCTRL
        42 |   // KEY_LEFTSHIFT
        54 |   // KEY_RIGHTSHIFT
        56 |   // KEY_LEFTALT
        58 |   // KEY_CAPSLOCK
        97 |   // KEY_RIGHTCTRL
        100 |  // KEY_RIGHTALT
        125 |  // KEY_LEFTMETA
        126 // KEY_RIGHTMETA
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_repeat_timing_handles_malformed_compositor_values() {
        assert_eq!(repeat_interval(0), None);
        assert_eq!(repeat_interval(-10), None);
        assert_eq!(repeat_interval(25), Some(Duration::from_millis(40)));
        assert_eq!(repeat_interval(2_000), Some(Duration::from_millis(1)));
        assert_eq!(repeat_delay_duration(-1), Duration::from_millis(0));
        assert_eq!(repeat_delay_duration(600), Duration::from_millis(600));
    }

    #[test]
    fn layer_buffer_geometry_rejects_invalid_or_oversized_compositor_sizes() {
        assert_eq!(layer_buffer_geometry(0, 1), None);
        assert_eq!(layer_buffer_geometry(1, 0), None);
        assert_eq!(layer_buffer_geometry(u32::MAX, 1), None);
        assert_eq!(layer_buffer_geometry(1, u32::MAX), None);
        assert_eq!(
            layer_buffer_geometry(4096, 4096),
            Some((4096, 4096, 16_384, MAX_LAYER_SHM_BUFFER_BYTES))
        );
        assert_eq!(layer_buffer_geometry(4097, 4096), None);
    }
}

// --- Async Event Loop ---

async fn run_event_loop(
    conn: Connection,
    mut event_queue: EventQueue<WaylandState>,
    mut state: WaylandState,
    mut command_rx: mpsc::UnboundedReceiver<LayerShellCommand>,
    wayland_fd: i32,
) -> Result<()> {
    // Safety: we own the connection, the fd remains valid for the connection lifetime
    let async_fd = unsafe { AsyncFd::new(OwnedFd::from_raw_fd(libc::dup(wayland_fd))) }
        .wrap_err("Failed to create AsyncFd for Wayland")?;

    loop {
        // Calculate next repeat deadline for select timeout
        let repeat_deadline = state.next_repeat_deadline();

        tokio::select! {
            ready = async_fd.readable() => {
                let mut guard = ready.wrap_err("AsyncFd readable error")?;

                // Flush any pending outgoing requests
                if let Err(e) = conn.flush() {
                    if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                        return Err(eyre!("Wayland flush error: {}", e));
                    }
                }

                // Try to read and dispatch events
                if let Some(guard_read) = conn.prepare_read() {
                    if let Err(e) = guard_read.read() {
                        if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                            return Err(eyre!("Wayland read error: {}", e));
                        }
                    }
                }

                event_queue.dispatch_pending(&mut state)
                    .wrap_err("Wayland dispatch error")?;

                // Flush again after dispatch (responses may have been queued)
                if let Err(e) = conn.flush() {
                    if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                        return Err(eyre!("Wayland flush error: {}", e));
                    }
                }

                guard.clear_ready();
            }
            cmd = command_rx.recv() => {
                match cmd {
                    Some(LayerShellCommand::CaptureKeyboard) => {
                        state.capture_keyboard();
                        conn.flush().ok();
                    }
                    Some(LayerShellCommand::Release) => {
                        state.release_grab();
                        conn.flush().ok();
                    }
                    Some(LayerShellCommand::Shutdown) | None => {
                        state.release_grab();
                        conn.flush().ok();
                        break;
                    }
                }
            }
            _ = async {
                match repeat_deadline {
                    Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // Fire key repeats
                state.fire_repeats();
            }
        }
    }

    Ok(())
}
