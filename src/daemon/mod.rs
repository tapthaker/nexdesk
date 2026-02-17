#[cfg(target_os = "macos")]
pub mod launchagent;

#[cfg(target_os = "linux")]
pub mod systemd;

use color_eyre::eyre::Result;

/// Install nexdesk as a system daemon/service.
/// `args` specifies the subcommand and arguments, e.g. `["serve"]` or `["connect", "192.168.1.50:4242"]`.
pub fn install_service(args: &[&str]) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::install(args)
    }

    #[cfg(target_os = "linux")]
    {
        systemd::install(args)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = args;
        Err(color_eyre::eyre::eyre!("Unsupported platform for daemon installation"))
    }
}
