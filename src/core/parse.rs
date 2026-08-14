//! File-type recognition and episode-key extraction / normalization.
//!
//! `ExtensionRegistry` decides whether a path is a video, subtitle, or unknown.
//! `extract_keys` finds the episode key for each filename in a group using
//! longest-common-prefix/suffix stripping with a ranking over candidate
//! variable fields. `extract_keys_cross` does the same by aligning the
//! variable slot across the video side and the subtitle side so that
//! stems with mismatched noise / episode-title tails still pair up.
//! `normalize_key` canonicalizes a raw key into an `EpisodeKey` so that
//! variants like `01` / `E1` / `EP01` / `01v2` all compare equal and
//! `01-02` / `01.5` survive as compound keys.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

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

    // Normalize each stem: NFKD + ASCII whitespace fold. Stems that were
    // already canonical borrow from the input; others live in `owned`.
    let normalized: Vec<Cow<'_, str>> = stems.iter().map(|s| normalize_stem(s)).collect();
    let needs_owned = normalized.iter().any(|c| matches!(c, Cow::Owned(_)));
    let owned_buf: Vec<String>;
    let norm_refs: Vec<&str> = if needs_owned {
        owned_buf = normalized.iter().map(|c| c.as_ref().to_string()).collect();
        owned_buf.iter().map(std::string::String::as_str).collect()
    } else {
        normalized.iter().map(std::convert::AsRef::as_ref).collect()
    };

    let lcp = common_prefix_len(&norm_refs);
    let lcs = common_suffix_len(&norm_refs);

    if lcp + lcs >= norm_refs[0].len() {
        // Entire stems are equal under LCP/LCS — no variable region.
        // Fall back to the heuristic on each full stem.
        return norm_refs.iter().map(|s| heuristic_single(s)).collect();
    }

    let cores: Vec<&str> = norm_refs.iter().map(|s| &s[lcp..s.len() - lcs]).collect();

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
                    .filter(|f| {
                        matches!(
                            f.kind,
                            FieldKind::Digit | FieldKind::Alpha | FieldKind::CjkAlpha
                        )
                            // Defensive: CJK-only "separator" run (e.g. `一`
                            // after LCP/LCS strips `中文 第…集`) is still a
                            // valid variable-region key.
                            || is_cjk_text_field(f)
                    })
                    .map_or(RawKey(None), |f| RawKey(Some(f.text.to_string())))
            })
            .collect(),
        None => {
            // Fallback: per-stem heuristic on the full stem. This handles
            // the cross-format case where alignment across the group
            // fails (e.g. `Show - 01` vs `Show.S01E01` share no core
            // fields worth picking).
            norm_refs.iter().map(|s| heuristic_single(s)).collect()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FieldKind {
    Digit,
    Alpha,
    Separator,
    /// Non-ASCII alphabetic token (e.g. `中文`, `機動`, `一`).
    CjkAlpha,
    /// Reserved for future CJK digit characters; not produced by `split_fields`
    /// today since CJK numerals are alphabetic in Unicode.
    #[allow(dead_code)] // placeholder for future CJK decimal / numeric support
    CjkDigit,
}

#[derive(Debug, Clone)]
struct Field<'a> {
    text: &'a str,
    kind: FieldKind,
}

// Compiled once. The pattern handles four disjoint alternatives:
//   - ASCII digit runs (numeric tokens; compound forms like `01-02` / `01.5`
//     are intentionally NOT combined with the digit run — they're rare in
//     practice and combining them causes spurious cross-side matches e.g.
//     `S02E01.123` would otherwise parse as `01.123` and defeat the variable
//     slot alignment)
//   - pure ASCII alpha runs
//   - any Unicode alphabetic run (catches pure CJK like `中文` / `機動`)
//   - any run of non-alphanumeric characters (separators)
static SPLIT_FIELDS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\d+|[A-Za-z]+|\p{Alphabetic}+|[^\p{Alphabetic}\p{Digit}]+").unwrap()
});

fn split_fields(s: &str) -> Vec<Field<'_>> {
    SPLIT_FIELDS_RE
        .find_iter(s)
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
            } else if text.chars().all(char::is_alphabetic) {
                if text.chars().any(|c| !c.is_ascii() && c.is_alphabetic()) {
                    FieldKind::CjkAlpha
                } else {
                    FieldKind::Alpha
                }
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
            .filter(|f| matches!(f.kind, FieldKind::Digit | FieldKind::Alpha | FieldKind::CjkAlpha))
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
    let mut bytes = first.len();
    for s in &stems[1..] {
        let mut matched = 0usize;
        for (a, b) in first.chars().zip(s.chars()) {
            if a == b {
                matched += a.len_utf8();
            } else {
                break;
            }
        }
        bytes = matched;
        if bytes == 0 {
            break;
        }
    }
    bytes
}

fn common_suffix_len(stems: &[&str]) -> usize {
    if stems.is_empty() {
        return 0;
    }
    let first = stems[0];
    let mut bytes = first.len();
    for s in &stems[1..] {
        let a_rev: Vec<char> = first.chars().rev().collect();
        let b_rev: Vec<char> = s.chars().rev().collect();
        let mut matched = 0usize;
        for (a, b) in a_rev.iter().zip(b_rev.iter()) {
            if a == b {
                matched += a.len_utf8();
            } else {
                break;
            }
        }
        bytes = matched;
        if bytes == 0 {
            break;
        }
    }
    bytes
}

/// Normalize a stem before key extraction.
///
/// Pipeline:
///   1. Unicode NFKD — collapses compatibility variants (e.g. `機動` →
///      `机动`, `為` → `为`) so that LCP/LCS land on the same byte
///      offsets across stems that differ only in script variant.
///   2. ASCII whitespace fold — collapses runs of `\t` / ` ` into a
///      single space and trims, so that `Show - 1 xyz` and
///      `Show.1xyz` don't diverge purely on whitespace.
///
/// Returns `Cow<str>`: `Borrowed` when the input was already normalized
/// (the common case for ASCII stems), `Owned` when the pipeline had to
/// rewrite.
fn normalize_stem(s: &str) -> Cow<'_, str> {
    // NFKD on pure ASCII is identity, so skip the allocation.
    let nfkd: Cow<'_, str> =
        if s.is_ascii() { Cow::Borrowed(s) } else { Cow::Owned(s.nfkd().collect::<String>()) };
    if !needs_whitespace_fold(nfkd.as_ref()) {
        return nfkd;
    }
    Cow::Owned(fold_ascii_whitespace(nfkd.as_ref()))
}

fn needs_whitespace_fold(s: &str) -> bool {
    let bytes = s.as_bytes();
    // Tabs always get folded to a single space.
    if bytes.contains(&b'\t') {
        return true;
    }
    if bytes.first().is_some_and(|b| *b == b' ') {
        return true;
    }
    if bytes.last().is_some_and(|b| *b == b' ') {
        return true;
    }
    bytes.windows(2).any(|w| w[0] == b' ' && w[1] == b' ')
}

fn fold_ascii_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = true;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// True when `f` is a `Separator` field whose text is entirely CJK
/// letters/digits — e.g. `一`, `機動` — and therefore represents a
/// non-ASCII alphanumeric token that the ASCII regex couldn't classify.
fn is_cjk_text_field(f: &Field<'_>) -> bool {
    matches!(f.kind, FieldKind::Separator)
        && !f.text.is_empty()
        && f.text.chars().all(|c| !c.is_ascii() && c.is_alphanumeric())
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
            // Walk back over any Separator fields to find the nearest
            // non-separator token; if it's a SPECIAL_TAG alpha (SP/OP/ED/
            // NCOP/NCED/OVA/PV/CM/Menu) — or a CJK numeral (一/二/...) —
            // return that as a text key so the digit doesn't get treated
            // as an episode number.
            if let Some(j) = (0..i).rev().find(|&j| !matches!(fields[j].kind, FieldKind::Separator))
                && matches!(fields[j].kind, FieldKind::Alpha)
            {
                let p = fields[j].text.to_ascii_lowercase();
                if SPECIAL_TAGS.contains(&p.as_str()) {
                    return RawKey(Some(fields[j].text.to_string()));
                }
                if CHINESE_NUMERALS.contains(&fields[j].text) {
                    return RawKey(Some(fields[j].text.to_string()));
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

    // CJK numerals (`一/二/.../十` and uppercase `壹/贰/.../拾`) act as
    // text keys: they have no ASCII case, so we scan the start of `lower`
    // (after ascii-lowercasing — CJK chars are unaffected) for any of them.
    if let Some(n) = CHINESE_NUMERALS.iter().find(|n| lower.starts_with(*n)) {
        return Some(EpisodeKey::Text(n.to_string()));
    }

    // Episode-marker patterns, in priority order:
    //   第 (\d+ ...) 话/集
    //   S\d+ E(p)? (\d+ ...)
    //   E(p)? (\d+ ...)
    //   bare (\d+ ...)  -- last resort for numeric
    for re in KEY_PATTERNS.iter() {
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

/// Episode-marker regexes, compiled once (they are used in a hot loop by
/// the cross-side alignment scorer).
static KEY_PATTERNS: std::sync::LazyLock<[Regex; 4]> = std::sync::LazyLock::new(|| {
    [
        Regex::new(r"第\s*(\d+(?:[-.]\d+)*)\s*[话集]").unwrap(),
        Regex::new(r"s\d+\s*e[p]?\s*(\d+(?:[-.]\d+)*)").unwrap(),
        Regex::new(r"e[p]?\s*(\d+(?:[-.]\d+)*)").unwrap(),
        Regex::new(r"(\d+(?:[-.]\d+)*)").unwrap(),
    ]
});

const SPECIAL_TAGS: &[&str] = &["ncop", "nced", "sp", "op", "ed", "ova", "pv", "cm", "menu"];

/// Numeric canonicalization regex, compiled once (hot path: called per
/// normalized numeric key by the cross-side alignment scorer).
static NUMERIC_RE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^(?P<num>[\d.\-]+?)(?:v\d+)?$").unwrap());

const CHINESE_NUMERALS: &[&str] = &[
    "一", "二", "三", "四", "五", "六", "七", "八", "九", "十", "壹", "贰", "叁", "肆", "伍", "陆",
    "柒", "捌", "玖", "拾",
];

/// Canonicalize a numeric key string: strip leading zeros on each numeric
/// component (split by `-` and `.`), drop revision `v\d+` suffix.
fn canonicalize_numeric(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    // Strip a revision marker like `v2` at the end.
    let caps = NUMERIC_RE.captures(&lower)?;
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
// Cross-side token alignment (matcher-rewrite)
// ---------------------------------------------------------------------------

/// Count how many stems each `(text, kind)` token appears in, and at which
/// `(stem_idx, token_idx)` positions within each stem.
///
/// This is the first step of the cross-side alignment algorithm: build a
/// per-side map of token → positions so we can later identify "anchor"
/// tokens (constant across stems) and "episodic" tokens (one value per
/// stem) without depending on LCP/LCS stripping.
fn count_token_occurrences(
    token_seqs: &[Vec<Field<'_>>],
) -> HashMap<(String, FieldKind), Vec<(usize, usize)>> {
    let mut counts: HashMap<(String, FieldKind), Vec<(usize, usize)>> = HashMap::new();
    for (stem_idx, tokens) in token_seqs.iter().enumerate() {
        for (token_idx, field) in tokens.iter().enumerate() {
            counts
                .entry((field.text.to_string(), field.kind))
                .or_default()
                .push((stem_idx, token_idx));
        }
    }
    counts
}

/// Per-slot diversity: for each slot index, the set of `(text, kind)` values
/// observed across stems. Used to determine which slots are "variable"
/// (diversity ≥ 2) on each side independently.
fn slot_value_sets(token_seqs: &[Vec<Field<'_>>]) -> Vec<HashSet<(String, FieldKind)>> {
    let max_len = token_seqs.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut sets = vec![HashSet::new(); max_len];
    for tokens in token_seqs {
        for (slot_idx, field) in tokens.iter().enumerate() {
            sets[slot_idx].insert((field.text.to_string(), field.kind));
        }
    }
    sets
}

/// A cross-side candidate: a `(text, kind)` token that appears on both sides
/// and whose slot is variable on both sides (so cross-side alignment is
/// meaningful). The `video_diversity` / `sub_diversity` fields are the
/// maximum per-slot diversity observed at any position where the token
/// appears (the "field value count" referred to in the design).
struct CrossSideCandidate {
    token_text: String,
    token_kind: FieldKind,
    video_diversity: usize,
    sub_diversity: usize,
    video_token_indices: Vec<(usize, usize)>,
    sub_token_indices: Vec<(usize, usize)>,
}

/// Build the ranked candidate list from per-side counts and per-slot
/// diversity.
///
/// Filtering rule: the token appears on both sides AND its max slot
/// diversity is ≥ 2 on both sides (the slot is variable on both sides).
/// Ranking: `Digit > CjkDigit > CjkAlpha > Alpha > Separator`, tie-broken
/// by `video_diversity + sub_diversity` (higher wins), then by total
/// occurrence count across both sides (lower = more "episodic" = wins),
/// then by `token_text.len()` ascending (shorter avoids release-group
/// hits), then lexicographic ascending.
fn cross_side_candidates(
    video_counts: &HashMap<(String, FieldKind), Vec<(usize, usize)>>,
    sub_counts: &HashMap<(String, FieldKind), Vec<(usize, usize)>>,
    video_slot_sets: &[HashSet<(String, FieldKind)>],
    sub_slot_sets: &[HashSet<(String, FieldKind)>],
) -> Vec<CrossSideCandidate> {
    let mut candidates: Vec<CrossSideCandidate> = Vec::new();
    for (key, video_indices) in video_counts {
        let Some(sub_indices) = sub_counts.get(key) else {
            continue;
        };
        let max_video_div = video_indices
            .iter()
            .map(|&(_, slot)| video_slot_sets.get(slot).map_or(0, HashSet::len))
            .max()
            .unwrap_or(0);
        let max_sub_div = sub_indices
            .iter()
            .map(|&(_, slot)| sub_slot_sets.get(slot).map_or(0, HashSet::len))
            .max()
            .unwrap_or(0);
        if max_video_div < 2 || max_sub_div < 2 {
            continue;
        }
        candidates.push(CrossSideCandidate {
            token_text: key.0.clone(),
            token_kind: key.1,
            video_diversity: max_video_div,
            sub_diversity: max_sub_div,
            video_token_indices: video_indices.clone(),
            sub_token_indices: sub_indices.clone(),
        });
    }
    candidates.sort_by(|a, b| {
        kind_priority(b.token_kind)
            .cmp(&kind_priority(a.token_kind))
            .then_with(|| {
                (b.video_diversity + b.sub_diversity).cmp(&(a.video_diversity + a.sub_diversity))
            })
            .then_with(|| {
                (a.video_token_indices.len() + a.sub_token_indices.len())
                    .cmp(&(b.video_token_indices.len() + b.sub_token_indices.len()))
            })
            .then_with(|| a.token_text.len().cmp(&b.token_text.len()))
            .then_with(|| a.token_text.cmp(&b.token_text))
    });
    candidates
}

/// Ordering priority for cross-side candidate selection. Mirrors the
/// existing `SegmentType::score` for ASCII tokens and extends it to CJK.
fn kind_priority(kind: FieldKind) -> u32 {
    match kind {
        FieldKind::Digit => 5,
        FieldKind::CjkDigit => 4,
        FieldKind::CjkAlpha => 3,
        FieldKind::Alpha => 2,
        FieldKind::Separator => 1,
    }
}

/// For a candidate token, pick the single slot index on each side where it
/// "represents the variable part". If the token appears at one slot per
/// side, that slot is unambiguous. Otherwise we prefer the slot whose
/// diversity equals the candidate's recorded diversity (i.e. the slot
/// that is itself variable) and break ties by lower slot index (closer to
/// the stem head).
fn pick_slot_for_candidate(
    indices: &[(usize, usize)],
    slot_sets: &[HashSet<(String, FieldKind)>],
) -> Option<usize> {
    if indices.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for &(_, slot) in indices {
        let div = slot_sets.get(slot).map_or(0, HashSet::len);
        let prefer = match best {
            None => true,
            Some((best_div, best_slot)) => div > best_div || (div == best_div && slot < best_slot),
        };
        if prefer {
            best = Some((div, slot));
        }
    }
    best.map(|(_, slot)| slot)
}

/// Compute how many cross-side `(video_stem, sub_stem)` pairs have
/// matching [`EpisodeKey`]s at the given slots. Returns
/// `(matches, total_pairs)`. Matching is done after normalization so
/// `1` <-> `01`, `E1` <-> `EP1`, `01` <-> `一`, etc. all compare equal.
fn alignment_score(
    video_tokens: &[Vec<Field<'_>>],
    sub_tokens: &[Vec<Field<'_>>],
    video_slot: usize,
    sub_slot: usize,
) -> (usize, usize) {
    let mut matches = 0usize;
    let mut total = 0usize;
    for v_stem in video_tokens {
        for s_stem in sub_tokens {
            let Some(v_field) = v_stem.get(video_slot) else { continue };
            let Some(s_field) = s_stem.get(sub_slot) else { continue };
            total += 1;
            let v_norm = normalize_key(Some(v_field.text));
            let s_norm = normalize_key(Some(s_field.text));
            if v_norm.is_some() && v_norm == s_norm {
                matches += 1;
            }
        }
    }
    (matches, total)
}

/// Pick the best cross-side slot pair from the candidate list.
///
/// For each candidate, derive the `(video_slot, sub_slot)` pair where
/// the candidate's slot is most variable (via
/// [`pick_slot_for_candidate`]). Score each pair by:
///
/// 1. Combined per-slot diversity of the *picked* slot on each side
///    (higher wins -- a real episode slot has many distinct values).
/// 2. Cross-side alignment: matches then total pairs (higher = more
///    confidence).
/// 3. Candidate position count (lower = more "episodic").
///
/// Diversity is scored first so that noisy short-noise matches (e.g.
/// a coincidental `1` ↔ `1` in release-group suffixes) don't override
/// the actual episode slot.
fn pick_best_slot_pair(
    candidates: &[CrossSideCandidate],
    video_tokens: &[Vec<Field<'_>>],
    sub_tokens: &[Vec<Field<'_>>],
    video_slot_sets: &[HashSet<(String, FieldKind)>],
    sub_slot_sets: &[HashSet<(String, FieldKind)>],
) -> Option<(usize, usize)> {
    type SlotPairScore = (usize, usize, usize, usize);
    let mut best: Option<(usize, usize, SlotPairScore)> = None;
    for cand in candidates {
        let Some(v_slot) = pick_slot_for_candidate(&cand.video_token_indices, video_slot_sets)
        else {
            continue;
        };
        let Some(s_slot) = pick_slot_for_candidate(&cand.sub_token_indices, sub_slot_sets) else {
            continue;
        };
        let alignment = alignment_score(video_tokens, sub_tokens, v_slot, s_slot);
        let v_div = video_slot_sets.get(v_slot).map_or(0, HashSet::len);
        let s_div = sub_slot_sets.get(s_slot).map_or(0, HashSet::len);
        let cand_occ = cand.video_token_indices.len() + cand.sub_token_indices.len();
        // Score key: (combined slot diversity, matches, total pairs,
        // -occurrences). Diversity is ranked first so noisy short-noise
        // matches (e.g. coincidental `1` <-> `1` in release-group
        // suffixes) don't override the actual episode slot.
        let key = (v_div + s_div, alignment.0, alignment.1, usize::MAX - cand_occ);
        if best.is_none_or(|b| key > b.2) {
            best = Some((v_slot, s_slot, key));
        }
    }
    let (v_slot, s_slot, _) = best?;
    Some((v_slot, s_slot))
}

/// Extract raw episode keys for the video and subtitle sides jointly.
///
/// The function:
///   1. NFKD + whitespace-folds each stem (via [`normalize_stem`]).
///   2. Tokenizes each stem into Digit / Alpha / Separator / `CjkAlpha`
///      fields (via [`split_fields`]).
///   3. Counts `(text, kind)` occurrences and per-slot diversity on each
///      side independently.
///   4. Builds a ranked candidate list and picks the top-1 token whose
///      slot is variable on both sides.
///   5. Extracts the field at that token's slot from each stem; stems
///      without the slot become `RawKey(None)`.
///   6. Falls back to per-side [`extract_keys`] (the LCP/LCS + heuristic
///      path) when there are zero candidates — i.e. the stems share no
///      variable token across sides.
///
/// The returned pair of `Vec<RawKey>` has one entry per input stem on the
/// corresponding side. Empty input sides produce empty output.
pub fn extract_keys_cross(videos: &[&str], subtitles: &[&str]) -> (Vec<RawKey>, Vec<RawKey>) {
    if videos.is_empty() && subtitles.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Normalize + tokenize each side.
    let video_norms: Vec<String> = videos.iter().map(|s| normalize_stem(s).into_owned()).collect();
    let sub_norms: Vec<String> = subtitles.iter().map(|s| normalize_stem(s).into_owned()).collect();
    let video_tokens: Vec<Vec<Field<'_>>> = video_norms.iter().map(|s| split_fields(s)).collect();
    let sub_tokens: Vec<Vec<Field<'_>>> = sub_norms.iter().map(|s| split_fields(s)).collect();

    // Per-side statistics.
    let video_counts = count_token_occurrences(&video_tokens);
    let sub_counts = count_token_occurrences(&sub_tokens);
    let video_slot_sets = slot_value_sets(&video_tokens);
    let sub_slot_sets = slot_value_sets(&sub_tokens);

    let candidates =
        cross_side_candidates(&video_counts, &sub_counts, &video_slot_sets, &sub_slot_sets);
    let Some((video_slot, sub_slot)) = pick_best_slot_pair(
        &candidates,
        &video_tokens,
        &sub_tokens,
        &video_slot_sets,
        &sub_slot_sets,
    ) else {
        // No shared variable token -- fall back to the LCP/LCS + heuristic
        // path on each side independently.
        let v_refs: Vec<&str> = video_norms.iter().map(String::as_str).collect();
        let s_refs: Vec<&str> = sub_norms.iter().map(String::as_str).collect();
        return (extract_keys(&v_refs), extract_keys(&s_refs));
    };

    let extract_at = |tokens: &[Vec<Field<'_>>], slot: usize| -> Vec<RawKey> {
        tokens
            .iter()
            .map(|fields| match fields.get(slot) {
                Some(f) => RawKey(Some(f.text.to_string())),
                None => RawKey(None),
            })
            .collect()
    };

    (extract_at(&video_tokens, video_slot), extract_at(&sub_tokens, sub_slot))
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

    #[test]
    fn common_prefix_len_cjk_lands_on_char_boundary() {
        // Both stems share `中文 第` but diverge on `一` vs `二`. lcp must
        // stop at byte 10 (end of `第`), never at 11 (mid-`一`).
        let stems = ["中文 第一集", "中文 第二集"];
        let raw: Vec<&str> = stems.to_vec();
        let lcp = common_prefix_len(&raw);
        assert_eq!(lcp, "中文 第".len());
        assert!(raw[0].is_char_boundary(lcp));
    }

    #[test]
    fn common_suffix_len_cjk_lands_on_char_boundary() {
        // Shared suffix `集` (3 bytes); lcs must be 3, not 4 (which would
        // start at byte 10, mid-`一`).
        let stems = ["中文 第一集", "中文 第二集"];
        let raw: Vec<&str> = stems.to_vec();
        let lcs = common_suffix_len(&raw);
        assert_eq!(lcs, "集".len());
    }

    #[test]
    fn extract_keys_does_not_panic_on_kimetsu_stems() {
        let stems = [
            "[Up to 21°C] 鬼滅之刃 柱訓練篇 - 02 (Baha 1920x1080 AVC AAC MP4) [784C8989]",
            "[Up to 21°C] 鬼滅之刃 柱訓練篇 - 04 (Baha 1920x1080 AVC AAC MP4) [41B42367]",
        ];
        let raw: Vec<&str> = stems.to_vec();
        // Must not panic. Result content is not asserted here — that is
        // covered by the corpus fixture. The unit test only guards
        // against future regressions of the char-boundary panic.
        let _ = extract_keys(&raw);
    }

    #[test]
    fn extract_keys_does_not_panic_on_chinese_numerals_stems() {
        let stems = ["中文 第一集", "中文 第二集"];
        let raw: Vec<&str> = stems.to_vec();
        let _ = extract_keys(&raw);
    }

    #[test]
    fn heuristic_single_sp_with_various_separators() {
        // SP followed by a single separator and a digit must return "SP"
        // as a text key (not collapse onto the episode digit).
        assert_eq!(heuristic_single("Show - SP 01"), RawKey(Some("SP".into())));
        assert_eq!(heuristic_single("Show.SP.01"), RawKey(Some("SP".into())));
        assert_eq!(heuristic_single("Show - SP_ 01"), RawKey(Some("SP".into())));
        // OP variant.
        assert_eq!(heuristic_single("Show - OP 02"), RawKey(Some("OP".into())));
        // Bare digit (no SPECIAL_TAG) still extracts the digit.
        assert_eq!(heuristic_single("Show - 01"), RawKey(Some("01".into())));
    }

    #[test]
    fn normalize_key_chinese_numeral_returns_text() {
        assert_eq!(normalize_key(Some("一")), Some(EpisodeKey::Text("一".into())));
        assert_eq!(normalize_key(Some("二")), Some(EpisodeKey::Text("二".into())));
        // 壹/贰/... are the financial-form variants.
        assert_eq!(normalize_key(Some("壹")), Some(EpisodeKey::Text("壹".into())));
    }

    #[test]
    fn extract_keys_chinese_numerals_pair_across_stems() {
        let stems = ["中文 第一集", "中文 第二集"];
        let raw: Vec<&str> = stems.to_vec();
        let raws = extract_keys(&raw);
        let keys: Vec<Option<EpisodeKey>> =
            raws.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(keys[0], Some(EpisodeKey::Text("一".into())));
        assert_eq!(keys[1], Some(EpisodeKey::Text("二".into())));
    }

    #[test]
    fn normalize_stem_ascii_unchanged() {
        assert_eq!(normalize_stem("Show - 01 [1080p]").as_ref(), "Show - 01 [1080p]");
    }

    #[test]
    fn normalize_stem_collapses_whitespace_runs() {
        assert_eq!(normalize_stem("Show   -  01").as_ref(), "Show - 01");
        assert_eq!(normalize_stem("  leading and trailing  ").as_ref(), "leading and trailing");
        assert_eq!(normalize_stem("tab\there").as_ref(), "tab here");
    }

    #[test]
    fn normalize_stem_applies_nfkd_for_compat_variants() {
        // Compatibility ligatures DO get decomposed by NFKD (e.g. ﬁ -> fi).
        assert_eq!(normalize_stem("ﬁle").as_ref(), "file");
        // Script-variant forms (機動 vs 机动, の為 vs ため) are distinct
        // code points and are NOT collapsed by NFKD — this is documented
        // as `wontfix` in design.md and is why SubRenamer case 11/12
        // remain test-stable without additional heuristics.
        assert_ne!(normalize_stem("機動").as_ref(), "机动");
    }

    #[test]
    fn extract_keys_whitespace_fold_does_not_break_pairing() {
        // "视频 1 xyz" and "视频 1xyz" — after whitespace fold they share
        // the same core, so the digit "1" is correctly extracted as key.
        let stems = ["视频 1 xyz", "视频 77 test xyz"];
        let raw: Vec<&str> = stems.to_vec();
        let raws = extract_keys(&raw);
        let keys: Vec<Option<EpisodeKey>> =
            raws.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(keys[0], Some(EpisodeKey::Number("1".into())));
        assert_eq!(keys[1], Some(EpisodeKey::Number("77".into())));
    }

    // --- Task 1.3: FieldKind classification for CJK + ASCII digit groups.

    #[test]
    fn split_fields_classifies_pure_cjk_as_cjk_alpha() {
        let fields = split_fields("中文");
        assert_eq!(fields.len(), 1);
        assert!(matches!(fields[0].kind, FieldKind::CjkAlpha));
        assert_eq!(fields[0].text, "中文");
    }

    #[test]
    fn split_fields_classifies_kanji_run_as_cjk_alpha() {
        let fields = split_fields("機動");
        assert_eq!(fields.len(), 1);
        assert!(matches!(fields[0].kind, FieldKind::CjkAlpha));
        assert_eq!(fields[0].text, "機動");
    }

    #[test]
    fn split_fields_splits_cjk_with_separator() {
        // Pure CJK runs are kept as a single CjkAlpha field; ASCII
        // whitespace between them falls through as a Separator.
        let fields = split_fields("中文 第一集");
        assert_eq!(fields.len(), 3);
        assert!(matches!(fields[0].kind, FieldKind::CjkAlpha));
        assert_eq!(fields[0].text, "中文");
        assert!(matches!(fields[1].kind, FieldKind::Separator));
        assert!(matches!(fields[2].kind, FieldKind::CjkAlpha));
        assert_eq!(fields[2].text, "第一集");
    }

    // --- Task 2.4: cross-side perspective: episode digit slot is variable
    //                on both sides even when its specific values differ.

    #[test]
    fn cross_side_basic_picks_episode_digit_slot() {
        // From the Basic corpus case: each side's digit slot has 3 distinct
        // values across stems. The token `01` only appears in 1 stem per
        // side, but its slot is variable on both sides — so the cross-side
        // algorithm picks the slot pair (5, 9) and extracts the value at
        // that slot from every stem, yielding the correct episode digits.
        let videos = ["abc.S02E01.123", "abc.S02E02.abc", "abc.S02E03.ccc"];
        let subs =
            ["[SubGroup] def.S02E01.xyz", "[SubGroup] def.S02E02.abc", "[SubGroup] def.S02E04.kkk"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![
                Some(EpisodeKey::Number("1".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("3".into())),
            ]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Number("1".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("4".into())),
            ]
        );
    }

    // --- Task 3.3: 4 wontfix cases now all pass via cross-side.

    #[test]
    fn cross_side_breaking_bad_picks_episode_digit() {
        let videos = [
            "Breaking.Bad.S03E04.Green.Light.2160p.Netflix.WEB-DL.DDP.5.1.H.265",
            "Breaking.Bad.S03E12.Half.Measures.2160p.Netflix.WEB-DL.DDP.5.1.H.265",
            "Breaking.Bad.S03E100.Test.Test.2160p.Netflix.WEB-DL.DDP.5.1.H.265",
        ];
        let subs = [
            "breaking.bad.s03e12.720p.hdtv.x264-ctu.en",
            "Breaking.Bad.S03E04.720p.HDTV.x264-CTU.en",
            "Breaking.Bad.S03E123.720p.HDTV.x264-CTU.en",
        ];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![
                Some(EpisodeKey::Number("4".into())),
                Some(EpisodeKey::Number("12".into())),
                Some(EpisodeKey::Number("100".into())),
            ]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Number("12".into())),
                Some(EpisodeKey::Number("4".into())),
                Some(EpisodeKey::Number("123".into())),
            ]
        );
    }

    #[test]
    fn cross_side_blackdoor_picks_episode_digit() {
        let videos = [
            "Black Mirror (2011)(1080p)(Webdl)(VP9)(14 lang-AAC- 2.0) (S01) PHDTeam",
            "Black Mirror (2011)(1080p)(Webdl)(VP9)(14 lang-AAC- 2.0) (S11) PHDTeam",
        ];
        let subs =
            ["Black Mirror_S01E01_Patnáct milionů meritů", "Black Mirror_S01E11_P15milMeritů-EN"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![Some(EpisodeKey::Number("1".into())), Some(EpisodeKey::Number("11".into())),]
        );
        assert_eq!(
            s_keys,
            vec![Some(EpisodeKey::Number("1".into())), Some(EpisodeKey::Number("11".into())),]
        );
    }

    #[test]
    fn cross_side_haikyuu_picks_episode_digit() {
        let videos = [
            "[Kamigami] Haikyuu!! S2 - 09 [1920x1080 HEVC AAC Sub(Chs,Cht,Jap)]",
            "[Kamigami] Haikyuu!! S2 - 10 [1920x1080 HEVC AAC Sub(Chs,Cht,Jap)]",
        ];
        let subs = [
            "[YYDM-11FANS][Haikyuu!!][09][BDRIP][720P][X264-10bit_AAC][40A7E056].en",
            "[YYDM-11FANS][Haikyuu!!][09][BDRIP][720P][X264-10bit_AAC][40A7E056].sc",
            "[YYDM-11FANS][Haikyuu!!][09][BDRIP][720P][X264-10bit_AAC][40A7E056].tc",
            "[YYDM-11FANS][Haikyuu!!][10][BDRIP][720P][X264-10bit_AAC][6FDEFD72].sc",
            "[YYDM-11FANS][Haikyuu!!][10][BDRIP][720P][X264-10bit_AAC][6FDEFD72].tc",
        ];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![Some(EpisodeKey::Number("9".into())), Some(EpisodeKey::Number("10".into())),]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Number("9".into())),
                Some(EpisodeKey::Number("9".into())),
                Some(EpisodeKey::Number("9".into())),
                Some(EpisodeKey::Number("10".into())),
                Some(EpisodeKey::Number("10".into())),
            ]
        );
    }

    // --- Task 6.1: fallback when no shared variable token.

    #[test]
    fn cross_side_falls_back_when_no_shared_token() {
        // Video and subtitle share no common token; each side has its
        // own variable slot that the other side knows nothing about.
        // The cross-side algorithm falls back to per-side extract_keys
        // (LCP/LCS + heuristic) so each side still gets a usable key.
        let videos = ["AAA_one", "AAA_two", "AAA_three"];
        let subs = ["BBB_aaa", "BBB_bbb", "BBB_ccc"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        // Per-side fallback: video side gets the variable trailing alpha
        // (one/two/three -> ONE/TWO/THREE text keys); subtitle side
        // gets the variable trailing alpha (aaa/bbb/ccc -> AAA/BBB/CCC).
        assert_eq!(
            v_keys,
            vec![
                Some(EpisodeKey::Text("ONE".into())),
                Some(EpisodeKey::Text("TWO".into())),
                Some(EpisodeKey::Text("THREE".into())),
            ]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Text("AAA".into())),
                Some(EpisodeKey::Text("BBB".into())),
                Some(EpisodeKey::Text("CCC".into())),
            ]
        );
    }

    #[test]
    fn cross_side_falls_back_to_per_side_when_only_anchor_is_shared() {
        // Both sides share a constant token (`01`) but the *variable*
        // parts differ (`x` / `y` on video, `a` / `b` on sub) and don't
        // appear on the other side. With no shared variable token the
        // cross-side algorithm falls back to per-side extract_keys,
        // which picks each side's own variable Alpha field.
        let videos = ["prefix_01_x", "prefix_01_y"];
        let subs = ["other_01_a", "other_01_b"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![Some(EpisodeKey::Text("X".into())), Some(EpisodeKey::Text("Y".into())),]
        );
        assert_eq!(
            s_keys,
            vec![Some(EpisodeKey::Text("A".into())), Some(EpisodeKey::Text("B".into())),]
        );
    }

    // --- Task 6.2: SPECIAL_TAGS + CHINESE_NUMERALS in cross-side path.

    #[test]
    fn cross_side_special_tag_in_mixed_variable_slot_yields_text_key() {
        // Mixed set where the variable slot contains one SPECIAL_TAG
        // and two numeric values; the cross-side algorithm picks the
        // slot, and `normalize_key` classifies `SP` as a text key
        // while the digit values become numeric keys.
        let videos = ["Death_Note - SP 01", "Death_Note - 02", "Death_Note - 03"];
        let subs = ["Death_Note - SP 01", "Death_Note - 02", "Death_Note - 03"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![
                Some(EpisodeKey::Text("SP".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("3".into())),
            ]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Text("SP".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("3".into())),
            ]
        );
    }

    #[test]
    fn cross_side_chinese_numeral_pair_yields_text_key() {
        // The cross-side algorithm extracts the whole CJK run
        // (`第一集` / `第二集`) since the new regex keeps CJK alpha
        // runs intact. `normalize_key` then classifies them as text
        // keys via the catch-all text path (the byte-level
        // `starts_with` doesn't recognize `第` as a numeral, so the
        // full run text is preserved as the text key). Both sides
        // produce matching texts, so pairing succeeds.
        let videos = ["中文 第一集", "中文 第二集"];
        let subs = ["【字幕】中文 第一集", "【字幕】中文 第二集"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![Some(EpisodeKey::Text("第一集".into())), Some(EpisodeKey::Text("第二集".into())),]
        );
        assert_eq!(
            s_keys,
            vec![Some(EpisodeKey::Text("第一集".into())), Some(EpisodeKey::Text("第二集".into())),]
        );
    }

    // --- Task 6.3: NFKD + whitespace folding still effective cross-side.

    #[test]
    fn cross_side_nfkd_pairs_simplified_traditional() {
        // Video uses simplified, subtitle has both simplified and
        // traditional (機動 vs 机动). NFKD normalizes both, and the
        // cross-side algorithm pairs the episode digit even though
        // the show title contains mixed scripts.
        let videos = [
            "[AI-Raws] 机动警察パトレイバー #1",
            "[AI-Raws] 机动警察パトレイバー #2",
            "[AI-Raws] 机动警察パトレイバー #10",
        ];
        let subs = [
            "[字幕] 机动警察 機動警察パトレイバー 01",
            "[字幕] 机动警察 機動警察パトレイバー 02",
            "[字幕] 机动警察 機動警察パトレイバー 10",
        ];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![
                Some(EpisodeKey::Number("1".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("10".into())),
            ]
        );
        assert_eq!(
            s_keys,
            vec![
                Some(EpisodeKey::Number("1".into())),
                Some(EpisodeKey::Number("2".into())),
                Some(EpisodeKey::Number("10".into())),
            ]
        );
    }

    #[test]
    fn cross_side_whitespace_fold_pairs() {
        // "视频 1 xyz" vs "字幕 1xyz" — after whitespace fold on each
        // side the cores share the same digit, and the cross-side
        // algorithm still pairs the two stems.
        let videos = ["视频 1 xyz", "视频 77 test xyz"];
        let subs = ["字幕 1xyz", "字幕 77test xyz"];
        let v_ref: Vec<&str> = videos.to_vec();
        let s_ref: Vec<&str> = subs.to_vec();
        let (v_raw, s_raw) = extract_keys_cross(&v_ref, &s_ref);
        let v_keys: Vec<Option<EpisodeKey>> =
            v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        let s_keys: Vec<Option<EpisodeKey>> =
            s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect();
        assert_eq!(
            v_keys,
            vec![Some(EpisodeKey::Number("1".into())), Some(EpisodeKey::Number("77".into())),]
        );
        assert_eq!(
            s_keys,
            vec![Some(EpisodeKey::Number("1".into())), Some(EpisodeKey::Number("77".into())),]
        );
    }
}
