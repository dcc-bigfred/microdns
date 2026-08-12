//! Linux rtnetlink watcher for interface / address churn.
//!
//! Subscribes to `RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR` and
//! signals [`IfaceChange`] whenever anything readable arrives. Payload parsing
//! is intentionally skipped — the main loop re-scans interfaces on each signal.
//! Polling remains the fallback when netlink is unavailable.

use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};

const BIND_RETRY: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// Signal that interface or address state may have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfaceChange;

/// Spawn a netlink watcher thread. Returns a receiver of change signals and a
/// stop flag. Bind failures are retried quietly; they never crash the daemon.
pub fn spawn() -> Result<(Receiver<IfaceChange>, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::channel();
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

fn watch_loop(tx: Sender<IfaceChange>, stop: Arc<AtomicBool>) -> Result<()> {
    let mut warned_bind = false;

    while !stop.load(Ordering::SeqCst) {
        let sock = match open_netlink() {
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
                    log::warn!("iface watcher: netlink bind failed: {e}; retrying (polling fallback active)");
                    warned_bind = true;
                } else {
                    log::debug!("iface watcher: netlink bind failed: {e}");
                }
                thread::sleep(BIND_RETRY);
                continue;
            }
        };

        while !stop.load(Ordering::SeqCst) {
            match recv_any(&sock) {
                Ok(true) => {
                    let _ = tx.send(IfaceChange);
                }
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

fn open_netlink() -> std::io::Result<OwnedFd> {
    // SAFETY: socket returns a new fd or -1; we wrap successful fds in OwnedFd.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };

    let tv = libc::timeval {
        tv_sec: 0,
        tv_usec: (RECV_TIMEOUT.as_millis() as i64) * 1000,
    };
    // SAFETY: sock is a valid netlink fd; timeval is valid for SO_RCVTIMEO.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            mem::size_of_val(&tv) as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR) as u32;
    let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = groups;

    // SAFETY: bind with a sockaddr_nl of correct size.
    let rc = unsafe {
        libc::bind(
            sock.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sock)
}

/// Returns `Ok(true)` when at least one netlink message was read.
fn recv_any(sock: &OwnedFd) -> std::io::Result<bool> {
    let mut buf = [0u8; 8192];
    let mut iov = [IoSliceMut::new(&mut buf)];
    // We only care that something arrived; ignore payload.
    // SAFETY: recvmsg with a valid fd and iovec.
    let n = unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = iov.as_mut_ptr() as *mut libc::iovec;
        msg.msg_iovlen = 1;
        libc::recvmsg(sock.as_raw_fd(), &mut msg, 0)
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
            return Ok(false);
        }
        return Err(err);
    }
    Ok(n > 0)
}
