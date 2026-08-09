//! Unit + integration tests for the legacy unicast mDNS responder.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use microdns::legacy_unicast::{
    build_response, choose_v4, hosts_match, parse_query, spawn, AnswerSet, ParsedQuery, LEGACY_TTL,
    QTYPE_A, QTYPE_AAAA, QTYPE_ANY,
};

fn encode_name(name: &str) -> Vec<u8> {
    let trimmed = name.trim_end_matches('.');
    let mut out = Vec::new();
    for label in trimmed.split('.') {
        let b = label.as_bytes();
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out.push(0);
    out
}

fn build_query(id: u16, qname: &str, qtype: u16, qclass: u16) -> Vec<u8> {
    let name = encode_name(qname);
    let mut pkt = Vec::with_capacity(12 + name.len() + 4);
    pkt.extend_from_slice(&id.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes()); // flags
    pkt.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&name);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&qclass.to_be_bytes());
    pkt
}

fn sample_answers() -> AnswerSet {
    AnswerSet {
        hosts: vec!["bigfred.local.".into()],
        v4: vec![
            (
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
            (Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::new(255, 0, 0, 0)),
        ],
        v6: vec![Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)],
    }
}

#[test]
fn parse_rejects_response_qr() {
    let mut pkt = build_query(1, "bigfred.local.", QTYPE_A, 1);
    pkt[2] = 0x80; // QR
    assert!(parse_query(&pkt, 54321).is_none());
}

#[test]
fn parse_rejects_mdns_src_port() {
    let pkt = build_query(1, "bigfred.local.", QTYPE_A, 1);
    assert!(parse_query(&pkt, 5353).is_none());
}

#[test]
fn parse_extracts_android_style_query() {
    let pkt = build_query(11110, "bigfred.local.", QTYPE_A, 1);
    let q = parse_query(&pkt, 54321).expect("parse");
    assert_eq!(
        q,
        ParsedQuery {
            id: 11110,
            qname: "bigfred.local.".into(),
            qtype: QTYPE_A,
            qclass: 1,
        }
    );
}

#[test]
fn parse_rejects_non_in_class() {
    let pkt = build_query(1, "bigfred.local.", QTYPE_A, 2);
    assert!(parse_query(&pkt, 12345).is_none());
}

#[test]
fn parse_accepts_qu_bit_in_class() {
    let pkt = build_query(1, "bigfred.local.", QTYPE_A, 0x8001);
    let q = parse_query(&pkt, 12345).expect("parse");
    assert_eq!(q.qclass, 0x8001);
}

#[test]
fn hosts_match_case_insensitive() {
    assert!(hosts_match(&["BigFred.Local.".into()], "bigfred.local"));
    assert!(!hosts_match(&["bigfred.local.".into()], "other.local."));
}

#[test]
fn choose_v4_prefers_same_subnet() {
    let answers = sample_answers();
    let chosen = choose_v4(&answers, Ipv4Addr::new(192, 168, 1, 50));
    assert_eq!(chosen, vec![Ipv4Addr::new(192, 168, 1, 10)]);
}

#[test]
fn choose_v4_falls_back_to_all() {
    let answers = sample_answers();
    let chosen = choose_v4(&answers, Ipv4Addr::new(172, 16, 0, 1));
    assert_eq!(
        chosen,
        vec![Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(10, 0, 0, 5),]
    );
}

#[test]
fn build_echoes_id_and_ttl() {
    let answers = sample_answers();
    let query = ParsedQuery {
        id: 0x2b76,
        qname: "bigfred.local.".into(),
        qtype: QTYPE_A,
        qclass: 0x8001,
    };
    let resp = build_response(&query, &answers, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)))
        .expect("response");
    assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 0x2b76);
    assert_eq!(u16::from_be_bytes([resp[2], resp[3]]), 0x8400);
    assert_eq!(u16::from_be_bytes([resp[4], resp[5]]), 1); // qdcount
    assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1); // ancount

    // Find TTL in first answer: after echoed question.
    // Walk past question name + type/class.
    let mut pos = 12usize;
    while pos < resp.len() && resp[pos] != 0 {
        pos += 1 + resp[pos] as usize;
    }
    pos += 1; // root
    pos += 4; // qtype + qclass
              // answer name
    while pos < resp.len() && resp[pos] != 0 {
        pos += 1 + resp[pos] as usize;
    }
    pos += 1;
    let rtype = u16::from_be_bytes([resp[pos], resp[pos + 1]]);
    let rclass = u16::from_be_bytes([resp[pos + 2], resp[pos + 3]]);
    let ttl = u32::from_be_bytes([resp[pos + 4], resp[pos + 5], resp[pos + 6], resp[pos + 7]]);
    assert_eq!(rtype, QTYPE_A);
    assert_eq!(rclass, 0x0001); // no cache-flush
    assert_eq!(ttl, LEGACY_TTL);

    // Question class should have QU cleared.
    let mut qpos = 12usize;
    while qpos < resp.len() && resp[qpos] != 0 {
        qpos += 1 + resp[qpos] as usize;
    }
    qpos += 1 + 2; // root + qtype
    let echoed_qclass = u16::from_be_bytes([resp[qpos], resp[qpos + 1]]);
    assert_eq!(echoed_qclass, 0x0001);
}

#[test]
fn build_none_for_unknown_host() {
    let answers = sample_answers();
    let query = ParsedQuery {
        id: 1,
        qname: "other.local.".into(),
        qtype: QTYPE_A,
        qclass: 1,
    };
    assert!(build_response(&query, &answers, IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
}

#[test]
fn build_aaaa_none_when_empty() {
    let mut answers = sample_answers();
    answers.v6.clear();
    let query = ParsedQuery {
        id: 1,
        qname: "bigfred.local.".into(),
        qtype: QTYPE_AAAA,
        qclass: 1,
    };
    assert!(build_response(&query, &answers, IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
}

#[test]
fn build_any_includes_a_and_aaaa() {
    let answers = sample_answers();
    let query = ParsedQuery {
        id: 9,
        qname: "bigfred.local.".into(),
        qtype: QTYPE_ANY,
        qclass: 1,
    };
    let resp = build_response(&query, &answers, IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)))
        .expect("response");
    assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 3); // 2 A + 1 AAAA
}

#[test]
fn spawn_echoes_transaction_id_on_ephemeral_port() {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind probe");
    let port = sock.local_addr().unwrap().port();
    // Free the port so the responder can bind it; we re-bind as client after spawn.
    drop(sock);

    let answers = Arc::new(RwLock::new(AnswerSet {
        hosts: vec!["bigfred.local.".into()],
        v4: vec![(Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(255, 0, 0, 0))],
        v6: Vec::new(),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    spawn(Arc::clone(&answers), port, Arc::clone(&stop)).expect("spawn");

    // Wait until responder is listening.
    let client = {
        let mut last_err = None;
        let mut bound = None;
        for _ in 0..50 {
            match UdpSocket::bind("127.0.0.1:0") {
                Ok(c) => {
                    c.set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    // Probe by sending; retry until we get a reply or timeout budget.
                    bound = Some(c);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
            thread::sleep(Duration::from_millis(20));
        }
        bound.unwrap_or_else(|| panic!("client bind: {last_err:?}"))
    };

    let query_id = 0xabcd;
    let pkt = build_query(query_id, "bigfred.local.", QTYPE_A, 1);
    let mut got = None;
    for _ in 0..40 {
        let _ = client.send_to(&pkt, ("127.0.0.1", port));
        let mut buf = [0u8; 512];
        match client.recv_from(&mut buf) {
            Ok((n, _)) => {
                got = Some(buf[..n].to_vec());
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    stop.store(true, Ordering::SeqCst);

    let resp = got.expect("timed out waiting for legacy unicast reply");
    assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), query_id);
    // TTL check on first answer
    let mut pos = 12usize;
    while pos < resp.len() && resp[pos] != 0 {
        pos += 1 + resp[pos] as usize;
    }
    pos += 1 + 4; // root + question type/class
    while pos < resp.len() && resp[pos] != 0 {
        pos += 1 + resp[pos] as usize;
    }
    pos += 1 + 4; // root + type/class
    let ttl = u32::from_be_bytes([resp[pos], resp[pos + 1], resp[pos + 2], resp[pos + 3]]);
    assert_eq!(ttl, LEGACY_TTL);
}
