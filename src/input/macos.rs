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
use std::cell::Cell;
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

#[cfg(target_os = "macos")]
const FIXED_POINT_SCALE: f64 = 65_536.0;

#[cfg(target_os = "macos")]
fn scroll_delta_fields(delta: f64) -> (i32, i64) {
    let delta = -delta;
    (
        delta.round() as i32,
        (delta * FIXED_POINT_SCALE).round() as i64,
    )
}

/// macOS input capturer using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSCapturer;

#[cfg(target_os = "macos")]
impl MacOSCapturer {
    pub fn new() -> Result<Self> {
        let (screen_width, screen_height) = query_screen_size()?;
        debug!("macOS capturer: screen {}x{}", screen_width, screen_height);
        Ok(Self)
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

#[cfg(target_os = "macos")]
struct SendableEventSource(CFRetained<CGEventSource>);

// CGEventSource is a Core Foundation value with no thread affinity. The
// injector owns it exclusively, and InputInjector's mutating operations
// serialize access. It may therefore move with the injector but is not Sync.
#[cfg(target_os = "macos")]
unsafe impl Send for SendableEventSource {}

#[cfg(target_os = "macos")]
struct ScreenSizeCache {
    width: Cell<u32>,
    height: Cell<u32>,
}

#[cfg(target_os = "macos")]
impl ScreenSizeCache {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width: Cell::new(width),
            height: Cell::new(height),
        }
    }

    fn get(&self) -> (u32, u32) {
        (self.width.get(), self.height.get())
    }

    fn update(&self, width: u32, height: u32) {
        self.width.set(width);
        self.height.set(height);
    }

    fn refresh(&self) -> Result<(u32, u32)> {
        let (width, height) = query_screen_size()?;
        self.update(width, height);
        Ok((width, height))
    }
}

/// macOS input injector using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSInjector {
    screen_size: ScreenSizeCache,
    /// Tracked modifier flags for synthesized key events
    modifier_flags: CGEventFlags,
    /// Bitmask of currently pressed mouse buttons (bit 0=left, 1=right, 2=middle)
    buttons_down: u8,
    /// Timestamp and button of last mouse-down, for multi-click detection
    last_click: Option<(std::time::Instant, u8)>,
    /// Current click count (1=single, 2=double, 3=triple)
    click_count: i64,
    /// Reused source for synthesized events. Creating a source may synchronously
    /// contact WindowServer, so it must stay off the per-event input path.
    event_source: SendableEventSource,
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
        let event_source = CGEventSource::new(source_state)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        CGEventSource::set_local_events_suppression_interval(Some(&event_source), 0.0);
        info!(
            "macOS injector: screen {}x{}, source_state={}, tap_location={}, post_mode={:?}",
            screen_width, screen_height, source_state.0, tap_location.0, post_mode
        );

        Ok(Self {
            screen_size: ScreenSizeCache::new(screen_width, screen_height),
            modifier_flags: CGEventFlags::empty(),
            buttons_down: 0,
            last_click: None,
            click_count: 0,
            event_source: SendableEventSource(event_source),
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

        let event = CGEvent::new(Some(&self.event_source.0))
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
        let started = Instant::now();
        let event = CGEvent::new_mouse_event(Some(&self.event_source.0), event_type, point, button)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
        log_core_graphics_timing("CGEventCreateMouseEvent", started);
        self.post_event(&event);
        Ok(())
    }

    fn current_position(&self) -> Result<(i32, i32)> {
        let started = Instant::now();
        let event = CGEvent::new(Some(&self.event_source.0))
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
                let event = CGEvent::new_mouse_event(
                    Some(&self.event_source.0),
                    event_type,
                    point,
                    cg_button,
                )
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

                if self.post_mode == PostMode::LegacyQuartz {
                    self.post_legacy_scroll(*dx, *dy);
                    return Ok(());
                }

                let (pixel_x, fixed_x) = scroll_delta_fields(*dx);
                let (pixel_y, fixed_y) = scroll_delta_fields(*dy);
                let continuous = *phase != ScrollPhase::None;
                let cg_phase: i64 = match phase {
                    ScrollPhase::Began => 1,
                    ScrollPhase::Changed => 2,
                    ScrollPhase::Ended => 4,
                    ScrollPhase::None => 0,
                };

                // Replay a trackpad gesture as one two-axis event. Keeping both
                // axes continuous and carrying Began/Changed/Ended lets macOS
                // and applications derive velocity from the incoming cadence.
                // The 16.16 fields retain sub-pixel deltas that would otherwise
                // be truncated and make slow gestures feel stepped.
                let wheel_count = if *dx != 0.0 || continuous { 2 } else { 1 };
                let event = CGEvent::new_scroll_wheel_event2(
                    Some(&self.event_source.0),
                    CGScrollEventUnit::Pixel,
                    wheel_count,
                    pixel_y,
                    pixel_x,
                    0,
                )
                .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::ScrollWheelEventPointDeltaAxis1,
                    i64::from(pixel_y),
                );
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::ScrollWheelEventPointDeltaAxis2,
                    i64::from(pixel_x),
                );
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
                    fixed_y,
                );
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
                    fixed_x,
                );
                if continuous {
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::ScrollWheelEventIsContinuous,
                        1,
                    );
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::ScrollWheelEventScrollPhase,
                        cg_phase,
                    );
                }
                self.post_event(&event);
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
                    let event = CGEvent::new_keyboard_event(
                        Some(&self.event_source.0),
                        mac_keycode,
                        *pressed,
                    )
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                    // kCGEventFlagsChanged = 12
                    CGEvent::set_type(Some(&event), CGEventType(12));
                    CGEvent::set_flags(Some(&event), self.modifier_flags);
                    self.post_event(&event);
                    return Ok(());
                }

                let event =
                    CGEvent::new_keyboard_event(Some(&self.event_source.0), mac_keycode, *pressed)
                        .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::set_flags(Some(&event), self.modifier_flags);
                self.post_event(&event);
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        // The client session refreshes this cache through screen_size() every
        // five seconds. Pointer frames must not synchronously query
        // WindowServer for dimensions on every movement.
        let (sw, sh) = self.screen_size.get();
        let x = x.clamp(0, sw as i32 - 1) as f64;
        let y = y.clamp(0, sh as i32 - 1) as f64;
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
        self.screen_size.refresh()
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn scroll_delta_fields_preserve_direction_and_fractional_motion() {
        assert_eq!(scroll_delta_fields(2.5), (-3, -163_840));
        assert_eq!(scroll_delta_fields(-0.25), (0, 16_384));
    }

    #[test]
    fn screen_size_cache_reflects_updated_dimensions() {
        let cache = ScreenSizeCache::new(1920, 1080);

        assert_eq!(cache.get(), (1920, 1080));
        cache.update(2560, 1440);
        assert_eq!(cache.get(), (2560, 1440));
    }
}
