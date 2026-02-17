use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr, eyre};
use evdev::{Device, InputEventKind, Key, RelativeAxisType};
use tracing::{debug, warn};
use x11rb::connection::Connection;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

/// Find pointer devices (devices that have REL_X and REL_Y).
fn find_pointer_devices() -> Result<Vec<PathBuf>> {
    let mut devices = Vec::new();
    let entries: Vec<_> = std::fs::read_dir("/dev/input")
        .wrap_err("Cannot read /dev/input")?
        .filter_map(|e| e.ok())
        .collect();

    for entry in entries {
        let path = entry.path();
        if !path.to_string_lossy().contains("event") {
            continue;
        }
        if let Ok(device) = Device::open(&path) {
            let supported = device.supported_relative_axes();
            if let Some(axes) = supported {
                if axes.contains(RelativeAxisType::REL_X) && axes.contains(RelativeAxisType::REL_Y) {
                    debug!("Found pointer device: {} ({})",
                           device.name().unwrap_or("unknown"), path.display());
                    devices.push(path);
                }
            }
        }
    }

    if devices.is_empty() {
        return Err(eyre!(
            "No pointer devices found. Make sure user is in the 'input' group: sudo usermod -aG input $USER"
        ));
    }

    Ok(devices)
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
/// Reads raw mouse events from /dev/input/ and accumulates position.
pub struct WaylandCapturer {
    devices: Vec<Device>,
    cursor_x: i32,
    cursor_y: i32,
    screen_width: u32,
    screen_height: u32,
    pressed_keys: HashSet<u32>,
    buttons: u8,
}

impl WaylandCapturer {
    pub fn new() -> Result<Self> {
        let paths = find_pointer_devices()?;
        let mut devices = Vec::new();
        for path in &paths {
            let device = Device::open(path)
                .wrap_err_with(|| format!("Failed to open {}", path.display()))?;
            // Set non-blocking via fcntl so poll doesn't block the async runtime
            let fd = device.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            devices.push(device);
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

    /// Process all pending events from all devices.
    fn drain_events(&mut self) {
        for device in &mut self.devices {
            loop {
                match device.fetch_events() {
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
                                InputEventKind::Key(key) => {
                                    let code = key.code() as u32;
                                    let pressed = event.value() != 0;

                                    // Track mouse buttons
                                    match key {
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
