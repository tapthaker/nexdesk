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

/// Factory boundary for creating an input injector for one client session.
pub trait InputInjectorFactory: Send + Sync {
    fn create(&self) -> Result<Box<dyn InputInjector>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformInputInjectorFactory;

impl InputInjectorFactory for PlatformInputInjectorFactory {
    fn create(&self) -> Result<Box<dyn InputInjector>> {
        create_platform_injector()
    }
}

/// Create a platform-appropriate input injector.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    PlatformInputInjectorFactory.create()
}

fn create_platform_injector() -> Result<Box<dyn InputInjector>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct StubInjector;

    impl InputInjector for StubInjector {
        fn inject(&mut self, _event: &Message) -> Result<()> {
            Ok(())
        }

        fn move_mouse(&mut self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }

        fn screen_size(&self) -> Result<(u32, u32)> {
            Ok((1920, 1080))
        }
    }

    struct StubFactory;

    impl InputInjectorFactory for StubFactory {
        fn create(&self) -> Result<Box<dyn InputInjector>> {
            Ok(Box::new(StubInjector))
        }
    }

    #[test]
    fn injector_factory_is_object_safe_and_creates_trait_objects() {
        let factory: &dyn InputInjectorFactory = &StubFactory;
        let injector = factory.create().unwrap();
        assert_eq!(injector.screen_size().unwrap(), (1920, 1080));
    }
}
