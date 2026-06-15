use color_eyre::eyre::{Result, WrapErr};
use tracing::debug;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ButtonMask, ConnectionExt as _, Screen};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

// X11 event type constants for XTest
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

fn root_window(screen: &Screen) -> xproto::Window {
    screen.root
}

/// X11-based input capturer using XQueryPointer and XQueryKeymap.
pub struct X11Capturer {
    conn: RustConnection,
    root: xproto::Window,
    prev_keymap: [u8; 32],
}

impl X11Capturer {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).wrap_err("Failed to connect to X11 display")?;
        let screen = &conn.setup().roots[screen_num];
        let root = root_window(screen);

        debug!(
            "X11 capturer: screen {}x{}",
            screen.width_in_pixels, screen.height_in_pixels
        );

        Ok(Self {
            conn,
            root,
            prev_keymap: [0u8; 32],
        })
    }
}

impl InputCapture for X11Capturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        let reply = self
            .conn
            .query_pointer(self.root)?
            .reply()
            .wrap_err("Failed to query pointer")?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        let reply = self
            .conn
            .get_geometry(self.root)?
            .reply()
            .wrap_err("Failed to query screen geometry")?;
        Ok((reply.width as u32, reply.height as u32))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        let reply = self
            .conn
            .query_pointer(self.root)?
            .reply()
            .wrap_err("Failed to query pointer")?;
        let mask = reply.mask;
        let mut buttons: u8 = 0;
        if mask.contains(ButtonMask::M1) {
            buttons |= 1;
        } // left
        if mask.contains(ButtonMask::M3) {
            buttons |= 2;
        } // right (Button3 in X11)
        if mask.contains(ButtonMask::M2) {
            buttons |= 4;
        } // middle
        Ok(buttons)
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        let reply = self
            .conn
            .query_keymap()?
            .reply()
            .wrap_err("Failed to query keymap")?;
        let keymap = reply.keys;

        let mut events = Vec::new();
        for byte_idx in 0..32 {
            let old = self.prev_keymap[byte_idx];
            let new = keymap[byte_idx];
            if old != new {
                for bit in 0..8 {
                    let keycode = (byte_idx * 8 + bit) as u32;
                    let was_pressed = (old >> bit) & 1 != 0;
                    let is_pressed = (new >> bit) & 1 != 0;
                    if was_pressed != is_pressed {
                        events.push(Message::KeyEvent {
                            keycode,
                            pressed: is_pressed,
                            modifiers: 0, // TODO: track modifiers
                        });
                    }
                }
            }
        }
        self.prev_keymap = keymap;
        Ok(events)
    }
}

/// X11-based input injector using XTest extension.
pub struct X11Injector {
    conn: RustConnection,
    root: xproto::Window,
}

impl X11Injector {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).wrap_err("Failed to connect to X11 display")?;

        conn.xtest_get_version(2, 1)?
            .reply()
            .wrap_err("XTest extension not available")?;

        let screen = &conn.setup().roots[screen_num];
        let root = root_window(screen);

        debug!(
            "X11 injector: screen {}x{}, XTest available",
            screen.width_in_pixels, screen.height_in_pixels
        );

        Ok(Self { conn, root })
    }
}

impl InputInjector for X11Injector {
    fn inject(&mut self, event: &Message) -> Result<()> {
        match event {
            Message::MouseMove { x, y } => {
                self.move_mouse(*x, *y)?;
            }
            Message::MouseButton { button, pressed } => {
                let x_button = match button {
                    0 => 1u8,
                    1 => 3,
                    2 => 2,
                    n => *n + 1,
                };
                let event_type = if *pressed {
                    BUTTON_PRESS
                } else {
                    BUTTON_RELEASE
                };
                self.conn
                    .xtest_fake_input(event_type, x_button, 0, self.root, 0, 0, 0)?;
                self.conn.flush()?;
            }
            Message::MouseScroll { dx, dy, .. } => {
                let idy = *dy as i32;
                let idx = *dx as i32;
                if idy != 0 {
                    let button = if idy > 0 { 4u8 } else { 5u8 };
                    for _ in 0..idy.unsigned_abs() {
                        self.conn
                            .xtest_fake_input(BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                        self.conn.xtest_fake_input(
                            BUTTON_RELEASE,
                            button,
                            0,
                            self.root,
                            0,
                            0,
                            0,
                        )?;
                    }
                }
                if idx != 0 {
                    let button = if idx > 0 { 7u8 } else { 6u8 };
                    for _ in 0..idx.unsigned_abs() {
                        self.conn
                            .xtest_fake_input(BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                        self.conn.xtest_fake_input(
                            BUTTON_RELEASE,
                            button,
                            0,
                            self.root,
                            0,
                            0,
                            0,
                        )?;
                    }
                }
                self.conn.flush()?;
            }
            Message::KeyEvent {
                keycode, pressed, ..
            } => {
                let event_type = if *pressed { KEY_PRESS } else { KEY_RELEASE };
                self.conn
                    .xtest_fake_input(event_type, *keycode as u8, 0, self.root, 0, 0, 0)?;
                self.conn.flush()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let (sw, sh) = self.screen_size()?;
        let x = x.clamp(0, sw as i32 - 1) as i16;
        let y = y.clamp(0, sh as i32 - 1) as i16;
        self.conn
            .xtest_fake_input(MOTION_NOTIFY, 0, 0, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        let reply = self
            .conn
            .get_geometry(self.root)?
            .reply()
            .wrap_err("Failed to query screen geometry")?;
        Ok((reply.width as u32, reply.height as u32))
    }
}
