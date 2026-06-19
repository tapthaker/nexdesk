use color_eyre::eyre::Result;

use crate::net::protocol::Message;

/// Trait for injecting input events into the local machine.
pub trait InputInjector: Send {
    /// Inject a single input event.
    fn inject(&mut self, event: &Message) -> Result<()>;

    /// Move the mouse to an absolute position.
    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()>;

    /// Get the screen dimensions.
    fn screen_size(&self) -> Result<(u32, u32)>;
}

/// Create a platform-appropriate input injector.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("DISPLAY").is_ok() {
            return crate::input::linux_x11::X11Injector::new()
                .map(|i| Box::new(i) as Box<dyn InputInjector>);
        }
        crate::input::linux_wayland::WaylandInjector::new()
            .map(|i| Box::new(i) as Box<dyn InputInjector>)
    }

    #[cfg(target_os = "macos")]
    {
        if std::env::var("NEXDESK_MACOS_INJECTOR").ok().as_deref() == Some("hid") {
            match crate::input::macos_hid::MacOSHidInjector::new() {
                Ok(i) => return Ok(Box::new(i) as Box<dyn InputInjector>),
                Err(e) => tracing::warn!("Experimental HID injector unavailable: {}; falling back to CGEvent", e),
            }
        }
        crate::input::macos::MacOSInjector::new().map(|i| Box::new(i) as Box<dyn InputInjector>)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform"))
    }
}
