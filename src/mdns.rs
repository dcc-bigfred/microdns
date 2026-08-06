//! mDNS/DNS-SD registration via the `mdns-sd` crate.
//!
//! Skips docker/veth/br-* interfaces; prefers UP non-loopback addresses.
//! Hostnames are advertised without a leading FQDN — we append `.local.`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::config::ServiceEntry;
use crate::error::{Error, Result};
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

    /// Register (or re-register) a service. Uses auto addresses when available.
    pub fn register(&self, entry: &ServiceEntry, host_override: Option<&str>) -> Result<()> {
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
        let ips = preferred_ipv4_addrs();

        let info = if ips.is_empty() {
            // No usable interface yet — register with addr_auto so mdns-sd
            // fills addresses when interfaces appear.
            ServiceInfo::new(&ty, &entry.name, &host, "", entry.port, props.clone())
                .map_err(|e| Error::Mdns(e.to_string()))?
                .enable_addr_auto()
        } else {
            let ip_strs: Vec<String> = ips.iter().map(|ip| ip.to_string()).collect();
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

/// Collect preferred IPv4 addresses: UP, non-loopback, not docker/veth/br-*.
#[must_use]
pub fn preferred_ipv4_addrs() -> Vec<Ipv4Addr> {
    let mut addrs = Vec::new();
    let Ok(ifaces) = list_interfaces() else {
        return addrs;
    };
    for iface in ifaces {
        if should_skip_iface(&iface.name) {
            continue;
        }
        if !iface.is_up || iface.is_loopback {
            continue;
        }
        for ip in iface.ipv4 {
            if !ip.is_loopback() && !ip.is_unspecified() {
                addrs.push(ip);
            }
        }
    }
    addrs
}

/// Whether to skip an interface by name (docker/veth/bridge).
#[must_use]
pub fn should_skip_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "docker0"
        || n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br-")
        || n.starts_with("br0")
        || n == "cni0"
        || n.starts_with("flannel")
        || n.starts_with("virbr")
}

#[derive(Debug)]
struct IfaceInfo {
    name: String,
    is_up: bool,
    is_loopback: bool,
    ipv4: Vec<Ipv4Addr>,
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
        // IFF_UP=0x1, IFF_LOOPBACK=0x8
        let is_up = (flags_val & 0x1) != 0 || operstate == "up";
        let is_loopback = (flags_val & 0x8) != 0 || name == "lo";
        let ipv4 = ipv4_for_iface(&name);
        out.push(IfaceInfo {
            name,
            is_up,
            is_loopback,
            ipv4,
        });
    }
    Ok(out)
}

fn ipv4_for_iface(name: &str) -> Vec<Ipv4Addr> {
    let mut ips = Vec::new();
    // Parse `ip -o -4 addr show` is unavailable as a dep; use /proc/net/fib_trie
    // is complex. Instead read from `getifaddrs` via libc.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return ips;
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
                        ips.push(ip);
                    }
                }
            }
            cur = iface.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    ips
}

/// Helper to register a dynamic dcc-bus service (`_z21._udp` / `_withrottle._tcp`).
pub fn dcc_service_entry(instance: &str, type_: &str, protocol: &str, port: u16) -> ServiceEntry {
    let mut txt = HashMap::new();
    txt.insert("proto".into(), protocol.into());
    ServiceEntry {
        name: instance.into(),
        type_: type_.into(),
        protocol: protocol.into(),
        port,
        host: Some(instance.into()),
        txt: Some(txt),
    }
}

/// Check whether any preferred interface currently has an IPv4 address.
#[must_use]
pub fn has_usable_iface() -> bool {
    !preferred_ipv4_addrs().is_empty()
}

/// Return first preferred IP as [`IpAddr`], if any.
#[must_use]
pub fn primary_ip() -> Option<IpAddr> {
    preferred_ipv4_addrs().into_iter().next().map(IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_type() {
        assert_eq!(normalize_service_type("_http._tcp"), "_http._tcp.local.");
        assert_eq!(
            normalize_service_type("_http._tcp.local"),
            "_http._tcp.local."
        );
        assert_eq!(
            normalize_service_type("_http._tcp.local."),
            "_http._tcp.local."
        );
    }

    #[test]
    fn normalize_host() {
        assert_eq!(normalize_hostname("bigfred"), "bigfred.local.");
        assert_eq!(normalize_hostname("bigfred.local"), "bigfred.local.");
        assert_eq!(normalize_hostname("bigfred.local."), "bigfred.local.");
    }

    #[test]
    fn skip_virtual_ifaces() {
        assert!(should_skip_iface("veth0abc"));
        assert!(should_skip_iface("br-1234abcd"));
        assert!(should_skip_iface("docker0"));
        assert!(!should_skip_iface("eth0"));
        assert!(!should_skip_iface("wlan0"));
        assert!(!should_skip_iface("enp1s0"));
    }

    #[test]
    fn dcc_entry_has_proto_txt() {
        let e = dcc_service_entry("hub1", "_z21._udp", "udp", 21105);
        assert_eq!(e.txt.as_ref().unwrap().get("proto").unwrap(), "udp");
    }
}
