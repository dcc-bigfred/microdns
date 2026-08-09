use microdns::microinit_watch::{
    is_dcc_bus_name, is_running, parse_dcc_bus_ids, Request, ServiceStatus,
};

#[test]
fn running_helper() {
    assert!(is_running(&ServiceStatus {
        name: "dcc-bus-2-5".into(),
        state: "running".into(),
        pid: Some(42),
    }));
    assert!(!is_running(&ServiceStatus {
        name: "dcc-bus-2-5".into(),
        state: "stopped".into(),
        pid: Some(42),
    }));
    assert!(!is_running(&ServiceStatus {
        name: "dcc-bus-2-5".into(),
        state: "running".into(),
        pid: None,
    }));
}

#[test]
fn dcc_bus_name_prefix() {
    assert!(is_dcc_bus_name("dcc-bus"));
    assert!(is_dcc_bus_name("dcc-bus-2-5"));
    assert!(!is_dcc_bus_name("bigfred"));
    assert!(!is_dcc_bus_name("dcc-busy"));
}

#[test]
fn parse_dcc_bus_ids_ok() {
    assert_eq!(parse_dcc_bus_ids("dcc-bus-2-5"), Some((2, 5)));
    assert_eq!(parse_dcc_bus_ids("dcc-bus-0-1"), Some((0, 1)));
    assert_eq!(parse_dcc_bus_ids("dcc-bus"), None);
    assert_eq!(parse_dcc_bus_ids("dcc-bus-2"), None);
    assert_eq!(parse_dcc_bus_ids("dcc-bus-2-5-9"), None);
    assert_eq!(parse_dcc_bus_ids("bigfred"), None);
}

#[test]
fn request_serializes_snake_case() {
    let l = serde_json::to_string(&Request::List).unwrap();
    assert_eq!(l, r#"{"type":"list"}"#);
}
