//! Sqlite-backed history of executed rename/copy operations.
//!
//! Schema (per design D6):
//!
//! ```sql
//! sessions(id INTEGER PRIMARY KEY, created_at TEXT NOT NULL, working_dir TEXT);
//! operations(id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id),
//!            action TEXT NOT NULL, src_path TEXT NOT NULL, dst_path TEXT NOT NULL,
//!            src_mtime INTEGER, src_size INTEGER, undone INTEGER NOT NULL DEFAULT 0);
//! CREATE INDEX idx_operations_src ON operations(src_path);
//! CREATE INDEX idx_operations_dst ON operations(dst_path);
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::execute::HistoricalOp;
use super::plan::PlannedAction;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,
    working_dir TEXT
);
CREATE TABLE IF NOT EXISTS operations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    action TEXT NOT NULL,
    src_path TEXT NOT NULL,
    dst_path TEXT NOT NULL,
    src_mtime INTEGER NOT NULL,
    src_size INTEGER NOT NULL,
    src_checksum TEXT NOT NULL DEFAULT '',
    undone INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_operations_src ON operations(src_path);
CREATE INDEX IF NOT EXISTS idx_operations_dst ON operations(dst_path);
";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: i64,
    pub created_at: String,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: i64,
    pub session_id: i64,
    pub action: String,
    pub src_path: String,
    pub dst_path: String,
    pub src_mtime: i64,
    pub src_size: i64,
    pub src_checksum: String,
    pub undone: bool,
}

impl OperationRecord {
    pub fn into_historical(&self) -> HistoricalOp {
        HistoricalOp {
            id: self.id,
            action: match self.action.as_str() {
                "rename" => PlannedAction::Rename,
                "copy" => PlannedAction::Copy,
                // Defensive default: unknown / future variants fall back to Rename.
                #[allow(clippy::match_same_arms)]
                _ => PlannedAction::Rename,
            },
            src_path: PathBuf::from(&self.src_path),
            dst_path: PathBuf::from(&self.dst_path),
            src_mtime: self.src_mtime,
            src_size: self.src_size,
            src_checksum: self.src_checksum.clone(),
            undone: self.undone,
        }
    }
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
    /// Open (or create) the database at `path` and ensure the schema exists.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create history db parent {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open history db {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("init history schema")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Lightweight in-place migration: add the `src_checksum` column to
    /// databases that were created before that field existed. New
    /// databases already have it from `SCHEMA`.
    fn migrate(conn: &Connection) -> Result<()> {
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(operations)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !cols.iter().any(|c| c == "src_checksum") {
            conn.execute_batch(
                "ALTER TABLE operations ADD COLUMN src_checksum TEXT NOT NULL DEFAULT ''",
            )
            .context("migrate: add src_checksum column")?;
        }
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_db_path())
    }

    /// Create a new session and return its id.
    pub fn create_session(&self, working_dir: Option<&Path>) -> Result<i64> {
        let now = chrono_like_now();
        let wd = working_dir.map(|p| p.display().to_string());
        self.conn.execute(
            "INSERT INTO sessions (created_at, working_dir) VALUES (?1, ?2)",
            params![now, wd],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Append a recorded operation to a session. `src_mtime`, `src_size`,
    /// and `src_checksum` are pre-snapshotted by the caller.
    pub fn record_operation(
        &self,
        session_id: i64,
        action: PlannedAction,
        src: &Path,
        dst: &Path,
        src_mtime: i64,
        src_size: i64,
        src_checksum: &str,
    ) -> Result<i64> {
        let action_str = match action {
            PlannedAction::Rename => "rename",
            PlannedAction::Copy => "copy",
        };
        self.conn.execute(
            "INSERT INTO operations
             (session_id, action, src_path, dst_path, src_mtime, src_size, src_checksum, undone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                session_id,
                action_str,
                src.display().to_string(),
                dst.display().to_string(),
                src_mtime,
                src_size,
                src_checksum
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Mark an operation as undone (used after a successful reverse).
    pub fn mark_undone(&self, op_id: i64) -> Result<()> {
        self.conn.execute("UPDATE operations SET undone = 1 WHERE id = ?1", params![op_id])?;
        Ok(())
    }

    /// Lookup all operations whose `dst_path` matches `path` and that are
    /// not yet undone. Used by the drag-in hint feature.
    pub fn find_by_dst(&self, path: &Path) -> Result<Vec<OperationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, action, src_path, dst_path, src_mtime, src_size, src_checksum, undone
             FROM operations WHERE dst_path = ?1 AND undone = 0
             ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map(params![path.display().to_string()], row_to_record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// List sessions in reverse chronological order.
    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, created_at, working_dir FROM sessions ORDER BY id DESC")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    working_dir: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// List operations belonging to a session, oldest first.
    pub fn operations_for_session(&self, session_id: i64) -> Result<Vec<OperationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, action, src_path, dst_path, src_mtime, src_size, src_checksum, undone
             FROM operations WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id], row_to_record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return every not-yet-undone operation across all sessions (oldest
    /// first) — used for whole-session undo.
    pub fn all_unresolved(&self) -> Result<Vec<OperationRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, action, src_path, dst_path, src_mtime, src_size, src_checksum, undone
             FROM operations WHERE undone = 0 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], row_to_record)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRecord> {
    Ok(OperationRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        action: row.get(2)?,
        src_path: row.get(3)?,
        dst_path: row.get(4)?,
        src_mtime: row.get(5)?,
        src_size: row.get(6)?,
        src_checksum: row.get(7)?,
        undone: row.get::<_, i64>(8)? != 0,
    })
}

/// Minimal RFC3339-ish "now" string without bringing in `chrono`.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("epoch:{secs}")
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> HistoryDb {
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
        HistoryDb::open(&path).unwrap()
    }

    #[test]
    fn create_session_and_record() {
        let db = tmp_db();
        let sid = db.create_session(Some(Path::new("/tmp"))).unwrap();
        let id = db
            .record_operation(
                sid,
                PlannedAction::Rename,
                Path::new("/tmp/a.ass"),
                Path::new("/tmp/b.ass"),
                100,
                200,
                "",
            )
            .unwrap();
        assert!(id > 0);
        let hits = db.find_by_dst(Path::new("/tmp/b.ass")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].src_path, "/tmp/a.ass");
        assert_eq!(hits[0].src_checksum, "");
        assert!(!hits[0].undone);
        db.mark_undone(id).unwrap();
        let hits = db.find_by_dst(Path::new("/tmp/b.ass")).unwrap();
        assert_eq!(hits.len(), 0);
    }

    #[test]
    fn list_sessions_in_reverse_order() {
        let db = tmp_db();
        let s1 = db.create_session(None).unwrap();
        let s2 = db.create_session(None).unwrap();
        let sessions = db.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, s2);
        assert_eq!(sessions[1].id, s1);
    }

    #[test]
    fn operations_for_session() {
        let db = tmp_db();
        let sid = db.create_session(None).unwrap();
        db.record_operation(sid, PlannedAction::Rename, Path::new("/a"), Path::new("/b"), 1, 1, "")
            .unwrap();
        db.record_operation(
            sid,
            PlannedAction::Copy,
            Path::new("/c"),
            Path::new("/d"),
            2,
            2,
            "deadbeef",
        )
        .unwrap();
        let ops = db.operations_for_session(sid).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].action, "rename");
        assert_eq!(ops[1].action, "copy");
    }
}
