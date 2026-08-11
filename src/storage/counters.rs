//! Lifetime counters for the privacy hub.
//!
//! Deliberately a key/value table: these are monotonically increasing totals
//! with no structure to them, and adding a new counter should not need a schema
//! migration.
//!
//! Nothing here is written per blocked request — see [`crate::stats::
//! PendingCounters`] for why, and [`Storage::add_counter`] for the batched
//! write that replaces it.

use rusqlite::params;

use super::Storage;

pub const BLOCKED_TOTAL: &str = "blocked_total";
pub const BYTES_SAVED_TOTAL: &str = "bytes_saved_total";

impl Storage {
    /// Read a counter. A counter that was never written reads as zero.
    pub fn counter(&self, key: &str) -> rusqlite::Result<u64> {
        let value: i64 = self.conn().query_row(
            "SELECT COALESCE((SELECT value FROM counters WHERE key = ?1), 0)",
            [key],
            |row| row.get(0),
        )?;
        // Counters are only ever incremented; a negative value would mean the
        // file was edited by hand. Clamp rather than wrap into a huge u64.
        Ok(value.max(0) as u64)
    }

    /// Add to a counter, creating it if absent.
    pub fn add_counter(&self, key: &str, delta: u64) -> rusqlite::Result<()> {
        if delta == 0 {
            return Ok(());
        }
        let mut stmt = self.conn().prepare_cached(
            "INSERT INTO counters (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = value + excluded.value",
        )?;
        stmt.execute(params![key, delta as i64])?;
        Ok(())
    }

    pub fn reset_counters(&self) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM counters", [])?;
        Ok(())
    }

    /// Number of rows in the history table, for the hub's fourth tile.
    pub fn history_count(&self) -> rusqlite::Result<u64> {
        let count: i64 =
            self.conn().query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::in_memory().unwrap()
    }

    #[test]
    fn an_unwritten_counter_reads_as_zero() {
        assert_eq!(storage().counter(BLOCKED_TOTAL).unwrap(), 0);
        assert_eq!(storage().counter("never-heard-of-it").unwrap(), 0);
    }

    #[test]
    fn counters_accumulate_across_writes() {
        let s = storage();
        s.add_counter(BLOCKED_TOTAL, 5).unwrap();
        s.add_counter(BLOCKED_TOTAL, 7).unwrap();
        assert_eq!(s.counter(BLOCKED_TOTAL).unwrap(), 12);

        // Independent keys do not interfere.
        s.add_counter(BYTES_SAVED_TOTAL, 1000).unwrap();
        assert_eq!(s.counter(BLOCKED_TOTAL).unwrap(), 12);
        assert_eq!(s.counter(BYTES_SAVED_TOTAL).unwrap(), 1000);
    }

    #[test]
    fn adding_zero_is_a_no_op() {
        let s = storage();
        s.add_counter(BLOCKED_TOTAL, 0).unwrap();
        // Not even a row is created, so nothing to clean up later.
        let rows: i64 =
            s.conn().query_row("SELECT COUNT(*) FROM counters", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn a_hand_edited_negative_value_reads_as_zero() {
        let s = storage();
        s.conn()
            .execute("INSERT INTO counters (key, value) VALUES ('blocked_total', -5)", [])
            .unwrap();
        assert_eq!(s.counter(BLOCKED_TOTAL).unwrap(), 0, "must not wrap to u64::MAX");
    }

    #[test]
    fn large_totals_survive_a_round_trip() {
        let s = storage();
        // A heavy profile over a year is comfortably inside i64.
        s.add_counter(BYTES_SAVED_TOTAL, 900_000_000_000).unwrap();
        assert_eq!(s.counter(BYTES_SAVED_TOTAL).unwrap(), 900_000_000_000);
    }

    #[test]
    fn resetting_clears_every_counter() {
        let s = storage();
        s.add_counter(BLOCKED_TOTAL, 3).unwrap();
        s.add_counter(BYTES_SAVED_TOTAL, 3).unwrap();
        s.reset_counters().unwrap();
        assert_eq!(s.counter(BLOCKED_TOTAL).unwrap(), 0);
        assert_eq!(s.counter(BYTES_SAVED_TOTAL).unwrap(), 0);
    }

    #[test]
    fn history_count_tracks_the_table() {
        let s = storage();
        assert_eq!(s.history_count().unwrap(), 0);
        s.record_visit("https://a.test/", "A").unwrap();
        s.record_visit("https://b.test/", "B").unwrap();
        // Deduplicated by URL, so a revisit does not bump the count.
        s.record_visit("https://a.test/", "A").unwrap();
        assert_eq!(s.history_count().unwrap(), 2);
    }
}
