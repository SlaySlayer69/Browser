//! Filter-list acquisition and compilation.
//!
//! Compiling EasyList + EasyPrivacy from source takes on the order of a second
//! and allocates heavily while parsing. Doing that at every launch would be the
//! single slowest thing the browser does, so the compiled engine is cached on
//! disk and refreshed on a background thread. Startup either loads the cache
//! (a few milliseconds) or starts with an empty engine and swaps in a real one
//! when the thread reports back.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::Blocker;

/// Result of a background refresh, handed back to the UI thread.
pub struct CompiledLists {
    /// A serialized engine, ready for [`Blocker::replace_engine`].
    pub engine: Vec<u8>,
    pub rule_sources: usize,
}

/// Whether the cached engine is recent enough to skip a refresh.
pub fn cache_is_fresh(cache_path: &Path, max_age_days: u64) -> bool {
    let Ok(metadata) = fs::metadata(cache_path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    // A file dated in the future (clock skew, a restored backup) counts as
    // stale rather than as fresh forever.
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age < Duration::from_secs(max_age_days.max(1) * 86_400),
        Err(_) => false,
    }
}

/// Load the compiled engine written by a previous run.
pub fn load_cached(cache_path: &Path) -> Option<Blocker> {
    let bytes = fs::read(cache_path).ok()?;
    Blocker::from_serialized(&bytes)
}

/// Load whatever list text is already on disk, ignoring unreadable files.
pub fn load_local_lists(lists_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(lists_dir) else {
        return Vec::new();
    };
    let mut lists = Vec::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "txt") {
            if let Ok(text) = fs::read_to_string(entry.path()) {
                lists.push(text);
            }
        }
    }
    lists
}

/// A stable, filesystem-safe name for a list URL.
fn cache_file_name(url: &str) -> String {
    let digest = crate::util::fnv1a(url.as_bytes(), crate::util::FNV_OFFSET);
    format!("{digest:016x}.txt")
}

/// Paths and URLs a refresh needs, packaged so they can be moved to a thread.
#[derive(Clone)]
pub struct RefreshRequest {
    pub urls: Vec<String>,
    pub lists_dir: PathBuf,
    pub cache_path: PathBuf,
}

/// Fetch every configured list, compile them, and persist both the raw text and
/// the compiled engine.
///
/// Runs on a worker thread. Returns `None` when nothing usable could be
/// produced, in which case the caller keeps whatever engine it already has.
#[cfg(windows)]
pub fn refresh(request: &RefreshRequest) -> Option<CompiledLists> {
    let mut lists = Vec::new();

    for url in &request.urls {
        let target = request.lists_dir.join(cache_file_name(url));
        match crate::platform::http::get_text(url) {
            Ok(text) if !text.trim().is_empty() => {
                let _ = fs::write(&target, &text);
                lists.push(text);
            }
            _ => {
                // A failed fetch falls back to the copy from last time, so one
                // unreachable list never disables blocking wholesale.
                if let Ok(text) = fs::read_to_string(&target) {
                    lists.push(text);
                }
            }
        }
    }

    if lists.is_empty() {
        return None;
    }

    // Compiled here, on the worker thread: `Engine` is `!Send` under the
    // `single-thread` feature, so only the serialized bytes cross back.
    //
    // `compile` consumes the list text, and the blocker is dropped before the
    // file is written. At this point the raw lists (~8 MB), the compiled engine
    // and the serialized buffer would otherwise all be resident at once.
    // Counted before the Vec is consumed: this is how many lists were actually
    // compiled, which is not the number configured when a fetch failed and no
    // local copy existed to fall back to.
    let rule_sources = lists.len();
    let blocker = Blocker::compile(lists);
    let engine = blocker.serialize();
    drop(blocker);

    if let Some(parent) = request.cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Write-then-rename so a crash cannot leave a half-written engine that the
    // next launch would try to deserialize.
    let tmp = request.cache_path.with_extension("bin.tmp");
    if fs::write(&tmp, &engine).is_ok() {
        let _ = fs::rename(&tmp, &request.cache_path);
    }

    Some(CompiledLists { engine, rule_sources })
}

/// Where a background refresh leaves its result for the UI thread to collect.
pub struct RefreshSlot {
    inner: std::sync::Mutex<Option<CompiledLists>>,
}

impl RefreshSlot {
    pub const fn new() -> Self {
        Self { inner: std::sync::Mutex::new(None) }
    }

    pub fn put(&self, compiled: CompiledLists) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(compiled);
        }
    }

    pub fn take(&self) -> Option<CompiledLists> {
        self.inner.lock().ok().and_then(|mut slot| slot.take())
    }
}

impl Default for RefreshSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_file_names_are_stable_and_distinct() {
        let a = cache_file_name("https://easylist.to/easylist/easylist.txt");
        assert_eq!(a, cache_file_name("https://easylist.to/easylist/easylist.txt"));
        assert_ne!(a, cache_file_name("https://easylist.to/easylist/easyprivacy.txt"));
        assert!(a.ends_with(".txt"));
        // Nothing from the URL survives into the path, so a hostile list URL
        // cannot escape the directory.
        assert!(a.trim_end_matches(".txt").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_missing_cache_is_never_fresh() {
        assert!(!cache_is_fresh(Path::new("/nonexistent/filters.bin"), 7));
    }

    #[test]
    fn a_just_written_cache_is_fresh() {
        let mut path = std::env::temp_dir();
        path.push(format!("cleandark-freshness-{}.bin", std::process::id()));
        fs::write(&path, b"x").unwrap();
        assert!(cache_is_fresh(&path, 7));
        // A zero-day policy still means "at least one day", not "always stale".
        assert!(cache_is_fresh(&path, 0));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn local_lists_are_read_from_a_directory() {
        let dir = std::env::temp_dir().join(format!("cleandark-lists-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("a.txt"), "||ads.example.com^").unwrap();
        fs::write(dir.join("ignored.bin"), "not a list").unwrap();

        let lists = load_local_lists(&dir);
        assert_eq!(lists.len(), 1);
        assert!(lists[0].contains("ads.example.com"));

        // A directory that does not exist yields no lists rather than panicking.
        assert!(load_local_lists(Path::new("/nonexistent")).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_does_not_produce_a_blocker() {
        let mut path = std::env::temp_dir();
        path.push(format!("cleandark-corrupt-{}.bin", std::process::id()));
        fs::write(&path, b"definitely not a serialized engine").unwrap();
        assert!(load_cached(&path).is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn the_refresh_slot_hands_over_exactly_once() {
        let slot = RefreshSlot::new();
        assert!(slot.take().is_none());
        slot.put(CompiledLists { engine: vec![1, 2, 3], rule_sources: 2 });
        assert_eq!(slot.take().unwrap().engine, vec![1, 2, 3]);
        assert!(slot.take().is_none(), "a result is consumed once");
    }
}
