//! Unix control socket: length-prefixed JSON, one request per connection.
//!
//! Protocol matches loco-server / microinit: 4-byte little-endian length + JSON.
//! Request `{ "type": "services_list" }` returns the current DesiredAds snapshot
//! (static `services[]` plus dynamic dcc-bus / microinit DNS-SD; not Z21 LAN beacons).

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use dcc_daemon::ipc::{
    read_frame_bytes, write_frame_with_limit, AcceptPolicy, Auth, BindError, BindOptions, Command,
    Connection, ErrorHandler, IpcError, RejectReason, Router, SessionMode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ServiceEntry;
use crate::datadir;
use crate::error::{Error, Result};
use crate::mdns::MdnsPublisher;
use crate::run::{DesiredAds, DynSource};
use crate::selfcheck;

const MAX_FRAME: usize = 1024 * 1024;

/// Live daemon handles the ctl `doctor` request can read.
#[derive(Clone)]
pub struct CtlRuntime {
    pub publisher: Arc<Mutex<MdnsPublisher>>,
    pub selfcheck: Arc<RwLock<selfcheck::Report>>,
}

/// Default control socket under the data root.
#[must_use]
pub fn default_socket() -> PathBuf {
    datadir::path(["run", "microdns.sock"])
}

/// Origin of a listed DNS-SD advertisement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ServiceSource {
    Static,
    DccBus,
    Microinit,
}

impl ServiceSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::DccBus => "dccBus",
            Self::Microinit => "microinit",
        }
    }
}

/// One advertised DNS-SD service as returned by `services_list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListedService {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub protocol: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt: Option<std::collections::HashMap<String, String>>,
    pub source: ServiceSource,
}

impl ListedService {
    fn from_entry(entry: &ServiceEntry, source: ServiceSource) -> Self {
        Self {
            name: entry.name.clone(),
            type_: entry.type_.clone(),
            protocol: entry.protocol.clone(),
            port: entry.port,
            host: entry.host.clone(),
            txt: entry.txt.clone(),
            source,
        }
    }
}

/// Success body for `services_list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServicesListBody {
    pub services: Vec<ListedService>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
}

/// Live-daemon slice of a doctor report (ctl `doctor` response).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonDoctor {
    pub services: Vec<ListedService>,
    pub registered: Vec<String>,
    pub metrics: HashMap<String, i64>,
    pub last_announce_secs_ago: HashMap<String, u64>,
    pub selfcheck: selfcheck::Report,
}

/// Flatten the current desired advertisement set for the ctl API.
///
/// Beacons are omitted: they are Z21 LAN broadcasts, not DNS-SD.
#[must_use]
pub fn listed_services(ads: &DesiredAds) -> Vec<ListedService> {
    let mut out = Vec::with_capacity(ads.static_services.len() + ads.dynamic.len());
    for svc in &ads.static_services {
        out.push(ListedService::from_entry(svc, ServiceSource::Static));
    }
    for dyn_ad in &ads.dynamic {
        let source = match dyn_ad.source {
            DynSource::DccBus => ServiceSource::DccBus,
            DynSource::Microinit => ServiceSource::Microinit,
        };
        out.push(ListedService::from_entry(&dyn_ad.entry, source));
    }
    out
}

pub fn write_frame_to(writer: &mut impl Write, msg: &impl Serialize) -> Result<()> {
    write_frame_with_limit(writer, msg, MAX_FRAME).map_err(|e| Error::Ipc(e.to_string()))
}

pub fn read_frame_from(reader: &mut impl std::io::Read) -> Result<Vec<u8>> {
    read_frame_bytes(reader, MAX_FRAME).map_err(|e| Error::Ipc(e.to_string()))
}

pub fn write_frame(stream: &mut UnixStream, msg: &impl Serialize) -> Result<()> {
    write_frame_to(stream, msg)
}

pub fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    read_frame_from(stream)
}

fn map_bind(e: BindError) -> Error {
    match e {
        BindError::AlreadyRunning {
            process_name,
            location,
            ..
        } => Error::Ipc(format!("{process_name} already running at {location}")),
        BindError::Io { path, source } => Error::io_at(path, source),
    }
}

struct CtlState {
    snapshot: Arc<RwLock<DesiredAds>>,
    runtime: Option<CtlRuntime>,
}

struct ServicesListCmd;
struct DoctorCmd;

impl Command<CtlState> for ServicesListCmd {
    fn name(&self) -> &'static str {
        "services_list"
    }
    fn execute(
        &self,
        state: &CtlState,
        _body: Value,
        conn: &mut Connection,
    ) -> std::result::Result<(), IpcError> {
        let ads = match state.snapshot.read() {
            Ok(g) => g.clone(),
            Err(_) => {
                let _ = conn.reply(&ErrorBody {
                    error: "internal_error".into(),
                });
                return Ok(());
            }
        };
        conn.reply(&ServicesListBody {
            services: listed_services(&ads),
        })
        .map_err(IpcError::from)
    }
}

impl Command<CtlState> for DoctorCmd {
    fn name(&self) -> &'static str {
        "doctor"
    }
    fn execute(
        &self,
        state: &CtlState,
        _body: Value,
        conn: &mut Connection,
    ) -> std::result::Result<(), IpcError> {
        match build_daemon_doctor(&state.snapshot, state.runtime.as_ref()) {
            Ok(body) => conn.reply(&body).map_err(IpcError::from),
            Err(e) => {
                let _ = conn.reply(&ErrorBody {
                    error: e.to_string(),
                });
                Ok(())
            }
        }
    }
}

struct CtlHooks;

impl ErrorHandler<CtlState> for CtlHooks {
    fn unknown(&self, _state: &CtlState, _type_name: &str, _body: &Value, conn: &mut Connection) {
        let _ = conn.reply(&ErrorBody {
            error: "invalid_request".into(),
        });
    }
    fn error(&self, _state: &CtlState, _err: &IpcError, conn: &mut Connection) {
        let _ = conn.reply(&ErrorBody {
            error: "invalid_request".into(),
        });
    }
    fn reject(&self, _state: &CtlState, _reason: RejectReason, _conn: &mut Connection) {}
}

fn ctl_router() -> std::result::Result<Router<CtlState>, Error> {
    let mut router = Router::new();
    router
        .add(ServicesListCmd)
        .map_err(|e| Error::Other(e.to_string()))?;
    router
        .add(DoctorCmd)
        .map_err(|e| Error::Other(e.to_string()))?;
    Ok(router)
}

/// Bind `$DATA_DIR/run/microdns.sock` (or `path`) and serve `services_list` in a
/// background accept thread. Snapshot is read on each request.
pub fn serve(path: &Path, snapshot: Arc<RwLock<DesiredAds>>) -> Result<()> {
    serve_with_runtime(path, snapshot, None)
}

/// Like [`serve`], with optional live publisher / selfcheck for `doctor`.
pub fn serve_with_runtime(
    path: &Path,
    snapshot: Arc<RwLock<DesiredAds>>,
    runtime: Option<CtlRuntime>,
) -> Result<()> {
    let state = Arc::new(CtlState { snapshot, runtime });
    dcc_daemon::ipc::serve_background(
        BindOptions {
            path: path.to_path_buf(),
            mode: 0o600,
            chown: None,
            process_name: "microdns",
        },
        AcceptPolicy {
            auth: Auth::None,
            session: SessionMode::OneShot,
            max_clients: None,
            max_frame: MAX_FRAME,
        },
        ctl_router()?,
        CtlHooks,
        state,
    )
    .map_err(map_bind)?;
    log::info!("ctl listening on {}", path.display());
    Ok(())
}

fn build_daemon_doctor(
    snapshot: &RwLock<DesiredAds>,
    runtime: Option<&CtlRuntime>,
) -> Result<DaemonDoctor> {
    let ads = snapshot
        .read()
        .map(|g| g.clone())
        .map_err(|_| Error::Ipc("internal_error".into()))?;
    let services = listed_services(&ads);
    let (registered, metrics, last_announce_secs_ago) = if let Some(rt) = runtime {
        let pub_guard = match rt.publisher.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let now = std::time::Instant::now();
        let last: HashMap<String, u64> = pub_guard
            .announce_log()
            .snapshot()
            .into_iter()
            .map(|(k, at)| (k, now.saturating_duration_since(at).as_secs()))
            .collect();
        let mut names: Vec<String> = pub_guard.registered_names().into_iter().collect();
        names.sort();
        (names, pub_guard.metrics(), last)
    } else {
        (Vec::new(), HashMap::new(), HashMap::new())
    };
    let selfcheck = runtime
        .and_then(|rt| rt.selfcheck.read().ok().map(|g| g.clone()))
        .unwrap_or_default();
    Ok(DaemonDoctor {
        services,
        registered,
        metrics,
        last_announce_secs_ago,
        selfcheck,
    })
}

fn connect(socket_path: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket_path).map_err(|e| {
        Error::Ipc(format!(
            "cannot connect to {}: {e} (is microdns running?)",
            socket_path.display()
        ))
    })
}

fn request_raw(socket_path: &Path, req: &impl Serialize) -> Result<Vec<u8>> {
    let mut stream = connect(socket_path)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    write_frame(&mut stream, req)?;
    read_frame(&mut stream)
}

/// Query a live daemon for the current advertisement list.
pub fn services_list(socket_path: &Path) -> Result<Vec<ListedService>> {
    let raw = request_raw(socket_path, &serde_json::json!({"type": "services_list"}))?;
    if let Ok(err) = serde_json::from_slice::<ErrorBody>(&raw) {
        if !err.error.is_empty() {
            return Err(Error::Ipc(err.error));
        }
    }
    let body: ServicesListBody = serde_json::from_slice(&raw)?;
    Ok(body.services)
}

/// Query a live daemon for a doctor snapshot (metrics, selfcheck, registered names).
pub fn doctor(socket_path: &Path) -> Result<DaemonDoctor> {
    let raw = request_raw(socket_path, &serde_json::json!({"type": "doctor"}))?;
    if let Ok(err) = serde_json::from_slice::<ErrorBody>(&raw) {
        if !err.error.is_empty() {
            return Err(Error::Ipc(err.error));
        }
    }
    Ok(serde_json::from_slice(&raw)?)
}

/// Human table: NAME, TYPE, PROTO, PORT, HOST, SOURCE (tabwriter-style).
pub fn print_human(w: &mut impl Write, services: &[ListedService]) -> Result<()> {
    const HDR: [&str; 6] = ["NAME", "TYPE", "PROTO", "PORT", "HOST", "SOURCE"];
    let mut rows: Vec<[String; 6]> = Vec::with_capacity(services.len());
    for s in services {
        let host = s
            .host
            .as_deref()
            .filter(|h| !h.is_empty())
            .unwrap_or("-")
            .to_string();
        rows.push([
            s.name.clone(),
            s.type_.clone(),
            s.protocol.clone(),
            s.port.to_string(),
            host,
            s.source.as_str().to_string(),
        ]);
    }
    let mut widths = [0usize; 6];
    for (i, h) in HDR.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    write_padded_row(w, &HDR.map(str::to_string), &widths)?;
    for row in &rows {
        write_padded_row(w, row, &widths)?;
    }
    Ok(())
}

fn write_padded_row(w: &mut impl Write, cells: &[String], widths: &[usize]) -> Result<()> {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            write!(w, "  ")?;
        }
        if i + 1 == cells.len() {
            write!(w, "{cell}")?;
        } else {
            write!(w, "{cell:<width$}", width = widths[i])?;
        }
    }
    writeln!(w)?;
    Ok(())
}

/// Pretty-print `{ "services": [...] }`.
pub fn print_json(w: &mut impl Write, services: &[ListedService]) -> Result<()> {
    serde_json::to_writer_pretty(
        &mut *w,
        &ServicesListBody {
            services: services.to_vec(),
        },
    )?;
    writeln!(w)?;
    Ok(())
}
