//! Browser orchestration: window, tabs, and the memory-reclaim policy.

pub mod reclaim;

#[cfg(windows)]
pub mod app;
#[cfg(windows)]
pub mod tab;

#[cfg(windows)]
pub use app::App;
