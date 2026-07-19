/// Wake the display from sleep when remote input arrives on Linux/X11.
///
/// macOS power assertions / caffeinate-style user-activity nudges were removed:
/// they did not fix the idle synthetic-input lag and can leave confusing power
/// assertion state during restarts.
#[cfg(target_os = "linux")]
pub fn wake_display() {
    let _ = std::process::Command::new("xset")
        .args(["dpms", "force", "on"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
