//! microdns — mDNS/DNS-SD advertisement daemon for BigFred OS.

pub mod beacon;
pub mod bigfred_watch;
pub mod config;
pub mod config_watch;
pub mod ctl;
pub mod datadir;
pub mod error;
pub mod iface_watch;
pub mod legacy_unicast;
pub mod mdns;
pub mod run;
pub mod signals;
pub(crate) mod sys;
pub mod version;

pub use error::{Error, Result};
