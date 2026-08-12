//! microdns — mDNS/DNS-SD advertisement daemon for BigFred OS.

pub mod beacon;
pub mod config;
pub mod config_watch;
pub mod datadir;
pub mod error;
pub mod iface_watch;
pub mod legacy_unicast;
pub mod mdns;
pub mod microinit_watch;
pub mod proc_scan;
pub mod run;
pub mod signals;
pub mod version;

pub use error::{Error, Result};
