use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr, eyre};
use evdev::{AbsoluteAxisType, Device, InputEventKind, Key, RelativeAxisType};
use tracing::{debug, warn};
use x11rb::connection::Connection;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

#[derive(Debug, Clone)]
enum PointerKind {
    /// Mouse — uses REL_X/REL_Y relative deltas
    Relative,
    /// Touchpad/trackpad — uses ABS_X/ABS_Y absolute coordinates,
    /// converted to relative deltas based on finger movement.
    Absolute {
        abs_x_range: f64,
        abs_y_range: f64,
    },
}

struct PointerDevice {
    device: Device,
    kind: PointerKind,
    /// Last ABS_X/ABS_Y seen (for computing deltas on touchpads)
    last_abs_x: Option<i32>,
    last_abs_y: Option<i32>,
    /// Whether a finger is currently touching the pad
    touching: bool,
    /// Number of fingers on the touchpad (1 = move, 2 = scroll)
    finger_count: u32,
}

/// Collect all /dev/input/event* entries once for reuse.
fn input_event_entries() -> Result<Vec<PathBuf>> {
    let entries: Vec<PathBuf> = std::fs::read_dir("/dev/input")
        .wrap_err("Cannot read /dev/input")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains("event"))
        .collect();
    Ok(entries)
}

/// Find pointer devices: mice (REL_X/REL_Y) and touchpads (ABS_X/ABS_Y + BTN_TOUCH).
/// Returns (path, kind) pairs and also collects the paths that were claimed as pointers.
fn find_pointer_devices(entries: &[PathBuf]) -> Result<(Vec<(PathBuf, PointerKind)>, HashSet<PathBuf>)> {
    let mut found = Vec::new();
    let mut claimed = HashSet::new();

    for path in entries {
        let device = match Device::open(path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let name = device.name().unwrap_or("unknown");

        // Check for relative-axis mouse
        if let Some(axes) = device.supported_relative_axes() {
            if axes.contains(RelativeAxisType::REL_X) && axes.contains(RelativeAxisType::REL_Y) {
                debug!("Found relative pointer: {} ({})", name, path.display());
                found.push((path.clone(), PointerKind::Relative));
                claimed.insert(path.clone());
                continue;
            }
        }

        // Check for absolute-axis touchpad (must have BTN_TOUCH to distinguish
        // from tablets/joysticks, or have "trackpad"/"touchpad" in name)
        if let Some(axes) = device.supported_absolute_axes() {
            if axes.contains(AbsoluteAxisType::ABS_X) && axes.contains(AbsoluteAxisType::ABS_Y) {
                let has_btn_touch = device.supported_keys()
                    .map_or(false, |k| k.contains(Key::BTN_TOUCH));
                let name_lower = name.to_lowercase();
                let name_match = name_lower.contains("trackpad") || name_lower.contains("touchpad");

                if has_btn_touch || name_match {
                    let abs_info_x = device.get_abs_state().map(|s| {
                        s.get(AbsoluteAxisType::ABS_X.0 as usize).cloned()
                    }).ok().flatten();
                    let abs_info_y = device.get_abs_state().map(|s| {
                        s.get(AbsoluteAxisType::ABS_Y.0 as usize).cloned()
                    }).ok().flatten();

                    if let (Some(ax), Some(ay)) = (abs_info_x, abs_info_y) {
                        let x_range = (ax.maximum - ax.minimum).max(1) as f64;
                        let y_range = (ay.maximum - ay.minimum).max(1) as f64;
                        debug!("Found absolute pointer: {} ({}) x:[{}..{}] y:[{}..{}]",
                               name, path.display(), ax.minimum, ax.maximum, ay.minimum, ay.maximum);
                        found.push((path.clone(), PointerKind::Absolute {
                            abs_x_range: x_range,
                            abs_y_range: y_range,
                        }));
                        claimed.insert(path.clone());
                        continue;
                    }
                }
            }
        }
    }

    if found.is_empty() {
        return Err(eyre!(
            "No pointer devices found. Make sure user is in the 'input' group: sudo usermod -aG input $USER"
        ));
    }

    Ok((found, claimed))
}

/// Find keyboard devices: devices with KEY_A support that are not already
/// claimed as pointer devices.
fn find_keyboard_devices(entries: &[PathBuf], pointer_paths: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for path in entries {
        if pointer_paths.contains(path) {
            continue;
        }
        let device = match Device::open(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let has_key_a = device.supported_keys()
            .map_or(false, |k| k.contains(Key::KEY_A));
        if has_key_a {
            let name = device.name().unwrap_or("unknown");
            debug!("Found keyboard: {} ({})", name, path.display());
            found.push(path.clone());
        }
    }
    found
}

/// Get screen size from X11 (XWayland provides correct screen dimensions)
/// or fallback to a DRM-based approach.
fn get_screen_size() -> (u32, u32) {
    // Try X11 first (XWayland reports correct size)
    if let Ok((conn, screen_num)) = x11rb::rust_connection::RustConnection::connect(None) {
        let screen = &conn.setup().roots[screen_num];
        let w = screen.width_in_pixels as u32;
        let h = screen.height_in_pixels as u32;
        if w > 0 && h > 0 {
            return (w, h);
        }
    }

    // Fallback
    (1920, 1080)
}

/// Evdev-based input capturer for Wayland.
/// Tracks cursor position from raw evdev events for edge detection and
/// forwards deltas to the remote when grabbed (active sharing).
pub struct WaylandCapturer {
    devices: Vec<PointerDevice>,
    keyboard_devices: Vec<Device>,
    cursor_x: i32,
    cursor_y: i32,
    grabbed: bool,
    screen_width: u32,
    screen_height: u32,
    pressed_keys: HashSet<u32>,
    buttons: u8,
    /// Accumulated scroll deltas (pixels) since last poll
    scroll_acc_x: f64,
    scroll_acc_y: f64,
}

impl WaylandCapturer {
    pub fn new() -> Result<Self> {
        let entries = input_event_entries()?;
        let (found, pointer_paths) = find_pointer_devices(&entries)?;
        let mut devices = Vec::new();
        for (path, kind) in &found {
            let device = Device::open(path)
                .wrap_err_with(|| format!("Failed to open {}", path.display()))?;
            // Set non-blocking via fcntl so poll doesn't block the async runtime
            let fd = device.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            devices.push(PointerDevice {
                device,
                kind: kind.clone(),
                last_abs_x: None,
                last_abs_y: None,
                touching: false,
                finger_count: 0,
            });
        }

        // Open keyboard devices (separate from pointer devices)
        let kb_paths = find_keyboard_devices(&entries, &pointer_paths);
        let mut keyboard_devices = Vec::new();
        for path in &kb_paths {
            let device = Device::open(path)
                .wrap_err_with(|| format!("Failed to open keyboard {}", path.display()))?;
            let fd = device.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            keyboard_devices.push(device);
        }

        let (screen_width, screen_height) = get_screen_size();
        debug!("Wayland evdev capturer: screen {}x{}, {} pointer(s), {} keyboard(s)",
               screen_width, screen_height, devices.len(), keyboard_devices.len());

        // Start cursor at center of screen
        let cursor_x = screen_width as i32 / 2;
        let cursor_y = screen_height as i32 / 2;

        Ok(Self {
            devices,
            keyboard_devices,
            cursor_x,
            cursor_y,
            grabbed: false,
            screen_width,
            screen_height,
            pressed_keys: HashSet::new(),
            buttons: 0,
            scroll_acc_x: 0.0,
            scroll_acc_y: 0.0,
        })
    }

    /// Touchpad-to-screen speed multiplier.
    /// Slightly below compositor speed so edge detection triggers
    /// close to when the real cursor reaches the edge.
    const TOUCHPAD_SPEED: f64 = 1.2;

    /// Pixels per scroll-wheel notch for REL_WHEEL/REL_HWHEEL events.
    const WHEEL_PIXELS: f64 = 15.0;

    /// Touchpad scroll sensitivity (fraction of screen per full-pad swipe).
    const TOUCHPAD_SCROLL_SPEED: f64 = 0.8;

    /// Process all pending events from all devices.
    fn drain_events(&mut self) {
        let sw = self.screen_width as f64;
        let sh = self.screen_height as f64;

        for pdev in &mut self.devices {
            loop {
                match pdev.device.fetch_events() {
                    Ok(events) => {
                        let mut got_any = false;
                        for event in events {
                            got_any = true;
                            match event.kind() {
                                InputEventKind::RelAxis(axis) => {
                                    let val = event.value();
                                    match axis {
                                        RelativeAxisType::REL_X => {
                                            self.cursor_x += val;
                                        }
                                        RelativeAxisType::REL_Y => {
                                            self.cursor_y += val;
                                        }
                                        RelativeAxisType::REL_WHEEL => {
                                            // Vertical scroll: positive = up
                                            self.scroll_acc_y += val as f64 * Self::WHEEL_PIXELS;
                                        }
                                        RelativeAxisType::REL_HWHEEL => {
                                            // Horizontal scroll: positive = right
                                            self.scroll_acc_x += val as f64 * Self::WHEEL_PIXELS;
                                        }
                                        _ => {}
                                    }
                                }
                                InputEventKind::AbsAxis(axis) => {
                                    if let PointerKind::Absolute { abs_x_range, abs_y_range } = &pdev.kind {
                                        // Only track movement while finger is touching
                                        if !pdev.touching {
                                            continue;
                                        }
                                        let val = event.value();

                                        if pdev.finger_count >= 2 {
                                            // Two-finger scroll mode: ABS deltas → scroll
                                            match axis {
                                                AbsoluteAxisType::ABS_X => {
                                                    if let Some(prev) = pdev.last_abs_x {
                                                        let delta = (val - prev) as f64 / abs_x_range * sw * Self::TOUCHPAD_SCROLL_SPEED;
                                                        self.scroll_acc_x += delta;
                                                    }
                                                    pdev.last_abs_x = Some(val);
                                                }
                                                AbsoluteAxisType::ABS_Y => {
                                                    if let Some(prev) = pdev.last_abs_y {
                                                        let delta = (val - prev) as f64 / abs_y_range * sh * Self::TOUCHPAD_SCROLL_SPEED;
                                                        // Negate: finger moving down → content scrolls up (natural scrolling)
                                                        self.scroll_acc_y -= delta;
                                                    }
                                                    pdev.last_abs_y = Some(val);
                                                }
                                                _ => {}
                                            }
                                        } else {
                                            // Single-finger cursor movement
                                            match axis {
                                                AbsoluteAxisType::ABS_X => {
                                                    if let Some(prev) = pdev.last_abs_x {
                                                        let delta = (val - prev) as f64 / abs_x_range * sw * Self::TOUCHPAD_SPEED;
                                                        self.cursor_x += delta as i32;
                                                    }
                                                    pdev.last_abs_x = Some(val);
                                                }
                                                AbsoluteAxisType::ABS_Y => {
                                                    if let Some(prev) = pdev.last_abs_y {
                                                        let delta = (val - prev) as f64 / abs_y_range * sh * Self::TOUCHPAD_SPEED;
                                                        self.cursor_y += delta as i32;
                                                    }
                                                    pdev.last_abs_y = Some(val);
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                InputEventKind::Key(key) => {
                                    let code = key.code() as u32;
                                    let pressed = event.value() != 0;

                                    match key {
                                        Key::BTN_TOUCH => {
                                            // Finger down/up on touchpad — reset tracking
                                            pdev.touching = pressed;
                                            if !pressed {
                                                pdev.last_abs_x = None;
                                                pdev.last_abs_y = None;
                                                pdev.finger_count = 0;
                                            }
                                        }
                                        Key::BTN_TOOL_FINGER => {
                                            // Single finger on pad
                                            if pressed {
                                                pdev.finger_count = 1;
                                            }
                                        }
                                        Key::BTN_TOOL_DOUBLETAP => {
                                            // Two fingers on pad — switch to scroll mode
                                            if pressed {
                                                pdev.finger_count = 2;
                                            } else if pdev.finger_count == 2 {
                                                pdev.finger_count = 1;
                                            }
                                            // Reset position tracking to avoid jump
                                            pdev.last_abs_x = None;
                                            pdev.last_abs_y = None;
                                        }
                                        Key::BTN_TOOL_TRIPLETAP => {
                                            if pressed {
                                                pdev.finger_count = 3;
                                            } else if pdev.finger_count == 3 {
                                                pdev.finger_count = 1;
                                            }
                                            pdev.last_abs_x = None;
                                            pdev.last_abs_y = None;
                                        }
                                        Key::BTN_LEFT => {
                                            if pressed { self.buttons |= 1; }
                                            else { self.buttons &= !1; }
                                        }
                                        Key::BTN_RIGHT => {
                                            if pressed { self.buttons |= 2; }
                                            else { self.buttons &= !2; }
                                        }
                                        Key::BTN_MIDDLE => {
                                            if pressed { self.buttons |= 4; }
                                            else { self.buttons &= !4; }
                                        }
                                        _ => {
                                            // Keyboard key tracking
                                            if pressed {
                                                self.pressed_keys.insert(code);
                                            } else {
                                                self.pressed_keys.remove(&code);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !got_any {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // No clamping here — the raw position is used to compute deltas
        // when sharing to a remote screen. The server loop clamps as needed
        // for local edge detection.

        // Drain keyboard devices for key events
        for kdev in &mut self.keyboard_devices {
            loop {
                match kdev.fetch_events() {
                    Ok(events) => {
                        let mut got_any = false;
                        for event in events {
                            got_any = true;
                            if let InputEventKind::Key(key) = event.kind() {
                                let code = key.code() as u32;
                                let pressed = event.value() != 0;
                                if pressed {
                                    self.pressed_keys.insert(code);
                                } else {
                                    self.pressed_keys.remove(&code);
                                }
                            }
                        }
                        if !got_any {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }
}

impl InputCapture for WaylandCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        Ok((self.cursor_x, self.cursor_y))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        Ok(self.buttons)
    }

    fn set_grab(&mut self, grab: bool) -> Result<()> {
        for pdev in &mut self.devices {
            if grab {
                pdev.device.grab().wrap_err("Failed to grab pointer device")?;
            } else {
                pdev.device.ungrab().wrap_err("Failed to ungrab pointer device")?;
            }
        }
        for kdev in &mut self.keyboard_devices {
            if grab {
                kdev.grab().wrap_err("Failed to grab keyboard device")?;
            } else {
                kdev.ungrab().wrap_err("Failed to ungrab keyboard device")?;
            }
        }
        self.grabbed = grab;
        debug!("Input devices {} ({} pointers, {} keyboards)",
               if grab { "grabbed" } else { "ungrabbed" },
               self.devices.len(), self.keyboard_devices.len());
        Ok(())
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        // Drain all pending events first — this updates cursor position, buttons, keys, and scroll
        let old_keys: HashSet<u32> = self.pressed_keys.clone();
        self.drain_events();

        // Compute key state changes
        let mut events = Vec::new();
        for &code in &self.pressed_keys {
            if !old_keys.contains(&code) {
                events.push(Message::KeyEvent {
                    keycode: code,
                    pressed: true,
                    modifiers: 0,
                });
            }
        }
        for &code in &old_keys {
            if !self.pressed_keys.contains(&code) {
                events.push(Message::KeyEvent {
                    keycode: code,
                    pressed: false,
                    modifiers: 0,
                });
            }
        }

        // Emit accumulated scroll events
        let sx = self.scroll_acc_x as i32;
        let sy = self.scroll_acc_y as i32;
        if sx != 0 || sy != 0 {
            events.push(Message::MouseScroll { dx: sx, dy: sy });
            self.scroll_acc_x = 0.0;
            self.scroll_acc_y = 0.0;
        }

        Ok(events)
    }
}

/// Wayland-based input injector (stub — Linux server uses X11 for injection
/// since XWayland accepts XTest events even under Wayland).
pub struct WaylandInjector;

impl WaylandInjector {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl InputInjector for WaylandInjector {
    fn inject(&mut self, _event: &Message) -> Result<()> {
        warn!("Wayland input injection not yet implemented");
        Ok(())
    }

    fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok(get_screen_size())
    }
}
