//! Legacy unicast / one-shot mDNS responder (RFC 6762 §5.1 / §6.7) plus
//! per-interface A/AAAA answers for multicast queries.
//!
//! `mdns-sd` 0.20.3 answers legacy queries via unicast but hardcodes transaction
//! ID=0 (`DnsOutgoing.multicast` is never cleared). Android `getaddrinfo` rejects
//! mismatched IDs. This module listens on the same port with `SO_REUSEPORT`,
//! answers A/AAAA queries with the query ID echoed, TTL 10, and no cache-flush
//! bit — and selects the answer address from the receiving interface via
//! `IP_PKTINFO` / `IPV6_PKTINFO`.
//!
//! Remove the legacy ID-echo path when upstream fixes `DnsOutgoing` multicast /
//! id encoding; keep the per-interface selection.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::error::{Error, Result};
use crate::mdns;
use crate::sys;

/// IPv4 address bound to a specific interface (for per-iface replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceAddr4 {
    /// Kernel interface name, e.g. `"eth0"` / `"wlan0"`.
    pub iface: String,
    pub addr: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub ifindex: u32,
}

/// IPv6 address bound to a specific interface (for per-iface replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceAddr6 {
    /// Kernel interface name, e.g. `"eth0"` / `"wlan0"`.
    pub iface: String,
    pub addr: Ipv6Addr,
    pub ifindex: u32,
}

/// Hosts and addresses answered for A/AAAA queries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AnswerSet {
    /// Normalized hosts, e.g. `"bigfred.local."`.
    pub hosts: Vec<String>,
    /// Preferred IPv4 addresses with netmask and ifindex.
    pub v4: Vec<IfaceAddr4>,
    /// Preferred global/ULA IPv6 addresses with ifindex.
    pub v6: Vec<IfaceAddr6>,
    /// Configured extra interface-name prefixes to skip (mirrors `Config`).
    pub skip_interfaces: Vec<String>,
    /// Optional allowlist of interface-name prefixes (mirrors `Config`).
    pub interfaces: Vec<String>,
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

/// Parsed A/AAAA/ANY query (first question only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    pub id: u16,
    /// Lowercased name with trailing dot.
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
}

/// Cross-thread signal so the main loop can force IGMP leave+join (and
/// optionally recreate UDP sockets) when the address set is unchanged —
/// the suspend/resume case, or a netlink LINK event with the same IPs.
#[derive(Debug)]
pub struct MembershipRefresh {
    epoch: AtomicU64,
    rebind: AtomicBool,
}

impl MembershipRefresh {
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            rebind: AtomicBool::new(false),
        }
    }

    /// Force leave+join on the existing sockets (netlink without IP churn).
    pub fn request_rejoin(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Drop and recreate UDP sockets, then join (suspend/resume).
    pub fn request_rebind(&self) {
        self.rebind.store(true, Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Current epoch. The responder rejoins whenever this value changes.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Consume the rebind flag. True once per [`request_rebind`].
    #[must_use]
    pub fn take_rebind(&self) -> bool {
        self.rebind.swap(false, Ordering::SeqCst)
    }
}

impl Default for MembershipRefresh {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the A/AAAA responder thread.
///
/// On `MDNS_PORT` (5353) joins multicast groups on preferred interfaces.
/// On any other port (tests), binds loopback / unspecified without multicast.
pub fn spawn(state: Arc<RwLock<AnswerSet>>, port: u16, stop: Arc<AtomicBool>) -> Result<()> {
    spawn_with_refresh(state, port, stop, Arc::new(MembershipRefresh::new()))
}

/// Like [`spawn`], with a shared refresh signal for netlink / suspend.
pub fn spawn_with_refresh(
    state: Arc<RwLock<AnswerSet>>,
    port: u16,
    stop: Arc<AtomicBool>,
    refresh: Arc<MembershipRefresh>,
) -> Result<()> {
    thread::Builder::new()
        .name("legacy-unicast".into())
        .spawn(move || {
            if let Err(e) = run_loop(state, port, stop, refresh) {
                log::warn!("legacy unicast responder stopped: {e}");
            }
        })
        .map_err(|e| Error::Other(format!("spawn legacy-unicast: {e}")))?;
    Ok(())
}

fn run_loop(
    state: Arc<RwLock<AnswerSet>>,
    port: u16,
    stop: Arc<AtomicBool>,
    refresh: Arc<MembershipRefresh>,
) -> Result<()> {
    let test_mode = port != MDNS_PORT;
    let mut warned_bind = false;
    let mut sock_v4: Option<Socket> = None;
    let mut sock_v6: Option<Socket> = None;
    let mut joined_v4: Vec<Ipv4Addr> = Vec::new();
    let mut joined_ifindexes: Vec<u32> = Vec::new();
    // Cache of the AnswerSet snapshot last used to compute multicast joins.
    // refresh_memberships is expensive (getifaddrs + /sys read) so we only
    // recompute when the AnswerSet actually changes — or when the main loop
    // bumps the refresh epoch (same IPs after suspend / link flap).
    let mut last_answer_snapshot: Option<AnswerSet> = None;
    let mut last_epoch: u64 = 0;

    while !stop.load(Ordering::SeqCst) {
        if !test_mode && refresh.take_rebind() {
            log::info!("legacy unicast rebinding UDP sockets after suspend/resume");
            sock_v4 = None;
            sock_v6 = None;
            joined_v4.clear();
            joined_ifindexes.clear();
            last_answer_snapshot = None;
            // Keep last_epoch behind the signal so the post-bind pass force-joins.
            last_epoch = refresh.epoch().saturating_sub(1);
        }

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
            let epoch = refresh.epoch();
            let epoch_changed = epoch != last_epoch;
            if changed || epoch_changed {
                refresh_memberships(
                    sock_v4.as_ref(),
                    sock_v6.as_ref(),
                    &state,
                    &mut joined_v4,
                    &mut joined_ifindexes,
                    epoch_changed,
                );
                last_answer_snapshot = current_snapshot;
                last_epoch = epoch;
            }
        }

        let mut got_any = false;

        if let Some(sock) = sock_v4.as_ref() {
            match sys::recv_with_pktinfo(sock, false) {
                Ok(Some(pkt)) => {
                    got_any = true;
                    handle_packet(sock, &pkt.buf[..pkt.len], pkt.peer, pkt.ifindex, &state);
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!("legacy unicast v4 recv: {e}");
                    sock_v4 = None;
                    joined_v4.clear();
                    last_answer_snapshot = None;
                }
            }
        }
        if let Some(sock) = sock_v6.as_ref() {
            match sys::recv_with_pktinfo(sock, true) {
                Ok(Some(pkt)) => {
                    got_any = true;
                    handle_packet(sock, &pkt.buf[..pkt.len], pkt.peer, pkt.ifindex, &state);
                }
                Ok(None) => {}
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

fn handle_packet(
    sock: &Socket,
    packet: &[u8],
    peer: SocketAddr,
    ifindex: Option<u32>,
    state: &RwLock<AnswerSet>,
) {
    let Some(query) = parse_query(packet) else {
        return;
    };
    let answers = match state.read() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    let Some(resp) = build_response(&query, &answers, peer.ip(), ifindex) else {
        return;
    };
    let dest = SockAddr::from(peer);
    if let Err(e) = sock.send_to(&resp, &dest) {
        log::debug!("legacy unicast send_to {peer}: {e}");
    }
}

fn bind_v4(port: u16, test_mode: bool) -> Result<Socket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        if let Err(e) = sock.set_reuse_port(true) {
            log::debug!("SO_REUSEPORT v4 unavailable: {e}");
        }
    }
    sock.set_read_timeout(Some(RECV_TIMEOUT))?;
    sys::enable_pktinfo_v4(&sock).map_err(Error::Io)?;
    let addr = if test_mode {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
    };
    sock.bind(&addr.into())?;
    Ok(sock)
}

fn bind_v6(port: u16, test_mode: bool) -> Result<Socket> {
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
    sys::enable_pktinfo_v6(&sock).map_err(Error::Io)?;
    let addr = if test_mode {
        SocketAddr::from((Ipv6Addr::LOCALHOST, port))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))
    };
    sock.bind(&addr.into())?;
    Ok(sock)
}

fn refresh_memberships(
    sock_v4: Option<&Socket>,
    sock_v6: Option<&Socket>,
    state: &RwLock<AnswerSet>,
    joined_v4: &mut Vec<Ipv4Addr>,
    joined_ifindexes: &mut Vec<u32>,
    force: bool,
) {
    // One read, so the addresses and the skip/allow lists are from the same
    // generation of the AnswerSet.
    let (v4, v6, allow, skip) = state
        .read()
        .map(|g| {
            (
                g.v4.clone(),
                g.v6.clone(),
                g.interfaces.clone(),
                g.skip_interfaces.clone(),
            )
        })
        .unwrap_or_default();

    let mut want_v4: Vec<Ipv4Addr> = v4.iter().map(|a| a.addr).collect();
    want_v4.sort_unstable();
    want_v4.dedup();
    let mut want_idx = mdns::preferred_iface_indexes(&allow, &skip);
    want_idx.sort_unstable();
    want_idx.dedup();

    if let Some(sock) = sock_v4 {
        if force || *joined_v4 != want_v4 {
            for ip in joined_v4.iter() {
                let _ = sock.leave_multicast_v4(&MDNS_GROUP_V4, ip);
            }
            joined_v4.clear();
            for a in &v4 {
                match sock.join_multicast_v4(&MDNS_GROUP_V4, &a.addr) {
                    Ok(()) => {
                        log::info!(
                            "mDNS multicast joined iface={} ifindex={} group={MDNS_GROUP_V4} local={}",
                            a.iface,
                            a.ifindex,
                            a.addr
                        );
                        if !joined_v4.contains(&a.addr) {
                            joined_v4.push(a.addr);
                        }
                    }
                    Err(e) => log::warn!(
                        "mDNS multicast join failed iface={} local={}: {e}",
                        a.iface,
                        a.addr
                    ),
                }
            }
            // Compare as sorted sets so re-deriving `want_v4` (sorted+deduped)
            // doesn't flap membership when the join order differs from the
            // declaration order.
            joined_v4.sort_unstable();
            joined_v4.dedup();
        }
    }

    if let Some(sock) = sock_v6 {
        if force || *joined_ifindexes != want_idx {
            for idx in joined_ifindexes.iter() {
                let _ = sock.leave_multicast_v6(&MDNS_GROUP_V6, *idx);
            }
            joined_ifindexes.clear();
            for idx in &want_idx {
                let iface_label = v6
                    .iter()
                    .find(|a| a.ifindex == *idx)
                    .map(|a| a.iface.as_str())
                    .or_else(|| {
                        v4.iter()
                            .find(|a| a.ifindex == *idx)
                            .map(|a| a.iface.as_str())
                    })
                    .unwrap_or("?");
                let local_v6: Vec<String> = v6
                    .iter()
                    .filter(|a| a.ifindex == *idx)
                    .map(|a| a.addr.to_string())
                    .collect();
                match sock.join_multicast_v6(&MDNS_GROUP_V6, *idx) {
                    Ok(()) => {
                        log::info!(
                            "mDNS multicast joined iface={iface_label} ifindex={idx} group={MDNS_GROUP_V6} local_v6={local_v6:?}"
                        );
                        joined_ifindexes.push(*idx);
                    }
                    Err(e) => log::warn!(
                        "mDNS multicast join failed iface={iface_label} ifindex={idx}: {e}"
                    ),
                }
            }
        }
    }
}

/// Parse a DNS query packet; returns [`None`] when the packet is not an
/// A/AAAA/ANY query we should answer.
pub fn parse_query(packet: &[u8]) -> Option<ParsedQuery> {
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

/// Build a response, or [`None`] when the query should be ignored.
pub fn build_response(
    query: &ParsedQuery,
    answers: &AnswerSet,
    querier: IpAddr,
    ifindex: Option<u32>,
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
        for ip in choose_v4_for_iface(answers, querier_v4, ifindex) {
            records.push((QTYPE_A, ip.octets().to_vec()));
        }
    }
    if want_aaaa {
        for ip in choose_v6_for_iface(answers, ifindex) {
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

/// Prefer addresses on the receiving ifindex; within those, same-subnet;
/// otherwise fall back to same-subnet globally, then all.
///
/// Builds a single result `Vec` with at most two linear scans — no intermediate
/// collections.
#[must_use]
pub fn choose_v4_for_iface(
    answers: &AnswerSet,
    querier: Ipv4Addr,
    ifindex: Option<u32>,
) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if let Some(idx) = ifindex {
        let has_iface = answers.v4.iter().any(|a| a.ifindex == idx);
        if has_iface {
            for a in &answers.v4 {
                if a.ifindex == idx && same_subnet(a.addr, a.mask, querier) {
                    out.push(a.addr);
                }
            }
            if !out.is_empty() {
                return out;
            }
            for a in &answers.v4 {
                if a.ifindex == idx {
                    out.push(a.addr);
                }
            }
            return out;
        }
    }
    choose_v4_into(answers, querier, &mut out);
    out
}

/// Prefer IPv6 addresses on the receiving ifindex; otherwise all configured v6.
///
/// Single scan into the result — no intermediate collections.
#[must_use]
pub fn choose_v6_for_iface(answers: &AnswerSet, ifindex: Option<u32>) -> Vec<Ipv6Addr> {
    let mut out = Vec::new();
    if let Some(idx) = ifindex {
        for a in &answers.v6 {
            if a.ifindex == idx {
                out.push(a.addr);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    for a in &answers.v6 {
        out.push(a.addr);
    }
    out
}

/// Prefer same-subnet IPv4 addresses; otherwise all configured v4.
#[must_use]
pub fn choose_v4(answers: &AnswerSet, querier: Ipv4Addr) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    choose_v4_into(answers, querier, &mut out);
    out
}

fn choose_v4_into(answers: &AnswerSet, querier: Ipv4Addr, out: &mut Vec<Ipv4Addr>) {
    debug_assert!(out.is_empty());
    for a in &answers.v4 {
        if same_subnet(a.addr, a.mask, querier) {
            out.push(a.addr);
        }
    }
    if !out.is_empty() {
        return;
    }
    for a in &answers.v4 {
        out.push(a.addr);
    }
}

fn same_subnet(addr: Ipv4Addr, mask: Ipv4Addr, querier: Ipv4Addr) -> bool {
    u32::from(addr) & u32::from(mask) == u32::from(querier) & u32::from(mask)
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
