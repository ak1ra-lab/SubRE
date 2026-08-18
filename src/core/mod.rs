//! Core logic for subtitle-renamer.
//!
//! This module is intentionally GUI-free: it owns parsing, matching, planning,
//! execution and the sqlite-backed history. The GUI layer (`crate::ui`) consumes
//! it; tests live in `tests/` (integration) and `#[cfg(test)]` blocks (unit).

pub mod config;
pub mod execute;
pub mod matcher;
pub mod parse;
pub mod plan;
pub mod state;

pub use config::{ConfigStore, UserConfig};
pub use execute::{ExecuteReport, OpOutcome, execute_plan};
pub use matcher::{MatchResult, Matcher, PairGroup};
pub use parse::{EpisodeKey, ExtensionRegistry, FileCategory, RawKey, extract_keys, normalize_key};
pub use plan::{
    ActionMode, Conflict, DEFAULT_TEMPLATE, NamingConfig, Plan, PlanError, PlannedOp, TokenMapping,
    generate_plan, render_template,
};
pub use state::{CopyRecord, RenameRecord, SessionRecord, StateDb};
