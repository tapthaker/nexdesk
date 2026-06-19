#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::raw::{c_char, c_int, c_long, c_void};
#[cfg(target_os = "macos")]
use std::ptr;

#[cfg(target_os = "macos")]
use color_eyre::eyre::{eyre, Result};
#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;
#[cfg(target_os = "macos")]
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayPixelsHigh, CGDisplayPixelsWide, CGEvent, CGEventSource, CGEventSourceStateID};
#[cfg(target_os = "macos")]
use tracing::{info, warn};

#[cfg(target_os = "macos")]
use crate::input::inject::InputInjector;
#[cfg(target_os = "macos")]
use crate::input::macos::MacOSInjector;
#[cfg(target_os = "macos")]
use crate::net::protocol::Message;

#[cfg(target_os = "macos")]
const MAIN_DISPLAY: CGDirectDisplayID = 0;

#[cfg(target_os = "macos")]
type CFAllocatorRef = *const c_void;
#[cfg(target_os = "macos")]
type CFTypeRef = *const c_void;
#[cfg(target_os = "macos")]
type CFStringRef = *const c_void;
#[cfg(target_os = "macos")]
type CFNumberRef = *const c_void;
#[cfg(target_os = "macos")]
type CFDataRef = *const c_void;
#[cfg(target_os = "macos")]
type CFMutableDictionaryRef = *mut c_void;
#[cfg(target_os = "macos")]
type IOHIDUserDeviceRef = *mut c_void;

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, c_str: *const c_char, encoding: u32) -> CFStringRef;
    fn CFNumberCreate(alloc: CFAllocatorRef, the_type: c_int, value_ptr: *const c_void) -> CFNumberRef;
    fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, length: c_long) -> CFDataRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;

    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: c_long,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
    fn CFRelease(cf: CFTypeRef);
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDUserDeviceCreate(allocator: CFAllocatorRef, properties: *const c_void) -> IOHIDUserDeviceRef;
    fn IOHIDUserDeviceHandleReport(device: IOHIDUserDeviceRef, report: *const u8, report_length: c_long) -> i32;
}

#[cfg(target_os = "macos")]
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
#[cfg(target_os = "macos")]
const K_CF_NUMBER_SINT32_TYPE: c_int = 3;

#[cfg(target_os = "macos")]
const MOUSE_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01,
    0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
    0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38,
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06,
    0xC0, 0xC0,
];

#[cfg(target_os = "macos")]
const KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0,
    0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08,
    0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00,
    0x29, 0x65, 0x81, 0x00, 0xC0,
];

#[cfg(target_os = "macos")]
pub struct MacOSHidInjector {
    mouse: IOHIDUserDeviceRef,
    keyboard: IOHIDUserDeviceRef,
    fallback: MacOSInjector,
    cursor_x: i32,
    cursor_y: i32,
    buttons: u8,
    modifiers: u8,
    keys: HashSet<u8>,
}

#[cfg(target_os = "macos")]
unsafe impl Send for MacOSHidInjector {}

#[cfg(target_os = "macos")]
impl Drop for MacOSHidInjector {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.mouse as CFTypeRef);
            CFRelease(self.keyboard as CFTypeRef);
        }
    }
}

#[cfg(target_os = "macos")]
impl MacOSHidInjector {
    pub fn new() -> Result<Self> {
        info!("Initializing experimental IOHIDUserDevice injector");
        let mouse = create_hid_device("Nexdesk Virtual Mouse", 2, 1, MOUSE_REPORT_DESCRIPTOR)?;
        info!("Created IOHID virtual mouse device");
        let keyboard = create_hid_device("Nexdesk Virtual Keyboard", 1, 6, KEYBOARD_REPORT_DESCRIPTOR)?;
        info!("Created IOHID virtual keyboard device");
        let fallback = MacOSInjector::new()?;
        let (cursor_x, cursor_y) = current_position().unwrap_or((0, 0));
        info!("Using experimental IOHIDUserDevice injector on macOS");
        Ok(Self { mouse, keyboard, fallback, cursor_x, cursor_y, buttons: 0, modifiers: 0, keys: HashSet::new() })
    }

    fn send_mouse_report(&self, buttons: u8, dx: i8, dy: i8, wheel: i8) -> Result<()> {
        let report = [buttons & 0x07, dx as u8, dy as u8, wheel as u8];
        let ret = unsafe { IOHIDUserDeviceHandleReport(self.mouse, report.as_ptr(), report.len() as c_long) };
        if ret != 0 { return Err(eyre!("IOHID mouse report failed: {}", ret)); }
        Ok(())
    }

    fn send_keyboard_report(&self) -> Result<()> {
        let mut report = [0u8; 8];
        report[0] = self.modifiers;
        for (i, key) in self.keys.iter().copied().take(6).enumerate() {
            report[2 + i] = key;
        }
        let ret = unsafe { IOHIDUserDeviceHandleReport(self.keyboard, report.as_ptr(), report.len() as c_long) };
        if ret != 0 { return Err(eyre!("IOHID keyboard report failed: {}", ret)); }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl InputInjector for MacOSHidInjector {
    fn inject(&mut self, event: &Message) -> Result<()> {
        match event {
            Message::MouseMove { x, y } => self.move_mouse(*x, *y),
            Message::MouseButton { button, pressed } => {
                let mask = match button { 0 => 0x01, 1 => 0x02, 2 => 0x04, _ => return Ok(()) };
                if *pressed { self.buttons |= mask; } else { self.buttons &= !mask; }
                self.send_mouse_report(self.buttons, 0, 0, 0)
            }
            Message::KeyEvent { keycode, pressed, .. } => {
                if let Some(bit) = evdev_to_hid_modifier(*keycode) {
                    if *pressed { self.modifiers |= bit; } else { self.modifiers &= !bit; }
                    return self.send_keyboard_report();
                }
                let Some(usage) = evdev_to_hid_usage(*keycode) else {
                    warn!("IOHID injector falling back for unsupported keycode {}", keycode);
                    return self.fallback.inject(event);
                };
                if *pressed { self.keys.insert(usage); } else { self.keys.remove(&usage); }
                self.send_keyboard_report()
            }
            // Scroll/media keys stay on the proven Quartz path for now.
            _ => self.fallback.inject(event),
        }
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<()> {
        let sw = CGDisplayPixelsWide(MAIN_DISPLAY) as i32;
        let sh = CGDisplayPixelsHigh(MAIN_DISPLAY) as i32;
        let target_x = x.clamp(0, sw - 1);
        let target_y = y.clamp(0, sh - 1);
        let mut dx = target_x - self.cursor_x;
        let mut dy = target_y - self.cursor_y;
        while dx != 0 || dy != 0 {
            let step_x = dx.clamp(-127, 127) as i8;
            let step_y = dy.clamp(-127, 127) as i8;
            self.send_mouse_report(self.buttons, step_x, step_y, 0)?;
            self.cursor_x += step_x as i32;
            self.cursor_y += step_y as i32;
            dx = target_x - self.cursor_x;
            dy = target_y - self.cursor_y;
        }
        Ok(())
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        self.fallback.screen_size()
    }
}

#[cfg(target_os = "macos")]
fn create_hid_device(name: &str, usage_page: i32, usage: i32, descriptor: &[u8]) -> Result<IOHIDUserDeviceRef> {
    unsafe {
        info!(
            "Creating IOHIDUserDevice: name='{}' usage_page={} usage={} descriptor_len={}",
            name,
            usage_page,
            usage,
            descriptor.len()
        );

        let dict = CFDictionaryCreateMutable(
            ptr::null(),
            0,
            &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
        );
        if dict.is_null() { return Err(eyre!("CFDictionaryCreateMutable failed for {}", name)); }

        set_string(dict, name, "Transport", "Virtual")?;
        set_string(dict, name, "Manufacturer", "Nexdesk")?;
        set_string(dict, name, "Product", name)?;
        set_string(dict, name, "SerialNumber", if usage == 1 { "nexdesk-mouse" } else { "nexdesk-keyboard" })?;
        set_i32(dict, name, "VendorID", 0x1209)?;
        set_i32(dict, name, "ProductID", if usage == 1 { 0x4242 } else { 0x4243 })?;
        set_i32(dict, name, "VersionNumber", 1)?;
        set_i32(dict, name, "PrimaryUsagePage", usage_page)?;
        set_i32(dict, name, "PrimaryUsage", usage)?;
        set_i32(dict, name, "MaxInputReportSize", 64)?;
        set_data(dict, name, "ReportDescriptor", descriptor)?;

        info!("Calling IOHIDUserDeviceCreate for {}", name);
        let device = IOHIDUserDeviceCreate(ptr::null(), dict as *const c_void);
        CFRelease(dict as CFTypeRef);
        if device.is_null() {
            return Err(eyre!(
                "IOHIDUserDeviceCreate returned null for {} (usage_page={}, usage={}, descriptor_len={})",
                name,
                usage_page,
                usage,
                descriptor.len()
            ));
        }
        Ok(device)
    }
}

#[cfg(target_os = "macos")]
unsafe fn set_string(dict: CFMutableDictionaryRef, device_name: &str, key: &str, value: &str) -> Result<()> {
    let k = cf_string(key)?;
    let v = cf_string(value)?;
    info!("IOHID property for {}: {}='{}'", device_name, key, value);
    CFDictionarySetValue(dict, k as *const c_void, v as *const c_void);
    CFRelease(k as CFTypeRef);
    CFRelease(v as CFTypeRef);
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn set_i32(dict: CFMutableDictionaryRef, device_name: &str, key: &str, value: i32) -> Result<()> {
    let k = cf_string(key)?;
    let n = CFNumberCreate(ptr::null(), K_CF_NUMBER_SINT32_TYPE, &value as *const _ as *const c_void);
    if n.is_null() {
        CFRelease(k as CFTypeRef);
        return Err(eyre!("CFNumberCreate failed for {} property {}", device_name, key));
    }
    info!("IOHID property for {}: {}={}", device_name, key, value);
    CFDictionarySetValue(dict, k as *const c_void, n as *const c_void);
    CFRelease(k as CFTypeRef);
    CFRelease(n as CFTypeRef);
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn set_data(dict: CFMutableDictionaryRef, device_name: &str, key: &str, value: &[u8]) -> Result<()> {
    let k = cf_string(key)?;
    let d = CFDataCreate(ptr::null(), value.as_ptr(), value.len() as c_long);
    if d.is_null() {
        CFRelease(k as CFTypeRef);
        return Err(eyre!("CFDataCreate failed for {} property {}", device_name, key));
    }
    info!("IOHID property for {}: {}=<{} bytes>", device_name, key, value.len());
    CFDictionarySetValue(dict, k as *const c_void, d as *const c_void);
    CFRelease(k as CFTypeRef);
    CFRelease(d as CFTypeRef);
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn cf_string(value: &str) -> Result<CFStringRef> {
    let c = CString::new(value).map_err(|e| eyre!("CString failed for '{}': {}", value, e))?;
    let s = CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8);
    if s.is_null() {
        return Err(eyre!("CFStringCreateWithCString failed for '{}'", value));
    }
    Ok(s)
}

#[cfg(target_os = "macos")]
fn current_position() -> Result<(i32, i32)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| eyre!("Failed to create CGEventSource"))?;
    let event = CGEvent::new(Some(&source)).ok_or_else(|| eyre!("Failed to create CGEvent"))?;
    let loc: CGPoint = CGEvent::location(Some(&event));
    Ok((loc.x as i32, loc.y as i32))
}

#[cfg(target_os = "macos")]
fn evdev_to_hid_modifier(evdev: u32) -> Option<u8> {
    match evdev {
        29 => Some(0x01),  // left ctrl
        42 => Some(0x02),  // left shift
        56 => Some(0x04),  // left alt
        125 => Some(0x08), // left gui
        97 => Some(0x10),  // right ctrl
        54 => Some(0x20),  // right shift
        100 => Some(0x40), // right alt
        126 => Some(0x80), // right gui
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn evdev_to_hid_usage(evdev: u32) -> Option<u8> {
    match evdev {
        // Letters
        30 => Some(0x04), 48 => Some(0x05), 46 => Some(0x06), 32 => Some(0x07),
        18 => Some(0x08), 33 => Some(0x09), 34 => Some(0x0A), 35 => Some(0x0B),
        23 => Some(0x0C), 36 => Some(0x0D), 37 => Some(0x0E), 38 => Some(0x0F),
        50 => Some(0x10), 49 => Some(0x11), 24 => Some(0x12), 25 => Some(0x13),
        16 => Some(0x14), 19 => Some(0x15), 31 => Some(0x16), 20 => Some(0x17),
        22 => Some(0x18), 47 => Some(0x19), 17 => Some(0x1A), 45 => Some(0x1B),
        21 => Some(0x1C), 44 => Some(0x1D),
        // Number row
        2 => Some(0x1E), 3 => Some(0x1F), 4 => Some(0x20), 5 => Some(0x21), 6 => Some(0x22),
        7 => Some(0x23), 8 => Some(0x24), 9 => Some(0x25), 10 => Some(0x26), 11 => Some(0x27),
        // Control / punctuation
        28 => Some(0x28), 1 => Some(0x29), 14 => Some(0x2A), 15 => Some(0x2B), 57 => Some(0x2C),
        12 => Some(0x2D), 13 => Some(0x2E), 26 => Some(0x2F), 27 => Some(0x30), 43 => Some(0x31),
        39 => Some(0x33), 40 => Some(0x34), 41 => Some(0x35), 51 => Some(0x36), 52 => Some(0x37), 53 => Some(0x38),
        58 => Some(0x39),
        // Function keys
        59 => Some(0x3A), 60 => Some(0x3B), 61 => Some(0x3C), 62 => Some(0x3D), 63 => Some(0x3E),
        64 => Some(0x3F), 65 => Some(0x40), 66 => Some(0x41), 67 => Some(0x42), 68 => Some(0x43),
        87 => Some(0x44), 88 => Some(0x45),
        // Navigation
        110 => Some(0x49), 102 => Some(0x4A), 104 => Some(0x4B), 111 => Some(0x4C),
        107 => Some(0x4D), 109 => Some(0x4E), 106 => Some(0x4F), 105 => Some(0x50),
        108 => Some(0x51), 103 => Some(0x52),
        _ => None,
    }
}
