//! Network-level content blocking.
//!
//! # Why this needs a cache
//!
//! WebView2 delivers `WebResourceRequested` on the UI thread, synchronously
//! blocking the requesting renderer until the handler returns. Every filter
//! registered widens the set of requests that make that round trip. A busy
//! page issues hundreds of subresource requests, so the per-request budget
//! here is *microseconds*, not milliseconds.
//!
//! Two things keep us inside that budget:
//!
//! * We register filters only for the resource contexts that actually carry
//!   ads and trackers (see [`crate::engine::interceptor`]). Top-level document
//!   navigations are handled in `NavigationStarting` instead, where the cost is
//!   paid once per page rather than once per subresource.
//! * Every decision is memoised in a fixed-size direct-mapped cache. Pages
//!   re-request the same origins constantly, so the hit rate is high and a hit
//!   costs one hash plus one array probe — no allocation, no locking.
//!
//! The cache is a fixed array rather than an LRU map on purpose: bounded,
//! allocation-free memory is worth more here than a perfect eviction policy.

pub mod lists;

use adblock::lists::{FilterSet, ParseOptions, RuleTypes};
use adblock::request::Request;
use adblock::Engine;

use crate::util::{fnv1a, FNV_OFFSET};

/// Number of slots in the decision cache. Power of two so the index is a mask.
/// 8192 slots x 16 bytes = 128 KiB, fixed for the lifetime of the process.
const CACHE_SLOTS: usize = 8192;
const CACHE_MASK: u64 = (CACHE_SLOTS as u64) - 1;

/// A memoised verdict. `key == 0` marks an empty slot, so the one key that
/// hashes to zero is simply never cached.
#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    blocked: bool,
}

struct DecisionCache {
    slots: Box<[Slot]>,
    hits: u64,
    misses: u64,
}

impl DecisionCache {
    fn new() -> Self {
        Self { slots: vec![Slot::default(); CACHE_SLOTS].into_boxed_slice(), hits: 0, misses: 0 }
    }

    #[inline]
    fn get(&mut self, key: u64) -> Option<bool> {
        if key == 0 {
            return None;
        }
        let slot = &self.slots[(key & CACHE_MASK) as usize];
        if slot.key == key {
            self.hits += 1;
            Some(slot.blocked)
        } else {
            self.misses += 1;
            None
        }
    }

    #[inline]
    fn put(&mut self, key: u64, blocked: bool) {
        if key == 0 {
            return;
        }
        self.slots[(key & CACHE_MASK) as usize] = Slot { key, blocked };
    }

    fn clear(&mut self) {
        self.slots.fill(Slot::default());
        self.hits = 0;
        self.misses = 0;
    }
}

/// Counters surfaced in the UI (per-tab shield badge) and in `about:` pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub checked: u64,
    pub blocked: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

pub struct Blocker {
    engine: Engine,
    cache: DecisionCache,
    enabled: bool,
    checked: u64,
    blocked: u64,
    /// Hosts the user has exempted, e.g. via the shield menu. Small and linear
    /// on purpose: a handful of entries, checked only on cache misses.
    allowlist: Vec<String>,
}

impl Blocker {
    /// An engine with no rules. Used before the filter lists have been
    /// compiled, so the first paint never waits on list parsing.
    pub fn empty() -> Self {
        Self {
            engine: Engine::new_with_filter_set(FilterSet::new(false)),
            cache: DecisionCache::new(),
            enabled: true,
            checked: 0,
            blocked: 0,
            allowlist: Vec::new(),
        }
    }

    /// Compile an engine from raw filter-list text.
    ///
    /// Takes the lists by value: they are several megabytes each, and
    /// `add_filter_list` consumes a `String` anyway, so borrowing here would
    /// only force a full copy of every list.
    ///
    /// [`RuleTypes::NetworkOnly`] drops cosmetic rules at parse time. We block
    /// before the request leaves the process and never inject element-hiding
    /// CSS, so keeping them would cost tens of megabytes of resident memory for
    /// rules we would never consult.
    pub fn compile(lists: Vec<String>) -> Self {
        let mut set = FilterSet::new(false);
        let opts = ParseOptions { rule_types: RuleTypes::NetworkOnly, ..ParseOptions::default() };
        for list in lists {
            set.add_filter_list(list, opts);
        }
        Self { engine: Engine::new_with_filter_set(set), ..Self::empty() }
    }

    /// Restore a previously compiled engine. Roughly two orders of magnitude
    /// faster than re-parsing the source lists at startup.
    pub fn from_serialized(bytes: &[u8]) -> Option<Self> {
        let mut engine = Engine::new_with_filter_set(FilterSet::new(false));
        engine.deserialize(bytes).ok()?;
        Some(Self { engine, ..Self::empty() })
    }

    pub fn serialize(&self) -> Vec<u8> {
        self.engine.serialize()
    }

    /// Swap in a freshly compiled engine, e.g. after a background list update.
    ///
    /// Takes serialized bytes rather than an [`Engine`] on purpose: with the
    /// `single-thread` feature an `Engine` is `!Send`, so the updater thread
    /// compiles and serializes in place and only the byte buffer crosses the
    /// thread boundary.
    pub fn replace_engine(&mut self, serialized: &[u8]) -> bool {
        let mut engine = Engine::new_with_filter_set(FilterSet::new(false));
        if engine.deserialize(serialized).is_err() {
            return false;
        }
        self.engine = engine;
        self.cache.clear();
        true
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.cache.clear();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn allow_host(&mut self, host: &str) {
        let host = host.to_ascii_lowercase();
        if !self.allowlist.contains(&host) {
            self.allowlist.push(host);
            self.cache.clear();
        }
    }

    pub fn remove_host_exception(&mut self, host: &str) {
        let host = host.to_ascii_lowercase();
        let before = self.allowlist.len();
        self.allowlist.retain(|h| h != &host);
        if self.allowlist.len() != before {
            self.cache.clear();
        }
    }

    /// Case-insensitive without allocating: this runs on every cache miss.
    pub fn is_host_allowed(&self, host: &str) -> bool {
        self.allowlist.iter().any(|h| h.eq_ignore_ascii_case(host))
    }

    pub fn stats(&self) -> Stats {
        Stats {
            checked: self.checked,
            blocked: self.blocked,
            cache_hits: self.cache.hits,
            cache_misses: self.cache.misses,
        }
    }

    /// The hot path. `true` means the request must not be issued.
    ///
    /// `resource_type` uses adblock's vocabulary ("script", "image", "xhr",
    /// "sub_frame", ...); [`crate::engine::resource`] maps WebView2's resource
    /// contexts onto it.
    ///
    /// `source_host` is passed in rather than derived from `source_url`: the
    /// caller already knows the host of the page it is loading, and parsing the
    /// URL here would put a full URL parse plus a `String` allocation in front
    /// of every cache lookup — including the hits this cache exists to make
    /// cheap. `source_url` is still needed by the matcher itself, which decides
    /// first- versus third-party from the full URL.
    pub fn should_block(
        &mut self,
        url: &str,
        source_url: &str,
        source_host: &str,
        resource_type: &str,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        self.checked += 1;

        let key = cache_key(url, source_host, resource_type);
        if let Some(hit) = self.cache.get(key) {
            if hit {
                self.blocked += 1;
            }
            return hit;
        }

        let verdict = self.evaluate(url, source_url, source_host, resource_type);
        self.cache.put(key, verdict);
        if verdict {
            self.blocked += 1;
        }
        verdict
    }

    fn evaluate(
        &self,
        url: &str,
        source_url: &str,
        source_host: &str,
        resource_type: &str,
    ) -> bool {
        // A page-level exception disables blocking for everything that page
        // loads, matching what users expect from a per-site shield toggle.
        if !source_host.is_empty() && self.is_host_allowed(source_host) {
            return false;
        }

        // An unparseable URL is not something we can reason about; letting it
        // through keeps a filter bug from breaking page loads.
        let Ok(request) = Request::new(url, source_url, resource_type, "GET") else {
            return false;
        };

        self.engine.check_network_request(&request).should_block()
    }
}

#[inline]
fn cache_key(url: &str, source_host: &str, resource_type: &str) -> u64 {
    // Only the source *host* is folded in: the verdict depends on first-party
    // vs third-party and on the page's domain, never on its path. That makes
    // every subresource on a page share cache entries, and keeps this function
    // allocation-free.
    let mut key = fnv1a(url.as_bytes(), FNV_OFFSET);
    key = fnv1a(source_host.as_bytes(), key);
    key = fnv1a(resource_type.as_bytes(), key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = "||ads.example.com^\n||tracker.test/pixel.gif\n@@||ads.example.com/allowed^";

    fn blocker() -> Blocker {
        Blocker::compile(vec![RULES.to_string()])
    }

    /// The app keeps the page host on the tab; tests derive it once here so the
    /// call sites stay readable.
    fn check(b: &mut Blocker, url: &str, source: &str, kind: &str) -> bool {
        let host = crate::util::host_of(source).unwrap_or_default();
        b.should_block(url, source, &host, kind)
    }

    #[test]
    fn blocks_matching_third_party_requests() {
        let mut b = blocker();
        assert!(check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script"));
    }

    #[test]
    fn honours_exception_rules() {
        let mut b = blocker();
        assert!(!check(&mut b, "https://ads.example.com/allowed/x.js", "https://news.test/", "script"));
    }

    #[test]
    fn leaves_unrelated_requests_alone() {
        let mut b = blocker();
        assert!(!check(&mut b, "https://news.test/app.js", "https://news.test/", "script"));
    }

    #[test]
    fn disabled_blocker_blocks_nothing() {
        let mut b = blocker();
        b.set_enabled(false);
        assert!(!check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script"));
    }

    #[test]
    fn per_site_allowlist_disables_blocking_for_that_page() {
        let mut b = blocker();
        b.allow_host("news.test");
        assert!(!check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script"));
        // ...but only for that page.
        assert!(check(&mut b, "https://ads.example.com/banner.js", "https://other.test/", "script"));

        b.remove_host_exception("news.test");
        assert!(check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script"));
    }

    #[test]
    fn repeated_requests_hit_the_cache() {
        let mut b = blocker();
        for _ in 0..10 {
            check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script");
        }
        let stats = b.stats();
        assert_eq!(stats.checked, 10);
        assert_eq!(stats.blocked, 10);
        assert_eq!(stats.cache_misses, 1, "only the first lookup should reach the engine");
        assert_eq!(stats.cache_hits, 9);
    }

    #[test]
    fn cache_is_invalidated_when_policy_changes() {
        let mut b = blocker();
        assert!(check(&mut b, "https://ads.example.com/a.js", "https://news.test/", "script"));
        b.allow_host("news.test");
        // Stale cache would still say "block" here.
        assert!(!check(&mut b, "https://ads.example.com/a.js", "https://news.test/", "script"));
    }

    #[test]
    fn the_hot_path_does_not_parse_the_source_url() {
        // Regression guard for the reason `source_host` is a parameter: an
        // empty host must not be treated as "allowlisted", and must not make
        // the blocker fall back to parsing anything.
        let mut b = blocker();
        assert!(b.should_block("https://ads.example.com/a.js", "", "", "script"));

        // An allowlist entry only applies when the caller supplies the host.
        b.allow_host("news.test");
        assert!(b.should_block("https://ads.example.com/a.js", "https://news.test/", "", "script"));
        assert!(!b.should_block(
            "https://ads.example.com/a.js",
            "https://news.test/",
            "news.test",
            "script"
        ));
    }

    #[test]
    fn allowlist_matching_is_case_insensitive_without_allocating() {
        let mut b = blocker();
        b.allow_host("News.TEST");
        assert!(b.is_host_allowed("news.test"));
        assert!(b.is_host_allowed("NEWS.test"));
        assert!(!b.is_host_allowed("other.test"));
    }

    #[test]
    fn cache_key_separates_resource_types_and_first_party_context() {
        let a = cache_key("https://x.test/a", "p.test", "script");
        assert_ne!(a, cache_key("https://x.test/a", "p.test", "image"));
        assert_ne!(a, cache_key("https://x.test/a", "q.test", "script"));
        // Same host -> same key, so every subresource on a page shares entries
        // regardless of the page's path.
        assert_eq!(a, cache_key("https://x.test/a", "p.test", "script"));
    }

    #[test]
    fn malformed_urls_are_allowed_through() {
        let mut b = blocker();
        assert!(!check(&mut b, "", "https://news.test/", "script"));
        assert!(!check(&mut b, "not a url", "https://news.test/", "script"));
    }

    #[test]
    fn empty_engine_blocks_nothing() {
        let mut b = Blocker::empty();
        assert!(!check(&mut b, "https://ads.example.com/banner.js", "https://news.test/", "script"));
    }

    #[test]
    fn serialized_engine_round_trips() {
        let original = blocker();
        let bytes = original.serialize();
        let mut restored = Blocker::from_serialized(&bytes).expect("deserialize");
        let _ = &mut restored;
        assert!(check(&mut restored, "https://ads.example.com/banner.js", "https://news.test/", "script"));
    }
}
