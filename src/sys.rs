//! Isolated Linux socket / netlink FFI helpers.
//!
//! Keeps `unsafe` concentrated here with explicit `SAFETY` proofs. Callers in
//! `iface_watch` and `legacy_unicast` use only the safe wrappers below.

use std::io::{ErrorKind, IoSliceMut};
use std::mem::{self, MaybeUninit};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Once;
use std::time::Duration;

use socket2::{SockAddr, SockAddrStorage, Socket};

/// Minimum jump in `CLOCK_BOOTTIME − CLOCK_MONOTONIC` treated as suspend/resume.
/// Monotonic pauses during sleep; boottime does not, so the skew jumps by the
/// sleep duration. 2 s is well above scheduling noise and well below a real
/// suspend.
pub const SUSPEND_SKEW_THRESHOLD: Duration = Duration::from_secs(2);

const CMSG_BUF: usize = 256;

/// Open an `AF_NETLINK` / `NETLINK_ROUTE` socket subscribed to link + address
/// multicast groups, with a receive timeout.
pub fn open_rtnetlink(recv_timeout: Duration) -> std::io::Result<OwnedFd> {
    // SAFETY: `socket` returns a fresh fd on success or -1 on failure. We only
    // wrap non-negative fds in `OwnedFd`, which takes exclusive ownership and
    // closes on drop. `SOCK_CLOEXEC` prevents fd leaks across `exec`.
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
    // SAFETY: `fd` is a freshly created, exclusive netlink socket fd.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };

    let usec = i64::try_from(recv_timeout.as_micros()).unwrap_or(i64::MAX);
    let tv = libc::timeval {
        tv_sec: usec / 1_000_000,
        tv_usec: usec % 1_000_000,
    };
    // SAFETY: `sock` is a valid netlink fd owned by us. `tv` is a properly
    // aligned `timeval` whose lifetime covers the setsockopt call. SO_RCVTIMEO
    // expects exactly that layout; no aliasing with other Rust references.
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
    // SAFETY: zeroed sockaddr_nl is a valid initial state; we then set family
    // and groups before bind. No concurrent access to `addr`.
    let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = groups;

    // SAFETY: `sock` is a valid netlink fd; `addr` is a fully initialized
    // sockaddr_nl of the size passed as the third argument. bind does not
    // retain the pointer after return.
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

/// Returns `Ok(true)` when at least one netlink message was read (payload ignored).
pub fn recv_netlink_any(sock: &OwnedFd) -> std::io::Result<bool> {
    let mut buf = [0u8; 8192];
    let mut iov = [IoSliceMut::new(&mut buf)];
    // SAFETY: `sock` is a valid netlink fd. `iov` points at a live mutable
    // buffer of known length for the duration of recvmsg. We do not retain
    // pointers after return; payload is discarded.
    let n = unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = iov.as_mut_ptr().cast();
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
    debug_assert!(n >= 0);
    Ok(n > 0)
}

/// Enable `IP_PKTINFO` so recvmsg delivers receiving-interface metadata (IPv4).
pub fn enable_pktinfo_v4(sock: &Socket) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: `sock` is a live UDP socket fd. `on` is a properly aligned int
    // whose lifetime covers the call. IP_PKTINFO expects a c_int value.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            &on as *const _ as *const libc::c_void,
            mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Enable `IPV6_RECVPKTINFO` so recvmsg delivers receiving-interface metadata (IPv6).
pub fn enable_pktinfo_v6(sock: &Socket) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: same contract as enable_pktinfo_v4, for IPV6_RECVPKTINFO.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVPKTINFO,
            &on as *const _ as *const libc::c_void,
            mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One received UDP datagram with optional receiving-interface index.
pub struct RecvPacket {
    pub len: usize,
    pub peer: SocketAddr,
    pub ifindex: Option<u32>,
    pub buf: [u8; 2048],
}

/// Receive one datagram and extract optional receiving ifindex from pktinfo.
pub fn recv_with_pktinfo(sock: &Socket, is_v6: bool) -> std::io::Result<Option<RecvPacket>> {
    let mut buf = [0u8; 2048];
    let mut control = [MaybeUninit::<u8>::uninit(); CMSG_BUF];
    let mut storage = SockAddrStorage::zeroed();
    let mut addr_len = storage.size_of();

    let mut iov = [IoSliceMut::new(&mut buf)];
    // SAFETY:
    // - `sock` is a live UDP socket with IP_PKTINFO / IPV6_RECVPKTINFO enabled.
    // - `iov` points at `buf` for the duration of recvmsg.
    // - `storage` is a zeroed SockAddrStorage large enough for any sockaddr;
    //   view_as yields a mutable reference whose pointer is valid for msg_name.
    // - `control` is MaybeUninit; the kernel writes up to msg_controllen bytes
    //   and sets msg_controllen to the initialized prefix. CMSG_FIRSTHDR /
    //   CMSG_NXTHDR only traverse that prefix, so CMSG_DATA points into
    //   initialized control bytes when present.
    // - parse_pktinfo_ifindex is called only when n >= 0, while `msg` is still
    //   live and describes the just-filled control buffer.
    let (n, ifindex) = unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_name = storage.view_as::<libc::sockaddr_storage>() as *mut _ as *mut libc::c_void;
        msg.msg_namelen = addr_len;
        msg.msg_iov = iov.as_mut_ptr().cast();
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        // msghdr.msg_controllen is size_t on glibc and u32 on musl.
        msg.msg_controllen = control.len() as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            (n, None)
        } else {
            addr_len = msg.msg_namelen;
            let ifindex = parse_pktinfo_ifindex(&msg, is_v6);
            (n, ifindex)
        }
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
            return Ok(None);
        }
        return Err(err);
    }
    if n == 0 {
        return Ok(None);
    }
    debug_assert!(n > 0, "recvmsg returned positive length after zero check");
    // SAFETY: storage was filled by recvmsg with length `addr_len` ≤ capacity.
    let addr = unsafe { SockAddr::new(storage, addr_len) };
    let peer = addr.as_socket().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidData, "recvmsg returned non-IP address")
    })?;
    Ok(Some(RecvPacket {
        len: n as usize,
        peer,
        ifindex,
        buf,
    }))
}

/// Extract ifindex from IP_PKTINFO / IPV6_PKTINFO control messages.
///
/// # Safety
/// `msg` must be a valid `msghdr` immediately after a successful `recvmsg`.
/// `msg_control` / `msg_controllen` describe only the kernel-initialized
/// control prefix; CMSG macros must not be used on an uninitialized buffer.
unsafe fn parse_pktinfo_ifindex(msg: &libc::msghdr, is_v6: bool) -> Option<u32> {
    if msg.msg_control.is_null() || msg.msg_controllen == 0 {
        return None;
    }
    // SAFETY: caller guarantees msg describes an initialized control prefix.
    // CMSG_FIRSTHDR / CMSG_NXTHDR walk only within msg_controllen.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        // SAFETY: cmsg is a non-null header inside the initialized control region.
        let hdr = unsafe { &*cmsg };
        if !is_v6 && hdr.cmsg_level == libc::IPPROTO_IP && hdr.cmsg_type == libc::IP_PKTINFO {
            // SAFETY: cmsg_type/level match in_pktinfo; CMSG_DATA points at
            // aligned payload of that size within the control buffer.
            let ptr = unsafe { libc::CMSG_DATA(cmsg) as *const libc::in_pktinfo };
            if !ptr.is_null() {
                return Some(unsafe { (*ptr).ipi_ifindex as u32 });
            }
        }
        if is_v6 && hdr.cmsg_level == libc::IPPROTO_IPV6 && hdr.cmsg_type == libc::IPV6_PKTINFO {
            // SAFETY: same as IPv4 path for in6_pktinfo / IPV6_PKTINFO.
            let ptr = unsafe { libc::CMSG_DATA(cmsg) as *const libc::in6_pktinfo };
            if !ptr.is_null() {
                return Some(unsafe { (*ptr).ipi6_ifindex });
            }
        }
        // SAFETY: cmsg is a valid current header; NXTHDR advances within the
        // initialized control region or returns null.
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    None
}

fn clock_gettime_duration(clock_id: libc::clockid_t) -> std::io::Result<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec; clock_gettime writes it and does not
    // retain the pointer. `clock_id` is a kernel clock constant.
    let rc = unsafe { libc::clock_gettime(clock_id, &mut ts) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // CLOCK_BOOTTIME / CLOCK_MONOTONIC are non-negative from boot; a negative
    // tv_sec is not reachable in practice, so saturating to zero is fine.
    let sec = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nsec = u32::try_from(ts.tv_nsec).unwrap_or(0);
    Ok(Duration::new(sec, nsec))
}

/// Accumulated suspend time: `CLOCK_BOOTTIME − CLOCK_MONOTONIC`.
///
/// Both clocks advance together while running; only boottime includes sleep.
/// After resume the difference jumps by (approximately) the sleep duration.
#[must_use]
pub fn boottime_monotonic_skew() -> Duration {
    match (
        clock_gettime_duration(libc::CLOCK_BOOTTIME),
        clock_gettime_duration(libc::CLOCK_MONOTONIC),
    ) {
        (Ok(boot), Ok(mono)) => boot.saturating_sub(mono),
        (Err(e), _) | (_, Err(e)) => {
            static WARN: Once = Once::new();
            WARN.call_once(|| {
                log::warn!("clock_gettime failed; suspend detection disabled: {e}");
            });
            Duration::ZERO
        }
    }
}

/// True when the boottime/monotonic skew jumped by at least
/// [`SUSPEND_SKEW_THRESHOLD`] — the process was frozen across suspend.
#[must_use]
pub fn suspend_detected(prev_skew: Duration, now_skew: Duration) -> bool {
    now_skew.saturating_sub(prev_skew) >= SUSPEND_SKEW_THRESHOLD
}
