//! Signal handling: SIGTERM / SIGINT request graceful shutdown.

use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

use crate::error::Result;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install SIGTERM / SIGINT handlers (async-signal-safe: only sets a flag).
pub fn install_handlers() -> Result<()> {
    // SAFETY: handler only stores to an AtomicBool (async-signal-safe).
    let action = SigAction::new(
        SigHandler::Handler(handle_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGTERM, &action)?;
        sigaction(Signal::SIGINT, &action)?;
    }
    Ok(())
}

/// Returns true once a shutdown signal has been received.
#[must_use]
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Request shutdown from another thread (tests / programmatic stop).
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Clear the shutdown flag (tests).
#[cfg(test)]
pub fn clear_shutdown() {
    SHUTDOWN.store(false, Ordering::SeqCst);
}
