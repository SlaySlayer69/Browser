//! Privacy and resource statistics shown on the new-tab page.
//!
//! # On "bandwidth saved" and "time saved"
//!
//! These are **estimates, and cannot be anything else**. We block a request
//! before it is issued, so the response never exists and its real size is never
//! known. Every browser that shows this number — Brave, Samsung Internet, the
//! rest — is multiplying a request count by an assumed average.
//!
//! What this module does about that: the assumptions live here as named
//! constants with the reasoning attached, the per-resource averages differ by
//! type instead of one blanket number, and the UI labels the figures as
//! estimated. The blocked-request count next to them is exact.

use serde::Serialize;

/// Rough average transfer size of a blocked resource, by filter type.
///
/// These are order-of-magnitude figures for *ad and tracker* resources
/// specifically, which skew smaller than the web average for scripts (tag
/// managers, beacons) and much smaller for "images" (tracking pixels are a
/// large share of blocked image requests).
const fn average_bytes(filter_type: &str) -> u64 {
    // `match` on &str is not const-friendly, so this is a small if-chain.
    // Kept as one function so the table is readable in one place.
    if str_eq(filter_type, "script") {
        45_000
    } else if str_eq(filter_type, "sub_frame") {
        60_000
    } else if str_eq(filter_type, "media") {
        200_000
    } else if str_eq(filter_type, "image") {
        // Dominated by 1x1 tracking pixels, pulled up by real ad creatives.
        18_000
    } else if str_eq(filter_type, "xhr") {
        8_000
    } else if str_eq(filter_type, "websocket") {
        2_000
    } else if str_eq(filter_type, "ping") || str_eq(filter_type, "csp_report") {
        // Beacons: a few hundred bytes of payload plus headers.
        800
    } else {
        10_000
    }
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Fixed cost we assume every blocked request would have carried: connection
/// setup amortised over keep-alive, plus parse and execute for the script case.
/// Deliberately conservative — a single tracker script can occupy the main
/// thread far longer than this.
const PER_REQUEST_OVERHEAD_MS: u64 = 12;

/// Assumed downlink, 2500 bytes/ms ≈ 20 Mbit/s. Used only to turn estimated
/// bytes into estimated milliseconds.
const ASSUMED_BYTES_PER_MS: u64 = 2_500;

/// Estimated bytes not transferred for one blocked request.
pub fn estimated_bytes(filter_type: &str) -> u64 {
    average_bytes(filter_type)
}

/// Estimated time not spent, from a request count and their estimated bytes.
pub fn estimated_time_saved_ms(requests: u64, bytes: u64) -> u64 {
    requests
        .saturating_mul(PER_REQUEST_OVERHEAD_MS)
        .saturating_add(bytes / ASSUMED_BYTES_PER_MS)
}

/// Everything the privacy hub renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyStats {
    /// Exact: requests blocked over the profile's lifetime.
    pub blocked_total: u64,
    /// Exact: requests blocked since this window opened.
    pub blocked_session: u64,
    /// Estimated.
    pub bytes_saved: u64,
    /// Estimated.
    pub time_saved_ms: u64,

    /// Measured: summed private commit of every browser process.
    pub memory_bytes: u64,
    /// Measured: share of one core-second, averaged over the sampling window.
    pub cpu_percent: f32,
    /// How many processes that covers, including our own.
    pub process_count: u32,

    /// Rows currently in the history table.
    pub history_entries: u64,
    /// Whether blocking is on at all; the hub greys out when it is not.
    pub blocking_enabled: bool,
}

/// Running totals kept in memory between database flushes.
///
/// Incrementing a SQLite row on every blocked request would put a write on the
/// hot path of a page load. Instead the counters accumulate here and are
/// flushed when the new-tab page asks for them, when enough have piled up, and
/// on shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct PendingCounters {
    pub requests: u64,
    pub bytes: u64,
}

impl PendingCounters {
    /// Record one blocked request.
    pub fn record(&mut self, filter_type: &str) {
        // Saturating throughout: these counters are read by the UI thread and
        // a wrap (or a debug-build panic) is never worth a statistic.
        self.requests = self.requests.saturating_add(1);
        self.bytes = self.bytes.saturating_add(estimated_bytes(filter_type));
    }

    /// Whether enough has accumulated that a crash would lose a visible amount.
    pub fn should_flush(&self) -> bool {
        self.requests >= 200
    }

    pub fn is_empty(&self) -> bool {
        self.requests == 0
    }

    /// Take the pending totals, resetting them.
    pub fn take(&mut self) -> (u64, u64) {
        let taken = (self.requests, self.bytes);
        *self = Self::default();
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_types_have_distinct_estimates() {
        assert!(estimated_bytes("media") > estimated_bytes("script"));
        assert!(estimated_bytes("script") > estimated_bytes("image"));
        assert!(estimated_bytes("image") > estimated_bytes("xhr"));
        assert!(estimated_bytes("xhr") > estimated_bytes("ping"));
    }

    #[test]
    fn an_unknown_type_still_gets_a_sane_estimate() {
        let fallback = estimated_bytes("something-new");
        assert!(fallback > 0);
        assert!(fallback < estimated_bytes("media"));
    }

    #[test]
    fn time_estimate_grows_with_both_inputs() {
        let base = estimated_time_saved_ms(100, 1_000_000);
        assert!(estimated_time_saved_ms(200, 1_000_000) > base);
        assert!(estimated_time_saved_ms(100, 2_000_000) > base);
        assert_eq!(estimated_time_saved_ms(0, 0), 0);
    }

    #[test]
    fn time_estimate_stays_conservative() {
        // 12.6k blocked requests at ~30 MB should land in single-digit minutes,
        // not the half-hour figures some browsers advertise. If someone tunes
        // the constants upward, this is the guard rail.
        let ms = estimated_time_saved_ms(12_600, 30_000_000);
        let minutes = ms / 60_000;
        assert!((2..=10).contains(&minutes), "got {minutes} minutes");
    }

    #[test]
    fn time_estimate_does_not_overflow_on_absurd_counts() {
        // Saturating, so a corrupted counter cannot panic the UI thread.
        let _ = estimated_time_saved_ms(u64::MAX, u64::MAX);
    }

    #[test]
    fn pending_counters_accumulate_and_reset() {
        let mut pending = PendingCounters::default();
        assert!(pending.is_empty());

        pending.record("script");
        pending.record("image");
        assert_eq!(pending.requests, 2);
        assert_eq!(pending.bytes, estimated_bytes("script") + estimated_bytes("image"));

        let (requests, bytes) = pending.take();
        assert_eq!(requests, 2);
        assert!(bytes > 0);
        assert!(pending.is_empty(), "take must reset");
        assert_eq!(pending.take(), (0, 0));
    }

    #[test]
    fn flush_threshold_is_not_reached_immediately() {
        let mut pending = PendingCounters::default();
        for _ in 0..199 {
            pending.record("script");
        }
        assert!(!pending.should_flush());
        pending.record("script");
        assert!(pending.should_flush());
    }
}
