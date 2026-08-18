//! Rename-plan generation.
//!
//! `generate_plan` is a pure function: given a [`MatchResult`] and a
//! [`NamingConfig`], it produces a [`Plan`] — a list of [`PlannedOp`]s with
//! each op tagged with any [`Conflict`]s. A "filesystem probe" trait lets the
//! caller (production code or tests) plug in a cheap existence check for
//! the target path, used to flag conflicts when the target file already
//! exists on disk.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::matcher::{FileEntry, MatchResult};
use super::parse::{detect_language_alias, find_boundary_token};

/// A single "token -> (value, target variable)" mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenMapping {
    pub token: String,
    pub value: String,
    /// Template variable this mapping fills; defaults to `lang`.
    #[serde(default = "default_var")]
    pub var: String,
}

fn default_var() -> String {
    "lang".to_string()
}

/// Naming configuration: a target-filename template plus token mappings that
/// fill `${...}` template variables. Replaces the former `SuffixConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NamingConfig {
    /// Target-filename template. Empty means the default `${video}.${ext}`.
    #[serde(default)]
    pub template: String,
    /// Fill `${lang}` from built-in aliases when no explicit mapping does.
    #[serde(default)]
    pub auto_fill_lang: bool,
    /// Case-sensitive token matching (default: case-insensitive).
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default, rename = "mapping")]
    pub mappings: Vec<TokenMapping>,
}

/// Default template: video main-name + subtitle extension.
pub const DEFAULT_TEMPLATE: &str = "${video}.${ext}";

impl NamingConfig {
    /// The template actually used, falling back to [`DEFAULT_TEMPLATE`].
    pub fn effective_template(&self) -> &str {
        if self.template.trim().is_empty() { DEFAULT_TEMPLATE } else { self.template.trim() }
    }

    /// Resolve template variables for `subtitle` into a `var -> value` map.
    ///
    /// Explicit mappings are matched with boundary + longest-token-first;
    /// among mappings sharing a var, the longest token wins (ties broken by
    /// earliest occurrence). If `${lang}` is still unset and `auto_fill_lang`
    /// is on, built-in aliases provide a fallback.
    pub fn resolve_vars(&self, subtitle: &FileEntry) -> HashMap<String, String> {
        let stem = &subtitle.stem;
        let mut vars: HashMap<String, String> = HashMap::new();

        // Track the best (value, token_len, first_pos) per var across all
        // explicit mappings that match the stem.
        let mut best: HashMap<&str, (&str, usize, usize)> = HashMap::new();
        for m in &self.mappings {
            if m.token.is_empty() || m.value.is_empty() || m.var.is_empty() {
                continue;
            }
            let Some(pos) = find_boundary_token(stem, &m.token, self.case_sensitive) else {
                continue;
            };
            let len = m.token.chars().count();
            let replace = match best.get(m.var.as_str()) {
                None => true,
                Some(&(_, blen, bpos)) => len > blen || (len == blen && pos < bpos),
            };
            if replace {
                best.insert(m.var.as_str(), (m.value.as_str(), len, pos));
            }
        }
        for (var, (value, _, _)) in best {
            vars.insert(var.to_string(), value.to_string());
        }

        if !vars.contains_key("lang")
            && self.auto_fill_lang
            && let Some(lang) = detect_language_alias(stem, self.case_sensitive)
        {
            vars.insert("lang".to_string(), lang);
        }

        vars
    }
}

/// Render `template`, substituting `${video}` / `${ext}` and any `${var}`
/// present in `vars`. An empty variable collapses the preceding run of
/// separator characters (`.`, `-`, `_`), and leading/trailing separators are
/// trimmed, so an empty variable never leaves a dangling separator.
pub fn render_template<S: std::hash::BuildHasher>(
    template: &str,
    vars: &HashMap<String, String, S>,
    video_stem: &str,
    ext: &str,
) -> String {
    fn is_sep(c: char) -> bool {
        matches!(c, '.' | '-' | '_')
    }

    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == '}') else {
                out.push(chars[i]);
                i += 1;
                continue;
            };
            let name: String = chars[i + 2..close].iter().collect();
            let value: Option<&str> = match name.as_str() {
                "video" => Some(video_stem),
                "ext" => Some(ext),
                other => vars.get(other).map(String::as_str),
            };
            match value {
                Some(v) if !v.is_empty() => out.push_str(v),
                _ => {
                    while out.chars().next_back().is_some_and(is_sep) {
                        out.pop();
                    }
                }
            }
            i = close + 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out.trim_matches(|c: char| is_sep(c)).to_string()
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
    /// Move via in-place rename (same directory). **Deletes the source
    /// file.** Reversed by undo.
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
    /// The requested action is not possible for this op. Reserved for
    /// future action constraints; currently unused.
    ActionUnsupported(PathBuf),
}

/// User-selectable policy: `Rename` = in-place rename in `subtitle_dir`;
/// `Copy` = copy to `video_dir`, source untouched. Default after migration
/// from v3's `Auto` is `Rename` (the safer cross-directory choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ActionMode {
    /// In-place rename inside the subtitle's directory. **Default.**
    #[default]
    Rename,
    /// Copy the source to the video's directory, leaving the source
    /// untouched. Safe even across filesystems.
    Copy,
}

impl<'de> Deserialize<'de> for ActionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ActionMode;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("`Rename` or `Copy` (legacy `Auto`/`Move` map to `Rename`)")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "Rename" | "Auto" | "Move" => Ok(ActionMode::Rename),
                    "Copy" => Ok(ActionMode::Copy),
                    other => Err(E::custom(format!("unknown action mode `{other}`"))),
                }
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Errors emitted by [`generate_plan`].
#[derive(Debug)]
pub enum PlanError {
    /// `Copy` mode requires a paired video to derive the target directory.
    /// `op_index` indexes into `Plan::ops`.
    NoVideoForCopy { op_index: usize },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoVideoForCopy { op_index } => {
                write!(f, "Copy mode requires paired video (op #{op_index})")
            }
        }
    }
}

impl std::error::Error for PlanError {}

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

/// Generate a plan for `result` using `naming`, `probe`, and the user's
/// `action_mode`.
///
/// The plan is pure with respect to filesystem state EXCEPT for the
/// `TargetExists` conflict detection, which reads via `probe`. Tests can
/// inject a stub probe that returns false to make plan generation fully
/// pure.
pub fn generate_plan(
    result: &MatchResult,
    naming: &NamingConfig,
    probe: &dyn FsProbe,
    action_mode: ActionMode,
) -> Result<Plan, PlanError> {
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
        // extension) to detect .idx+.sub pairs. A BTreeMap keeps the units
        // (and thus the plan ops) in deterministic, cross-group-consistent
        // lexicographic order.
        let mut units: BTreeMap<String, Vec<&FileEntry>> = BTreeMap::new();
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
            // share the template-derived main-name.
            for sub in members {
                let vars = naming.resolve_vars(sub);
                let target_basename =
                    render_template(naming.effective_template(), &vars, &video_main, &sub.ext);
                let target_dir = match action_mode {
                    ActionMode::Rename => {
                        sub.path.parent().map(std::path::Path::to_path_buf).unwrap_or_default()
                    }
                    ActionMode::Copy => match video.as_ref().and_then(|v| v.path.parent()) {
                        Some(d) => d.to_path_buf(),
                        None => {
                            return Err(PlanError::NoVideoForCopy { op_index: ops.len() });
                        }
                    },
                };
                let target_path = target_dir.join(&target_basename);

                // Pick the action based on whether the target directory
                // matches the source directory. `Rename` mode lands in the
                // subtitle's own directory (so `same_dir` is always true),
                // but we keep the same derivation so the structural rule
                // is expressed in one place.
                let same_dir = target_path.parent() == sub.path.parent();
                let action = if same_dir { PlannedAction::Rename } else { PlannedAction::Copy };

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

    Ok(Plan { ops })
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
    fn plan_ops_sorted_by_subtitle_stem() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        // Deliberately out-of-order ingestion; stems have no internal dots so
        // each subtitle is its own unit.
        let s = vec![
            entry("/subs/Show 01 _z.ass"),
            entry("/subs/Show 01 _a.ass"),
            entry("/subs/Show 01 _m.ass"),
        ];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
        let stems: Vec<&str> = plan.ops.iter().map(|o| o.subtitle.stem.as_str()).collect();
        assert_eq!(stems, vec!["Show 01 _a", "Show 01 _m", "Show 01 _z"]);
    }

    #[test]
    fn default_plan_no_suffix() {
        let m = Matcher::new();
        let v = vec![entry("/videos/[Group] Show - 01 [1080p].mkv")];
        let s = vec![entry("/subs/Show.S01E01.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops.len(), 1);
        let op = &plan.ops[0];
        assert_eq!(op.target_basename, "[Group] Show - 01 [1080p].ass");
        assert_eq!(op.action, PlannedAction::Rename); // target in subtitle dir
        assert!(!plan.has_conflicts());
    }

    #[test]
    fn template_with_literal_suffix() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg =
            NamingConfig { template: "${video}.zh-Hans.${ext}".into(), ..NamingConfig::default() };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hans.ass");
    }

    #[test]
    fn language_alias_fills_lang_var() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.cht.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg = NamingConfig {
            template: "${video}.${lang}.${ext}".into(),
            auto_fill_lang: true,
            ..NamingConfig::default()
        };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hant.ass");
    }

    #[test]
    fn token_mapping_fills_lang_var() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg = NamingConfig {
            template: "${video}.${lang}.${ext}".into(),
            mappings: vec![TokenMapping {
                token: "chs".into(),
                value: "zh-Hans".into(),
                var: "lang".into(),
            }],
            ..NamingConfig::default()
        };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hans.ass");
    }

    #[test]
    fn explicit_mapping_overrides_alias() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        // `_track3` is explicit -> zh-Hans; `chs` alias also present but
        // explicit wins for the same var.
        let s = vec![entry("/subs/Show.S01E01.chs_track3.ass")];
        let r = m.match_files(&v, &s, None, None);
        let cfg = NamingConfig {
            template: "${video}.${lang}.${ext}".into(),
            auto_fill_lang: true,
            mappings: vec![TokenMapping {
                token: "track3".into(),
                value: "zh-Hans".into(),
                var: "lang".into(),
            }],
            ..NamingConfig::default()
        };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].target_basename, "Show - 01.zh-Hans.ass");
    }

    #[test]
    fn empty_var_collapses_separator() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        // No group / lang mappings, so `${group}` and `${lang}` are empty.
        let cfg = NamingConfig {
            template: "${video}.${group}-${lang}.${ext}".into(),
            ..NamingConfig::default()
        };
        let plan = generate_plan(&r, &cfg, &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].target_basename, "Show - 01.ass");
    }

    #[test]
    fn idx_sub_unit_shares_main_name() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.idx"), entry("/subs/Show.S01E01.sub")];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
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
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
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
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
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
        let plan =
            generate_plan(&r, &NamingConfig::default(), &AlwaysExists, ActionMode::Rename).unwrap();
        assert!(plan.has_conflicts());
        assert!(
            plan.ops
                .iter()
                .any(|o| o.conflicts.iter().any(|c| matches!(c, Conflict::TargetExists(_))))
        );
    }

    #[test]
    fn action_mode_rename_uses_rename_in_place() {
        let m = Matcher::new();
        let v = vec![entry("/media/Show - 01.mkv")];
        let s = vec![entry("/media/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
        assert_eq!(plan.ops[0].action, PlannedAction::Rename);
    }

    #[test]
    fn action_mode_rename_stays_in_subtitle_dir() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Rename).unwrap();
        // Rename mode puts the target inside the subtitle's own dir; the
        // resulting action is `Rename` (same-dir target_path).
        assert_eq!(plan.ops[0].action, PlannedAction::Rename);
        assert_eq!(plan.ops[0].target_path.parent().unwrap(), Path::new("/subs"));
    }

    #[test]
    fn action_mode_copy_to_video_dir() {
        let m = Matcher::new();
        let v = vec![entry("/videos/Show - 01.mkv")];
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        let plan =
            generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Copy).unwrap();
        assert_eq!(plan.ops[0].action, PlannedAction::Copy);
        assert_eq!(plan.ops[0].target_path.parent().unwrap(), Path::new("/videos"));
    }

    #[test]
    fn action_mode_copy_without_video_errors() {
        let m = Matcher::new();
        let v: Vec<FileEntry> = Vec::new();
        let s = vec![entry("/subs/Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        match generate_plan(&r, &NamingConfig::default(), &NoExists, ActionMode::Copy) {
            Err(PlanError::NoVideoForCopy { op_index }) => assert_eq!(op_index, 0),
            other => panic!("expected NoVideoForCopy, got {other:?}"),
        }
    }

    fn mapping(token: &str, value: &str, var: &str) -> TokenMapping {
        TokenMapping { token: token.into(), value: value.into(), var: var.into() }
    }

    #[test]
    fn resolve_vars_collects_multiple_vars() {
        let cfg = NamingConfig {
            mappings: vec![
                mapping("X2&CASO", "华盟字幕社", "group"),
                mapping("track3", "zh-Hans", "lang"),
            ],
            ..NamingConfig::default()
        };
        let sub = entry("[X2&CASO][Death_Note][01]_track3.ass");
        let vars = cfg.resolve_vars(&sub);
        assert_eq!(vars.get("group").map(String::as_str), Some("华盟字幕社"));
        assert_eq!(vars.get("lang").map(String::as_str), Some("zh-Hans"));
    }

    #[test]
    fn resolve_vars_longest_token_wins() {
        let cfg = NamingConfig {
            mappings: vec![mapping("track", "short", "lang"), mapping("track3", "zh-Hans", "lang")],
            ..NamingConfig::default()
        };
        let sub = entry("..._track3.ass");
        let vars = cfg.resolve_vars(&sub);
        assert_eq!(vars.get("lang").map(String::as_str), Some("zh-Hans"));
    }

    #[test]
    fn resolve_vars_case_sensitive() {
        let cfg = NamingConfig {
            case_sensitive: true,
            mappings: vec![mapping("chs", "zh-Hans", "lang")],
            ..NamingConfig::default()
        };
        assert_eq!(cfg.resolve_vars(&entry("Show.CHS.ass")).get("lang").map(String::as_str), None);
        assert_eq!(
            cfg.resolve_vars(&entry("Show.chs.ass")).get("lang").map(String::as_str),
            Some("zh-Hans")
        );
    }

    #[test]
    fn render_template_custom_var_and_empty_video() {
        let mut vars = HashMap::new();
        vars.insert("dual".to_string(), "简日双语".to_string());
        assert_eq!(
            render_template("${video}.${dual}.${ext}", &vars, "Show - 01", "ass"),
            "Show - 01.简日双语.ass"
        );
        assert_eq!(
            render_template(
                "${video}.${group}-${lang}.${ext}",
                &HashMap::new(),
                "Show - 01",
                "ass"
            ),
            "Show - 01.ass"
        );
    }
}
