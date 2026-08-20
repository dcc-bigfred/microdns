use std::io::Cursor;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use microdns::config::ServiceEntry;
use microdns::ctl::{
    listed_services, print_human, read_frame_from, serve, services_list, write_frame,
    ListedService, ServiceSource, ServicesListBody,
};
use microdns::run::{BeaconWant, DesiredAds, DynAd};

static TMP_SOCK_SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_sock() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = TMP_SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("microdns-ctl-{nanos}-{seq}.sock"))
}

fn sample_ads() -> DesiredAds {
    DesiredAds {
        static_services: vec![ServiceEntry {
            name: "bigfred".into(),
            type_: "_http._tcp".into(),
            protocol: "tcp".into(),
            port: 8080,
            host: Some("bigfred".into()),
            txt: None,
        }],
        dynamic: vec![
            DynAd {
                entry: ServiceEntry {
                    name: "BigFred #2".into(),
                    type_: "_withrottle._tcp".into(),
                    protocol: "tcp".into(),
                    port: 12090,
                    host: None,
                    txt: None,
                },
            },
            DynAd {
                entry: ServiceEntry {
                    name: "BigFred #2".into(),
                    type_: "_z21._udp".into(),
                    protocol: "udp".into(),
                    port: 21105,
                    host: None,
                    txt: None,
                },
            },
        ],
        beacons: vec![BeaconWant {
            port: 21105,
            serial: 1,
        }],
        ..DesiredAds::default()
    }
}

#[test]
fn listed_services_static_and_dcc_bus_not_beacons() {
    let listed = listed_services(&sample_ads());
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].source, ServiceSource::Static);
    assert_eq!(listed[0].name, "bigfred");
    assert_eq!(listed[1].source, ServiceSource::DccBus);
    assert_eq!(listed[1].type_, "_withrottle._tcp");
    assert_eq!(listed[2].type_, "_z21._udp");
}

#[test]
fn source_serializes_camel_case() {
    assert_eq!(
        serde_json::to_value(ServiceSource::Static).unwrap(),
        serde_json::json!("static")
    );
    assert_eq!(
        serde_json::to_value(ServiceSource::DccBus).unwrap(),
        serde_json::json!("dccBus")
    );
}

#[test]
fn reject_oversized_length() {
    let mut data = (1024 * 1024 + 1_u32).to_le_bytes().to_vec();
    data.extend_from_slice(&[0u8; 8]);
    let mut cur = Cursor::new(data);
    let err = read_frame_from(&mut cur).unwrap_err();
    assert!(err.to_string().contains("too large"));
}

#[test]
fn frame_roundtrip_unix_pair() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let handle = std::thread::spawn(move || {
        let raw = microdns::ctl::read_frame(&mut b).unwrap();
        let req: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(req["type"], "services_list");
        write_frame(
            &mut b,
            &ServicesListBody {
                services: Vec::new(),
            },
        )
        .unwrap();
    });
    write_frame(&mut a, &serde_json::json!({"type": "services_list"})).unwrap();
    let raw = microdns::ctl::read_frame(&mut a).unwrap();
    let body: ServicesListBody = serde_json::from_slice(&raw).unwrap();
    assert!(body.services.is_empty());
    handle.join().unwrap();
}

#[test]
fn services_list_roundtrip_on_temp_socket() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    let snapshot = Arc::new(RwLock::new(sample_ads()));
    serve(&sock, Arc::clone(&snapshot)).unwrap();

    let listed = services_list(&sock).unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].name, "bigfred");
    assert_eq!(listed[0].source, ServiceSource::Static);
    assert_eq!(listed[1].source, ServiceSource::DccBus);
    assert_eq!(listed[2].port, 21105);

    let err = serve(&sock, snapshot).unwrap_err();
    assert!(err.to_string().contains("already running"));

    let _ = std::fs::remove_file(&sock);
}

#[test]
fn services_list_missing_socket_mentions_running() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    let err = services_list(&sock).unwrap_err();
    assert!(err.to_string().contains("cannot connect"));
    assert!(err.to_string().contains("is microdns running?"));
}

#[test]
fn unknown_type_is_invalid_request() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    serve(&sock, Arc::new(RwLock::new(DesiredAds::default()))).unwrap();

    let mut stream = UnixStream::connect(&sock).unwrap();
    write_frame(&mut stream, &serde_json::json!({"type": "watch"})).unwrap();
    let raw = microdns::ctl::read_frame(&mut stream).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["error"], "invalid_request");

    let _ = std::fs::remove_file(&sock);
}

#[test]
fn bind_reuses_stale_socket() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    {
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    }
    assert!(sock.exists());
    serve(&sock, Arc::new(RwLock::new(DesiredAds::default()))).unwrap();
    let listed = services_list(&sock).unwrap();
    assert!(listed.is_empty());
    let _ = std::fs::remove_file(&sock);
}

fn human_table(services: &[ListedService]) -> String {
    let mut buf = Vec::new();
    print_human(&mut buf, services).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn human_table_headers_and_sources() {
    let listed = listed_services(&sample_ads());
    let table = human_table(&listed);
    let header = table.lines().next().unwrap();
    for col in ["NAME", "TYPE", "PROTO", "PORT", "HOST", "SOURCE"] {
        assert!(header.contains(col), "missing column {col} in {header:?}");
    }
    assert!(table.contains("static"));
    assert!(table.contains("dccBus"));
    assert!(table.contains("bigfred"));
    assert!(table.contains("_withrottle._tcp"));
    let data_lines: Vec<&str> = table.lines().skip(1).collect();
    assert!(data_lines
        .iter()
        .any(|l| l.contains("8080") && l.contains("bigfred")));
    assert!(data_lines
        .iter()
        .any(|l| l.contains("12090") && l.contains('-') && l.contains("dccBus")));
}

#[test]
fn cli_rejects_invalid_output_format() {
    let exe = env!("CARGO_BIN_EXE_microdns");
    let out = std::process::Command::new(exe)
        .args(["services", "list", "-o", "xml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("invalid") && err.contains("human") && err.contains("json"),
        "{err}"
    );
}
