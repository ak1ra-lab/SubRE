//! Filesystem execution: rename / copy per the plan, plus undo.
//!
//! This module is the ONLY place that touches the filesystem for renames
//! or copies. Errors are collected per-op and returned in [`ExecuteReport`]
//! so a partial failure does not abort the run.

use std::fs;
use std::path::{Path, PathBuf};

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
    fs::copy(&op.subtitle.path, &op.target_path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    pub action: PlannedAction,
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    pub src_mtime: i64,
    pub src_size: i64,
}

/// Snapshot of `path`'s mtime + size + content checksum, taken before
/// the rename/copy. The checksum is a SHA-256 hex digest of the file's
/// bytes at snapshot time and is used during undo to verify that the
/// destination file has not been altered since the rename/copy.
pub fn snapshot(path: &Path) -> std::io::Result<(i64, i64, String)> {
    let meta = fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let size = meta.len() as i64;
    let checksum = sha256_hex(path)?;
    Ok((mtime, size, checksum))
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

/// One historical operation read back from the sqlite history layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalOp {
    pub id: i64,
    pub action: PlannedAction,
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    pub src_mtime: i64,
    pub src_size: i64,
    /// SHA-256 hex digest of the source file at snapshot time, used to
    /// verify content integrity before undoing. Empty for legacy rows
    /// written before the checksum column existed.
    pub src_checksum: String,
    pub undone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    Ok,
    /// Destination file is missing or has changed since the operation.
    DstChanged(String),
    /// The original src path is now occupied.
    SrcOccupied(String),
    /// IO error during undo.
    Io(String),
}

/// Undo a single historical operation, validating against the recorded
/// snapshot. For rename: dst must exist with same mtime+size+checksum,
/// src must be empty; reverse-rename. For copy: dst must exist with same
/// mtime+size+checksum, then delete dst.
pub fn undo_one(op: &HistoricalOp) -> UndoOutcome {
    let dst_meta = match fs::metadata(&op.dst_path) {
        Ok(m) => m,
        Err(e) => return UndoOutcome::DstChanged(format!("dst missing: {e}")),
    };
    let dst_size = dst_meta.len() as i64;
    let dst_mtime = dst_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(-1, |d| d.as_secs() as i64);

    if dst_size != op.src_size {
        return UndoOutcome::DstChanged(format!(
            "dst size {} != snapshot {}",
            dst_size, op.src_size
        ));
    }
    if dst_mtime != op.src_mtime {
        return UndoOutcome::DstChanged(format!(
            "dst mtime {} != snapshot {}",
            dst_mtime, op.src_mtime
        ));
    }
    // Content-integrity check. If a checksum was recorded at write
    // time, recompute it on the dst and refuse to undo if the file has
    // been altered (mtime/size alone can miss silent rewrites).
    if !op.src_checksum.is_empty() {
        match sha256_hex(&op.dst_path) {
            Ok(current) if current != op.src_checksum => {
                return UndoOutcome::DstChanged(format!(
                    "dst checksum {} != snapshot {}",
                    &current[..8],
                    &op.src_checksum[..8]
                ));
            }
            Err(e) => {
                return UndoOutcome::DstChanged(format!("dst read for checksum failed: {e}"));
            }
            _ => {}
        }
    }

    match op.action {
        PlannedAction::Rename => {
            // src must be free.
            if op.src_path.exists() {
                return UndoOutcome::SrcOccupied(op.src_path.display().to_string());
            }
            if let Err(e) = fs::rename(&op.dst_path, &op.src_path) {
                return UndoOutcome::Io(e.to_string());
            }
        }
        PlannedAction::Copy => {
            // Reverse of copy = delete the dst.
            if let Err(e) = fs::remove_file(&op.dst_path) {
                return UndoOutcome::Io(e.to_string());
            }
        }
    }
    UndoOutcome::Ok
}

// ---------------------------------------------------------------------------
// Unit tests (use tempdir)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::matcher::Matcher;
    use crate::core::plan::{SuffixConfig, generate_plan};

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

    #[test]
    fn rename_same_dir() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("rename_same_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
            &SuffixConfig::default(),
            &NoExists,
            crate::core::plan::ActionMode::Auto,
        );
        let report = execute_plan(&plan).unwrap();
        assert!(report.all_ok());
        assert!(dir.join("Show - 01.ass").exists());
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_cross_dir() {
        let root = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let root = root.join("copy_cross");
        let _ = std::fs::remove_dir_all(&root);
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
            &SuffixConfig::default(),
            &NoExists,
            crate::core::plan::ActionMode::Auto,
        );
        let report = execute_plan(&plan).unwrap();
        assert!(report.all_ok());
        assert!(videos.join("Show - 01.ass").exists());
        assert!(sub.exists(), "source must be preserved");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_rename_round_trip() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("undo_rename");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("orig.ass");
        let dst = dir.join("renamed.ass");
        write(&src, b"hello");
        let (mtime, size, checksum) = snapshot(&src).unwrap();
        std::fs::rename(&src, &dst).unwrap();
        let hist = HistoricalOp {
            id: 0,
            action: PlannedAction::Rename,
            src_path: src.clone(),
            dst_path: dst.clone(),
            src_mtime: mtime,
            src_size: size,
            src_checksum: checksum,
            undone: false,
        };
        assert_eq!(undo_one(&hist), UndoOutcome::Ok);
        assert!(src.exists());
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_rename_rejects_when_src_occupied() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("undo_occupied");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("orig.ass");
        let dst = dir.join("renamed.ass");
        write(&src, b"a");
        let (mtime, size, checksum) = snapshot(&src).unwrap();
        std::fs::rename(&src, &dst).unwrap();
        // Now occupy the src path with another file.
        write(&src, b"intruder");
        let hist = HistoricalOp {
            id: 0,
            action: PlannedAction::Rename,
            src_path: src.clone(),
            dst_path: dst.clone(),
            src_mtime: mtime,
            src_size: size,
            src_checksum: checksum,
            undone: false,
        };
        match undo_one(&hist) {
            UndoOutcome::SrcOccupied(_) => {}
            other => panic!("expected SrcOccupied, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_rename_rejects_when_dst_modified() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("undo_modified");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("orig.ass");
        let dst = dir.join("renamed.ass");
        write(&src, b"a");
        let (mtime, size, checksum) = snapshot(&src).unwrap();
        std::fs::rename(&src, &dst).unwrap();
        // Modify the dst after the rename.
        std::fs::write(&dst, b"changed").unwrap();
        let hist = HistoricalOp {
            id: 0,
            action: PlannedAction::Rename,
            src_path: src.clone(),
            dst_path: dst.clone(),
            src_mtime: mtime,
            src_size: size,
            src_checksum: checksum,
            undone: false,
        };
        match undo_one(&hist) {
            UndoOutcome::DstChanged(_) => {}
            other => panic!("expected DstChanged, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_copy_deletes_dst() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("undo_copy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.ass");
        let dst = dir.join("dst.ass");
        write(&src, b"hi");
        let (mtime, size, checksum) = snapshot(&src).unwrap();
        std::fs::copy(&src, &dst).unwrap();
        let hist = HistoricalOp {
            id: 0,
            action: PlannedAction::Copy,
            src_path: src.clone(),
            dst_path: dst.clone(),
            src_mtime: mtime,
            src_size: size,
            src_checksum: checksum,
            undone: false,
        };
        assert_eq!(undo_one(&hist), UndoOutcome::Ok);
        assert!(src.exists());
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Replace the dst's bytes but preserve size and (where possible)
    /// mtime. The checksum recorded at snapshot time should still differ
    /// and undo should be refused — proving the checksum catches content
    /// tampering that mtime+size would miss.
    #[test]
    fn undo_rejects_on_content_tamper_with_same_size() {
        let dir = std::env::temp_dir().join(format!("sr_test_{}", std::process::id()));
        let dir = dir.join("undo_content_tamper");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("orig.ass");
        let dst = dir.join("renamed.ass");
        write(&src, b"original content here!"); // 22 bytes
        let (mtime, size, checksum) = snapshot(&src).unwrap();
        assert!(size > 0);
        assert!(!checksum.is_empty());
        std::fs::rename(&src, &dst).unwrap();
        // Tamper: write a different payload of the SAME length so size
        // stays equal; explicitly set mtime back to the original to also
        // defeat the mtime check.
        std::fs::write(&dst, b"different bytes here!!").unwrap(); // 22 bytes
        filetime_set(&dst, mtime);
        let hist = HistoricalOp {
            id: 0,
            action: PlannedAction::Rename,
            src_path: src.clone(),
            dst_path: dst.clone(),
            src_mtime: mtime,
            src_size: size,
            src_checksum: checksum,
            undone: false,
        };
        match undo_one(&hist) {
            UndoOutcome::DstChanged(msg) => {
                assert!(msg.contains("checksum"), "expected checksum rejection, got: {msg}");
            }
            other => panic!("expected DstChanged, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Force a file's mtime to a specific unix-seconds value.
    fn filetime_set(path: &Path, secs: i64) {
        let ft = filetime::FileTime::from_unix_time(secs, 0);
        filetime::set_file_mtime(path, ft).expect("set mtime");
    }
}
