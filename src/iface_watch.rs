//! Linux rtnetlink watcher for interface / address churn.
//!
//! Subscribes to `RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR` and
//! signals [`IfaceChange`] when a **relevant** interface (allow/skip filtered)
//! changes. Loopback, docker/veth/br-*, and configured skip prefixes are
//! ignored so a wireless-programmer job on `wlan0` does not re-announce
//! Ethernet mDNS. Events are debounced ([`DEBOUNCE`]) so a burst (DHCP, link
//! flap) becomes one signal.
//!
//! Payload parsing extracts the interface name / ifindex; the main loop still
//! re-scans addresses on each signal. Polling remains the fallback when
//! netlink is unavailable.
//!
//! The signal channel is bounded ([`IFACE_CHANGE_CAPACITY`]). Overflow drops
//! events: the signal is idempotent ("something changed"), so losing duplicates
//! under burst is safe and keeps memory bounded (§1.3 / §8.5).

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mdns;
use crate::sys;

const BIND_RETRY: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_millis(500);
/// Coalesce a burst of netlink events into one signal.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(400);

/// Bound on coalesced iface-change signals waiting for the main loop.
pub(crate) const IFACE_CHANGE_CAPACITY: usize = 32;

const NLMSG_HDRLEN: usize = 16;
const IFINFOMSG_LEN: usize = 16;
const IFADDRMSG_LEN: usize = 8;
const RTA_HDRLEN: usize = 4;

/// Signal that interface or address state may have changed on a used NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IfaceChange;

/// Spawn a netlink watcher thread. Returns a receiver of change signals and a
/// stop flag. Bind failures are retried quietly; they never crash the daemon.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn spawn() -> Result<(Receiver<IfaceChange>, Arc<AtomicBool>)> {
    spawn_filtered(Arc::new(RwLock::new(Config::default())))
}

/// Like [`spawn`], with a live config so skip/allow lists apply after reload.
pub(crate) fn spawn_filtered(
    config: Arc<RwLock<Config>>,
) -> Result<(Receiver<IfaceChange>, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::sync_channel(IFACE_CHANGE_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thr = Arc::clone(&stop);

    thread::Builder::new()
        .name("iface-watch".into())
        .spawn(move || {
            if let Err(e) = watch_loop(tx, stop_thr, config) {
                log::warn!("iface watcher stopped: {e}");
            }
        })
        .map_err(|e| Error::Other(format!("spawn iface-watch: {e}")))?;

    Ok((rx, stop))
}

fn watch_loop(
    tx: SyncSender<IfaceChange>,
    stop: Arc<AtomicBool>,
    config: Arc<RwLock<Config>>,
) -> Result<()> {
    let mut warned_bind = false;

    while !stop.load(Ordering::SeqCst) {
        let sock = match sys::open_rtnetlink(RECV_TIMEOUT) {
            Ok(s) => {
                if warned_bind {
                    log::info!("iface watcher: netlink socket recovered");
                    warned_bind = false;
                } else {
                    log::info!("iface watcher: listening on rtnetlink");
                }
                s
            }
            Err(e) => {
                if !warned_bind {
                    log::warn!(
                        "iface watcher: netlink bind failed: {e}; retrying (polling fallback active)"
                    );
                    warned_bind = true;
                } else {
                    log::debug!("iface watcher: netlink bind failed: {e}");
                }
                thread::sleep(BIND_RETRY);
                continue;
            }
        };

        let mut pending = false;
        let mut deadline = Instant::now();

        while !stop.load(Ordering::SeqCst) {
            let mut buf = [0u8; 8192];
            match sys::recv_netlink(&sock, &mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    let (allow, skip) = config
                        .read()
                        .map(|c| (c.interfaces.clone(), c.skip_interfaces.clone()))
                        .unwrap_or_default();
                    if netlink_is_relevant(&buf[..n], &allow, &skip) {
                        pending = true;
                        deadline = Instant::now() + DEBOUNCE;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    log::warn!("iface watcher: recv failed: {e}; rebinding");
                    break;
                }
            }

            if pending && Instant::now() >= deadline {
                loop {
                    buf = [0u8; 8192];
                    match sys::recv_netlink(&sock, &mut buf) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                pending = false;
                match tx.try_send(IfaceChange) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
            }
        }
    }
    Ok(())
}

/// True when `buf` contains a link/addr event for an interface we advertise on.
#[must_use]
pub(crate) fn netlink_is_relevant(buf: &[u8], allow: &[String], skip: &[String]) -> bool {
    for (ifindex, name) in parse_netlink_ifaces(buf) {
        let resolved = if name.is_empty() {
            name_for_ifindex(ifindex).unwrap_or_default()
        } else {
            name
        };
        if resolved.is_empty() {
            // Unknown name: let the main loop re-scan rather than drop the event.
            return true;
        }
        if mdns::iface_name_relevant(&resolved, allow, skip) {
            return true;
        }
    }
    false
}

/// Parse interface index + name from a netlink route dump / event buffer.
#[must_use]
pub(crate) fn parse_netlink_ifaces(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + NLMSG_HDRLEN <= buf.len() {
        let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap_or([0; 4])) as usize;
        if len < NLMSG_HDRLEN || off + len > buf.len() {
            break;
        }
        let nlmsg_type = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap_or([0; 2]));
        let payload = &buf[off + NLMSG_HDRLEN..off + len];
        match nlmsg_type {
            libc::RTM_NEWLINK | libc::RTM_DELLINK | libc::RTM_GETLINK
                if payload.len() >= IFINFOMSG_LEN =>
            {
                let ifindex =
                    i32::from_ne_bytes(payload[4..8].try_into().unwrap_or([0; 4])) as u32;
                let name =
                    rta_str(&payload[IFINFOMSG_LEN..], libc::IFLA_IFNAME).unwrap_or_default();
                if ifindex != 0 {
                    out.push((ifindex, name));
                }
            }
            libc::RTM_NEWADDR | libc::RTM_DELADDR if payload.len() >= IFADDRMSG_LEN => {
                let ifindex = u32::from_ne_bytes(payload[4..8].try_into().unwrap_or([0; 4]));
                let name =
                    rta_str(&payload[IFADDRMSG_LEN..], libc::IFA_LABEL).unwrap_or_default();
                if ifindex != 0 {
                    out.push((ifindex, name));
                }
            }
            _ => {}
        }
        off += align4(len);
    }
    out
}

fn rta_str(attrs: &[u8], want: u16) -> Option<String> {
    let mut off = 0usize;
    while off + RTA_HDRLEN <= attrs.len() {
        let rta_len = u16::from_ne_bytes(attrs[off..off + 2].try_into().ok()?) as usize;
        let rta_type = u16::from_ne_bytes(attrs[off + 2..off + 4].try_into().ok()?);
        if rta_len < RTA_HDRLEN || off + rta_len > attrs.len() {
            break;
        }
        if rta_type == want {
            let data = &attrs[off + RTA_HDRLEN..off + rta_len];
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            return String::from_utf8(data[..end].to_vec()).ok();
        }
        off += align4(rta_len);
    }
    None
}

fn align4(n: usize) -> usize {
    n.saturating_add(3) & !3
}

fn name_for_ifindex(ifindex: u32) -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let Ok(idx) = std::fs::read_to_string(entry.path().join("ifindex")) else {
            continue;
        };
        if idx.trim().parse::<u32>().ok() == Some(ifindex) {
            return Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

/// Drain any pending iface-change signals (for tests / main-loop coalescing).
pub(crate) fn drain(rx: &Receiver<IfaceChange>) {
    while rx.try_recv().is_ok() {}
}

/// Wait up to `timeout` for an iface-change, draining coalesced extras.
pub(crate) fn recv_timeout(
    rx: &Receiver<IfaceChange>,
    timeout: Duration,
) -> std::result::Result<IfaceChange, RecvTimeoutError> {
    let signal = rx.recv_timeout(timeout)?;
    drain(rx);
    Ok(signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn encode_rta(typ: u16, data: &[u8]) -> Vec<u8> {
        let rta_len = RTA_HDRLEN + data.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(rta_len as u16).to_ne_bytes());
        out.extend_from_slice(&typ.to_ne_bytes());
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    fn encode_nlmsg(nlmsg_type: u16, payload: &[u8]) -> Vec<u8> {
        let len = NLMSG_HDRLEN + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(len as u32).to_ne_bytes());
        out.extend_from_slice(&nlmsg_type.to_ne_bytes());
        out.extend_from_slice(&0u16.to_ne_bytes()); // flags
        out.extend_from_slice(&0u32.to_ne_bytes()); // seq
        out.extend_from_slice(&0u32.to_ne_bytes()); // pid
        out.extend_from_slice(payload);
        out
    }

    fn ifinfomsg(ifindex: i32) -> Vec<u8> {
        let mut p = vec![0u8; IFINFOMSG_LEN];
        p[4..8].copy_from_slice(&ifindex.to_ne_bytes());
        p
    }

    fn ifaddrmsg(ifindex: u32) -> Vec<u8> {
        let mut p = vec![0u8; IFADDRMSG_LEN];
        p[4..8].copy_from_slice(&ifindex.to_ne_bytes());
        p
    }

    fn link_event(ifindex: i32, name: &str) -> Vec<u8> {
        let mut payload = ifinfomsg(ifindex);
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.push(0);
        payload.extend_from_slice(&encode_rta(libc::IFLA_IFNAME, &name_bytes));
        encode_nlmsg(libc::RTM_NEWLINK, &payload)
    }

    fn addr_event(ifindex: u32, name: &str) -> Vec<u8> {
        let mut payload = ifaddrmsg(ifindex);
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.push(0);
        payload.extend_from_slice(&encode_rta(libc::IFA_LABEL, &name_bytes));
        encode_nlmsg(libc::RTM_NEWADDR, &payload)
    }

    #[test]
    fn parse_newlink_extracts_name() {
        let buf = link_event(2, "eth0");
        assert_eq!(parse_netlink_ifaces(&buf), vec![(2, "eth0".into())]);
    }

    #[test]
    fn parse_newaddr_extracts_label() {
        let buf = addr_event(3, "wlan0");
        assert_eq!(parse_netlink_ifaces(&buf), vec![(3, "wlan0".into())]);
    }

    #[test]
    fn skipped_wlan_is_not_relevant() {
        let buf = link_event(3, "wlan0");
        assert!(!netlink_is_relevant(&buf, &[], &["wlan".into()]));
        assert!(netlink_is_relevant(&buf, &[], &[]));
    }

    #[test]
    fn docker_veth_not_relevant() {
        assert!(!netlink_is_relevant(
            &link_event(10, "veth0abc"),
            &[],
            &[]
        ));
        assert!(!netlink_is_relevant(&link_event(11, "docker0"), &[], &[]));
        assert!(!netlink_is_relevant(&link_event(1, "lo"), &[], &[]));
    }

    #[test]
    fn eth_is_relevant_by_default() {
        assert!(netlink_is_relevant(&link_event(2, "eth0"), &[], &[]));
        assert!(netlink_is_relevant(
            &link_event(2, "eth0"),
            &["eth".into()],
            &[]
        ));
        assert!(!netlink_is_relevant(
            &link_event(2, "eth0"),
            &["wlan".into()],
            &[]
        ));
    }

    #[test]
    fn spawn_starts_and_stops() {
        let (rx, stop) = spawn().expect("spawn iface watcher");
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::SeqCst);
        let _ = rx.recv_timeout(Duration::from_millis(200));
    }

    #[test]
    fn iface_change_channel_is_bounded() {
        const { assert!(IFACE_CHANGE_CAPACITY > 0) };
    }

    #[test]
    fn iface_change_on_dummy_addr_add() {
        let name = format!("mdnstst{}", std::process::id() % 10000);
        let add = std::process::Command::new("ip")
            .args(["link", "add", &name, "type", "dummy"])
            .output();
        let Ok(out) = add else {
            eprintln!("skip: ip not available");
            return;
        };
        if !out.status.success() {
            eprintln!(
                "skip: cannot create dummy iface (need CAP_NET_ADMIN): {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        let (rx, stop) = spawn().expect("spawn");
        drain(&rx);

        let _ = std::process::Command::new("ip")
            .args(["link", "set", &name, "up"])
            .status();
        let _ = std::process::Command::new("ip")
            .args(["addr", "add", "192.0.2.10/32", "dev", &name])
            .status();

        let got = recv_timeout(&rx, Duration::from_secs(2)).is_ok();

        let _ = std::process::Command::new("ip")
            .args(["link", "del", &name])
            .status();
        stop.store(true, Ordering::SeqCst);

        assert!(got, "expected IfaceChange after adding address on {name}");
    }

    #[test]
    fn iface_change_on_dummy_down_up_same_addr() {
        let name = format!("mdnsdn{}", std::process::id() % 10000);
        let add = std::process::Command::new("ip")
            .args(["link", "add", &name, "type", "dummy"])
            .output();
        let Ok(out) = add else {
            eprintln!("skip: ip not available");
            return;
        };
        if !out.status.success() {
            eprintln!(
                "skip: cannot create dummy iface (need CAP_NET_ADMIN): {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }

        let _ = std::process::Command::new("ip")
            .args(["link", "set", &name, "up"])
            .status();
        let _ = std::process::Command::new("ip")
            .args(["addr", "add", "192.0.2.11/32", "dev", &name])
            .status();

        let (rx, stop) = spawn().expect("spawn");
        drain(&rx);

        let _ = std::process::Command::new("ip")
            .args(["link", "set", &name, "down"])
            .status();
        let _ = std::process::Command::new("ip")
            .args(["link", "set", &name, "up"])
            .status();

        let got = recv_timeout(&rx, Duration::from_secs(2)).is_ok();

        let _ = std::process::Command::new("ip")
            .args(["link", "del", &name])
            .status();
        stop.store(true, Ordering::SeqCst);

        assert!(
            got,
            "expected IfaceChange after down/up with the same address on {name}"
        );
    }

    #[test]
    fn suspend_detected_on_boottime_skew_jump() {
        let prev = Duration::from_millis(10);
        assert!(!crate::sys::suspend_detected(prev, prev));
        assert!(!crate::sys::suspend_detected(
            prev,
            prev + Duration::from_millis(500)
        ));
        assert!(crate::sys::suspend_detected(
            prev,
            prev + Duration::from_secs(2)
        ));
        assert!(crate::sys::suspend_detected(
            prev,
            prev + Duration::from_secs(60)
        ));
        assert!(!crate::sys::suspend_detected(
            Duration::from_secs(10),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn boottime_monotonic_skew_is_small_while_running() {
        let a = crate::sys::boottime_monotonic_skew();
        std::thread::sleep(Duration::from_millis(20));
        let b = crate::sys::boottime_monotonic_skew();
        assert!(
            !crate::sys::suspend_detected(a, b),
            "skew must not jump across the suspend threshold while running (a={a:?} b={b:?})"
        );
    }
}
