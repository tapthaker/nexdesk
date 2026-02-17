use color_eyre::eyre::Result;

use crate::net::protocol::Message;

/// Trait for capturing input events from the local machine.
pub trait InputCapture: Send {
    /// Get the current mouse position.
    fn mouse_position(&self) -> Result<(i32, i32)>;

    /// Get the screen dimensions.
    fn screen_size(&self) -> Result<(u32, u32)>;

    /// Get the current mouse button state bitmask.
    /// Bit 0 = left, bit 1 = right, bit 2 = middle.
    fn mouse_buttons(&self) -> Result<u8>;

    /// Poll for keyboard state changes. Returns key events since last call.
    fn poll_key_events(&mut self) -> Result<Vec<Message>>;
}

/// Create a platform-appropriate input capturer.
pub fn create_capturer() -> Result<Box<dyn InputCapture>> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("DISPLAY").is_ok() {
            return crate::input::linux_x11::X11Capturer::new()
                .map(|c| Box::new(c) as Box<dyn InputCapture>);
        }
        crate::input::linux_wayland::WaylandCapturer::new()
            .map(|c| Box::new(c) as Box<dyn InputCapture>)
    }

    #[cfg(target_os = "macos")]
    {
        crate::input::macos::MacOSCapturer::new()
            .map(|c| Box::new(c) as Box<dyn InputCapture>)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform"))
    }
}
