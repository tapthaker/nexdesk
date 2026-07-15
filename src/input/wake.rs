/// Wake the display from sleep when remote input arrives on Linux/X11.
///
/// macOS power assertions / caffeinate-style user-activity nudges were removed:
/// they did not fix the idle synthetic-input lag and can leave confusing power
/// assertion state during restarts.
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn xset_wake_command_uses_dpms_force_on() {
        assert_eq!(xset_wake_args(), ["dpms", "force", "on"]);
    }
}
