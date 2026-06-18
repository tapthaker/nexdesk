/// Keeps macOS in an interactive power state while Nexdesk is connected.
///
/// This is scoped to the connection lifetime. It avoids the multi-second
/// first-input/network wake penalty after the Mac has been idle, without
/// spawning an external `caffeinate` process.
#[must_use]
pub struct InteractivePowerAssertion {
    #[cfg(target_os = "macos")]
    ids: Vec<u32>,
}

impl InteractivePowerAssertion {
    pub fn new(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            let ids = create_macos_power_assertions(reason);
            Self { ids }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = reason;
            Self {}
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for InteractivePowerAssertion {
    fn drop(&mut self) {
        unsafe {
            for id in self.ids.drain(..) {
                let ret = IOPMAssertionRelease(id);
                if ret != 0 {
                    tracing::debug!("IOPMAssertionRelease({}) returned {}", id, ret);
                }
            }
        }
    }
}

/// Keep the HID/WindowServer input path warm without visibly moving the cursor.
///
/// Some macOS systems accept synthetic mouse events immediately after idle but
/// do not reflect them smoothly for several seconds. Posting a no-op pointer
/// event periodically while connected keeps that path hot.
pub fn tickle_input_system() {
    #[cfg(target_os = "macos")]
    {
        use objc2_core_graphics::{
            CGEvent, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
            CGEventType, CGMouseButton,
        };

        let Some(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            return;
        };
        let Some(event) = CGEvent::new(Some(&source)) else {
            return;
        };
        let loc = CGEvent::location(Some(&event));
        unsafe {
            CGWarpMouseCursorPosition(loc);
        }
        if let Some(move_event) = CGEvent::new_mouse_event(
            Some(&source),
            CGEventType::MouseMoved,
            loc,
            CGMouseButton::Left,
        ) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&move_event));
        }
    }
}

/// Wake the display from sleep when remote input arrives.
///
/// On macOS, uses IOKit to declare user activity, which is the same mechanism
/// as `caffeinate -u`. On Linux, uses `xset dpms force on` for X11/XWayland.
pub fn wake_display() {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;

        const IOPM_USER_ACTIVE_LOCAL: u32 = 0;

        let name = CFString::new("nexdesk remote input");
        let mut assertion_id: u32 = 0;
        let ret = unsafe {
            IOPMAssertionDeclareUserActivity(
                name.as_concrete_TypeRef(),
                IOPM_USER_ACTIVE_LOCAL,
                &mut assertion_id,
            )
        };
        if ret != 0 {
            tracing::debug!("IOPMAssertionDeclareUserActivity returned {}", ret);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xset")
            .args(["dpms", "force", "on"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(target_os = "macos")]
use core_foundation::string::CFStringRef;
#[cfg(target_os = "macos")]
use objc2_core_foundation::CGPoint;

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut u32,
    ) -> i32;

    fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;

    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
}

#[cfg(target_os = "macos")]
fn create_macos_power_assertions(reason: &str) -> Vec<u32> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

    let name = CFString::new(reason);
    let assertion_types = [
        // Keep the machine responsive for network/input while idle.
        "PreventUserIdleSystemSleep",
        // Avoid display/WindowServer cold-start on first remote input.
        "PreventUserIdleDisplaySleep",
    ];

    let mut ids = Vec::new();
    for assertion_type in assertion_types {
        let assertion_type = CFString::new(assertion_type);
        let mut id = 0;
        let ret = unsafe {
            IOPMAssertionCreateWithName(
                assertion_type.as_concrete_TypeRef(),
                K_IOPM_ASSERTION_LEVEL_ON,
                name.as_concrete_TypeRef(),
                &mut id,
            )
        };
        if ret == 0 {
            ids.push(id);
        } else {
            tracing::debug!(
                "IOPMAssertionCreateWithName({}) returned {}",
                assertion_type,
                ret
            );
        }
    }

    if !ids.is_empty() {
        tracing::info!("Holding macOS interactive power assertion while connected");
    }

    ids
}
