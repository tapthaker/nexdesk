pub mod capture;
pub mod inject;
pub mod keymap;

#[cfg(target_os = "linux")]
pub mod linux_x11;

#[cfg(target_os = "linux")]
pub mod linux_wayland;

#[cfg(target_os = "linux")]
pub mod wayland_layer_shell;

#[cfg(target_os = "macos")]
pub mod macos;

/// Check (and on macOS, prompt for) required input permissions.
/// On macOS this triggers the Accessibility permissions dialog if not already granted.
/// On other platforms this is a no-op.
pub fn ensure_accessibility() -> color_eyre::eyre::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;

        extern "C" {
            fn AXIsProcessTrustedWithOptions(
                options: core_foundation::base::CFTypeRef,
            ) -> bool;
        }

        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(key, value)]);

        let trusted = unsafe {
            AXIsProcessTrustedWithOptions(options.as_CFTypeRef())
        };

        if trusted {
            tracing::info!("Accessibility: granted");
        } else {
            tracing::warn!(
                "Accessibility permission required. A system dialog should have appeared. \
                 Grant access in System Settings > Privacy & Security > Accessibility, \
                 then restart nexdesk."
            );
            return Err(color_eyre::eyre::eyre!(
                "Accessibility permission not granted. Grant access and restart."
            ));
        }
    }

    Ok(())
}
