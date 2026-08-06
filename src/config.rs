//! Configuration for microdns (`$DATA_DIR/etc/microdns.json`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::datadir;
use crate::error::{Error, Result};

/// Default config path under the data root.
#[must_use]
pub fn default_config_path() -> PathBuf {
    datadir::path(["etc", "microdns.json"])
}

/// microinit control socket under the data root.
#[must_use]
pub fn default_microinit_socket() -> PathBuf {
    datadir::path(["run", "microinit.sock"])
}

/// One static DNS-SD service entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEntry {
    /// DNS-SD instance name.
    pub name: String,
    /// Service type, e.g. `_http._tcp`.
    #[serde(rename = "type")]
    pub type_: String,
    /// Transport protocol (`tcp` / `udp`); informational alongside `type`.
    pub protocol: String,
    pub port: u16,
    /// Optional hostname without `.local` (e.g. `bigfred` → `bigfred.local.`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Optional TXT record key/value pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt: Option<HashMap<String, String>>,
}

/// Optional dcc-bus discovery / advertisement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DccBusConfig {
    /// Feature toggle; when false only static `services[]` are advertised.
    #[serde(default)]
    pub enabled: bool,
    /// Expected Z21 UDP listen port.
    #[serde(default = "default_z21_port")]
    pub z21_port: u16,
    /// Expected WiThrottle TCP listen port.
    #[serde(default = "default_withrottle_port")]
    pub withrottle_port: u16,
    /// Broadcast LAN_GET_SERIAL_NUMBER reply when Z21 port is listening.
    #[serde(default = "default_true")]
    pub beacon: bool,
}

impl Default for DccBusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            z21_port: default_z21_port(),
            withrottle_port: default_withrottle_port(),
            beacon: true,
        }
    }
}

fn default_z21_port() -> u16 {
    21105
}

fn default_withrottle_port() -> u16 {
    12090
}

fn default_true() -> bool {
    true
}

/// Quiet retry intervals (milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    #[serde(default = "default_microinit_ms")]
    pub microinit_ms: u64,
    #[serde(default = "default_proc_ms")]
    pub proc_ms: u64,
    #[serde(default = "default_mdns_ms")]
    pub mdns_ms: u64,
    #[serde(default = "default_iface_ms")]
    pub iface_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            microinit_ms: default_microinit_ms(),
            proc_ms: default_proc_ms(),
            mdns_ms: default_mdns_ms(),
            iface_ms: default_iface_ms(),
        }
    }
}

fn default_microinit_ms() -> u64 {
    2000
}
fn default_proc_ms() -> u64 {
    2000
}
fn default_mdns_ms() -> u64 {
    3000
}
fn default_iface_ms() -> u64 {
    5000
}

/// Top-level microdns configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub services: Vec<ServiceEntry>,
    #[serde(default)]
    pub dcc_bus: DccBusConfig,
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            services: vec![ServiceEntry {
                name: "bigfred".into(),
                type_: "_http._tcp".into(),
                protocol: "tcp".into(),
                port: 8080,
                host: Some("bigfred".into()),
                txt: Some(HashMap::from([("path".into(), "/".into())])),
            }],
            dcc_bus: DccBusConfig::default(),
            retry: RetryConfig::default(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        for svc in &self.services {
            if svc.name.is_empty() {
                return Err(Error::Config("service name must not be empty".into()));
            }
            if svc.type_.is_empty() {
                return Err(Error::Config(format!(
                    "service '{}': type must not be empty",
                    svc.name
                )));
            }
            if svc.port == 0 {
                return Err(Error::Config(format!(
                    "service '{}': port must be non-zero",
                    svc.name
                )));
            }
        }
        Ok(())
    }
}

/// Load config from `path`, creating a default file if missing.
pub fn load_or_create(path: &Path) -> Result<Config> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
    }

    if !path.exists() {
        let cfg = Config::default();
        save(path, &cfg)?;
        return Ok(cfg);
    }

    let data = fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
    let cfg: Config = serde_json::from_str(&data)?;
    cfg.validate()?;
    Ok(cfg)
}

/// Persist config as pretty JSON.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
    }
    let mut data = serde_json::to_string_pretty(cfg)?;
    data.push('\n');
    fs::write(path, data).map_err(|e| Error::io_at(path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("microdns-cfg-{name}-{nanos}.json"))
    }

    #[test]
    fn default_roundtrip() {
        let cfg = Config::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert!(!back.dcc_bus.enabled);
        assert_eq!(back.dcc_bus.z21_port, 21105);
        assert_eq!(back.retry.mdns_ms, 3000);
    }

    #[test]
    fn load_or_create_seeds_default() {
        let path = tmp_path("seed");
        let _ = fs::remove_file(&path);
        let cfg = load_or_create(&path).unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "bigfred");
        assert!(path.exists());
        let again = load_or_create(&path).unwrap();
        assert_eq!(cfg, again);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn type_field_renames() {
        let json = r#"{
            "services": [
                {"name":"x","type":"_http._tcp","protocol":"tcp","port":80}
            ]
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.services[0].type_, "_http._tcp");
    }
}
