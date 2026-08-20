//! Unix control socket: length-prefixed JSON, one request per connection.
//!
//! Protocol matches loco-server / microinit: 4-byte little-endian length + JSON.
//! Request `{ "type": "services_list" }` returns the current DesiredAds snapshot
//! (static `services[]` plus dynamic dcc-bus / microinit DNS-SD; not Z21 LAN beacons).

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ServiceEntry;
use crate::datadir;
use crate::error::{Error, Result};
use crate::run::{DesiredAds, DynSource};

const MAX_FRAME: usize = 1024 * 1024;

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

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(rename = "type")]
    type_: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
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
    let payload = serde_json::to_vec(msg)?;
    if payload.len() > MAX_FRAME {
        return Err(Error::Ipc(format!(
            "frame length {} exceeds max {MAX_FRAME}",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| Error::Ipc("frame too large for u32 length prefix".into()))?
        .to_le_bytes();
    writer.write_all(&len)?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame_from(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(Error::Ipc(format!("frame length {len} too large")));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn write_frame(stream: &mut UnixStream, msg: &impl Serialize) -> Result<()> {
    write_frame_to(stream, msg)
}

pub fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    read_frame_from(stream)
}

/// Bind the control socket without stealing a live daemon's inode.
///
/// 1. `connect` — if a peer answers, refuse (`already running`); do not unlink.
/// 2. `NotFound` / `ConnectionRefused` — leftover inode (or nothing) → unlink, then bind.
/// 3. Any other connect error is returned as-is (do not unlink a mystery path).
fn bind_singleton(socket_path: &Path) -> Result<UnixListener> {
    match UnixStream::connect(socket_path) {
        Ok(stream) => {
            let pid = peer_pid(&stream);
            let where_ = if pid != 0 {
                format!("{} (pid {pid})", socket_path.display())
            } else {
                socket_path.display().to_string()
            };
            return Err(Error::Ipc(format!("microdns already running at {where_}")));
        }
        Err(e) if is_stale_socket_connect_error(&e) => {}
        Err(e) => return Err(Error::io_at(socket_path, e)),
    }
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(socket_path, e)),
    }
    UnixListener::bind(socket_path).map_err(|e| Error::io_at(socket_path, e))
}

fn is_stale_socket_connect_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn peer_pid(stream: &UnixStream) -> u32 {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .map(|c| c.pid() as u32)
        .unwrap_or(0)
}

fn apply_socket_perms(socket_path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(socket_path)
        .map_err(|e| Error::io_at(socket_path, e))?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(socket_path, perms).map_err(|e| Error::io_at(socket_path, e))?;
    Ok(())
}

/// Bind `$DATA_DIR/run/microdns.sock` (or `path`) and serve `services_list` in a
/// background accept thread. Snapshot is read on each request.
pub fn serve(path: &Path, snapshot: Arc<RwLock<DesiredAds>>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
        }
    }
    let listener = bind_singleton(path)?;
    apply_socket_perms(path)?;
    log::info!("ctl listening on {}", path.display());

    let path = path.to_path_buf();
    thread::Builder::new()
        .name("ctl".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let snap = Arc::clone(&snapshot);
                        thread::spawn(move || handle_conn(stream, snap));
                    }
                    Err(_) => {
                        if !path.exists() {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(|e| Error::Ipc(format!("ctl thread: {e}")))?;
    Ok(())
}

fn handle_conn(mut stream: UnixStream, snapshot: Arc<RwLock<DesiredAds>>) {
    let raw = match read_frame(&mut stream) {
        Ok(b) => b,
        Err(_) => return,
    };
    let req: Request = match serde_json::from_slice(&raw) {
        Ok(r) => r,
        Err(_) => {
            let _ = write_frame(
                &mut stream,
                &ErrorBody {
                    error: "invalid_request".into(),
                },
            );
            return;
        }
    };
    if req.type_ != "services_list" {
        let _ = write_frame(
            &mut stream,
            &ErrorBody {
                error: "invalid_request".into(),
            },
        );
        return;
    }
    let ads = match snapshot.read() {
        Ok(g) => g.clone(),
        Err(_) => {
            let _ = write_frame(
                &mut stream,
                &ErrorBody {
                    error: "internal_error".into(),
                },
            );
            return;
        }
    };
    let body = ServicesListBody {
        services: listed_services(&ads),
    };
    let _ = write_frame(&mut stream, &body);
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
