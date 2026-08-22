//! Local + live-daemon diagnostics for `microdns doctor`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{load_or_create, Config};
use crate::ctl::{self, DaemonDoctor};
use crate::error::Result;
use crate::legacy_unicast::IfaceAddr4;
use crate::mdns;
use crate::selfcheck;

/// Combined doctor output (local kernel state plus optional live daemon).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub interfaces: Vec<IfaceRow>,
    pub skip_interfaces: Vec<String>,
    pub allow_interfaces: Vec<String>,
    pub igmp: HashMap<String, Vec<String>>,
    pub igmp_raw: String,
    pub dev_mcast: String,
    pub daemon: Option<DaemonDoctor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IfaceRow {
    pub name: String,
    pub ifindex: u32,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
}

/// Collect a report from this host. Daemon fields require a running ctl socket.
pub fn collect(config_path: &Path, socket: &Path) -> Result<DoctorReport> {
    let cfg = load_or_create(config_path)?;
    Ok(collect_with_config(&cfg, socket))
}

#[must_use]
pub fn collect_with_config(cfg: &Config, socket: &Path) -> DoctorReport {
    let v4 = mdns::preferred_ipv4_ifaces(&cfg.interfaces, &cfg.skip_interfaces);
    let v6 = mdns::preferred_ipv6_addrs(&cfg.interfaces, &cfg.skip_interfaces);
    let interfaces = iface_rows(&v4, &v6);
    let igmp_raw = fs::read_to_string("/proc/net/igmp").unwrap_or_default();
    let igmp_parsed = selfcheck::parse_igmp(&igmp_raw);
    let igmp: HashMap<String, Vec<String>> = igmp_parsed
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|ip| ip.to_string()).collect()))
        .collect();
    let dev_mcast = fs::read_to_string("/proc/net/dev_mcast").unwrap_or_default();
    let daemon = ctl::doctor(socket).ok();
    DoctorReport {
        interfaces,
        skip_interfaces: cfg.skip_interfaces.clone(),
        allow_interfaces: cfg.interfaces.clone(),
        igmp,
        igmp_raw,
        dev_mcast,
        daemon,
    }
}

fn iface_rows(v4: &[IfaceAddr4], v6: &[crate::legacy_unicast::IfaceAddr6]) -> Vec<IfaceRow> {
    let mut names: Vec<String> = v4
        .iter()
        .map(|a| a.iface.clone())
        .chain(v6.iter().map(|a| a.iface.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let ifindex = v4
                .iter()
                .find(|a| a.iface == name)
                .map(|a| a.ifindex)
                .or_else(|| v6.iter().find(|a| a.iface == name).map(|a| a.ifindex))
                .unwrap_or(0);
            let ipv4: Vec<String> = v4
                .iter()
                .filter(|a| a.iface == name)
                .map(|a| a.addr.to_string())
                .collect();
            let ipv6: Vec<String> = v6
                .iter()
                .filter(|a| a.iface == name)
                .map(|a| a.addr.to_string())
                .collect();
            IfaceRow {
                name,
                ifindex,
                ipv4,
                ipv6,
            }
        })
        .collect()
}

pub fn print_human(w: &mut impl Write, report: &DoctorReport) -> Result<()> {
    writeln!(w, "interfaces (usable for mDNS):")?;
    if report.interfaces.is_empty() {
        writeln!(w, "  (none)")?;
    }
    for iface in &report.interfaces {
        writeln!(
            w,
            "  {} ifindex={} ipv4={:?} ipv6={:?}",
            iface.name, iface.ifindex, iface.ipv4, iface.ipv6
        )?;
    }
    writeln!(
        w,
        "allow={:?} skip={:?}",
        report.allow_interfaces, report.skip_interfaces
    )?;
    writeln!(w, "igmp groups:")?;
    if report.igmp.is_empty() {
        writeln!(w, "  (empty /proc/net/igmp)")?;
    }
    let mut names: Vec<_> = report.igmp.keys().cloned().collect();
    names.sort();
    for name in names {
        let groups = &report.igmp[&name];
        let mdns = groups.iter().any(|g| g == "224.0.0.251");
        writeln!(
            w,
            "  {name}: {:?} mdns={}",
            groups,
            if mdns { "yes" } else { "NO" }
        )?;
    }
    match &report.daemon {
        None => writeln!(w, "daemon: not running (ctl socket unreachable)")?,
        Some(d) => {
            writeln!(w, "daemon: running")?;
            writeln!(w, "  registered: {:?}", d.registered)?;
            writeln!(
                w,
                "  services: {}",
                d.services
                    .iter()
                    .map(|s| format!("{} {} :{}", s.name, s.type_, s.port))
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
            writeln!(
                w,
                "  metrics: register={} register-resend={} unregister={} unregister-resend={} respond={}",
                d.metrics.get("register").copied().unwrap_or(0),
                d.metrics.get("register-resend").copied().unwrap_or(0),
                d.metrics.get("unregister").copied().unwrap_or(0),
                d.metrics.get("unregister-resend").copied().unwrap_or(0),
                d.metrics.get("respond").copied().unwrap_or(0),
            )?;
            writeln!(w, "  last announce (seconds ago): {:?}", d.last_announce_secs_ago)?;
            writeln!(
                w,
                "  selfcheck: ok={} escalation={:?} {}",
                d.selfcheck.ok, d.selfcheck.escalation, d.selfcheck.message
            )?;
        }
    }
    Ok(())
}

pub fn print_json(w: &mut impl Write, report: &DoctorReport) -> Result<()> {
    serde_json::to_writer_pretty(&mut *w, report)?;
    writeln!(w)?;
    Ok(())
}
