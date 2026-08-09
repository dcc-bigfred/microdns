//! Legacy unicast / one-shot mDNS responder (RFC 6762 §5.1 / §6.7).
//!
//! `mdns-sd` 0.20.3 answers legacy queries via unicast but hardcodes transaction
//! ID=0 (`DnsOutgoing.multicast` is never cleared). Android `getaddrinfo` rejects
//! mismatched IDs. This module listens on the same port with `SO_REUSEPORT`,
//! answers A/AAAA one-shot queries with the query ID echoed, TTL 10, and no
//! cache-flush bit.
//!
//! Remove when upstream fixes `DnsOutgoing` multicast / id encoding.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

use crate::error::{Error, Result};
use crate::mdns;

/// Hosts and addresses answered for one-shot A/AAAA queries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AnswerSet {
    /// Normalized hosts, e.g. `"bigfred.local."`.
    pub hosts: Vec<String>,
    /// Preferred IPv4 addresses with netmasks `(addr, mask)`.
    pub v4: Vec<(Ipv4Addr, Ipv4Addr)>,
    /// Preferred global/ULA IPv6 addresses.
    pub v6: Vec<Ipv6Addr>,
    /// Configured extra interface-name prefixes to skip (mirrors `Config`).
    pub skip_interfaces: Vec<String>,
}

pub const MDNS_PORT: u16 = 5353;
pub const LEGACY_TTL: u32 = 10;
pub const QTYPE_A: u16 = 1;
pub const QTYPE_AAAA: u16 = 28;
pub const QTYPE_ANY: u16 = 255;

const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const BIND_RETRY: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_millis(200);
const HEADER_QR_AA: u16 = 0x8400;

/// Parsed one-shot query (first question only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    pub id: u16,
    /// Lowercased name with trailing dot.
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
}

/// Spawn the legacy-unicast responder thread.
///
/// On `MDNS_PORT` (5353) joins multicast groups on preferred interfaces.
/// On any other port (tests), binds loopback / unspecified without multicast.
pub fn spawn(state: Arc<RwLock<AnswerSet>>, port: u16, stop: Arc<AtomicBool>) -> Result<()> {
    thread::Builder::new()
        .name("legacy-unicast".into())
        .spawn(move || {
            if let Err(e) = run_loop(state, port, stop) {
                log::warn!("legacy unicast responder stopped: {e}");
            }
        })
        .map_err(|e| Error::Other(format!("spawn legacy-unicast: {e}")))?;
    Ok(())
}

fn run_loop(state: Arc<RwLock<AnswerSet>>, port: u16, stop: Arc<AtomicBool>) -> Result<()> {
    let test_mode = port != MDNS_PORT;
    let mut warned_bind = false;
    let mut sock_v4: Option<UdpSocket> = None;
    let mut sock_v6: Option<UdpSocket> = None;
    let mut joined_v4: Vec<Ipv4Addr> = Vec::new();
    let mut joined_ifindexes: Vec<u32> = Vec::new();
    // Cache of the AnswerSet snapshot last used to compute multicast joins.
    // refresh_memberships is expensive (getifaddrs + /sys read) so we only
    // recompute when the AnswerSet actually changes.
    let mut last_answer_snapshot: Option<AnswerSet> = None;

    while !stop.load(Ordering::SeqCst) {
        if sock_v4.is_none() {
            match bind_v4(port, test_mode) {
                Ok(s) => {
                    log::info!("legacy unicast IPv4 listening on 0.0.0.0:{port}");
                    sock_v4 = Some(s);
                    warned_bind = false;
                }
                Err(e) => {
                    if !warned_bind {
                        log::warn!("legacy unicast IPv4 bind :{port}: {e}; retrying");
                        warned_bind = true;
                    }
                }
            }
        }
        if sock_v6.is_none() {
            match bind_v6(port, test_mode) {
                Ok(s) => {
                    log::info!("legacy unicast IPv6 listening on [::]:{port}");
                    sock_v6 = Some(s);
                }
                Err(e) => {
                    log::debug!("legacy unicast IPv6 bind :{port}: {e}");
                }
            }
        }

        if sock_v4.is_none() && sock_v6.is_none() {
            thread::sleep(BIND_RETRY);
            continue;
        }

        if !test_mode {
            let current_snapshot = state.read().map(|g| g.clone()).ok();
            let changed = current_snapshot.as_ref() != last_answer_snapshot.as_ref();
            if changed {
                refresh_memberships(
                    sock_v4.as_ref(),
                    sock_v6.as_ref(),
                    &state,
                    &mut joined_v4,
                    &mut joined_ifindexes,
                );
                last_answer_snapshot = current_snapshot;
            }
        }

        let mut buf = [0u8; 2048];
        let mut got_any = false;

        if let Some(sock) = sock_v4.as_ref() {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    got_any = true;
                    handle_packet(sock, &buf[..n], peer, &state);
                }
                Err(e) if is_timeout(&e) => {}
                Err(e) => {
                    log::debug!("legacy unicast v4 recv: {e}");
                    sock_v4 = None;
                    joined_v4.clear();
                    last_answer_snapshot = None;
                }
            }
        }
        if let Some(sock) = sock_v6.as_ref() {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    got_any = true;
                    handle_packet(sock, &buf[..n], peer, &state);
                }
                Err(e) if is_timeout(&e) => {}
                Err(e) => {
                    log::debug!("legacy unicast v6 recv: {e}");
                    sock_v6 = None;
                    joined_ifindexes.clear();
                    last_answer_snapshot = None;
                }
            }
        }

        if !got_any {
            thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn handle_packet(sock: &UdpSocket, packet: &[u8], peer: SocketAddr, state: &RwLock<AnswerSet>) {
    let Some(query) = parse_query(packet, peer.port()) else {
        return;
    };
    let answers = match state.read() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    let Some(resp) = build_response(&query, &answers, peer.ip()) else {
        return;
    };
    if let Err(e) = sock.send_to(&resp, peer) {
        log::debug!("legacy unicast send_to {peer}: {e}");
    }
}

fn bind_v4(port: u16, test_mode: bool) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        if let Err(e) = sock.set_reuse_port(true) {
            log::debug!("SO_REUSEPORT v4 unavailable: {e}");
        }
    }
    sock.set_read_timeout(Some(RECV_TIMEOUT))?;
    let addr = if test_mode {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
    };
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

fn bind_v6(port: u16, test_mode: bool) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        if let Err(e) = sock.set_reuse_port(true) {
            log::debug!("SO_REUSEPORT v6 unavailable: {e}");
        }
    }
    sock.set_only_v6(true)?;
    sock.set_read_timeout(Some(RECV_TIMEOUT))?;
    let addr = if test_mode {
        SocketAddr::from((Ipv6Addr::LOCALHOST, port))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))
    };
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

fn refresh_memberships(
    sock_v4: Option<&UdpSocket>,
    sock_v6: Option<&UdpSocket>,
    state: &RwLock<AnswerSet>,
    joined_v4: &mut Vec<Ipv4Addr>,
    joined_ifindexes: &mut Vec<u32>,
) {
    let mut want_v4: Vec<Ipv4Addr> = state
        .read()
        .map(|g| g.v4.iter().map(|(ip, _)| *ip).collect())
        .unwrap_or_default();
    want_v4.sort_unstable();
    want_v4.dedup();
    // Prefer explicit iface list; also pick up indexes when AnswerSet has no v4
    // yet (v6-only / A-over-v6 queries). Use the configured skip list so the
    // indexes stay consistent with the v4/v6 address selection.
    let skip: Vec<String> = state
        .read()
        .map(|g| g.skip_interfaces.clone())
        .unwrap_or_default();
    let mut want_idx = mdns::preferred_iface_indexes(&skip);
    want_idx.sort_unstable();
    want_idx.dedup();

    if let Some(sock) = sock_v4 {
        if *joined_v4 != want_v4 {
            for ip in joined_v4.iter() {
                let _ = sock.leave_multicast_v4(&MDNS_GROUP_V4, ip);
            }
            joined_v4.clear();
            for ip in &want_v4 {
                match sock.join_multicast_v4(&MDNS_GROUP_V4, ip) {
                    Ok(()) => joined_v4.push(*ip),
                    Err(e) => log::debug!("join multicast v4 on {ip}: {e}"),
                }
            }
        }
    }

    if let Some(sock) = sock_v6 {
        if *joined_ifindexes != want_idx {
            for idx in joined_ifindexes.iter() {
                let _ = sock.leave_multicast_v6(&MDNS_GROUP_V6, *idx);
            }
            joined_ifindexes.clear();
            for idx in &want_idx {
                match sock.join_multicast_v6(&MDNS_GROUP_V6, *idx) {
                    Ok(()) => joined_ifindexes.push(*idx),
                    Err(e) => log::debug!("join multicast v6 ifindex {idx}: {e}"),
                }
            }
        }
    }
}

/// Parse a DNS query packet; returns [`None`] when the packet is not a legacy
/// one-shot A/AAAA/ANY query we should answer.
pub fn parse_query(packet: &[u8], src_port: u16) -> Option<ParsedQuery> {
    if src_port == MDNS_PORT {
        return None;
    }
    if packet.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([packet[0], packet[1]]);
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if (flags & 0x8000) != 0 {
        return None; // QR set → response
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return None;
    }

    let mut pos = 12usize;
    let qname = read_name(packet, &mut pos)?;
    if pos + 4 > packet.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
    let qclass = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);

    if (qclass & 0x7fff) != 1 {
        return None;
    }
    if qtype != QTYPE_A && qtype != QTYPE_AAAA && qtype != QTYPE_ANY {
        return None;
    }

    Some(ParsedQuery {
        id,
        qname,
        qtype,
        qclass,
    })
}

/// Build a unicast response, or [`None`] when the query should be ignored.
pub fn build_response(
    query: &ParsedQuery,
    answers: &AnswerSet,
    querier: IpAddr,
) -> Option<Vec<u8>> {
    if !hosts_match(&answers.hosts, &query.qname) {
        return None;
    }

    let mut records: Vec<(u16, Vec<u8>)> = Vec::new(); // (rtype, rdata)

    let want_a = query.qtype == QTYPE_A || query.qtype == QTYPE_ANY;
    let want_aaaa = query.qtype == QTYPE_AAAA || query.qtype == QTYPE_ANY;

    if want_a {
        let querier_v4 = match querier {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
        };
        for ip in choose_v4(answers, querier_v4) {
            records.push((QTYPE_A, ip.octets().to_vec()));
        }
    }
    if want_aaaa {
        for ip in &answers.v6 {
            records.push((QTYPE_AAAA, ip.octets().to_vec()));
        }
    }

    if records.is_empty() {
        return None;
    }

    let name_wire = encode_name(&query.qname);
    let qclass_clear = query.qclass & 0x7fff;

    let mut out =
        Vec::with_capacity(12 + name_wire.len() + 4 + records.len() * (name_wire.len() + 14));
    out.extend_from_slice(&query.id.to_be_bytes());
    out.extend_from_slice(&HEADER_QR_AA.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    out.extend_from_slice(&(records.len() as u16).to_be_bytes()); // ancount
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&0u16.to_be_bytes()); // arcount

    // Question (QU bit cleared).
    out.extend_from_slice(&name_wire);
    out.extend_from_slice(&query.qtype.to_be_bytes());
    out.extend_from_slice(&qclass_clear.to_be_bytes());

    // Answers: class IN (no cache-flush), TTL 10, uncompressed names.
    for (rtype, rdata) in records {
        out.extend_from_slice(&name_wire);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // class IN
        out.extend_from_slice(&LEGACY_TTL.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(&rdata);
    }

    Some(out)
}

/// Prefer same-subnet IPv4 addresses; otherwise all configured v4.
#[must_use]
pub fn choose_v4(answers: &AnswerSet, querier: Ipv4Addr) -> Vec<Ipv4Addr> {
    let same: Vec<Ipv4Addr> = answers
        .v4
        .iter()
        .filter(|(addr, mask)| {
            u32::from(*addr) & u32::from(*mask) == u32::from(querier) & u32::from(*mask)
        })
        .map(|(addr, _)| *addr)
        .collect();
    if !same.is_empty() {
        same
    } else {
        answers.v4.iter().map(|(addr, _)| *addr).collect()
    }
}

/// Case-insensitive host match against normalized names (trailing dot).
#[must_use]
pub fn hosts_match(hosts: &[String], qname: &str) -> bool {
    let q = normalize_lookup_name(qname);
    hosts.iter().any(|h| normalize_lookup_name(h) == q)
}

fn normalize_lookup_name(name: &str) -> String {
    let mut n = name.trim().to_ascii_lowercase();
    if !n.ends_with('.') {
        n.push('.');
    }
    n
}

fn encode_name(name: &str) -> Vec<u8> {
    let trimmed = name.trim_end_matches('.');
    let mut out = Vec::new();
    if trimmed.is_empty() {
        out.push(0);
        return out;
    }
    for label in trimmed.split('.') {
        let bytes = label.as_bytes();
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out.push(0);
    out
}

fn read_name(packet: &[u8], pos: &mut usize) -> Option<String> {
    let mut labels = Vec::new();
    let mut jumped = false;
    let mut cursor = *pos;
    let mut guard = 0usize;

    loop {
        if guard > 64 || cursor >= packet.len() {
            return None;
        }
        guard += 1;
        let len = packet[cursor];
        if len == 0 {
            cursor += 1;
            if !jumped {
                *pos = cursor;
            }
            break;
        }
        if (len & 0xc0) == 0xc0 {
            if cursor + 1 >= packet.len() {
                return None;
            }
            let ptr = (((len as usize) & 0x3f) << 8) | (packet[cursor + 1] as usize);
            if !jumped {
                *pos = cursor + 2;
                jumped = true;
            }
            cursor = ptr;
            continue;
        }
        let label_len = len as usize;
        cursor += 1;
        if cursor + label_len > packet.len() {
            return None;
        }
        let label = std::str::from_utf8(&packet[cursor..cursor + label_len]).ok()?;
        labels.push(label.to_ascii_lowercase());
        cursor += label_len;
    }

    let mut name = labels.join(".");
    name.push('.');
    Some(name)
}
