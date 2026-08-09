use std::collections::HashSet;
use std::path::PathBuf;

use microdns::proc_scan::{listen_ports_for_pid, parse_net_line, parse_socket_link, TCP_LISTEN};

#[test]
fn parse_socket_link_ok() {
    assert_eq!(
        parse_socket_link(&PathBuf::from("socket:[12345]")),
        Some(12345)
    );
    assert_eq!(parse_socket_link(&PathBuf::from("pipe:[1]")), None);
}

#[test]
fn parse_tcp_listen_line() {
    let mut inodes = HashSet::new();
    inodes.insert(12345);
    // Typical /proc/net/tcp line (abbreviated columns padded).
    let line = "   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  0        0 12345 1 0000000000000000 100 0 0 10 0";
    assert_eq!(parse_net_line(line, &inodes, Some(TCP_LISTEN)), Some(8080));
    assert_eq!(parse_net_line(line, &inodes, Some("01")), None);
}

#[test]
fn self_pid_scan_does_not_panic() {
    let pid = std::process::id() as i32;
    let ports = listen_ports_for_pid(pid).unwrap();
    // May be empty; just ensure it succeeds.
    let _ = ports;
}
