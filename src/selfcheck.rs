//! Periodic self-verification of mDNS multicast membership and announcements.

use std::collections::HashMap;
use std::fs;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::legacy_unicast::IfaceAddr4;
use crate::mdns::{AnnounceLog, MdnsPublisher};

/// IPv4 mDNS group 224.0.0.251, as it appears in `/proc/net/igmp` (LE hex).
pub const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// Snapshot of the last self-check, shared with `microdns doctor`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub ok: bool,
    pub igmp_ok: bool,
    pub daemon_alive: bool,
    pub announce_fresh: bool,
    pub missing_igmp_ifaces: Vec<String>,
    pub stale_services: Vec<String>,
    pub escalation: Escalation,
    pub message: String,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            ok: true,
            igmp_ok: true,
            daemon_alive: true,
            announce_fresh: true,
            missing_igmp_ifaces: Vec::new(),
            stale_services: Vec::new(),
            escalation: Escalation::None,
            message: "not checked yet".into(),
        }
    }
}

/// How far the main loop has gone to recover from a failed check.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Escalation {
    None,
    Reannounce,
    RecreateDaemon,
}

/// Parse `/proc/net/igmp` into iface → joined groups.
///
/// Group addresses are stored little-endian hex (e.g. `FB0000E0` = 224.0.0.251).
#[must_use]
pub fn parse_igmp(contents: &str) -> HashMap<String, Vec<Ipv4Addr>> {
    let mut out: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Idx") {
            continue;
        }
        if !line.starts_with(|c: char| c.is_whitespace()) {
            // `2       eth0      :     2      V3`
            let mut parts = trimmed.split_whitespace();
            let _idx = parts.next();
            if let Some(name) = parts.next() {
                let name = name.trim_end_matches(':').to_string();
                current = Some(name.clone());
                out.entry(name).or_default();
            }
            continue;
        }
        if let Some(name) = &current {
            if let Some(token) = trimmed.split_whitespace().next() {
                if let Some(ip) = parse_igmp_group(token) {
                    out.entry(name.clone()).or_default().push(ip);
                }
            }
        }
    }
    out
}

fn parse_igmp_group(token: &str) -> Option<Ipv4Addr> {
    let hex = token.trim();
    if hex.len() != 8 {
        return None;
    }
    let le = u32::from_str_radix(hex, 16).ok()?;
    // /proc/net/igmp prints the address in host nibble order on little-endian:
    // 224.0.0.251 (0xE00000FB) → FB0000E0.
    Some(Ipv4Addr::from(le.to_le_bytes()))
}

/// Ifaces from `want` that do not currently have 224.0.0.251 joined.
#[must_use]
pub fn missing_mdns_membership(
    igmp: &HashMap<String, Vec<Ipv4Addr>>,
    want: &[IfaceAddr4],
) -> Vec<String> {
    let mut missing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in want {
        if !seen.insert(&a.iface) {
            continue;
        }
        let groups = igmp.get(&a.iface).map(Vec::as_slice).unwrap_or(&[]);
        if !groups.contains(&MDNS_GROUP_V4) {
            missing.push(a.iface.clone());
        }
    }
    missing.sort();
    missing
}

/// Services whose last `Announce` event is older than `fresh_for`.
#[must_use]
pub fn stale_announces(
    expected: &[String],
    log: &AnnounceLog,
    fresh_for: Duration,
    now: Instant,
) -> Vec<String> {
    let snap = log.snapshot();
    let mut stale = Vec::new();
    for name in expected {
        match snap.get(name) {
            Some(at) if now.saturating_duration_since(*at) <= fresh_for => {}
            _ => stale.push(name.clone()),
        }
    }
    stale.sort();
    stale
}

/// Run one check against live kernel + daemon state.
#[must_use]
pub fn evaluate(
    publisher: &MdnsPublisher,
    want_v4: &[IfaceAddr4],
    expected_names: &[String],
    fresh_for: Duration,
    now: Instant,
) -> Report {
    let igmp_raw = fs::read_to_string("/proc/net/igmp").unwrap_or_default();
    let igmp = parse_igmp(&igmp_raw);
    let missing_igmp = if want_v4.is_empty() {
        Vec::new()
    } else {
        missing_mdns_membership(&igmp, want_v4)
    };
    let daemon_alive = publisher.daemon_alive();
    let log_empty = publisher.announce_log().snapshot().is_empty();
    let stale = if expected_names.is_empty() || log_empty {
        // Cold start / monitor not yet delivering Announce events.
        Vec::new()
    } else {
        stale_announces(expected_names, publisher.announce_log(), fresh_for, now)
    };
    let igmp_ok = missing_igmp.is_empty();
    let announce_fresh = stale.is_empty();
    let ok = igmp_ok && daemon_alive && announce_fresh;
    let message = if ok {
        "ok".into()
    } else {
        let mut parts = Vec::new();
        if !daemon_alive {
            parts.push("mdns-sd thread not running".to_string());
        }
        if !igmp_ok {
            parts.push(format!(
                "224.0.0.251 not joined on {}",
                missing_igmp.join(",")
            ));
        }
        if !announce_fresh {
            parts.push(format!("stale announces: {}", stale.join(",")));
        }
        parts.join("; ")
    };
    Report {
        ok,
        igmp_ok,
        daemon_alive,
        announce_fresh,
        missing_igmp_ifaces: missing_igmp,
        stale_services: stale,
        escalation: Escalation::None,
        message,
    }
}
