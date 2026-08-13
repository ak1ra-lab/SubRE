//! Integration tests driven by `tests/fixtures/match_cases.json`.
//!
//! Each case declares video stems, subtitle stems, and the expected pairs.
//! The test feeds them into the matcher and asserts the pair set matches.

use std::path::PathBuf;

use serde::Deserialize;

use subtitle_renamer::core::matcher::{FileEntry, Matcher};

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    videos: Vec<String>,
    subtitles: Vec<String>,
    expected_pairs: Vec<[String; 2]>,
}

fn load_fixtures() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("match_cases.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("parse match_cases.json")
}

fn entry(stem: &str) -> FileEntry {
    FileEntry::from_path(PathBuf::from(format!("/tmp/{stem}")))
}

#[test]
fn corpus_pairs_match_expected() {
    let fx = load_fixtures();
    let matcher = Matcher::new();
    for case in &fx.cases {
        let videos: Vec<FileEntry> = case.videos.iter().map(|s| entry(s)).collect();
        let subtitles: Vec<FileEntry> = case.subtitles.iter().map(|s| entry(s)).collect();
        let result = matcher.match_files(&videos, &subtitles, None, None);

        let mut actual: Vec<[String; 2]> = Vec::new();
        for g in &result.groups {
            if let Some(v) = &g.video {
                for sub in &g.subtitles {
                    actual.push([v.stem.clone(), sub.stem.clone()]);
                }
            }
        }
        actual.sort();
        let mut expected = case.expected_pairs.clone();
        expected.sort();

        assert_eq!(
            actual, expected,
            "case `{}` produced a different pairing than expected.\nactual:   {actual:?}\nexpected: {expected:?}",
            case.name
        );

        // Unmatched subtitles must not be silently dropped.
        let unmatched_stems: Vec<String> =
            result.unmatched_subtitles().map(|s| s.stem.clone()).collect();
        let expected_unmatched: Vec<String> = subtitles
            .iter()
            .filter(|s| !case.expected_pairs.iter().any(|p| p[1] == s.stem))
            .map(|s| s.stem.clone())
            .collect();
        let mut unmatched_stems = unmatched_stems;
        unmatched_stems.sort();
        let mut expected_unmatched = expected_unmatched;
        expected_unmatched.sort();
        assert_eq!(
            unmatched_stems, expected_unmatched,
            "case `{}` unmatched subtitle set differs.\nactual:   {unmatched_stems:?}\nexpected: {expected_unmatched:?}",
            case.name
        );
    }
}
