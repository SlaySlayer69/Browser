//! Background-tab reclamation policy.
//!
//! WebView2 gives us three levers, in increasing order of savings and of cost
//! to restore:
//!
//! 1. `ICoreWebView2_19::SetMemoryUsageTargetLevel(LOW)` — asks the renderer to
//!    trim caches. Instant to undo, no visible effect.
//! 2. `ICoreWebView2_3::TrySuspend()` — freezes the document. The renderer
//!    process stays, but its timers stop and its heap is purged. `Resume()`
//!    restores it without a reload.
//! 3. Discarding — we drop the controller entirely and keep only the URL and
//!    title. The renderer goes away; re-activating reloads the page.
//!
//! Chromium's own `--process-per-site` means several tabs can share a renderer,
//! so suspending one tab of a site does not necessarily free a process. The
//! per-document freeze still purges that document's heap, which is where most
//! of the memory sits.
//!
//! The decision function is pure so the thresholds can be tested without a
//! browser attached.

use crate::config::{ReclaimMode, Settings};

/// What state a tab's renderer is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPower {
    /// Fully live.
    Normal,
    /// Live, but asked to trim its caches.
    LowMemory,
    /// Frozen via `TrySuspend`.
    Suspended,
    /// No controller at all; only URL and title are retained.
    Discarded,
}

impl TabPower {
    /// Whether the tab is drawn dimmed in the tab strip.
    pub fn is_asleep(self) -> bool {
        matches!(self, Self::Suspended | Self::Discarded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimAction {
    /// Leave the tab as it is.
    None,
    /// `SetMemoryUsageTargetLevel(LOW)`.
    LowerMemoryTarget,
    /// `TrySuspend()`.
    Suspend,
    /// Tear the controller down.
    Discard,
    /// Bring the tab fully back — `Resume()` and/or recreate the controller.
    Restore,
}

/// Inputs the policy needs about one tab.
#[derive(Debug, Clone, Copy)]
pub struct TabState {
    /// Seconds since this tab was last the active one.
    pub idle_secs: u64,
    pub is_active: bool,
    /// A tab playing audio is never suspended: freezing it would cut the sound.
    pub is_audible: bool,
    pub power: TabPower,
}

/// Decide what to do with one tab.
///
/// `minimized` suspends the notion of an "active" tab entirely: a minimized
/// window shows nothing, so its foreground tab is as reclaimable as the rest.
/// That is the single largest memory lever available while the browser sits in
/// the taskbar, and it costs the user nothing — restoring the window resumes
/// the tab before it is painted.
pub fn decide(tab: TabState, minimized: bool, settings: &Settings) -> ReclaimAction {
    // The active tab of a visible window must always be fully live.
    if tab.is_active && !minimized {
        return if tab.power == TabPower::Normal {
            ReclaimAction::None
        } else {
            ReclaimAction::Restore
        };
    }

    let Some((suspend_after, discard_after)) = settings.reclaim_thresholds() else {
        // Reclaiming is off: undo anything we did earlier, but never fight a
        // tab that is already awake.
        return if tab.power == TabPower::Normal {
            ReclaimAction::None
        } else {
            ReclaimAction::Restore
        };
    };

    // Audible background tabs get the cheap lever only.
    if tab.is_audible {
        return match tab.power {
            TabPower::Normal => ReclaimAction::LowerMemoryTarget,
            TabPower::Suspended | TabPower::Discarded => ReclaimAction::Restore,
            TabPower::LowMemory => ReclaimAction::None,
        };
    }

    if let Some(discard_after) = discard_after {
        if tab.idle_secs >= discard_after && tab.power != TabPower::Discarded {
            return ReclaimAction::Discard;
        }
    }

    if tab.idle_secs >= suspend_after {
        return match tab.power {
            TabPower::Normal | TabPower::LowMemory => ReclaimAction::Suspend,
            // Already suspended or discarded: nothing further to do.
            TabPower::Suspended | TabPower::Discarded => ReclaimAction::None,
        };
    }

    // Backgrounded but not yet past the suspend threshold.
    match tab.power {
        TabPower::Normal => ReclaimAction::LowerMemoryTarget,
        _ => ReclaimAction::None,
    }
}

/// How often the policy timer should fire, in milliseconds.
///
/// Tied to the shortest threshold so a tab is never left waiting much longer
/// than configured, but floored so an aggressive profile does not turn the
/// timer itself into the wakeup cost we are trying to avoid.
pub fn tick_interval_ms(settings: &Settings) -> u32 {
    match settings.reclaim_mode {
        ReclaimMode::Off => 0,
        _ => {
            let shortest = settings.reclaim_thresholds().map(|(s, _)| s).unwrap_or(120);
            ((shortest.max(10) / 2) as u32).clamp(5, 60) * 1000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReclaimMode;

    fn balanced() -> Settings {
        Settings { reclaim_mode: ReclaimMode::Balanced, suspend_after_secs: 120, ..Default::default() }
    }

    fn aggressive() -> Settings {
        Settings {
            reclaim_mode: ReclaimMode::Aggressive,
            suspend_after_secs: 30,
            discard_after_secs: 600,
            ..Default::default()
        }
    }

    fn tab(idle_secs: u64, power: TabPower) -> TabState {
        TabState { idle_secs, is_active: false, is_audible: false, power }
    }

    #[test]
    fn the_active_tab_is_never_reclaimed() {
        let state = TabState { is_active: true, ..tab(10_000, TabPower::Normal) };
        assert_eq!(decide(state, false, &aggressive()), ReclaimAction::None);
    }

    #[test]
    fn activating_a_sleeping_tab_restores_it() {
        for power in [TabPower::Suspended, TabPower::Discarded, TabPower::LowMemory] {
            let state = TabState { is_active: true, ..tab(0, power) };
            assert_eq!(decide(state, false, &balanced()), ReclaimAction::Restore, "{power:?}");
        }
    }

    #[test]
    fn a_fresh_background_tab_only_lowers_its_memory_target() {
        assert_eq!(decide(tab(5, TabPower::Normal), false, &balanced()), ReclaimAction::LowerMemoryTarget);
        // ...and is not asked twice.
        assert_eq!(decide(tab(5, TabPower::LowMemory), false, &balanced()), ReclaimAction::None);
    }

    #[test]
    fn passing_the_suspend_threshold_suspends() {
        assert_eq!(decide(tab(119, TabPower::LowMemory), false, &balanced()), ReclaimAction::None);
        assert_eq!(decide(tab(120, TabPower::LowMemory), false, &balanced()), ReclaimAction::Suspend);
        assert_eq!(decide(tab(500, TabPower::Suspended), false, &balanced()), ReclaimAction::None);
    }

    #[test]
    fn balanced_mode_never_discards() {
        assert_eq!(decide(tab(86_400, TabPower::Suspended), false, &balanced()), ReclaimAction::None);
    }

    #[test]
    fn aggressive_mode_discards_long_idle_tabs() {
        assert_eq!(decide(tab(30, TabPower::Normal), false, &aggressive()), ReclaimAction::Suspend);
        assert_eq!(decide(tab(600, TabPower::Suspended), false, &aggressive()), ReclaimAction::Discard);
        // Once discarded there is nothing left to reclaim.
        assert_eq!(decide(tab(9_999, TabPower::Discarded), false, &aggressive()), ReclaimAction::None);
    }

    #[test]
    fn audible_tabs_are_never_frozen() {
        let playing = TabState { is_audible: true, ..tab(10_000, TabPower::Normal) };
        assert_eq!(decide(playing, false, &aggressive()), ReclaimAction::LowerMemoryTarget);

        let already_trimmed = TabState { is_audible: true, ..tab(10_000, TabPower::LowMemory) };
        assert_eq!(decide(already_trimmed, false, &aggressive()), ReclaimAction::None);
    }

    #[test]
    fn a_tab_that_starts_playing_while_suspended_is_restored() {
        let state = TabState { is_audible: true, ..tab(10_000, TabPower::Suspended) };
        assert_eq!(decide(state, false, &aggressive()), ReclaimAction::Restore);
    }

    #[test]
    fn turning_reclaiming_off_wakes_sleeping_tabs() {
        let off = Settings { reclaim_mode: ReclaimMode::Off, ..Default::default() };
        assert_eq!(decide(tab(10_000, TabPower::Suspended), false, &off), ReclaimAction::Restore);
        assert_eq!(decide(tab(10_000, TabPower::Normal), false, &off), ReclaimAction::None);
    }

    #[test]
    fn a_minimized_window_reclaims_its_foreground_tab_too() {
        // Nothing is on screen, so the active tab has no claim on being live.
        let active = TabState { is_active: true, ..tab(300, TabPower::Normal) };
        assert_eq!(decide(active, false, &balanced()), ReclaimAction::None);
        assert_eq!(decide(active, true, &balanced()), ReclaimAction::Suspend);
    }

    #[test]
    fn restoring_the_window_wakes_the_foreground_tab() {
        let active = TabState { is_active: true, ..tab(300, TabPower::Suspended) };
        assert_eq!(decide(active, true, &balanced()), ReclaimAction::None);
        assert_eq!(decide(active, false, &balanced()), ReclaimAction::Restore);
    }

    #[test]
    fn minimizing_still_does_not_freeze_audio() {
        // Music kept playing in the foreground tab must survive minimising the
        // window — that is the most common way people listen to it.
        let playing = TabState { is_active: true, is_audible: true, ..tab(300, TabPower::Normal) };
        assert_eq!(decide(playing, true, &aggressive()), ReclaimAction::LowerMemoryTarget);

        let trimmed = TabState { is_active: true, is_audible: true, ..tab(300, TabPower::LowMemory) };
        assert_eq!(decide(trimmed, true, &aggressive()), ReclaimAction::None);
    }

    #[test]
    fn timer_interval_is_bounded() {
        assert_eq!(tick_interval_ms(&Settings { reclaim_mode: ReclaimMode::Off, ..Default::default() }), 0);
        assert_eq!(tick_interval_ms(&balanced()), 60_000);
        assert_eq!(tick_interval_ms(&aggressive()), 15_000);

        // A pathologically small threshold must not produce a 0 ms timer.
        let twitchy = Settings {
            reclaim_mode: ReclaimMode::Balanced,
            suspend_after_secs: 1,
            ..Default::default()
        };
        assert_eq!(tick_interval_ms(&twitchy), 5_000);
    }

    #[test]
    fn sleeping_states_are_reported_to_the_tab_strip() {
        assert!(TabPower::Suspended.is_asleep());
        assert!(TabPower::Discarded.is_asleep());
        assert!(!TabPower::Normal.is_asleep());
        assert!(!TabPower::LowMemory.is_asleep());
    }
}
