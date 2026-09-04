//! Linux inotify-based configuration watcher (no polling).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use dcc_daemon::config::{spawn_signal, PathFilter, WatchSpec};

use crate::error::{Error, Result};

const DEBOUNCE: Duration = Duration::from_millis(300);

/// Signal that the configuration file may have changed.
pub type ReloadSignal = dcc_daemon::config::Reload;

/// Filter path events relevant to the watched config basename.
pub fn is_relevant_path(path: &Path, config_name: &str) -> bool {
    dcc_daemon::config::is_relevant_path(path, &PathFilter::Basename(config_name.to_string()))
}

/// Spawn an inotify watcher thread. Returns a receiver of debounce-coalesced reload signals.
pub fn spawn(config_path: PathBuf) -> Result<(Receiver<ReloadSignal>, Arc<AtomicBool>)> {
    spawn_signal(vec![WatchSpec::file(config_path)], DEBOUNCE)
        .map_err(|e| Error::Other(e.to_string()))
}
