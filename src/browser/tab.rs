//! A single tab.
//!
//! A tab outlives its WebView: discarding tears the controller down but keeps
//! the URL and title so the strip still shows something meaningful and
//! activating it can reload.

use std::time::Instant;

use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2Controller};
use windows::Win32::Foundation::HWND;

use crate::browser::reclaim::TabPower;
use crate::ipc::TabView;

pub struct Tab {
    pub id: u32,
    /// Child window the controller draws into. Outlives discarding.
    pub host_window: HWND,
    /// `None` while discarded.
    pub controller: Option<ICoreWebView2Controller>,
    pub webview: Option<ICoreWebView2>,

    pub url: String,
    /// Host of `url`, kept alongside it rather than derived on demand.
    ///
    /// The blocker needs it for every intercepted request; parsing the URL
    /// there would put a full parse and a `String` allocation in front of every
    /// cache lookup. Updated only when the URL changes — once per navigation.
    pub host: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,

    pub power: TabPower,
    pub last_active: Instant,
    pub audible: bool,
    pub muted: bool,

    /// URL of the navigation the main frame is currently committing, used to
    /// tell a top-level document request apart from an iframe.
    pub pending_main_frame: Option<String>,
    /// Requests blocked since the last main-frame navigation.
    pub blocked: u64,
    /// Last `<meta name="theme-color">` the page declared.
    pub theme_color: Option<String>,
    /// Cached answer to "is this URL bookmarked". Refreshed when the URL
    /// changes or the user toggles the star, so the toolbar can be updated
    /// without a database round trip per navigation event.
    pub bookmarked: bool,
}

impl Tab {
    pub fn new(id: u32, host_window: HWND, url: String) -> Self {
        Self {
            id,
            host: crate::util::host_of(&url).unwrap_or_default(),
            host_window,
            controller: None,
            webview: None,
            title: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            power: TabPower::Normal,
            last_active: Instant::now(),
            audible: false,
            muted: false,
            pending_main_frame: None,
            blocked: 0,
            theme_color: None,
            bookmarked: false,
            url,
        }
    }

    /// Change the URL and everything derived from it.
    ///
    /// The single place that keeps `host` in sync; assigning `url` directly
    /// would silently leave the blocker matching against the previous page.
    pub fn set_url(&mut self, url: String) {
        if self.url == url {
            return;
        }
        self.host = crate::util::host_of(&url).unwrap_or_default();
        self.url = url;
    }

    pub fn is_live(&self) -> bool {
        self.webview.is_some()
    }

    pub fn idle_secs(&self) -> u64 {
        self.last_active.elapsed().as_secs()
    }

    /// Label for the tab strip: the title if the page provided one, otherwise
    /// the host, otherwise a placeholder.
    ///
    /// Borrows in both of the common cases; only the empty-URL fallback owns.
    pub fn display_title(&self) -> std::borrow::Cow<'_, str> {
        if !self.title.trim().is_empty() {
            return std::borrow::Cow::Borrowed(&self.title);
        }
        if !self.host.is_empty() {
            return std::borrow::Cow::Borrowed(&self.host);
        }
        std::borrow::Cow::Borrowed("New Tab")
    }

    pub fn to_view(&self) -> TabView {
        TabView {
            id: self.id,
            // One allocation for the wire format instead of three: the title
            // is borrowed, elided in place when short enough, and only then
            // copied into the event.
            title: crate::util::elide(&self.display_title(), 60).into_owned(),
            url: self.url.clone(),
            loading: self.loading,
            asleep: self.power.is_asleep(),
            muted: self.muted,
            audible: self.audible,
        }
    }

    /// Drop the WebView but keep the tab. Frees the renderer's memory; the page
    /// reloads when the tab is next activated.
    pub fn discard(&mut self) {
        if let Some(controller) = self.controller.take() {
            unsafe {
                let _ = controller.Close();
            }
        }
        self.webview = None;
        self.power = TabPower::Discarded;
        self.loading = false;
        self.can_go_back = false;
        self.can_go_forward = false;
        self.pending_main_frame = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(url: &str, title: &str) -> Tab {
        let mut t = Tab::new(1, HWND(std::ptr::null_mut()), url.to_string());
        t.title = title.to_string();
        t
    }

    #[test]
    fn the_host_is_derived_once_and_kept_in_sync() {
        let mut t = tab("https://Example.COM/a", "");
        assert_eq!(t.host, "example.com");

        t.set_url("https://other.test/b".to_string());
        assert_eq!(t.host, "other.test");
        assert_eq!(t.url, "https://other.test/b");

        // A URL that cannot be parsed leaves an empty host rather than a stale
        // one from the previous page.
        t.set_url("about:blank".to_string());
        assert_eq!(t.host, "");
    }

    #[test]
    fn display_title_falls_back_to_the_host() {
        assert_eq!(tab("https://example.com/a", "Real Title").display_title(), "Real Title");
        assert_eq!(tab("https://example.com/a", "").display_title(), "example.com");
        assert_eq!(tab("https://example.com/a", "   ").display_title(), "example.com");
        assert_eq!(tab("", "").display_title(), "New Tab");
    }

    #[test]
    fn a_new_tab_is_not_live_until_a_webview_is_attached() {
        assert!(!tab("https://x.test/", "").is_live());
    }

    #[test]
    fn the_view_reports_sleeping_state_and_elides_long_titles() {
        let mut t = tab("https://x.test/", &"y".repeat(200));
        t.power = TabPower::Suspended;
        let view = t.to_view();
        assert!(view.asleep);
        assert_eq!(view.title.chars().count(), 60);
    }
}
