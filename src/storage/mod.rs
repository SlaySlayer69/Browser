//! Local, on-disk state: history, downloads, bookmarks and speed dials.
//!
//! One SQLite file holds all four. SQLite is the right size of tool here: a
//! ~700 KB statically linked engine with a page cache we cap explicitly, versus
//! four hand-rolled file formats that would each need their own crash-safety
//! story. Nothing is ever written for an incognito window — see
//! [`Storage::in_memory`].

mod bookmarks;
mod downloads;
mod history;
mod speeddial;

pub use bookmarks::Bookmark;
pub use downloads::{Download, DownloadState};
pub use history::Visit;
pub use speeddial::SpeedDial;

use std::path::Path;

use rusqlite::Connection;

/// Schema version. Bump when adding a migration to [`migrate`].
const SCHEMA_VERSION: i64 = 1;

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let storage = Self { conn };
        storage.configure()?;
        migrate(&storage.conn)?;
        Ok(storage)
    }

    /// Backing store for incognito windows: never touches the disk, freed when
    /// the window closes.
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let storage = Self { conn };
        storage.configure()?;
        migrate(&storage.conn)?;
        Ok(storage)
    }

    fn configure(&self) -> rusqlite::Result<()> {
        // WAL keeps readers from blocking the UI thread behind a writer.
        // A negative cache_size is in KiB, so this caps SQLite's page cache at
        // 2 MiB instead of the 2 MB-and-growing default.
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -2000;
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE history (
                 id         INTEGER PRIMARY KEY,
                 url        TEXT    NOT NULL,
                 title      TEXT    NOT NULL DEFAULT '',
                 host       TEXT    NOT NULL DEFAULT '',
                 visited_at INTEGER NOT NULL,
                 visits     INTEGER NOT NULL DEFAULT 1
             );
             -- One row per URL; revisits bump `visits` and `visited_at`.
             CREATE UNIQUE INDEX history_url ON history(url);
             CREATE INDEX history_visited_at ON history(visited_at DESC);

             CREATE TABLE downloads (
                 id             INTEGER PRIMARY KEY,
                 url            TEXT    NOT NULL,
                 target_path    TEXT    NOT NULL,
                 total_bytes    INTEGER NOT NULL DEFAULT 0,
                 received_bytes INTEGER NOT NULL DEFAULT 0,
                 state          INTEGER NOT NULL,
                 started_at     INTEGER NOT NULL,
                 finished_at    INTEGER
             );
             CREATE INDEX downloads_started_at ON downloads(started_at DESC);

             CREATE TABLE bookmarks (
                 id       INTEGER PRIMARY KEY,
                 url      TEXT    NOT NULL,
                 title    TEXT    NOT NULL DEFAULT '',
                 added_at INTEGER NOT NULL,
                 position INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX bookmarks_url ON bookmarks(url);

             CREATE TABLE speed_dials (
                 id       INTEGER PRIMARY KEY,
                 url      TEXT    NOT NULL,
                 title    TEXT    NOT NULL DEFAULT '',
                 accent   TEXT    NOT NULL DEFAULT '',
                 position INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX speed_dials_url ON speed_dials(url);",
        )?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent() {
        let storage = Storage::in_memory().unwrap();
        // Running it again must not fail on "table already exists".
        migrate(storage.conn()).unwrap();
        let version: i64 =
            storage.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn an_in_memory_database_starts_empty() {
        // Incognito windows get this; nothing may survive from the profile.
        let storage = Storage::in_memory().unwrap();
        assert!(storage.recent_history(10).unwrap().is_empty());
        assert!(storage.bookmarks().unwrap().is_empty());
    }
}
