//! File-type recognition and episode-key extraction / normalization.
//!
//! `ExtensionRegistry` decides whether a path is a video, subtitle, or unknown.
//! `extract_keys` finds the episode key for each filename in a group using
//! longest-common-prefix/suffix stripping with a ranking over candidate
//! variable fields. `normalize_key` canonicalizes a raw key into an
//! `EpisodeKey` so that variants like `01` / `E1` / `EP01` / `01v2` all
//! compare equal and `01-02` / `01.5` survive as compound keys.

use std::collections::HashSet;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Classification of a single dropped file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileCategory {
    Video,
    Subtitle,
    Unknown,
}

/// A canonical episode key suitable for equality comparison across stems.
///
/// Variants like `01`, `E1`, `EP01`, `01v2` all normalize to
/// `EpisodeKey::Number("1")`. Ranges (`01-02`) and decimals (`01.5`) are kept
/// as compound strings inside `Number`. Non-numeric entries such as `NCOP`,
/// `SP`, `OVA`, `PV` become `EpisodeKey::Text("NCOP")`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EpisodeKey {
    /// Numeric episode key (possibly compound: "1", "1-2", "1.5").
    Number(String),
    /// Non-numeric text key, upper-cased.
    Text(String),
}

/// Numeric-aware ordering: episodes are ordered by their leading numeric
/// component so that `1, 2, 10, 11, 100` come out in numeric rather than
/// lexicographic order. Compound keys (`1-2`, `1.5`) break ties via the
/// rest of the string. `Number` always orders before `Text`.
impl Ord for EpisodeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => numeric_compare(a, b),
            (Self::Number(_), Self::Text(_)) => std::cmp::Ordering::Less,
            (Self::Text(_), Self::Number(_)) => std::cmp::Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for EpisodeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare two canonical numeric key strings by leading integer, with a
/// string tiebreak. Handles bare integers (`"1"`, `"10"`) and compound
/// forms (`"1-2"`, `"1.5"`) by using the leading integer.
fn numeric_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let na = leading_int(a);
    let nb = leading_int(b);
    match (na, nb) {
        (Some(x), Some(y)) => match x.cmp(&y) {
            std::cmp::Ordering::Equal => a.cmp(b),
            other => other,
        },
        _ => a.cmp(b),
    }
}

fn leading_int(s: &str) -> Option<u64> {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = i + 1;
        } else {
            break;
        }
    }
    if end == 0 { None } else { s[..end].parse::<u64>().ok() }
}

impl EpisodeKey {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Number(s) | Self::Text(s) => s,
        }
    }
}

/// Raw, unnormalized key produced by `extract_keys`.
///
/// A raw key is either a single digit/letter run extracted from the variable
/// region of a filename stem, or `None` when no key could be isolated (e.g.
/// a single file in a group, or a stem that is entirely static).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawKey(pub Option<String>);

/// Registry of recognized file extensions, with user-overridable slots.
#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    video: HashSet<String>,
    subtitle: HashSet<String>,
    custom_video: HashSet<String>,
    custom_subtitle: HashSet<String>,
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        let mut video = HashSet::new();
        for ext in [
            "mkv", "mp4", "m4v", "avi", "mov", "wmv", "flv", "ts", "m2ts", "mts", "mpg", "mpeg",
            "vob", "webm", "rmvb", "f4v", "3gp",
        ] {
            video.insert(ext.to_ascii_lowercase());
        }
        let mut subtitle = HashSet::new();
        for ext in ["ass", "ssa", "srt", "sub", "idx", "vtt", "smi", "sup"] {
            subtitle.insert(ext.to_ascii_lowercase());
        }
        Self { video, subtitle, custom_video: HashSet::new(), custom_subtitle: HashSet::new() }
    }

    /// Build a registry with user-supplied additional extensions.
    #[must_use]
    pub fn with_custom(
        mut self,
        video: impl IntoIterator<Item = String>,
        subtitle: impl IntoIterator<Item = String>,
    ) -> Self {
        for v in video {
            self.custom_video.insert(v.to_ascii_lowercase());
        }
        for s in subtitle {
            self.custom_subtitle.insert(s.to_ascii_lowercase());
        }
        self
    }

    pub fn add_custom_video(&mut self, ext: &str) {
        self.custom_video.insert(ext.trim_start_matches('.').to_ascii_lowercase());
    }

    pub fn add_custom_subtitle(&mut self, ext: &str) {
        self.custom_subtitle.insert(ext.trim_start_matches('.').to_ascii_lowercase());
    }

    pub fn custom_video(&self) -> &HashSet<String> {
        &self.custom_video
    }

    pub fn custom_subtitle(&self) -> &HashSet<String> {
        &self.custom_subtitle
    }

    /// Classify a path by its final extension.
    pub fn categorize(&self, path: &Path) -> FileCategory {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return FileCategory::Unknown;
        };
        let ext = ext.to_ascii_lowercase();
        if self.video.contains(&ext) || self.custom_video.contains(&ext) {
            FileCategory::Video
        } else if self.subtitle.contains(&ext) || self.custom_subtitle.contains(&ext) {
            FileCategory::Subtitle
        } else {
            FileCategory::Unknown
        }
    }

    /// Split a path's basename into (stem, extension). The extension is the
    /// substring after the LAST `.` in the basename, per design D4. If no
    /// `.` is present, the whole basename is the stem and the extension is
    /// empty.
    pub fn split_basename(path: &Path) -> (String, String) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return (String::new(), String::new());
        };
        match name.rfind('.') {
            Some(idx) if idx > 0 => (name[..idx].to_string(), name[idx + 1..].to_string()),
            _ => (name.to_string(), String::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Episode-key extraction (diff over a group of stems)
// ---------------------------------------------------------------------------

/// Extract raw episode keys for each stem in `stems` using the diff algorithm.
///
/// The returned vector has one entry per input stem. An entry is `RawKey(None)`
/// when no variable region exists (e.g. single-file group, or a stem whose
/// variable region is empty after LCP/LCS stripping). The caller is expected
/// to pass the result to [`normalize_key`] before using it for pairing.
pub fn extract_keys(stems: &[&str]) -> Vec<RawKey> {
    if stems.is_empty() {
        return Vec::new();
    }
    if stems.len() == 1 {
        // Single-file group: no diff possible. Try regex-free, heuristic
        // extraction on the bare stem so that a user dropping one well-named
        // video and one well-named subtitle still gets the right keys via
        // the matcher's single-file shortcut.
        return vec![heuristic_single(stems[0])];
    }

    let lcp = common_prefix_len(stems);
    let lcs = common_suffix_len(stems);

    if lcp + lcs >= stems[0].len() {
        // Entire stems are equal under LCP/LCS — no variable region.
        // Fall back to the heuristic on each full stem.
        return stems.iter().map(|s| heuristic_single(s)).collect();
    }

    let cores: Vec<&str> = stems.iter().map(|s| &s[lcp..s.len() - lcs]).collect();

    // Split each core into fields (digit runs / alpha runs / separators).
    let fields_per_core: Vec<Vec<Field<'_>>> = cores.iter().map(|c| split_fields(c)).collect();

    // Pick the variable field via ranking; if alignment fails, treat the
    // whole core as a single variable field.
    let chosen_index = match rank_variable_field(&fields_per_core) {
        Some(idx) => Some(idx),
        None if fields_per_core.iter().all(|f| f.len() == 1) => Some(0),
        None => None,
    };

    match chosen_index {
        Some(idx) => fields_per_core
            .iter()
            .map(|fields| {
                fields
                    .get(idx)
                    .filter(|f| matches!(f.kind, FieldKind::Digit | FieldKind::Alpha))
                    .map_or(RawKey(None), |f| RawKey(Some(f.text.to_string())))
            })
            .collect(),
        None => {
            // Fallback: per-stem heuristic on the full stem. This handles
            // the cross-format case where alignment across the group
            // fails (e.g. `Show - 01` vs `Show.S01E01` share no core
            // fields worth picking).
            stems.iter().map(|s| heuristic_single(s)).collect()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FieldKind {
    Digit,
    Alpha,
    Separator,
}

#[derive(Debug, Clone)]
struct Field<'a> {
    text: &'a str,
    kind: FieldKind,
}

fn split_fields(s: &str) -> Vec<Field<'_>> {
    // Number token regex: digits, optionally joined by '-' or '.' between
    // digits (preserves "01-02", "01.5" as a single field).
    let re = Regex::new(r"\d+(?:[-.]\d+)*|[A-Za-z]+|[^A-Za-z0-9]+").unwrap();
    re.find_iter(s)
        .map(|m| {
            let text = m.as_str();
            let kind = if text.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '.') {
                // Digits joined by '-' or '.' is still a numeric field
                // (e.g. "01-02", "01.5"). Must contain at least one digit.
                if text.chars().any(|c| c.is_ascii_digit()) {
                    FieldKind::Digit
                } else {
                    FieldKind::Separator
                }
            } else if text.chars().all(|c| c.is_ascii_alphabetic()) {
                FieldKind::Alpha
            } else {
                FieldKind::Separator
            };
            Field { text, kind }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentType {
    /// Pure-digit run (e.g. "01", "108").
    PureDigit,
    /// Contains digits but also letters (e.g. "108p", "01v2").
    ContainsDigit,
    /// Pure text, no digits (e.g. "NCOP", "SP").
    PureText,
}

impl SegmentType {
    fn score(self) -> u32 {
        match self {
            SegmentType::PureDigit => 3,
            SegmentType::ContainsDigit => 2,
            SegmentType::PureText => 1,
        }
    }
}

fn classify(text: &str) -> SegmentType {
    if !text.chars().any(|c| c.is_ascii_digit()) {
        SegmentType::PureText
    } else if text.chars().filter(|c| !c.is_ascii_digit() && *c != '-' && *c != '.').all(|_| true)
        && text.chars().any(|c| c.is_ascii_digit())
        && !text.chars().any(|c| c.is_ascii_alphabetic())
    {
        SegmentType::PureDigit
    } else {
        SegmentType::ContainsDigit
    }
}

/// Heuristic: does this digit/alpha run sit next to an episode marker
/// (`E`, `EP`, `第`, `话`, `集`) on either side?
fn near_marker(cores: &[Vec<Field<'_>>], field_index: usize, stem_index: usize) -> bool {
    let Some(fields) = cores.get(stem_index) else {
        return false;
    };
    let left = fields.get(field_index.wrapping_sub(1)).map(|f| f.text.to_ascii_lowercase());
    let right = fields.get(field_index + 1).map(|f| f.text.to_ascii_lowercase());
    let is_marker = |t: &str| {
        matches!(t, "e" | "ep" | "第" | "话" | "集")
            || t.starts_with("第")
            || t.ends_with("话")
            || t.ends_with("集")
    };
    left.as_deref().is_some_and(is_marker) || right.as_deref().is_some_and(is_marker)
}

/// Pick the best variable field across all stems, or `None` if no field
/// varies across the group.
fn rank_variable_field(cores: &[Vec<Field<'_>>]) -> Option<usize> {
    let n_cores = cores.len();
    if n_cores == 0 {
        return None;
    }
    let max_fields = cores.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut best: Option<(u32, usize)> = None;

    for idx in 0..max_fields {
        let values: Vec<&Field<'_>> = cores
            .iter()
            .filter_map(|f| f.get(idx))
            .filter(|f| matches!(f.kind, FieldKind::Digit | FieldKind::Alpha))
            .collect();
        if values.len() < n_cores {
            // Some stems don't have this field — ignore for ranking.
            continue;
        }
        // Determine the set of distinct values.
        let mut distinct: Vec<&str> = values.iter().map(|f| f.text).collect();
        distinct.sort_unstable();
        distinct.dedup();
        if distinct.len() < 2 {
            continue; // not a variable field
        }

        let seg_type = classify(distinct[0]);
        if matches!(seg_type, SegmentType::PureText)
            && distinct.iter().all(|t| classify(t) == SegmentType::PureText)
        {
            // OK as a text candidate.
        } else if distinct.iter().any(|t| classify(t) != seg_type) {
            // Mixed types in same field — skip (alignment is suspect).
            continue;
        }

        let varies_in_all = distinct.len() == n_cores;
        let marker_bonus: u32 = (0..n_cores)
            .filter(|i| near_marker(cores, idx, *i))
            .count()
            .try_into()
            .unwrap_or(u32::MAX);

        let type_score = seg_type.score();
        let score = marker_bonus.saturating_mul(1_000)
            + type_score.saturating_mul(10)
            + u32::from(varies_in_all);

        match best {
            Some((s, _)) if s >= score => {}
            _ => best = Some((score, idx)),
        }
    }
    best.map(|(_, idx)| idx)
}

fn common_prefix_len(stems: &[&str]) -> usize {
    if stems.is_empty() {
        return 0;
    }
    let first = stems[0];
    let mut len = first.len();
    for s in &stems[1..] {
        let mut i = 0;
        while i < len && i < s.len() && first.as_bytes()[i] == s.as_bytes()[i] {
            i += 1;
        }
        len = i;
        if len == 0 {
            break;
        }
    }
    len
}

fn common_suffix_len(stems: &[&str]) -> usize {
    if stems.is_empty() {
        return 0;
    }
    let first = stems[0];
    let mut len = first.len();
    for s in &stems[1..] {
        let mut i = 0;
        while i < len && i < s.len() {
            let a = first.as_bytes()[first.len() - 1 - i];
            let b = s.as_bytes()[s.len() - 1 - i];
            if a == b {
                i += 1;
            } else {
                break;
            }
        }
        len = i;
        if len == 0 {
            break;
        }
    }
    len
}

/// Fallback heuristic for single-file groups: extract the first plausible
/// numeric or text key from the bare stem so that a single well-named file
/// still yields a usable key.
pub(crate) fn heuristic_single(stem: &str) -> RawKey {
    let fields = split_fields(stem);
    // Look for "tagged specials" first: a digit field whose immediately
    // preceding alpha field is one of the well-known non-episode tags
    // (SP, OP, ED, NC, NCOP, NCED, OVA, PV, …). These should be treated as
    // text keys so that e.g. `SP01` doesn't collapse onto plain episode
    // `01`. Return just the tag (e.g. "SP") — normalize_key will treat
    // it as a text key.
    for (i, f) in fields.iter().enumerate() {
        if matches!(f.kind, FieldKind::Digit) {
            if let Some(prev) = fields.get(i.wrapping_sub(1))
                && matches!(prev.kind, FieldKind::Alpha)
            {
                let p = prev.text.to_ascii_lowercase();
                if SPECIAL_TAGS.contains(&p.as_str()) {
                    return RawKey(Some(prev.text.to_string()));
                }
            }
            return RawKey(Some(f.text.to_string()));
        }
    }
    // Otherwise the first alpha field.
    for f in &fields {
        if matches!(f.kind, FieldKind::Alpha) {
            return RawKey(Some(f.text.to_string()));
        }
    }
    RawKey(None)
}

// ---------------------------------------------------------------------------
// Key normalization
// ---------------------------------------------------------------------------

/// Normalize a raw key (or the full stem as a fallback) into an `EpisodeKey`.
///
/// The returned `EpisodeKey` is what the matcher uses for grouping. A return
/// value of `None` indicates the raw key could not be normalized at all
/// (caller should consider the stem unmatchable on this side).
pub fn normalize_key(raw: Option<&str>) -> Option<EpisodeKey> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }

    let lower = s.to_ascii_lowercase();

    // If the raw begins with a known non-episode tag (SP, OP, ED, NCOP,
    // OVA, PV, …), treat it as a text key rather than letting the numeric
    // patterns strip the tag and return a bare episode number.
    if let Some(tag) = SPECIAL_TAGS.iter().find(|t| lower.starts_with(*t)) {
        return Some(EpisodeKey::Text(tag.to_ascii_uppercase()));
    }

    // Episode-marker patterns, in priority order:
    //   第 (\d+ ...) 话/集
    //   S\d+ E(p)? (\d+ ...)
    //   E(p)? (\d+ ...)
    //   bare (\d+ ...)  -- last resort for numeric
    let patterns: &[&str] = &[
        r"第\s*(\d+(?:[-.]\d+)*)\s*[话集]",
        r"s\d+\s*e[p]?\s*(\d+(?:[-.]\d+)*)",
        r"e[p]?\s*(\d+(?:[-.]\d+)*)",
        r"(\d+(?:[-.]\d+)*)",
    ];

    for pat in patterns {
        let Ok(re) = Regex::new(pat) else { continue };
        if let Some(caps) = re.captures(&lower)
            && let Some(m) = caps.get(1)
        {
            let canonical = canonicalize_numeric(m.as_str());
            if let Some(c) = canonical {
                return Some(EpisodeKey::Number(c));
            }
        }
    }

    // Non-numeric text key.
    let text = s.trim_start_matches(|c: char| !c.is_alphanumeric());
    let text: String = text.chars().take_while(|c| c.is_alphanumeric()).collect();
    if text.is_empty() {
        return None;
    }
    Some(EpisodeKey::Text(text.to_ascii_uppercase()))
}

const SPECIAL_TAGS: &[&str] = &["ncop", "nced", "sp", "op", "ed", "ova", "pv", "cm", "menu"];

/// Canonicalize a numeric key string: strip leading zeros on each numeric
/// component (split by `-` and `.`), drop revision `v\d+` suffix.
fn canonicalize_numeric(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    // Strip a revision marker like `v2` at the end.
    let re = Regex::new(r"^(?P<num>[\d.\-]+?)(?:v\d+)?$").unwrap();
    let caps = re.captures(&lower)?;
    let body = caps.name("num")?.as_str();
    if body.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut current = String::new();
    for ch in body.chars() {
        if ch == '-' || ch == '.' {
            out.push_str(strip_leading_zeros(&current));
            current.clear();
            out.push(ch);
        } else {
            current.push(ch);
        }
    }
    out.push_str(strip_leading_zeros(&current));
    Some(out)
}

fn strip_leading_zeros(s: &str) -> &str {
    let stripped = s.trim_start_matches('0');
    if stripped.is_empty() && s.chars().any(|c| c.is_ascii_digit()) {
        "0"
    } else if stripped.is_empty() {
        s
    } else {
        stripped
    }
}

// ---------------------------------------------------------------------------
// Language token extraction (used by plan.rs for default suffix resolution)
// ---------------------------------------------------------------------------

/// Built-in table of language/group tokens matched on the stem.
pub const LANGUAGE_TOKENS: &[&str] = &[
    "chs", "cht", "sc", "tc", "jp", "jpn", "eng", "en", "gb", "tw", "简体", "繁體", "简", "繁",
    "jpsc", "jptc",
];

/// If the stem contains a recognized language token, return it (lower-cased).
pub fn detect_language_token(stem: &str) -> Option<String> {
    let lower = stem.to_ascii_lowercase();
    for token in LANGUAGE_TOKENS {
        let token_lower = token.to_ascii_lowercase();
        if lower.contains(&token_lower) {
            return Some(token_lower);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Regex-based extraction (task 2.5: user-supplied regex fallback)
// ---------------------------------------------------------------------------

/// Apply a user-supplied regex (with one capture group) to each stem and
/// produce normalized keys.
pub fn extract_with_regex(stems: &[&str], pattern: &str) -> Vec<RawKey> {
    let Ok(re) = Regex::new(pattern) else {
        return vec![RawKey(None); stems.len()];
    };
    stems
        .iter()
        .map(|s| {
            re.captures(s)
                .and_then(|c| c.get(1))
                .map_or(RawKey(None), |m| RawKey(Some(m.as_str().to_string())))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_registry_categorizes_basic() {
        let reg = ExtensionRegistry::new();
        assert_eq!(reg.categorize(Path::new("show.mkv")), FileCategory::Video);
        assert_eq!(reg.categorize(Path::new("show.ass")), FileCategory::Subtitle);
        assert_eq!(reg.categorize(Path::new("show.txt")), FileCategory::Unknown);
    }

    #[test]
    fn extension_registry_honors_custom_overrides() {
        let mut reg = ExtensionRegistry::new();
        reg.add_custom_subtitle("zh.ass");
        assert_eq!(reg.categorize(Path::new("show.zh.ass")), FileCategory::Subtitle);
    }

    #[test]
    fn split_basename_uses_last_dot() {
        let (stem, ext) = ExtensionRegistry::split_basename(Path::new("Show.S01E01.chs.ass"));
        assert_eq!(stem, "Show.S01E01.chs");
        assert_eq!(ext, "ass");
    }

    #[test]
    fn split_basename_no_dot() {
        let (stem, ext) = ExtensionRegistry::split_basename(Path::new("README"));
        assert_eq!(stem, "README");
        assert_eq!(ext, "");
    }

    #[test]
    fn extract_keys_clean_single_segment() {
        let stems =
            ["[Group] Show - 01 [1080p]", "[Group] Show - 02 [1080p]", "[Group] Show - 03 [1080p]"];
        let raw: Vec<&str> = stems.to_vec();
        let keys = extract_keys(&raw);
        assert_eq!(
            keys,
            vec![RawKey(Some("1".into())), RawKey(Some("2".into())), RawKey(Some("3".into()))]
        );
    }

    #[test]
    fn extract_keys_multiple_segments_picks_digit_leftmost() {
        // Resolution varies along with episode: should pick the leftmost
        // pure-digit varying field.
        let stems = ["[Group] Show - 01 [1080p]", "[Group] Show - 02 [720p]"];
        let raw: Vec<&str> = stems.to_vec();
        let keys = extract_keys(&raw);
        assert_eq!(keys, vec![RawKey(Some("1".into())), RawKey(Some("2".into()))]);
    }

    #[test]
    fn extract_keys_near_ep_marker() {
        let stems = ["Show EP01.mkv", "Show EP02.mkv", "Show EP03.mkv"];
        let raw: Vec<&str> = stems.to_vec();
        let keys = extract_keys(&raw);
        assert_eq!(
            keys,
            vec![RawKey(Some("1".into())), RawKey(Some("2".into())), RawKey(Some("3".into()))]
        );
    }

    #[test]
    fn normalize_strips_leading_zeros_and_e_prefix() {
        assert_eq!(normalize_key(Some("01")), Some(EpisodeKey::Number("1".into())));
        assert_eq!(normalize_key(Some("E1")), Some(EpisodeKey::Number("1".into())));
        assert_eq!(normalize_key(Some("EP1")), Some(EpisodeKey::Number("1".into())));
        assert_eq!(normalize_key(Some("第01话")), Some(EpisodeKey::Number("1".into())));
    }

    #[test]
    fn normalize_preserves_range_and_decimal() {
        assert_eq!(normalize_key(Some("01-02")), Some(EpisodeKey::Number("1-2".into())));
        assert_eq!(normalize_key(Some("01.5")), Some(EpisodeKey::Number("1.5".into())));
    }

    #[test]
    fn normalize_strips_revision_marker() {
        assert_eq!(normalize_key(Some("01v2")), Some(EpisodeKey::Number("1".into())));
    }

    #[test]
    fn normalize_season_episode() {
        assert_eq!(normalize_key(Some("S01E01")), Some(EpisodeKey::Number("1".into())));
    }

    #[test]
    fn normalize_text_key() {
        assert_eq!(normalize_key(Some("NCOP")), Some(EpisodeKey::Text("NCOP".into())));
    }

    #[test]
    fn heuristic_single_finds_digit() {
        let r = heuristic_single("[Group] Show - 01 [1080p]");
        assert_eq!(r, RawKey(Some("01".into())));
    }

    #[test]
    fn episode_key_orders_numerically() {
        let mut keys = vec![
            EpisodeKey::Number("10".into()),
            EpisodeKey::Number("2".into()),
            EpisodeKey::Number("1".into()),
            EpisodeKey::Number("100".into()),
            EpisodeKey::Text("NCOP".into()),
            EpisodeKey::Number("3".into()),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                EpisodeKey::Number("1".into()),
                EpisodeKey::Number("2".into()),
                EpisodeKey::Number("3".into()),
                EpisodeKey::Number("10".into()),
                EpisodeKey::Number("100".into()),
                EpisodeKey::Text("NCOP".into()),
            ]
        );
    }

    #[test]
    fn language_token_detection() {
        assert_eq!(detect_language_token("Show.S01E01.chs.ass"), Some("chs".into()));
        assert_eq!(detect_language_token("Show.S01E01.cht"), Some("cht".into()));
        assert_eq!(detect_language_token("No.Lang.Here"), None);
    }

    #[test]
    fn extract_with_regex_captures_first_group() {
        let stems = ["Show ep01", "Show ep02", "Show ep03"];
        let raw: Vec<&str> = stems.to_vec();
        let keys = extract_with_regex(&raw, r"(?i)ep(\d+)");
        assert_eq!(
            keys,
            vec![RawKey(Some("01".into())), RawKey(Some("02".into())), RawKey(Some("03".into()))]
        );
    }
}
