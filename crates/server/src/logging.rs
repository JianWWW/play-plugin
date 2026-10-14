//! Runtime log-level control.
//!
//! The app installs the global EnvFilter's reload handle at startup; a page
//! sending `hello {debug:true}` raises the level to debug. Raise-only — the
//! native process is shared machine-wide, see docs/adr/0001.

use std::sync::OnceLock;
use tracing::level_filters::LevelFilter;
use tracing::Level;
use tracing_subscriber::reload::Handle;
use tracing_subscriber::{EnvFilter, Registry};

static HANDLE: OnceLock<Handle<EnvFilter, Registry>> = OnceLock::new();

/// Called once by the app during `init_logging`.
pub fn install(handle: Handle<EnvFilter, Registry>) {
    let _ = HANDLE.set(handle);
}

/// Raise (never lower) the process log level to `level`.
/// No-op if the current level is already at least as verbose, or before `install`.
pub fn raise_to(level: &str) {
    let Some(handle) = HANDLE.get() else { return };
    let Ok(target) = level.parse::<Level>() else { return };
    let mut current = None;
    let _ = handle.modify(|f| current = f.max_level_hint());
    if should_raise(current, target) {
        let _ = handle.reload(EnvFilter::new(level));
    }
}

/// None (unknown) → raise; never lower an explicit more-verbose level
/// (e.g. RUST_LOG=trace must survive a page requesting debug).
fn should_raise(current: Option<LevelFilter>, target: Level) -> bool {
    current.map_or(true, |c| c < LevelFilter::from(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_only() {
        assert!(should_raise(None, Level::DEBUG));
        assert!(should_raise(Some(LevelFilter::OFF), Level::DEBUG));
        assert!(should_raise(Some(LevelFilter::INFO), Level::DEBUG));
        assert!(!should_raise(Some(LevelFilter::DEBUG), Level::DEBUG));
        assert!(!should_raise(Some(LevelFilter::TRACE), Level::DEBUG)); // don't lower
    }
}
