#[cfg(target_os = "macos")]
use color_eyre::eyre::Result;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{CFRetained, CGPoint};

#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide, CGEvent, CGEventField,
    CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};

#[cfg(target_os = "macos")]
use std::os::raw::{c_int, c_uint};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use tracing::{debug, info, warn};

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
const CORE_GRAPHICS_DIAGNOSTIC_THRESHOLD: Duration = Duration::from_millis(4);

#[cfg(target_os = "macos")]
pub(crate) fn query_screen_size() -> Result<(u32, u32)> {
    Ok((
        CGDisplayPixelsWide(MAIN_DISPLAY) as u32,
        CGDisplayPixelsHigh(MAIN_DISPLAY) as u32,
    ))
}

#[cfg(target_os = "macos")]
fn log_core_graphics_timing(operation: &str, started: Instant) {
    let elapsed = started.elapsed();
    if elapsed >= CORE_GRAPHICS_DIAGNOSTIC_THRESHOLD {
        warn!(
            "CoreGraphics diagnostics: operation={} elapsed_us={}",
            operation,
            elapsed.as_micros()
        );
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CGPostMouseEvent(
        mouse_cursor_position: CGPoint,
        update_mouse_cursor_position: c_int,
        button_count: c_uint,
        mouse_button_down: c_int,
        ...
    ) -> i32;
    fn CGPostScrollWheelEvent(wheel_count: c_uint, wheel1: i32, ...) -> i32;
    fn CGPostKeyboardEvent(key_char: u16, virtual_key: u16, key_down: c_int) -> i32;
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostMode {
    CGEvent,
    LegacyQuartz,
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
        query_screen_size()
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
    /// CGEvent source state used for synthesized events.
    source_state: CGEventSourceStateID,
    /// CGEvent tap location used for synthesized events.
    tap_location: CGEventTapLocation,
    /// Posting API used for ordinary mouse/keyboard/scroll events.
    post_mode: PostMode,
}

#[cfg(target_os = "macos")]
impl MacOSInjector {
    pub fn new() -> Result<Self> {
        let screen_width = CGDisplayPixelsWide(MAIN_DISPLAY) as u32;
        let screen_height = CGDisplayPixelsHigh(MAIN_DISPLAY) as u32;
        let source_state = match std::env::var("NEXDESK_MACOS_EVENT_SOURCE")
            .unwrap_or_else(|_| "hid".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "combined" | "session" => CGEventSourceStateID::CombinedSessionState,
            "private" => CGEventSourceStateID::Private,
            _ => CGEventSourceStateID::HIDSystemState,
        };
        let tap_location = match std::env::var("NEXDESK_MACOS_EVENT_TAP")
            .unwrap_or_else(|_| "session".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "hid" => CGEventTapLocation::HIDEventTap,
            "annotated" => CGEventTapLocation::AnnotatedSessionEventTap,
            _ => CGEventTapLocation::SessionEventTap,
        };
        let post_mode = match std::env::var("NEXDESK_MACOS_POST_MODE")
            .unwrap_or_else(|_| "cgevent".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "legacy" | "quartz" | "cgpost" => PostMode::LegacyQuartz,
            _ => PostMode::CGEvent,
        };
        info!(
            "macOS injector: screen {}x{}, source_state={}, tap_location={}, post_mode={:?}",
            screen_width, screen_height, source_state.0, tap_location.0, post_mode
        );

        Ok(Self {
            screen_width,
            screen_height,
            modifier_flags: CGEventFlags::empty(),
            buttons_down: 0,
            last_click: None,
            click_count: 0,
            source_state,
            tap_location,
            post_mode,
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

    fn event_source(&self) -> Result<CFRetained<CGEventSource>> {
        let started = Instant::now();
        let source = CGEventSource::new(self.source_state)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        log_core_graphics_timing("CGEventSourceCreate", started);

        // Avoid CoreGraphics' default local-event suppression window. This is
        // documented for remote-operation events and is harmless if the delay is
        // unrelated, but gives us a cheap A/B point for idle wake sluggishness.
        let started = Instant::now();
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        log_core_graphics_timing("CGEventSourceSetLocalEventsSuppressionInterval", started);
        Ok(source)
    }

    fn post_event(&self, event: &CGEvent) {
        let started = Instant::now();
        CGEvent::post(self.tap_location, Some(event));
        log_core_graphics_timing("CGEventPost", started);
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

        let source = self.event_source()?;
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

        self.post_event(&event);
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
        let source = self.event_source()?;
        let started = Instant::now();
        let event = CGEvent::new_mouse_event(Some(&source), event_type, point, button)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
        log_core_graphics_timing("CGEventCreateMouseEvent", started);
        self.post_event(&event);
        Ok(())
    }

    fn current_position(&self) -> Result<(i32, i32)> {
        let source = self.event_source()?;
        let started = Instant::now();
        let event = CGEvent::new(Some(&source))
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;
        log_core_graphics_timing("CGEventCreate", started);
        let started = Instant::now();
        let loc = CGEvent::location(Some(&event));
        log_core_graphics_timing("CGEventGetLocation", started);
        Ok((loc.x as i32, loc.y as i32))
    }

    fn post_legacy_mouse_state(&self, x: f64, y: f64) {
        let left = if self.buttons_down & 1 != 0 { 1 } else { 0 };
        let right = if self.buttons_down & 2 != 0 { 1 } else { 0 };
        let middle = if self.buttons_down & 4 != 0 { 1 } else { 0 };
        let ret = unsafe { CGPostMouseEvent(CGPoint { x, y }, 1, 3, left, right, middle) };
        if ret != 0 {
            debug!("CGPostMouseEvent returned {}", ret);
        }
    }

    fn post_legacy_scroll(&self, dx: f64, dy: f64) {
        let wheel1 = -(dy as i32);
        let wheel2 = -(dx as i32);
        let ret = unsafe { CGPostScrollWheelEvent(2, wheel1, wheel2) };
        if ret != 0 {
            debug!("CGPostScrollWheelEvent returned {}", ret);
        }
    }

    fn post_legacy_key(&self, mac_keycode: u16, pressed: bool) {
        let ret = unsafe { CGPostKeyboardEvent(0, mac_keycode, if pressed { 1 } else { 0 }) };
        if ret != 0 {
            debug!("CGPostKeyboardEvent returned {}", ret);
        }
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
                if self.post_mode == PostMode::LegacyQuartz {
                    self.post_legacy_mouse_state(cx as f64, cy as f64);
                    return Ok(());
                }
                let point = CGPoint {
                    x: cx as f64,
                    y: cy as f64,
                };
                let source = self.event_source()?;
                let event =
                    CGEvent::new_mouse_event(Some(&source), event_type, point, cg_button)
                        .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::MouseEventClickState,
                    self.click_count,
                );
                self.post_event(&event);
            }
            Message::MouseScroll { dx, dy, phase } => {
                use crate::net::protocol::ScrollPhase;

                let source = self.event_source()?;

                if self.post_mode == PostMode::LegacyQuartz {
                    self.post_legacy_scroll(*dx, *dy);
                    return Ok(());
                }

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
                    self.post_event(&event);
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
                    self.post_event(&event);
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
                let source = self.event_source()?;

                if self.post_mode == PostMode::LegacyQuartz {
                    self.update_modifier_flags(*keycode, *pressed);
                    self.post_legacy_key(mac_keycode, *pressed);
                    return Ok(());
                }

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
                    self.post_event(&event);
                    return Ok(());
                }

                let event = CGEvent::new_keyboard_event(Some(&source), mac_keycode, *pressed)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::set_flags(Some(&event), self.modifier_flags);
                self.post_event(&event);
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let started = Instant::now();
        let sw = CGDisplayPixelsWide(MAIN_DISPLAY) as i32;
        let sh = CGDisplayPixelsHigh(MAIN_DISPLAY) as i32;
        log_core_graphics_timing("CGDisplayPixelDimensions", started);
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
        if self.post_mode == PostMode::LegacyQuartz {
            self.post_legacy_mouse_state(x, y);
            return Ok(());
        }
        if matches!(event_type, CGEventType::MouseMoved) {
            let started = Instant::now();
            let ret = unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
            log_core_graphics_timing("CGWarpMouseCursorPosition", started);
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
