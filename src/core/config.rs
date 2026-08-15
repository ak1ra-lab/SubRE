//! User configuration persistence (TOML under the XDG config dir).
//!
//! Fields mirror the parts of [`crate::core::plan::NamingConfig`] plus
//! user-custom extension overrides. Loaded on startup, saved on demand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::plan::{ActionMode, NamingConfig};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub suffix: NamingConfig,
    #[serde(default)]
    pub custom_video_exts: Vec<String>,
    #[serde(default)]
    pub custom_subtitle_exts: Vec<String>,
    #[serde(default)]
    pub video_regex: Option<String>,
    #[serde(default)]
    pub subtitle_regex: Option<String>,
    #[serde(default)]
    pub action_mode: ActionMode,
}

/// Returns the default config path: `dirs::config_dir()/subtitle-renamer/config.toml`.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("subtitle-renamer")
        .join("config.toml")
}

#[derive(Debug)]
pub struct ConfigStore;

impl ConfigStore {
    pub fn load(path: &Path) -> Result<UserConfig> {
        if !path.exists() {
            return Ok(UserConfig::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: UserConfig =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn load_default() -> Result<UserConfig> {
        Self::load(&default_config_path())
    }

    pub fn save(cfg: &UserConfig, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config parent {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(cfg).context("serialize config")?;
        std::fs::write(path, text).with_context(|| format!("write config {}", path.display()))?;
        Ok(())
    }

    pub fn save_default(cfg: &UserConfig) -> Result<()> {
        Self::save(cfg, &default_config_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::plan::{MappingScope, TokenMapping};

    #[test]
    fn round_trip_toml() {
        let dir = std::env::temp_dir().join(format!("sr_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut cfg = UserConfig::default();
        cfg.suffix.template = "${video}.${lang}.${ext}".into();
        cfg.suffix.mappings.push(TokenMapping {
            token: "chs".into(),
            value: "zh-Hans".into(),
            var: "lang".into(),
            scope: MappingScope::Global,
        });
        cfg.custom_subtitle_exts = vec!["sup".into()];
        ConfigStore::save(&cfg, &path).unwrap();
        let loaded = ConfigStore::load(&path).unwrap();
        assert_eq!(loaded.suffix.template, "${video}.${lang}.${ext}");
        assert_eq!(loaded.suffix.mappings.len(), 1);
        assert_eq!(loaded.suffix.mappings[0].token, "chs");
        assert_eq!(loaded.custom_subtitle_exts, vec!["sup".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_returns_default() {
        let path = Path::new("/nonexistent/path/config.toml");
        let cfg = ConfigStore::load(path).unwrap();
        assert_eq!(cfg.suffix.template, "");
        assert!(cfg.suffix.mappings.is_empty());
    }

    #[test]
    fn action_mode_round_trip() {
        use crate::core::plan::ActionMode;
        let dir = std::env::temp_dir().join(format!("sr_cfg_am_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        for &mode in &[ActionMode::Auto, ActionMode::Copy] {
            let cfg = UserConfig { action_mode: mode, ..UserConfig::default() };
            ConfigStore::save(&cfg, &path).unwrap();
            let loaded = ConfigStore::load(&path).unwrap();
            assert_eq!(loaded.action_mode, mode);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_mode_move_degrades_to_auto() {
        use crate::core::plan::ActionMode;
        let dir = std::env::temp_dir().join(format!("sr_cfg_move_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "action_mode = \"Move\"\n").unwrap();
        let loaded = ConfigStore::load(&path).unwrap();
        assert_eq!(loaded.action_mode, ActionMode::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
