use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use microdns::config::{load_or_create, Config};

fn tmp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("microdns-cfg-{name}-{nanos}.json"))
}

#[test]
fn bigfred_enabled_defaults_true_when_key_absent() {
    let json = r#"{"services":[],"dccBus":{"enabled":true}}"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    assert!(cfg.bigfred.enabled);
    assert!(cfg.dcc_bus.enabled);
    assert_eq!(cfg.retry.bigfred_ms, 45_000);
}

#[test]
fn default_roundtrip() {
    let cfg = Config::default();
    let json = serde_json::to_string_pretty(&cfg).unwrap();
    let back: Config = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, back);
    assert!(!back.dcc_bus.enabled);
    assert!(back.bigfred.enabled);
    assert_eq!(back.dcc_bus.z21_port, 21105);
    assert_eq!(back.retry.mdns_ms, 3000);
    assert_eq!(back.retry.bigfred_ms, 45_000);
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

/// The BigFred OS overlay ships `"skipInterfaces": ["wlan"]`, and nothing else
/// guards that JSON key: a rename would silently turn the hub's opt-out into a
/// no-op, since unknown fields are ignored.
#[test]
fn skip_interfaces_binds_to_the_camel_case_key() {
    let json = r#"{
            "services": [],
            "skipInterfaces": ["wlan"]
        }"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.skip_interfaces, vec!["wlan".to_string()]);
}

#[test]
fn skip_interfaces_defaults_to_empty_when_absent() {
    let json = r#"{"services": []}"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    assert!(
        cfg.skip_interfaces.is_empty(),
        "an existing config file must keep advertising on wlan*"
    );
}

#[test]
fn interfaces_defaults_to_empty_when_absent() {
    let json = r#"{"services": []}"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    assert!(
        cfg.interfaces.is_empty(),
        "empty interfaces means advertise on all usable ifaces"
    );
}

#[test]
fn interfaces_binds_to_the_camel_case_key() {
    let json = r#"{
            "services": [],
            "interfaces": ["eth", "enp"]
        }"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.interfaces, vec!["eth".to_string(), "enp".to_string()]);
}

#[test]
fn validate_rejects_duplicate_interfaces() {
    let cfg = Config {
        interfaces: vec!["eth".into(), "ETH".into()],
        ..Config::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_empty_interface_entry() {
    let cfg = Config {
        interfaces: vec!["  ".into()],
        ..Config::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_bad_dns_sd_type() {
    let mut cfg = Config::default();
    cfg.services[0].type_ = "_http._sctp".into();
    assert!(cfg.validate().is_err());
    cfg.services[0].type_ = "_http._tcp".into();
    cfg.services[0].protocol = "udp".into();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_duplicate_names() {
    let mut cfg = Config::default();
    cfg.services.push(cfg.services[0].clone());
    assert!(cfg.validate().is_err());
}
