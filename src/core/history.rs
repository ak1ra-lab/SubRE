//! Sqlite-backed history of in-place renames, keyed by file identity.
//!
//! Schema (per design D1):
//!
//! ```sql
//! sessions(id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL, working_dir TEXT,
//!          undo_of INTEGER REFERENCES sessions(id));
//! renames(id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id),
//!         dir TEXT NOT NULL, checksum TEXT NOT NULL, unit_id TEXT,
//!         old_name TEXT NOT NULL, new_name TEXT NOT NULL, at INTEGER NOT NULL);
//! CREATE INDEX idx_renames_identity ON renames(dir, checksum, at);
//! CREATE INDEX idx_renames_session ON renames(session_id);
//! ```
//!
//! A file's identity is `(dir, checksum)`. Only in-place renames are
//! recorded (copy is not). Undo creates a new session whose `undo_of`
//! points back to the session being reversed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    working_dir TEXT,
    undo_of INTEGER REFERENCES sessions(id)
);
CREATE TABLE IF NOT EXISTS renames (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    dir TEXT NOT NULL,
    checksum TEXT NOT NULL,
    unit_id TEXT,
    old_name TEXT NOT NULL,
    new_name TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_renames_identity ON renames(dir, checksum, at);
CREATE INDEX IF NOT EXISTS idx_renames_session ON renames(session_id);
";

/// Schema version. Older databases (version < `CURRENT_VERSION`) predate
/// the checksum/append-only model and are rebuilt from scratch.
const CURRENT_VERSION: i64 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: i64,
    pub created_at: i64,
    pub working_dir: Option<String>,
    pub undo_of: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRecord {
    pub id: i64,
    pub session_id: i64,
    pub dir: String,
    pub checksum: String,
    pub unit_id: Option<String>,
    pub old_name: String,
    pub new_name: String,
    pub at: i64,
}

/// Default database location: `dirs::data_dir()/subtitle-renamer/history.db`.
pub fn default_db_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("subtitle-renamer")
        .join("history.db")
}

#[derive(Debug)]
pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    /// Open (or create) the database at `path`, migrating/recreating the
    /// schema as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create history db parent {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open history db {}", path.display()))?;
        Self::migrate(&conn)?;
        conn.execute_batch(SCHEMA).context("init history schema")?;
        Ok(Self { conn })
    }

    /// If the database predates the current schema, drop any legacy
    /// tables and bump `user_version`. Fresh databases start at version 0
    /// and receive the current schema for free.
    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read user_version")?;
        if version < CURRENT_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS operations;
                 DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS renames;
                 PRAGMA user_version = 2;",
            )
            .context("migrate: drop legacy history tables")?;
        }
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_db_path())
    }

    /// Create a new session and return its id.
    pub fn create_session(&self, working_dir: Option<&Path>) -> Result<i64> {
        self.create_session_inner(None, working_dir)
    }

    /// Create a session that reverses `undo_of` and return its id.
    pub fn create_undo_session(&self, undo_of: i64, working_dir: Option<&Path>) -> Result<i64> {
        self.create_session_inner(Some(undo_of), working_dir)
    }

    fn create_session_inner(
        &self,
        undo_of: Option<i64>,
        working_dir: Option<&Path>,
    ) -> Result<i64> {
        let now = now_epoch();
        let wd = working_dir.map(|p| p.display().to_string());
        self.conn.execute(
            "INSERT INTO sessions (created_at, working_dir, undo_of) VALUES (?1, ?2, ?3)",
            params![now, wd, undo_of],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Append a rename event to a session. `dir` + `old_name`/`new_name`
    /// are basename-level; `checksum` is the file's content fingerprint
    /// (identity). `unit_id` links `.idx`+`.sub` pairs.
    pub fn record_rename(
        &self,
        session_id: i64,
        dir: &Path,
        checksum: &str,
        unit_id: Option<&str>,
        old_name: &str,
        new_name: &str,
    ) -> Result<i64> {
        let at = now_epoch();
        self.conn.execute(
            "INSERT INTO renames (session_id, dir, checksum, unit_id, old_name, new_name, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                dir.display().to_string(),
                checksum,
                unit_id,
                old_name,
                new_name,
                at
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List sessions in reverse chronological order (newest first).
    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, working_dir, undo_of FROM sessions ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    working_dir: row.get(2)?,
                    undo_of: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// List rename events belonging to a session, oldest first.
    pub fn renames_for_session(&self, session_id: i64) -> Result<Vec<RenameRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, dir, checksum, unit_id, old_name, new_name, at
             FROM renames WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_rename)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All rename events for a file identity `(dir, checksum)`, in time
    /// order. Used by the drag-in hint.
    pub fn timeline_for(&self, dir: &Path, checksum: &str) -> Result<Vec<RenameRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, dir, checksum, unit_id, old_name, new_name, at
             FROM renames WHERE dir = ?1 AND checksum = ?2 ORDER BY at ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![dir.display().to_string(), checksum], row_to_rename)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn row_to_rename(row: &rusqlite::Row<'_>) -> rusqlite::Result<RenameRecord> {
    Ok(RenameRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        dir: row.get(2)?,
        checksum: row.get(3)?,
        unit_id: row.get(4)?,
        old_name: row.get(5)?,
        new_name: row.get(6)?,
        at: row.get(7)?,
    })
}

fn now_epoch() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (HistoryDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_hist_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        (HistoryDb::open(&path).unwrap(), path)
    }

    #[test]
    fn create_session_and_record_rename() {
        let (db, _) = tmp_db();
        let sid = db.create_session(Some(Path::new("/tmp"))).unwrap();
        let id =
            db.record_rename(sid, Path::new("/tmp"), "deadbeef", None, "a.ass", "b.ass").unwrap();
        assert!(id > 0);
        let renames = db.renames_for_session(sid).unwrap();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_name, "a.ass");
        assert_eq!(renames[0].new_name, "b.ass");
        assert_eq!(renames[0].checksum, "deadbeef");
    }

    #[test]
    fn list_sessions_in_reverse_order() {
        let (db, _) = tmp_db();
        let s1 = db.create_session(None).unwrap();
        let s2 = db.create_session(None).unwrap();
        let sessions = db.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, s2);
        assert_eq!(sessions[1].id, s1);
        assert_eq!(sessions[0].undo_of, None);
    }

    #[test]
    fn undo_session_links_undo_of() {
        let (db, _) = tmp_db();
        let original = db.create_session(None).unwrap();
        let undo = db.create_undo_session(original, None).unwrap();
        let sessions = db.list_sessions().unwrap();
        let undo_rec = sessions.iter().find(|s| s.id == undo).unwrap();
        assert_eq!(undo_rec.undo_of, Some(original));
    }

    #[test]
    fn renames_for_session_ordered_and_scoped() {
        let (db, _) = tmp_db();
        let s1 = db.create_session(None).unwrap();
        let s2 = db.create_session(None).unwrap();
        db.record_rename(s1, Path::new("/a"), "c1", Some("u1"), "1.idx", "x.idx").unwrap();
        db.record_rename(s1, Path::new("/a"), "c2", Some("u1"), "1.sub", "x.sub").unwrap();
        db.record_rename(s2, Path::new("/b"), "c3", None, "2.ass", "y.ass").unwrap();
        let ops = db.renames_for_session(s1).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].old_name, "1.idx");
        assert_eq!(ops[1].old_name, "1.sub");
        assert!(ops.iter().all(|o| o.session_id == s1));
    }

    #[test]
    fn timeline_for_identity() {
        let (db, _) = tmp_db();
        let sid = db.create_session(None).unwrap();
        db.record_rename(sid, Path::new("/subs"), "abc", None, "orig.ass", "Show - 01.ass")
            .unwrap();
        // A different file (different checksum) in the same dir is out of scope.
        db.record_rename(sid, Path::new("/subs"), "def", None, "other.ass", "Other.ass").unwrap();
        let timeline = db.timeline_for(Path::new("/subs"), "abc").unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].new_name, "Show - 01.ass");
    }

    #[test]
    fn open_rebuilds_legacy_schema() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_hist_legacy_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        // Create an old-style `operations` table to mimic a legacy db.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE operations (id INTEGER PRIMARY KEY);").unwrap();
        drop(conn);
        // Opening should drop `operations` and install the new schema.
        let db = HistoryDb::open(&path).unwrap();
        let has_operations: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='operations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_operations, 0);
        // And the new tables exist and work.
        let sid = db.create_session(None).unwrap();
        assert!(sid > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
