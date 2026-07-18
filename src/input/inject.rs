use color_eyre::eyre::Result;

use crate::net::protocol::Message;

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxInjectorBackend {
    X11,
    Wayland,
}

#[cfg(any(target_os = "linux", test))]
fn choose_linux_injector(override_value: &str, has_display: bool) -> LinuxInjectorBackend {
    match override_value.trim().to_ascii_lowercase().as_str() {
        "x11" => LinuxInjectorBackend::X11,
        "wayland" => LinuxInjectorBackend::Wayland,
        // XTest through XWayland is currently the only implemented Linux
        // injection path. A Wayland session normally still exports DISPLAY.
        _ if has_display => LinuxInjectorBackend::X11,
        _ => LinuxInjectorBackend::Wayland,
    }
}

/// Trait for injecting input events into the local machine.
pub trait InputInjector: Send {
    /// Inject a single input event.
    fn inject(&mut self, event: &Message) -> Result<()>;

    /// Move the mouse to an absolute position.
    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()>;

    /// Get the currently stored screen dimensions.
    fn screen_size(&self) -> Result<(u32, u32)>;

    /// Refresh screen geometry and return the new dimensions. Backends whose
    /// geometry is inherently live may use the default implementation.
    fn refresh_screen_size(&mut self) -> Result<(u32, u32)> {
        self.screen_size()
    }

    /// Return and clear whether the last refresh observed a topology change
    /// that may not be visible in the aggregate width and height (for example,
    /// replacing one display with another at a different origin).
    fn take_screen_geometry_changed(&mut self) -> bool {
        false
    }

    /// Get the pointer position in the same normalized desktop coordinates as
    /// `move_mouse`. Backends that cannot query it may return `None`.
    fn cursor_position(&self) -> Result<Option<(i32, i32)>> {
        Ok(None)
    }

    /// Show or hide the local pointer while control is on another screen.
    fn set_cursor_visible(&mut self, _visible: bool) -> Result<()> {
        Ok(())
    }
}

/// Create a platform-appropriate input injector.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        let injector_override = std::env::var("NEXDESK_LINUX_INJECTOR").unwrap_or_default();
        match choose_linux_injector(&injector_override, std::env::var("DISPLAY").is_ok()) {
            LinuxInjectorBackend::X11 => crate::input::linux_x11::X11Injector::new()
                .map(|i| Box::new(i) as Box<dyn InputInjector>),
            LinuxInjectorBackend::Wayland => crate::input::linux_wayland::WaylandInjector::new()
                .map(|i| Box::new(i) as Box<dyn InputInjector>),
        }
    }

    #[cfg(target_os = "macos")]
    {
        if std::env::var("NEXDESK_MACOS_INJECTOR").ok().as_deref() == Some("hid") {
            match crate::input::macos_hid::MacOSHidInjector::new() {
                Ok(i) => return Ok(Box::new(i) as Box<dyn InputInjector>),
                Err(e) => tracing::warn!(
                    "Experimental HID injector unavailable: {}; falling back to CGEvent",
                    e
                ),
            }
        }
        crate::input::macos::MacOSInjector::new().map(|i| Box::new(i) as Box<dyn InputInjector>)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_wayland_session_prefers_implemented_xwayland_injector() {
        assert_eq!(choose_linux_injector("", true), LinuxInjectorBackend::X11);
    }

    #[test]
    fn linux_injector_override_is_respected() {
        assert_eq!(
            choose_linux_injector("wayland", true),
            LinuxInjectorBackend::Wayland
        );
        assert_eq!(
            choose_linux_injector("x11", false),
            LinuxInjectorBackend::X11
        );
    }
}
