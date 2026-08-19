//! Main daemon loop: orchestrates config watch, mDNS, dcc-bus discovery, beacon.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::beacon::{self, virtual_serial};
use crate::bigfred_watch::{self, Program};
use crate::config::{self, Config, ServiceEntry};
use crate::config_watch::{self, ReloadSignal};
use crate::error::Result;
use crate::iface_watch::{self, IfaceChange};
use crate::legacy_unicast::{self, AnswerSet, MembershipRefresh};
use crate::mdns::{self, MdnsPublisher};
use crate::signals;
use crate::sys;
use crate::version;

/// Shared runtime state updated on config reload.
struct Shared {
    config: RwLock<Config>,
    config_path: PathBuf,
}

/// Throttle repeated failure logs: first warn, then debug; info once on recovery.
struct FailThrottle {
    failing: bool,
    last_msg: String,
}

impl FailThrottle {
    fn new() -> Self {
        Self {
            failing: false,
            last_msg: String::new(),
        }
    }

    fn fail(&mut self, context: &str, err: &dyn std::fmt::Display) {
        let msg = format!("{context}: {err}");
        if !self.failing || self.last_msg != msg {
            if !self.failing {
                log::warn!("{msg}");
            } else {
                log::debug!("{msg}");
            }
            self.last_msg = msg;
            self.failing = true;
        } else {
            log::debug!("{msg}");
        }
    }

    fn ok(&mut self, context: &str) {
        if self.failing {
            log::info!("{context}: recovered");
            self.failing = false;
            self.last_msg.clear();
        }
    }
}

/// One dynamic DNS-SD registration derived from a running dcc-bus process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynAd {
    pub entry: ServiceEntry,
}

/// One Z21 LAN discovery beacon (port + virtual serial).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeaconWant {
    pub port: u16,
    pub serial: u32,
}

/// Desired advertisement set derived from config + loco-server `dcc_bus_list`.
///
/// `ips` (IPv4) and `ips_v6` (IPv6) are both included so DHCP / SLAAC privacy
/// address churn triggers re-registration of A/AAAA via mdns-sd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredAds {
    pub static_services: Vec<ServiceEntry>,
    pub dynamic: Vec<DynAd>,
    pub beacons: Vec<BeaconWant>,
    pub ips: Vec<Ipv4Addr>,
    pub ips_v6: Vec<Ipv6Addr>,
    pub skip_interfaces: Vec<String>,
    pub interfaces: Vec<String>,
}

struct ActiveBeacon {
    want: BeaconWant,
    stop: Arc<AtomicBool>,
}

/// Run the daemon until shutdown signal.
pub fn run(config_path: &Path) -> Result<()> {
    let cfg = config::load_or_create(config_path)?;
    log::info!(
        "microdns starting version={} config={}",
        version::info().version,
        config_path.display()
    );

    if let Err(e) = signals::install_handlers() {
        log::warn!("could not install signal handlers: {e}");
    }

    let shared = Arc::new(Shared {
        config: RwLock::new(cfg),
        config_path: config_path.to_path_buf(),
    });

    let stop = Arc::new(AtomicBool::new(false));
    let (reload_rx, watch_stop) = config_watch::spawn(config_path.to_path_buf())?;
    let (iface_rx, iface_watch_stop) = match iface_watch::spawn() {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("iface watcher failed to start: {e}; relying on polling");
            let (_tx, rx) = std::sync::mpsc::channel::<IfaceChange>();
            (rx, Arc::new(AtomicBool::new(true)))
        }
    };

    let publisher = Arc::new(Mutex::new(MdnsPublisher::new()));
    let beacons: Arc<Mutex<Vec<ActiveBeacon>>> = Arc::new(Mutex::new(Vec::new()));
    let answer_set = Arc::new(RwLock::new(AnswerSet::default()));
    let membership = Arc::new(MembershipRefresh::new());
    if let Err(e) = legacy_unicast::spawn_with_refresh(
        Arc::clone(&answer_set),
        legacy_unicast::MDNS_PORT,
        Arc::clone(&stop),
        Arc::clone(&membership),
    ) {
        log::warn!("legacy unicast responder failed to start: {e}");
    }

    // Config reload thread → updates shared config.
    {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("reload".into())
            .spawn(move || reload_loop(shared, reload_rx, stop))
            .ok();
    }

    // Main orchestration: iface/mdns + optional dcc-bus.
    let mut iface_thr = FailThrottle::new();
    let mut mdns_thr = FailThrottle::new();
    let mut bigfred_thr = FailThrottle::new();
    let mut last_programs: Option<Vec<Program>> = None;
    let mut next_bigfred_probe = Instant::now();

    let mut last_desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        ips_v6: Vec::new(),
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    };
    let mut registered: HashMap<String, ServiceEntry> = HashMap::new();
    let mut force_reannounce = false;
    let mut recreate_daemon = false;
    let mut last_skew = sys::boottime_monotonic_skew();

    while !signals::shutdown_requested() && !stop.load(Ordering::SeqCst) {
        if sys::suspend_detected(last_skew, sys::boottime_monotonic_skew()) {
            last_skew = sys::boottime_monotonic_skew();
            apply_wake(
                WakeReason::Suspend,
                &membership,
                &mut force_reannounce,
                &mut recreate_daemon,
            );
        }

        let cfg = shared.config.read().map(|c| c.clone()).unwrap_or_default();
        let mdns_ms = cfg.retry.mdns_ms;

        // Ensure mDNS daemon (quiet retry). Recreate after suspend so mdns-sd
        // binds fresh sockets and rejoins 224.0.0.251.
        {
            let mut pub_guard = lock_mutex(&publisher);
            let daemon_res = if recreate_daemon {
                pub_guard.recreate_daemon()
            } else {
                pub_guard.ensure_daemon()
            };
            match daemon_res {
                Ok(()) => {
                    if recreate_daemon {
                        registered.clear();
                        recreate_daemon = false;
                    }
                    mdns_thr.ok("mDNS daemon");
                }
                Err(e) => {
                    mdns_thr.fail("mDNS daemon", &e);
                    let reason =
                        sleep_or_iface(&iface_rx, Duration::from_millis(mdns_ms), &mut last_skew);
                    apply_wake(
                        reason,
                        &membership,
                        &mut force_reannounce,
                        &mut recreate_daemon,
                    );
                    continue;
                }
            }
        }

        let ips = mdns::preferred_ipv4_addrs(&cfg.interfaces, &cfg.skip_interfaces);
        let ips_v6: Vec<Ipv6Addr> =
            mdns::preferred_ipv6_addrs(&cfg.interfaces, &cfg.skip_interfaces)
                .into_iter()
                .map(|a| a.addr)
                .collect();
        if ips.is_empty() {
            let why = if !cfg.interfaces.is_empty() {
                format!(
                    "none of configured interfaces present/usable (interfaces={:?}, skip={:?})",
                    cfg.interfaces, cfg.skip_interfaces
                )
            } else {
                let mut why =
                    String::from("no running non-loopback IPv4 (skipping docker/veth/br-*");
                if !cfg.skip_interfaces.is_empty() {
                    why.push_str(&format!(", configured {:?}", cfg.skip_interfaces));
                }
                why.push(')');
                why
            };
            iface_thr.fail("network interface", &why);
        } else {
            iface_thr.ok("network interface");
        }

        // Take interface lists once; AnswerSet clones, DesiredAds owns.
        let interfaces = cfg.interfaces;
        let skip_interfaces = cfg.skip_interfaces;
        {
            let mut hosts = Vec::new();
            for svc in &cfg.services {
                let host =
                    mdns::normalize_hostname(svc.host.as_deref().unwrap_or(&version::hostname()));
                if !hosts.iter().any(|h| h == &host) {
                    hosts.push(host);
                }
            }
            let next = AnswerSet {
                hosts,
                v4: mdns::preferred_ipv4_ifaces(&interfaces, &skip_interfaces),
                v6: mdns::preferred_ipv6_addrs(&interfaces, &skip_interfaces),
                skip_interfaces: skip_interfaces.clone(),
                interfaces: interfaces.clone(),
            };
            if let Ok(mut w) = answer_set.write() {
                if *w != next {
                    log_detected_interfaces(&next);
                    *w = next;
                }
            }
        }

        let mut desired = DesiredAds {
            static_services: cfg.services,
            dynamic: Vec::new(),
            beacons: Vec::new(),
            ips,
            ips_v6,
            skip_interfaces,
            interfaces,
        };
        let bigfred_enabled = cfg.bigfred.enabled;
        let bigfred_socket = cfg.bigfred.socket_path();
        let beacon = cfg.dcc_bus.beacon;
        let retry = cfg.retry;

        if bigfred_enabled {
            if Instant::now() >= next_bigfred_probe {
                match bigfred_watch::dcc_bus_list(&bigfred_socket) {
                    Ok(programs) => {
                        last_programs = Some(programs);
                        bigfred_thr.ok("bigfred socket");
                        next_bigfred_probe =
                            Instant::now() + Duration::from_millis(retry.poll_ms.max(500));
                    }
                    Err(e) => {
                        last_programs = None;
                        bigfred_thr.fail("bigfred socket", &e);
                        next_bigfred_probe =
                            Instant::now() + Duration::from_millis(retry.bigfred_ms.max(500));
                    }
                }
            }
            if let Some(programs) = &last_programs {
                for p in programs {
                    append_station_ads(&mut desired, p, beacon);
                }
                desired.dynamic.sort_by(|a, b| {
                    (&a.entry.name, &a.entry.type_, a.entry.port).cmp(&(
                        &b.entry.name,
                        &b.entry.type_,
                        b.entry.port,
                    ))
                });
                desired.beacons.sort_unstable();
                desired.beacons.dedup();
            }
        }

        // Reconcile advertisements when desired set changes (incl. IP churn)
        // or when netlink/suspend forced a refresh with the same IPs.
        if desired != last_desired || force_reannounce {
            let ips_changed = force_reannounce
                || desired.ips != last_desired.ips
                || desired.ips_v6 != last_desired.ips_v6
                || desired.interfaces != last_desired.interfaces
                || desired.skip_interfaces != last_desired.skip_interfaces;
            if let Err(e) = reconcile(&publisher, &desired, &mut registered, &beacons, ips_changed)
            {
                mdns_thr.fail("mDNS register", &e);
            } else {
                mdns_thr.ok("mDNS register");
                last_desired = desired;
                force_reannounce = false;
            }
        }

        // Sleep until next poll; wake early on shutdown, netlink, or suspend.
        let sleep_ms = if bigfred_enabled {
            let until_probe = next_bigfred_probe
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            until_probe.min(retry.iface_ms)
        } else {
            retry.iface_ms.min(retry.mdns_ms)
        };
        let reason = sleep_or_iface(
            &iface_rx,
            Duration::from_millis(sleep_ms.max(500)),
            &mut last_skew,
        );
        apply_wake(
            reason,
            &membership,
            &mut force_reannounce,
            &mut recreate_daemon,
        );
    }

    log::info!("microdns shutting down");
    stop.store(true, Ordering::SeqCst);
    watch_stop.store(true, Ordering::SeqCst);
    iface_watch_stop.store(true, Ordering::SeqCst);
    {
        let mut active = lock_mutex(&beacons);
        for b in active.drain(..) {
            b.stop.store(true, Ordering::SeqCst);
        }
    }
    {
        let mut p = lock_mutex(&publisher);
        p.shutdown();
    }
    Ok(())
}

pub fn append_station_ads(desired: &mut DesiredAds, program: &Program, beacon: bool) {
    if !program.running {
        return;
    }
    let instance = bigfred_watch::instance_name(program.command_station_id);
    let serial = virtual_serial(program.layout_id, program.command_station_id);
    let layout_name = program.layout_name.trim();

    if program.z21_enabled && program.z21_port != 0 {
        desired.dynamic.push(DynAd {
            entry: mdns::dcc_service_entry(
                &instance,
                "_z21._udp",
                "udp",
                program.z21_port,
                program.layout_id,
                program.command_station_id,
                layout_name,
                Some(serial),
            ),
        });
        if beacon {
            desired.beacons.push(BeaconWant {
                port: program.z21_port,
                serial,
            });
        }
    }

    if program.withrottle_enabled && program.withrottle_port != 0 {
        desired.dynamic.push(DynAd {
            entry: mdns::dcc_service_entry(
                &instance,
                "_withrottle._tcp",
                "tcp",
                program.withrottle_port,
                program.layout_id,
                program.command_station_id,
                layout_name,
                None,
            ),
        });
    }
}

fn reload_loop(
    shared: Arc<Shared>,
    rx: std::sync::mpsc::Receiver<ReloadSignal>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) && !signals::shutdown_requested() {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ReloadSignal) => match config::load_or_create(&shared.config_path) {
                Ok(cfg) => {
                    // load_or_create already validates; keep last-known-good on failure.
                    if let Ok(mut w) = shared.config.write() {
                        *w = cfg;
                    }
                    log::info!("config reloaded from {}", shared.config_path.display());
                }
                Err(e) => log::warn!("config reload failed; keeping previous config: {e}"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Apply desired ads without withdrawing working records first.
///
/// 1. Register entries that are entirely new.
/// 2. Refresh entries that changed (or when IPs changed): unregister then register.
/// 3. Only then unregister keys that are no longer desired.
/// 4. On register failure, return Err so the caller keeps `last_desired` and retries
///    without committing a broken state.
fn reconcile(
    publisher: &Arc<Mutex<MdnsPublisher>>,
    desired: &DesiredAds,
    registered: &mut HashMap<String, ServiceEntry>,
    beacons: &Arc<Mutex<Vec<ActiveBeacon>>>,
    ips_changed: bool,
) -> Result<()> {
    let pub_guard = lock_mutex(publisher);
    let allow = &desired.interfaces;
    let skip = &desired.skip_interfaces;

    let mut desired_map: HashMap<String, ServiceEntry> = HashMap::new();
    for svc in &desired.static_services {
        let key = MdnsPublisher::fullname(&svc.name, &svc.type_);
        desired_map.insert(key, svc.clone());
    }
    for dyn_ad in &desired.dynamic {
        let key = MdnsPublisher::fullname(&dyn_ad.entry.name, &dyn_ad.entry.type_);
        desired_map.insert(key, dyn_ad.entry.clone());
    }

    // Phase 1: add brand-new keys while old ads still answer queries.
    for (key, entry) in &desired_map {
        if registered.contains_key(key) {
            continue;
        }
        pub_guard.register(entry, entry.host.as_deref(), allow, skip)?;
        registered.insert(key.clone(), entry.clone());
    }

    // Phase 2: refresh changed content or rebound addresses after DHCP/iface churn.
    for (key, entry) in &desired_map {
        let Some(prev) = registered.get(key) else {
            continue;
        };
        if prev == entry && !ips_changed {
            continue;
        }
        let _ = pub_guard.unregister(key);
        pub_guard.register(entry, entry.host.as_deref(), allow, skip)?;
        registered.insert(key.clone(), entry.clone());
    }

    // Phase 3: drop obsolete keys only after desired set is registered.
    let desired_keys: HashSet<String> = desired_map.keys().cloned().collect();
    let stale: Vec<String> = registered
        .keys()
        .filter(|k| !desired_keys.contains(*k))
        .cloned()
        .collect();
    for key in stale {
        let _ = pub_guard.unregister(&key);
        registered.remove(&key);
    }

    drop(pub_guard);
    reconcile_beacons(beacons, &desired.beacons)?;
    Ok(())
}

fn reconcile_beacons(beacons: &Arc<Mutex<Vec<ActiveBeacon>>>, want: &[BeaconWant]) -> Result<()> {
    let want_set: HashSet<&BeaconWant> = want.iter().collect();
    let mut active = lock_mutex(beacons);

    active.retain(|b| {
        if want_set.contains(&b.want) {
            true
        } else {
            b.stop.store(true, Ordering::SeqCst);
            false
        }
    });

    let active_set: HashSet<BeaconWant> = active.iter().map(|b| b.want.clone()).collect();
    for w in want {
        if active_set.contains(w) {
            continue;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let frame = beacon::serial_reply(w.serial);
        beacon::spawn(w.port, frame, Arc::clone(&stop))?;
        active.push(ActiveBeacon {
            want: w.clone(),
            stop,
        });
    }
    Ok(())
}

/// Recover from a poisoned mutex: a poisoned lock means another thread panicked
/// while holding it. Prefer continuing with the inner value over crashing the
/// daemon (reliability contract: warn, do not abort).
fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("mutex poisoned; recovering inner state");
            poisoned.into_inner()
        }
    }
}

/// Log usable interfaces and their addresses when the advertisement set changes.
fn log_detected_interfaces(answers: &AnswerSet) {
    if answers.v4.is_empty() && answers.v6.is_empty() {
        log::info!("network interfaces: none usable for mDNS");
        return;
    }

    // Group by interface name so one line lists all addresses for that iface.
    let mut names: Vec<String> = answers
        .v4
        .iter()
        .map(|a| a.iface.clone())
        .chain(answers.v6.iter().map(|a| a.iface.clone()))
        .collect();
    names.sort_unstable();
    names.dedup();

    for name in names {
        let v4: Vec<String> = answers
            .v4
            .iter()
            .filter(|a| a.iface == name)
            .map(|a| format!("{} (ifindex={})", a.addr, a.ifindex))
            .collect();
        let v6: Vec<String> = answers
            .v6
            .iter()
            .filter(|a| a.iface == name)
            .map(|a| format!("{} (ifindex={})", a.addr, a.ifindex))
            .collect();
        let ifindex = answers
            .v4
            .iter()
            .find(|a| a.iface == name)
            .map(|a| a.ifindex)
            .or_else(|| {
                answers
                    .v6
                    .iter()
                    .find(|a| a.iface == name)
                    .map(|a| a.ifindex)
            })
            .unwrap_or(0);
        log::info!(
            "network interface detected name={name} ifindex={ifindex} ipv4={v4:?} ipv6={v6:?} multicast_group=224.0.0.251"
        );
    }
}

/// Why the main loop woke from its idle wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeReason {
    Timeout,
    IfaceChange,
    Suspend,
    Shutdown,
}

fn apply_wake(
    reason: WakeReason,
    membership: &MembershipRefresh,
    force_reannounce: &mut bool,
    recreate_daemon: &mut bool,
) {
    match reason {
        WakeReason::IfaceChange => {
            // Same IPs after a link flap / resume still need IGMP leave+join
            // and an mDNS announce; AddressSet equality would skip both.
            membership.request_rejoin();
            *force_reannounce = true;
        }
        WakeReason::Suspend => {
            log::info!("suspend/resume detected; rebinding mDNS sockets");
            membership.request_rebind();
            *recreate_daemon = true;
            *force_reannounce = true;
        }
        WakeReason::Timeout | WakeReason::Shutdown => {}
    }
}

/// Sleep until `total` elapses, shutdown is requested, a netlink iface change
/// arrives, or CLOCK_BOOTTIME/MONOTONIC skew jumps (suspend/resume).
fn sleep_or_iface(
    iface_rx: &std::sync::mpsc::Receiver<IfaceChange>,
    total: Duration,
    last_skew: &mut Duration,
) -> WakeReason {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if signals::shutdown_requested() {
            return WakeReason::Shutdown;
        }
        let now_skew = sys::boottime_monotonic_skew();
        if sys::suspend_detected(*last_skew, now_skew) {
            *last_skew = now_skew;
            return WakeReason::Suspend;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(200));
        match iface_watch::recv_timeout(iface_rx, slice) {
            Ok(IfaceChange) => {
                let now_skew = sys::boottime_monotonic_skew();
                if sys::suspend_detected(*last_skew, now_skew) {
                    *last_skew = now_skew;
                    return WakeReason::Suspend;
                }
                log::debug!("iface change signaled; refreshing");
                return WakeReason::IfaceChange;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(slice);
            }
        }
    }
    let now_skew = sys::boottime_monotonic_skew();
    if sys::suspend_detected(*last_skew, now_skew) {
        *last_skew = now_skew;
        return WakeReason::Suspend;
    }
    WakeReason::Timeout
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn iface_change_forces_rejoin_not_rebind() {
        let membership = MembershipRefresh::new();
        let mut force = false;
        let mut recreate = false;
        apply_wake(
            WakeReason::IfaceChange,
            &membership,
            &mut force,
            &mut recreate,
        );
        assert!(force);
        assert!(!recreate);
        assert_eq!(membership.epoch(), 1);
        assert!(!membership.take_rebind());
    }

    #[test]
    fn suspend_forces_rebind_and_recreate() {
        let membership = MembershipRefresh::new();
        let mut force = false;
        let mut recreate = false;
        apply_wake(WakeReason::Suspend, &membership, &mut force, &mut recreate);
        assert!(force);
        assert!(recreate);
        assert_eq!(membership.epoch(), 1);
        assert!(membership.take_rebind());
    }

    #[test]
    fn sleep_returns_iface_change() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(IfaceChange).unwrap();
        let mut last_skew = sys::boottime_monotonic_skew();
        let reason = sleep_or_iface(&rx, Duration::from_secs(2), &mut last_skew);
        assert_eq!(reason, WakeReason::IfaceChange);
    }

    #[test]
    fn sleep_times_out_without_signal() {
        let (_tx, rx) = mpsc::sync_channel::<IfaceChange>(1);
        let mut last_skew = sys::boottime_monotonic_skew();
        let reason = sleep_or_iface(&rx, Duration::from_millis(50), &mut last_skew);
        assert_eq!(reason, WakeReason::Timeout);
    }
}
