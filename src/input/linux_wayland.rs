use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use color_eyre::eyre::{eyre, Result, WrapErr};
use evdev::{AbsoluteAxisType, Device, InputEventKind, Key, RelativeAxisType};
use tracing::debug;
use x11rb::connection::Connection;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::{Message, MAX_KEYCODE, MAX_SCROLL_DELTA};

#[derive(Debug, Clone)]
enum PointerKind {
    /// Mouse — uses REL_X/REL_Y relative deltas
    Relative,
    /// Touchpad/trackpad — uses ABS_X/ABS_Y absolute coordinates,
    /// converted to relative deltas based on finger movement.
    Absolute { abs_x_range: f64, abs_y_range: f64 },
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
fn find_pointer_devices(
    entries: &[PathBuf],
) -> Result<(Vec<(PathBuf, PointerKind)>, HashSet<PathBuf>)> {
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
                    .map_or(false, |k| k.contains(Key::BTN_TOUCH));
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
                        let x_range = abs_axis_range(ax.minimum, ax.maximum);
                        let y_range = abs_axis_range(ay.minimum, ay.maximum);
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

fn saturating_i32_add(current: i32, delta: i32) -> i32 {
    current.saturating_add(delta)
}

fn abs_axis_range(minimum: i32, maximum: i32) -> f64 {
    i64::from(maximum).saturating_sub(i64::from(minimum)).max(1) as f64
}

fn abs_axis_delta(current: i32, previous: i32) -> f64 {
    (i64::from(current) - i64::from(previous)) as f64
}

fn set_fd_nonblocking(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn add_scroll_delta(accumulator: &mut f64, delta: f64) {
    if !delta.is_finite() {
        return;
    }
    *accumulator = (*accumulator + delta).clamp(-MAX_SCROLL_DELTA, MAX_SCROLL_DELTA);
}

fn protocol_keycode(code: u32) -> Option<u32> {
    (code <= MAX_KEYCODE).then_some(code)
}

fn record_key_event(
    pressed_keys: &mut HashSet<u32>,
    pending: &mut Vec<Message>,
    code: u32,
    value: i32,
) {
    let Some(keycode) = protocol_keycode(code) else {
        return;
    };

    match value {
        0 => {
            pressed_keys.remove(&keycode);
            pending.push(Message::KeyEvent {
                keycode,
                pressed: false,
                modifiers: 0,
            });
        }
        1 => {
            pressed_keys.insert(keycode);
            pending.push(Message::KeyEvent {
                keycode,
                pressed: true,
                modifiers: 0,
            });
        }
        // Linux emits EV_KEY value 2 using the source machine's configured
        // repeat delay/rate. Forward it as another key-down; synthetic macOS
        // events do not start native key repeat from the initial key-down.
        2 if pressed_keys.contains(&keycode) => pending.push(Message::KeyEvent {
            keycode,
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
    fn evdev_key_repeats_preserve_order_and_held_state() {
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
    fn cursor_add_saturates() {
        assert_eq!(saturating_i32_add(i32::MAX, 1), i32::MAX);
        assert_eq!(saturating_i32_add(i32::MIN, -1), i32::MIN);
        assert_eq!(saturating_i32_add(10, -3), 7);
    }

    #[test]
    fn absolute_axis_range_does_not_overflow() {
        assert_eq!(abs_axis_range(0, 100), 100.0);
        assert_eq!(abs_axis_range(100, 0), 1.0);
        assert_eq!(abs_axis_range(i32::MIN, i32::MAX), 4_294_967_295.0);
    }

    #[test]
    fn absolute_axis_delta_does_not_overflow() {
        assert_eq!(abs_axis_delta(100, 40), 60.0);
        assert_eq!(abs_axis_delta(i32::MAX, i32::MIN), 4_294_967_295.0);
        assert_eq!(abs_axis_delta(i32::MIN, i32::MAX), -4_294_967_295.0);
    }

    #[test]
    fn set_fd_nonblocking_reports_invalid_fd() {
        assert!(set_fd_nonblocking(-1).is_err());
    }

    #[test]
    fn scroll_accumulator_clamps_to_protocol_limit() {
        let mut acc = MAX_SCROLL_DELTA - 1.0;
        add_scroll_delta(&mut acc, 10.0);
        assert_eq!(acc, MAX_SCROLL_DELTA);

        add_scroll_delta(&mut acc, f64::NAN);
        assert_eq!(acc, MAX_SCROLL_DELTA);

        add_scroll_delta(&mut acc, -MAX_SCROLL_DELTA * 3.0);
        assert_eq!(acc, -MAX_SCROLL_DELTA);
    }

    #[test]
    fn keycode_filter_matches_protocol_range() {
        assert_eq!(protocol_keycode(MAX_KEYCODE), Some(MAX_KEYCODE));
        assert_eq!(protocol_keycode(MAX_KEYCODE + 1), None);
    }

    #[test]
    fn pressed_key_state_ignores_unsupported_codes() {
        let mut keys = HashSet::new();
        update_protocol_key_state(&mut keys, MAX_KEYCODE, true);
        update_protocol_key_state(&mut keys, MAX_KEYCODE + 1, true);
        assert_eq!(keys, HashSet::from([MAX_KEYCODE]));
        update_protocol_key_state(&mut keys, MAX_KEYCODE + 1, false);
        assert_eq!(keys, HashSet::from([MAX_KEYCODE]));
        update_protocol_key_state(&mut keys, MAX_KEYCODE, false);
        assert!(keys.is_empty());
    }
}

fn has_keyboard_or_media_keys(device: &Device) -> bool {
    device.supported_keys().map_or(false, |keys| {
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
            set_fd_nonblocking(device.as_raw_fd())
                .wrap_err_with(|| format!("Failed to set {} non-blocking", path.display()))?;
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
            set_fd_nonblocking(device.as_raw_fd()).wrap_err_with(|| {
                format!("Failed to set keyboard {} non-blocking", path.display())
            })?;
            keyboard_devices.push(device);
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
            screen_width,
            screen_height,
            pressed_keys: HashSet::new(),
            pending_key_events: Vec::new(),
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
                                            self.cursor_x = saturating_i32_add(self.cursor_x, val);
                                        }
                                        RelativeAxisType::REL_Y => {
                                            self.cursor_y = saturating_i32_add(self.cursor_y, val);
                                        }
                                        RelativeAxisType::REL_WHEEL => {
                                            // Vertical scroll: positive = up
                                            add_scroll_delta(
                                                &mut self.scroll_acc_y,
                                                val as f64 * Self::WHEEL_PIXELS,
                                            );
                                        }
                                        RelativeAxisType::REL_HWHEEL => {
                                            // Horizontal scroll: positive = right
                                            add_scroll_delta(
                                                &mut self.scroll_acc_x,
                                                val as f64 * Self::WHEEL_PIXELS,
                                            );
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
                                                        let delta = abs_axis_delta(val, prev)
                                                            / abs_x_range
                                                            * sw
                                                            * Self::TOUCHPAD_SCROLL_SPEED;
                                                        add_scroll_delta(
                                                            &mut self.scroll_acc_x,
                                                            delta,
                                                        );
                                                    }
                                                    pdev.last_abs_x = Some(val);
                                                }
                                                AbsoluteAxisType::ABS_Y => {
                                                    if let Some(prev) = pdev.last_abs_y {
                                                        let delta = abs_axis_delta(val, prev)
                                                            / abs_y_range
                                                            * sh
                                                            * Self::TOUCHPAD_SCROLL_SPEED;
                                                        // Negate: finger moving down → content scrolls up (natural scrolling)
                                                        add_scroll_delta(
                                                            &mut self.scroll_acc_y,
                                                            -delta,
                                                        );
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
                                                        let delta = abs_axis_delta(val, prev)
                                                            / abs_x_range
                                                            * sw
                                                            * Self::TOUCHPAD_SPEED;
                                                        self.cursor_x = saturating_i32_add(
                                                            self.cursor_x,
                                                            delta as i32,
                                                        );
                                                    }
                                                    pdev.last_abs_x = Some(val);
                                                }
                                                AbsoluteAxisType::ABS_Y => {
                                                    if let Some(prev) = pdev.last_abs_y {
                                                        let delta = abs_axis_delta(val, prev)
                                                            / abs_y_range
                                                            * sh
                                                            * Self::TOUCHPAD_SPEED;
                                                        self.cursor_y = saturating_i32_add(
                                                            self.cursor_y,
                                                            delta as i32,
                                                        );
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
                                            // Keyboard key tracking. Keep only protocol-supported
                                            // keycodes in long-lived state so device-specific or
                                            // pointer-only evdev codes cannot accumulate forever.
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
                                record_key_event(
                                    &mut self.pressed_keys,
                                    &mut self.pending_key_events,
                                    code,
                                    event.value(),
                                );
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
        Ok(get_screen_size())
    }

    fn mouse_buttons(&self) -> Result<u8> {
        Ok(self.buttons)
    }

    fn set_grab(&mut self, grab: bool) -> Result<()> {
        if grab {
            for idx in 0..self.devices.len() {
                if let Err(e) = self.devices[idx].device.grab() {
                    for pdev in self.devices.iter_mut().take(idx) {
                        pdev.device.ungrab().ok();
                    }
                    return Err(eyre!("Failed to grab pointer device {}: {}", idx, e));
                }
            }
            for idx in 0..self.keyboard_devices.len() {
                if let Err(e) = self.keyboard_devices[idx].grab() {
                    for kdev in self.keyboard_devices.iter_mut().take(idx) {
                        kdev.ungrab().ok();
                    }
                    for pdev in &mut self.devices {
                        pdev.device.ungrab().ok();
                    }
                    return Err(eyre!("Failed to grab keyboard device {}: {}", idx, e));
                }
            }
        } else {
            let mut errors = Vec::new();
            for (idx, pdev) in self.devices.iter_mut().enumerate() {
                if let Err(e) = pdev.device.ungrab() {
                    errors.push(format!("pointer {idx}: {e}"));
                }
            }
            for (idx, kdev) in self.keyboard_devices.iter_mut().enumerate() {
                if let Err(e) = kdev.ungrab() {
                    errors.push(format!("keyboard {idx}: {e}"));
                }
            }
            if !errors.is_empty() {
                return Err(eyre!(
                    "Failed to ungrab input devices: {}",
                    errors.join(", ")
                ));
            }
        }
        self.grabbed = grab;
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
            for idx in 0..self.keyboard_devices.len() {
                if let Err(e) = self.keyboard_devices[idx].grab() {
                    for kdev in self.keyboard_devices.iter_mut().take(idx) {
                        kdev.ungrab().ok();
                    }
                    return Err(eyre!("Failed to grab keyboard device {}: {}", idx, e));
                }
            }
        } else {
            let mut errors = Vec::new();
            for (idx, kdev) in self.keyboard_devices.iter_mut().enumerate() {
                if let Err(e) = kdev.ungrab() {
                    errors.push(format!("keyboard {idx}: {e}"));
                }
            }
            if !errors.is_empty() {
                return Err(eyre!(
                    "Failed to ungrab keyboard devices: {}",
                    errors.join(", ")
                ));
            }
        }
        debug!(
            "Keyboard devices {} ({})",
            if grab { "grabbed" } else { "ungrabbed" },
            self.keyboard_devices.len()
        );
        Ok(())
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        // Preserve the kernel event order, including EV_KEY value 2 repeats.
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
}

/// Wayland-based input injector (stub — Linux server uses X11 for injection
/// since XWayland accepts XTest events even under Wayland).
pub struct WaylandInjector;

impl WaylandInjector {
    pub fn new() -> Result<Self> {
        Err(eyre!(
            "Wayland input injection is not implemented; run a true X11 session or set NEXDESK_LINUX_INJECTOR=x11 to force the experimental XTest/XWayland injector"
        ))
    }
}

impl InputInjector for WaylandInjector {
    fn inject(&mut self, _event: &Message) -> Result<()> {
        Err(eyre!("Wayland input injection is not implemented"))
    }

    fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
        Err(eyre!("Wayland input injection is not implemented"))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok(get_screen_size())
    }
}
