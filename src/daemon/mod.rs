#[cfg(target_os = "macos")]
pub mod launchagent;

#[cfg(target_os = "linux")]
pub mod systemd;

use color_eyre::eyre::Result;

/// Install nexdesk as a system daemon/service.
pub fn install_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::install()
    }

    #[cfg(target_os = "linux")]
    {
        systemd::install()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!("Unsupported platform for daemon installation"))
    }
}
