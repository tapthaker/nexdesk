#[cfg(target_os = "macos")]
use color_eyre::eyre::Result;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;

#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGEvent, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGEventSource, CGScrollEventUnit,
};

#[cfg(target_os = "macos")]
use tracing::debug;

#[cfg(target_os = "macos")]
use crate::input::capture::InputCapture;
#[cfg(target_os = "macos")]
use crate::input::inject::InputInjector;
#[cfg(target_os = "macos")]
use crate::net::protocol::Message;

#[cfg(target_os = "macos")]
const MAIN_DISPLAY: CGDirectDisplayID = 0;

/// macOS input capturer using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSCapturer {
    screen_width: u32,
    screen_height: u32,
}

#[cfg(target_os = "macos")]
impl MacOSCapturer {
    pub fn new() -> Result<Self> {
        let screen_width = CGDisplayPixelsWide(MAIN_DISPLAY) as u32;
        let screen_height = CGDisplayPixelsHigh(MAIN_DISPLAY) as u32;
        debug!("macOS capturer: screen {}x{}", screen_width, screen_height);
        Ok(Self {
            screen_width,
            screen_height,
        })
    }
}

#[cfg(target_os = "macos")]
impl InputCapture for MacOSCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        let event = CGEvent::new(Some(&source))
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;
        let loc = CGEvent::location(Some(&event));
        Ok((loc.x as i32, loc.y as i32))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }

    fn mouse_buttons(&self) -> Result<u8> {
        // CGEventSource button state
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        let mut buttons: u8 = 0;
        if CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Left) {
            buttons |= 1;
        }
        if CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Right) {
            buttons |= 2;
        }
        if CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Center) {
            buttons |= 4;
        }
        let _ = source; // keep source alive
        Ok(buttons)
    }

    fn poll_key_events(&mut self) -> Result<Vec<Message>> {
        // macOS doesn't have a simple keymap query like X11.
        // Key events will be captured via CGEventTap in a future enhancement.
        // For now, keyboard forwarding works when macOS is the client (injector).
        Ok(Vec::new())
    }
}

/// macOS input injector using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSInjector {
    screen_width: u32,
    screen_height: u32,
}

#[cfg(target_os = "macos")]
impl MacOSInjector {
    pub fn new() -> Result<Self> {
        let screen_width = CGDisplayPixelsWide(MAIN_DISPLAY) as u32;
        let screen_height = CGDisplayPixelsHigh(MAIN_DISPLAY) as u32;
        debug!("macOS injector: screen {}x{}", screen_width, screen_height);
        Ok(Self {
            screen_width,
            screen_height,
        })
    }

    fn post_mouse_event(
        &self,
        event_type: CGEventType,
        x: f64,
        y: f64,
        button: CGMouseButton,
    ) -> Result<()> {
        let point = CGPoint { x, y };
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        let event = CGEvent::new_mouse_event(Some(&source), event_type, point, button)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn current_position(&self) -> Result<(i32, i32)> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        let event = CGEvent::new(Some(&source))
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;
        let loc = CGEvent::location(Some(&event));
        Ok((loc.x as i32, loc.y as i32))
    }
}

#[cfg(target_os = "macos")]
impl InputInjector for MacOSInjector {
    fn inject(&mut self, event: &Message) -> Result<()> {
        match event {
            Message::MouseMove { x, y } => {
                self.move_mouse(*x, *y)?;
            }
            Message::MouseButton { button, pressed } => {
                let (event_type, cg_button) = match (button, pressed) {
                    (0, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                    (0, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    (1, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                    (1, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                    (2, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                    (2, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                    _ => return Ok(()),
                };
                let (cx, cy) = self.current_position()?;
                self.post_mouse_event(event_type, cx as f64, cy as f64, cg_button)?;
            }
            Message::MouseScroll { dx, dy } => {
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event = CGEvent::new_scroll_wheel_event2(
                    Some(&source),
                    CGScrollEventUnit::Pixel,
                    1, // wheel_count
                    *dy,
                    *dx,
                    0,
                )
                .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            Message::KeyEvent {
                keycode, pressed, ..
            } => {
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event = CGEvent::new_keyboard_event(Some(&source), *keycode as u16, *pressed)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let x = x.clamp(0, self.screen_width as i32 - 1) as f64;
        let y = y.clamp(0, self.screen_height as i32 - 1) as f64;
        self.post_mouse_event(
            CGEventType::MouseMoved,
            x,
            y,
            CGMouseButton::Left,
        )?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.screen_width, self.screen_height))
    }
}
