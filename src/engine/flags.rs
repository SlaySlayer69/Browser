//! Chromium command-line construction.
//!
//! These arguments are handed to WebView2 through
//! `ICoreWebView2EnvironmentOptions::AdditionalBrowserArguments`. Three things
//! are worth knowing before editing this file:
//!
//! 1. The string is parsed *once*, when the browser process for a user-data
//!    folder is created. Two environments sharing a user-data folder must pass
//!    identical arguments or the second one fails with
//!    `HRESULT_FROM_WIN32(ERROR_INVALID_STATE)`. That is why incognito reuses
//!    this same string and is separated by a controller option instead.
//!
//! 2. WebView2 rejects a small blocklist outright (notably `--user-data-dir`,
//!    which is passed as its own parameter) and **silently ignores everything
//!    it does not recognise**. A misspelled switch or a renamed Chromium
//!    feature therefore fails invisibly at runtime. Only add entries here that
//!    you have confirmed against the Chromium source for the Edge version you
//!    target; treat the list as something to re-verify, not as fire-and-forget.
//!
//! 3. Flags that trade away a security boundary are gated behind explicit
//!    settings and default to *keeping* the boundary.

use crate::config::Settings;

/// Features disabled unconditionally, passed as one `--disable-features=` list.
///
/// Every entry is either a background service that costs a thread and periodic
/// wakeups, or a helper process unreachable from our UI.
const DISABLED_FEATURES: &[&str] = &[
    // WebView2's out-of-process UI helpers. Each is a separate utility
    // process; neither is reachable in our UI.
    "msWebOOUI",
    "msPdfOOUI",
    // Chromium keeps a spare renderer warm so the *next* navigation starts
    // faster. That is one idle process (tens of MB) we would rather not pay
    // for in a memory-first browser.
    "SpareRendererForSitePerProcess",
    // No sign-in, no sync, no autofill server calls: we ship our own vault.
    "AutofillServerCommunication",
    // Translation UI and its language-detection model.
    "Translate",
    // Periodic hint fetches for the optimization guide.
    "OptimizationHints",
    // Cast/DIAL device discovery sends periodic multicast traffic.
    "MediaRouter",
    "DialMediaRouteProvider",
    // Global media controls keep observers alive per playing tab.
    "GlobalMediaControls",
];

/// Features we turn on because they reduce resident memory or wakeups.
const ENABLED_FEATURES: &[&str] = &[
    // Purge as much renderer memory as possible once every page in a renderer
    // is frozen. This is the in-renderer counterpart to our own TrySuspend
    // policy and lands even before the host decides to suspend a tab.
    "FreezePurgeMemoryAllPagesFrozen",
    // Detect fully-occluded windows and stop painting them.
    "CalculateNativeWinOcclusion",
];

/// Value-less switches.
const SWITCHES: &[&str] = &[
    // One renderer per *site* rather than per tab. Ten tabs on the same site
    // collapse into one process; this is the single largest RAM lever we have,
    // and it does not weaken site isolation — cross-site content still gets
    // its own process.
    "--process-per-site",
    // Variations fetches, component updates, domain-reliability uploads and
    // crash-upload scheduling all live behind this one switch.
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-domain-reliability",
    "--disable-breakpad",
    "--no-pings",
    "--no-first-run",
    "--no-default-browser-check",
    "--no-service-autorun",
    "--disable-speech-api",
    "--disable-client-side-phishing-detection",
    // The extension system is not exposed in the UI; keep its process host and
    // manifest machinery from initialising at all. `are_browser_extensions_
    // enabled = false` on the environment options covers the WebView2 side.
    "--disable-extensions",
];

/// Build the `AdditionalBrowserArguments` string for a profile.
pub fn browser_arguments(settings: &Settings) -> String {
    let mut args: Vec<String> = SWITCHES.iter().map(|s| (*s).to_string()).collect();
    let mut disabled: Vec<&str> = DISABLED_FEATURES.to_vec();

    if settings.disable_back_forward_cache {
        // BFCache keeps a whole frozen document tree alive per history entry.
        disabled.push("BackForwardCache");
    }

    if settings.disable_smartscreen {
        // Opt-in: stops per-navigation URL reputation lookups. Privacy and CPU
        // win, safety loss.
        disabled.push("msSmartScreenProtection");
    }

    if settings.disable_site_isolation {
        // Opt-in, and a real downgrade: cross-origin iframes may now share a
        // renderer with their embedder.
        args.push("--disable-site-isolation-trials".into());
    }

    args.push(format!("--disable-features={}", disabled.join(",")));
    args.push(format!("--enable-features={}", ENABLED_FEATURES.join(",")));
    args.push(format!("--renderer-process-limit={}", settings.renderer_process_limit.max(1)));

    let disk_cache_bytes = u64::from(settings.disk_cache_mb.clamp(16, 4096)) * 1024 * 1024;
    args.push(format!("--disk-cache-size={disk_cache_bytes}"));
    // The media cache is accounted separately from the HTTP cache.
    args.push(format!("--media-cache-size={}", disk_cache_bytes / 4));

    args.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(settings: &Settings) -> String {
        browser_arguments(settings)
    }

    #[test]
    fn process_per_site_is_always_set() {
        let a = args(&Settings::default());
        assert!(a.contains("--process-per-site"));
        // The mutually exclusive alternative must never appear.
        assert!(!a.contains("--process-per-tab"));
    }

    #[test]
    fn security_flags_require_opt_in() {
        let default = args(&Settings::default());
        assert!(!default.contains("--disable-site-isolation-trials"));
        assert!(!default.contains("msSmartScreenProtection"));

        let opted_in = args(&Settings {
            disable_site_isolation: true,
            disable_smartscreen: true,
            ..Default::default()
        });
        assert!(opted_in.contains("--disable-site-isolation-trials"));
        assert!(opted_in.contains("msSmartScreenProtection"));
    }

    #[test]
    fn cache_sizes_are_clamped_to_sane_bounds() {
        let tiny = args(&Settings { disk_cache_mb: 0, ..Default::default() });
        assert!(tiny.contains(&format!("--disk-cache-size={}", 16u64 * 1024 * 1024)));

        let huge = args(&Settings { disk_cache_mb: u32::MAX, ..Default::default() });
        assert!(huge.contains(&format!("--disk-cache-size={}", 4096u64 * 1024 * 1024)));
    }

    #[test]
    fn renderer_limit_never_reaches_zero() {
        // 0 means "unlimited" to Chromium, the opposite of the intent.
        assert!(args(&Settings { renderer_process_limit: 0, ..Default::default() })
            .contains("--renderer-process-limit=1"));
    }

    #[test]
    fn feature_lists_are_emitted_once_and_comma_joined() {
        let a = args(&Settings::default());
        assert_eq!(a.matches("--disable-features=").count(), 1);
        assert_eq!(a.matches("--enable-features=").count(), 1);
        assert!(a.contains("msWebOOUI,msPdfOOUI"));
    }

    #[test]
    fn bfcache_toggle_is_reflected() {
        assert!(args(&Settings::default()).contains("BackForwardCache"));
        assert!(!args(&Settings { disable_back_forward_cache: false, ..Default::default() })
            .contains("BackForwardCache"));
    }

    #[test]
    fn every_switch_is_well_formed() {
        // Guards against a stray space or a missing `--`, which WebView2 would
        // swallow without complaint.
        for switch in SWITCHES {
            assert!(switch.starts_with("--"), "{switch} must start with --");
            assert!(!switch.contains(' '), "{switch} must not contain a space");
        }
        for feature in DISABLED_FEATURES.iter().chain(ENABLED_FEATURES) {
            assert!(!feature.contains(',') && !feature.contains(' '), "{feature} is malformed");
            assert!(!feature.starts_with("--"), "{feature} is a feature name, not a switch");
        }
    }

    #[test]
    fn user_data_dir_is_never_passed_as_an_argument() {
        // WebView2 rejects the whole environment if it appears here.
        assert!(!args(&Settings::default()).contains("--user-data-dir"));
    }
}
