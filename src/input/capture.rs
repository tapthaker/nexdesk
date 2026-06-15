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

    /// Grab or ungrab input devices. When grabbed, the local desktop
    /// does not receive the events (exclusive access for remote sharing).
    fn set_grab(&mut self, _grab: bool) -> Result<()> {
        Ok(()) // default no-op for platforms that don't need it
    }

    /// Grab or ungrab only keyboard input devices when the platform can
    /// separate keyboard capture from pointer capture.
    fn set_keyboard_grab(&mut self, grab: bool) -> Result<()> {
        self.set_grab(grab)
    }
}

/// Create a platform-appropriate input capturer.
pub fn create_capturer() -> Result<Box<dyn InputCapture>> {
    #[cfg(target_os = "linux")]
    {
        // On Wayland, XQueryPointer returns stale data, so use evdev.
        // WAYLAND_DISPLAY is set even when XWayland provides DISPLAY.
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            tracing::info!("Wayland session detected, using evdev capturer");
            return crate::input::linux_wayland::WaylandCapturer::new()
                .map(|c| Box::new(c) as Box<dyn InputCapture>);
        }
        if std::env::var("DISPLAY").is_ok() {
            return crate::input::linux_x11::X11Capturer::new()
                .map(|c| Box::new(c) as Box<dyn InputCapture>);
        }
        Err(color_eyre::eyre::eyre!(
            "No display server detected (set DISPLAY or WAYLAND_DISPLAY)"
        ))
    }

    #[cfg(target_os = "macos")]
    {
        crate::input::macos::MacOSCapturer::new().map(|c| Box::new(c) as Box<dyn InputCapture>)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform"))
    }
}
