use color_eyre::eyre::Result;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

/// X11-based input capturer using XRecord/XTest.
pub struct X11Capturer {
    // TODO Phase 3: x11rb connection
}

impl X11Capturer {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl InputCapture for X11Capturer {
    fn start(&mut self, _callback: Box<dyn Fn(Message) + Send>) -> Result<()> {
        // TODO Phase 3: XRecord event capture
        tracing::warn!("X11 input capture not yet implemented");
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn mouse_position(&self) -> Result<(i32, i32)> {
        Ok((0, 0))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
}

/// X11-based input injector using XTest.
pub struct X11Injector {
    // TODO Phase 3: x11rb connection
}

impl X11Injector {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl InputInjector for X11Injector {
    fn inject(&mut self, _event: &Message) -> Result<()> {
        // TODO Phase 3: XTest event injection
        tracing::warn!("X11 input injection not yet implemented");
        Ok(())
    }

    fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
}
