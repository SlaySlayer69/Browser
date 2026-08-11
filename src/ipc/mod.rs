//! The message protocol between the HTML chrome and the Rust host.
//!
//! Everything the UI can do goes through here, and nothing else is exposed:
//! the chrome WebView has no host-object bridge and no `AddHostObjectToScript`
//! surface, so a compromised renderer can only send these variants. Each one is
//! validated on arrival — see [`crate::browser`].
//!
//! Both the chrome WebView and our internal pages (new tab, history,
//! downloads, bookmarks, vault) speak this protocol over
//! `window.chrome.webview.postMessage`.

use serde::{Deserialize, Serialize};

use crate::blocker::Stats;
use crate::stats::PrivacyStats;
use crate::storage::{Bookmark, Download, SpeedDial, Visit};
use crate::vault::CredentialSummary;

/// UI -> host.
#[derive(Debug, Clone, Deserialize)]
// `rename_all` renames the *variants*; `rename_all_fields` is what makes the
// payload keys camelCase. Without the second attribute the UI would have to
// send `new_tab`, which is exactly the kind of mismatch that only shows up at
// runtime.
#[serde(tag = "cmd", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Command {
    // ---- Navigation -------------------------------------------------
    /// Raw omnibox text; the host decides URL vs. search.
    OmniboxSubmit { text: String, #[serde(default)] new_tab: bool },
    Navigate { url: String },
    Back,
    Forward,
    Reload { #[serde(default)] bypass_cache: bool },
    Stop,

    // ---- Tabs --------------------------------------------------------
    NewTab { #[serde(default)] url: Option<String> },
    CloseTab { id: u32 },
    ActivateTab { id: u32 },
    MoveTab { id: u32, to_index: usize },

    // ---- Window ------------------------------------------------------
    WindowMinimize,
    WindowToggleMaximize,
    WindowClose,
    NewIncognitoWindow,

    /// The omnibox dropdown changed height; the host resizes the chrome
    /// overlay so the list can extend over the page without the page
    /// reflowing. `0` collapses it back to the header strip.
    ChromeOverlayHeight { px: u32 },

    // ---- Page-reported state -----------------------------------------
    /// Sent by the injected bridge when a page declares
    /// `<meta name="theme-color">`. Empty string means "no colour".
    ThemeColor { color: String },

    // ---- History ------------------------------------------------------
    QueryHistory { #[serde(default)] query: String, #[serde(default = "default_limit")] limit: u32 },
    DeleteHistoryEntry { id: i64 },
    ClearHistory { #[serde(default)] since_millis: Option<i64> },

    // ---- Downloads -----------------------------------------------------
    QueryDownloads,
    RemoveDownload { id: i64 },
    ClearDownloads,
    OpenDownload { id: i64 },
    ShowDownloadInFolder { id: i64 },
    CancelDownload { id: i64 },

    // ---- Bookmarks -----------------------------------------------------
    QueryBookmarks,
    ToggleBookmark,
    RemoveBookmark { id: i64 },
    RenameBookmark { id: i64, title: String },
    ReorderBookmarks { ids: Vec<i64> },

    // ---- Speed dials ----------------------------------------------------
    QuerySpeedDials,
    AddSpeedDial { url: String, #[serde(default)] title: String, #[serde(default)] accent: String },
    RemoveSpeedDial { id: i64 },
    UpdateSpeedDial { id: i64, title: String, #[serde(default)] accent: String },
    ReorderSpeedDials { ids: Vec<i64> },

    // ---- Password vault --------------------------------------------------
    VaultStatus,
    VaultCreate { master_password: String },
    VaultUnlock { master_password: String },
    VaultLock,
    VaultList,
    VaultReveal { id: i64 },
    VaultUpsert {
        host: String,
        username: String,
        password: String,
        #[serde(default)]
        note: String,
    },
    VaultRemove { id: i64 },
    VaultChangeMasterPassword { master_password: String },

    // ---- Content blocking -------------------------------------------------
    /// Toggle the shield for the active page's host.
    SetShieldForHost { host: String, blocking: bool },
    SetAdblockEnabled { enabled: bool },

    // ---- Privacy hub --------------------------------------------------------
    /// Polled by the new-tab page while it is visible. Sampling CPU needs two
    /// readings, so the polling interval doubles as the averaging window.
    QueryPrivacyStats,
    ResetPrivacyStats,

    // ---- Settings ----------------------------------------------------------
    QuerySettings,
    SetChameleon { enabled: bool },
    SetReduceMotion { enabled: bool },
    SetSearchTemplate { template: String },
}

fn default_limit() -> u32 {
    200
}

/// Host -> UI.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "evt", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Event<'a> {
    /// Full tab-strip state. Sent whenever a tab is added, removed, moved,
    /// retitled or activated.
    Tabs { tabs: Vec<TabView>, active: Option<u32> },

    /// Per-tab navigation state for the toolbar.
    Navigation {
        id: u32,
        url: &'a str,
        title: &'a str,
        can_go_back: bool,
        can_go_forward: bool,
        loading: bool,
        bookmarked: bool,
        secure: bool,
        /// Requests blocked on this tab since its last navigation.
        blocked: u64,
        /// `false` when the user has exempted this host.
        shield_active: bool,
    },

    /// Accent to tint the chrome with, or `None` to fall back to the theme
    /// default. Only ever sent when the chameleon setting is on.
    Accent { color: Option<String> },

    /// Incognito windows render a distinct chrome and never persist.
    WindowMode { incognito: bool, maximized: bool },

    History { items: Vec<Visit> },
    Downloads { items: Vec<Download> },
    /// Frequent, small update while a transfer is running.
    DownloadProgress { id: i64, received: i64, total: i64 },
    Bookmarks { items: Vec<Bookmark> },
    SpeedDials { items: Vec<SpeedDial>, suggested: bool },

    Vault { state: VaultState, #[serde(skip_serializing_if = "Option::is_none")] items: Option<Vec<CredentialSummary>> },
    /// Answer to an explicit reveal request.
    VaultSecret { id: i64, password: String },
    BlockerStats { stats: Stats, enabled: bool },
    Privacy { stats: PrivacyStats },
    Settings { settings: SettingsView },

    /// Transient message in the chrome; the UI decides how to show it.
    Toast { message: String, kind: ToastKind },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabView {
    pub id: u32,
    pub title: String,
    pub url: String,
    pub loading: bool,
    /// A suspended or discarded tab is drawn dimmer in the strip.
    pub asleep: bool,
    pub muted: bool,
    pub audible: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum VaultState {
    /// No vault file yet: the UI asks the user to choose a master password.
    Uninitialised,
    Locked,
    Unlocked,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

/// The subset of settings the UI can read and display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub chameleon_enabled: bool,
    pub reduce_motion: bool,
    pub accent: String,
    pub adblock_enabled: bool,
    pub search_template: String,
    pub reclaim_mode: &'static str,
}

impl Event<'_> {
    /// Serialize for `PostWebMessageAsJson`. Falls back to a null message
    /// rather than panicking: a UI update is never worth taking the browser
    /// down for.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".to_string())
    }
}

impl Command {
    /// Parse a message from a WebView. Unknown or malformed messages return
    /// `None` and are dropped by the caller.
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_from_the_shape_the_ui_sends() {
        let cmd = Command::parse(r#"{"cmd":"omniboxSubmit","text":"rust lang"}"#).unwrap();
        match cmd {
            Command::OmniboxSubmit { text, new_tab } => {
                assert_eq!(text, "rust lang");
                assert!(!new_tab, "optional flags default to false");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn optional_fields_have_defaults() {
        assert!(matches!(
            Command::parse(r#"{"cmd":"newTab"}"#),
            Some(Command::NewTab { url: None })
        ));
        match Command::parse(r#"{"cmd":"queryHistory"}"#).unwrap() {
            Command::QueryHistory { query, limit } => {
                assert!(query.is_empty());
                assert_eq!(limit, 200);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn payload_keys_are_camel_case_in_both_directions() {
        // Regression guard: enum-level `rename_all` only renames variants, so
        // without `rename_all_fields` these keys silently become snake_case.
        match Command::parse(r#"{"cmd":"omniboxSubmit","text":"x","newTab":true}"#).unwrap() {
            Command::OmniboxSubmit { new_tab, .. } => assert!(new_tab),
            other => panic!("unexpected {other:?}"),
        }
        match Command::parse(r#"{"cmd":"vaultUnlock","masterPassword":"pw"}"#).unwrap() {
            Command::VaultUnlock { master_password } => assert_eq!(master_password, "pw"),
            other => panic!("unexpected {other:?}"),
        }
        match Command::parse(r#"{"cmd":"moveTab","id":1,"toIndex":3}"#).unwrap() {
            Command::MoveTab { to_index, .. } => assert_eq!(to_index, 3),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_or_malformed_messages_are_rejected() {
        assert!(Command::parse(r#"{"cmd":"selfDestruct"}"#).is_none());
        assert!(Command::parse("not json").is_none());
        assert!(Command::parse("{}").is_none());
        // Right variant, wrong payload type.
        assert!(Command::parse(r#"{"cmd":"closeTab","id":"not a number"}"#).is_none());
        // Missing a required field.
        assert!(Command::parse(r#"{"cmd":"navigate"}"#).is_none());
    }

    #[test]
    fn payload_structs_serialize_the_keys_the_ui_reads() {
        // The internal pages index these by name (`visitedAt`, `targetPath`,
        // ...). A missing `rename_all` on any of these structs shows up as
        // `undefined` in the UI and nowhere else, so pin them here.
        let visit = Visit {
            id: 1,
            url: "https://x.test/".into(),
            title: "X".into(),
            visited_at: 42,
            visits: 3,
        };
        let json = serde_json::to_string(&visit).unwrap();
        assert!(json.contains(r#""visitedAt":42"#), "{json}");

        let download = Download {
            id: 1,
            url: "https://x.test/f".into(),
            target_path: "C:\\f".into(),
            total_bytes: 10,
            received_bytes: 5,
            state: crate::storage::DownloadState::InProgress,
            started_at: 7,
            finished_at: None,
        };
        let json = serde_json::to_string(&download).unwrap();
        for key in [r#""targetPath""#, r#""totalBytes""#, r#""receivedBytes""#, r#""startedAt""#] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        assert!(json.contains(r#""state":"inprogress""#), "{json}");

        let bookmark = Bookmark {
            id: 1,
            url: "https://x.test/".into(),
            title: "X".into(),
            added_at: 9,
            position: 0,
        };
        assert!(serde_json::to_string(&bookmark).unwrap().contains(r#""addedAt":9"#));

        let credential = CredentialSummary {
            id: 1,
            host: "x.test".into(),
            username: "alice".into(),
            note: String::new(),
            updated_at: 11,
        };
        assert!(serde_json::to_string(&credential).unwrap().contains(r#""updatedAt":11"#));
    }

    #[test]
    fn privacy_stats_serialize_the_keys_the_hub_reads() {
        // ui/newtab.js indexes these by name; a rename here is invisible until
        // the hub renders "NaN".
        let json = Event::Privacy {
            stats: crate::stats::PrivacyStats {
                blocked_total: 12_600,
                blocked_session: 42,
                bytes_saved: 34_500_000,
                time_saved_ms: 165_000,
                memory_bytes: 380_000_000,
                cpu_percent: 3.5,
                process_count: 6,
                history_entries: 900,
                blocking_enabled: true,
            },
        }
        .to_json();

        for key in [
            r#""blockedTotal":12600"#,
            r#""blockedSession":42"#,
            r#""bytesSaved":34500000"#,
            r#""timeSavedMs":165000"#,
            r#""memoryBytes":380000000"#,
            r#""cpuPercent":3.5"#,
            r#""processCount":6"#,
            r#""historyEntries":900"#,
            r#""blockingEnabled":true"#,
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
    }

    #[test]
    fn privacy_commands_parse() {
        assert!(matches!(
            Command::parse(r#"{"cmd":"queryPrivacyStats"}"#),
            Some(Command::QueryPrivacyStats)
        ));
        assert!(matches!(
            Command::parse(r#"{"cmd":"resetPrivacyStats"}"#),
            Some(Command::ResetPrivacyStats)
        ));
    }

    #[test]
    fn events_serialize_with_a_discriminator() {
        let json = Event::Accent { color: Some("#112233".into()) }.to_json();
        assert!(json.contains(r#""evt":"accent""#));
        assert!(json.contains("#112233"));

        let none = Event::Accent { color: None }.to_json();
        assert!(none.contains(r#""color":null"#));
    }

    #[test]
    fn tab_view_uses_camel_case_keys() {
        let event = Event::Tabs {
            tabs: vec![TabView {
                id: 1,
                title: "T".into(),
                url: "https://x.test/".into(),
                loading: false,
                asleep: true,
                muted: false,
                audible: false,
            }],
            active: Some(1),
        };
        let json = event.to_json();
        assert!(json.contains(r#""asleep":true"#));
        assert!(json.contains(r#""active":1"#));
    }

    #[test]
    fn vault_events_omit_the_item_list_when_absent() {
        let json = Event::Vault { state: VaultState::Locked, items: None }.to_json();
        assert!(json.contains(r#""state":"locked""#));
        assert!(!json.contains("items"), "absent list must not serialize as null");
    }

    #[test]
    fn navigation_event_carries_shield_state() {
        let json = Event::Navigation {
            id: 3,
            url: "https://x.test/",
            title: "X",
            can_go_back: true,
            can_go_forward: false,
            loading: false,
            bookmarked: false,
            secure: true,
            blocked: 7,
            shield_active: true,
        }
        .to_json();
        assert!(json.contains(r#""canGoBack":true"#));
        assert!(json.contains(r#""blocked":7"#));
        assert!(json.contains(r#""shieldActive":true"#));
    }
}
