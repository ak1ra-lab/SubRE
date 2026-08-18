//! Application-state persistence: rename history plus session/identity tracking.
//!
//! Schema (per design D3, v4):
//!
//! ```sql
//! sessions(id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL, subtitle_dir TEXT,
//!          undo_of INTEGER REFERENCES sessions(id));
//! renames(id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id),
//!         dir TEXT NOT NULL, checksum TEXT NOT NULL, unit_id TEXT,
//!         old_name TEXT NOT NULL, new_name TEXT NOT NULL, at INTEGER NOT NULL);
//! copies(id INTEGER PRIMARY KEY, session_id INTEGER REFERENCES sessions(id),
//!        at INTEGER NOT NULL,
//!        src_dir TEXT NOT NULL, src_name TEXT NOT NULL, src_checksum TEXT NOT NULL,
//!        dst_dir TEXT NOT NULL, dst_name TEXT NOT NULL, dst_checksum TEXT NOT NULL,
//!        unit_id TEXT);
//! ```
//!
//! A file's identity is `(dir, checksum)`. Only in-place renames are
//! recorded (copy is not). Undo creates a new session whose `undo_of`
//! points back to the session being reversed. `copies` is an append-only
//! log; undo of a copy is intentionally not supported.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    subtitle_dir TEXT,
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
CREATE TABLE IF NOT EXISTS copies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER REFERENCES sessions(id),
    at INTEGER NOT NULL,
    src_dir TEXT NOT NULL,
    src_name TEXT NOT NULL,
    src_checksum TEXT NOT NULL,
    dst_dir TEXT NOT NULL,
    dst_name TEXT NOT NULL,
    dst_checksum TEXT NOT NULL,
    unit_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_renames_identity ON renames(dir, checksum, at);
CREATE INDEX IF NOT EXISTS idx_renames_session ON renames(session_id);
CREATE INDEX IF NOT EXISTS idx_copies_dst     ON copies(dst_dir, dst_checksum);
CREATE INDEX IF NOT EXISTS idx_copies_src     ON copies(src_dir, src_checksum);
CREATE INDEX IF NOT EXISTS idx_copies_session ON copies(session_id);
";

/// Schema version. Older databases (version < 4) predate the
/// checksum/append-only model and are rebuilt from scratch; v3 just
/// needs the `working_dir` → `subtitle_dir` rename and the new `copies`
/// table.
#[allow(dead_code)] // referenced by future migration branches
const CURRENT_VERSION: i64 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: i64,
    pub created_at: i64,
    pub subtitle_dir: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyRecord {
    pub id: i64,
    pub session_id: Option<i64>,
    pub at: i64,
    pub src_dir: String,
    pub src_name: String,
    pub src_checksum: String,
    pub dst_dir: String,
    pub dst_name: String,
    pub dst_checksum: String,
    pub unit_id: Option<String>,
}

/// Default database location: `dirs::data_dir()/subtitle-renamer/state.db`.
pub fn default_state_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("subtitle-renamer").join("state.db")
}

#[derive(Debug)]
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open (or create) the database at `path`, migrating/recreating the
    /// schema as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state db parent {}", parent.display()))?;
        }
        let conn = Connection::open(Self::resolve_db_path(path)?)
            .with_context(|| format!("open state db {}", path.display()))?;
        Self::migrate(&conn)?;
        conn.execute_batch(SCHEMA).context("init state schema")?;
        Ok(Self { conn })
    }

    /// If `state.db` is missing but a legacy `app.db` exists alongside it,
    /// rename the legacy file in place so the user's history survives the
    /// storage rename. If `state.db` already exists, or `app.db` does not
    /// exist, this is a no-op and `path` is returned unchanged.
    fn resolve_db_path(path: &Path) -> Result<&Path> {
        if !path.exists()
            && let Some(parent) = path.parent()
        {
            let legacy = parent.join("app.db");
            if legacy.exists() {
                std::fs::rename(&legacy, path).with_context(|| {
                    format!("migrate legacy db {} -> {}", legacy.display(), path.display())
                })?;
            }
        }
        Ok(path)
    }

    /// Bring `conn` up to the current schema. v0/v1 databases had a
    /// different shape and are dropped wholesale; v2 only needs
    /// `token_mappings` removed (already lands at v3); v3 → v4 renames
    /// `sessions.working_dir` → `sessions.subtitle_dir` and adds the
    /// `copies` table + its indexes. Fresh databases (version 0) get the
    /// current schema for free via `SCHEMA` after this runs.
    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read user_version")?;
        if version < 2 {
            conn.execute_batch(
                "DROP TABLE IF EXISTS operations;
                 DROP TABLE IF EXISTS sessions;
                 DROP TABLE IF EXISTS renames;
                 DROP TABLE IF EXISTS token_mappings;
                 PRAGMA user_version = 4;",
            )
            .context("migrate: drop pre-v2 state tables")?;
        } else if version == 2 {
            conn.execute_batch(
                "DROP TABLE IF EXISTS token_mappings;
                 ALTER TABLE sessions RENAME COLUMN working_dir TO subtitle_dir;
                 CREATE TABLE IF NOT EXISTS copies (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id INTEGER REFERENCES sessions(id),
                     at INTEGER NOT NULL,
                     src_dir TEXT NOT NULL,
                     src_name TEXT NOT NULL,
                     src_checksum TEXT NOT NULL,
                     dst_dir TEXT NOT NULL,
                     dst_name TEXT NOT NULL,
                     dst_checksum TEXT NOT NULL,
                     unit_id TEXT
                 );
                 CREATE INDEX IF NOT EXISTS idx_copies_dst     ON copies(dst_dir, dst_checksum);
                 CREATE INDEX IF NOT EXISTS idx_copies_src     ON copies(src_dir, src_checksum);
                 CREATE INDEX IF NOT EXISTS idx_copies_session ON copies(session_id);
                 PRAGMA user_version = 4;",
            )
            .context("migrate: v2 -> v4 (drop token_mappings, rename column, add copies)")?;
        } else if version == 3 {
            conn.execute_batch(
                "ALTER TABLE sessions RENAME COLUMN working_dir TO subtitle_dir;
                 CREATE TABLE IF NOT EXISTS copies (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id INTEGER REFERENCES sessions(id),
                     at INTEGER NOT NULL,
                     src_dir TEXT NOT NULL,
                     src_name TEXT NOT NULL,
                     src_checksum TEXT NOT NULL,
                     dst_dir TEXT NOT NULL,
                     dst_name TEXT NOT NULL,
                     dst_checksum TEXT NOT NULL,
                     unit_id TEXT
                 );
                 CREATE INDEX IF NOT EXISTS idx_copies_dst     ON copies(dst_dir, dst_checksum);
                 CREATE INDEX IF NOT EXISTS idx_copies_src     ON copies(src_dir, src_checksum);
                 CREATE INDEX IF NOT EXISTS idx_copies_session ON copies(session_id);
                 PRAGMA user_version = 4;",
            )
            .context("migrate: v3 -> v4 (rename working_dir, add copies)")?;
        }
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_state_path())
    }

    /// Create a new session and return its id. `subtitle_dir` is the
    /// directory of the renamed subtitles (None when the apply touched no
    /// subtitles — should be rare in practice).
    pub fn create_session(&self, subtitle_dir: Option<&Path>) -> Result<i64> {
        let subtitle_dir = subtitle_dir.map(|p| p.display().to_string());
        self.insert_session(None, subtitle_dir.as_deref())
    }

    /// Create a session that reverses `undo_of` and return its id. The
    /// new session inherits the `subtitle_dir` of the session it reverses
    /// (a `NULL` original yields a `NULL` undo session). The signature
    /// takes only `undo_of` because the correct `subtitle_dir` is derived
    /// here — historical bugs from letting callers pass `None` is why
    /// this is now opinionated.
    pub fn create_undo_session(&self, undo_of: i64) -> Result<i64> {
        let inherited: Option<String> = self
            .conn
            .query_row("SELECT subtitle_dir FROM sessions WHERE id = ?1", params![undo_of], |row| {
                row.get(0)
            })
            .optional()
            .context("read original session subtitle_dir")?
            .flatten();
        self.insert_session(Some(undo_of), inherited.as_deref())
    }

    fn insert_session(&self, undo_of: Option<i64>, subtitle_dir: Option<&str>) -> Result<i64> {
        let now = now_epoch();
        self.conn.execute(
            "INSERT INTO sessions (created_at, subtitle_dir, undo_of) VALUES (?1, ?2, ?3)",
            params![now, subtitle_dir, undo_of],
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

    /// Append a copy event to the `copies` log. The log is append-only
    /// and not undoable by design (reverting a copy would require deleting
    /// the destination file, which is too dangerous to expose).
    #[allow(clippy::too_many_arguments)]
    pub fn record_copy(
        &self,
        session_id: Option<i64>,
        src_dir: &Path,
        src_name: &str,
        src_checksum: &str,
        dst_dir: &Path,
        dst_name: &str,
        dst_checksum: &str,
        unit_id: Option<&str>,
    ) -> Result<i64> {
        let at = now_epoch();
        self.conn.execute(
            "INSERT INTO copies
                 (session_id, at, src_dir, src_name, src_checksum,
                  dst_dir, dst_name, dst_checksum, unit_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                at,
                src_dir.display().to_string(),
                src_name,
                src_checksum,
                dst_dir.display().to_string(),
                dst_name,
                dst_checksum,
                unit_id,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List sessions in reverse chronological order (newest first).
    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, subtitle_dir, undo_of FROM sessions ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    subtitle_dir: row.get(2)?,
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

    /// All copy events matching either side of a `(dir, checksum)`
    /// identity, in time order. Mirrors [`Self::timeline_for`].
    pub fn copies_for_identity(&self, dir: &Path, checksum: &str) -> Result<Vec<CopyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, at,
                    src_dir, src_name, src_checksum,
                    dst_dir, dst_name, dst_checksum, unit_id
             FROM copies
             WHERE (dst_dir = ?1 AND dst_checksum = ?2)
                OR (src_dir = ?1 AND src_checksum = ?2)
             ORDER BY at ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![dir.display().to_string(), checksum], row_to_copy)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All copy events touching any of `dirs` on either side, ordered
    /// newest-first. Used by the History popup's scope filter.
    pub fn copies_in_dirs(&self, dirs: &[PathBuf]) -> Result<Vec<CopyRecord>> {
        if dirs.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", dirs.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, session_id, at,
                    src_dir, src_name, src_checksum,
                    dst_dir, dst_name, dst_checksum, unit_id
             FROM copies
             WHERE src_dir IN ({placeholders}) OR dst_dir IN ({placeholders})
             ORDER BY at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_vec: Vec<String> = dirs.iter().map(|p| p.display().to_string()).collect();
        let second = params_vec.clone();
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params_vec.iter().chain(second.iter())),
                row_to_copy,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Extension trait that adds `optional()` to `Result<T>` so a missing row
/// reads as `Ok(None)` without pulling in the full `rusqlite::OptionalExtension`
/// trait bound noise at every call site.
trait OptionalRow<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
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

fn row_to_copy(row: &rusqlite::Row<'_>) -> rusqlite::Result<CopyRecord> {
    Ok(CopyRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        at: row.get(2)?,
        src_dir: row.get(3)?,
        src_name: row.get(4)?,
        src_checksum: row.get(5)?,
        dst_dir: row.get(6)?,
        dst_name: row.get(7)?,
        dst_checksum: row.get(8)?,
        unit_id: row.get(9)?,
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

    fn tmp_db() -> (StateDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_state_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        (StateDb::open(&path).unwrap(), path)
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
        let undo = db.create_undo_session(original).unwrap();
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
        let db = StateDb::open(&path).unwrap();
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

    #[test]
    fn open_drops_v2_token_mappings_but_preserves_sessions() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_v2_mig_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        // Lay down a v2 database: real `sessions` row plus a vestigial
        // `token_mappings` table that should be dropped on open.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 created_at INTEGER NOT NULL,
                 working_dir TEXT,
                 undo_of INTEGER REFERENCES sessions(id)
             );
             CREATE TABLE renames (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 dir TEXT NOT NULL,
                 checksum TEXT NOT NULL,
                 unit_id TEXT,
                 old_name TEXT NOT NULL,
                 new_name TEXT NOT NULL,
                 at INTEGER NOT NULL
             );
             CREATE TABLE token_mappings (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 token TEXT NOT NULL,
                 value TEXT NOT NULL,
                 var TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             INSERT INTO sessions (id, created_at, working_dir, undo_of)
                 VALUES (1, 1700000000, '/subs', NULL);
             PRAGMA user_version = 2;",
        )
        .unwrap();
        drop(conn);
        let db = StateDb::open(&path).unwrap();
        let sessions = db.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1, "v2 session row must survive migration");
        assert_eq!(sessions[0].subtitle_dir.as_deref(), Some("/subs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_v3_migrates_to_v4_with_copies() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_v3_mig_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        // Lay down a v3 database with the legacy `working_dir` column name
        // and no `copies` table; opening must rename the column and create
        // the copies table + its 3 indexes.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 created_at INTEGER NOT NULL,
                 working_dir TEXT,
                 undo_of INTEGER REFERENCES sessions(id)
             );
             CREATE TABLE renames (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 dir TEXT NOT NULL,
                 checksum TEXT NOT NULL,
                 unit_id TEXT,
                 old_name TEXT NOT NULL,
                 new_name TEXT NOT NULL,
                 at INTEGER NOT NULL
             );
             INSERT INTO sessions (id, created_at, working_dir, undo_of)
                 VALUES (1, 1700000000, '/subs', NULL);
             PRAGMA user_version = 3;",
        )
        .unwrap();
        drop(conn);

        let db = StateDb::open(&path).unwrap();

        let has_working: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'working_dir'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_working, 0, "legacy working_dir column must be gone");
        let has_subtitle: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'subtitle_dir'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_subtitle, 1, "new subtitle_dir column must exist");

        let copies_table: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='copies'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(copies_table, 1, "copies table must be created");

        let index_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND tbl_name='copies'
                   AND name IN ('idx_copies_dst', 'idx_copies_src', 'idx_copies_session')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 3, "all 3 copies indexes must exist");

        // The pre-existing session must still be queryable via the new column.
        let sessions = db.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].subtitle_dir.as_deref(), Some("/subs"));

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn record_copy_and_query_by_dst() {
        let (db, path) = tmp_db();
        let id = db
            .record_copy(
                None,
                Path::new("/subs"),
                "orig.ass",
                "cs1",
                Path::new("/videos"),
                "renamed.ass",
                "cs1",
                None,
            )
            .unwrap();
        assert!(id > 0);
        let by_dst = db.copies_for_identity(Path::new("/videos"), "cs1").unwrap();
        assert_eq!(by_dst.len(), 1);
        assert_eq!(by_dst[0].src_name, "orig.ass");
        assert_eq!(by_dst[0].dst_name, "renamed.ass");
        let by_src = db.copies_for_identity(Path::new("/subs"), "cs1").unwrap();
        assert_eq!(by_src.len(), 1);
        assert_eq!(by_src[0].id, by_dst[0].id);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn undo_session_inherits_subtitle_dir() {
        let (db, path) = tmp_db();
        let original = db.create_session(Some(Path::new("/a"))).unwrap();
        let undo = db.create_undo_session(original).unwrap();
        let sessions = db.list_sessions().unwrap();
        let undo_rec = sessions.iter().find(|s| s.id == undo).unwrap();
        assert_eq!(undo_rec.subtitle_dir.as_deref(), Some("/a"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
