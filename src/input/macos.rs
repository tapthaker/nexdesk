use color_eyre::eyre::Result;

use crate::input::capture::InputCapture;
use crate::input::inject::InputInjector;
use crate::net::protocol::Message;

/// macOS input capturer using CoreGraphics CGEventTap.
pub struct MacOSCapturer {
    // TODO Phase 3: CGEventTap
}

impl MacOSCapturer {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl InputCapture for MacOSCapturer {
    fn start(&mut self, _callback: Box<dyn Fn(Message) + Send>) -> Result<()> {
        tracing::warn!("macOS input capture not yet implemented");
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

/// macOS input injector using CGEventPost.
pub struct MacOSInjector {
    // TODO Phase 3: CGEventPost
}

impl MacOSInjector {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl InputInjector for MacOSInjector {
    fn inject(&mut self, _event: &Message) -> Result<()> {
        tracing::warn!("macOS input injection not yet implemented");
        Ok(())
    }

    fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
}
