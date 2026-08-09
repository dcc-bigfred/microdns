use microdns::mdns::{
    dcc_service_entry, normalize_hostname, normalize_service_type, should_skip_iface,
};

#[test]
fn normalize_type() {
    assert_eq!(normalize_service_type("_http._tcp"), "_http._tcp.local.");
    assert_eq!(
        normalize_service_type("_http._tcp.local"),
        "_http._tcp.local."
    );
    assert_eq!(
        normalize_service_type("_http._tcp.local."),
        "_http._tcp.local."
    );
}

#[test]
fn normalize_host() {
    assert_eq!(normalize_hostname("bigfred"), "bigfred.local.");
    assert_eq!(normalize_hostname("bigfred.local"), "bigfred.local.");
    assert_eq!(normalize_hostname("bigfred.local."), "bigfred.local.");
}

#[test]
fn skip_virtual_ifaces() {
    // Built-in virtual/container interfaces are always skipped.
    assert!(should_skip_iface("veth0abc", &[]));
    assert!(should_skip_iface("br-1234abcd", &[]));
    assert!(should_skip_iface("docker0", &[]));
    assert!(!should_skip_iface("eth0", &[]));
    assert!(!should_skip_iface("enp1s0", &[]));
    // wlan* is NOT skipped by default (mDNS advertises on WiFi on a laptop).
    assert!(!should_skip_iface("wlan0", &[]));
    assert!(!should_skip_iface("WLAN0", &[]));
    // ...but is skipped when configured (e.g. the BigFred hub reserves the
    // radio for wireless-programmer). Matching is by name prefix.
    assert!(should_skip_iface("wlan0", &["wlan".into()]));
    assert!(should_skip_iface("WLAN0", &["WLAN".into()]));
    assert!(should_skip_iface("wlan0", &["wlan".into(), "wlp".into()]));
    assert!(should_skip_iface("wlp3s0", &["wlan".into(), "wlp".into()]));
    // An empty/blank entry in the skip list matches nothing.
    assert!(!should_skip_iface("wlan0", &["".into()]));
    assert!(!should_skip_iface("eth0", &["wlan".into()]));
    assert!(!should_skip_iface("wlp3s0", &["wlan".into()]));
}

#[test]
fn dcc_entry_has_proto_txt() {
    let e = dcc_service_entry("hub1", "_z21._udp", "udp", 21105, 2, 5, Some(258_002_005));
    let txt = e.txt.as_ref().unwrap();
    assert_eq!(txt.get("proto").unwrap(), "udp");
    assert_eq!(txt.get("layoutId").unwrap(), "2");
    assert_eq!(txt.get("commandStationId").unwrap(), "5");
    assert_eq!(txt.get("serial").unwrap(), "258002005");
}
