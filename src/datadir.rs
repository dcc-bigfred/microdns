//! Persistent data root resolution.
//!
//! Priority: `DATA_DIR` (absolute only), then `/data` (hub default).

pub use dcc_daemon::datadir::{path, root, set_root, DEFAULT_ROOT, ENV_DATA_DIR};
