use tracing::{info, warn};

/// Hold this guard for the daemon lifetime to prevent idle system sleep while
/// still allowing the display to sleep normally.
#[cfg(target_os = "macos")]
pub struct IdleSleepInhibitor {
    assertion_id: Option<u32>,
}

#[cfg(target_os = "macos")]
impl Drop for IdleSleepInhibitor {
    fn drop(&mut self) {
        if let Some(id) = self.assertion_id.take() {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub struct IdleSleepInhibitor {
    child: Option<std::process::Child>,
}

#[cfg(target_os = "linux")]
impl Drop for IdleSleepInhibitor {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub struct IdleSleepInhibitor;

/// Prevent automatic idle system sleep without preventing display sleep.
#[cfg(target_os = "macos")]
pub fn inhibit_idle_system_sleep() -> IdleSleepInhibitor {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    let assertion_type = CFString::new("PreventUserIdleSystemSleep");
    let reason = CFString::new("Nexdesk network connection");
    let mut assertion_id = 0u32;
    let status = unsafe {
        IOPMAssertionCreateWithName(
            assertion_type.as_concrete_TypeRef() as *const std::ffi::c_void,
            255,
            reason.as_concrete_TypeRef() as *const std::ffi::c_void,
            &mut assertion_id,
        )
    };
    if status == 0 {
        info!("Preventing idle system sleep while allowing display sleep");
        IdleSleepInhibitor {
            assertion_id: Some(assertion_id),
        }
    } else {
        warn!(
            "Failed to prevent idle system sleep: IOKit status {}",
            status
        );
        IdleSleepInhibitor { assertion_id: None }
    }
}

/// Use logind's idle inhibitor on Linux. This does not block explicit sleep,
/// lid-close actions, or display power management.
#[cfg(target_os = "linux")]
pub fn inhibit_idle_system_sleep() -> IdleSleepInhibitor {
    let child = std::process::Command::new("systemd-inhibit")
        .args([
            "--what=idle",
            "--who=Nexdesk",
            "--why=Keep network KVM available",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            info!("Preventing idle system sleep while allowing display sleep");
            IdleSleepInhibitor { child: Some(child) }
        }
        Err(e) => {
            warn!("Failed to start Linux idle-sleep inhibitor: {}", e);
            IdleSleepInhibitor { child: None }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn inhibit_idle_system_sleep() -> IdleSleepInhibitor {
    IdleSleepInhibitor
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const std::ffi::c_void,
        assertion_level: u32,
        assertion_name: *const std::ffi::c_void,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: *const std::ffi::c_void,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

/// Wake the macOS display by declaring remote user activity.
#[cfg(target_os = "macos")]
pub fn wake_display() {
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
    if status != 0 {
        warn!("Failed to wake macOS display: IOKit status {}", status);
    }
}

#[cfg(target_os = "linux")]
fn xset_wake_args() -> [&'static str; 3] {
    ["dpms", "force", "on"]
}

#[cfg(target_os = "linux")]
pub fn wake_display() {
    // This function is called from a short-lived helper thread. Use `status`
    // instead of `spawn` so the xset child is reaped and repeated wake nudges
    // do not accumulate zombie processes.
    let _ = std::process::Command::new("xset")
        .args(xset_wake_args())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn wake_display() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn xset_wake_command_uses_dpms_force_on() {
        assert_eq!(xset_wake_args(), ["dpms", "force", "on"]);
    }
}
