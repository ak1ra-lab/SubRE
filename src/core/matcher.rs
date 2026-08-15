//! Video ↔ subtitle matcher.
//!
//! Given a set of normalized episode keys for the video side and the
//! subtitle side, produce a [`MatchResult`] containing:
//!   - paired groups (video files with zero or more subtitles);
//!   - unmatched videos;
//!   - unmatched subtitles.
//!
//! Constraints (per spec):
//!   - Each subtitle belongs to at most one video.
//!   - A video may pair with many subtitles (e.g. one 简体 + one 繁体).
//!   - Unmatched items are preserved, never silently dropped.
//!   - Manual overrides: a subtitle can be assigned to / detached from a
//!     video explicitly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::parse::{
    EpisodeKey, ExtensionRegistry, FileCategory, RawKey, extract_keys, extract_keys_cross,
    normalize_key,
};

/// A single file dropped into the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: std::path::PathBuf,
    pub stem: String,
    pub ext: String,
}

impl FileEntry {
    pub fn from_path(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let (stem, ext) = super::parse::ExtensionRegistry::split_basename(&path);
        Self { path, stem, ext }
    }
}

/// A pairing group: one video and zero-or-more subtitles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairGroup {
    pub video: Option<FileEntry>,
    pub subtitles: Vec<FileEntry>,
    pub key: EpisodeKey,
    /// True when subtitles are paired only because of a manual assignment
    /// (i.e. the key was not derived automatically).
    pub manual: bool,
}

/// Final matching result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatchResult {
    pub groups: Vec<PairGroup>,
}

impl MatchResult {
    /// All paired groups (video + ≥1 subtitle OR video-only with no subtitles).
    pub fn paired(&self) -> impl Iterator<Item = &PairGroup> {
        self.groups.iter().filter(|g| g.video.is_some())
    }

    pub fn unmatched_videos(&self) -> impl Iterator<Item = &FileEntry> {
        self.groups
            .iter()
            .filter(|g| g.video.is_some() && g.subtitles.is_empty())
            .filter_map(|g| g.video.as_ref())
    }

    pub fn unmatched_subtitles(&self) -> impl Iterator<Item = &FileEntry> {
        self.groups
            .iter()
            .filter(|g| g.video.is_none() && !g.subtitles.is_empty())
            .flat_map(|g| g.subtitles.iter())
    }
}

/// Drives the matching process and exposes manual override APIs.
#[derive(Debug, Default)]
pub struct Matcher;

impl Matcher {
    pub fn new() -> Self {
        Self
    }

    /// Match `videos` and `subtitles`. A single cross-side alignment pass
    /// picks one slot pair and favors the dominant naming style, so mixed
    /// batches are re-matched iteratively over the leftover subset until no
    /// progress is made. Optional per-side regex overrides are consulted when
    /// provided.
    pub fn match_files(
        &self,
        videos: &[FileEntry],
        subtitles: &[FileEntry],
        video_regex: Option<&str>,
        subtitle_regex: Option<&str>,
    ) -> MatchResult {
        let mut result = Self::match_once(videos, subtitles, video_regex, subtitle_regex);

        let mut prev_leftover = videos.len().saturating_add(subtitles.len()).saturating_add(1);
        loop {
            let leftover_videos: Vec<FileEntry> = result.unmatched_videos().cloned().collect();
            let leftover_subs: Vec<FileEntry> = result.unmatched_subtitles().cloned().collect();
            if leftover_videos.is_empty() || leftover_subs.is_empty() {
                break;
            }
            let leftover = leftover_videos.len().saturating_add(leftover_subs.len());
            if leftover >= prev_leftover {
                break;
            }
            prev_leftover = leftover;

            // Only re-match when the subset still qualifies for cross-side
            // alignment. Small leftovers fall back to per-file heuristics,
            // which extract the wrong key (e.g. the season `S02` instead of
            // the episode) and would produce spurious pairs.
            if leftover_videos.len() <= 1
                || leftover_subs.len() <= 1
                || video_regex.is_some()
                || subtitle_regex.is_some()
            {
                break;
            }

            let sub =
                Self::match_once(&leftover_videos, &leftover_subs, video_regex, subtitle_regex);
            result.groups.retain(|g| !is_leftover(g));
            result.groups.extend(sub.groups);
        }

        // Deterministic ordering, mirroring the original BTreeMap iteration.
        result.groups.sort_by(|a, b| a.key.cmp(&b.key));
        result
    }

    /// Single-pass match over `videos` and `subtitles`.
    fn match_once(
        videos: &[FileEntry],
        subtitles: &[FileEntry],
        video_regex: Option<&str>,
        subtitle_regex: Option<&str>,
    ) -> MatchResult {
        // When one side has a single file and the other has many, force
        // per-file heuristic on both sides. The diff algorithm picks the
        // "most-varying" field within a side, which is wrong for 1vN
        // matching (the subtitles may vary on a *language* token rather
        // than the *episode* token — we want them all paired to the single
        // video based on whatever episode each one carries).
        let one_to_many_videos =
            !videos.is_empty() && !subtitles.is_empty() && subtitles.len() > 1 && videos.len() == 1;
        let one_to_many_subtitles =
            !videos.is_empty() && !subtitles.is_empty() && videos.len() > 1 && subtitles.len() == 1;
        let force_heuristic_videos = one_to_many_videos || one_to_many_subtitles;
        let force_heuristic_subtitles = one_to_many_subtitles || one_to_many_videos;
        // Cross-side token alignment kicks in when both sides have more than
        // one stem and neither side is using a user-supplied regex override.
        // Regex overrides and 1vN scenarios keep the legacy per-side path so
        // existing behavior is preserved.
        let use_cross_side = !force_heuristic_videos
            && !force_heuristic_subtitles
            && videos.len() > 1
            && subtitles.len() > 1
            && video_regex.is_none()
            && subtitle_regex.is_none();
        let (video_keys, subtitle_keys) = if use_cross_side {
            let video_stems: Vec<&str> = videos.iter().map(|f| f.stem.as_str()).collect();
            let subtitle_stems: Vec<&str> = subtitles.iter().map(|f| f.stem.as_str()).collect();
            let (v_raw, s_raw) = extract_keys_cross(&video_stems, &subtitle_stems);
            (
                v_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect(),
                s_raw.iter().map(|r| normalize_key(r.0.as_deref())).collect(),
            )
        } else {
            let v = if force_heuristic_videos {
                heuristic_keys(videos)
            } else {
                compute_keys(videos, video_regex)
            };
            let s = if force_heuristic_subtitles {
                heuristic_keys(subtitles)
            } else {
                compute_keys(subtitles, subtitle_regex)
            };
            (v, s)
        };

        let mut by_key: BTreeMap<EpisodeKey, PairGroup> = BTreeMap::new();

        // 1v1 fast path: exactly one video and exactly one subtitle and
        // both extracted keys are absent (or both present and equal).
        if videos.len() == 1 && subtitles.len() == 1 {
            let v_key = &video_keys[0];
            let s_key = &subtitle_keys[0];
            let same = match (v_key, s_key) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            };
            if same {
                let key = v_key
                    .clone()
                    .or_else(|| s_key.clone())
                    .unwrap_or(EpisodeKey::Text(String::new()));
                let mut group = PairGroup {
                    video: Some(videos[0].clone()),
                    subtitles: vec![subtitles[0].clone()],
                    key,
                    manual: false,
                };
                // Normalize: replace possibly-empty text key with a
                // synthetic numeric key for 1v1 single-file pairings so
                // downstream consumers can rely on a non-empty key.
                if matches!(group.key, EpisodeKey::Text(ref s) if s.is_empty()) {
                    group.key = EpisodeKey::Text("__single__".into());
                }
                return MatchResult { groups: vec![group] };
            }
        }

        // Group videos by key.
        for (video, key) in videos.iter().zip(video_keys.iter()) {
            if let Some(k) = key {
                let entry = by_key.entry(k.clone()).or_insert_with(|| PairGroup {
                    video: None,
                    subtitles: Vec::new(),
                    key: k.clone(),
                    manual: false,
                });
                if entry.video.is_none() {
                    entry.video = Some(video.clone());
                } else {
                    // Multiple videos with the same key: keep the first;
                    // the second becomes unmatched and gets its own
                    // single-video group.
                    let solo = PairGroup {
                        video: Some(video.clone()),
                        subtitles: Vec::new(),
                        key: EpisodeKey::Text(format!("__dupe_video:{}", video.stem)),
                        manual: false,
                    };
                    by_key
                        .entry(EpisodeKey::Text(format!("__dupe_video:{}", video.stem)))
                        .or_insert(solo);
                }
            } else {
                // No key for this video — preserve as unmatched.
                let synthetic = EpisodeKey::Text(format!("__unmatched_video:{}", video.stem));
                by_key.entry(synthetic.clone()).or_insert(PairGroup {
                    video: Some(video.clone()),
                    subtitles: Vec::new(),
                    key: synthetic,
                    manual: false,
                });
            }
        }

        // Pair subtitles: at most one subtitle per key-group's video.
        // A video can have multiple subtitles with the same key (e.g.
        // 简体 + 繁体): they all attach.
        for (sub, key) in subtitles.iter().zip(subtitle_keys.iter()) {
            if let Some(k) = key
                && let Some(group) = by_key.get_mut(k)
            {
                group.subtitles.push(sub.clone());
                continue;
            }
            // Unmatched subtitle (no key or no matching video).
            let synthetic = EpisodeKey::Text(format!(
                "__unmatched_sub:{}:{}",
                key.as_ref().map(|k| k.as_str().to_string()).unwrap_or_default(),
                sub.stem
            ));
            by_key.entry(synthetic.clone()).or_insert(PairGroup {
                video: None,
                subtitles: vec![sub.clone()],
                key: synthetic,
                manual: false,
            });
        }

        let mut groups: Vec<PairGroup> = by_key.into_values().collect();
        for group in &mut groups {
            // Deterministic, cross-group-consistent subtitle ordering.
            group.subtitles.sort_by(|a, b| a.stem.cmp(&b.stem));
        }
        MatchResult { groups }
    }

    /// Manually attach a subtitle to a video (by index in the result).
    pub fn manual_attach(
        &self,
        result: &mut MatchResult,
        video_index: usize,
        subtitle_index: usize,
    ) {
        let video = result.groups.get(video_index).and_then(|g| g.video.clone());
        let subtitle = result.groups.get(subtitle_index).and_then(|g| g.subtitles.first().cloned());
        if let (Some(_video), Some(subtitle)) = (video, subtitle) {
            // Remove the subtitle from its current group.
            result.groups[subtitle_index].subtitles.clear();
            // Attach to the video group.
            result.groups[video_index].subtitles.push(subtitle);
            result.groups[video_index].manual = true;
        }
    }

    /// Detach a subtitle from a video.
    pub fn manual_detach(
        &self,
        result: &mut MatchResult,
        video_index: usize,
        subtitle_index: usize,
    ) {
        if let Some(group) = result.groups.get_mut(video_index)
            && subtitle_index < group.subtitles.len()
        {
            group.subtitles.remove(subtitle_index);
            group.manual = true;
        }
    }
}

fn compute_keys(files: &[FileEntry], regex_override: Option<&str>) -> Vec<Option<EpisodeKey>> {
    let stems: Vec<&str> = files.iter().map(|f| f.stem.as_str()).collect();
    let raws: Vec<RawKey> = if let Some(pat) = regex_override {
        super::parse::extract_with_regex(&stems, pat)
    } else {
        extract_keys(&stems)
    };
    raws.iter().map(|r| normalize_key(r.0.as_deref())).collect()
}

/// True for groups that contribute nothing to a match: a lone video, or a
/// lone subtitle. These are the leftovers re-matched in [`Matcher::match_files`].
fn is_leftover(group: &PairGroup) -> bool {
    (group.video.is_some() && group.subtitles.is_empty())
        || (group.video.is_none() && !group.subtitles.is_empty())
}

fn heuristic_keys(files: &[FileEntry]) -> Vec<Option<EpisodeKey>> {
    files
        .iter()
        .map(|f| {
            let raw = super::parse::heuristic_single(&f.stem);
            normalize_key(raw.0.as_deref())
        })
        .collect()
}

/// Collect recognized media files from `dir` non-recursively, classifying
/// each by extension via `registry`. Returns `(videos, subtitles)`; files
/// with unknown extensions are skipped and subdirectories are not descended.
pub fn collect_media_files(
    registry: &ExtensionRegistry,
    dir: &std::path::Path,
) -> (Vec<FileEntry>, Vec<FileEntry>) {
    let mut videos = Vec::new();
    let mut subtitles = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (videos, subtitles);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match registry.categorize(&path) {
            FileCategory::Video => videos.push(FileEntry::from_path(path)),
            FileCategory::Subtitle => subtitles.push(FileEntry::from_path(path)),
            FileCategory::Unknown => {}
        }
    }
    (videos, subtitles)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str) -> FileEntry {
        FileEntry::from_path(PathBuf::from(name))
    }

    #[test]
    fn single_video_single_subtitle_pair_directly() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.S01E01.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        assert!(r.groups[0].video.is_some());
        assert_eq!(r.groups[0].subtitles.len(), 1);
    }

    #[test]
    fn multi_video_multi_subtitle_pair_by_key() {
        let m = Matcher::new();
        let v = vec![
            entry("[Group] Show - 01 [1080p].mkv"),
            entry("[Group] Show - 02 [1080p].mkv"),
            entry("[Group] Show - 03 [1080p].mkv"),
        ];
        let s = vec![
            entry("Show.S01E01.chs.ass"),
            entry("Show.S01E02.chs.ass"),
            entry("Show.S01E03.chs.ass"),
        ];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 3);
        for g in &r.groups {
            assert!(g.video.is_some(), "video present: {:?}", g.key);
            assert_eq!(g.subtitles.len(), 1);
        }
        assert_eq!(r.unmatched_videos().count(), 0);
        assert_eq!(r.unmatched_subtitles().count(), 0);
    }

    #[test]
    fn mixed_naming_styles_pair_across_styles() {
        let m = Matcher::new();
        // Uniform video side; subtitle side mixes two layouts that place the
        // episode token at different field offsets, so a single cross-side
        // alignment pass can only satisfy one of them.
        let videos: Vec<FileEntry> =
            (1..=6).map(|i| entry(&format!("Death_Note - {i:02}.mkv"))).collect();
        // Style A (eps 1-4): the extra [JP_GB_BIG5] segment pushes the
        // episode token to a deeper field slot.
        let style_a: Vec<FileEntry> = (1..=4)
            .map(|i| {
                entry(&format!("[X2&CASO][Death_Note][JP_GB_BIG5][{i:02}][DVDRIP]_track3.ass"))
            })
            .collect();
        // Style B (eps 5-6): no extra segment, episode token at a shallower slot.
        let style_b: Vec<FileEntry> = (5..=6)
            .map(|i| entry(&format!("[X2-Raws][Death_Note][{i:02}][DVDRIP].sn.ass")))
            .collect();
        let subtitles: Vec<FileEntry> = style_a.into_iter().chain(style_b).collect();

        let r = m.match_files(&videos, &subtitles, None, None);
        assert_eq!(r.unmatched_videos().count(), 0, "every video should pair");
        assert_eq!(r.unmatched_subtitles().count(), 0, "every subtitle should pair");

        let mut pairs: Vec<(String, String)> = r
            .groups
            .iter()
            .filter_map(|g| {
                g.video
                    .as_ref()
                    .zip(g.subtitles.first())
                    .map(|(v, s)| (v.stem.clone(), s.stem.clone()))
            })
            .collect();
        pairs.sort();
        assert_eq!(pairs.len(), 6);
        // Style B subtitles pair to their own videos, not to style A slots.
        for ep in ["05", "06"] {
            assert!(
                pairs.iter().any(|(v, s)| v.ends_with(ep)
                    && s.contains("X2-Raws")
                    && s.contains(&format!("[{ep}]"))),
                "episode {ep} should pair to its [X2-Raws] subtitle: {pairs:?}"
            );
        }
    }

    #[test]
    fn iteration_terminates_and_preserves_unmatched() {
        let m = Matcher::new();
        let v = vec![
            entry("[G] Show - 01.mkv"),
            entry("[G] Show - 02.mkv"),
            entry("[G] Show - 03.mkv"),
        ];
        let s = vec![
            entry("Show.S01E01.chs.ass"),
            entry("Show.S01E02.chs.ass"),
            entry("Show.S01E03.chs.ass"),
            entry("Other.NCOP.ass"),
        ];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.unmatched_videos().count(), 0);
        assert_eq!(r.unmatched_subtitles().count(), 1);
        assert!(r.unmatched_subtitles().any(|s| s.stem.contains("NCOP")));
    }

    #[test]
    fn one_to_many_video_pair() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.S01E01.chs.ass"), entry("Show.S01E01.cht.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].subtitles.len(), 2);
    }

    #[test]
    fn subtitles_sorted_within_group() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        // Deliberately out-of-order ingestion.
        let s = vec![
            entry("Show.S01E01._track5.ass"),
            entry("Show.S01E01._track3.ass"),
            entry("Show.S01E01._track4.ass"),
        ];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        let stems: Vec<&str> = r.groups[0].subtitles.iter().map(|f| f.stem.as_str()).collect();
        assert_eq!(
            stems,
            vec!["Show.S01E01._track3", "Show.S01E01._track4", "Show.S01E01._track5"]
        );
    }

    #[test]
    fn idx_sub_pair_stays_adjacent() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.S01E01.idx"), entry("Show.S01E01.sub")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        let exts: Vec<&str> = r.groups[0].subtitles.iter().map(|f| f.ext.as_str()).collect();
        assert_eq!(exts, vec!["idx", "sub"]);
    }

    #[test]
    fn unmatched_items_are_preserved() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv"), entry("Show - 02.mkv")];
        let s = vec![entry("Show.S01E01.chs.ass"), entry("Show.S01E03.chs.ass")];
        let r = m.match_files(&v, &s, None, None);
        // 1 group with ep 1 (paired), 1 group with ep 2 (unmatched video),
        // 1 group with ep 3 (unmatched subtitle).
        assert_eq!(r.unmatched_videos().count(), 1);
        assert_eq!(r.unmatched_subtitles().count(), 1);
    }

    #[test]
    fn text_key_pairing() {
        let m = Matcher::new();
        let v = vec![entry("Show - NCOP.mkv")];
        let s = vec![entry("Show.NCOP.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].subtitles.len(), 1);
    }

    #[test]
    fn text_key_unmatched() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.SP01.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.unmatched_subtitles().count(), 1);
    }

    #[test]
    fn manual_attach_and_detach() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv"), entry("Show - 02.mkv")];
        let s = vec![entry("Show.S01E01.chs.ass"), entry("Show.S01E03.chs.ass")];
        let mut r = m.match_files(&v, &s, None, None);
        // Attach the unmatched subtitle (S01E03) to the video "Show - 02".
        // Find the unmatched-subtitle group and the "Show - 02" video group
        // by inspection.
        let sub_idx = r
            .groups
            .iter()
            .position(|g| g.video.is_none() && !g.subtitles.is_empty())
            .expect("unmatched subtitle group");
        let video_idx = r
            .groups
            .iter()
            .position(|g| g.video.as_ref().is_some_and(|v| v.stem.contains("02")))
            .expect("video 02 group");
        m.manual_attach(&mut r, video_idx, sub_idx);
        // After attach: target group has 1 subtitle, source has 0.
        assert_eq!(r.groups[video_idx].subtitles.len(), 1);
        assert_eq!(r.groups[sub_idx].subtitles.len(), 0);
        assert!(r.groups[video_idx].manual);
    }

    #[test]
    fn regex_override_recovers_pairing() {
        let m = Matcher::new();
        let v = vec![entry("foo_01.mkv"), entry("foo_02.mkv")];
        let s = vec![entry("bar_ep1.ass"), entry("bar_ep2.ass")];
        let r = m.match_files(&v, &s, None, Some(r"(?i)ep(\d+)"));
        assert_eq!(r.groups.len(), 2);
        for g in &r.groups {
            if g.video.is_some() {
                assert_eq!(g.subtitles.len(), 1);
            }
        }
    }

    #[test]
    fn leading_zero_normalization_pairs_e1_with_01() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.E1.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].subtitles.len(), 1);
    }

    #[test]
    fn revision_marker_v2_pairs_with_base() {
        let m = Matcher::new();
        let v = vec![entry("Show - 01.mkv")];
        let s = vec![entry("Show.01v2.ass")];
        let r = m.match_files(&v, &s, None, None);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].subtitles.len(), 1);
    }

    #[test]
    fn match_files_37x37_within_time_budget() {
        // Regression guard: the cross-side alignment scorer used to
        // recompile its episode-key regexes on every `normalize_key` call,
        // making a 37x37 match take ~52s (release). A generous budget only
        // trips on that class of catastrophic regression, not on slow CI.
        let m = Matcher::new();
        let videos: Vec<FileEntry> =
            (1..=37).map(|i| entry(&format!("[Group] Show - {i:02} [1080p].mkv"))).collect();
        let subtitles: Vec<FileEntry> =
            (1..=37).map(|i| entry(&format!("Show.S01E{i:02}.chs.ass"))).collect();
        let start = std::time::Instant::now();
        let r = m.match_files(&videos, &subtitles, None, None);
        let elapsed = start.elapsed();
        assert_eq!(r.groups.len(), 37);
        assert!(elapsed.as_secs() < 5, "37x37 match took {elapsed:?}");
    }

    #[test]
    fn collect_media_files_nonrecursive_classifies() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sr_scan_{}_{}_{}",
            std::process::id(),
            n,
            std::thread::current().name().unwrap_or("main")
        ));
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("a.mkv"), b"v").unwrap();
        std::fs::write(dir.join("b.ass"), b"s").unwrap();
        std::fs::write(dir.join("c.txt"), b"x").unwrap();
        std::fs::write(nested.join("d.mkv"), b"v").unwrap();

        let reg = ExtensionRegistry::new();
        let (videos, subtitles) = collect_media_files(&reg, &dir);
        assert_eq!(videos.len(), 1);
        assert_eq!(subtitles.len(), 1);
        assert_eq!(videos[0].stem, "a");
        assert_eq!(subtitles[0].stem, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
