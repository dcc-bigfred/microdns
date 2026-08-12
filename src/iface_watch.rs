//! Linux rtnetlink watcher for interface / address churn.
//!
//! Subscribes to `RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR` and
//! signals [`IfaceChange`] whenever anything readable arrives. Payload parsing
//! is intentionally skipped — the main loop re-scans interfaces on each signal.
//! Polling remains the fallback when netlink is unavailable.
//!
//! The signal channel is bounded ([`IFACE_CHANGE_CAPACITY`]). Overflow drops
//! events: the signal is idempotent ("something changed"), so losing duplicates
//! under burst is safe and keeps memory bounded (§1.3 / §8.5).

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::sys;

const BIND_RETRY: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// Bound on coalesced iface-change signals waiting for the main loop.
pub(crate) const IFACE_CHANGE_CAPACITY: usize = 32;

/// Signal that interface or address state may have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IfaceChange;

/// Spawn a netlink watcher thread. Returns a receiver of change signals and a
/// stop flag. Bind failures are retried quietly; they never crash the daemon.
pub(crate) fn spawn() -> Result<(Receiver<IfaceChange>, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::sync_channel(IFACE_CHANGE_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thr = Arc::clone(&stop);

    thread::Builder::new()
        .name("iface-watch".into())
        .spawn(move || {
            if let Err(e) = watch_loop(tx, stop_thr) {
                log::warn!("iface watcher stopped: {e}");
            }
        })
        .map_err(|e| Error::Other(format!("spawn iface-watch: {e}")))?;

    Ok((rx, stop))
}

fn watch_loop(tx: SyncSender<IfaceChange>, stop: Arc<AtomicBool>) -> Result<()> {
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

        while !stop.load(Ordering::SeqCst) {
            match sys::recv_netlink_any(&sock) {
                Ok(true) => match tx.try_send(IfaceChange) {
                    Ok(()) => {}
                    // Full: at least one change is already queued; drop extras.
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                },
                Ok(false) => {} // timeout
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    log::warn!("iface watcher: recv failed: {e}; rebinding");
                    break;
                }
            }
        }
    }
    Ok(())
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

    #[test]
    fn spawn_starts_and_stops() {
        let (rx, stop) = spawn().expect("spawn iface watcher");
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::SeqCst);
        let _ = rx.recv_timeout(Duration::from_millis(200));
    }

    #[test]
    fn iface_change_channel_is_bounded() {
        // Capacity is a compile-time constant; keep the check as a const assert.
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
}
