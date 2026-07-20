use color_eyre::eyre::Result;

use crate::ports::{DisplaySessionControl, SleepInhibitor};

/// Production display/session adapter. Upstream intentionally does not create
/// macOS power assertions because they can survive confusingly across service
/// restarts and did not resolve synthetic-input lag.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformDisplaySessionControl;

impl DisplaySessionControl for PlatformDisplaySessionControl {
    fn inhibit_idle_sleep(&self) -> Result<Box<dyn SleepInhibitor>> {
        Ok(Box::new(NoopSleepInhibitor))
    }

    fn wake_display(&self) -> Result<()> {
        wake_display();
        Ok(())
    }
}

struct NoopSleepInhibitor;

/// Wake the display from sleep when remote input arrives on Linux/X11.
#[cfg(target_os = "linux")]
pub fn wake_display() {
    let _ = std::process::Command::new("xset")
        .args(["dpms", "force", "on"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "linux"))]
pub fn wake_display() {}
