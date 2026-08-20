use std::collections::{BTreeMap, HashSet};

use microdns::config::ServiceEntry;
use microdns::microinit_watch::{ads_from_snapshot, WatchFeed, WatchedService};

fn svc(name: &str, state: &str, labels: &[(&str, &str)]) -> WatchedService {
    let mut map = BTreeMap::new();
    for (k, v) in labels {
        map.insert((*k).into(), (*v).into());
    }
    WatchedService {
        name: name.into(),
        state: state.into(),
        labels: map,
    }
}

#[test]
fn running_with_port_type_and_txt() {
    let mut warned = HashSet::new();
    let ads = ads_from_snapshot(
        &[svc(
            "bigfred",
            "running",
            &[
                ("microdns-port", "8080"),
                ("microdns-type", "_http._tcp"),
                ("microdns-host", "bigfred"),
                ("microdns-txt-path", "/"),
            ],
        )],
        &mut warned,
    );
    assert_eq!(
        ads,
        vec![ServiceEntry {
            name: "bigfred".into(),
            type_: "_http._tcp".into(),
            protocol: "tcp".into(),
            port: 8080,
            host: Some("bigfred".into()),
            txt: Some(std::collections::HashMap::from([(
                "path".into(),
                "/".into()
            )])),
        }]
    );
    assert!(warned.is_empty());
}

#[test]
fn missing_host_leaves_entry_host_none() {
    let mut warned = HashSet::new();
    let ads = ads_from_snapshot(
        &[svc(
            "app",
            "running",
            &[("microdns-port", "8090"), ("microdns-type", "_http._tcp")],
        )],
        &mut warned,
    );
    assert_eq!(ads.len(), 1);
    assert_eq!(ads[0].host, None);
}

#[test]
fn skips_not_running() {
    let mut warned = HashSet::new();
    let ads = ads_from_snapshot(
        &[svc(
            "bigfred",
            "stopped",
            &[("microdns-port", "8080"), ("microdns-type", "_http._tcp")],
        )],
        &mut warned,
    );
    assert!(ads.is_empty());
    assert!(warned.is_empty());
}

#[test]
fn warns_once_on_missing_type() {
    let mut warned = HashSet::new();
    let row = svc("web", "running", &[("microdns-port", "80")]);
    let ads = ads_from_snapshot(std::slice::from_ref(&row), &mut warned);
    assert!(ads.is_empty());
    assert_eq!(warned.len(), 1);
    let _ = ads_from_snapshot(&[row], &mut warned);
    assert_eq!(warned.len(), 1);
}

#[test]
fn rejects_zero_and_unparseable_port() {
    let mut warned = HashSet::new();
    let ads = ads_from_snapshot(
        &[
            svc(
                "a",
                "running",
                &[("microdns-port", "0"), ("microdns-type", "_http._tcp")],
            ),
            svc(
                "b",
                "running",
                &[("microdns-port", "x"), ("microdns-type", "_http._tcp")],
            ),
        ],
        &mut warned,
    );
    assert!(ads.is_empty());
    assert_eq!(warned.len(), 2);
}

#[test]
fn udp_type_sets_protocol() {
    let mut warned = HashSet::new();
    let ads = ads_from_snapshot(
        &[svc(
            "z21",
            "running",
            &[("microdns-port", "21105"), ("microdns-type", "_z21._udp")],
        )],
        &mut warned,
    );
    assert_eq!(ads[0].protocol, "udp");
    assert_eq!(ads[0].type_, "_z21._udp");
}

#[test]
fn drain_into_keeps_last_good_when_idle() {
    let feed = WatchFeed::disconnected();
    let mut last = Some(vec![ServiceEntry {
        name: "bigfred".into(),
        type_: "_http._tcp".into(),
        protocol: "tcp".into(),
        port: 8080,
        host: Some("bigfred".into()),
        txt: None,
    }]);
    assert!(!feed.drain_into(&mut last));
    assert_eq!(last.as_ref().unwrap()[0].name, "bigfred");
}
