//! Integration test for token -> variable mappings + naming template,
//! driven by `tests/fixtures/naming_cases.json`. Each case declares a
//! template, optional `auto_fill_lang` / `case_sensitive` flags, a list
//! of token mappings, a single video stem, one or more subtitle stems
//! and the expected target basenames. The harness feeds each subtitle
//! stem through `NamingConfig::resolve_vars` + `render_template` and
//! asserts the resulting basenames match the expected list. Per-case
//! pass / fail / wontfix is reported so a single bad case is debuggable
//! without binary search.

use std::path::PathBuf;

use serde::Deserialize;

use subtitle_renamer::core::matcher::FileEntry;
use subtitle_renamer::core::plan::{NamingConfig, TokenMapping, render_template};

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<NamingCase>,
}

#[derive(Debug, Deserialize)]
struct NamingCase {
    name: String,
    #[serde(default)]
    wontfix: bool,
    #[serde(default)]
    wontfix_reason: Option<String>,
    #[serde(default)]
    template: String,
    #[serde(default)]
    auto_fill_lang: bool,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    mappings: Vec<MappingJson>,
    video_stem: String,
    subtitle_stems: Vec<String>,
    expected_targets: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MappingJson {
    token: String,
    value: String,
    #[serde(default = "default_var")]
    var: String,
}

fn default_var() -> String {
    "lang".to_string()
}

const FIXTURE_FILE: &str = "naming_cases.json";

fn load_fixture() -> Vec<NamingCase> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(FIXTURE_FILE);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let fx: Fixture =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    fx.cases
}

fn entry(stem: &str) -> FileEntry {
    FileEntry::from_path(PathBuf::from(format!("/subs/{stem}")))
}

fn build_config(case: &NamingCase) -> NamingConfig {
    NamingConfig {
        template: case.template.clone(),
        auto_fill_lang: case.auto_fill_lang,
        case_sensitive: case.case_sensitive,
        mappings: case
            .mappings
            .iter()
            .map(|m| TokenMapping {
                token: m.token.clone(),
                value: m.value.clone(),
                var: m.var.clone(),
            })
            .collect(),
    }
}

fn run_case(case: &NamingCase) -> Result<(), String> {
    let cfg = build_config(case);
    let mut actual: Vec<String> = Vec::new();
    for stem in &case.subtitle_stems {
        let sub = entry(stem);
        let vars = cfg.resolve_vars(&sub);
        // `${video}` and `${ext}` come from the video main-name and the
        // subtitle's actual extension; build a small vars map that
        // exposes the resolved ext alongside the named variables.
        let video_main = case.video_stem.clone();
        let ext = sub.ext.clone();
        eprintln!(
            "[naming] case={} stem={:?} ext={:?} vars={:?}",
            case.name, sub.stem, sub.ext, vars
        );
        let basename = render_template(cfg.effective_template(), &vars, &video_main, &ext);
        actual.push(basename);
    }

    let mut sorted_actual = actual.clone();
    sorted_actual.sort();
    let mut sorted_expected = case.expected_targets.clone();
    sorted_expected.sort();

    if sorted_actual != sorted_expected {
        return Err(format!(
            "target basename mismatch\n  actual:   {actual:?}\n  expected:   {expected:?}",
            expected = case.expected_targets
        ));
    }
    Ok(())
}

#[test]
fn naming_targets_match_expected() {
    let cases = load_fixture();
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    let mut panicked = Vec::new();
    let mut skipped = Vec::new();

    for case in &cases {
        if case.wontfix {
            skipped.push((case.name.clone(), case.wontfix_reason.clone().unwrap_or_default()));
            continue;
        }
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
        "\n=== naming summary: {total} cases, {pass_n} pass, {fail_n} fail, {panic_n} panic, {skip_n} wontfix ==="
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
