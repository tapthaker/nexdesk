/// Wake the display from sleep when remote input arrives.
///
/// On macOS, uses IOKit to declare user activity (same mechanism as `caffeinate -u`).
/// On Linux, uses `xset dpms force on` to turn the display on.
pub fn wake_display() {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;

        #[link(name = "IOKit", kind = "framework")]
        extern "C" {
            fn IOPMAssertionDeclareUserActivity(
                assertion_name: core_foundation::string::CFStringRef,
                user_type: u32,
                assertion_id: *mut u32,
            ) -> i32;
        }

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
        // Works for X11 and XWayland sessions
        let _ = std::process::Command::new("xset")
            .args(["dpms", "force", "on"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
