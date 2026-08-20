//! mDNS/DNS-SD registration via the `mdns-sd` crate.
//!
//! Skips docker/veth/br-* interfaces; prefers RUNNING non-loopback addresses.
//! Hostnames are advertised without a leading FQDN — we append `.local.`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::config::ServiceEntry;
use crate::error::{Error, Result};
use crate::legacy_unicast::{IfaceAddr4, IfaceAddr6};
use crate::version;

/// Tracked registrations keyed by full service name.
pub struct MdnsPublisher {
    daemon: Option<ServiceDaemon>,
    registered: Mutex<HashSet<String>>,
}

impl MdnsPublisher {
    /// Create a publisher. Succeeds even if the daemon cannot start yet;
    /// callers retry via [`ensure_daemon`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            daemon: None,
            registered: Mutex::new(HashSet::new()),
        }
    }

    /// Ensure the underlying `ServiceDaemon` is running.
    pub fn ensure_daemon(&mut self) -> Result<()> {
        if self.daemon.is_some() {
            return Ok(());
        }
        match ServiceDaemon::new() {
            Ok(d) => {
                log::info!("mDNS service daemon started");
                self.daemon = Some(d);
                Ok(())
            }
            Err(e) => Err(Error::Mdns(format!("ServiceDaemon::new: {e}"))),
        }
    }

    /// Register (or re-register) a service.
    ///
    /// Host A/AAAA records are published by mdns-sd via multicast so that local
    /// resolvers (e.g. avahi-daemon backing nss-mdns) cache them and the host
    /// can resolve its own `.local` names. The legacy unicast responder still
    /// answers A/AAAA for direct (non-5353) legacy queries, but mdns-sd owns
    /// the multicast A/AAAA announcements.
    pub fn register(
        &self,
        entry: &ServiceEntry,
        host_override: Option<&str>,
        allow: &[String],
        skip: &[String],
    ) -> Result<()> {
        let daemon = self
            .daemon
            .as_ref()
            .ok_or_else(|| Error::Mdns("daemon not started".into()))?;

        let ty = normalize_service_type(&entry.type_);
        let host = normalize_hostname(
            host_override
                .or(entry.host.as_deref())
                .unwrap_or(&version::hostname()),
        );
        let props = entry.txt.clone().unwrap_or_default();

        // Collect preferred IPv4 + IPv6 addresses (allow/skip filtered) so
        // mdns-sd publishes A/AAAA on the wire. Empty → addr_auto lets mdns-sd
        // fill from host interfaces when they appear later.
        let v4 = preferred_ipv4_addrs(allow, skip);
        let v6 = preferred_ipv6_addrs(allow, skip);
        let info = if v4.is_empty() && v6.is_empty() {
            ServiceInfo::new(&ty, &entry.name, &host, "", entry.port, props)
                .map_err(|e| Error::Mdns(e.to_string()))?
                .enable_addr_auto()
        } else {
            let mut ip_strs: Vec<String> = v6.iter().map(|a| a.addr.to_string()).collect();
            ip_strs.extend(v4.iter().map(|ip| ip.to_string()));
            let joined = ip_strs.join(",");
            ServiceInfo::new(&ty, &entry.name, &host, joined.as_str(), entry.port, props)
                .map_err(|e| Error::Mdns(e.to_string()))?
        };

        let fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| Error::Mdns(format!("register {fullname}: {e}")))?;

        if let Ok(mut set) = self.registered.lock() {
            set.insert(fullname.clone());
        }
        log::info!(
            "registered mDNS service instance={} type={} host={} port={}",
            entry.name,
            ty,
            host,
            entry.port
        );
        Ok(())
    }

    /// Unregister a previously registered fullname (best-effort).
    pub fn unregister(&self, fullname: &str) -> Result<()> {
        let daemon = match self.daemon.as_ref() {
            Some(d) => d,
            None => return Ok(()),
        };
        daemon
            .unregister(fullname)
            .map_err(|e| Error::Mdns(format!("unregister {fullname}: {e}")))?;
        if let Ok(mut set) = self.registered.lock() {
            set.remove(fullname);
        }
        Ok(())
    }

    /// Snapshot of currently registered fullnames.
    pub fn registered_names(&self) -> HashSet<String> {
        self.registered
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Build the DNS-SD fullname for an instance + type (for tracking).
    #[must_use]
    pub fn fullname(instance: &str, type_: &str) -> String {
        let ty = normalize_service_type(type_);
        format!("{instance}.{ty}")
    }

    /// Shut down the daemon.
    pub fn shutdown(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            let names: Vec<String> = self.registered_names().into_iter().collect();
            for name in names {
                let _ = daemon.unregister(&name);
            }
            let _ = daemon.shutdown();
        }
        if let Ok(mut set) = self.registered.lock() {
            set.clear();
        }
    }

    /// Drop the current `ServiceDaemon` and start a new one.
    ///
    /// Needed after suspend/resume: mdns-sd only rejoins multicast when the IP
    /// set changes, so the old sockets keep a stale NIC filter. A new daemon
    /// binds fresh UDP sockets and joins `224.0.0.251` from scratch.
    pub fn recreate_daemon(&mut self) -> Result<()> {
        self.shutdown();
        self.ensure_daemon()
    }
}

impl Default for MdnsPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MdnsPublisher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Ensure service type ends with `.local.` (e.g. `_http._tcp` → `_http._tcp.local.`).
#[must_use]
pub fn normalize_service_type(type_: &str) -> String {
    let t = type_.trim().trim_end_matches('.');
    if t.ends_with(".local") {
        format!("{t}.")
    } else {
        format!("{t}.local.")
    }
}

/// Ensure hostname ends with `.local.` and has no extra dots stripped wrongly.
/// Input `"bigfred"` → `"bigfred.local."`; `"bigfred.local"` → `"bigfred.local."`.
#[must_use]
pub fn normalize_hostname(host: &str) -> String {
    let h = host.trim().trim_end_matches('.');
    let bare = h.strip_suffix(".local").unwrap_or(h);
    format!("{bare}.local.")
}

/// Collect preferred IPv4 addresses: running, non-loopback, allowlisted, not skipped.
#[must_use]
pub fn preferred_ipv4_addrs(allow: &[String], skip: &[String]) -> Vec<Ipv4Addr> {
    preferred_ipv4_ifaces(allow, skip)
        .into_iter()
        .map(|a| a.addr)
        .collect()
}

/// Preferred IPv4 addresses with netmasks and ifindex (for per-iface replies).
#[must_use]
pub fn preferred_ipv4_ifaces(allow: &[String], skip: &[String]) -> Vec<IfaceAddr4> {
    let mut addrs = Vec::new();
    let Ok(ifaces) = list_interfaces() else {
        return addrs;
    };
    for iface in ifaces {
        if !iface_usable(&iface, allow, skip) {
            continue;
        }
        for (ip, mask) in iface.ipv4 {
            if !ip.is_loopback() && !ip.is_unspecified() {
                addrs.push(IfaceAddr4 {
                    iface: iface.name.clone(),
                    addr: ip,
                    mask,
                    ifindex: iface.ifindex,
                });
            }
        }
    }
    addrs
}

/// Preferred global/ULA IPv6 addresses with ifindex (no loopback/unspecified/link-local).
#[must_use]
pub fn preferred_ipv6_addrs(allow: &[String], skip: &[String]) -> Vec<IfaceAddr6> {
    let mut addrs = Vec::new();
    let Ok(ifaces) = list_interfaces() else {
        return addrs;
    };
    for iface in ifaces {
        if !iface_usable(&iface, allow, skip) {
            continue;
        }
        for ip in iface.ipv6 {
            if ip.is_loopback() || ip.is_unspecified() || is_ipv6_link_local(&ip) {
                continue;
            }
            addrs.push(IfaceAddr6 {
                iface: iface.name.clone(),
                addr: ip,
                ifindex: iface.ifindex,
            });
        }
    }
    addrs
}

/// Interface index for multicast group joins (IPv6).
#[must_use]
pub fn preferred_iface_indexes(allow: &[String], skip: &[String]) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(ifaces) = list_interfaces() else {
        return out;
    };
    for iface in ifaces {
        if !iface_usable(&iface, allow, skip) {
            continue;
        }
        if iface.ipv4.is_empty() && iface.ipv6.is_empty() {
            continue;
        }
        if iface.ifindex != 0 {
            out.push(iface.ifindex);
        }
    }
    out
}

fn iface_usable(iface: &IfaceInfo, allow: &[String], skip: &[String]) -> bool {
    if should_skip_iface(&iface.name, skip) {
        return false;
    }
    if !is_allowed_iface(&iface.name, allow) {
        return false;
    }
    if !iface.is_up || iface.is_loopback {
        return false;
    }
    true
}

/// Whether the link is usable for mDNS: `IFF_RUNNING` or `operstate == "up"`.
///
/// `IFF_UP` alone is not enough — after suspend a Wi‑Fi NIC often stays
/// administratively up (`IFF_UP`) with `operstate=dormant` and a stale DHCP
/// address. Treating that as live skips the multicast rejoin.
#[must_use]
pub fn iface_link_ready(flags_val: u32, operstate: &str) -> bool {
    (flags_val & libc::IFF_RUNNING as u32) != 0 || operstate.eq_ignore_ascii_case("up")
}

fn is_ipv6_link_local(ip: &Ipv6Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80
}

/// Whether `name` is allowed by the optional allowlist.
///
/// Empty `allow` means every interface is allowed. Otherwise matching is by
/// case-insensitive name **prefix**, same rules as [`should_skip_iface`].
#[must_use]
pub fn is_allowed_iface(name: &str, allow: &[String]) -> bool {
    if allow.is_empty() {
        return true;
    }
    let n = name.to_ascii_lowercase();
    allow.iter().any(|p| {
        let p = p.trim();
        !p.is_empty()
            && n.get(..p.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })
}

/// Whether to skip an interface by name.
///
/// Always skips the built-in container/virtual bridge interfaces
/// (docker/veth/br-*/cni/flannel/virbr). The `skip` list adds extra
/// case-insensitive **prefix** matches from configuration (e.g. `["wlan"]` on
/// the BigFred hub, where `wireless-programmer` owns the WiFi radio and mDNS
/// must not leak onto a device config network). By default `wlan*` is NOT
/// skipped, so mDNS advertises on WiFi on a generic/laptop install.
#[must_use]
pub fn should_skip_iface(name: &str, skip: &[String]) -> bool {
    let n = name.to_ascii_lowercase();
    if n == "docker0"
        || n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br-")
        || n.starts_with("br0")
        || n == "cni0"
        || n.starts_with("flannel")
        || n.starts_with("virbr")
    {
        return true;
    }
    // Prefix match without allocating: `n` is already lowercased, so compare
    // its head case-insensitively. `get` rather than slicing, so an entry whose
    // byte length lands mid-char cannot panic.
    skip.iter().any(|p| {
        let p = p.trim();
        !p.is_empty()
            && n.get(..p.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })
}

#[derive(Debug)]
struct IfaceInfo {
    name: String,
    is_up: bool,
    is_loopback: bool,
    ifindex: u32,
    ipv4: Vec<(Ipv4Addr, Ipv4Addr)>,
    ipv6: Vec<Ipv6Addr>,
}

fn list_interfaces() -> Result<Vec<IfaceInfo>> {
    // Prefer /sys + /proc to avoid extra deps; fall back to empty on failure.
    let mut out = Vec::new();
    let entries = match fs::read_dir("/sys/class/net") {
        Ok(e) => e,
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let operstate = fs::read_to_string(entry.path().join("operstate"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let flags = fs::read_to_string(entry.path().join("flags")).unwrap_or_default();
        let flags_val = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).unwrap_or(0);
        // IFF_RUNNING (carrier) or operstate=up — not IFF_UP alone (see iface_link_ready).
        let is_up = iface_link_ready(flags_val, &operstate);
        let is_loopback = (flags_val & libc::IFF_LOOPBACK as u32) != 0 || name == "lo";
        let ifindex = fs::read_to_string(entry.path().join("ifindex"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let (ipv4, ipv6) = addrs_for_iface(&name);
        out.push(IfaceInfo {
            name,
            is_up,
            is_loopback,
            ifindex,
            ipv4,
            ipv6,
        });
    }
    Ok(out)
}

fn addrs_for_iface(name: &str) -> (Vec<(Ipv4Addr, Ipv4Addr)>, Vec<Ipv6Addr>) {
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return (ipv4, ipv6);
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let iface = &*cur;
            let iname = if iface.ifa_name.is_null() {
                ""
            } else {
                std::ffi::CStr::from_ptr(iface.ifa_name)
                    .to_str()
                    .unwrap_or("")
            };
            if iname == name {
                if let Some(addr) = iface.ifa_addr.as_ref() {
                    if addr.sa_family as i32 == libc::AF_INET {
                        let sin = &*(iface.ifa_addr as *const libc::sockaddr_in);
                        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                        let mask = if iface.ifa_netmask.is_null() {
                            Ipv4Addr::new(255, 255, 255, 0)
                        } else {
                            let smask = &*(iface.ifa_netmask as *const libc::sockaddr_in);
                            Ipv4Addr::from(u32::from_be(smask.sin_addr.s_addr))
                        };
                        ipv4.push((ip, mask));
                    } else if addr.sa_family as i32 == libc::AF_INET6 {
                        let sin6 = &*(iface.ifa_addr as *const libc::sockaddr_in6);
                        let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                        ipv6.push(ip);
                    }
                }
            }
            cur = iface.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    (ipv4, ipv6)
}

/// Helper to register a dynamic dcc-bus service (`_z21._udp` / `_withrottle._tcp`).
///
/// TXT: `layoutId`, `commandStationId`, `proto`, optional `layoutName`, and for
/// Z21 also `serial`.
pub fn dcc_service_entry(
    instance: &str,
    type_: &str,
    protocol: &str,
    port: u16,
    layout_id: u32,
    command_station_id: u32,
    layout_name: &str,
    serial: Option<u32>,
) -> ServiceEntry {
    let mut txt = HashMap::new();
    txt.insert("proto".into(), protocol.into());
    txt.insert("layoutId".into(), layout_id.to_string());
    txt.insert("commandStationId".into(), command_station_id.to_string());
    if !layout_name.is_empty() {
        txt.insert("layoutName".into(), layout_name.into());
    }
    if let Some(serial) = serial {
        txt.insert("serial".into(), serial.to_string());
    }
    ServiceEntry {
        name: instance.into(),
        type_: type_.into(),
        protocol: protocol.into(),
        port,
        host: None,
        txt: Some(txt),
    }
}

/// Check whether any preferred interface currently has an IPv4 address.
#[must_use]
pub fn has_usable_iface(allow: &[String], skip: &[String]) -> bool {
    !preferred_ipv4_addrs(allow, skip).is_empty()
}

/// Return first preferred IP as [`IpAddr`], if any.
#[must_use]
pub fn primary_ip(allow: &[String], skip: &[String]) -> Option<IpAddr> {
    preferred_ipv4_addrs(allow, skip)
        .into_iter()
        .next()
        .map(IpAddr::V4)
}
