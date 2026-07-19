pub mod capture;
pub mod inject;
pub mod keymap;
pub mod session;
pub mod wake;

#[cfg(target_os = "linux")]
pub mod linux_x11;

#[cfg(target_os = "linux")]
pub mod linux_wayland;

#[cfg(target_os = "linux")]
pub mod wayland_layer_shell;

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub mod wayland_layer_shell {
    /// Stub types for non-Linux platforms (never instantiated at runtime).
    #[derive(Debug)]
    pub enum LayerShellEvent {
        EdgeEnter {
            direction: crate::net::protocol::Direction,
        },
        EdgeLeave,
        GrabLost,
        MouseMove {
            dx: f64,
            dy: f64,
        },
        MouseButton {
            button: u32,
            pressed: bool,
        },
        MouseScroll {
            dx: f64,
            dy: f64,
        },
        ScrollEnd,
        KeyEvent {
            keycode: u32,
            pressed: bool,
        },
        KeyModifiers {
            depressed: u32,
            latched: u32,
            locked: u32,
            group: u32,
        },
    }

    #[derive(Debug)]
    pub enum LayerShellCommand {
        CaptureKeyboard,
        Release,
        Shutdown,
    }
}

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_hid;

/// Check if accessibility permission is currently granted (no prompt).
/// Always returns true on non-macOS platforms.
pub fn is_accessibility_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        unsafe { AXIsProcessTrusted() }
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// Request accessibility permission, showing the system prompt dialog on macOS.
/// Returns whether permission is currently granted.
/// Always returns true on non-macOS platforms.
pub fn request_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;

        extern "C" {
            fn AXIsProcessTrustedWithOptions(options: core_foundation::base::CFTypeRef) -> bool;
        }

        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(key, value)]);

        unsafe { AXIsProcessTrustedWithOptions(options.as_CFTypeRef()) }
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// Check (and on macOS, prompt for) required input permissions.
/// On macOS this triggers the Accessibility permissions dialog if not already granted.
/// On other platforms this is a no-op.
pub fn ensure_accessibility() -> color_eyre::eyre::Result<()> {
    if request_accessibility() {
        tracing::info!("Accessibility: granted");
        Ok(())
    } else {
        tracing::warn!(
            "Accessibility permission required. A system dialog should have appeared. \
             Grant access in System Settings > Privacy & Security > Accessibility, \
             then restart nexdesk."
        );
        Err(color_eyre::eyre::eyre!(
            "Accessibility permission not granted. Grant access and restart."
        ))
    }
}
