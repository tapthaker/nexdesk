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

#[derive(Debug)]
enum PointerKind {
    /// Mouse — uses REL_X/REL_Y relative deltas
    Relative,
    /// Touchpad/trackpad — uses ABS_X/ABS_Y absolute coordinates
    Absolute {
        abs_x_min: i32,
        abs_x_max: i32,
        abs_y_min: i32,
        abs_y_max: i32,
    },
}

struct PointerDevice {
    device: Device,
    kind: PointerKind,
}

/// Find pointer devices: mice (REL_X/REL_Y) and touchpads (ABS_X/ABS_Y + BTN_TOUCH).
fn find_pointer_devices() -> Result<Vec<(PathBuf, PointerKind)>> {
    let mut found = Vec::new();
    let entries: Vec<_> = std::fs::read_dir("/dev/input")
        .wrap_err("Cannot read /dev/input")?
        .filter_map(|e| e.ok())
        .collect();

    for entry in entries {
        let path = entry.path();
        if !path.to_string_lossy().contains("event") {
            continue;
        }
        let device = match Device::open(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let name = device.name().unwrap_or("unknown");

        // Check for relative-axis mouse
        if let Some(axes) = device.supported_relative_axes() {
            if axes.contains(RelativeAxisType::REL_X) && axes.contains(RelativeAxisType::REL_Y) {
                debug!("Found relative pointer: {} ({})", name, path.display());
                found.push((path, PointerKind::Relative));
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
                        debug!("Found absolute pointer: {} ({}) x:[{}..{}] y:[{}..{}]",
                               name, path.display(), ax.minimum, ax.maximum, ay.minimum, ay.maximum);
                        found.push((path, PointerKind::Absolute {
                            abs_x_min: ax.minimum,
                            abs_x_max: ax.maximum,
                            abs_y_min: ay.minimum,
                            abs_y_max: ay.maximum,
                        }));
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

    Ok(found)
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
/// Reads raw mouse/touchpad events from /dev/input/ and tracks position.
pub struct WaylandCapturer {
    devices: Vec<PointerDevice>,
    cursor_x: i32,
    cursor_y: i32,
    screen_width: u32,
    screen_height: u32,
    pressed_keys: HashSet<u32>,
    buttons: u8,
}

impl WaylandCapturer {
    pub fn new() -> Result<Self> {
        let found = find_pointer_devices()?;
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
                kind: match kind {
                    PointerKind::Relative => PointerKind::Relative,
                    PointerKind::Absolute { abs_x_min, abs_x_max, abs_y_min, abs_y_max } =>
                        PointerKind::Absolute {
                            abs_x_min: *abs_x_min,
                            abs_x_max: *abs_x_max,
                            abs_y_min: *abs_y_min,
                            abs_y_max: *abs_y_max,
                        },
                },
            });
        }

        let (screen_width, screen_height) = get_screen_size();
        debug!("Wayland evdev capturer: screen {}x{}, {} device(s)",
               screen_width, screen_height, devices.len());

        // Start cursor at center of screen
        let cursor_x = screen_width as i32 / 2;
        let cursor_y = screen_height as i32 / 2;

        Ok(Self {
            devices,
            cursor_x,
            cursor_y,
            screen_width,
            screen_height,
            pressed_keys: HashSet::new(),
            buttons: 0,
        })
    }

    /// Map an absolute axis value to screen coordinates.
    fn map_abs(val: i32, abs_min: i32, abs_max: i32, screen_max: u32) -> i32 {
        let range = (abs_max - abs_min).max(1) as f64;
        let normalized = (val - abs_min) as f64 / range;
        (normalized * screen_max as f64) as i32
    }

    /// Process all pending events from all devices.
    fn drain_events(&mut self) {
        let sw = self.screen_width;
        let sh = self.screen_height;

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
                                        _ => {}
                                    }
                                }
                                InputEventKind::AbsAxis(axis) => {
                                    if let PointerKind::Absolute { abs_x_min, abs_x_max, abs_y_min, abs_y_max } = &pdev.kind {
                                        match axis {
                                            AbsoluteAxisType::ABS_X => {
                                                self.cursor_x = Self::map_abs(event.value(), *abs_x_min, *abs_x_max, sw);
                                            }
                                            AbsoluteAxisType::ABS_Y => {
                                                self.cursor_y = Self::map_abs(event.value(), *abs_y_min, *abs_y_max, sh);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                InputEventKind::Key(key) => {
                                    let code = key.code() as u32;
                                    let pressed = event.value() != 0;

                                    // Track mouse buttons (including touchpad taps)
                                    match key {
                                        Key::BTN_LEFT | Key::BTN_TOUCH => {
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

        // Clamp cursor to screen bounds
        self.cursor_x = self.cursor_x.clamp(0, self.screen_width as i32 - 1);
        self.cursor_y = self.cursor_y.clamp(0, self.screen_height as i32 - 1);
    }
}

impl InputCapture for WaylandCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        // We use interior mutability pattern through the Mutex in quic.rs
        // drain_events is called in poll_key_events which is called each poll cycle
        Ok((self.cursor_x, self.cursor_y))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        Ok(self.buttons)
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        // Drain all pending events first — this updates cursor position, buttons, and keys
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
