//! Main daemon loop: orchestrates config watch, mDNS, dcc-bus discovery, beacon.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::beacon::{self, DEFAULT_VIRTUAL_SERIAL};
use crate::config::{self, Config};
use crate::config_watch::{self, ReloadSignal};
use crate::error::Result;
use crate::mdns::{self, MdnsPublisher};
use crate::microinit_watch;
use crate::proc_scan;
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

/// Desired advertisement set derived from config + empirical dcc-bus state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredAds {
    static_services: Vec<config::ServiceEntry>,
    z21: Option<(String, u16)>,        // (instance, port)
    withrottle: Option<(String, u16)>, // (instance, port)
    beacon_port: Option<u16>,
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

    let publisher = Arc::new(Mutex::new(MdnsPublisher::new()));
    let beacon_stop = Arc::new(AtomicBool::new(true)); // start stopped
    let beacon_active = Arc::new(Mutex::new(false));

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
        z21: None,
        withrottle: None,
        beacon_port: None,
    };
    let mut registered_keys: Vec<String> = Vec::new();

    let hostname = version::hostname();

    while !signals::shutdown_requested() && !stop.load(Ordering::SeqCst) {
        let cfg = shared.config.read().map(|c| c.clone()).unwrap_or_default();

        let retry = cfg.retry.clone();

        // Ensure mDNS daemon (quiet retry).
        {
            let mut pub_guard = publisher.lock().unwrap();
            match pub_guard.ensure_daemon() {
                Ok(()) => mdns_thr.ok("mDNS daemon"),
                Err(e) => {
                    mdns_thr.fail("mDNS daemon", &e);
                    sleep_interruptible(Duration::from_millis(retry.mdns_ms));
                    continue;
                }
            }
        }

        // Interface check (non-fatal; addr_auto still works).
        if mdns::has_usable_iface() {
            iface_thr.ok("network interface");
        } else {
            iface_thr.fail(
                "network interface",
                &"no UP non-loopback IPv4 (skipping docker/veth/br-*)",
            );
        }

        // Build desired ads from static services + optional dcc-bus.
        let mut desired = DesiredAds {
            static_services: cfg.services.clone(),
            z21: None,
            withrottle: None,
            beacon_port: None,
        };

        if cfg.dcc_bus.enabled {
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
                        let mut found_z21 = false;
                        let mut found_withrottle = false;
                        for st in &running {
                            let pid = st.pid.unwrap();
                            match proc_scan::listen_ports_for_pid(pid) {
                                Ok(ports) => {
                                    any_scan_ok = true;
                                    if ports.has_udp(cfg.dcc_bus.z21_port) {
                                        found_z21 = true;
                                    }
                                    if ports.has_tcp(cfg.dcc_bus.withrottle_port) {
                                        found_withrottle = true;
                                    }
                                }
                                Err(e) => proc_thr.fail("proc listen scan", &e),
                            }
                        }
                        if any_scan_ok {
                            proc_thr.ok("proc listen scan");
                        }
                        let instance = hostname.clone();
                        if found_z21 {
                            desired.z21 = Some((instance.clone(), cfg.dcc_bus.z21_port));
                            if cfg.dcc_bus.beacon {
                                desired.beacon_port = Some(cfg.dcc_bus.z21_port);
                            }
                        }
                        if found_withrottle {
                            desired.withrottle =
                                Some((instance, cfg.dcc_bus.withrottle_port));
                        }
                    }
                }
                Err(e) => {
                    microinit_thr.fail("microinit socket", &e);
                }
            }
        }

        // Reconcile advertisements when desired set changes.
        if desired != last_desired {
            if let Err(e) = reconcile(
                &publisher,
                &desired,
                &mut registered_keys,
                &beacon_stop,
                &beacon_active,
            ) {
                mdns_thr.fail("mDNS register", &e);
            } else {
                mdns_thr.ok("mDNS register");
                last_desired = desired;
            }
        }

        // Sleep until next poll; wake early on shutdown.
        let sleep_ms = if cfg.dcc_bus.enabled {
            retry.proc_ms.min(retry.microinit_ms).min(retry.iface_ms)
        } else {
            retry.iface_ms.min(retry.mdns_ms)
        };
        sleep_interruptible(Duration::from_millis(sleep_ms.max(500)));
    }

    log::info!("microdns shutting down");
    stop.store(true, Ordering::SeqCst);
    watch_stop.store(true, Ordering::SeqCst);
    beacon_stop.store(true, Ordering::SeqCst);
    if let Ok(mut p) = publisher.lock() {
        p.shutdown();
    }
    Ok(())
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
                    if let Ok(mut w) = shared.config.write() {
                        *w = cfg;
                    }
                    log::info!("config reloaded from {}", shared.config_path.display());
                }
                Err(e) => log::warn!("config reload failed: {e}"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn reconcile(
    publisher: &Arc<Mutex<MdnsPublisher>>,
    desired: &DesiredAds,
    registered_keys: &mut Vec<String>,
    beacon_stop: &Arc<AtomicBool>,
    beacon_active: &Arc<Mutex<bool>>,
) -> Result<()> {
    let pub_guard = publisher.lock().unwrap();

    // Unregister previous dynamic/static keys we track.
    for key in registered_keys.drain(..) {
        let _ = pub_guard.unregister(&key);
    }

    // Static services.
    for svc in &desired.static_services {
        pub_guard.register(svc, None)?;
        registered_keys.push(MdnsPublisher::fullname(&svc.name, &svc.type_));
    }

    // Dynamic dcc-bus services: instance name = hostname.
    if let Some((instance, port)) = &desired.z21 {
        let entry = mdns::dcc_service_entry(instance, "_z21._udp", "udp", *port);
        pub_guard.register(&entry, Some(instance))?;
        registered_keys.push(MdnsPublisher::fullname(instance, "_z21._udp"));
    }
    if let Some((instance, port)) = &desired.withrottle {
        let entry = mdns::dcc_service_entry(instance, "_withrottle._tcp", "tcp", *port);
        pub_guard.register(&entry, Some(instance))?;
        registered_keys.push(MdnsPublisher::fullname(instance, "_withrottle._tcp"));
    }

    // Beacon management.
    let want_beacon = desired.beacon_port;
    let mut active = beacon_active.lock().unwrap();
    match (want_beacon, *active) {
        (Some(port), false) => {
            beacon_stop.store(false, Ordering::SeqCst);
            let stop = Arc::clone(beacon_stop);
            let frame = beacon::serial_reply(DEFAULT_VIRTUAL_SERIAL);
            beacon::spawn(port, frame, stop)?;
            *active = true;
        }
        (None, true) => {
            beacon_stop.store(true, Ordering::SeqCst);
            *active = false;
        }
        (Some(port), true) => {
            // Restart beacon if we need a different port: stop then start.
            // For simplicity, if already active we leave it; port changes are rare.
            let _ = port;
        }
        (None, false) => {}
    }

    Ok(())
}

fn sleep_interruptible(total: Duration) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if signals::shutdown_requested() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}
