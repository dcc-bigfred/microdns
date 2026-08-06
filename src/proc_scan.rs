//! Scan `/proc` for listen sockets belonging to a process.
//!
//! Finds socket inodes via `/proc/<pid>/fd`, then matches them against
//! `/proc/net/tcp{,6}` (LISTEN state `0A`) and `/proc/net/udp{,6}`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::error::Result;

/// Ports found listening for a process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListenPorts {
    pub tcp: HashSet<u16>,
    pub udp: HashSet<u16>,
}

impl ListenPorts {
    #[must_use]
    pub fn has_tcp(&self, port: u16) -> bool {
        self.tcp.contains(&port)
    }

    #[must_use]
    pub fn has_udp(&self, port: u16) -> bool {
        self.udp.contains(&port)
    }
}

/// Collect listen ports for `pid` by matching socket inodes.
pub fn listen_ports_for_pid(pid: i32) -> Result<ListenPorts> {
    let inodes = socket_inodes(pid)?;
    if inodes.is_empty() {
        return Ok(ListenPorts::default());
    }

    let mut out = ListenPorts::default();
    collect_tcp("/proc/net/tcp", &inodes, &mut out.tcp)?;
    collect_tcp("/proc/net/tcp6", &inodes, &mut out.tcp)?;
    collect_udp("/proc/net/udp", &inodes, &mut out.udp)?;
    collect_udp("/proc/net/udp6", &inodes, &mut out.udp)?;
    Ok(out)
}

fn socket_inodes(pid: i32) -> Result<HashSet<u64>> {
    let fd_dir = format!("/proc/{pid}/fd");
    let mut inodes = HashSet::new();
    let entries = match fs::read_dir(&fd_dir) {
        Ok(e) => e,
        Err(_) => return Ok(inodes),
    };
    for entry in entries.flatten() {
        let link = match fs::read_link(entry.path()) {
            Ok(l) => l,
            Err(_) => continue,
        };
        if let Some(inode) = parse_socket_link(&link) {
            inodes.insert(inode);
        }
    }
    Ok(inodes)
}

/// Parse `socket:[12345]` symlink target.
fn parse_socket_link(path: &Path) -> Option<u64> {
    let s = path.to_str()?;
    let rest = s.strip_prefix("socket:[")?;
    let num = rest.strip_suffix(']')?;
    num.parse().ok()
}

/// TCP LISTEN state in `/proc/net/tcp*` is hex `0A`.
const TCP_LISTEN: &str = "0A";

fn collect_tcp(path: &str, inodes: &HashSet<u64>, out: &mut HashSet<u16>) -> Result<()> {
    let data = match fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    for (i, line) in data.lines().enumerate() {
        if i == 0 {
            continue; // header
        }
        if let Some(port) = parse_net_line(line, inodes, Some(TCP_LISTEN)) {
            out.insert(port);
        }
    }
    Ok(())
}

fn collect_udp(path: &str, inodes: &HashSet<u64>, out: &mut HashSet<u16>) -> Result<()> {
    let data = match fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    for (i, line) in data.lines().enumerate() {
        if i == 0 {
            continue;
        }
        // UDP has no LISTEN state filter; presence of the inode is enough.
        if let Some(port) = parse_net_line(line, inodes, None) {
            out.insert(port);
        }
    }
    Ok(())
}

/// Parse a `/proc/net/{tcp,udp}{,6}` data line.
///
/// Columns (whitespace-separated): sl, local_address, rem_address, st, ...
/// local_address is `IP:PORT` in hex. Inode is typically column index 9.
fn parse_net_line(line: &str, inodes: &HashSet<u64>, require_state: Option<&str>) -> Option<u16> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 10 {
        return None;
    }
    if let Some(st) = require_state {
        if !cols[3].eq_ignore_ascii_case(st) {
            return None;
        }
    }
    let inode: u64 = cols[9].parse().ok()?;
    if !inodes.contains(&inode) {
        return None;
    }
    let local = cols[1];
    let port_hex = local.rsplit_once(':')?.1;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
}
