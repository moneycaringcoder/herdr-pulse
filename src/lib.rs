//! pulse — per-workspace agent activity history for herdr.
//!
//! The crate is split into a library plus a thin binary so that the integration
//! tests in `tests/` can reach the real modules. A binary-only crate would hide
//! all of this behind `#[path]` includes, which break as soon as a module refers
//! to `crate::`.

pub mod config;
pub mod daemon;
pub mod herdr;
pub mod history;
pub mod model;
pub(crate) mod private_fs;
pub mod render;
pub mod setup;
pub mod supervise;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Seconds since the Unix epoch.
///
/// A clock that predates the epoch is impossible in practice and meaningless
/// here, so it collapses to 0 rather than propagating an error through every
/// call site.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
