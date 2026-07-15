use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_SERVER_ADDR_BYTES: usize = 512;
const MAX_SWITCH_EDGE_BYTES: usize = 64;
pub const MAX_TRUSTED_FINGERPRINTS: usize = 1024;

#[cfg(unix)]
fn restrict_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).wrap_err_with(|| {
        format!(
            "Failed to restrict directory permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .wrap_err_with(|| format!("Failed to restrict file permissions: {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn read_bounded_to_string(path: &Path, max_bytes: usize) -> Result<String> {
    let file = std::fs::File::open(path)
        .wrap_err_with(|| format!("Failed to open config file: {}", path.display()))?;
    let mut limited = file.take(max_bytes as u64 + 1);
    let mut contents = String::new();
    limited
        .read_to_string(&mut contents)
        .wrap_err_with(|| format!("Failed to read config file: {}", path.display()))?;
    if contents.len() > max_bytes {
        return Err(color_eyre::eyre::eyre!(
            "Config file too large: {} bytes (max {})",
            contents.len(),
            max_bytes
        ));
    }
    Ok(contents)
}

fn ensure_config_size(contents: &str) -> Result<()> {
    if contents.len() > MAX_CONFIG_BYTES {
        return Err(color_eyre::eyre::eyre!(
            "Config payload too large: {} bytes (max {})",
            contents.len(),
            MAX_CONFIG_BYTES
        ));
    }
    Ok(())
}

fn sanitize_hostname(value: &str) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { '�' } else { ch };
        let len = ch.len_utf8();
        if sanitized.len().saturating_add(len) > crate::net::protocol::MAX_PEER_NAME_BYTES {
            break;
        }
        sanitized.push(ch);
    }
    if sanitized.is_empty() {
        "nexdesk".to_string()
    } else {
        sanitized
    }
}

fn default_hostname() -> String {
    sanitize_hostname(&gethostname::gethostname().to_string_lossy())
}

fn default_port() -> u16 {
    4242
}

fn normalize_optional_address(value: Option<String>) -> Option<String> {
    let value = value?;
    if value.chars().any(char::is_control) {
        return None;
    }
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SERVER_ADDR_BYTES {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_optional_role(value: Option<String>) -> Option<String> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "server" => Some("server".to_string()),
        "client" => Some("client".to_string()),
        _ => None,
    }
}

fn normalize_optional_switch_edge(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut normalized = String::new();
    for ch in trimmed.chars().flat_map(char::to_lowercase) {
        let ch = if ch.is_control() { '�' } else { ch };
        let len = ch.len_utf8();
        if normalized.len().saturating_add(len) > MAX_SWITCH_EDGE_BYTES {
            break;
        }
        normalized.push(ch);
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn normalize_trusted_fingerprint(value: &str) -> Option<String> {
    let hex: String = value
        .trim()
        .chars()
        .filter(|c| *c != ':')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        hex.as_bytes()
            .chunks(2)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn normalize_trusted_fingerprints(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let Some(fp) = normalize_trusted_fingerprint(&value) else {
            continue;
        };
        if seen.insert(fp.clone()) {
            normalized.push(fp);
            if normalized.len() >= MAX_TRUSTED_FINGERPRINTS {
                break;
            }
        }
    }
    normalized
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NexdeskConfig {
    /// This machine's hostname
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Port for the QUIC server
    #[serde(default = "default_port")]
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
        restrict_dir_permissions(&dir)?;
        Ok(dir)
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn certs_dir() -> Result<PathBuf> {
        let dir = Self::config_dir()?.join("certs");
        std::fs::create_dir_all(&dir)?;
        restrict_dir_permissions(&dir)?;
        Ok(dir)
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            restrict_file_permissions(&path)?;
            let contents = read_bounded_to_string(&path, MAX_CONFIG_BYTES)?;
            let mut config: NexdeskConfig =
                toml::from_str(&contents).wrap_err("Failed to parse config file")?;
            config.fill_runtime_defaults();
            Ok(config)
        } else {
            Ok(Self::default_config())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let config = self.normalized_for_persistence();
        let contents = toml::to_string_pretty(&config).wrap_err("Failed to serialize config")?;
        ensure_config_size(&contents)?;
        let config_dir = Self::config_dir()?;
        let mut tmp_file = tempfile::Builder::new()
            .prefix("config.toml.")
            .tempfile_in(&config_dir)
            .wrap_err_with(|| {
                format!(
                    "Failed to create temporary config file in {}",
                    config_dir.display()
                )
            })?;
        restrict_file_permissions(tmp_file.path())?;
        tmp_file.write_all(contents.as_bytes()).wrap_err_with(|| {
            format!(
                "Failed to write temporary config file: {}",
                tmp_file.path().display()
            )
        })?;
        tmp_file
            .as_file_mut()
            .sync_all()
            .wrap_err("Failed to sync temporary config file")?;
        tmp_file
            .persist(&path)
            .map_err(|e| e.error)
            .wrap_err_with(|| format!("Failed to replace config file: {}", path.display()))?;
        sync_directory(&config_dir).wrap_err_with(|| {
            format!(
                "Failed to sync config directory after save: {}",
                config_dir.display()
            )
        })?;
        Ok(())
    }

    fn default_config() -> Self {
        Self {
            hostname: default_hostname(),
            port: default_port(),
            role: None,
            server_addr: None,
            switch_edge: None,
            trusted_fingerprints: Vec::new(),
        }
    }

    fn fill_runtime_defaults(&mut self) {
        if self.hostname.is_empty() {
            self.hostname = default_hostname();
        } else {
            self.hostname = sanitize_hostname(&self.hostname);
        }
        if self.port == 0 {
            self.port = default_port();
        }
        self.role = normalize_optional_role(self.role.take());
        self.server_addr = normalize_optional_address(self.server_addr.take());
        self.switch_edge = normalize_optional_switch_edge(self.switch_edge.take());
        self.trusted_fingerprints =
            normalize_trusted_fingerprints(std::mem::take(&mut self.trusted_fingerprints));
    }

    fn normalized_for_persistence(&self) -> Self {
        let mut config = self.clone();
        config.fill_runtime_defaults();
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_config_reader_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, vec![b'a'; 17]).unwrap();
        assert_eq!(read_bounded_to_string(&path, 17).unwrap().len(), 17);
        assert!(read_bounded_to_string(&path, 16).is_err());
    }

    #[test]
    fn config_size_check_rejects_oversized_payloads() {
        assert!(ensure_config_size(&"x".repeat(MAX_CONFIG_BYTES)).is_ok());
        assert!(ensure_config_size(&"x".repeat(MAX_CONFIG_BYTES + 1)).is_err());
    }

    #[test]
    fn hostname_normalization_is_protocol_safe() {
        assert_eq!(sanitize_hostname("host\nname"), "host�name");
        assert_eq!(sanitize_hostname(""), "nexdesk");
        assert_eq!(
            sanitize_hostname(&"a".repeat(crate::net::protocol::MAX_PEER_NAME_BYTES + 1)).len(),
            crate::net::protocol::MAX_PEER_NAME_BYTES
        );
    }

    #[test]
    fn config_deserializes_older_files_with_missing_defaults() {
        let mut config: NexdeskConfig = toml::from_str("role = 'server'\n").unwrap();
        config.fill_runtime_defaults();
        assert_eq!(config.role.as_deref(), Some("server"));
        assert!(!config.hostname.is_empty());
        assert_eq!(config.port, 4242);
        assert!(config.trusted_fingerprints.is_empty());
    }

    #[test]
    fn config_normalizes_zero_port_to_default() {
        let mut config: NexdeskConfig = toml::from_str("hostname = ''\nport = 0\n").unwrap();
        config.fill_runtime_defaults();
        assert!(!config.hostname.is_empty());
        assert_eq!(config.port, 4242);
    }

    #[test]
    fn config_normalizes_server_addr_for_service_use() {
        assert_eq!(
            normalize_optional_address(Some("  example.local:4242  ".into())).as_deref(),
            Some("example.local:4242")
        );
        assert!(normalize_optional_address(Some("\n".into())).is_none());
        assert!(normalize_optional_address(Some("host\nname".into())).is_none());
        assert!(normalize_optional_address(Some("host:4242\n".into())).is_none());
        assert!(normalize_optional_address(Some("x".repeat(MAX_SERVER_ADDR_BYTES + 1))).is_none());
    }

    #[test]
    fn config_normalizes_role_for_service_use() {
        assert_eq!(
            normalize_optional_role(Some(" SERVER \n".into())).as_deref(),
            Some("server")
        );
        assert_eq!(
            normalize_optional_role(Some("client".into())).as_deref(),
            Some("client")
        );
        assert!(normalize_optional_role(Some("connect".into())).is_none());
        assert!(normalize_optional_role(Some("".into())).is_none());
    }

    #[test]
    fn config_normalizes_switch_edge_without_hiding_invalid_values() {
        assert_eq!(
            normalize_optional_switch_edge(Some(" Right \n".into())).as_deref(),
            Some("right")
        );
        assert_eq!(
            normalize_optional_switch_edge(Some("bad\x1b[31m".into())).as_deref(),
            Some("bad�[31m")
        );
        assert_eq!(
            normalize_optional_switch_edge(Some("x".repeat(MAX_SWITCH_EDGE_BYTES + 1)))
                .unwrap()
                .len(),
            MAX_SWITCH_EDGE_BYTES
        );
        assert!(normalize_optional_switch_edge(Some("  ".into())).is_none());
    }

    #[test]
    fn config_is_normalized_before_persistence() {
        let config = NexdeskConfig {
            hostname: "bad\nhost".into(),
            port: 0,
            role: Some("invalid".into()),
            server_addr: Some("host:4242\n".into()),
            switch_edge: Some(" RIGHT\x1b[31m".into()),
            trusted_fingerprints: vec![],
        };
        let normalized = config.normalized_for_persistence();
        assert_eq!(normalized.hostname, "bad�host");
        assert_eq!(normalized.port, 4242);
        assert_eq!(normalized.role, None);
        assert_eq!(normalized.server_addr, None);
        assert_eq!(normalized.switch_edge.as_deref(), Some("right�[31m"));
    }

    #[test]
    fn trusted_fingerprints_are_normalized_deduped_and_bounded() {
        let first = "aa".repeat(32);
        let second = "bb".repeat(32);
        let mut values = vec![
            first.clone(),
            first.to_ascii_uppercase(),
            "not-a-fingerprint".into(),
        ];
        values.extend(std::iter::repeat_n(
            second.clone(),
            MAX_TRUSTED_FINGERPRINTS + 10,
        ));
        let normalized = normalize_trusted_fingerprints(values);
        assert_eq!(normalized.len(), 2);
        assert_eq!(
            normalized[0],
            normalize_trusted_fingerprint(&first).unwrap()
        );
        assert_eq!(
            normalized[1],
            normalize_trusted_fingerprint(&second).unwrap()
        );

        let many = (0..MAX_TRUSTED_FINGERPRINTS + 10)
            .map(|idx| format!("{idx:064X}"))
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_trusted_fingerprints(many).len(),
            MAX_TRUSTED_FINGERPRINTS
        );
    }
}
