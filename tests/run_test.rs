use microdns::bigfred_watch::{instance_name, DccBusList, Program, Request};
use microdns::config::ServiceEntry;
use microdns::mdns::MdnsPublisher;
use microdns::run::{
    announce_burst_delays, append_station_ads, bigfred_backoff_ms, plan_reconcile, BeaconWant,
    DesiredAds, ReconcileAction,
};
use std::collections::HashMap;
use std::time::Duration;

fn program_running_wit() -> Program {
    Program {
        layout_id: 2,
        layout_name: "Klubowa".into(),
        name: "dcc-bus-2-5".into(),
        status: "running".into(),
        running: true,
        command_station_id: 5,
        withrottle_enabled: true,
        withrottle_port: 12091,
        z21_enabled: true,
        z21_port: 21106,
    }
}

#[test]
fn request_serializes_snake_case() {
    let l = serde_json::to_string(&Request::DccBusList).unwrap();
    assert_eq!(l, r#"{"type":"dcc_bus_list"}"#);
}

#[test]
fn instance_name_format() {
    assert_eq!(instance_name(5), "BigFred #5");
}

#[test]
fn parses_dcc_bus_list_json() {
    let json = r#"{
        "programs": [{
            "layoutId": 2,
            "layoutName": "Klubowa",
            "name": "dcc-bus-2-5",
            "status": "running",
            "running": true,
            "commandStationId": 5,
            "withrottleEnabled": true,
            "withrottlePort": 12091,
            "z21Enabled": false,
            "z21Port": 21105
        }]
    }"#;
    let body: DccBusList = serde_json::from_str(json).unwrap();
    assert_eq!(body.programs.len(), 1);
    assert_eq!(body.programs[0].withrottle_port, 12091);
    assert!(!body.programs[0].z21_enabled);
}

#[test]
fn append_station_uses_api_ports_not_defaults() {
    let mut desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        ips_v6: Vec::new(),
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    };
    append_station_ads(&mut desired, &program_running_wit(), true, None);
    assert_eq!(desired.dynamic.len(), 2);
    let wit = desired
        .dynamic
        .iter()
        .find(|d| d.entry.type_ == "_withrottle._tcp")
        .unwrap();
    assert_eq!(wit.entry.name, "BigFred #5");
    assert_eq!(wit.entry.port, 12091);
    assert_eq!(wit.source, microdns::run::DynSource::DccBus);
    let txt = wit.entry.txt.as_ref().unwrap();
    assert_eq!(txt.get("layoutId").map(String::as_str), Some("2"));
    assert_eq!(txt.get("layoutName").map(String::as_str), Some("Klubowa"));
    assert_eq!(txt.get("commandStationId").map(String::as_str), Some("5"));
    assert_eq!(wit.entry.host, None);
    assert_eq!(
        desired.beacons,
        vec![BeaconWant {
            port: 21106,
            serial: 258_002_005
        }]
    );
}

#[test]
fn append_station_sets_configured_host() {
    let mut desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        ips_v6: Vec::new(),
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    };
    append_station_ads(&mut desired, &program_running_wit(), false, Some("bigfred"));
    assert!(desired
        .dynamic
        .iter()
        .all(|d| d.entry.host.as_deref() == Some("bigfred")));
}

#[test]
fn append_station_skips_stopped_and_disabled() {
    let mut desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        ips_v6: Vec::new(),
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    };
    let mut stopped = program_running_wit();
    stopped.running = false;
    append_station_ads(&mut desired, &stopped, true, None);
    assert!(desired.dynamic.is_empty());

    let mut no_wit = program_running_wit();
    no_wit.withrottle_enabled = false;
    no_wit.z21_enabled = false;
    append_station_ads(&mut desired, &no_wit, true, None);
    assert!(desired.dynamic.is_empty());
}

#[test]
fn plan_reconcile_refresh_does_not_drop() {
    let key = MdnsPublisher::fullname("bigfred", "_http._tcp");
    let entry = ServiceEntry {
        name: "bigfred".into(),
        type_: "_http._tcp".into(),
        protocol: "tcp".into(),
        port: 8080,
        host: Some("bigfred".into()),
        txt: None,
    };
    let mut desired = HashMap::new();
    desired.insert(key.clone(), entry.clone());
    let mut registered = HashMap::new();
    registered.insert(key.clone(), entry);

    let same = plan_reconcile(&desired, &registered, false);
    assert!(same.is_empty(), "unchanged set must not unregister: {same:?}");

    let refresh = plan_reconcile(&desired, &registered, true);
    assert_eq!(refresh, vec![ReconcileAction::Refresh(key.clone())]);
    assert!(
        !refresh
            .iter()
            .any(|a| matches!(a, ReconcileAction::Drop(_))),
        "refresh must not emit goodbye/drop"
    );
}

#[test]
fn plan_reconcile_add_and_drop() {
    let keep = MdnsPublisher::fullname("keep", "_http._tcp");
    let gone = MdnsPublisher::fullname("gone", "_http._tcp");
    let newbie = MdnsPublisher::fullname("new", "_http._tcp");
    let entry = |name: &str| ServiceEntry {
        name: name.into(),
        type_: "_http._tcp".into(),
        protocol: "tcp".into(),
        port: 80,
        host: None,
        txt: None,
    };
    let mut desired = HashMap::new();
    desired.insert(keep.clone(), entry("keep"));
    desired.insert(newbie.clone(), entry("new"));
    let mut registered = HashMap::new();
    registered.insert(keep.clone(), entry("keep"));
    registered.insert(gone.clone(), entry("gone"));

    let actions = plan_reconcile(&desired, &registered, false);
    assert_eq!(
        actions,
        vec![
            ReconcileAction::Add(newbie),
            ReconcileAction::Drop(gone),
        ]
    );
}

#[test]
fn announce_burst_is_powers_of_two() {
    assert_eq!(
        announce_burst_delays(4),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
        ]
    );
    assert!(announce_burst_delays(0).is_empty());
}

#[test]
fn bigfred_backoff_grows_to_cap() {
    assert_eq!(bigfred_backoff_ms(0, 45_000), 2_000);
    assert_eq!(bigfred_backoff_ms(1, 45_000), 4_000);
    assert_eq!(bigfred_backoff_ms(2, 45_000), 8_000);
    assert_eq!(bigfred_backoff_ms(10, 45_000), 45_000);
}
