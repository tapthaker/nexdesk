use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistenceRoots {
    config_root: PathBuf,
}

impl PersistenceRoots {
    pub fn production() -> Result<Self> {
        let proj = ProjectDirs::from("com", "nexdesk", "nexdesk")
            .ok_or_else(|| color_eyre::eyre::eyre!("Cannot determine config directory"))?;
        Ok(Self::from_config_root(proj.config_dir()))
    }

    pub fn from_config_root(root: impl Into<PathBuf>) -> Self {
        Self {
            config_root: root.into(),
        }
    }

    pub fn config_root(&self) -> &std::path::Path {
        &self.config_root
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_root.join("config.toml")
    }

    pub fn certificates_dir(&self) -> PathBuf {
        self.config_root.join("certs")
    }

    pub fn status_path(&self) -> PathBuf {
        self.config_root.join("runtime-status.json")
    }

    pub fn ensure_config_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_root).wrap_err_with(|| {
            format!(
                "Failed to create config dir: {}",
                self.config_root.display()
            )
        })
    }
}

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
        let roots = PersistenceRoots::production()?;
        roots.ensure_config_root()?;
        Ok(roots.config_root().to_path_buf())
    }

    pub fn config_path() -> Result<PathBuf> {
        let roots = PersistenceRoots::production()?;
        roots.ensure_config_root()?;
        Ok(roots.config_path())
    }

    pub fn certs_dir() -> Result<PathBuf> {
        let roots = PersistenceRoots::production()?;
        roots.ensure_config_root()?;
        let dir = roots.certificates_dir();
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&PersistenceRoots::production()?)
    }

    pub fn load_from(roots: &PersistenceRoots) -> Result<Self> {
        roots.ensure_config_root()?;
        let path = roots.config_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).wrap_err("Failed to read config file")?;
            let config: NexdeskConfig =
                toml::from_str(&contents).wrap_err("Failed to parse config file")?;
            Ok(config)
        } else {
            Ok(Self::default_config())
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&PersistenceRoots::production()?)
    }

    pub fn save_to(&self, roots: &PersistenceRoots) -> Result<()> {
        roots.ensure_config_root()?;
        let path = roots.config_path();
        let contents = toml::to_string_pretty(self).wrap_err("Failed to serialize config")?;
        std::fs::write(&path, contents).wrap_err("Failed to write config file")?;
        Ok(())
    }

    fn default_config() -> Self {
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_roots_keep_config_certificates_and_status_in_temp_root() {
        let temp = tempfile::tempdir().unwrap();
        let roots = PersistenceRoots::from_config_root(temp.path());
        let config = NexdeskConfig {
            hostname: "test-host".to_string(),
            port: 4242,
            ..NexdeskConfig::default()
        };

        config.save_to(&roots).unwrap();
        let loaded = NexdeskConfig::load_from(&roots).unwrap();

        assert_eq!(loaded.hostname, "test-host");
        assert_eq!(roots.config_path(), temp.path().join("config.toml"));
        assert_eq!(roots.certificates_dir(), temp.path().join("certs"));
        assert_eq!(roots.status_path(), temp.path().join("runtime-status.json"));
    }
}
