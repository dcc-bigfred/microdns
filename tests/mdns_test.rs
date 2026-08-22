use microdns::mdns::{
    dcc_service_entry, iface_link_ready, iface_name_relevant, is_allowed_iface, normalize_hostname,
    normalize_service_type, preferred_ipv4_ifaces, preferred_ipv6_addrs, should_skip_iface,
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
fn allowlist_empty_means_all() {
    assert!(is_allowed_iface("eth0", &[]));
    assert!(is_allowed_iface("wlan0", &[]));
}

#[test]
fn allowlist_prefix_match() {
    assert!(is_allowed_iface("eth0", &["eth".into()]));
    assert!(is_allowed_iface("ETH0", &["eth".into()]));
    assert!(!is_allowed_iface("wlan0", &["eth".into()]));
    assert!(!is_allowed_iface("eth0", &["".into()]));
}

#[test]
fn link_ready_requires_running_or_operstate_up() {
    // IFF_UP alone after suspend is not a live link.
    assert!(!iface_link_ready(libc::IFF_UP as u32, "down"));
    assert!(!iface_link_ready(libc::IFF_UP as u32, "dormant"));
    assert!(!iface_link_ready(libc::IFF_UP as u32, "unknown"));
    // IFF_RUNNING is sufficient even if operstate is unknown (dummy).
    assert!(iface_link_ready(libc::IFF_RUNNING as u32, "unknown"));
    assert!(iface_link_ready(
        (libc::IFF_UP | libc::IFF_RUNNING) as u32,
        "down"
    ));
    // operstate=up covers drivers that omit IFF_RUNNING.
    assert!(iface_link_ready(libc::IFF_UP as u32, "up"));
    assert!(iface_link_ready(0, "up"));
    assert!(iface_link_ready(0, "UP"));
    assert!(!iface_link_ready(0, "down"));
}

#[test]
fn dcc_entry_has_proto_txt() {
    let e = dcc_service_entry(
        "hub1",
        "_z21._udp",
        "udp",
        21105,
        2,
        5,
        "Klubowa",
        Some(258_002_005),
        None,
    );
    let txt = e.txt.as_ref().unwrap();
    assert_eq!(txt.get("proto").unwrap(), "udp");
    assert_eq!(txt.get("layoutId").unwrap(), "2");
    assert_eq!(txt.get("commandStationId").unwrap(), "5");
    assert_eq!(txt.get("layoutName").unwrap(), "Klubowa");
    assert_eq!(txt.get("serial").unwrap(), "258002005");
    assert_eq!(e.host, None);
}

#[test]
fn dcc_entry_keeps_configured_host() {
    let e = dcc_service_entry(
        "hub1",
        "_withrottle._tcp",
        "tcp",
        12090,
        1,
        1,
        "",
        None,
        Some("bigfred"),
    );
    assert_eq!(e.host.as_deref(), Some("bigfred"));
}

#[test]
fn dcc_entry_blank_host_is_none() {
    let e = dcc_service_entry(
        "hub1",
        "_withrottle._tcp",
        "tcp",
        12090,
        1,
        1,
        "",
        None,
        Some("  "),
    );
    assert_eq!(e.host, None);
}

#[test]
fn iface_name_relevant_skips_lo_docker_and_configured() {
    assert!(!iface_name_relevant("lo", &[], &[]));
    assert!(!iface_name_relevant("docker0", &[], &[]));
    assert!(!iface_name_relevant("wlan0", &[], &["wlan".into()]));
    assert!(iface_name_relevant("eth0", &[], &["wlan".into()]));
    assert!(iface_name_relevant("wlan0", &[], &[]));
}

#[test]
fn preferred_addrs_are_sorted_and_stable() {
    let a = preferred_ipv4_ifaces(&[], &[]);
    let b = preferred_ipv4_ifaces(&[], &[]);
    assert_eq!(a, b, "IPv4 iface list must be deterministic");
    let v6a = preferred_ipv6_addrs(&[], &[]);
    let v6b = preferred_ipv6_addrs(&[], &[]);
    assert_eq!(v6a, v6b, "IPv6 addr list must be deterministic");
    for window in a.windows(2) {
        assert!(
            (&window[0].iface, &window[0].addr) <= (&window[1].iface, &window[1].addr),
            "IPv4 list not sorted: {a:?}"
        );
    }
}
