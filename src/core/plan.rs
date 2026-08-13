//! Rename-plan generation.
//!
//! `generate_plan` is a pure function: given a [`MatchResult`] and a
//! [`SuffixConfig`], it produces a [`Plan`] — a list of [`PlannedOp`]s with
//! each op tagged with any [`Conflict`]s. A "filesystem probe" trait lets the
//! caller (production code or tests) plug in a cheap existence check for
//! the target path, used to flag conflicts when the target file already
//! exists on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::matcher::{FileEntry, MatchResult};
use super::parse::detect_language_token;

/// What suffix to apply to a subtitle when building its target filename.
///
/// Resolution order (per design D4):
///   1. Token mapping table (`token_map`).
///   2. Auto-detected language token from the stem (only if
///      `auto_extract_language_token` is enabled — default OFF).
///   3. Global suffix string.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SuffixConfig {
    /// Global suffix appended between the video main-name and the subtitle
    /// extension. May be empty.
    pub global: String,
    /// Map from detected language token (lower-case) to canonical suffix
    /// (without the leading dot, e.g. "zh-Hans").
    pub token_map: HashMap<String, String>,
    /// When true, an unconfigured language token in the subtitle stem is
    /// used as the default suffix. Default is false so that the "no
    /// suffix configured" plan stays clean.
    #[serde(default)]
    pub auto_extract_language_token: bool,
}

impl SuffixConfig {
    /// Resolve the effective suffix for `subtitle`.
    pub fn resolve_for(&self, subtitle: &FileEntry) -> String {
        // 1. Mapping table (look up the auto-detected token, if any).
        if let Some(tok) = detect_language_token(&subtitle.stem)
            && let Some(mapped) = self.token_map.get(&tok)
        {
            return format!(".{}", mapped.trim_start_matches('.'));
        }
        // 2. Language token auto-extraction (opt-in).
        if self.auto_extract_language_token
            && let Some(tok) = detect_language_token(&subtitle.stem)
        {
            return format!(".{tok}");
        }
        // 3. Global suffix.
        let g = self.global.trim();
        if g.is_empty() { String::new() } else { format!(".{}", g.trim_start_matches('.')) }
    }
}

/// One planned filesystem operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedOp {
    /// The paired video entry (None for an unmatched subtitle).
    pub video: Option<FileEntry>,
    /// The subtitle being renamed/copied.
    pub subtitle: FileEntry,
    /// The fully-resolved target path (including directory and extension).
    pub target_path: PathBuf,
    /// The proposed target filename (basename), useful for previewing.
    pub target_basename: String,
    /// Whether this op will rename (same directory) or copy (cross-directory).
    pub action: PlannedAction,
    /// Conflicts that apply to this op (empty if clean).
    pub conflicts: Vec<Conflict>,
    /// Logical unit id for paired `.idx` + `.sub` subtitles: both share the
    /// same `unit_id` so they get the same main name.
    pub unit_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlannedAction {
    /// Move via rename — same directory OR across directories on the same
    /// filesystem (atomic), or copy+delete across filesystems. **Deletes
    /// the source file.** Reversed by undo with the same validation as
    /// copy.
    Rename,
    /// Copy the source to the target directory, leaving the source
    /// untouched. **Does not delete the source.** Reversed by undo with
    /// a delete-on-dst action.
    Copy,
}

/// Conflict markers attached to a [`PlannedOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Conflict {
    /// Two planned ops resolve to the same target path.
    DuplicateTarget(PathBuf),
    /// The target path already exists on disk (and is not part of this plan
    /// as a source).
    TargetExists(PathBuf),
    /// The requested action is not possible for this op (e.g. Rename
    /// across filesystems when the user asked for Move mode but the
    /// target directory is the same as the source — degenerate case).
    ActionUnsupported(PathBuf),
}

/// User-selectable policy for how a plan should translate cross-directory
/// pairings into filesystem operations.
///
/// The default (`Auto`) follows design D5: same directory → rename in
/// place; different directory → copy (preserve originals). Other modes
/// override that policy uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ActionMode {
    /// Same-dir Rename, cross-dir Copy. Honors design D5. **Default.**
    #[default]
    Auto,
    /// Always Copy — never touch the source file. Safe even across
    /// filesystems.
    Copy,
    /// Always Rename — move the subtitle into the video directory. The
    /// source file is deleted as part of the rename (or copy+delete if
    /// the two directories are on different filesystems).
    Move,
}

/// A complete plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub ops: Vec<PlannedOp>,
}

impl Plan {
    pub fn has_conflicts(&self) -> bool {
        self.ops.iter().any(|o| !o.conflicts.is_empty())
    }
}

/// Filesystem probe trait — lets the planner detect pre-existing target
/// files without taking a hard dependency on `std::fs`.
pub trait FsProbe {
    fn exists(&self, path: &Path) -> bool;
}

#[derive(Debug)]
pub struct StdFsProbe;

impl FsProbe for StdFsProbe {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Generate a plan for `result` using `suffix`, `probe`, and the user's
/// `action_mode`.
///
/// The plan is pure with respect to filesystem state EXCEPT for the
/// `TargetExists` conflict detection, which reads via `probe`. Tests can
/// inject a stub probe that returns false to make plan generation fully
/// pure.
pub fn generate_plan(
    result: &MatchResult,
    suffix: &SuffixConfig,
    probe: &dyn FsProbe,
    action_mode: ActionMode,
) -> Plan {
    let mut ops = Vec::new();

    // Group subtitles by stem-base so .idx + .sub share the same unit.
    let mut sources: Vec<&super::matcher::PairGroup> = Vec::new();
    for group in &result.groups {
        if group.subtitles.is_empty() {
            continue;
        }
        sources.push(group);
    }

    // First pass: compute target basenames per unit, attach unit_id.
    // A unit is the set of subtitles sharing a stem-base (filename minus
    // the final extension).
    for group in sources {
        let video = group.video.clone();
        let video_main =
            video.as_ref().map_or_else(|| group.subtitles[0].stem.clone(), |v| v.stem.clone());

        // Group the group's subtitles by stem-base (basename without last
        // extension) to detect .idx+.sub pairs.
        let mut units: HashMap<String, Vec<&FileEntry>> = HashMap::new();
        for sub in &group.subtitles {
            let base = stem_base(&sub.stem);
            units.entry(base).or_default().push(sub);
        }

        for (base, members) in units {
            let unit_id =
                if members.len() > 1 { Some(format!("{video_main}|{base}")) } else { None };

            // The "main" extension is the longest / canonical one (per
            // design: idx is the index, sub is the data — main = idx).
            // But for target naming we keep each member's own extension and
            // share the suffix + main-name.
            for sub in members {
                let effective_suffix = suffix.resolve_for(sub);
                let main_name = if effective_suffix.is_empty() {
                    video_main.clone()
                } else {
                    format!("{video_main}{effective_suffix}")
                };
                let target_basename = format!("{}.{}", main_name, sub.ext);
                let target_dir = video.as_ref().and_then(|v| v.path.parent()).map_or_else(
                    || sub.path.parent().map(std::path::Path::to_path_buf).unwrap_or_default(),
                    std::path::Path::to_path_buf,
                );
                let target_path = target_dir.join(&target_basename);

                // Pick the action based on the user's mode and the
                // src/target directory relationship.
                let same_dir = video
                    .as_ref()
                    .and_then(|v| v.path.parent())
                    .is_some_and(|vdir| Some(vdir) == sub.path.parent());
                let action = match action_mode {
                    ActionMode::Auto => {
                        if same_dir {
                            PlannedAction::Rename
                        } else {
                            PlannedAction::Copy
                        }
                    }
                    ActionMode::Copy => PlannedAction::Copy,
                    ActionMode::Move => PlannedAction::Rename,
                };

                ops.push(PlannedOp {
                    video: video.clone(),
                    subtitle: sub.clone(),
                    target_path,
                    target_basename,
                    action,
                    conflicts: Vec::new(),
                    unit_id: unit_id.clone(),
                });
            }
        }
    }

    // Second pass: detect duplicate targets within the plan itself.
    let mut by_target: HashMap<PathBuf, usize> = HashMap::new();
    for (idx, op) in ops.iter().enumerate() {
        *by_target.entry(op.target_path.clone()).or_insert(0) += 1;
        let _ = idx;
    }
    let source_paths: std::collections::HashSet<PathBuf> =
        ops.iter().map(|o| o.subtitle.path.clone()).collect();
    for op in &mut ops {
        if by_target.get(&op.target_path).copied().unwrap_or(0) > 1 {
            op.conflicts.push(Conflict::DuplicateTarget(op.target_path.clone()));
        }
        // TargetExists only counts when the target is NOT one of the
        // source paths in this plan (otherwise it would always conflict
        // with itself).
        let is_a_source = source_paths.contains(&op.target_path);
        if !is_a_source && probe.exists(&op.target_path) {
            op.conflicts.push(Conflict::TargetExists(op.target_path.clone()));
        }
    }

    Plan { ops }
}

/// Strip the last `.ext` from a stem, returning the "stem-base" used to
/// group `.idx` + `.sub` pairs.
fn stem_base(stem: &str) -> String {
    match stem.rfind('.') {
        Some(idx) if idx > 0 => stem[..idx].to_string(),
        _ => stem.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::matcher::Matcher;
    use std::path::PathBuf;

    struct NoExists;
    impl FsProbe for NoExists {
        fn exists(&self, _: &Path) -> bool {
            false
        }
    }

    fn entry(name: &str) -> FileEntry {
        FileEntry::from_path(PathBuf::from(name))
    }

    #[test]
    fn default_plan_no_suffix() {
        let m = Matcher::new();
        let v = vec![entry("/videos/[Group] Show - 01 [1080p].mkv")];
        let s = vec![entry("/subs/Show.S01E01.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops.len(), 1);
        let op = &plan.ops[0];
        assert_eq!(op.target_basename, "[Group] Show - 01 [1080p].ass");
        assert_eq!(op.action, PlannedAction::Copy); // different dirs
        assert!(!plan.has_conflicts());
    }

    #[test]
    fn global_suffix_applied() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg = SuffixConfig {
            global: "zh-Hans".into(),
            token_map: HashMap::new(),
            auto_extract_language_token: false,
        };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hans.ass");
    }

    #[test]
    fn language_token_auto_extracted() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.cht.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg = SuffixConfig { auto_extract_language_token: true, ..SuffixConfig::default() };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].target_basename, "Show - 01.cht.ass");
    }

    #[test]
    fn token_mapping_overrides_auto() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        let mut cfg = SuffixConfig::default();
        cfg.token_map.insert("chs".into(), "zh-Hans".into());
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hans.ass");
    }

    #[test]
    fn idx_sub_unit_shares_main_name() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.idx"), entry("/subs/Show.S01E01.sub")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops.len(), 2);
        let basenames: Vec<&str> = plan.ops.iter().map(|o| o.target_basename.as_str()).collect();
        assert!(basenames.contains(&"Show - 01.idx"));
        assert!(basenames.contains(&"Show - 01.sub"));
        // Same unit_id.
        let ids: Vec<_> = plan.ops.iter().map(|o| o.unit_id.clone()).collect();
        assert_eq!(ids[0], ids[1]);
        assert!(ids[0].is_some());
    }

    #[test]
    fn same_dir_uses_rename() {
        let m = Matcher::new();
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/media/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].action, PlannedAction::Rename);
    }

    #[test]
    fn duplicate_target_flagged_as_conflict() {
        let m = Matcher::new();
        // Two subtitles that would collide (both end up as
        // "Show - 01.chs.ass"): the matching logic pairs both to the same
        // video. With no suffix and both named cht they collide; with chs
        // they don't. We force a collision by giving the same language
        // token twice via direct plan manipulation is not possible from
        // match_files. Instead we craft two files that, after no-suffix
        // resolution, collide.
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/media/Show.S01E01.chs.ass"), entry("/media/Show.S01E01.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert!(plan.has_conflicts());
        assert!(
            plan.ops
                .iter()
                .any(|o| o.conflicts.iter().any(|c| matches!(c, Conflict::DuplicateTarget(_))))
        );
    }

    #[test]
    fn target_exists_flagged_via_probe() {
        struct AlwaysExists;
        impl FsProbe for AlwaysExists {
            fn exists(&self, _: &Path) -> bool {
                true
            }
        }
        let m = Matcher::new();
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &AlwaysExists, ActionMode::Auto);
        assert!(plan.has_conflicts());
        assert!(
            plan.ops
                .iter()
                .any(|o| o.conflicts.iter().any(|c| matches!(c, Conflict::TargetExists(_))))
        );
    }

    #[test]
    fn action_mode_auto_uses_rename_in_place() {
        let m = Matcher::new();
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/media/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].action, PlannedAction::Rename);
    }

    #[test]
    fn action_mode_auto_uses_copy_across_dirs() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Auto);
        assert_eq!(plan.ops[0].action, PlannedAction::Copy);
    }

    #[test]
    fn action_mode_copy_overrides_rename() {
        let m = Matcher::new();
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/media/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Copy);
        assert_eq!(plan.ops[0].action, PlannedAction::Copy);
    }

    #[test]
    fn action_mode_move_overrides_copy() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan = generate_plan(&r, &SuffixConfig::default(), &NoExists, ActionMode::Move);
        assert_eq!(plan.ops[0].action, PlannedAction::Rename);
    }
}
