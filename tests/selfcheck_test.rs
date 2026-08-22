use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use microdns::legacy_unicast::IfaceAddr4;
use microdns::mdns::AnnounceLog;
use microdns::selfcheck::{missing_mdns_membership, parse_igmp, stale_announces, MDNS_GROUP_V4};

const SAMPLE_IGMP: &str = "\
Idx\tDevice    : Count Querier\tGroup    Users Timer    Reporter
1\tlo        :     1      V3
\t\t\t\t010000E0     1 0:00000000\t0
2\teth0      :     2      V3
\t\t\t\tFB0000E0     1 0:00000000\t0
\t\t\t\t010000E0     1 0:00000000\t0
3\twlan0     :     1      V3
\t\t\t\t010000E0     1 0:00000000\t0
";

#[test]
fn parse_igmp_finds_mdns_group_on_eth0() {
    let groups = parse_igmp(SAMPLE_IGMP);
    assert!(groups["eth0"].contains(&MDNS_GROUP_V4));
    assert!(!groups["wlan0"].contains(&MDNS_GROUP_V4));
    assert!(!groups["lo"].contains(&MDNS_GROUP_V4));
}

#[test]
fn missing_membership_lists_ifaces_without_group() {
    let groups = parse_igmp(SAMPLE_IGMP);
    let want = vec![
        IfaceAddr4 {
            iface: "eth0".into(),
            addr: Ipv4Addr::new(192, 168, 0, 1),
            mask: Ipv4Addr::new(255, 255, 255, 0),
            ifindex: 2,
        },
        IfaceAddr4 {
            iface: "wlan0".into(),
            addr: Ipv4Addr::new(192, 168, 1, 1),
            mask: Ipv4Addr::new(255, 255, 255, 0),
            ifindex: 3,
        },
    ];
    assert_eq!(
        missing_mdns_membership(&groups, &want),
        vec!["wlan0".to_string()]
    );
}

#[test]
fn stale_announces_when_missing_or_old() {
    let log = AnnounceLog::new();
    log.record("fresh._http._tcp.local.");
    let now = Instant::now();
    let expected = vec![
        "fresh._http._tcp.local.".into(),
        "gone._http._tcp.local.".into(),
    ];
    let stale = stale_announces(&expected, &log, Duration::from_secs(60), now);
    assert_eq!(stale, vec!["gone._http._tcp.local.".to_string()]);
}
