use color_eyre::eyre::Result;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

/// Wayland-based input capturer using libei (stub).
pub struct WaylandCapturer;

impl WaylandCapturer {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl InputCapture for WaylandCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        Ok((0, 0))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        Ok(0)
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        Ok(Vec::new())
    }
}

/// Wayland-based input injector (stub).
pub struct WaylandInjector;

impl WaylandInjector {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

impl InputInjector for WaylandInjector {
    fn inject(&mut self, _event: &Message) -> Result<()> {
        tracing::warn!("Wayland input injection not yet implemented");
        Ok(())
    }

    fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
}
