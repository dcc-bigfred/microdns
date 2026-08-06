//! Z21 LAN discovery beacon: broadcast LAN_GET_SERIAL_NUMBER reply frames.
//!
//! Frame format matches Go `z21server.SerialReply`:
//! `DataLen(u16 LE) + Header(u16 LE=0x0010) + serial(u32 LE)`.
//! Default serial when no layout: `258_000_000`.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};

/// HeaderGetSerialNumber = 0x0010
pub const HEADER_GET_SERIAL_NUMBER: u16 = 0x0010;

/// VirtualSerial default when no layout: 258_000_000
pub const DEFAULT_VIRTUAL_SERIAL: u32 = 258_000_000;

const BEACON_INTERVAL: Duration = Duration::from_secs(2);

/// Build a LAN_GET_SERIAL_NUMBER reply frame for `serial`.
#[must_use]
pub fn serial_reply(serial: u32) -> Vec<u8> {
    build_reply(HEADER_GET_SERIAL_NUMBER, &serial.to_le_bytes())
}

fn build_reply(header: u16, data: &[u8]) -> Vec<u8> {
    let data_len = (4 + data.len()) as u16;
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&header.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Spawn a background beacon that broadcasts `frame` to `255.255.255.255:port`
/// every 2 seconds until `stop` is set.
pub fn spawn(port: u16, frame: Vec<u8>, stop: Arc<AtomicBool>) -> Result<()> {
    if port == 0 {
        return Err(Error::Other("beacon port must be non-zero".into()));
    }
    if frame.is_empty() {
        return Err(Error::Other("beacon frame must not be empty".into()));
    }

    thread::Builder::new()
        .name("z21-beacon".into())
        .spawn(move || {
            if let Err(e) = run_beacon(port, &frame, stop) {
                log::warn!("z21 beacon stopped: {e}");
            }
        })
        .map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
}

fn run_beacon(port: u16, frame: &[u8], stop: Arc<AtomicBool>) -> Result<()> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;

    let dest = SocketAddrV4::new(Ipv4Addr::BROADCAST, port);
    log::info!(
        "z21 discovery beacon started dest={dest} length={}",
        frame.len()
    );

    let mut warned = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match sock.send_to(frame, dest) {
            Ok(_) => {
                if warned {
                    log::info!("z21 beacon send recovered");
                    warned = false;
                }
            }
            Err(e) => {
                if !warned {
                    log::warn!("z21 beacon send failed: {e}");
                    warned = true;
                } else {
                    log::debug!("z21 beacon send failed: {e}");
                }
            }
        }
        // Sleep in small slices so stop is responsive.
        let deadline = std::time::Instant::now() + BEACON_INTERVAL;
        while std::time::Instant::now() < deadline {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_serial_frame() {
        let frame = serial_reply(DEFAULT_VIRTUAL_SERIAL);
        // length=8, header=0x0010, serial LE
        assert_eq!(frame.len(), 8);
        assert_eq!(u16::from_le_bytes([frame[0], frame[1]]), 8);
        assert_eq!(u16::from_le_bytes([frame[2], frame[3]]), 0x0010);
        assert_eq!(
            u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
            258_000_000
        );
    }
}
