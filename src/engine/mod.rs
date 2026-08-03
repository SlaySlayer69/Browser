//! The WebView2 binding: environment creation, per-WebView configuration, the
//! Chromium command line, and the resource-context mapping the blocker needs.
//!
//! `flags` and `resource` are portable so their rules can be unit tested on any
//! host; the rest talks to WebView2 directly and is Windows-only.

pub mod flags;
pub mod resource;

#[cfg(windows)]
pub mod environment;
#[cfg(windows)]
pub mod webview;
