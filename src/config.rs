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

/// BigFred loco-server control socket under the data root.
#[must_use]
pub fn default_bigfred_socket() -> PathBuf {
    datadir::path(["run", "bigfred.sock"])
}

/// Poll loco-server for dcc-bus programs and advertise their ports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BigFredConfig {
    /// Feature toggle; default **true** when the key is absent.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override socket path. Empty / omitted → `$DATA_DIR/run/bigfred.sock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

impl Default for BigFredConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: None,
        }
    }
}

impl BigFredConfig {
    /// Resolved control-socket path.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        match &self.socket {
            Some(s) if !s.trim().is_empty() => PathBuf::from(s),
            _ => default_bigfred_socket(),
        }
    }
}

/// Default microinit control socket under the data root.
#[must_use]
pub fn default_microinit_socket() -> PathBuf {
    datadir::path(["run", "microinit.sock"])
}

/// Watch microinit for services labeled `microdns-port` / `microdns-type`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MicroinitConfig {
    /// Feature toggle; default **true** when the key is absent.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override socket path. Empty / omitted → `$DATA_DIR/run/microinit.sock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

impl Default for MicroinitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: None,
        }
    }
}

impl MicroinitConfig {
    /// Resolved control-socket path.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        match &self.socket {
            Some(s) if !s.trim().is_empty() => PathBuf::from(s),
            _ => default_microinit_socket(),
        }
    }
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

/// Z21 LAN serial broadcast when a `_z21._udp` port is advertised.
/// Extra keys in existing JSON (`enabled`, port guesses) are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DccBusConfig {
    /// Broadcast LAN_GET_SERIAL_NUMBER reply when a Z21 port is advertised.
    #[serde(default = "default_true")]
    pub beacon: bool,
    /// Optional DNS-SD hostname without `.local` for dcc-bus ads.
    /// Absent/empty: mdns-sd uses the kernel hostname; the ctl table shows `-`.
    /// Product templates (BigFred OS / loco-server) set `"bigfred"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl Default for DccBusConfig {
    fn default() -> Self {
        Self {
            beacon: true,
            host: None,
        }
    }
}

impl DccBusConfig {
    /// Trimmed non-empty `host`, if configured.
    #[must_use]
    pub fn advertised_host(&self) -> Option<&str> {
        self.host
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

fn default_true() -> bool {
    true
}

/// Quiet retry intervals (milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    /// Poll interval while the BigFred socket is answering (milliseconds).
    /// `microinitMs` is accepted as an alias for existing JSON files.
    #[serde(default = "default_poll_ms", alias = "microinitMs")]
    pub poll_ms: u64,
    #[serde(default = "default_mdns_ms")]
    pub mdns_ms: u64,
    #[serde(default = "default_iface_ms")]
    pub iface_ms: u64,
    /// Retry interval when the BigFred socket is missing (milliseconds).
    #[serde(default = "default_bigfred_ms")]
    pub bigfred_ms: u64,
    /// Reconnect backoff while the microinit watch socket is down (milliseconds).
    /// Independent of `microinitMs`, which is a leftover alias for [`Self::poll_ms`].
    #[serde(default = "default_microinit_reconnect_ms")]
    pub microinit_reconnect_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            poll_ms: default_poll_ms(),
            mdns_ms: default_mdns_ms(),
            iface_ms: default_iface_ms(),
            bigfred_ms: default_bigfred_ms(),
            microinit_reconnect_ms: default_microinit_reconnect_ms(),
        }
    }
}

fn default_poll_ms() -> u64 {
    25_000
}
fn default_mdns_ms() -> u64 {
    3000
}
fn default_iface_ms() -> u64 {
    5000
}
fn default_bigfred_ms() -> u64 {
    45_000
}
fn default_microinit_reconnect_ms() -> u64 {
    3000
}

/// Unsolicited mDNS re-announcement (RFC 6762 §8.3 plus a periodic refresh).
///
/// `mdns-sd` only sends two announcements at register time. Clients that miss
/// those packets (or flush on a later goodbye) would never see the service
/// again without this ticker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnnounceConfig {
    /// Interval between full re-announcements of the current set (milliseconds).
    /// Default 55 s, below the 120 s host-record TTL.
    #[serde(default = "default_announce_period_ms")]
    pub period_ms: u64,
    /// Extra announcements after a real advertisement change, at 1 s, 2 s, 4 s, …
    /// `burstCount` 4 → 1 s, 2 s, 4 s, 8 s. Zero disables the burst.
    #[serde(default = "default_announce_burst_count")]
    pub burst_count: u8,
}

impl Default for AnnounceConfig {
    fn default() -> Self {
        Self {
            period_ms: default_announce_period_ms(),
            burst_count: default_announce_burst_count(),
        }
    }
}

fn default_announce_period_ms() -> u64 {
    55_000
}
fn default_announce_burst_count() -> u8 {
    4
}

/// Periodic self-verification of multicast membership and announcements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SelfCheckConfig {
    /// How often to verify IGMP membership and recent announcements (milliseconds).
    #[serde(default = "default_selfcheck_period_ms")]
    pub period_ms: u64,
}

impl Default for SelfCheckConfig {
    fn default() -> Self {
        Self {
            period_ms: default_selfcheck_period_ms(),
        }
    }
}

fn default_selfcheck_period_ms() -> u64 {
    60_000
}

/// Top-level microdns configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub services: Vec<ServiceEntry>,
    #[serde(default)]
    pub bigfred: BigFredConfig,
    #[serde(default)]
    pub microinit: MicroinitConfig,
    #[serde(default)]
    pub dcc_bus: DccBusConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub announce: AnnounceConfig,
    #[serde(default)]
    pub selfcheck: SelfCheckConfig,
    /// Extra interface name prefixes to skip (case-insensitive), in addition
    /// to the built-in docker/veth/br-*/... list. Empty by default so mDNS
    /// advertises on every usable interface (including `wlan*`) — operators
    /// who reserve the WiFi radio for another purpose (e.g. the BigFred hub,
    /// where `wireless-programmer` owns the radio) add `["wlan"]` here.
    #[serde(default)]
    pub skip_interfaces: Vec<String>,
    /// Optional allowlist of interface name prefixes (case-insensitive).
    /// Empty (default) means advertise on every usable interface that is not
    /// skipped. When non-empty, only matching interfaces are used; a listed
    /// interface that is temporarily missing logs a warning and is retried —
    /// it does not crash the daemon.
    #[serde(default)]
    pub interfaces: Vec<String>,
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
            bigfred: BigFredConfig::default(),
            microinit: MicroinitConfig::default(),
            dcc_bus: DccBusConfig::default(),
            retry: RetryConfig::default(),
            announce: AnnounceConfig::default(),
            selfcheck: SelfCheckConfig::default(),
            skip_interfaces: Vec::new(),
            interfaces: Vec::new(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for svc in &self.services {
            if svc.name.is_empty() {
                return Err(Error::Config("service name must not be empty".into()));
            }
            if !names.insert(svc.name.clone()) {
                return Err(Error::Config(format!(
                    "duplicate service name '{}'",
                    svc.name
                )));
            }
            validate_service_type(&svc.name, &svc.type_)?;
            validate_protocol(&svc.name, &svc.type_, &svc.protocol)?;
            if svc.port == 0 {
                return Err(Error::Config(format!(
                    "service '{}': port must be non-zero",
                    svc.name
                )));
            }
        }
        validate_iface_prefixes("skipInterfaces", &self.skip_interfaces)?;
        validate_iface_prefixes("interfaces", &self.interfaces)?;
        if self.announce.period_ms < 1000 {
            return Err(Error::Config(
                "announce.periodMs must be at least 1000".into(),
            ));
        }
        if self.announce.burst_count > 8 {
            return Err(Error::Config(
                "announce.burstCount must be at most 8".into(),
            ));
        }
        if self.selfcheck.period_ms < 1000 {
            return Err(Error::Config(
                "selfcheck.periodMs must be at least 1000".into(),
            ));
        }
        Ok(())
    }
}

fn validate_iface_prefixes(field: &str, entries: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(Error::Config(format!("{field}: entries must not be empty")));
        }
        let key = trimmed.to_ascii_lowercase();
        if !seen.insert(key) {
            return Err(Error::Config(format!(
                "{field}: duplicate prefix '{trimmed}'"
            )));
        }
    }
    Ok(())
}

/// Accept `_name._tcp` / `_name._udp`, optionally with a `.local` suffix.
pub(crate) fn validate_service_type(name: &str, type_: &str) -> Result<()> {
    if type_.is_empty() {
        return Err(Error::Config(format!(
            "service '{name}': type must not be empty"
        )));
    }
    let base = type_
        .trim()
        .trim_end_matches('.')
        .strip_suffix(".local")
        .unwrap_or(type_.trim().trim_end_matches('.'));
    let mut parts = base.split('.');
    let (Some(svc), Some(trans), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Error::Config(format!(
            "service '{name}': type '{type_}' must look like _http._tcp or _z21._udp"
        )));
    };
    if !svc.starts_with('_') || svc.len() < 2 {
        return Err(Error::Config(format!(
            "service '{name}': type '{type_}' service label must start with '_'"
        )));
    }
    if trans != "_tcp" && trans != "_udp" {
        return Err(Error::Config(format!(
            "service '{name}': type '{type_}' transport must be _tcp or _udp"
        )));
    }
    Ok(())
}

fn validate_protocol(name: &str, type_: &str, protocol: &str) -> Result<()> {
    let proto = protocol.trim().to_ascii_lowercase();
    if proto != "tcp" && proto != "udp" {
        return Err(Error::Config(format!(
            "service '{name}': protocol must be tcp or udp"
        )));
    }
    let base = type_
        .trim()
        .trim_end_matches('.')
        .strip_suffix(".local")
        .unwrap_or(type_.trim().trim_end_matches('.'));
    let expected = if base.ends_with("._tcp") {
        "tcp"
    } else if base.ends_with("._udp") {
        "udp"
    } else {
        return Ok(());
    };
    if proto != expected {
        return Err(Error::Config(format!(
            "service '{name}': protocol '{protocol}' does not match type '{type_}'"
        )));
    }
    Ok(())
}

/// `tcp` / `udp` from a validated DNS-SD type, or `None` if the type is unusable.
#[must_use]
pub(crate) fn protocol_from_type(type_: &str) -> Option<&'static str> {
    let base = type_
        .trim()
        .trim_end_matches('.')
        .strip_suffix(".local")
        .unwrap_or(type_.trim().trim_end_matches('.'));
    if base.ends_with("._tcp") {
        Some("tcp")
    } else if base.ends_with("._udp") {
        Some("udp")
    } else {
        None
    }
}

/// Load config from `path`, creating a default file if missing.
pub fn load_or_create(path: &Path) -> Result<Config> {
    use dcc_daemon::config::Load;
    let cfg = dcc_daemon::config::JsonFile::<Config>::new(path)
        .create_default()
        .load()
        .map_err(map_config)?;
    cfg.validate()?;
    Ok(cfg)
}

fn map_config(e: dcc_daemon::config::ConfigError) -> Error {
    match e {
        dcc_daemon::config::ConfigError::Io { path, source } => {
            Error::io_at(PathBuf::from(path), source)
        }
        dcc_daemon::config::ConfigError::Json(j) => Error::Json(j),
        dcc_daemon::config::ConfigError::Other(s) => Error::Other(s),
    }
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
