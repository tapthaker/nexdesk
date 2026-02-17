use color_eyre::eyre::{Result, WrapErr};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Screen};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use tracing::debug;

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

/// X11-based input capturer using XQueryPointer polling.
pub struct X11Capturer {
    conn: RustConnection,
    root: xproto::Window,
    screen_width: u32,
    screen_height: u32,
}

impl X11Capturer {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)
            .wrap_err("Failed to connect to X11 display")?;
        let screen = &conn.setup().roots[screen_num];
        let root = root_window(screen);
        let screen_width = screen.width_in_pixels as u32;
        let screen_height = screen.height_in_pixels as u32;

        debug!("X11 capturer: screen {}x{}", screen_width, screen_height);

        Ok(Self {
            conn,
            root,
            screen_width,
            screen_height,
        })
    }
}

impl InputCapture for X11Capturer {
    fn start(&mut self, _callback: Box<dyn Fn(Message) + Send>) -> Result<()> {
        // Polling-based capture is driven externally by the serve loop
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn mouse_position(&self) -> Result<(i32, i32)> {
        let reply = self.conn.query_pointer(self.root)?.reply()
            .wrap_err("Failed to query pointer")?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }
}

/// X11-based input injector using XTest extension.
pub struct X11Injector {
    conn: RustConnection,
    root: xproto::Window,
    screen_width: u32,
    screen_height: u32,
}

impl X11Injector {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)
            .wrap_err("Failed to connect to X11 display")?;

        // Verify XTest extension is available
        conn.xtest_get_version(2, 1)?.reply()
            .wrap_err("XTest extension not available")?;

        let screen = &conn.setup().roots[screen_num];
        let root = root_window(screen);
        let screen_width = screen.width_in_pixels as u32;
        let screen_height = screen.height_in_pixels as u32;

        debug!("X11 injector: screen {}x{}, XTest available", screen_width, screen_height);

        Ok(Self {
            conn,
            root,
            screen_width,
            screen_height,
        })
    }
}

impl InputInjector for X11Injector {
    fn inject(&mut self, event: &Message) -> Result<()> {
        match event {
            Message::MouseMove { x, y } => {
                self.move_mouse(*x, *y)?;
            }
            Message::MouseButton { button, pressed } => {
                // X11 buttons: 1=left, 2=middle, 3=right
                let x_button = match button {
                    0 => 1u8, // left
                    1 => 3,   // right
                    2 => 2,   // middle
                    n => *n + 1,
                };
                let event_type = if *pressed { BUTTON_PRESS } else { BUTTON_RELEASE };
                self.conn.xtest_fake_input(event_type, x_button, 0, self.root, 0, 0, 0)?;
                self.conn.flush()?;
            }
            Message::MouseScroll { dx, dy } => {
                // X11 scroll: button 4=up, 5=down, 6=left, 7=right
                if *dy != 0 {
                    let button = if *dy > 0 { 4u8 } else { 5u8 };
                    for _ in 0..dy.unsigned_abs() {
                        self.conn.xtest_fake_input(BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                        self.conn.xtest_fake_input(BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
                    }
                }
                if *dx != 0 {
                    let button = if *dx > 0 { 7u8 } else { 6u8 };
                    for _ in 0..dx.unsigned_abs() {
                        self.conn.xtest_fake_input(BUTTON_PRESS, button, 0, self.root, 0, 0, 0)?;
                        self.conn.xtest_fake_input(BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
                    }
                }
                self.conn.flush()?;
            }
            Message::KeyEvent { keycode, pressed, .. } => {
                let event_type = if *pressed { KEY_PRESS } else { KEY_RELEASE };
                self.conn.xtest_fake_input(event_type, *keycode as u8, 0, self.root, 0, 0, 0)?;
                self.conn.flush()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        // Clamp to screen bounds
        let x = x.clamp(0, self.screen_width as i32 - 1) as i16;
        let y = y.clamp(0, self.screen_height as i32 - 1) as i16;

        self.conn.xtest_fake_input(MOTION_NOTIFY, 0, 0, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }
}
