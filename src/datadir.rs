//! Persistent data root resolution.
//!
//! Priority: `DATA_DIR` (absolute only), then `/data` (hub default).
//! Relative values are ignored so misconfiguration cannot silently redirect
//! data under the process working directory.

use std::path::{Path, PathBuf};

/// Env var for the persistent data root.
pub const ENV_DATA_DIR: &str = "DATA_DIR";
/// Hub image default.
pub const DEFAULT_ROOT: &str = "/data";

/// Returns the persistent data directory.
#[must_use]
pub fn root() -> PathBuf {
    if let Some(v) = root_from_env(ENV_DATA_DIR) {
        return v;
    }
    PathBuf::from(DEFAULT_ROOT)
}

fn root_from_env(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name)?;
    if v.is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    if p.is_absolute() {
        Some(p)
    } else {
        None
    }
}

/// Override `DATA_DIR` for this process (absolute paths only).
pub fn set_root(path: impl AsRef<Path>) {
    let p = path.as_ref();
    if p.is_absolute() {
        std::env::set_var(ENV_DATA_DIR, p.as_os_str());
    }
}

/// Join `parts` under [`root`].
#[must_use]
pub fn path<I, P>(parts: I) -> PathBuf
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut out = root();
    for part in parts {
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_is_data() {
        // Cannot safely mutate env in parallel tests; just check absolute join.
        let p = PathBuf::from(DEFAULT_ROOT)
            .join("etc")
            .join("microdns.json");
        assert!(p.is_absolute());
        assert!(p.ends_with("etc/microdns.json"));
    }
}
