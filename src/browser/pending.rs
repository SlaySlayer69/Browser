//! Coalescing of chrome updates.
//!
//! # The problem
//!
//! A single page load fires a burst of WebView2 events — `NavigationStarting`,
//! `SourceChanged`, several `HistoryChanged`, one or more
//! `DocumentTitleChanged`, then `NavigationCompleted` — and each of them used
//! to push a message to the chrome WebView immediately. Every push is a JSON
//! serialization on our side, a cross-process message, a JavaScript wakeup in
//! the chrome renderer and a DOM reconcile. Six to ten of those per page load,
//! and the last one is the only state anybody sees.
//!
//! # The fix
//!
//! Event handlers mark *what* changed instead of pushing. The first mark posts
//! a single application message to our own window; because `PostMessage` queues
//! behind everything already pending, the whole burst has landed by the time
//! that message is dispatched, and one flush sends the final state.
//!
//! This is the invalidate-then-paint pattern every GUI toolkit uses, and it has
//! the property that matters most here: **no timer**. An idle browser posts
//! nothing and wakes up for nothing.

/// What the chrome still needs to be told about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingUi {
    /// The tab strip: order, titles, loading and sleeping state.
    pub tabs: bool,
    /// Toolbar state for this tab. Only ever the active one — the chrome
    /// discards navigation events for background tabs, so recording them would
    /// be work that is thrown away.
    pub navigation: Option<u32>,
    /// The downloads list.
    pub downloads: bool,
}

impl PendingUi {
    pub fn is_empty(&self) -> bool {
        !self.tabs && self.navigation.is_none() && !self.downloads
    }

    pub fn mark_tabs(&mut self) {
        self.tabs = true;
    }

    /// Record that the active tab's toolbar state changed.
    pub fn mark_navigation(&mut self, tab_id: u32) {
        self.navigation = Some(tab_id);
    }

    pub fn mark_downloads(&mut self) {
        self.downloads = true;
    }

    /// Take everything pending, leaving the set empty.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_set_has_nothing_to_send() {
        assert!(PendingUi::default().is_empty());
    }

    #[test]
    fn marks_accumulate_rather_than_replace() {
        let mut pending = PendingUi::default();
        pending.mark_tabs();
        pending.mark_downloads();
        pending.mark_navigation(7);

        assert!(pending.tabs);
        assert!(pending.downloads);
        assert_eq!(pending.navigation, Some(7));
        assert!(!pending.is_empty());
    }

    #[test]
    fn a_burst_of_identical_marks_collapses() {
        // This is the whole point: ten title changes during one page load must
        // cost exactly one flush.
        let mut pending = PendingUi::default();
        for _ in 0..10 {
            pending.mark_tabs();
        }
        let flushed = pending.take();
        assert!(flushed.tabs);
        assert!(pending.is_empty(), "take must leave nothing behind");
    }

    #[test]
    fn a_later_navigation_supersedes_an_earlier_one() {
        // Only the newest state is worth sending; the chrome renders state, not
        // a history of it.
        let mut pending = PendingUi::default();
        pending.mark_navigation(1);
        pending.mark_navigation(2);
        assert_eq!(pending.navigation, Some(2));
    }

    /// Models the event burst of one page load and counts how many times the
    /// chrome would actually be written to.
    ///
    /// Each flush is a JSON serialization, a cross-process message, a JS wakeup
    /// and a DOM reconcile, so the ratio here is the whole point of the module.
    #[test]
    fn a_page_load_collapses_to_a_single_flush() {
        let mut pending = PendingUi::default();
        let mut flushes = 0;

        // The sequence WebView2 delivers for a typical navigation. Every one of
        // these used to push immediately.
        let burst: &[fn(&mut PendingUi)] = &[
            |p| p.mark_tabs(),           // NavigationStarting
            |p| p.mark_navigation(1),    // NavigationStarting
            |p| p.mark_navigation(1),    // SourceChanged
            |p| p.mark_navigation(1),    // HistoryChanged
            |p| p.mark_navigation(1),    // HistoryChanged (redirect)
            |p| p.mark_tabs(),           // DocumentTitleChanged
            |p| p.mark_tabs(),           // DocumentTitleChanged (SPA retitles)
            |p| p.mark_tabs(),           // NavigationCompleted
            |p| p.mark_navigation(1),    // NavigationCompleted
        ];
        for event in burst {
            event(&mut pending);
        }

        // The posted message is dispatched once, after the burst has landed.
        if !pending.is_empty() {
            pending.take();
            flushes += 1;
        }

        assert_eq!(flushes, 1, "{} events must cost one flush", burst.len());
        assert!(pending.is_empty());
    }

    #[test]
    fn taking_twice_yields_nothing_the_second_time() {
        let mut pending = PendingUi::default();
        pending.mark_tabs();
        assert!(!pending.take().is_empty());
        assert!(pending.take().is_empty());
    }
}
