use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct NexdeskConfig {
    /// This machine's hostname
    pub hostname: String,

    /// Port for the QUIC server
    pub port: u16,

    /// Role: "server" or "client"
    pub role: Option<String>,

    /// Server address (for client mode)
    pub server_addr: Option<String>,

    /// Screen edge that triggers switching (e.g. "right", "left")
    pub switch_edge: Option<String>,

    /// Trusted peer fingerprints
    #[serde(default)]
    pub trusted_fingerprints: Vec<String>,
}

impl NexdeskConfig {
    pub fn config_dir() -> Result<PathBuf> {
        let proj = ProjectDirs::from("com", "nexdesk", "nexdesk")
            .ok_or_else(|| color_eyre::eyre::eyre!("Cannot determine config directory"))?;
        let dir = proj.config_dir().to_path_buf();
        std::fs::create_dir_all(&dir)
            .wrap_err_with(|| format!("Failed to create config dir: {}", dir.display()))?;
        Ok(dir)
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn certs_dir() -> Result<PathBuf> {
        let dir = Self::config_dir()?.join("certs");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            let contents = std::fs::read_to_string(&path)
                .wrap_err("Failed to read config file")?;
            let config: NexdeskConfig = toml::from_str(&contents)
                .wrap_err("Failed to parse config file")?;
            Ok(config)
        } else {
            Ok(Self::default_config())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let contents = toml::to_string_pretty(self)
            .wrap_err("Failed to serialize config")?;
        std::fs::write(&path, contents)
            .wrap_err("Failed to write config file")?;
        Ok(())
    }

    fn default_config() -> Self {
        let hostname = gethostname::gethostname()
            .to_string_lossy()
            .into_owned();
        Self {
            hostname,
            port: 4242,
            role: None,
            server_addr: None,
            switch_edge: None,
            trusted_fingerprints: Vec::new(),
        }
    }
}
