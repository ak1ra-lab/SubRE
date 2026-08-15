//! Integration test for the token -> variable mapping + naming template,
//! driven by the real Death Note example under `examples/` (one video with
//! several `_trackN` subtitles must produce distinct target filenames).

use std::path::PathBuf;

use subtitle_renamer::core::matcher::FileEntry;
use subtitle_renamer::core::plan::{MappingScope, NamingConfig, TokenMapping, render_template};

fn entry(stem: &str) -> FileEntry {
    FileEntry::from_path(PathBuf::from(format!("/subs/{stem}")))
}

fn mapping(token: &str, value: &str, var: &str) -> TokenMapping {
    TokenMapping {
        token: token.into(),
        value: value.into(),
        var: var.into(),
        scope: MappingScope::Global,
    }
}

fn death_note_config() -> NamingConfig {
    NamingConfig {
        template: "${video}.${group}-${lang}.${ext}".into(),
        mappings: vec![
            mapping("X2&CASO", "华盟字幕社", "group"),
            mapping("track3", "zh-Hans", "lang"),
            mapping("track4", "zh-Hant", "lang"),
            mapping("track5", "jp", "lang"),
            mapping("track6", "zh-Hans&jp", "lang"),
        ],
        ..NamingConfig::default()
    }
}

#[test]
fn death_note_tracks_get_distinct_targets() {
    let cfg = death_note_config();
    let video = "Death_Note - 01 (BD 1280x720 AVC AAC)";
    let tracks = ["track3", "track4", "track5", "track6"];
    let expected_langs = ["zh-Hans", "zh-Hant", "jp", "zh-Hans&jp"];

    let mut basenames = Vec::new();
    for (track, expected_lang) in tracks.iter().zip(expected_langs.iter()) {
        let stem =
            format!("[X2&CASO][Death_Note][JP_GB_BIG5][01][DVDRIP][x264_Vorbis][F464D61D]_{track}");
        let sub = entry(&stem);
        let vars = cfg.resolve_vars(&sub);
        assert_eq!(vars.get("group").map(String::as_str), Some("华盟字幕社"), "{track}");
        assert_eq!(vars.get("lang").map(String::as_str), Some(*expected_lang), "{track}");
        basenames.push(render_template(cfg.effective_template(), &vars, video, "ass"));
    }

    // Every track gets a distinct target name (no DuplicateTarget).
    let mut unique = basenames.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), basenames.len(), "targets must be distinct: {basenames:?}");
}
