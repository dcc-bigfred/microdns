//! Client for microinit `{type: watch}` on `$DATA_DIR/run/microinit.sock`.
//!
//! Framing matches microinit: 4-byte little-endian length + JSON, max 16 MiB.
//! The connection stays open. Heartbeats are ignored. Snapshots replace the
//! advertised set. Reconnects with backoff; the caller keeps last-good ads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use dcc_daemon::ipc::{read_frame_bytes, write_frame_with_limit};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config, ServiceEntry};
use crate::error::{Error, Result};

/// microinit IPC max payload (see microinit `MAX_IPC_FRAME_BYTES`).
const MAX_FRAME: usize = 16 * 1024 * 1024;
const IDLE_READ: Duration = Duration::from_secs(30);
const WATCH_CAPACITY: usize = 1;
const LABEL_PORT: &str = "microdns-port";
const LABEL_TYPE: &str = "microdns-type";
const LABEL_HOST: &str = "microdns-host";
const TXT_PREFIX: &str = "microdns-txt-";

#[derive(Debug, Clone, Serialize)]
struct WatchRequest {
    #[serde(rename = "type")]
    type_: &'static str,
    label_keys: Vec<&'static str>,
}

#[derive(Debug, Clone, Deserialize)]
struct TypedFrame {
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    services: Vec<WatchedService>,
}

/// One microinit `list`/`watch` row (unknown fields ignored).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WatchedService {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Latest mapped ads plus a coalescing wake signal for the main loop.
pub struct WatchFeed {
    rx: Receiver<()>,
    latest: Arc<Mutex<Option<Vec<ServiceEntry>>>>,
}

impl WatchFeed {
    /// Drain wake signals and copy the newest snapshot into `last` if one exists.
    /// Returns true when a wake was pending (even if the snapshot is unchanged).
    pub fn drain_into(&self, last: &mut Option<Vec<ServiceEntry>>) -> bool {
        let mut woke = false;
        while self.rx.try_recv().is_ok() {
            woke = true;
        }
        if !woke {
            return false;
        }
        let guard = match self.latest.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(ref snap) = *guard {
            *last = Some(snap.clone());
        }
        true
    }

    /// Empty feed used when the watcher thread failed to start.
    #[must_use]
    pub fn disconnected() -> Self {
        let (_tx, rx) = mpsc::sync_channel(WATCH_CAPACITY);
        Self {
            rx,
            latest: Arc::new(Mutex::new(None)),
        }
    }
}

/// Spawn a watch thread. Returns a coalescing snapshot feed and a stop flag.
///
/// Socket path and reconnect interval are read from `config` on each reconnect
/// so a config reload applies without restarting the process.
pub fn spawn(
    config: Arc<RwLock<Config>>,
    stop: Arc<AtomicBool>,
) -> Result<(WatchFeed, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::sync_channel(WATCH_CAPACITY);
    let latest = Arc::new(Mutex::new(None));
    let latest_thr = Arc::clone(&latest);
    let stop_thr = Arc::clone(&stop);
    thread::Builder::new()
        .name("microinit-watch".into())
        .spawn(move || watch_loop(config, tx, latest_thr, stop_thr))
        .map_err(|e| Error::Other(format!("spawn microinit-watch: {e}")))?;
    Ok((WatchFeed { rx, latest }, stop))
}

fn watch_loop(
    config: Arc<RwLock<Config>>,
    tx: SyncSender<()>,
    latest: Arc<Mutex<Option<Vec<ServiceEntry>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut failing = false;
    while !stop.load(Ordering::SeqCst) {
        let (enabled, socket, reconnect) = {
            let cfg = match config.read() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            (
                cfg.microinit.enabled,
                cfg.microinit.socket_path(),
                Duration::from_millis(cfg.retry.microinit_reconnect_ms.max(500)),
            )
        };
        if !enabled {
            sleep_interruptible(reconnect, &stop);
            continue;
        }
        match session(&socket, &tx, &latest, &stop) {
            Ok(()) => {
                if failing {
                    log::info!("microinit watch: recovered");
                    failing = false;
                }
            }
            Err(e) => {
                if !failing {
                    log::warn!("microinit watch: {e}");
                    failing = true;
                } else {
                    log::debug!("microinit watch: {e}");
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        sleep_interruptible(reconnect, &stop);
    }
}

fn session(
    socket: &Path,
    tx: &SyncSender<()>,
    latest: &Mutex<Option<Vec<ServiceEntry>>>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket).map_err(|e| {
        Error::Ipc(format!(
            "cannot connect to {}: {e} (is microinit running?)",
            socket.display()
        ))
    })?;
    let _ = stream.set_read_timeout(Some(IDLE_READ));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    write_frame(
        &mut stream,
        &WatchRequest {
            type_: "watch",
            label_keys: vec![LABEL_PORT],
        },
    )?;

    let mut warned: HashSet<String> = HashSet::new();
    while !stop.load(Ordering::SeqCst) {
        let raw = read_frame(&mut stream)?;
        let frame: TypedFrame = serde_json::from_slice(&raw)?;
        match frame.type_.as_str() {
            "heartbeat" => {}
            "error" => {
                return Err(Error::Ipc(
                    frame.message.unwrap_or_else(|| "watch error".into()),
                ));
            }
            "list" => {
                let ads = ads_from_snapshot(&frame.services, &mut warned);
                publish(tx, latest, ads)?;
            }
            other => {
                log::debug!("microinit watch: ignoring frame type={other}");
            }
        }
    }
    Ok(())
}

fn publish(
    tx: &SyncSender<()>,
    latest: &Mutex<Option<Vec<ServiceEntry>>>,
    ads: Vec<ServiceEntry>,
) -> Result<()> {
    {
        let mut guard = match latest.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = Some(ads);
    }
    match tx.try_send(()) {
        Ok(()) | Err(TrySendError::Full(())) => Ok(()),
        Err(TrySendError::Disconnected(())) => Err(Error::Ipc("watch consumer gone".into())),
    }
}

fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(200);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::SeqCst) {
        let step = left.min(slice);
        thread::sleep(step);
        left = left.saturating_sub(step);
    }
}

/// Map a watch snapshot to DNS-SD entries. Only `running` services with a
/// valid `microdns-port` + `microdns-type` are advertised. Missing
/// `microdns-host` leaves [`ServiceEntry::host`] unset so mDNS uses the kernel
/// hostname.
#[must_use]
pub fn ads_from_snapshot(
    services: &[WatchedService],
    warned: &mut HashSet<String>,
) -> Vec<ServiceEntry> {
    let mut out = Vec::new();
    for svc in services {
        match map_service(svc) {
            MapResult::Ad(entry) => {
                warned.remove(&svc.name);
                out.push(entry);
            }
            MapResult::Skip => {
                warned.remove(&svc.name);
            }
            MapResult::Invalid(reason) => {
                if warned.insert(svc.name.clone()) {
                    log::warn!("microinit watch: skipping '{}': {reason}", svc.name);
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.name, &a.type_, a.port).cmp(&(&b.name, &b.type_, b.port)));
    out
}

enum MapResult {
    Ad(ServiceEntry),
    Skip,
    Invalid(String),
}

fn map_service(svc: &WatchedService) -> MapResult {
    if svc.state != "running" {
        return MapResult::Skip;
    }
    let Some(port_s) = svc.labels.get(LABEL_PORT) else {
        return MapResult::Skip;
    };
    let port: u16 = match port_s.parse() {
        Ok(p) if p != 0 => p,
        _ => {
            return MapResult::Invalid(format!(
                "label {LABEL_PORT}={port_s:?} is not a non-zero u16"
            ));
        }
    };
    let Some(type_) = svc.labels.get(LABEL_TYPE).map(|s| s.trim().to_string()) else {
        return MapResult::Invalid(format!("missing {LABEL_TYPE}"));
    };
    if let Err(e) = config::validate_service_type(&svc.name, &type_) {
        return MapResult::Invalid(e.to_string());
    }
    let Some(protocol) = config::protocol_from_type(&type_) else {
        return MapResult::Invalid(format!("cannot derive protocol from type {type_}"));
    };
    if svc.name.is_empty() {
        return MapResult::Invalid("empty service name".into());
    }
    let host = svc
        .labels
        .get(LABEL_HOST)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let txt = txt_from_labels(&svc.labels);
    MapResult::Ad(ServiceEntry {
        name: svc.name.clone(),
        type_,
        protocol: protocol.to_string(),
        port,
        host,
        txt,
    })
}

fn txt_from_labels(labels: &BTreeMap<String, String>) -> Option<HashMap<String, String>> {
    let mut txt = HashMap::new();
    for (key, value) in labels {
        let Some(rest) = key.strip_prefix(TXT_PREFIX) else {
            continue;
        };
        if rest.is_empty() || value.is_empty() {
            continue;
        }
        txt.insert(rest.to_string(), value.clone());
    }
    if txt.is_empty() {
        None
    } else {
        Some(txt)
    }
}

fn write_frame(stream: &mut UnixStream, msg: &impl Serialize) -> Result<()> {
    write_frame_with_limit(stream, msg, MAX_FRAME).map_err(|e| Error::Ipc(e.to_string()))
}

fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    read_frame_bytes(stream, MAX_FRAME).map_err(|e| Error::Ipc(e.to_string()))
}
