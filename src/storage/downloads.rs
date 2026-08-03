//! Download history.
//!
//! Rows mirror WebView2's `ICoreWebView2DownloadOperation` state machine. The
//! in-progress byte counter is updated from `BytesReceivedChanged`, which fires
//! often, so that write goes through a cached statement and touches one row.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::Storage;
use crate::util::now_millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadState {
    InProgress,
    Completed,
    Interrupted,
    Cancelled,
}

impl DownloadState {
    fn to_i64(self) -> i64 {
        match self {
            Self::InProgress => 0,
            Self::Completed => 1,
            Self::Interrupted => 2,
            Self::Cancelled => 3,
        }
    }

    fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::Completed,
            2 => Self::Interrupted,
            3 => Self::Cancelled,
            _ => Self::InProgress,
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::InProgress)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: i64,
    pub url: String,
    pub target_path: String,
    pub total_bytes: i64,
    pub received_bytes: i64,
    pub state: DownloadState,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl Storage {
    pub fn start_download(
        &self,
        url: &str,
        target_path: &str,
        total_bytes: i64,
    ) -> rusqlite::Result<i64> {
        self.conn().execute(
            "INSERT INTO downloads
                 (url, target_path, total_bytes, received_bytes, state, started_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![url, target_path, total_bytes, DownloadState::InProgress.to_i64(), now_millis()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    /// Hot path: called repeatedly while bytes arrive.
    pub fn update_download_progress(&self, id: i64, received: i64) -> rusqlite::Result<()> {
        let mut stmt = self
            .conn()
            .prepare_cached("UPDATE downloads SET received_bytes = ?2 WHERE id = ?1")?;
        stmt.execute(params![id, received])?;
        Ok(())
    }

    pub fn finish_download(&self, id: i64, state: DownloadState) -> rusqlite::Result<()> {
        // `finished_at` is only meaningful once the transfer stopped.
        let finished_at = state.is_terminal().then(now_millis);
        self.conn().execute(
            "UPDATE downloads SET state = ?2, finished_at = ?3 WHERE id = ?1",
            params![id, state.to_i64(), finished_at],
        )?;
        Ok(())
    }

    /// Also records the final size, which WebView2 only knows on completion.
    pub fn finish_download_with_size(
        &self,
        id: i64,
        state: DownloadState,
        total_bytes: i64,
        received_bytes: i64,
    ) -> rusqlite::Result<()> {
        let finished_at = state.is_terminal().then(now_millis);
        self.conn().execute(
            "UPDATE downloads
             SET state = ?2, total_bytes = ?3, received_bytes = ?4, finished_at = ?5
             WHERE id = ?1",
            params![id, state.to_i64(), total_bytes, received_bytes, finished_at],
        )?;
        Ok(())
    }

    pub fn recent_downloads(&self, limit: u32) -> rusqlite::Result<Vec<Download>> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, url, target_path, total_bytes, received_bytes,
                    state, started_at, finished_at
             FROM downloads ORDER BY started_at DESC LIMIT ?1",
        )?;
        let downloads = stmt.query_map([limit], map_download)?.collect();
        downloads
    }

    pub fn get_download(&self, id: i64) -> rusqlite::Result<Option<Download>> {
        self.conn()
            .query_row(
                "SELECT id, url, target_path, total_bytes, received_bytes,
                        state, started_at, finished_at
                 FROM downloads WHERE id = ?1",
                [id],
                map_download,
            )
            .optional()
    }

    /// Remove the list entry. The file on disk is deliberately left alone.
    pub fn remove_download(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM downloads WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn clear_downloads(&self) -> rusqlite::Result<()> {
        self.conn().execute("DELETE FROM downloads", [])?;
        Ok(())
    }

    /// Drop finished entries older than `days`. In-progress rows are kept
    /// regardless of age.
    pub fn prune_downloads(&self, days: u64) -> rusqlite::Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = now_millis() - (days as i64) * 86_400_000;
        self.conn().execute(
            "DELETE FROM downloads WHERE state <> ?1 AND started_at < ?2",
            params![DownloadState::InProgress.to_i64(), cutoff],
        )
    }

    /// Mark downloads that were still running when the browser exited. Without
    /// this they would show as active forever.
    pub fn interrupt_stale_downloads(&self) -> rusqlite::Result<usize> {
        self.conn().execute(
            "UPDATE downloads SET state = ?2, finished_at = ?3 WHERE state = ?1",
            params![
                DownloadState::InProgress.to_i64(),
                DownloadState::Interrupted.to_i64(),
                now_millis()
            ],
        )
    }
}

fn map_download(row: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
    Ok(Download {
        id: row.get(0)?,
        url: row.get(1)?,
        target_path: row.get(2)?,
        total_bytes: row.get(3)?,
        received_bytes: row.get(4)?,
        state: DownloadState::from_i64(row.get(5)?),
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        Storage::in_memory().unwrap()
    }

    #[test]
    fn removing_an_entry_keeps_the_others() {
        let s = storage();
        let a = s.start_download("https://x.test/a", "a", 1).unwrap();
        s.start_download("https://x.test/b", "b", 1).unwrap();
        s.remove_download(a).unwrap();
        assert_eq!(s.recent_downloads(10).unwrap().len(), 1);
        s.clear_downloads().unwrap();
        assert!(s.recent_downloads(10).unwrap().is_empty());
    }

    #[test]
    fn pruning_keeps_running_downloads_regardless_of_age() {
        let s = storage();
        let old_done = s.start_download("https://x.test/old", "old", 1).unwrap();
        s.finish_download(old_done, DownloadState::Completed).unwrap();
        let old_running = s.start_download("https://x.test/run", "run", 1).unwrap();

        s.conn().execute("UPDATE downloads SET started_at = 0", []).unwrap();

        assert_eq!(s.prune_downloads(30).unwrap(), 1);
        let left = s.recent_downloads(10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, old_running);

        // days == 0 means "keep everything", not "delete everything".
        assert_eq!(s.prune_downloads(0).unwrap(), 0);
    }

    #[test]
    fn downloads_running_at_shutdown_become_interrupted() {
        let s = storage();
        let id = s.start_download("https://x.test/f", "f", 10).unwrap();
        assert_eq!(s.interrupt_stale_downloads().unwrap(), 1);
        assert_eq!(s.get_download(id).unwrap().unwrap().state, DownloadState::Interrupted);
        // Second run has nothing left to fix up.
        assert_eq!(s.interrupt_stale_downloads().unwrap(), 0);
    }

}
