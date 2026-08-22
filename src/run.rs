//! Main daemon loop: orchestrates config watch, mDNS, dcc-bus discovery, microinit watch, beacon.

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
use crate::ctl;
use crate::error::Result;
use crate::iface_watch::{self, IfaceChange};
use crate::legacy_unicast::{self, AnswerSet, MembershipRefresh};
use crate::mdns::{self, MdnsPublisher};
use crate::microinit_watch;
use crate::signals;
use crate::sys;
use crate::version;

/// Shared runtime state updated on config reload.
struct Shared {
    config: Arc<RwLock<Config>>,
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

/// Origin of a dynamic DNS-SD advertisement (not static `services[]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynSource {
    DccBus,
    Microinit,
}

/// One dynamic DNS-SD registration derived from dcc-bus or microinit labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynAd {
    pub entry: ServiceEntry,
    pub source: DynSource,
}

/// One Z21 LAN discovery beacon (port + virtual serial).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeaconWant {
    pub port: u16,
    pub serial: u32,
}

/// Desired advertisement set derived from config, loco-server `dcc_bus_list`,
/// and microinit watch snapshots.
///
/// `ips` (IPv4) and `ips_v6` (IPv6) are both included so DHCP / SLAAC privacy
/// address churn triggers re-registration of A/AAAA via mdns-sd.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    run_with_socket(config_path, &ctl::default_socket())
}

/// Like [`run`], with an explicit control-socket path.
pub fn run_with_socket(config_path: &Path, ctl_socket: &Path) -> Result<()> {
    let cfg = config::load_or_create(config_path)?;
    log::info!(
        "microdns starting version={} config={}",
        version::info().version,
        config_path.display()
    );

    if let Err(e) = signals::install_handlers() {
        log::warn!("could not install signal handlers: {e}");
    }

    let config = Arc::new(RwLock::new(cfg));
    let shared = Arc::new(Shared {
        config: Arc::clone(&config),
        config_path: config_path.to_path_buf(),
    });

    let stop = Arc::new(AtomicBool::new(false));
    let (reload_rx, watch_stop) = config_watch::spawn(config_path.to_path_buf())?;
    let (iface_rx, iface_watch_stop) = match iface_watch::spawn_filtered(Arc::clone(&config)) {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("iface watcher failed to start: {e}; relying on polling");
            let (_tx, rx) = std::sync::mpsc::channel::<IfaceChange>();
            (rx, Arc::new(AtomicBool::new(true)))
        }
    };
    let (microinit_rx, microinit_watch_stop) =
        match microinit_watch::spawn(Arc::clone(&config), Arc::clone(&stop)) {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("microinit watcher failed to start: {e}");
                (
                    microinit_watch::WatchFeed::disconnected(),
                    Arc::new(AtomicBool::new(true)),
                )
            }
        };

    let publisher = Arc::new(Mutex::new(MdnsPublisher::new()));
    let beacons: Arc<Mutex<Vec<ActiveBeacon>>> = Arc::new(Mutex::new(Vec::new()));
    let desired_snapshot = Arc::new(RwLock::new(DesiredAds::default()));
    let selfcheck_snapshot = Arc::new(RwLock::new(crate::selfcheck::Report::default()));
    ctl::serve_with_runtime(
        ctl_socket,
        Arc::clone(&desired_snapshot),
        Some(ctl::CtlRuntime {
            publisher: Arc::clone(&publisher),
            selfcheck: Arc::clone(&selfcheck_snapshot),
        }),
    )?;
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
    let mut programs_stale_since: Option<Instant> = None;
    let mut bigfred_failures: u32 = 0;
    let mut next_bigfred_probe = Instant::now();
    let mut last_microinit: Option<Vec<ServiceEntry>> = None;

    let mut last_desired = DesiredAds::default();
    let mut registered: HashMap<String, ServiceEntry> = HashMap::new();
    let mut force_reannounce = false;
    let mut recreate_daemon = false;
    let mut last_skew = sys::boottime_monotonic_skew();
    let mut next_periodic = Instant::now()
        + Duration::from_millis(
            shared
                .config
                .read()
                .map(|c| c.announce.period_ms)
                .unwrap_or(55_000),
        );
    let mut burst_deadlines: Vec<Instant> = Vec::new();
    let mut next_selfcheck = Instant::now()
        + Duration::from_millis(
            shared
                .config
                .read()
                .map(|c| c.selfcheck.period_ms)
                .unwrap_or(60_000),
        );
    let mut selfcheck_escalation = crate::selfcheck::Escalation::None;

    while !signals::shutdown_requested() && !stop.load(Ordering::SeqCst) {
        let now_skew = sys::boottime_monotonic_skew();
        if sys::suspend_detected(last_skew, now_skew) {
            last_skew = now_skew;
            apply_wake(
                WakeReason::Suspend,
                &membership,
                &mut force_reannounce,
                &mut recreate_daemon,
            );
        }

        let cfg = shared.config.read().map(|c| c.clone()).unwrap_or_default();
        drain_microinit(&microinit_rx, &mut last_microinit);
        if !cfg.microinit.enabled {
            last_microinit = None;
        }
        let mdns_ms = cfg.retry.mdns_ms;
        let announce_period = Duration::from_millis(cfg.announce.period_ms.max(1000));
        let selfcheck_period = Duration::from_millis(cfg.selfcheck.period_ms.max(1000));
        let now = Instant::now();
        if now >= next_periodic {
            force_reannounce = true;
            next_periodic = now + announce_period;
        }
        burst_deadlines.retain(|t| {
            if now >= *t {
                force_reannounce = true;
                false
            } else {
                true
            }
        });

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
                    let reason = sleep_or_iface(
                        &iface_rx,
                        &microinit_rx,
                        &mut last_microinit,
                        Duration::from_millis(mdns_ms),
                        &mut last_skew,
                    );
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
            let hostname = version::hostname();
            for svc in &cfg.services {
                let host = mdns::normalize_hostname(svc.host.as_deref().unwrap_or(&hostname));
                if !hosts.iter().any(|h| h == &host) {
                    hosts.push(host);
                }
            }
            if let Some(entries) = &last_microinit {
                for svc in entries {
                    let host = mdns::normalize_hostname(svc.host.as_deref().unwrap_or(&hostname));
                    if !hosts.iter().any(|h| h == &host) {
                        hosts.push(host);
                    }
                }
            }
            if let Some(h) = cfg.dcc_bus.advertised_host() {
                let host = mdns::normalize_hostname(h);
                if !hosts.iter().any(|existing| existing == &host) {
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
                        programs_stale_since = None;
                        bigfred_failures = 0;
                        bigfred_thr.ok("bigfred socket");
                        next_bigfred_probe =
                            Instant::now() + Duration::from_millis(retry.poll_ms.max(500));
                    }
                    Err(e) => {
                        bigfred_thr.fail("bigfred socket", &e);
                        if last_programs.is_some() {
                            let stale_from = *programs_stale_since.get_or_insert(Instant::now());
                            if Instant::now().saturating_duration_since(stale_from)
                                >= Duration::from_millis(retry.bigfred_ms.max(500))
                            {
                                log::warn!(
                                    "bigfred socket down for {}ms; withdrawing dcc-bus ads",
                                    retry.bigfred_ms
                                );
                                last_programs = None;
                                programs_stale_since = None;
                            }
                        }
                        let wait = bigfred_backoff_ms(bigfred_failures, retry.bigfred_ms);
                        bigfred_failures = bigfred_failures.saturating_add(1);
                        next_bigfred_probe = Instant::now() + Duration::from_millis(wait);
                    }
                }
            }
            if let Some(programs) = &last_programs {
                for p in programs {
                    append_station_ads(&mut desired, p, beacon, cfg.dcc_bus.advertised_host());
                }
            }
        }
        if let Some(entries) = &last_microinit {
            for entry in entries {
                desired.dynamic.push(DynAd {
                    entry: entry.clone(),
                    source: DynSource::Microinit,
                });
            }
        }
        sort_dynamic_ads(&mut desired.dynamic);
        desired.beacons.sort_unstable();
        desired.beacons.dedup();

        if let Ok(mut w) = desired_snapshot.write() {
            *w = desired.clone();
        }

        // Reconcile advertisements when desired set changes (incl. IP churn)
        // or when netlink/suspend/periodic ticker forced a refresh. Refresh is
        // register() only — never unregister() — so we do not emit goodbye
        // packets that wipe client caches.
        let desired_changed = desired != last_desired;
        if desired_changed || force_reannounce {
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
                if desired_changed {
                    burst_deadlines = announce_burst_deadlines(cfg.announce.burst_count);
                }
                last_desired = desired;
                force_reannounce = false;
            }
        }

        if Instant::now() >= next_selfcheck {
            let expected: Vec<String> = last_desired
                .static_services
                .iter()
                .chain(last_desired.dynamic.iter().map(|d| &d.entry))
                .map(|e| MdnsPublisher::fullname(&e.name, &e.type_))
                .collect();
            let want_v4 = mdns::preferred_ipv4_ifaces(
                &last_desired.interfaces,
                &last_desired.skip_interfaces,
            );
            let fresh_for = announce_period + Duration::from_secs(15);
            let mut report = {
                let pub_guard = lock_mutex(&publisher);
                crate::selfcheck::evaluate(
                    &pub_guard,
                    &want_v4,
                    &expected,
                    fresh_for,
                    Instant::now(),
                )
            };
            if report.ok {
                selfcheck_escalation = crate::selfcheck::Escalation::None;
            } else {
                match selfcheck_escalation {
                    crate::selfcheck::Escalation::None => {
                        log::warn!("selfcheck failed; re-announcing: {}", report.message);
                        force_reannounce = true;
                        selfcheck_escalation = crate::selfcheck::Escalation::Reannounce;
                    }
                    crate::selfcheck::Escalation::Reannounce => {
                        log::error!(
                            "selfcheck still failing; recreating mDNS daemon: {}",
                            report.message
                        );
                        recreate_daemon = true;
                        force_reannounce = true;
                        selfcheck_escalation = crate::selfcheck::Escalation::RecreateDaemon;
                    }
                    crate::selfcheck::Escalation::RecreateDaemon => {
                        log::error!(
                            "selfcheck still failing after daemon recreate: {}",
                            report.message
                        );
                    }
                }
            }
            report.escalation = selfcheck_escalation;
            if let Ok(mut w) = selfcheck_snapshot.write() {
                *w = report;
            }
            next_selfcheck = Instant::now() + selfcheck_period;
        }

        // Sleep until next poll / announce / selfcheck; wake early on netlink.
        let until_periodic = next_periodic
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        let until_burst = burst_deadlines
            .iter()
            .map(|t| t.saturating_duration_since(Instant::now()).as_millis() as u64)
            .min()
            .unwrap_or(u64::MAX);
        let until_selfcheck = next_selfcheck
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        let sleep_ms = if bigfred_enabled {
            let until_probe = next_bigfred_probe
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            until_probe.min(retry.iface_ms)
        } else {
            retry.iface_ms.min(retry.mdns_ms)
        };
        let sleep_ms = sleep_ms
            .min(until_periodic)
            .min(until_burst)
            .min(until_selfcheck);
        let reason = sleep_or_iface(
            &iface_rx,
            &microinit_rx,
            &mut last_microinit,
            Duration::from_millis(sleep_ms.max(200)),
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
    microinit_watch_stop.store(true, Ordering::SeqCst);
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
    program: &Program,
    beacon: bool,
    host: Option<&str>,
) {
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
                host,
            ),
            source: DynSource::DccBus,
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
                host,
            ),
            source: DynSource::DccBus,
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
/// The action list comes from [`plan_reconcile`], which orders `Add` before
/// `Refresh` before `Drop` so existing records keep answering queries until the
/// desired set is registered, and never pairs a refresh with an unregister.
/// On register failure this returns `Err` so the caller keeps `last_desired` and
/// retries without committing a broken state.
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

    for action in plan_reconcile(&desired_map, registered, ips_changed) {
        match action {
            // A refresh re-registers without unregister: mdns-sd overwrites the
            // existing ServiceInfo, while unregister would send a goodbye
            // (TTL 0) plus a second one 120 ms later, landing after the new
            // announcement and wiping client caches.
            ReconcileAction::Add(key) | ReconcileAction::Refresh(key) => {
                let Some(entry) = desired_map.get(&key) else {
                    continue;
                };
                pub_guard.register(entry, entry.host.as_deref(), allow, skip)?;
                registered.insert(key, entry.clone());
            }
            ReconcileAction::Drop(key) => {
                let _ = pub_guard.unregister(&key);
                registered.remove(&key);
            }
        }
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

/// Exponential backoff for a missing BigFred socket: 2 s, 4 s, 8 s, … cap.
#[must_use]
pub fn bigfred_backoff_ms(failures: u32, cap: u64) -> u64 {
    let shift = failures.min(15);
    (2000u64.saturating_mul(1u64 << shift))
        .min(cap.max(500))
        .max(500)
}

/// Unsolicited re-announce deadlines after a real advertisement change.
/// `burst_count` 4 → now+1s, +2s, +4s, +8s.
#[must_use]
pub fn announce_burst_deadlines(burst_count: u8) -> Vec<Instant> {
    let now = Instant::now();
    announce_burst_delays(burst_count)
        .into_iter()
        .map(|d| now + d)
        .collect()
}

/// Burst delays as durations (testable without Instant::now).
#[must_use]
pub fn announce_burst_delays(burst_count: u8) -> Vec<Duration> {
    (0..burst_count)
        .map(|i| Duration::from_secs(1u64 << i.min(31)))
        .collect()
}

fn txt_sort_key(txt: &Option<HashMap<String, String>>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = txt
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort();
    pairs
}

fn sort_dynamic_ads(dynamic: &mut [DynAd]) {
    dynamic.sort_by(|a, b| {
        (
            a.entry.name.as_str(),
            a.entry.type_.as_str(),
            a.entry.port,
            a.entry.host.as_deref().unwrap_or(""),
            txt_sort_key(&a.entry.txt),
        )
            .cmp(&(
                b.entry.name.as_str(),
                b.entry.type_.as_str(),
                b.entry.port,
                b.entry.host.as_deref().unwrap_or(""),
                txt_sort_key(&b.entry.txt),
            ))
    });
}

/// Plan of register/refresh/drop actions. Refresh never includes unregister.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    Add(String),
    Refresh(String),
    Drop(String),
}

/// Compute reconcile actions. Refresh is register-only (no goodbye).
#[must_use]
pub fn plan_reconcile(
    desired: &HashMap<String, ServiceEntry>,
    registered: &HashMap<String, ServiceEntry>,
    content_or_addr_changed: bool,
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();
    for (key, entry) in desired {
        if !registered.contains_key(key) {
            actions.push(ReconcileAction::Add(key.clone()));
            continue;
        }
        if registered.get(key) != Some(entry) || content_or_addr_changed {
            actions.push(ReconcileAction::Refresh(key.clone()));
        }
    }
    for key in registered.keys() {
        if !desired.contains_key(key) {
            actions.push(ReconcileAction::Drop(key.clone()));
        }
    }
    actions.sort_by(|a, b| {
        fn rank(x: &ReconcileAction) -> u8 {
            match x {
                ReconcileAction::Add(_) => 0,
                ReconcileAction::Refresh(_) => 1,
                ReconcileAction::Drop(_) => 2,
            }
        }
        fn key(x: &ReconcileAction) -> &str {
            match x {
                ReconcileAction::Add(k)
                | ReconcileAction::Refresh(k)
                | ReconcileAction::Drop(k) => k,
            }
        }
        rank(a).cmp(&rank(b)).then_with(|| key(a).cmp(key(b)))
    });
    actions
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
    Microinit,
}

fn apply_wake(
    reason: WakeReason,
    membership: &MembershipRefresh,
    force_reannounce: &mut bool,
    recreate_daemon: &mut bool,
) {
    match reason {
        WakeReason::IfaceChange => {
            // Suspend already queued a full rebind+recreate; don't bump epoch
            // again for a follow-up netlink event (link flap after wake).
            if *recreate_daemon {
                return;
            }
            // Same IPs after a link flap still need a re-announce. Do NOT
            // leave+join IGMP here: that drops multicast on snooping switches.
            // Membership is refreshed when AnswerSet addresses actually change.
            *force_reannounce = true;
        }
        WakeReason::Suspend => {
            log::info!("suspend/resume detected; rebinding mDNS sockets");
            membership.request_rebind();
            *recreate_daemon = true;
            *force_reannounce = true;
        }
        WakeReason::Timeout | WakeReason::Shutdown | WakeReason::Microinit => {}
    }
}

fn drain_microinit(
    feed: &microinit_watch::WatchFeed,
    last: &mut Option<Vec<ServiceEntry>>,
) -> bool {
    feed.drain_into(last)
}

/// Sleep until `total` elapses, shutdown is requested, a netlink iface change
/// arrives, a microinit watch snapshot arrives, or CLOCK_BOOTTIME/MONOTONIC
/// skew jumps (suspend/resume).
///
/// Netlink is polled at [`IFACE_POLL_SLICE`]. Suspend detection is a pair of
/// `clock_gettime` syscalls, so it runs at most once per
/// [`SUSPEND_CHECK_INTERVAL`] unless netlink already woke us.
const IFACE_POLL_SLICE: Duration = Duration::from_millis(200);
const SUSPEND_CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn sleep_or_iface(
    iface_rx: &std::sync::mpsc::Receiver<IfaceChange>,
    microinit: &microinit_watch::WatchFeed,
    last_microinit: &mut Option<Vec<ServiceEntry>>,
    total: Duration,
    last_skew: &mut Duration,
) -> WakeReason {
    let deadline = Instant::now() + total;
    let mut next_skew_check = Instant::now();
    while Instant::now() < deadline {
        if signals::shutdown_requested() {
            return WakeReason::Shutdown;
        }
        if drain_microinit(microinit, last_microinit) {
            return WakeReason::Microinit;
        }
        let now = Instant::now();
        if now >= next_skew_check {
            let now_skew = sys::boottime_monotonic_skew();
            if sys::suspend_detected(*last_skew, now_skew) {
                *last_skew = now_skew;
                return WakeReason::Suspend;
            }
            next_skew_check = now + SUSPEND_CHECK_INTERVAL;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let until_skew = next_skew_check.saturating_duration_since(Instant::now());
        let slice = remaining.min(IFACE_POLL_SLICE).min(until_skew);
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
    if drain_microinit(microinit, last_microinit) {
        return WakeReason::Microinit;
    }
    WakeReason::Timeout
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn iface_change_forces_reannounce_not_rejoin() {
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
        assert_eq!(membership.epoch(), 0);
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
    fn iface_change_does_not_bump_epoch_when_recreate_pending() {
        let membership = MembershipRefresh::new();
        let mut force = false;
        let mut recreate = false;
        apply_wake(WakeReason::Suspend, &membership, &mut force, &mut recreate);
        apply_wake(
            WakeReason::IfaceChange,
            &membership,
            &mut force,
            &mut recreate,
        );
        assert_eq!(membership.epoch(), 1);
        assert!(recreate);
        assert!(force);
        assert!(membership.take_rebind());
    }

    #[test]
    fn sleep_returns_iface_change() {
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(IfaceChange).unwrap();
        let mi_feed = microinit_watch::WatchFeed::disconnected();
        let mut last_microinit = None;
        let mut last_skew = sys::boottime_monotonic_skew();
        let reason = sleep_or_iface(
            &rx,
            &mi_feed,
            &mut last_microinit,
            Duration::from_secs(2),
            &mut last_skew,
        );
        assert_eq!(reason, WakeReason::IfaceChange);
    }

    #[test]
    fn sleep_times_out_without_signal() {
        let (_tx, rx) = mpsc::sync_channel::<IfaceChange>(1);
        let mi_feed = microinit_watch::WatchFeed::disconnected();
        let mut last_microinit = None;
        let mut last_skew = sys::boottime_monotonic_skew();
        let reason = sleep_or_iface(
            &rx,
            &mi_feed,
            &mut last_microinit,
            Duration::from_millis(50),
            &mut last_skew,
        );
        assert_eq!(reason, WakeReason::Timeout);
    }
}
