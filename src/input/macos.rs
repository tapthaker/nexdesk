#[cfg(target_os = "macos")]
use color_eyre::eyre::Result;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{CFRetained, CGPoint};

#[cfg(target_os = "macos")]
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags,
    CGDisplayRegisterReconfigurationCallback, CGEvent, CGEventField, CGEventFlags, CGEventSource,
    CGEventSourceStateID, CGEventTapLocation, CGEventType, CGGetActiveDisplayList, CGMainDisplayID,
    CGMouseButton, CGScrollEventUnit,
};

#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::os::raw::{c_int, c_uint, c_void};
#[cfg(target_os = "macos")]
use std::sync::{LazyLock, Mutex};

#[cfg(target_os = "macos")]
use tracing::{debug, info};

#[cfg(target_os = "macos")]
use crate::input::capture::InputCapture;
#[cfg(target_os = "macos")]
use crate::input::inject::InputInjector;
#[cfg(target_os = "macos")]
use crate::input::keymap;
#[cfg(target_os = "macos")]
use crate::net::protocol::Message;

#[cfg(target_os = "macos")]
const MAX_ACTIVE_DISPLAYS: usize = 32;
#[cfg(target_os = "macos")]
const MAX_SCROLL_PIXELS_PER_MESSAGE: i32 = 4096;

#[cfg(target_os = "macos")]
static DESKTOP_BOUNDS_CACHE: LazyLock<Mutex<Option<DesktopBounds>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(target_os = "macos")]
static DISPLAY_CALLBACK_STATUS: LazyLock<i32> = LazyLock::new(|| unsafe {
    CGDisplayRegisterReconfigurationCallback(
        Some(display_reconfiguration_callback),
        std::ptr::null_mut(),
    )
    .0
});

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CGDisplayHideCursor(display: CGDirectDisplayID) -> i32;
    fn CGDisplayShowCursor(display: CGDirectDisplayID) -> i32;
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
pub struct MacOSCapturer;

#[cfg(target_os = "macos")]
impl MacOSCapturer {
    pub fn new() -> Result<Self> {
        let (screen_width, screen_height) = desktop_size_u32()?;
        debug!("macOS capturer: desktop {}x{}", screen_width, screen_height);
        Ok(Self)
    }
}

#[cfg(target_os = "macos")]
impl InputCapture for MacOSCapturer {
    fn mouse_position(&self) -> Result<(i32, i32)> {
        normalized_cursor_position(desktop_bounds()?)
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        desktop_size_u32()
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DesktopBounds {
    pub min_x: i32,
    pub min_y: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "macos")]
fn desktop_bounds_from_rects(rects: &[(f64, f64, f64, f64)]) -> Option<DesktopBounds> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    for &(x, y, width, height) in rects {
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
        {
            continue;
        }
        min_x = min_x.min(x.floor());
        min_y = min_y.min(y.floor());
        max_x = max_x.max((x + width).ceil());
        max_y = max_y.max((y + height).ceil());
    }

    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
    }
    let min_x = i32::try_from(min_x as i64).ok()?;
    let min_y = i32::try_from(min_y as i64).ok()?;
    let width = u32::try_from((max_x as i64).checked_sub(i64::from(min_x))?).ok()?;
    let height = u32::try_from((max_y as i64).checked_sub(i64::from(min_y))?).ok()?;
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return None;
    }
    Some(DesktopBounds {
        min_x,
        min_y,
        width,
        height,
    })
}

#[cfg(target_os = "macos")]
fn query_desktop_bounds() -> Result<DesktopBounds> {
    let mut displays = [0 as CGDirectDisplayID; MAX_ACTIVE_DISPLAYS];
    let mut count = 0u32;
    let status =
        unsafe { CGGetActiveDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count) };
    if status.0 != 0 || count == 0 {
        return Err(color_eyre::eyre::eyre!(
            "Failed to query active macOS displays: status {}, count {}",
            status.0,
            count
        ));
    }

    let rects: Vec<_> = displays[..count.min(displays.len() as u32) as usize]
        .iter()
        .map(|display| {
            let bounds = CGDisplayBounds(*display);
            (
                bounds.origin.x,
                bounds.origin.y,
                bounds.size.width,
                bounds.size.height,
            )
        })
        .collect();
    desktop_bounds_from_rects(&rects)
        .ok_or_else(|| color_eyre::eyre::eyre!("Active macOS displays have invalid desktop bounds"))
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn display_reconfiguration_callback(
    _display: CGDirectDisplayID,
    _flags: CGDisplayChangeSummaryFlags,
    _user_info: *mut c_void,
) {
    let mut cache = DESKTOP_BOUNDS_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cache = None;
}

#[cfg(target_os = "macos")]
pub(crate) fn desktop_bounds() -> Result<DesktopBounds> {
    // CoreGraphics invalidates the cached union whenever display topology or
    // mode changes. If callback registration fails, query on every call rather
    // than risk using stale geometry.
    if *DISPLAY_CALLBACK_STATUS != 0 {
        return query_desktop_bounds();
    }

    let mut cache = DESKTOP_BOUNDS_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(bounds) = *cache {
        return Ok(bounds);
    }

    let bounds = query_desktop_bounds()?;
    *cache = Some(bounds);
    Ok(bounds)
}

#[cfg(target_os = "macos")]
fn refresh_desktop_bounds() -> Result<DesktopBounds> {
    let bounds = query_desktop_bounds()?;
    let mut cache = DESKTOP_BOUNDS_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cache = Some(bounds);
    Ok(bounds)
}

#[cfg(target_os = "macos")]
fn desktop_size_u32() -> Result<(u32, u32)> {
    let bounds = desktop_bounds()?;
    Ok((bounds.width, bounds.height))
}

#[cfg(target_os = "macos")]
fn normalized_desktop_point(bounds: DesktopBounds, x: i32, y: i32) -> CGPoint {
    let x = x.clamp(0, bounds.width as i32 - 1);
    let y = y.clamp(0, bounds.height as i32 - 1);
    CGPoint {
        x: f64::from(bounds.min_x.saturating_add(x)),
        y: f64::from(bounds.min_y.saturating_add(y)),
    }
}

#[cfg(target_os = "macos")]
fn normalized_cursor_position(bounds: DesktopBounds) -> Result<(i32, i32)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
    let event = CGEvent::new(Some(&source))
        .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;
    let loc = CGEvent::location(Some(&event));
    Ok((
        (loc.x as i32).saturating_sub(bounds.min_x),
        (loc.y as i32).saturating_sub(bounds.min_y),
    ))
}

#[cfg(target_os = "macos")]
fn consume_scroll_pixels(remainder: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    let total = *remainder + delta;
    let pixels = total.trunc().clamp(
        -(MAX_SCROLL_PIXELS_PER_MESSAGE as f64),
        MAX_SCROLL_PIXELS_PER_MESSAGE as f64,
    ) as i32;
    *remainder = if pixels.unsigned_abs() == MAX_SCROLL_PIXELS_PER_MESSAGE as u32 {
        0.0
    } else {
        total - pixels as f64
    };
    pixels
}

/// macOS input injector using CoreGraphics.
#[cfg(target_os = "macos")]
pub struct MacOSInjector {
    /// Currently pressed modifier keycodes used to derive synthesized flags.
    modifier_keys_down: HashSet<u32>,
    /// Non-modifier keys currently held, used to mark repeated key-downs.
    keys_down: HashSet<u32>,
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
    /// Fractional scroll pixels retained until they accumulate to an integer.
    scroll_x_remainder: f64,
    /// Fractional scroll pixels retained until they accumulate to an integer.
    scroll_y_remainder: f64,
    /// Desktop geometry snapshot shared by movement and edge reconciliation.
    desktop_bounds: DesktopBounds,
    /// Tracks CoreGraphics' balanced cursor hide/show calls.
    cursor_hidden: bool,
}

#[cfg(target_os = "macos")]
impl MacOSInjector {
    pub fn new() -> Result<Self> {
        let desktop_bounds = refresh_desktop_bounds()?;
        let (screen_width, screen_height) = (desktop_bounds.width, desktop_bounds.height);
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
            modifier_keys_down: HashSet::new(),
            keys_down: HashSet::new(),
            modifier_flags: CGEventFlags::empty(),
            buttons_down: 0,
            last_click: None,
            click_count: 0,
            source_state,
            tap_location,
            post_mode,
            scroll_x_remainder: 0.0,
            scroll_y_remainder: 0.0,
            desktop_bounds,
            cursor_hidden: false,
        })
    }

    pub(crate) fn desktop_bounds_snapshot(&self) -> DesktopBounds {
        self.desktop_bounds
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

    fn flags_for_modifier_keys(keys: &HashSet<u32>) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for &keycode in keys {
            if let Some(flag) = Self::modifier_flag(keycode) {
                flags |= flag;
            }
        }
        flags
    }

    /// Update tracked modifier flags based on an evdev keycode press/release.
    fn update_modifier_flags(&mut self, keycode: u32, pressed: bool) {
        if Self::modifier_flag(keycode).is_none() {
            return;
        }
        if pressed {
            self.modifier_keys_down.insert(keycode);
        } else {
            self.modifier_keys_down.remove(&keycode);
        }
        self.modifier_flags = Self::flags_for_modifier_keys(&self.modifier_keys_down);
    }

    fn event_source(&self) -> Result<CFRetained<CGEventSource>> {
        let source = CGEventSource::new(self.source_state)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEventSource"))?;
        // Avoid CoreGraphics' default local-event suppression window. This is
        // documented for remote-operation events and is harmless if the delay is
        // unrelated, but gives us a cheap A/B point for idle wake sluggishness.
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        Ok(source)
    }

    fn post_event(&self, event: &CGEvent) {
        CGEvent::post(self.tap_location, Some(event));
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
        let event = CGEvent::new_mouse_event(Some(&source), event_type, point, button)
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create mouse event"))?;
        self.post_event(&event);
        Ok(())
    }

    fn current_position(&self) -> Result<(i32, i32)> {
        let source = self.event_source()?;
        let event = CGEvent::new(Some(&source))
            .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create CGEvent"))?;
        let loc = CGEvent::location(Some(&event));
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

    fn post_legacy_scroll(&self, dx: i32, dy: i32) {
        let wheel1 = -dy;
        let wheel2 = -dx;
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
impl Drop for MacOSInjector {
    fn drop(&mut self) {
        if self.cursor_hidden {
            unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
            self.cursor_hidden = false;
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

                let dy_pixels = consume_scroll_pixels(&mut self.scroll_y_remainder, *dy);
                let dx_pixels = consume_scroll_pixels(&mut self.scroll_x_remainder, *dx);
                let source = self.event_source()?;

                if self.post_mode == PostMode::LegacyQuartz {
                    self.post_legacy_scroll(dx_pixels, dy_pixels);
                    return Ok(());
                }

                // Vertical scroll: pixel-based events without the continuous
                // flag. This works in all apps including Firefox.
                if dy_pixels != 0 {
                    let event = CGEvent::new_scroll_wheel_event2(
                        Some(&source),
                        CGScrollEventUnit::Pixel,
                        1,
                        -dy_pixels,
                        0,
                        0,
                    )
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create scroll event"))?;
                    self.post_event(&event);
                }

                // Horizontal scroll: continuous trackpad events with phases.
                // This is what triggers swipe-to-navigate in browsers/Finder.
                if dx_pixels != 0 || (*phase == ScrollPhase::Ended && dy_pixels == 0) {
                    let event = CGEvent::new_scroll_wheel_event2(
                        Some(&source),
                        CGScrollEventUnit::Pixel,
                        2,
                        0,
                        -dx_pixels,
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
                        -i64::from(dx_pixels),
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

                let is_repeat = if *pressed {
                    !self.keys_down.insert(*keycode)
                } else {
                    self.keys_down.remove(keycode);
                    false
                };
                let event = CGEvent::new_keyboard_event(Some(&source), mac_keycode, *pressed)
                    .ok_or_else(|| color_eyre::eyre::eyre!("Failed to create key event"))?;
                CGEvent::set_flags(Some(&event), self.modifier_flags);
                if is_repeat {
                    // kCGKeyboardEventAutorepeat. Apps still receive an ordinary
                    // key-down, while frameworks can identify it as a repeat.
                    CGEvent::set_integer_value_field(Some(&event), CGEventField(8), 1);
                }
                self.post_event(&event);
            }
            _ => {}
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let point = normalized_desktop_point(self.desktop_bounds, x, y);
        let x = point.x;
        let y = point.y;
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
            let ret = unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
            if ret != 0 {
                debug!("CGWarpMouseCursorPosition returned {}", ret);
            }
        }
        self.post_mouse_event(event_type, x, y, button)?;
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        Ok((self.desktop_bounds.width, self.desktop_bounds.height))
    }

    fn refresh_screen_size(&mut self) -> Result<(u32, u32)> {
        self.desktop_bounds = refresh_desktop_bounds()?;
        self.screen_size()
    }

    fn cursor_position(&self) -> Result<Option<(i32, i32)>> {
        normalized_cursor_position(self.desktop_bounds).map(Some)
    }

    fn set_cursor_visible(&mut self, visible: bool) -> Result<()> {
        if visible != self.cursor_hidden {
            return Ok(());
        }
        let status = unsafe {
            if visible {
                CGDisplayShowCursor(CGMainDisplayID())
            } else {
                CGDisplayHideCursor(CGMainDisplayID())
            }
        };
        if status != 0 {
            return Err(color_eyre::eyre::eyre!(
                "Failed to {} macOS cursor: status {}",
                if visible { "show" } else { "hide" },
                status
            ));
        }
        self.cursor_hidden = !visible;
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn modifier_flags_preserve_same_logical_modifier_until_all_keys_released() {
        let mut keys = HashSet::new();
        keys.insert(EVDEV_KEY_LEFTSHIFT);
        keys.insert(EVDEV_KEY_RIGHTSHIFT);
        assert!(MacOSInjector::flags_for_modifier_keys(&keys).contains(CGEventFlags::MaskShift));

        keys.remove(&EVDEV_KEY_LEFTSHIFT);
        assert!(MacOSInjector::flags_for_modifier_keys(&keys).contains(CGEventFlags::MaskShift));

        keys.remove(&EVDEV_KEY_RIGHTSHIFT);
        assert!(!MacOSInjector::flags_for_modifier_keys(&keys).contains(CGEventFlags::MaskShift));
    }

    #[test]
    fn scroll_pixel_consumption_preserves_fractional_deltas() {
        let mut remainder = 0.0;
        assert_eq!(consume_scroll_pixels(&mut remainder, 0.4), 0);
        assert_eq!(consume_scroll_pixels(&mut remainder, 0.4), 0);
        assert_eq!(consume_scroll_pixels(&mut remainder, 0.4), 1);
        assert!((remainder - 0.2).abs() < 1e-12);
        assert_eq!(consume_scroll_pixels(&mut remainder, -0.7), 0);
        assert!((remainder + 0.5).abs() < 1e-12);
        assert_eq!(consume_scroll_pixels(&mut remainder, -0.7), -1);
        assert!((remainder + 0.2).abs() < 1e-12);
    }

    #[test]
    fn scroll_pixel_consumption_ignores_non_finite_deltas() {
        let mut remainder = 0.25;
        assert_eq!(consume_scroll_pixels(&mut remainder, f64::INFINITY), 0);
        assert_eq!(remainder, 0.25);
    }

    #[test]
    fn scroll_pixel_consumption_caps_extreme_protocol_deltas() {
        let mut remainder = 0.25;
        assert_eq!(consume_scroll_pixels(&mut remainder, 10_000.0), 4096);
        assert_eq!(remainder, 0.0);
        assert_eq!(consume_scroll_pixels(&mut remainder, -10_000.0), -4096);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn desktop_bounds_include_all_side_by_side_displays() {
        let bounds =
            desktop_bounds_from_rects(&[(0.0, 0.0, 1470.0, 956.0), (1470.0, 0.0, 2560.0, 1440.0)])
                .unwrap();

        assert_eq!(
            bounds,
            DesktopBounds {
                min_x: 0,
                min_y: 0,
                width: 4030,
                height: 1440,
            }
        );
    }

    #[test]
    fn desktop_bounds_normalize_negative_display_origins() {
        let bounds = desktop_bounds_from_rects(&[
            (-2560.0, -200.0, 2560.0, 1440.0),
            (0.0, 0.0, 1470.0, 956.0),
        ])
        .unwrap();

        assert_eq!(
            bounds,
            DesktopBounds {
                min_x: -2560,
                min_y: -200,
                width: 4030,
                height: 1440,
            }
        );
    }

    #[test]
    fn desktop_bounds_reject_empty_or_invalid_displays() {
        assert_eq!(desktop_bounds_from_rects(&[]), None);
        assert_eq!(desktop_bounds_from_rects(&[(0.0, 0.0, 0.0, 10.0)]), None);
        assert_eq!(
            desktop_bounds_from_rects(&[(f64::NAN, 0.0, 10.0, 10.0)]),
            None
        );
    }
}
