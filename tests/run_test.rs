use std::collections::HashSet;

use microdns::proc_scan::ListenPorts;
use microdns::run::{append_station_ads, pick_tcp_port, pick_udp_port, BeaconWant, DesiredAds};

#[test]
fn pick_ports_prefer_configured() {
    let mut ports = ListenPorts::default();
    ports.udp.extend([21105, 21106]);
    ports.tcp.extend([12090, 12091]);
    assert_eq!(pick_udp_port(&ports, 21106), Some(21106));
    assert_eq!(pick_tcp_port(&ports, 12091), Some(12091));
}

#[test]
fn pick_ports_fallback_range() {
    let mut ports = ListenPorts::default();
    ports.udp.insert(21150);
    ports.tcp.insert(12095);
    assert_eq!(pick_udp_port(&ports, 21105), Some(21150));
    assert_eq!(pick_tcp_port(&ports, 12090), Some(12095));
}

#[test]
fn append_station_builds_identity() {
    let mut desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        skip_interfaces: Vec::new(),
    };
    let mut ports = ListenPorts {
        tcp: HashSet::new(),
        udp: HashSet::new(),
    };
    ports.udp.insert(21106);
    ports.tcp.insert(12091);
    append_station_ads(&mut desired, "dcc-bus-2-5", &ports, 21105, 12090, true);
    assert_eq!(desired.dynamic.len(), 2);
    assert_eq!(desired.dynamic[0].entry.name, "BigFred #5");
    assert_eq!(desired.dynamic[0].entry.port, 21106);
    let txt = desired.dynamic[0].entry.txt.as_ref().unwrap();
    assert_eq!(txt.get("layoutId").map(String::as_str), Some("2"));
    assert_eq!(txt.get("commandStationId").map(String::as_str), Some("5"));
    assert_eq!(txt.get("serial").map(String::as_str), Some("258002005"));
    assert_eq!(
        desired.beacons,
        vec![BeaconWant {
            port: 21106,
            serial: 258_002_005
        }]
    );
}
