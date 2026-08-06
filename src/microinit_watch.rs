//! Client for microinit length-prefixed JSON IPC.
//!
//! Protocol: 4-byte little-endian length + JSON payload.
//! Used to discover whether `dcc-bus` is running and obtain its PID.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    List,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    List {
        services: Vec<ServiceStatus>,
    },
    Error {
        message: String,
        #[serde(default)]
        #[allow(dead_code)]
        code: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub pid: Option<i32>,
}

/// Prefix used by BigFred for per-station microinit services (`dcc-bus-2-5`, …).
pub const DCC_BUS_PREFIX: &str = "dcc-bus";

/// Returns every microinit service whose name is `dcc-bus` or starts with `dcc-bus-`.
pub fn list_dcc_bus_services(socket_path: &Path) -> Result<Vec<ServiceStatus>> {
    let resp = request(socket_path, &Request::List)?;
    match resp {
        Response::List { services } => Ok(services
            .into_iter()
            .filter(|s| is_dcc_bus_name(&s.name))
            .collect()),
        Response::Error { message, .. } => Err(Error::Ipc(message)),
        _ => Err(Error::Ipc("unexpected list response".into())),
    }
}

/// True for `dcc-bus` or `dcc-bus-<layout>-<cs>` style names.
#[must_use]
pub fn is_dcc_bus_name(name: &str) -> bool {
    name == DCC_BUS_PREFIX || name.starts_with("dcc-bus-")
}

fn request(socket_path: &Path, req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| Error::Ipc(format!("cannot connect to {}: {e}", socket_path.display())))?;
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

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(Error::Ipc(format!("frame length {len} too large")));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

/// True when the service reports a running state with a PID.
#[must_use]
pub fn is_running(status: &ServiceStatus) -> bool {
    status.state.eq_ignore_ascii_case("running") && status.pid.is_some_and(|p| p > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_helper() {
        assert!(is_running(&ServiceStatus {
            name: "dcc-bus-2-5".into(),
            state: "running".into(),
            pid: Some(42),
        }));
        assert!(!is_running(&ServiceStatus {
            name: "dcc-bus-2-5".into(),
            state: "stopped".into(),
            pid: Some(42),
        }));
        assert!(!is_running(&ServiceStatus {
            name: "dcc-bus-2-5".into(),
            state: "running".into(),
            pid: None,
        }));
    }

    #[test]
    fn dcc_bus_name_prefix() {
        assert!(is_dcc_bus_name("dcc-bus"));
        assert!(is_dcc_bus_name("dcc-bus-2-5"));
        assert!(!is_dcc_bus_name("bigfred"));
        assert!(!is_dcc_bus_name("dcc-busy"));
    }

    #[test]
    fn request_serializes_snake_case() {
        let l = serde_json::to_string(&Request::List).unwrap();
        assert_eq!(l, r#"{"type":"list"}"#);
    }
}
