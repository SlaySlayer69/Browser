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
    pub host: HWND,
    /// `None` while discarded.
    pub controller: Option<ICoreWebView2Controller>,
    pub webview: Option<ICoreWebView2>,

    pub url: String,
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
}

impl Tab {
    pub fn new(id: u32, host: HWND, url: String) -> Self {
        Self {
            id,
            host,
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
            url,
        }
    }

    pub fn is_live(&self) -> bool {
        self.webview.is_some()
    }

    pub fn idle_secs(&self) -> u64 {
        self.last_active.elapsed().as_secs()
    }

    /// Label for the tab strip: the title if the page provided one, otherwise
    /// the host, otherwise a placeholder.
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.clone();
        }
        crate::util::host_of(&self.url).unwrap_or_else(|| "New Tab".to_string())
    }

    pub fn to_view(&self) -> TabView {
        TabView {
            id: self.id,
            title: crate::util::elide(&self.display_title(), 60),
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
