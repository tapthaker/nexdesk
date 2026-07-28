use color_eyre::eyre::Result;

use crate::net::protocol::Message;

/// Query the current primary screen dimensions without borrowing the input
/// capturer. Call this from a blocking worker, never from the input loop.
pub fn query_platform_screen_size() -> Result<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        crate::input::linux_wayland::query_screen_size()
    }

    #[cfg(target_os = "macos")]
    {
        crate::input::macos::query_screen_size()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform"))
    }
}

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

    /// Poll only keyboard devices when pointer events are already supplied by
    /// another backend such as Wayland layer-shell. Other backends may use the
    /// regular polling implementation.
    fn poll_key_events_only(&mut self) -> Result<Vec<Message>> {
        self.poll_key_events()
    }

    /// Poll whether a touchpad currently has at least two fingers in contact.
    /// Backends that cannot observe contact state return `None`.
    fn poll_scroll_contact(&mut self) -> Result<Option<bool>> {
        Ok(None)
    }

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

/// Factory boundary for creating an input capturer for one server connection.
pub trait InputCaptureFactory: Send + Sync {
    fn create(&self) -> Result<Box<dyn InputCapture>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformInputCaptureFactory;

impl InputCaptureFactory for PlatformInputCaptureFactory {
    fn create(&self) -> Result<Box<dyn InputCapture>> {
        create_platform_capturer()
    }
}

/// Create a platform-appropriate input capturer.
pub fn create_capturer() -> Result<Box<dyn InputCapture>> {
    PlatformInputCaptureFactory.create()
}

fn create_platform_capturer() -> Result<Box<dyn InputCapture>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct StubCapturer;

    impl InputCapture for StubCapturer {
        fn mouse_position(&self) -> Result<(i32, i32)> {
            Ok((100, 200))
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

    struct StubFactory;

    impl InputCaptureFactory for StubFactory {
        fn create(&self) -> Result<Box<dyn InputCapture>> {
            Ok(Box::new(StubCapturer))
        }
    }

    #[test]
    fn capture_factory_is_object_safe_and_creates_trait_objects() {
        let factory: &dyn InputCaptureFactory = &StubFactory;
        let capturer = factory.create().unwrap();
        assert_eq!(capturer.mouse_position().unwrap(), (100, 200));
        assert_eq!(capturer.screen_size().unwrap(), (1920, 1080));
    }
}
