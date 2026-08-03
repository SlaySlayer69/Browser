//! Speed dials: the tiles under the search bar on the new-tab page.
//!
//! Each tile carries an optional accent colour so the new-tab page can draw a
//! coloured monogram instead of fetching a favicon. That keeps the new-tab page
//! free of network requests entirely — it renders identically offline and costs
//! nothing to open.

use rusqlite::params;
use serde::Serialize;

use super::Storage;
use crate::util::host_of;

/// Tiles beyond this do not fit the grid and would push the fold.
pub const MAX_SPEED_DIALS: usize = 12;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpeedDial {
    pub id: i64,
    pub url: String,
    pub title: String,
    /// `#rrggbb`, or empty to let the UI derive one from the host.
    pub accent: String,
    pub position: i64,
    /// Letter drawn on the tile. Computed, not stored.
    pub monogram: String,
}

impl Storage {
    pub fn add_speed_dial(&self, url: &str, title: &str, accent: &str) -> rusqlite::Result<i64> {
        let next_position: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM speed_dials",
            [],
            |r| r.get(0),
        )?;

        self.conn().execute(
            "INSERT INTO speed_dials (url, title, accent, position)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(url) DO UPDATE SET
                 title  = CASE WHEN excluded.title <> '' THEN excluded.title
                               ELSE speed_dials.title END,
                 accent = CASE WHEN excluded.accent <> '' THEN excluded.accent
                               ELSE speed_dials.accent END",
            params![url, title, accent, next_position],
        )?;

        self.conn()
            .query_row("SELECT id FROM speed_dials WHERE url = ?1", [url], |r| r.get(0))
    }

    /// `true` when the grid is full and [`add_speed_dial`] would overflow it.
    pub fn speed_dials_full(&self) -> rusqlite::Result<bool> {
        let count: i64 = self.conn().query_row("SELECT COUNT(*) FROM speed_dials", [], |r| r.get(0))?;
        Ok(count as usize >= MAX_SPEED_DIALS)
    }

    pub fn remove_speed_dial(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM speed_dials WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn speed_dials(&self) -> rusqlite::Result<Vec<SpeedDial>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, url, title, accent, position
             FROM speed_dials ORDER BY position ASC, id ASC LIMIT ?1",
        )?;
        let dials = stmt.query_map([MAX_SPEED_DIALS as i64], |row| {
            let url: String = row.get(1)?;
            let title: String = row.get(2)?;
            Ok(SpeedDial {
                monogram: monogram_for(&url, &title),
                id: row.get(0)?,
                accent: row.get(3)?,
                position: row.get(4)?,
                url,
                title,
            })
        })?
        .collect();
        dials
    }

    pub fn update_speed_dial(
        &self,
        id: i64,
        title: &str,
        accent: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE speed_dials SET title = ?2, accent = ?3 WHERE id = ?1",
            params![id, title, accent],
        )?;
        Ok(())
    }

    pub fn reorder_speed_dials(&self, ordered_ids: &[i64]) -> rusqlite::Result<()> {
        let mut stmt =
            self.conn().prepare_cached("UPDATE speed_dials SET position = ?2 WHERE id = ?1")?;
        for (index, id) in ordered_ids.iter().enumerate() {
            stmt.execute(params![id, index as i64])?;
        }
        Ok(())
    }

    /// Suggest tiles for an empty new-tab page from the most-visited history
    /// entries. Called once, when the user has no dials of their own.
    pub fn suggested_speed_dials(&self, limit: u32) -> rusqlite::Result<Vec<SpeedDial>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT url, title FROM history
             WHERE host <> ''
             GROUP BY host
             ORDER BY SUM(visits) DESC, MAX(visited_at) DESC
             LIMIT ?1",
        )?;
        let dials = stmt
            .query_map([limit.min(MAX_SPEED_DIALS as u32)], |row| {
                let url: String = row.get(0)?;
                let title: String = row.get(1)?;
                Ok(SpeedDial {
                    id: 0,
                    monogram: monogram_for(&url, &title),
                    accent: String::new(),
                    position: 0,
                    url,
                    title,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(dials)
    }
}

/// First letter of the title, else of the registrable host, uppercased.
fn monogram_for(url: &str, title: &str) -> String {
    let source = if title.trim().is_empty() {
        host_of(url).map(|h| h.trim_start_matches("www.").to_string()).unwrap_or_default()
    } else {
        title.trim().to_string()
    };

    source
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::in_memory().unwrap()
    }

    #[test]
    fn monograms_prefer_the_title_then_the_host() {
        assert_eq!(monogram_for("https://example.com/", "Rust"), "R");
        assert_eq!(monogram_for("https://www.example.com/", ""), "E");
        assert_eq!(monogram_for("https://example.com/", "  "), "E");
        // Nothing usable at all.
        assert_eq!(monogram_for("not a url", ""), "?");
        // Leading punctuation is skipped.
        assert_eq!(monogram_for("https://x.test/", "!important"), "I");
    }

    #[test]
    fn dials_keep_insertion_order_and_expose_a_monogram() {
        let s = storage();
        s.add_speed_dial("https://a.test/", "Alpha", "#ff0000").unwrap();
        s.add_speed_dial("https://b.test/", "Beta", "").unwrap();

        let dials = s.speed_dials().unwrap();
        assert_eq!(dials.len(), 2);
        assert_eq!(dials[0].title, "Alpha");
        assert_eq!(dials[0].monogram, "A");
        assert_eq!(dials[0].accent, "#ff0000");
        assert_eq!(dials[1].monogram, "B");
    }

    #[test]
    fn adding_the_same_url_updates_in_place() {
        let s = storage();
        let first = s.add_speed_dial("https://a.test/", "Alpha", "#111111").unwrap();
        let second = s.add_speed_dial("https://a.test/", "Renamed", "").unwrap();
        assert_eq!(first, second);

        let dials = s.speed_dials().unwrap();
        assert_eq!(dials.len(), 1);
        assert_eq!(dials[0].title, "Renamed");
        // Empty accent must not clear the existing one.
        assert_eq!(dials[0].accent, "#111111");
    }

    #[test]
    fn the_grid_is_capped() {
        let s = storage();
        for i in 0..(MAX_SPEED_DIALS + 4) {
            s.add_speed_dial(&format!("https://s{i}.test/"), &format!("S{i}"), "").unwrap();
        }
        assert!(s.speed_dials_full().unwrap());
        assert_eq!(s.speed_dials().unwrap().len(), MAX_SPEED_DIALS);
    }

    #[test]
    fn reordering_and_removal() {
        let s = storage();
        let a = s.add_speed_dial("https://a.test/", "A", "").unwrap();
        let b = s.add_speed_dial("https://b.test/", "B", "").unwrap();
        s.reorder_speed_dials(&[b, a]).unwrap();
        assert_eq!(s.speed_dials().unwrap()[0].title, "B");

        s.remove_speed_dial(b).unwrap();
        let left = s.speed_dials().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "A");
        assert!(!s.speed_dials_full().unwrap());
    }

    #[test]
    fn suggestions_come_from_the_most_visited_hosts() {
        let s = storage();
        s.record_visit("https://rare.test/", "Rare").unwrap();
        for _ in 0..5 {
            s.record_visit("https://often.test/", "Often").unwrap();
        }
        let suggestions = s.suggested_speed_dials(5).unwrap();
        assert_eq!(suggestions[0].title, "Often");
        assert_eq!(suggestions[0].monogram, "O");
    }

    #[test]
    fn updating_title_and_accent() {
        let s = storage();
        let id = s.add_speed_dial("https://a.test/", "A", "#000000").unwrap();
        s.update_speed_dial(id, "Renamed", "#00ff00").unwrap();
        let d = &s.speed_dials().unwrap()[0];
        assert_eq!(d.title, "Renamed");
        assert_eq!(d.accent, "#00ff00");
    }
}
