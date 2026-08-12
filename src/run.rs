//! Main daemon loop: orchestrates config watch, mDNS, dcc-bus discovery, beacon.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::beacon::{self, virtual_serial};
use crate::config::{self, Config, ServiceEntry};
use crate::config_watch::{self, ReloadSignal};
use crate::error::Result;
use crate::iface_watch::{self, IfaceChange};
use crate::legacy_unicast::{self, AnswerSet};
use crate::mdns::{self, MdnsPublisher};
use crate::microinit_watch;
use crate::proc_scan::{self, ListenPorts};
use crate::signals;
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

/// Desired advertisement set derived from config + empirical dcc-bus state.
///
/// `ips` is included so DHCP / interface address changes trigger re-registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredAds {
    pub static_services: Vec<ServiceEntry>,
    pub dynamic: Vec<DynAd>,
    pub beacons: Vec<BeaconWant>,
    pub ips: Vec<Ipv4Addr>,
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
    if let Err(e) = legacy_unicast::spawn(
        Arc::clone(&answer_set),
        legacy_unicast::MDNS_PORT,
        Arc::clone(&stop),
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
    let mut microinit_thr = FailThrottle::new();
    let mut proc_thr = FailThrottle::new();

    let mut last_desired = DesiredAds {
        static_services: Vec::new(),
        dynamic: Vec::new(),
        beacons: Vec::new(),
        ips: Vec::new(),
        skip_interfaces: Vec::new(),
        interfaces: Vec::new(),
    };
    let mut registered: HashMap<String, ServiceEntry> = HashMap::new();

    while !signals::shutdown_requested() && !stop.load(Ordering::SeqCst) {
        let cfg = shared.config.read().map(|c| c.clone()).unwrap_or_default();
        let mdns_ms = cfg.retry.mdns_ms;

        // Ensure mDNS daemon (quiet retry).
        {
            let mut pub_guard = lock_mutex(&publisher);
            match pub_guard.ensure_daemon() {
                Ok(()) => mdns_thr.ok("mDNS daemon"),
                Err(e) => {
                    mdns_thr.fail("mDNS daemon", &e);
                    sleep_or_iface(&iface_rx, Duration::from_millis(mdns_ms));
                    continue;
                }
            }
        }

        let ips = mdns::preferred_ipv4_addrs(&cfg.interfaces, &cfg.skip_interfaces);
        if ips.is_empty() {
            let why = if !cfg.interfaces.is_empty() {
                format!(
                    "none of configured interfaces present/usable (interfaces={:?}, skip={:?})",
                    cfg.interfaces, cfg.skip_interfaces
                )
            } else {
                let mut why = String::from("no UP non-loopback IPv4 (skipping docker/veth/br-*");
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
                    *w = next;
                }
            }
        }

        let mut desired = DesiredAds {
            static_services: cfg.services,
            dynamic: Vec::new(),
            beacons: Vec::new(),
            ips,
            skip_interfaces,
            interfaces,
        };
        let dcc_enabled = cfg.dcc_bus.enabled;
        let z21_port = cfg.dcc_bus.z21_port;
        let withrottle_port = cfg.dcc_bus.withrottle_port;
        let beacon = cfg.dcc_bus.beacon;
        let retry = cfg.retry;

        if dcc_enabled {
            let sock = config::default_microinit_socket();
            match microinit_watch::list_dcc_bus_services(&sock) {
                Ok(services) => {
                    let running: Vec<_> = services
                        .iter()
                        .filter(|s| microinit_watch::is_running(s))
                        .collect();
                    if running.is_empty() {
                        microinit_thr.fail("microinit dcc-bus", &"no running dcc-bus-* service");
                    } else {
                        microinit_thr.ok("microinit dcc-bus");
                        let mut any_scan_ok = false;
                        for st in &running {
                            let Some(pid) = st.pid else {
                                continue;
                            };
                            match proc_scan::listen_ports_for_pid(pid) {
                                Ok(ports) => {
                                    any_scan_ok = true;
                                    append_station_ads(
                                        &mut desired,
                                        &st.name,
                                        &ports,
                                        z21_port,
                                        withrottle_port,
                                        beacon,
                                    );
                                }
                                Err(e) => proc_thr.fail("proc listen scan", &e),
                            }
                        }
                        if any_scan_ok {
                            proc_thr.ok("proc listen scan");
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
                Err(e) => {
                    microinit_thr.fail("microinit socket", &e);
                }
            }
        }

        // Reconcile advertisements when desired set changes (incl. IP churn).
        if desired != last_desired {
            let ips_changed = desired.ips != last_desired.ips
                || desired.interfaces != last_desired.interfaces
                || desired.skip_interfaces != last_desired.skip_interfaces;
            if let Err(e) = reconcile(&publisher, &desired, &mut registered, &beacons, ips_changed)
            {
                mdns_thr.fail("mDNS register", &e);
            } else {
                mdns_thr.ok("mDNS register");
                last_desired = desired;
            }
        }

        // Sleep until next poll; wake early on shutdown or netlink iface change.
        let sleep_ms = if dcc_enabled {
            retry.proc_ms.min(retry.microinit_ms).min(retry.iface_ms)
        } else {
            retry.iface_ms.min(retry.mdns_ms)
        };
        sleep_or_iface(&iface_rx, Duration::from_millis(sleep_ms.max(500)));
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

pub fn append_station_ads(
    desired: &mut DesiredAds,
    service_name: &str,
    ports: &ListenPorts,
    prefer_z21: u16,
    prefer_wt: u16,
    beacon: bool,
) {
    let (layout_id, command_station_id) = match microinit_watch::parse_dcc_bus_ids(service_name) {
        Some(ids) => ids,
        None => {
            log::debug!("skipping dcc-bus service without layout/cs ids: {service_name}");
            return;
        }
    };
    let instance = microinit_watch::instance_name(command_station_id);
    let serial = virtual_serial(layout_id, command_station_id);

    if let Some(port) = pick_udp_port(ports, prefer_z21) {
        desired.dynamic.push(DynAd {
            entry: mdns::dcc_service_entry(
                &instance,
                "_z21._udp",
                "udp",
                port,
                layout_id,
                command_station_id,
                Some(serial),
            ),
        });
        if beacon {
            desired.beacons.push(BeaconWant { port, serial });
        }
    }

    if let Some(port) = pick_tcp_port(ports, prefer_wt) {
        desired.dynamic.push(DynAd {
            entry: mdns::dcc_service_entry(
                &instance,
                "_withrottle._tcp",
                "tcp",
                port,
                layout_id,
                command_station_id,
                None,
            ),
        });
    }
}

pub fn pick_udp_port(ports: &ListenPorts, prefer: u16) -> Option<u16> {
    if ports.has_udp(prefer) {
        return Some(prefer);
    }
    let mut udp: Vec<u16> = ports.udp.iter().copied().collect();
    udp.sort_unstable();
    udp.iter()
        .copied()
        .find(|p| (21_105..=22_000).contains(p))
        .or_else(|| udp.first().copied())
}

pub fn pick_tcp_port(ports: &ListenPorts, prefer: u16) -> Option<u16> {
    if ports.has_tcp(prefer) {
        return Some(prefer);
    }
    let mut tcp: Vec<u16> = ports.tcp.iter().copied().collect();
    tcp.sort_unstable();
    tcp.iter()
        .copied()
        .find(|p| (12_090..=12_200).contains(p))
        .or_else(|| tcp.first().copied())
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

/// Sleep until `total` elapses, shutdown is requested, or a netlink iface change arrives.
fn sleep_or_iface(iface_rx: &std::sync::mpsc::Receiver<IfaceChange>, total: Duration) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if signals::shutdown_requested() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(200));
        match iface_watch::recv_timeout(iface_rx, slice) {
            Ok(IfaceChange) => {
                log::debug!("iface change signaled; refreshing");
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(slice);
            }
        }
    }
}
