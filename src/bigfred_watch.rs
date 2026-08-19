//! Client for loco-server length-prefixed JSON on `$DATA_DIR/run/bigfred.sock`.
//!
//! Protocol: 4-byte little-endian length + JSON payload. One request per
//! connection (poll). Success body for `dcc_bus_list` is `{ "programs": [...] }`
//! matching REST; errors are `{ "error": "<code>" }`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    DccBusList,
    Version,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DccBusList {
    #[serde(default)]
    pub programs: Vec<Program>,
}

/// One dcc-bus program as returned by loco-server (`DccBusProgramStatus`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Program {
    #[serde(default)]
    pub layout_id: u32,
    #[serde(default)]
    pub layout_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub command_station_id: u32,
    #[serde(default)]
    pub withrottle_enabled: bool,
    #[serde(default)]
    pub withrottle_port: u16,
    #[serde(default)]
    pub z21_enabled: bool,
    #[serde(default)]
    pub z21_port: u16,
}

/// Poll `dcc_bus_list` and return programs.
pub fn dcc_bus_list(socket_path: &Path) -> Result<Vec<Program>> {
    let raw = request_raw(socket_path, &Request::DccBusList)?;
    if let Ok(err) = serde_json::from_slice::<ErrorBody>(&raw) {
        if let Some(code) = err.error {
            if !code.is_empty() {
                return Err(Error::Ipc(code));
            }
        }
    }
    let body: DccBusList = serde_json::from_slice(&raw)?;
    Ok(body.programs)
}

/// DNS-SD instance label (`BigFred #{csId}`).
#[must_use]
pub fn instance_name(command_station_id: u32) -> String {
    format!("BigFred #{command_station_id}")
}

fn request_raw(socket_path: &Path, req: &Request) -> Result<Vec<u8>> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        Error::Ipc(format!(
            "cannot connect to {}: {e} (is loco-server running?)",
            socket_path.display()
        ))
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    write_frame(&mut stream, req)?;
    read_frame(&mut stream)
}

fn write_frame(stream: &mut UnixStream, msg: &impl Serialize) -> Result<()> {
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
    stream.write_all(&len)?;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(Error::Ipc(format!("frame length {len} too large")));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}
