//! Filesystem execution: rename / copy per the plan, plus session-level rollback.
//!
//! This module is the ONLY place that touches the filesystem for renames
//! or copies. Errors are collected per-op and returned in [`ExecuteReport`]
//! so a partial failure does not abort the run.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::plan::{Plan, PlannedAction, PlannedOp};

#[derive(Debug, Error)]
pub enum ExecuteError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("plan is empty")]
    Empty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpOutcome {
    pub op_index: usize,
    pub success: bool,
    pub error: Option<String>,
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    pub action: PlannedAction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecuteReport {
    pub outcomes: Vec<OpOutcome>,
}

impl ExecuteReport {
    pub fn all_ok(&self) -> bool {
        self.outcomes.iter().all(|o| o.success)
    }
}

/// Execute every op in `plan`. Returns one `OpOutcome` per op in input order.
pub fn execute_plan(plan: &Plan) -> Result<ExecuteReport, ExecuteError> {
    if plan.ops.is_empty() {
        return Err(ExecuteError::Empty);
    }
    let mut outcomes = Vec::with_capacity(plan.ops.len());
    for (idx, op) in plan.ops.iter().enumerate() {
        let outcome = match op.action {
            PlannedAction::Rename => do_rename(op),
            PlannedAction::Copy => do_copy(op),
        };
        outcomes.push(OpOutcome {
            op_index: idx,
            success: outcome.is_ok(),
            error: outcome.err().map(|e| e.to_string()),
            src_path: op.subtitle.path.clone(),
            dst_path: op.target_path.clone(),
            action: op.action,
        });
    }
    Ok(ExecuteReport { outcomes })
}

fn do_rename(op: &PlannedOp) -> Result<(), ExecuteError> {
    fs::rename(&op.subtitle.path, &op.target_path)?;
    Ok(())
}

fn do_copy(op: &PlannedOp) -> Result<(), ExecuteError> {
    // Copy mode where the target basename already equals the source name
    // (e.g. a re-applied plan) degenerates to src == dst; `fs::copy` onto
    // itself is unreliable (EINVAL or truncation), so count it as a
    // successful no-op instead.
    if op.subtitle.path == op.target_path {
        return Ok(());
    }
    fs::copy(&op.subtitle.path, &op.target_path)?;
    Ok(())
}

/// SHA-256 of the file at `path`, returned as a 64-char lowercase hex
/// string. Computed in streaming fashion so memory use stays flat for
/// large files.
pub fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use std::fmt::Write;
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for byte in digest {
        let _ = write!(s, "{byte:02x}");
    }
    Ok(s)
}

/// Memoized SHA-256 cache keyed by `(path, mtime, size)` so that unchanged
/// files are not re-read and re-hashed on every refresh.
#[derive(Debug, Default)]
pub struct ChecksumCache {
    entries: HashMap<PathBuf, (SystemTime, u64, String)>,
}

impl ChecksumCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached SHA-256 for `path`, recomputing (and caching) it
    /// only when the file's `(mtime, size)` changed since the last call.
    /// Returns `None` when the file cannot be read or its metadata fetched.
    pub fn cached_sha256(&mut self, path: &Path) -> Option<String> {
        let meta = fs::metadata(path).ok()?;
        let mtime = meta.modified().ok()?;
        let size = meta.len();
        if let Some((cached_mtime, cached_size, checksum)) = self.entries.get(path)
            && *cached_mtime == mtime
            && *cached_size == size
        {
            return Some(checksum.clone());
        }
        let checksum = sha256_hex(path).ok()?;
        self.entries.insert(path.to_path_buf(), (mtime, size, checksum.clone()));
        Some(checksum)
    }
}

// ---------------------------------------------------------------------------
// Rollback (session-level undo)
// ---------------------------------------------------------------------------

/// One file in a rollback unit: the identity `(dir, checksum)` plus the
/// name to roll back from (`new_name`, current) and to (`old_name`).
#[derive(Debug, Clone)]
pub struct RollbackItem {
    pub dir: PathBuf,
    pub checksum: String,
    /// The name the file should be renamed back to (original name).
    pub old_name: String,
    /// The name the file currently has.
    pub new_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackOutcome {
    Ok,
    /// A member's `new_name` file is missing or its content differs.
    DstChanged(String),
    /// A member's `old_name` path is now occupied.
    SrcOccupied(String),
    /// IO error during rollback.
    Io(String),
}

impl std::fmt::Display for RollbackOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::DstChanged(detail) => write!(f, "dest changed or missing: {detail}"),
            Self::SrcOccupied(path) => write!(f, "original name now occupied: {path}"),
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

/// Roll back a whole unit atomically: first validate every member (the
/// file is still at `new_name` with a matching checksum, and `old_name`
/// is free), then rename each member back to `old_name`. Any validation
/// failure skips the entire unit.
pub fn rollback_unit(items: &[RollbackItem]) -> RollbackOutcome {
    for item in items {
        let new_path = item.dir.join(&item.new_name);
        let old_path = item.dir.join(&item.old_name);
        match sha256_hex(&new_path) {
            Ok(current) if current == item.checksum => {}
            Ok(current) => {
                return RollbackOutcome::DstChanged(format!(
                    "dst checksum {} != recorded {}",
                    &current[..8],
                    &item.checksum[..8]
                ));
            }
            Err(e) => {
                return RollbackOutcome::DstChanged(format!("dst missing: {e}"));
            }
        }
        if old_path.exists() {
            return RollbackOutcome::SrcOccupied(old_path.display().to_string());
        }
    }
    for item in items {
        let new_path = item.dir.join(&item.new_name);
        let old_path = item.dir.join(&item.old_name);
        if let Err(e) = fs::rename(&new_path, &old_path) {
            return RollbackOutcome::Io(e.to_string());
        }
    }
    RollbackOutcome::Ok
}

// ---------------------------------------------------------------------------
// Unit tests (use tempdir)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::matcher::Matcher;
    use crate::core::plan::{NamingConfig, generate_plan};

    struct NoExists;
    impl crate::core::plan::FsProbe for NoExists {
        fn exists(&self, _: &Path) -> bool {
            false
        }
    }

    fn write(p: &Path, content: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn tmpdir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_exec_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let dir = dir.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rename_same_dir() {
        let dir = tmpdir("rename_same_dir");
        let video = dir.join("Show - 01.mkv");
        let sub = dir.join("Show.S01E01.ass");
        write(&video, b"video");
        write(&sub, b"sub");
        let m = Matcher::new();
        let v = vec![crate::core::matcher::FileEntry::from_path(video.clone())];
        let s = vec![crate::core::matcher::FileEntry::from_path(sub.clone())];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(
            &r,
            &NamingConfig::default(),
            &NoExists,
            crate::core::plan::ActionMode::Rename,
        )
        .unwrap();
        let report = execute_plan(&plan).unwrap();
        assert!(report.all_ok());
        assert!(dir.join("Show - 01.ass").exists());
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_cross_dir() {
        let root = tmpdir("copy_cross");
        let videos = root.join("videos");
        let subs = root.join("subs");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::create_dir_all(&subs).unwrap();
        let video = videos.join("Show - 01.mkv");
        let sub = subs.join("Show.S01E01.ass");
        write(&video, b"video");
        write(&sub, b"sub");
        let m = Matcher::new();
        let v = vec![crate::core::matcher::FileEntry::from_path(video.clone())];
        let s = vec![crate::core::matcher::FileEntry::from_path(sub.clone())];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(
            &r,
            &NamingConfig::default(),
            &NoExists,
            crate::core::plan::ActionMode::Copy,
        )
        .unwrap();
        let report = execute_plan(&plan).unwrap();
        assert!(report.all_ok());
        assert!(videos.join("Show - 01.ass").exists());
        assert!(sub.exists(), "source must be preserved");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_same_dir_preserves_source() {
        let dir = tmpdir("copy_same_dir");
        let video = dir.join("Show - 01.mkv");
        let sub = dir.join("Show.S01E01.ass");
        write(&video, b"video");
        write(&sub, b"sub");
        let m = Matcher::new();
        let v = vec![crate::core::matcher::FileEntry::from_path(video.clone())];
        let s = vec![crate::core::matcher::FileEntry::from_path(sub.clone())];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(
            &r,
            &NamingConfig::default(),
            &NoExists,
            crate::core::plan::ActionMode::Copy,
        )
        .unwrap();
        assert_eq!(plan.ops[0].action, PlannedAction::Copy);
        let report = execute_plan(&plan).unwrap();
        assert!(report.all_ok());
        // Copy semantics hold even in the same directory: the source file
        // stays and a distinct target appears beside it.
        assert!(sub.exists(), "source must be preserved");
        let dst = dir.join("Show - 01.ass");
        assert!(dst.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"sub");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn self_copy_counts_ok_without_io() {
        let root = tmpdir("self_copy_ok");
        let f = root.join("same.ass");
        write(&f, b"payload");
        let op = PlannedOp {
            video: None,
            subtitle: crate::core::matcher::FileEntry::from_path(f.clone()),
            target_path: f.clone(),
            target_basename: "same.ass".into(),
            action: PlannedAction::Copy,
            conflicts: Vec::new(),
            unit_id: None,
        };
        let report = execute_plan(&Plan { ops: vec![op] }).unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert!(report.outcomes[0].success);
        assert!(report.outcomes[0].error.is_none());
        assert_eq!(report.outcomes[0].action, PlannedAction::Copy);
        // A failed `fs::copy` onto itself could truncate the file; the
        // guard must leave it byte-for-byte intact.
        assert_eq!(std::fs::read(&f).unwrap(), b"payload");
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 7);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_outcome_display_is_human_readable() {
        assert_eq!(RollbackOutcome::Ok.to_string(), "ok");
        assert_eq!(
            RollbackOutcome::DstChanged("dst checksum deadbeef != recorded cafebeef".into())
                .to_string(),
            "dest changed or missing: dst checksum deadbeef != recorded cafebeef"
        );
        assert_eq!(
            RollbackOutcome::SrcOccupied("/subs/orig.ass".into()).to_string(),
            "original name now occupied: /subs/orig.ass"
        );
        assert_eq!(
            RollbackOutcome::Io("permission denied".into()).to_string(),
            "io error: permission denied"
        );
    }

    #[test]
    fn rollback_unit_round_trip() {
        let dir = tmpdir("rollback_ok");
        let old = dir.join("orig.ass");
        let new = dir.join("renamed.ass");
        write(&old, b"hello");
        let checksum = sha256_hex(&old).unwrap();
        std::fs::rename(&old, &new).unwrap();
        let item = RollbackItem {
            dir: dir.clone(),
            checksum,
            old_name: "orig.ass".into(),
            new_name: "renamed.ass".into(),
        };
        assert_eq!(rollback_unit(&[item]), RollbackOutcome::Ok);
        assert!(old.exists());
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_unit_rejects_modified_dst() {
        let dir = tmpdir("rollback_modified");
        let old = dir.join("orig.ass");
        let new = dir.join("renamed.ass");
        write(&old, b"hello");
        let checksum = sha256_hex(&old).unwrap();
        std::fs::rename(&old, &new).unwrap();
        std::fs::write(&new, b"changed").unwrap();
        let item = RollbackItem {
            dir: dir.clone(),
            checksum,
            old_name: "orig.ass".into(),
            new_name: "renamed.ass".into(),
        };
        match rollback_unit(&[item]) {
            RollbackOutcome::DstChanged(_) => {}
            other => panic!("expected DstChanged, got {other:?}"),
        }
        assert!(new.exists(), "nothing should be renamed on failure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_unit_rejects_occupied_old_name() {
        let dir = tmpdir("rollback_occupied");
        let old = dir.join("orig.ass");
        let new = dir.join("renamed.ass");
        write(&old, b"a");
        let checksum = sha256_hex(&old).unwrap();
        std::fs::rename(&old, &new).unwrap();
        write(&old, b"intruder");
        let item = RollbackItem {
            dir: dir.clone(),
            checksum,
            old_name: "orig.ass".into(),
            new_name: "renamed.ass".into(),
        };
        match rollback_unit(&[item]) {
            RollbackOutcome::SrcOccupied(_) => {}
            other => panic!("expected SrcOccupied, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_unit_skips_whole_unit_when_one_member_fails() {
        let dir = tmpdir("rollback_unit_fail");
        let idx_old = dir.join("show.idx");
        let sub_old = dir.join("show.sub");
        let idx_new = dir.join("Show - 01.idx");
        let sub_new = dir.join("Show - 01.sub");
        write(&idx_old, b"idx");
        write(&sub_old, b"sub");
        let idx_checksum = sha256_hex(&idx_old).unwrap();
        let sub_checksum = sha256_hex(&sub_old).unwrap();
        std::fs::rename(&idx_old, &idx_new).unwrap();
        std::fs::rename(&sub_old, &sub_new).unwrap();
        // Tamper with the .sub member only.
        std::fs::write(&sub_new, b"tampered").unwrap();
        let items = vec![
            RollbackItem {
                dir: dir.clone(),
                checksum: idx_checksum,
                old_name: "show.idx".into(),
                new_name: "Show - 01.idx".into(),
            },
            RollbackItem {
                dir: dir.clone(),
                checksum: sub_checksum,
                old_name: "show.sub".into(),
                new_name: "Show - 01.sub".into(),
            },
        ];
        match rollback_unit(&items) {
            RollbackOutcome::DstChanged(_) => {}
            other => panic!("expected DstChanged, got {other:?}"),
        }
        // Neither member may be renamed (whole unit skipped).
        assert!(idx_new.exists());
        assert!(sub_new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_cache_reuses_unchanged_file() {
        let dir = tmpdir("checksum_cache_hit");
        let f = dir.join("a.ass");
        write(&f, b"hello");
        let mut cache = ChecksumCache::new();
        let first = cache.cached_sha256(&f).unwrap();
        let second = cache.cached_sha256(&f).unwrap();
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_cache_invalidates_on_size_change() {
        let dir = tmpdir("checksum_cache_size");
        let f = dir.join("b.ass");
        write(&f, b"hello");
        let mut cache = ChecksumCache::new();
        let first = cache.cached_sha256(&f).unwrap();
        write(&f, b"hello world");
        let second = cache.cached_sha256(&f).unwrap();
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_cache_missing_file_returns_none() {
        let dir = tmpdir("checksum_cache_missing");
        let f = dir.join("missing.ass");
        let mut cache = ChecksumCache::new();
        assert!(cache.cached_sha256(&f).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
