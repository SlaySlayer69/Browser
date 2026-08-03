//! Browsing history.
//!
//! Deduplicated by URL: a revisit updates the timestamp and increments a
//! counter instead of appending a row. A month of heavy browsing stays in the
//! low thousands of rows, which keeps both the file and the search cheap.

use rusqlite::params;
use serde::Serialize;

use super::Storage;
use crate::util::{host_of, now_millis};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Visit {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub visited_at: i64,
    pub visits: i64,
}

impl Storage {
    /// Record a visit. Returns the row id.
    pub fn record_visit(&self, url: &str, title: &str) -> rusqlite::Result<i64> {
        // `about:` and our own internal pages are navigation targets, not
        // things a user wants to find again in their history.
        if url.is_empty() || url.starts_with("about:") || url.starts_with("cleandark://") {
            return Ok(0);
        }

        let host = host_of(url).unwrap_or_default();
        self.conn().execute(
            "INSERT INTO history (url, title, host, visited_at, visits)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(url) DO UPDATE SET
                 visited_at = excluded.visited_at,
                 visits     = history.visits + 1,
                 -- A later navigation may arrive before the title is known;
                 -- never overwrite a real title with an empty one.
                 title      = CASE WHEN excluded.title <> '' THEN excluded.title
                                   ELSE history.title END",
            params![url, title, host, now_millis()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Update the title of the most recent visit to `url`, if any.
    pub fn update_title(&self, url: &str, title: &str) -> rusqlite::Result<()> {
        if title.is_empty() {
            return Ok(());
        }
        self.conn()
            .execute("UPDATE history SET title = ?2 WHERE url = ?1", params![url, title])?;
        Ok(())
    }

    /// Most recent visits, newest first.
    pub fn recent_history(&self, limit: u32) -> rusqlite::Result<Vec<Visit>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, url, title, visited_at, visits
             FROM history ORDER BY visited_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], map_visit)?;
        rows.collect()
    }

    /// Substring search over title and URL, newest first.
    ///
    /// A `LIKE '%term%'` scan cannot use the index, but with history bounded to
    /// a few thousand rows it completes well inside a frame. Swapping in FTS5
    /// would cost a second copy of every title on disk for no perceptible gain
    /// at this scale.
    pub fn search_history(&self, query: &str, limit: u32) -> rusqlite::Result<Vec<Visit>> {
        if query.trim().is_empty() {
            return self.recent_history(limit);
        }
        let pattern = format!("%{}%", escape_like(query.trim()));
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, url, title, visited_at, visits
             FROM history
             WHERE title LIKE ?1 ESCAPE '\\' OR url LIKE ?1 ESCAPE '\\'
             ORDER BY visits DESC, visited_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit], map_visit)?;
        rows.collect()
    }

    pub fn delete_visit(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM history WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn clear_history(&self) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM history", [])?;
        Ok(())
    }

    /// Drop everything visited in the last `millis` milliseconds.
    pub fn clear_history_since(&self, millis: i64) -> rusqlite::Result<()> {
        let cutoff = now_millis() - millis;
        self.conn().execute("DELETE FROM history WHERE visited_at >= ?1", [cutoff])?;
        Ok(())
    }
}

fn map_visit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Visit> {
    Ok(Visit {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        visited_at: row.get(3)?,
        visits: row.get(4)?,
    })
}

/// Escape LIKE wildcards so a user searching for "100%" does not match
/// everything.
fn escape_like(input: &str) -> String {
    input.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::in_memory().unwrap()
    }

    #[test]
    fn revisits_deduplicate_and_count() {
        let s = storage();
        s.record_visit("https://example.com/", "Example").unwrap();
        s.record_visit("https://example.com/", "Example").unwrap();
        s.record_visit("https://example.com/", "Example").unwrap();

        let history = s.recent_history(10).unwrap();
        assert_eq!(history.len(), 1, "one row per URL");
        assert_eq!(history[0].visits, 3);
    }

    #[test]
    fn a_later_empty_title_does_not_erase_a_known_one() {
        let s = storage();
        s.record_visit("https://example.com/", "Real Title").unwrap();
        s.record_visit("https://example.com/", "").unwrap();
        assert_eq!(s.recent_history(1).unwrap()[0].title, "Real Title");
    }

    #[test]
    fn internal_pages_are_not_recorded() {
        let s = storage();
        s.record_visit("about:blank", "").unwrap();
        s.record_visit("cleandark://newtab", "New Tab").unwrap();
        s.record_visit("", "").unwrap();
        assert!(s.recent_history(10).unwrap().is_empty());
    }

    #[test]
    fn search_matches_title_and_url() {
        let s = storage();
        s.record_visit("https://rust-lang.org/", "Rust Language").unwrap();
        s.record_visit("https://example.com/", "Something Else").unwrap();

        assert_eq!(s.search_history("rust", 10).unwrap().len(), 1);
        assert_eq!(s.search_history("Language", 10).unwrap().len(), 1);
        assert_eq!(s.search_history("nothing", 10).unwrap().len(), 0);
    }

    #[test]
    fn like_wildcards_in_a_query_are_literal() {
        let s = storage();
        s.record_visit("https://example.com/a", "plain").unwrap();
        s.record_visit("https://example.com/b", "100% sure").unwrap();

        // Without escaping, "%" would match every row.
        let hits = s.search_history("100%", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "100% sure");

        assert_eq!(s.search_history("_", 10).unwrap().len(), 0);
    }

    #[test]
    fn empty_search_returns_recent_history() {
        let s = storage();
        s.record_visit("https://example.com/", "E").unwrap();
        assert_eq!(s.search_history("   ", 10).unwrap().len(), 1);
    }

    #[test]
    fn deleting_and_clearing() {
        let s = storage();
        let _ = s.record_visit("https://a.test/", "a").unwrap();
        s.record_visit("https://b.test/", "b").unwrap();

        let id = s.recent_history(10).unwrap()[0].id;
        s.delete_visit(id).unwrap();
        assert_eq!(s.recent_history(10).unwrap().len(), 1);

        s.clear_history().unwrap();
        assert!(s.recent_history(10).unwrap().is_empty());
    }

    #[test]
    fn clearing_a_time_range_keeps_older_entries() {
        let s = storage();
        s.record_visit("https://old.test/", "old").unwrap();
        s.conn()
            .execute("UPDATE history SET visited_at = 0 WHERE url = 'https://old.test/'", [])
            .unwrap();
        s.record_visit("https://new.test/", "new").unwrap();

        // Clear the last hour: the backdated row must survive.
        s.clear_history_since(3_600_000).unwrap();
        let remaining = s.recent_history(10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].url, "https://old.test/");
    }
}
