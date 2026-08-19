//! Integration tests driven by `tests/fixtures/match_cases.json` and
//! `tests/fixtures/match_cases_subrenamer.json`.
//!
//! Each case declares video stems, subtitle stems, and the expected pairs.
//! The test feeds them into the matcher and asserts the pair set matches.
//! Each case is reported as `pass` / `fail` / `panic` so that a single
//! failing case is debuggable without binary search.

use std::path::PathBuf;

use serde::Deserialize;

use subre::core::matcher::{FileEntry, Matcher};

#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(default)]
    #[allow(dead_code)]
    _about: Option<String>,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    #[serde(default)]
    wontfix: bool,
    #[serde(default)]
    wontfix_reason: Option<String>,
    videos: Vec<String>,
    subtitles: Vec<String>,
    expected_pairs: Vec<[String; 2]>,
}

const FIXTURE_FILES: &[&str] = &["match_cases.json", "match_cases_subrenamer.json"];

fn load_fixtures() -> Vec<Case> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let mut out = Vec::new();
    for fname in FIXTURE_FILES {
        let path = dir.join(fname);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let fx: Fixture =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        out.extend(fx.cases);
    }
    out
}

fn entry(stem: &str) -> FileEntry {
    FileEntry::from_path(PathBuf::from(format!("/tmp/{stem}")))
}

fn normalize_pair(p: &[String; 2]) -> [String; 2] {
    [p[0].clone(), p[1].clone()]
}

fn run_case(case: &Case) -> Result<(), String> {
    let matcher = Matcher::new();
    let videos: Vec<FileEntry> = case.videos.iter().map(|s| entry(s)).collect();
    let subtitles: Vec<FileEntry> = case.subtitles.iter().map(|s| entry(s)).collect();
    let result = matcher.match_files(&videos, &subtitles, None, None);

    let mut actual: Vec<[String; 2]> = Vec::new();
    for g in &result.groups {
        if let Some(v) = &g.video {
            for sub in &g.subtitles {
                actual.push(normalize_pair(&[v.stem.clone(), sub.stem.clone()]));
            }
        }
    }
    actual.sort();
    let mut expected = case.expected_pairs.clone();
    expected.sort();

    if actual != expected {
        return Err(format!("pairing mismatch\n  actual:   {actual:?}\n  expected: {expected:?}"));
    }

    let mut unmatched_stems: Vec<String> =
        result.unmatched_subtitles().map(|s| s.stem.clone()).collect();
    unmatched_stems.sort();
    let mut expected_unmatched: Vec<String> = subtitles
        .iter()
        .filter(|s| !case.expected_pairs.iter().any(|p| p[1] == s.stem))
        .map(|s| s.stem.clone())
        .collect();
    expected_unmatched.sort();
    if unmatched_stems != expected_unmatched {
        return Err(format!(
            "unmatched set mismatch\n  actual:   {unmatched_stems:?}\n  expected: {expected_unmatched:?}"
        ));
    }
    Ok(())
}

#[test]
fn corpus_pairs_match_expected() {
    let cases = load_fixtures();
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    let mut panicked = Vec::new();
    let mut skipped = Vec::new();

    for case in &cases {
        if case.wontfix {
            skipped.push((case.name.clone(), case.wontfix_reason.clone().unwrap_or_default()));
            continue;
        }
        // Run inside catch_unwind so a panic in one case doesn't abort the
        // rest of the suite — we want a per-case pass/fail/panic report.
        let name = case.name.clone();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(case))) {
            Ok(Ok(())) => passed.push(name),
            Ok(Err(msg)) => failed.push((name, msg)),
            Err(_) => panicked.push(name),
        }
    }

    let total = cases.len();
    let pass_n = passed.len();
    let fail_n = failed.len();
    let panic_n = panicked.len();
    let skip_n = skipped.len();

    println!(
        "\n=== corpus summary: {total} cases, {pass_n} pass, {fail_n} fail, {panic_n} panic, {skip_n} wontfix ==="
    );
    for name in &passed {
        println!("  pass:   {name}");
    }
    for (name, reason) in &skipped {
        let reason = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
        println!("  wontfix:{name}{reason}");
    }
    for (name, msg) in &failed {
        println!("  FAIL:   {name}\n    {msg}");
    }
    for name in &panicked {
        println!("  PANIC:  {name}");
    }

    assert!(
        failed.is_empty() && panicked.is_empty(),
        "{fail_n} cases failed, {panic_n} cases panicked (see stdout for details)"
    );
}
