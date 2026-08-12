//! Tests for the rtnetlink interface watcher.

use std::sync::atomic::Ordering;
use std::time::Duration;

use microdns::iface_watch;

#[test]
fn spawn_starts_and_stops() {
    let (rx, stop) = iface_watch::spawn().expect("spawn iface watcher");
    // Give the thread a moment to open the netlink socket (or fail quietly).
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);
    // Receiver may or may not see events; just ensure it does not panic.
    let _ = rx.recv_timeout(Duration::from_millis(200));
}

#[test]
fn iface_change_on_dummy_addr_add() {
    // Requires CAP_NET_ADMIN to create a dummy iface; skip when unavailable.
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

    let (rx, stop) = iface_watch::spawn().expect("spawn");
    // Drain any startup noise.
    while rx.try_recv().is_ok() {}

    let _ = std::process::Command::new("ip")
        .args(["link", "set", &name, "up"])
        .status();
    let _ = std::process::Command::new("ip")
        .args(["addr", "add", "192.0.2.10/32", "dev", &name])
        .status();

    let got = rx.recv_timeout(Duration::from_secs(2)).is_ok();

    let _ = std::process::Command::new("ip")
        .args(["link", "del", &name])
        .status();
    stop.store(true, Ordering::SeqCst);

    assert!(got, "expected IfaceChange after adding address on {name}");
}
