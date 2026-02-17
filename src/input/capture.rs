use color_eyre::eyre::Result;

use crate::net::protocol::Message;

/// Trait for capturing input events from the local machine.
pub trait InputCapture: Send {
    /// Start capturing input. Calls the callback for each event.
    fn start(&mut self, callback: Box<dyn Fn(Message) + Send>) -> Result<()>;

    /// Stop capturing input.
    fn stop(&mut self) -> Result<()>;

    /// Get the current mouse position.
    fn mouse_position(&self) -> Result<(i32, i32)>;

    /// Get the screen dimensions.
    fn screen_size(&self) -> Result<(u32, u32)>;
}

/// Create a platform-appropriate input capturer.
pub fn create_capturer() -> Result<Box<dyn InputCapture>> {
    #[cfg(target_os = "linux")]
    {
        // Try X11 first, fall back to Wayland
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
