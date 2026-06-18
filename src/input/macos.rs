#[cfg(target_os = "macos")]
use color_eyre::eyre::Result;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;

#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide, CGEvent, CGEventField,
    CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
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

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
}

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
        Ok((
            CGDisplayPixelsWide(MAIN_DISPLAY) as u32,
            CGDisplayPixelsHigh(MAIN_DISPLAY) as u32,
        ))
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
        if CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Center)
        {
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

/// Map evdev keycode to macOS NX_KEYTYPE for media/special keys.
/// Returns None for non-media keys.
#[cfg(target_os = "macos")]
fn evdev_to_media_key(evdev: u32) -> Option<i32> {
    match evdev {
        113 => Some(7),  // KEY_MUTE         -> NX_KEYTYPE_MUTE
        114 => Some(1),  // KEY_VOLUMEDOWN   -> NX_KEYTYPE_SOUND_DOWN
        115 => Some(0),  // KEY_VOLUMEUP     -> NX_KEYTYPE_SOUND_UP
        163 => Some(17), // KEY_NEXTSONG     -> NX_KEYTYPE_NEXT
        164 => Some(16), // KEY_PLAYPAUSE    -> NX_KEYTYPE_PLAY
        165 => Some(18), // KEY_PREVIOUSSONG -> NX_KEYTYPE_PREVIOUS
        224 => Some(22), // KEY_BRIGHTNESSDOWN -> NX_KEYTYPE_BRIGHTNESS_DOWN
        225 => Some(21), // KEY_BRIGHTNESSUP   -> NX_KEYTYPE_BRIGHTNESS_UP
        _ => None,
    }
}

/// macOS input injector using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSInjector {
    screen_width: u32,
    screen_height: u32,
    /// Tracked modifier flags for synthesized key events
    modifier_flags: CGEventFlags,
    /// Bitmask of currently pressed mouse buttons (bit 0=left, 1=right, 2=middle)
    buttons_down: u8,
    /// Timestamp and button of last mouse-down, for multi-click detection
    last_click: Option<(std::time::Instant, u8)>,
    /// Current click count (1=single, 2=double, 3=triple)
    click_count: i64,
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
            last_click: None,
            click_count: 0,
        })
    }

    fn modifier_flag(keycode: u32) -> Option<CGEventFlags> {
        match keycode {
            EVDEV_KEY_LEFTSHIFT | EVDEV_KEY_RIGHTSHIFT => Some(CGEventFlags::MaskShift),
            EVDEV_KEY_LEFTCTRL | EVDEV_KEY_RIGHTCTRL => Some(CGEventFlags::MaskControl),
            EVDEV_KEY_LEFTALT | EVDEV_KEY_RIGHTALT => Some(CGEventFlags::MaskAlternate),
            EVDEV_KEY_LEFTMETA | EVDEV_KEY_RIGHTMETA => Some(CGEventFlags::MaskCommand),
            _ => None,
        }
    }

    /// Update tracked modifier flags based on an evdev keycode press/release.
    fn update_modifier_flags(&mut self, keycode: u32, pressed: bool) {
        let Some(flag) = Self::modifier_flag(keycode) else {
            return;
        };
        if pressed {
            self.modifier_flags |= flag;
        } else {
            self.modifier_flags -= flag;
        }
    }

    /// Post an NSSystemDefined media key event (volume, brightness, play, etc.)
    fn post_media_key_event(&self, nx_keytype: i32, pressed: bool) -> color_eyre::eyre::Result<()> {
        const NX_SYSDEFINED: u32 = 14;
        const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i64 = 8;
        const NX_KEYDOWN: i64 = 0x0A;
        const NX_KEYUP: i64 = 0x0B;
        // These are the private-but-stable CGEvent fields used by
        // NSSystemDefined / NX_SUBTYPE_AUX_CONTROL_BUTTONS events.
        const CG_EVENT_SUBTYPE_FIELD: CGEventField = CGEventField(133);
        const CG_EVENT_DATA1_FIELD: CGEventField = CGEventField(134);
        const CG_EVENT_DATA2_FIELD: CGEventField = CGEventField(135);

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        let event = CGEvent::new(Some(&source))
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;

        CGEvent::set_type(Some(&event), CGEventType(NX_SYSDEFINED));

        // data1 layout for aux-control-button events:
        //   high word: NX_KEYTYPE_*
        //   next byte: NX_KEYDOWN / NX_KEYUP
        let key_state = if pressed { NX_KEYDOWN } else { NX_KEYUP };
        let data1 = ((nx_keytype as i64) << 16) | (key_state << 8);

        CGEvent::set_integer_value_field(
            Some(&event),
            CG_EVENT_SUBTYPE_FIELD,
            NX_SUBTYPE_AUX_CONTROL_BUTTONS,
        );
        CGEvent::set_integer_value_field(Some(&event), CG_EVENT_DATA1_FIELD, data1);
        CGEvent::set_integer_value_field(Some(&event), CG_EVENT_DATA2_FIELD, -1);

        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
        Ok(())
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
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
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
                    // Track multi-click: if same button pressed within 500ms, increment count
                    let now = std::time::Instant::now();
                    if let Some((last_time, last_btn)) = self.last_click {
                        if last_btn == *button && now.duration_since(last_time).as_millis() < 500 {
                            self.click_count += 1;
                        } else {
                            self.click_count = 1;
                        }
                    } else {
                        self.click_count = 1;
                    }
                    self.last_click = Some((now, *button));
                } else {
                    self.buttons_down &= !(1 << bit);
                }
                let (cx, cy) = self.current_position()?;
                let point = CGPoint {
                    x: cx as f64,
                    y: cy as f64,
                };
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
                let event =
                    CGEvent::new_mouse_event(Some(&source), event_type, point, cg_button)
                        .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::MouseEventClickState,
                    self.click_count,
                );
                CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
            }
            Message::MouseScroll { dx, dy, phase } => {
                use crate::net::protocol::ScrollPhase;

                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;

                // Vertical scroll: pixel-based events without the continuous
                // flag. This works in all apps including Firefox.
                if *dy != 0.0 {
                    let event = CGEvent::new_scroll_wheel_event2(
                        Some(&source),
                        CGScrollEventUnit::Pixel,
                        1,
                        -*dy as i32,
                        0,
                        0,
                    )
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
                }

                // Horizontal scroll: continuous trackpad events with phases.
                // This is what triggers swipe-to-navigate in browsers/Finder.
                if *dx != 0.0 || (*phase == ScrollPhase::Ended && *dy == 0.0) {
                    let event = CGEvent::new_scroll_wheel_event2(
                        Some(&source),
                        CGScrollEventUnit::Pixel,
                        2,
                        0,
                        -*dx as i32,
                        0,
                    )
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::ScrollWheelEventIsContinuous,
                        1,
                    );
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::ScrollWheelEventPointDeltaAxis2,
                        -*dx as i64,
                    );
                    let cg_phase: i64 = match phase {
                        ScrollPhase::Began => 1,
                        ScrollPhase::Changed => 2,
                        ScrollPhase::Ended => 4,
                        ScrollPhase::None => 0,
                    };
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::ScrollWheelEventScrollPhase,
                        cg_phase,
                    );
                    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
                }
            }
            Message::KeyEvent {
                keycode, pressed, ..
            } => {
                // Media keys use NSSystemDefined events, not keyboard events.
                if let Some(nx_keytype) = evdev_to_media_key(*keycode) {
                    self.post_media_key_event(nx_keytype, *pressed)?;
                    return Ok(());
                }

                let mac_keycode = match keymap::evdev_to_macos(*keycode) {
                    Some(k) => k,
                    None => {
                        debug!("Unmapped evdev keycode: {}", keycode);
                        return Ok(());
                    }
                };
                let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;

                // macOS represents modifier transitions as FlagsChanged events.
                // Posting plain KeyDown/KeyUp for Shift/Ctrl/Option/Command can
                // leave the global modifier state stuck if a switch-back or
                // disconnect races with the key release.
                if Self::modifier_flag(*keycode).is_some() {
                    self.update_modifier_flags(*keycode, *pressed);
                    let event =
                        CGEvent::new_keyboard_event(Some(&source), mac_keycode, *pressed)
                            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                    // kCGEventFlagsChanged = 12
                    CGEvent::set_type(Some(&event), CGEventType(12));
                    CGEvent::set_flags(Some(&event), self.modifier_flags);
                    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
                    return Ok(());
                }

                let event = CGEvent::new_keyboard_event(Some(&source), mac_keycode, *pressed)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::set_flags(Some(&event), self.modifier_flags);
                CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
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
        if matches!(event_type, CGEventType::MouseMoved) {
            let ret = unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
            if ret != 0 {
                debug!("CGWarpMouseCursorPosition returned {}", ret);
            }
        }
        self.post_mouse_event(event_type, x, y, button)?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((
            CGDisplayPixelsWide(MAIN_DISPLAY) as u32,
            CGDisplayPixelsHigh(MAIN_DISPLAY) as u32,
        ))
    }
}
