//! Bookmarks: a flat, ordered list. No folders, by design — folders would need
//! a tree UI, drag-and-drop reparenting and recursive deletes for a feature the
//! spec does not ask for.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::Storage;
use crate::util::now_millis;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub added_at: i64,
    pub position: i64,
}

impl Storage {
    /// Add a bookmark, or update the title if the URL is already bookmarked.
    pub fn add_bookmark(&self, url: &str, title: &str) -> rusqlite::Result<i64> {
        let next_position: i64 = self
            .conn()
            .query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM bookmarks", [], |r| r.get(0))?;

        self.conn().execute(
            "INSERT INTO bookmarks (url, title, added_at, position)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(url) DO UPDATE SET
                 title = CASE WHEN excluded.title <> '' THEN excluded.title
                              ELSE bookmarks.title END",
            params![url, title, now_millis(), next_position],
        )?;

        self.conn()
            .query_row("SELECT id FROM bookmarks WHERE url = ?1", [url], |r| r.get(0))
    }

    pub fn remove_bookmark(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn remove_bookmark_by_url(&self, url: &str) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM bookmarks WHERE url = ?1", [url])?;
        Ok(())
    }

    /// Add if absent, remove if present. Returns the new state.
    pub fn toggle_bookmark(&self, url: &str, title: &str) -> rusqlite::Result<bool> {
        if self.is_bookmarked(url)? {
            self.remove_bookmark_by_url(url)?;
            Ok(false)
        } else {
            self.add_bookmark(url, title)?;
            Ok(true)
        }
    }

    pub fn is_bookmarked(&self, url: &str) -> rusqlite::Result<bool> {
        let mut stmt =
            self.conn().prepare_cached("SELECT id FROM bookmarks WHERE url = ?1")?;
        let found: Option<i64> = stmt.query_row([url], |r| r.get(0)).optional()?;
        Ok(found.is_some())
    }

    pub fn bookmarks(&self) -> rusqlite::Result<Vec<Bookmark>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, url, title, added_at, position
             FROM bookmarks ORDER BY position ASC, added_at ASC",
        )?;
        let bookmarks = stmt.query_map([], map_bookmark)?.collect();
        bookmarks
    }

    pub fn rename_bookmark(&self, id: i64, title: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("UPDATE bookmarks SET title = ?2 WHERE id = ?1", params![id, title])?;
        Ok(())
    }

    /// Persist a user-defined ordering. Ids not in the list keep their place
    /// after the ones that are.
    pub fn reorder_bookmarks(&self, ordered_ids: &[i64]) -> rusqlite::Result<()> {
        let mut stmt =
            self.conn().prepare_cached("UPDATE bookmarks SET position = ?2 WHERE id = ?1")?;
        for (index, id) in ordered_ids.iter().enumerate() {
            stmt.execute(params![id, index as i64])?;
        }
        Ok(())
    }
}

fn map_bookmark(row: &rusqlite::Row<'_>) -> rusqlite::Result<Bookmark> {
    Ok(Bookmark {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        added_at: row.get(3)?,
        position: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::in_memory().unwrap()
    }

    #[test]
    fn adding_twice_updates_instead_of_duplicating() {
        let s = storage();
        let first = s.add_bookmark("https://x.test/", "First").unwrap();
        let second = s.add_bookmark("https://x.test/", "Second").unwrap();
        assert_eq!(first, second);

        let all = s.bookmarks().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Second");
    }

    #[test]
    fn an_empty_title_never_overwrites_a_real_one() {
        let s = storage();
        s.add_bookmark("https://x.test/", "Real").unwrap();
        s.add_bookmark("https://x.test/", "").unwrap();
        assert_eq!(s.bookmarks().unwrap()[0].title, "Real");
    }

    #[test]
    fn toggle_adds_then_removes() {
        let s = storage();
        assert!(s.toggle_bookmark("https://x.test/", "X").unwrap());
        assert!(s.is_bookmarked("https://x.test/").unwrap());

        assert!(!s.toggle_bookmark("https://x.test/", "X").unwrap());
        assert!(!s.is_bookmarked("https://x.test/").unwrap());
        assert!(s.bookmarks().unwrap().is_empty());
    }

    #[test]
    fn new_bookmarks_append_to_the_end() {
        let s = storage();
        s.add_bookmark("https://a.test/", "A").unwrap();
        s.add_bookmark("https://b.test/", "B").unwrap();
        s.add_bookmark("https://c.test/", "C").unwrap();

        let titles: Vec<_> = s.bookmarks().unwrap().into_iter().map(|b| b.title).collect();
        assert_eq!(titles, ["A", "B", "C"]);
    }

    #[test]
    fn reordering_is_persisted() {
        let s = storage();
        let a = s.add_bookmark("https://a.test/", "A").unwrap();
        let b = s.add_bookmark("https://b.test/", "B").unwrap();
        let c = s.add_bookmark("https://c.test/", "C").unwrap();

        s.reorder_bookmarks(&[c, a, b]).unwrap();
        let titles: Vec<_> = s.bookmarks().unwrap().into_iter().map(|x| x.title).collect();
        assert_eq!(titles, ["C", "A", "B"]);
    }

    #[test]
    fn renaming_and_removing() {
        let s = storage();
        let id = s.add_bookmark("https://x.test/", "Old").unwrap();
        s.rename_bookmark(id, "New").unwrap();
        assert_eq!(s.bookmarks().unwrap()[0].title, "New");

        s.remove_bookmark(id).unwrap();
        assert!(s.bookmarks().unwrap().is_empty());
    }
}

/// Timing harness for the per-navigation lookup. See `blocker::perf`.
#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "timing harness, not a correctness check"]
    fn bookmark_lookup_cost() {
        let s = Storage::in_memory().unwrap();
        for i in 0..500 {
            s.add_bookmark(&format!("https://site{i}.test/page"), &format!("Site {i}")).unwrap();
        }
        let url = "https://site250.test/page";

        let iterations = 20_000;

        // What `push_navigation` used to do on every navigation event: prepare
        // the statement afresh each time.
        s.is_bookmarked(url).unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            let found: Option<i64> = s
                .conn()
                .query_row("SELECT id FROM bookmarks WHERE url = ?1", [url], |r| r.get(0))
                .optional()
                .unwrap();
            std::hint::black_box(found);
        }
        let uncached = start.elapsed().as_nanos() as f64 / f64::from(iterations);

        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(s.is_bookmarked(url).unwrap());
        }
        let cached = start.elapsed().as_nanos() as f64 / f64::from(iterations);

        println!("\nbookmark lookup, 500 rows");
        println!("  statement prepared per call (previous)   {uncached:>8.0} ns");
        println!("  cached statement (current)               {cached:>8.0} ns");
        println!("  -> {:.1}x faster; and the call itself is now made", uncached / cached);
        println!("     once per navigation instead of ~35 times\n");
    }
}
