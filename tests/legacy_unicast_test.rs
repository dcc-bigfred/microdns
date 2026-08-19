//! Unit + integration tests for the legacy unicast mDNS responder.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use microdns::legacy_unicast::{
    build_response, choose_v4, choose_v4_for_iface, choose_v6_for_iface, hosts_match,
    memberships_need_refresh, parse_query, should_replace_memberships, spawn, AnswerSet,
    IfaceAddr4, IfaceAddr6, MembershipRefresh, ParsedQuery, LEGACY_TTL, QTYPE_A, QTYPE_AAAA,
    QTYPE_ANY,
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
            IfaceAddr4 {
                iface: "eth0".into(),
                addr: Ipv4Addr::new(192, 168, 1, 10),
                mask: Ipv4Addr::new(255, 255, 255, 0),
                ifindex: 2, // eth0
            },
            IfaceAddr4 {
                iface: "wlan0".into(),
                addr: Ipv4Addr::new(10, 0, 0, 5),
                mask: Ipv4Addr::new(255, 0, 0, 0),
                ifindex: 3, // wlan0
            },
        ],
        v6: vec![IfaceAddr6 {
            iface: "eth0".into(),
            addr: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            ifindex: 2,
        }],
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    }
}

#[test]
fn parse_rejects_response_qr() {
    let mut pkt = build_query(1, "bigfred.local.", QTYPE_A, 1);
    pkt[2] = 0x80; // QR
    assert!(parse_query(&pkt).is_none());
}

#[test]
fn parse_accepts_mdns_multicast_query() {
    // Multicast queries (src port 5353) are answered; content selects per-iface IP.
    let pkt = build_query(1, "bigfred.local.", QTYPE_A, 1);
    let q = parse_query(&pkt).expect("parse");
    assert_eq!(q.qtype, QTYPE_A);
}

#[test]
fn parse_extracts_android_style_query() {
    let pkt = build_query(11110, "bigfred.local.", QTYPE_A, 1);
    let q = parse_query(&pkt).expect("parse");
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
    assert!(parse_query(&pkt).is_none());
}

#[test]
fn parse_accepts_qu_bit_in_class() {
    let pkt = build_query(1, "bigfred.local.", QTYPE_A, 0x8001);
    let q = parse_query(&pkt).expect("parse");
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
fn choose_v4_for_iface_prefers_receiving_iface() {
    let answers = sample_answers();
    // Query from eth subnet but arrived on wlan (ifindex 3) → answer wlan IP.
    let chosen = choose_v4_for_iface(&answers, Ipv4Addr::new(192, 168, 1, 50), Some(3));
    assert_eq!(chosen, vec![Ipv4Addr::new(10, 0, 0, 5)]);
}

#[test]
fn choose_v4_for_iface_same_subnet_within_iface() {
    let mut answers = sample_answers();
    answers.v4.push(IfaceAddr4 {
        iface: "wlan0".into(),
        addr: Ipv4Addr::new(10, 1, 0, 5),
        mask: Ipv4Addr::new(255, 255, 0, 0),
        ifindex: 3,
    });
    let chosen = choose_v4_for_iface(&answers, Ipv4Addr::new(10, 0, 0, 99), Some(3));
    assert_eq!(chosen, vec![Ipv4Addr::new(10, 0, 0, 5)]);
}

#[test]
fn choose_v4_for_iface_falls_back_without_ifindex() {
    let answers = sample_answers();
    let chosen = choose_v4_for_iface(&answers, Ipv4Addr::new(192, 168, 1, 50), None);
    assert_eq!(chosen, vec![Ipv4Addr::new(192, 168, 1, 10)]);
}

#[test]
fn choose_v6_for_iface_prefers_receiving_iface() {
    let mut answers = sample_answers();
    answers.v6.push(IfaceAddr6 {
        iface: "wlan0".into(),
        addr: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2),
        ifindex: 3,
    });
    let chosen = choose_v6_for_iface(&answers, Some(3));
    assert_eq!(chosen, vec![Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2)]);
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
    let resp = build_response(
        &query,
        &answers,
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
        Some(2),
    )
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
    assert!(build_response(&query, &answers, IpAddr::V4(Ipv4Addr::LOCALHOST), None).is_none());
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
    assert!(build_response(&query, &answers, IpAddr::V4(Ipv4Addr::LOCALHOST), None).is_none());
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
    let resp = build_response(
        &query,
        &answers,
        IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
        None,
    )
    .expect("response");
    assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 3); // 2 A + 1 AAAA
}

#[test]
fn spawn_echoes_transaction_id_on_ephemeral_port() {
    let query_id = 0xabcd;
    let pkt = build_query(query_id, "bigfred.local.", QTYPE_A, 1);

    let mut got = None;
    for attempt in 0..5 {
        let probe = match UdpSocket::bind("127.0.0.1:0") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let answers = Arc::new(RwLock::new(AnswerSet {
            hosts: vec!["bigfred.local.".into()],
            v4: vec![IfaceAddr4 {
                iface: "lo".into(),
                addr: Ipv4Addr::new(127, 0, 0, 1),
                mask: Ipv4Addr::new(255, 0, 0, 0),
                ifindex: 1,
            }],
            v6: Vec::new(),
            skip_interfaces: Vec::new(),
            interfaces: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        if spawn(Arc::clone(&answers), port, Arc::clone(&stop)).is_err() {
            continue;
        }

        let client = match UdpSocket::bind("127.0.0.1:0") {
            Ok(c) => c,
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                continue;
            }
        };
        client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        for _ in 0..20 {
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
        if got.is_some() {
            break;
        }
        eprintln!(
            "spawn_echoes_transaction_id: attempt {} got no reply, retrying",
            attempt
        );
    }

    let resp = got.expect("timed out waiting for legacy unicast reply after 5 attempts");
    assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), query_id);
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

#[test]
fn same_ips_still_refresh_when_epoch_changes() {
    // Suspend/resume and LINK flaps keep the address set identical; the
    // refresh epoch is what forces leave+join of 224.0.0.251.
    assert!(!memberships_need_refresh(false, false));
    assert!(memberships_need_refresh(true, false));
    assert!(memberships_need_refresh(false, true));
    assert!(memberships_need_refresh(true, true));
}

#[test]
fn force_replaces_memberships_even_when_joined_matches() {
    let joined = vec![std::net::Ipv4Addr::new(192, 168, 1, 10)];
    let want = joined.clone();
    assert!(!should_replace_memberships(&joined, &want, false));
    assert!(should_replace_memberships(&joined, &want, true));
    let other = vec![std::net::Ipv4Addr::new(10, 0, 0, 1)];
    assert!(should_replace_memberships(&joined, &other, false));
}

#[test]
fn membership_refresh_rejoin_does_not_rebind() {
    let r = MembershipRefresh::new();
    assert_eq!(r.epoch(), 0);
    r.request_rejoin();
    assert_eq!(r.epoch(), 1);
    assert!(!r.take_rebind());
    r.request_rejoin();
    assert_eq!(r.epoch(), 2);
}

#[test]
fn membership_refresh_rebind_sets_flag_and_epoch() {
    let r = MembershipRefresh::new();
    r.request_rebind();
    assert_eq!(r.epoch(), 1);
    assert!(r.take_rebind());
    assert!(!r.take_rebind());
}
