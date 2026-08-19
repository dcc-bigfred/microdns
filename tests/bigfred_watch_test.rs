use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use microdns::bigfred_watch::dcc_bus_list;

fn tmp_sock() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("microdns-bf-{nanos}.sock"))
}

fn write_frame(stream: &mut impl Write, payload: &[u8]) {
    let len = u32::try_from(payload.len()).unwrap().to_le_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}

#[test]
fn dcc_bus_list_reads_programs_from_socket() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut hdr = [0u8; 4];
        stream.read_exact(&mut hdr).unwrap();
        let n = u32::from_le_bytes(hdr) as usize;
        let mut buf = vec![0u8; n];
        stream.read_exact(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("dcc_bus_list"));
        let body = br#"{"programs":[{"layoutId":2,"layoutName":"Klubowa","name":"dcc-bus-2-5","status":"running","running":true,"commandStationId":5,"withrottleEnabled":true,"withrottlePort":12091,"z21Enabled":false,"z21Port":21105}]}"#;
        write_frame(&mut stream, body);
    });

    let programs = dcc_bus_list(&sock).unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_file(&sock);
    assert_eq!(programs.len(), 1);
    assert_eq!(programs[0].withrottle_port, 12091);
}

#[test]
fn dcc_bus_list_missing_socket_is_ipc_error() {
    let sock = tmp_sock();
    let _ = std::fs::remove_file(&sock);
    let err = dcc_bus_list(&sock).unwrap_err();
    assert!(err.to_string().contains("cannot connect"));
}
