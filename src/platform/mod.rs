//! Win32 platform layer: the host window, a small HTTP client for filter-list
//! updates, and shell integration for downloads.
//!
//! Everything here is `cfg(windows)`; the portable core (blocker, storage,
//! vault, config, IPC) does not depend on this module.

pub mod http;
pub mod shell;
pub mod window;
