use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result, WrapErr};
use evdev::{AbsoluteAxisType, Device, InputEventKind, Key, RelativeAxisType};
use tracing::{debug, info, warn};
use x11rb::connection::Connection;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::{Message, MAX_KEYCODE};

fn record_key_event(
    pressed_keys: &mut HashSet<u32>,
    pending: &mut Vec<Message>,
    code: u32,
    value: i32,
) {
    if code > MAX_KEYCODE {
        return;
    }

    match value {
        0 => {
            pressed_keys.remove(&code);
            pending.push(Message::KeyEvent {
                keycode: code,
                pressed: false,
                modifiers: 0,
            });
        }
        1 => {
            pressed_keys.insert(code);
            pending.push(Message::KeyEvent {
                keycode: code,
                pressed: true,
                modifiers: 0,
            });
        }
        // Preserve the source machine's evdev repeat timing. Injected macOS
        // key-down events do not initiate native repeat on their own.
        2 if pressed_keys.contains(&code) => pending.push(Message::KeyEvent {
            keycode: code,
            pressed: true,
            modifiers: 0,
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_key_events_preserve_order_repeats_and_held_state() {
        let mut pressed = HashSet::new();
        let mut events = Vec::new();

        record_key_event(&mut pressed, &mut events, 30, 1);
        record_key_event(&mut pressed, &mut events, 30, 2);
        record_key_event(&mut pressed, &mut events, 30, 0);
        record_key_event(&mut pressed, &mut events, 30, 2);

        assert!(!pressed.contains(&30));
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            Message::KeyEvent {
                keycode: 30,
                pressed: true,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            Message::KeyEvent {
                keycode: 30,
                pressed: true,
                ..
            }
        ));
        assert!(matches!(
            events[2],
            Message::KeyEvent {
                keycode: 30,
                pressed: false,
                ..
            }
        ));
    }

    #[test]
    fn evdev_key_events_reject_protocol_unsupported_keycodes() {
        let mut pressed = HashSet::new();
        let mut events = Vec::new();

        record_key_event(&mut pressed, &mut events, MAX_KEYCODE + 1, 1);

        assert!(pressed.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn input_topology_refresh_is_order_insensitive_and_detects_hotplug() {
        let first = vec![
            PathBuf::from("/dev/input/event1"),
            PathBuf::from("/dev/input/event2"),
        ];
        let reordered = vec![
            PathBuf::from("/dev/input/event2"),
            PathBuf::from("/dev/input/event1"),
        ];
        let replaced = vec![
            PathBuf::from("/dev/input/event2"),
            PathBuf::from("/dev/input/event3"),
        ];

        assert!(!input_topology_requires_refresh(false, &first, &reordered));
        assert!(input_topology_requires_refresh(false, &first, &replaced));
        assert!(input_topology_requires_refresh(true, &first, &reordered));
    }
}

#[derive(Debug, Clone)]
enum PointerKind {
    /// Mouse — uses REL_X/REL_Y relative deltas
    Relative,
    /// Touchpad/trackpad — uses ABS_X/ABS_Y absolute coordinates,
    /// converted to relative deltas based on finger movement.
    Absolute { abs_x_range: f64, abs_y_range: f64 },
}

type PointerDevices = (Vec<(PathBuf, PointerKind)>, HashSet<PathBuf>);

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

struct KeyboardDevice {
    path: PathBuf,
    device: Device,
}

fn device_paths_changed(current: &[PathBuf], discovered: &[PathBuf]) -> bool {
    current.len() != discovered.len() || current.iter().any(|path| !discovered.contains(path))
}

fn input_topology_requires_refresh(
    force: bool,
    current: &[PathBuf],
    discovered: &[PathBuf],
) -> bool {
    force || device_paths_changed(current, discovered)
}

fn open_keyboard_device(path: &PathBuf) -> Result<KeyboardDevice> {
    let device = Device::open(path)
        .wrap_err_with(|| format!("Failed to open keyboard {}", path.display()))?;
    let fd = device.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    Ok(KeyboardDevice {
        path: path.clone(),
        device,
    })
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
fn find_pointer_devices(entries: &[PathBuf]) -> Result<PointerDevices> {
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
                let has_btn_touch = device
                    .supported_keys()
                    .is_some_and(|k| k.contains(Key::BTN_TOUCH));
                let name_lower = name.to_lowercase();
                let name_match = name_lower.contains("trackpad") || name_lower.contains("touchpad");

                if has_btn_touch || name_match {
                    let abs_info_x = device
                        .get_abs_state()
                        .map(|s| s.get(AbsoluteAxisType::ABS_X.0 as usize).cloned())
                        .ok()
                        .flatten();
                    let abs_info_y = device
                        .get_abs_state()
                        .map(|s| s.get(AbsoluteAxisType::ABS_Y.0 as usize).cloned())
                        .ok()
                        .flatten();

                    if let (Some(ax), Some(ay)) = (abs_info_x, abs_info_y) {
                        let x_range = (ax.maximum - ax.minimum).max(1) as f64;
                        let y_range = (ay.maximum - ay.minimum).max(1) as f64;
                        debug!(
                            "Found absolute pointer: {} ({}) x:[{}..{}] y:[{}..{}]",
                            name,
                            path.display(),
                            ax.minimum,
                            ax.maximum,
                            ay.minimum,
                            ay.maximum
                        );
                        found.push((
                            path.clone(),
                            PointerKind::Absolute {
                                abs_x_range: x_range,
                                abs_y_range: y_range,
                            },
                        ));
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

fn has_keyboard_or_media_keys(device: &Device) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(Key::KEY_A)
            || keys.contains(Key::KEY_LEFTMETA)
            || keys.contains(Key::KEY_RIGHTMETA)
            || keys.contains(Key::KEY_EQUAL)
            || keys.contains(Key::KEY_MINUS)
            || keys.contains(Key::KEY_MUTE)
            || keys.contains(Key::KEY_VOLUMEDOWN)
            || keys.contains(Key::KEY_VOLUMEUP)
            || keys.contains(Key::KEY_PLAYPAUSE)
            || keys.contains(Key::KEY_NEXTSONG)
            || keys.contains(Key::KEY_PREVIOUSSONG)
            || keys.contains(Key::KEY_BRIGHTNESSDOWN)
            || keys.contains(Key::KEY_BRIGHTNESSUP)
    })
}

/// Find keyboard devices, including separate consumer-control devices used
/// by many keyboards for media keys.
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
        if has_keyboard_or_media_keys(&device) {
            let name = device.name().unwrap_or("unknown");
            debug!("Found keyboard: {} ({})", name, path.display());
            found.push(path.clone());
        }
    }
    found
}

/// Get screen size from X11. XWayland provides the compositor dimensions.
pub(crate) fn query_screen_size() -> Result<(u32, u32)> {
    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
        .wrap_err("Failed to connect to X11 for screen dimensions")?;
    let screen = &conn.setup().roots[screen_num];
    let size = (
        screen.width_in_pixels as u32,
        screen.height_in_pixels as u32,
    );
    if size.0 == 0 || size.1 == 0 {
        return Err(eyre!(
            "X11 returned an invalid screen size {}x{}",
            size.0,
            size.1
        ));
    }
    Ok(size)
}

fn get_screen_size() -> (u32, u32) {
    query_screen_size().unwrap_or((1920, 1080))
}

/// Evdev-based input capturer for Wayland.
/// Tracks cursor position from raw evdev events for edge detection and
/// forwards deltas to the remote when grabbed (active sharing).
pub struct WaylandCapturer {
    devices: Vec<PointerDevice>,
    keyboard_devices: Vec<KeyboardDevice>,
    cursor_x: i32,
    cursor_y: i32,
    grabbed: bool,
    keyboard_grabbed: bool,
    last_keyboard_refresh: Instant,
    input_event_paths: Vec<PathBuf>,
    screen_width: u32,
    screen_height: u32,
    pressed_keys: HashSet<u32>,
    pending_key_events: Vec<Message>,
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
            keyboard_devices.push(open_keyboard_device(path)?);
        }

        let (screen_width, screen_height) = get_screen_size();
        debug!(
            "Wayland evdev capturer: screen {}x{}, {} pointer(s), {} keyboard(s)",
            screen_width,
            screen_height,
            devices.len(),
            keyboard_devices.len()
        );

        // Start cursor at center of screen
        let cursor_x = screen_width as i32 / 2;
        let cursor_y = screen_height as i32 / 2;

        Ok(Self {
            devices,
            keyboard_devices,
            cursor_x,
            cursor_y,
            grabbed: false,
            keyboard_grabbed: false,
            last_keyboard_refresh: Instant::now(),
            input_event_paths: entries,
            screen_width,
            screen_height,
            pressed_keys: HashSet::new(),
            pending_key_events: Vec::new(),
            buttons: 0,
            scroll_acc_x: 0.0,
            scroll_acc_y: 0.0,
        })
    }

    const KEYBOARD_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

    /// Touchpad-to-screen speed multiplier.
    /// Slightly below compositor speed so edge detection triggers
    /// close to when the real cursor reaches the edge.
    const TOUCHPAD_SPEED: f64 = 1.2;

    /// Pixels per scroll-wheel notch for REL_WHEEL/REL_HWHEEL events.
    const WHEEL_PIXELS: f64 = 15.0;

    /// Touchpad scroll sensitivity (fraction of screen per full-pad swipe).
    const TOUCHPAD_SCROLL_SPEED: f64 = 0.8;

    fn refresh_keyboard_devices(&mut self, force: bool) -> Result<()> {
        self.last_keyboard_refresh = Instant::now();
        let entries = input_event_entries()?;

        // Reading the directory is cheap. Probing every evdev device is not:
        // capability ioctls can take hundreds of milliseconds on some HID
        // devices, so only do the expensive discovery after actual hotplug.
        if !input_topology_requires_refresh(force, &self.input_event_paths, &entries) {
            return Ok(());
        }

        let pointer_paths = find_pointer_devices(&entries)
            .map(|(_, paths)| paths)
            .unwrap_or_default();
        let discovered = find_keyboard_devices(&entries, &pointer_paths);
        let current = self
            .keyboard_devices
            .iter()
            .map(|keyboard| keyboard.path.clone())
            .collect::<Vec<_>>();
        self.input_event_paths = entries;
        if !force && !device_paths_changed(&current, &discovered) {
            return Ok(());
        }

        let mut replacements = Vec::new();
        for path in &discovered {
            let mut keyboard = open_keyboard_device(path)?;
            if self.keyboard_grabbed {
                keyboard
                    .device
                    .grab()
                    .wrap_err_with(|| format!("Failed to grab keyboard {}", path.display()))?;
            }
            replacements.push(keyboard);
        }

        for keycode in self.pressed_keys.drain() {
            self.pending_key_events.push(Message::KeyEvent {
                keycode,
                pressed: false,
                modifiers: 0,
            });
        }
        self.keyboard_devices = replacements;
        info!(
            "Refreshed Wayland keyboard devices after input hotplug ({} device(s))",
            self.keyboard_devices.len()
        );
        Ok(())
    }

    /// Read one currently available batch from each keyboard. Avoid looping
    /// until WouldBlock: a high-rate device can keep refilling its queue and
    /// monopolize the async connection task.
    fn drain_keyboard_events(&mut self) -> Result<()> {
        let mut refresh_required = false;
        for keyboard in &mut self.keyboard_devices {
            match keyboard.device.fetch_events() {
                Ok(events) => {
                    for event in events {
                        if let InputEventKind::Key(key) = event.kind() {
                            record_key_event(
                                &mut self.pressed_keys,
                                &mut self.pending_key_events,
                                key.code() as u32,
                                event.value(),
                            );
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    warn!(
                        "Keyboard device {} became unavailable: {}",
                        keyboard.path.display(),
                        error
                    );
                    refresh_required = true;
                }
            }
        }
        if refresh_required {
            self.refresh_keyboard_devices(true)?;
        } else if self.last_keyboard_refresh.elapsed() >= Self::KEYBOARD_REFRESH_INTERVAL {
            self.refresh_keyboard_devices(false)?;
        }
        Ok(())
    }

    /// Process all pending events from all devices.
    fn drain_events(&mut self) {
        let sw = self.screen_width as f64;
        let sh = self.screen_height as f64;

        // One fetch per pointer per poll bounds work even when a high-rate
        // device continuously replenishes its kernel queue.
        for pdev in &mut self.devices {
            match pdev.device.fetch_events() {
                Ok(events) => {
                    for event in events {
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
                                if let PointerKind::Absolute {
                                    abs_x_range,
                                    abs_y_range,
                                } = &pdev.kind
                                {
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
                                                    let delta = (val - prev) as f64 / abs_x_range
                                                        * sw
                                                        * Self::TOUCHPAD_SCROLL_SPEED;
                                                    self.scroll_acc_x += delta;
                                                }
                                                pdev.last_abs_x = Some(val);
                                            }
                                            AbsoluteAxisType::ABS_Y => {
                                                if let Some(prev) = pdev.last_abs_y {
                                                    let delta = (val - prev) as f64 / abs_y_range
                                                        * sh
                                                        * Self::TOUCHPAD_SCROLL_SPEED;
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
                                                    let delta = (val - prev) as f64 / abs_x_range
                                                        * sw
                                                        * Self::TOUCHPAD_SPEED;
                                                    self.cursor_x += delta as i32;
                                                }
                                                pdev.last_abs_x = Some(val);
                                            }
                                            AbsoluteAxisType::ABS_Y => {
                                                if let Some(prev) = pdev.last_abs_y {
                                                    let delta = (val - prev) as f64 / abs_y_range
                                                        * sh
                                                        * Self::TOUCHPAD_SPEED;
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
                                        if pressed {
                                            self.buttons |= 1;
                                        } else {
                                            self.buttons &= !1;
                                        }
                                    }
                                    Key::BTN_RIGHT => {
                                        if pressed {
                                            self.buttons |= 2;
                                        } else {
                                            self.buttons &= !2;
                                        }
                                    }
                                    Key::BTN_MIDDLE => {
                                        if pressed {
                                            self.buttons |= 4;
                                        } else {
                                            self.buttons &= !4;
                                        }
                                    }
                                    _ => {
                                        record_key_event(
                                            &mut self.pressed_keys,
                                            &mut self.pending_key_events,
                                            code,
                                            event.value(),
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => warn!("Failed to read pointer events: {}", error),
            }
        }

        // No clamping here — the raw position is used to compute deltas
        // when sharing to a remote screen. The server loop clamps as needed
        // for local edge detection.

        if let Err(error) = self.drain_keyboard_events() {
            warn!("Failed to refresh keyboard devices: {}", error);
        }
    }
}

impl InputCapture for WaylandCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        Ok((self.cursor_x, self.cursor_y))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok(get_screen_size())
    }

    fn mouse_buttons(&self) -> Result<u8> {
        Ok(self.buttons)
    }

    fn set_grab(&mut self, grab: bool) -> Result<()> {
        if grab {
            // Check the cheap /dev/input topology snapshot first. Reopening and
            // probing every device on every activation can block input handoff
            // for hundreds of milliseconds even when nothing was hotplugged.
            self.refresh_keyboard_devices(false)?;
        }
        for pdev in &mut self.devices {
            if grab {
                pdev.device
                    .grab()
                    .wrap_err("Failed to grab pointer device")?;
            } else {
                pdev.device
                    .ungrab()
                    .wrap_err("Failed to ungrab pointer device")?;
            }
        }
        for keyboard in &mut self.keyboard_devices {
            if grab {
                keyboard
                    .device
                    .grab()
                    .wrap_err("Failed to grab keyboard device")?;
            } else {
                keyboard
                    .device
                    .ungrab()
                    .wrap_err("Failed to ungrab keyboard device")?;
            }
        }
        self.grabbed = grab;
        self.keyboard_grabbed = grab;
        debug!(
            "Input devices {} ({} pointers, {} keyboards)",
            if grab { "grabbed" } else { "ungrabbed" },
            self.devices.len(),
            self.keyboard_devices.len()
        );
        Ok(())
    }

    fn set_keyboard_grab(&mut self, grab: bool) -> Result<()> {
        if grab {
            // Periodic polling maintains input_event_paths, so activation only
            // needs the inexpensive topology check. A real path change still
            // triggers a full keyboard refresh before devices are grabbed.
            self.refresh_keyboard_devices(false)?;
        }
        for keyboard in &mut self.keyboard_devices {
            if grab {
                keyboard
                    .device
                    .grab()
                    .wrap_err("Failed to grab keyboard device")?;
            } else {
                keyboard
                    .device
                    .ungrab()
                    .wrap_err("Failed to ungrab keyboard device")?;
            }
        }
        self.keyboard_grabbed = grab;
        debug!(
            "Keyboard devices {} ({})",
            if grab { "grabbed" } else { "ungrabbed" },
            self.keyboard_devices.len()
        );
        Ok(())
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        // Preserve kernel event order, including EV_KEY value 2 repeats.
        self.drain_events();
        let mut events = std::mem::take(&mut self.pending_key_events);

        // Emit accumulated scroll events (discrete mouse wheel — no phase)
        let sx = self.scroll_acc_x;
        let sy = self.scroll_acc_y;
        if sx != 0.0 || sy != 0.0 {
            events.push(Message::MouseScroll {
                dx: sx,
                dy: sy,
                phase: crate::net::protocol::ScrollPhase::None,
            });
            self.scroll_acc_x = 0.0;
            self.scroll_acc_y = 0.0;
        }

        Ok(events)
    }

    fn poll_key_events_only(&mut self) -> Result<Vec<Message>> {
        self.drain_keyboard_events()?;
        Ok(std::mem::take(&mut self.pending_key_events))
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
