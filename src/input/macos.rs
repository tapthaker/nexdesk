#[cfg(target_os = "macos")]
use color_eyre::eyre::Result;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;

#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGEvent, CGEventField, CGEventFlags, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGEventSource, CGScrollEventUnit,
};

#[cfg(target_os = "macos")]
use tracing::{debug, warn};

#[cfg(target_os = "macos")]
use crate::input::capture::InputCapture;
#[cfg(target_os = "macos")]
use crate::input::inject::InputInjector;
#[cfg(target_os = "macos")]
use crate::input::keymap;
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
        Ok((CGDisplayPixelsWide(MAIN_DISPLAY) as u32, CGDisplayPixelsHigh(MAIN_DISPLAY) as u32))
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

// Evdev keycodes for modifier keys
#[cfg(target_os = "macos")]
const EVDEV_KEY_LEFTCTRL: u32 = 29;
#[cfg(target_os = "macos")]
const EVDEV_KEY_LEFTSHIFT: u32 = 42;
#[cfg(target_os = "macos")]
const EVDEV_KEY_RIGHTSHIFT: u32 = 54;
#[cfg(target_os = "macos")]
const EVDEV_KEY_LEFTALT: u32 = 56;
#[cfg(target_os = "macos")]
const EVDEV_KEY_RIGHTCTRL: u32 = 97;
#[cfg(target_os = "macos")]
const EVDEV_KEY_RIGHTALT: u32 = 100;
#[cfg(target_os = "macos")]
const EVDEV_KEY_LEFTMETA: u32 = 125;
#[cfg(target_os = "macos")]
const EVDEV_KEY_RIGHTMETA: u32 = 126;

/// macOS input injector using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSInjector {
    screen_width: u32,
    screen_height: u32,
    /// Tracked modifier flags for synthesized key events
    modifier_flags: CGEventFlags,
    /// Bitmask of currently pressed mouse buttons (bit 0=left, 1=right, 2=middle)
    buttons_down: u8,
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
            modifier_flags: CGEventFlags::empty(),
            buttons_down: 0,
        })
    }

    /// Update tracked modifier flags based on an evdev keycode press/release.
    fn update_modifier_flags(&mut self, keycode: u32, pressed: bool) {
        let flag = match keycode {
            EVDEV_KEY_LEFTSHIFT | EVDEV_KEY_RIGHTSHIFT => CGEventFlags::MaskShift,
            EVDEV_KEY_LEFTCTRL | EVDEV_KEY_RIGHTCTRL => CGEventFlags::MaskControl,
            EVDEV_KEY_LEFTALT | EVDEV_KEY_RIGHTALT => CGEventFlags::MaskAlternate,
            EVDEV_KEY_LEFTMETA | EVDEV_KEY_RIGHTMETA => CGEventFlags::MaskCommand,
            _ => return,
        };
        if pressed {
            self.modifier_flags |= flag;
        } else {
            self.modifier_flags -= flag;
        }
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
                debug!("Injecting mouse move to ({}, {})", x, y);
                self.move_mouse(*x, *y)?;
            }
            Message::MouseButton { button, pressed } => {
                let (event_type, cg_button, bit) = match (button, pressed) {
                    (0, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0u8),
                    (0, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0u8),
                    (1, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1u8),
                    (1, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1u8),
                    (2, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 2u8),
                    (2, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 2u8),
                    _ => return Ok(()),
                };
                if *pressed {
                    self.buttons_down |= 1 << bit;
                } else {
                    self.buttons_down &= !(1 << bit);
                }
                let (cx, cy) = self.current_position()?;
                let point = CGPoint { x: cx as f64, y: cy as f64 };
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event = CGEvent::new_mouse_event(Some(&source), event_type, point, cg_button)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
                // Click count must be 1 for macOS to initiate drag-and-drop sessions.
                CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 1);
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            Message::MouseScroll { dx, dy } => {
                // Negate: scroll values arrive in traditional convention (positive=up)
                // but CGEvent synthetic events bypass macOS natural scrolling, so we
                // invert to match the default natural-scrolling direction.
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event = CGEvent::new_scroll_wheel_event2(
                    Some(&source),
                    CGScrollEventUnit::Pixel,
                    1, // wheel_count
                    -*dy,
                    -*dx,
                    0,
                )
                .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            Message::KeyEvent {
                keycode, pressed, ..
            } => {
                // Update modifier state BEFORE creating the event so that
                // modifier key-down events carry their own flag.
                self.update_modifier_flags(*keycode, *pressed);

                let mac_keycode = match keymap::evdev_to_macos(*keycode) {
                    Some(k) => k,
                    None => {
                        debug!("Unmapped evdev keycode: {}", keycode);
                        return Ok(());
                    }
                };
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event = CGEvent::new_keyboard_event(Some(&source), mac_keycode, *pressed)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::set_flags(Some(&event), self.modifier_flags);
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let sw = CGDisplayPixelsWide(MAIN_DISPLAY) as i32;
        let sh = CGDisplayPixelsHigh(MAIN_DISPLAY) as i32;
        let x = x.clamp(0, sw - 1) as f64;
        let y = y.clamp(0, sh - 1) as f64;
        // macOS requires drag event types when a button is held, otherwise
        // the move is not recognized as part of a drag operation.
        let (event_type, button) = if self.buttons_down & 1 != 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons_down & 2 != 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.buttons_down & 4 != 0 {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post_mouse_event(event_type, x, y, button)?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((CGDisplayPixelsWide(MAIN_DISPLAY) as u32, CGDisplayPixelsHigh(MAIN_DISPLAY) as u32))
    }
}
