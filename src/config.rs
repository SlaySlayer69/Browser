//! Profile paths and user settings.
//!
//! Settings are a flat JSON document so the file stays readable and the parse
//! cost at startup is a few microseconds. Anything that changes Chromium's
//! command line lives here, because those values have to be known *before* the
//! WebView2 environment is created and cannot be changed afterwards.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a profile keeps its state on disk.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Root of the profile, e.g. `%LOCALAPPDATA%\CleanDark`.
    pub root: PathBuf,
    /// WebView2's own user-data folder (cache, cookies, local storage).
    pub webview_data: PathBuf,
    /// Our SQLite database (history, downloads, bookmarks, speed dials).
    pub database: PathBuf,
    /// The encrypted password vault.
    pub vault: PathBuf,
    /// Pre-compiled adblock engine, so startup never re-parses the raw lists.
    pub filter_cache: PathBuf,
    /// Raw downloaded filter lists.
    pub filter_lists: PathBuf,
    /// settings.json
    pub settings: PathBuf,
}

impl Paths {
    pub fn resolve() -> std::io::Result<Self> {
        let root = directories::ProjectDirs::from("", "", "CleanDark")
            .map(|dirs| dirs.data_local_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let paths = Self {
            webview_data: root.join("webview2"),
            database: root.join("cleandark.sqlite"),
            vault: root.join("vault.bin"),
            filter_cache: root.join("filters.bin"),
            filter_lists: root.join("lists"),
            settings: root.join("settings.json"),
            root,
        };

        fs::create_dir_all(&paths.root)?;
        fs::create_dir_all(&paths.webview_data)?;
        fs::create_dir_all(&paths.filter_lists)?;
        Ok(paths)
    }
}

/// How aggressively background tabs are reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReclaimMode {
    /// Never touch background tabs. Fastest switching, highest RAM.
    Off,
    /// Lower the renderer's memory target, then suspend after a delay.
    Balanced,
    /// Suspend quickly and fully discard long-idle tabs (renderer torn down,
    /// tab reloads on activation).
    Aggressive,
}

impl Default for ReclaimMode {
    fn default() -> Self {
        Self::Balanced
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    // ---- Search -------------------------------------------------------
    /// `{q}` is replaced with the percent-encoded query.
    pub search_template: String,

    // ---- Memory -------------------------------------------------------
    pub reclaim_mode: ReclaimMode,
    /// Seconds a tab must be inactive before it is suspended.
    pub suspend_after_secs: u64,
    /// Seconds a tab must be inactive before it is fully discarded.
    /// Only used in [`ReclaimMode::Aggressive`].
    pub discard_after_secs: u64,
    /// Hard cap on concurrent renderer processes.
    pub renderer_process_limit: u32,
    /// On-disk HTTP cache cap, in megabytes.
    pub disk_cache_mb: u32,
    /// Disabling the back/forward cache frees a retained renderer heap per tab
    /// at the cost of slower history navigation.
    pub disable_back_forward_cache: bool,

    // ---- Security trade-offs (opt-in, default = keep the protection) ----
    /// Collapses same-site frames into one process. Saves a lot of RAM on
    /// heavily-framed pages, but weakens Spectre-class isolation between a
    /// page and its cross-origin iframes. Off by default on purpose.
    pub disable_site_isolation: bool,
    /// SmartScreen sends URL reputation data to Microsoft. Turning it off is a
    /// privacy/CPU win and a safety loss.
    pub disable_smartscreen: bool,

    // ---- Content blocking ---------------------------------------------
    pub adblock_enabled: bool,
    /// Rebuild the compiled engine when the cache is older than this.
    pub filter_refresh_days: u64,
    pub filter_list_urls: Vec<String>,

    // ---- Appearance ----------------------------------------------------
    /// Opt-in: tint the chrome with the page's `<meta name="theme-color">`.
    pub chameleon_enabled: bool,
    /// Fallback accent used when a page has no theme-color.
    pub accent: String,
    /// Respect the OS "reduce motion" setting *and* allow forcing it here.
    pub reduce_motion: bool,

    // ---- Privacy -------------------------------------------------------
    pub save_history: bool,
    /// Delete downloads older than this many days from the list (files stay).
    pub download_history_days: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search_template: "https://duckduckgo.com/?q={q}".into(),

            reclaim_mode: ReclaimMode::Balanced,
            suspend_after_secs: 120,
            discard_after_secs: 1800,
            renderer_process_limit: 8,
            disk_cache_mb: 128,
            disable_back_forward_cache: true,

            disable_site_isolation: false,
            disable_smartscreen: false,

            adblock_enabled: true,
            filter_refresh_days: 5,
            filter_list_urls: vec![
                "https://easylist.to/easylist/easylist.txt".into(),
                "https://easylist.to/easylist/easyprivacy.txt".into(),
                "https://secure.fanboy.co.nz/fanboy-annoyance.txt".into(),
            ],

            chameleon_enabled: false,
            accent: "#4c8dff".into(),
            reduce_motion: false,

            save_history: true,
            download_history_days: 90,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        // A malformed or partially-written settings file must never stop the
        // browser from starting; fall back to defaults and rewrite on save.
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Write-then-rename so a crash mid-write cannot truncate the file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }

    /// Suspend/discard thresholds as durations, `None` when reclaiming is off.
    pub fn reclaim_thresholds(&self) -> Option<(u64, Option<u64>)> {
        match self.reclaim_mode {
            ReclaimMode::Off => None,
            ReclaimMode::Balanced => Some((self.suspend_after_secs, None)),
            ReclaimMode::Aggressive => Some((
                self.suspend_after_secs.min(30),
                Some(self.discard_after_secs),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_json() {
        let json = serde_json::to_string(&Settings::default()).unwrap();
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.search_template, Settings::default().search_template);
        assert_eq!(parsed.reclaim_mode, ReclaimMode::Balanced);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let parsed: Settings = serde_json::from_str("{}").unwrap();
        assert!(parsed.adblock_enabled);
        assert!(!parsed.disable_site_isolation);
    }

    #[test]
    fn security_downgrades_are_off_by_default() {
        let s = Settings::default();
        assert!(!s.disable_site_isolation, "site isolation must stay on unless opted out");
        assert!(!s.disable_smartscreen);
    }

    #[test]
    fn aggressive_mode_clamps_suspend_delay_and_enables_discard() {
        let s = Settings { reclaim_mode: ReclaimMode::Aggressive, ..Default::default() };
        let (suspend, discard) = s.reclaim_thresholds().unwrap();
        assert_eq!(suspend, 30);
        assert_eq!(discard, Some(1800));
        assert!(Settings { reclaim_mode: ReclaimMode::Off, ..Default::default() }
            .reclaim_thresholds()
            .is_none());
    }
}
