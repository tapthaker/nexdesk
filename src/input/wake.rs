#[cfg(target_os = "macos")]
use color_eyre::eyre::eyre;
use color_eyre::eyre::Result;

use crate::ports::{DisplaySessionControl, SleepInhibitor};

/// Production display/session adapter. Idle sleep remains uninhibited, while
/// explicit wake requests notify the platform that the remote user is active.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformDisplaySessionControl;

impl DisplaySessionControl for PlatformDisplaySessionControl {
    fn inhibit_idle_sleep(&self) -> Result<Box<dyn SleepInhibitor>> {
        Ok(Box::new(NoopSleepInhibitor))
    }

    fn wake_display(&self) -> Result<()> {
        wake_display()
    }
}

struct NoopSleepInhibitor;

/// Wake the display from sleep when remote input arrives on Linux/X11.
#[cfg(target_os = "linux")]
pub fn wake_display() -> Result<()> {
    std::process::Command::new("xset")
        .args(["dpms", "force", "on"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: *const std::ffi::c_void,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;
}

/// Wake the macOS display with a short-lived remote-user activity declaration.
/// This does not install the process-lifetime idle-sleep assertion that caused
/// problems during earlier Nexdesk restarts.
#[cfg(target_os = "macos")]
pub fn wake_display() -> Result<()> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    const USER_ACTIVE_REMOTE: u32 = 1;

    let reason = CFString::new("Nexdesk remote pointer activity");
    let mut assertion_id = 0u32;
    let status = unsafe {
        IOPMAssertionDeclareUserActivity(
            reason.as_concrete_TypeRef() as *const std::ffi::c_void,
            USER_ACTIVE_REMOTE,
            &mut assertion_id,
        )
    };

    iokit_wake_result(status)
}

#[cfg(target_os = "macos")]
fn iokit_wake_result(status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(eyre!(
            "IOKit failed to declare remote user activity: status {}",
            status
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn wake_display() -> Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn iokit_wake_status_reports_platform_failures() {
        assert!(iokit_wake_result(0).is_ok());
        assert!(iokit_wake_result(-1)
            .unwrap_err()
            .to_string()
            .contains("status -1"));
    }
}
