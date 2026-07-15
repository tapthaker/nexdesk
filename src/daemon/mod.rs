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
        Err(color_eyre::eyre::eyre!(
            "Unsupported platform for daemon installation"
        ))
    }
}

/// Start the previously installed daemon/service.
pub fn start_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::start()
    }

    #[cfg(target_os = "linux")]
    {
        systemd::start()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!(
            "Unsupported platform for starting the daemon"
        ))
    }
}

/// Stop the installed daemon/service.
pub fn stop_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::stop()
    }

    #[cfg(target_os = "linux")]
    {
        systemd::stop()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!(
            "Unsupported platform for stopping the daemon"
        ))
    }
}

/// Print a short daemon/process/listener status summary.
pub fn print_status() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::print_status()
    }

    #[cfg(target_os = "linux")]
    {
        systemd::print_status()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!(
            "Unsupported platform for daemon status"
        ))
    }
}

/// Print detailed daemon diagnostics and recent logs.
pub fn print_log() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchagent::print_log()
    }

    #[cfg(target_os = "linux")]
    {
        systemd::print_log()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(color_eyre::eyre::eyre!(
            "Unsupported platform for daemon logs"
        ))
    }
}
