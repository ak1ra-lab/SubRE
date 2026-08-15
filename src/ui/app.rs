//! eframe App implementation for the subtitle-renamer GUI.
//!
//! Layout (eframe 4-zone):
//!   - `SidePanel`: two `CollapsingHeader`s (`📁 Video source` /
//!     `📁 Subtitle source`) with filtered Browse / Clear buttons, plus
//!     a `📁 Browse (Auto)…` entry (files or folder, auto-classified)
//!     and a `🗑 Clear all` button at the bottom.
//!   - `TopBottomPanel::top` (toolbar): `Action:` `ComboBox` on the left,
//!     `▶ Apply` (green, rightmost), `📜 History`, `📋 Copy mv` packed
//!     right-to-left.
//!   - `TopBottomPanel::bottom` (status): `status_message`.
//!   - `CentralPanel`: history hint banner (if any), inline `⚙ Settings`
//!     collapsing header, three-column `TableBuilder`,
//!     `Unmatched / unknown` collapsing.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use egui_extras::TableBuilder;

use crate::core::config::{ConfigStore, UserConfig};
use crate::core::execute::{
    ChecksumCache, ExecuteReport, OpOutcome, RollbackItem, RollbackOutcome, execute_plan,
    rollback_unit, sha256_hex,
};
use crate::core::history::{HistoryDb, RenameRecord};
use crate::core::matcher::{FileEntry, MatchResult, Matcher, collect_media_files};
use crate::core::parse::{ExtensionRegistry, FileCategory};
use crate::core::plan::{
    ActionMode, Conflict, MappingScope, NamingConfig, Plan, PlannedAction, PlannedOp, StdFsProbe,
    TokenMapping, generate_plan,
};

/// Which side a path should be ingested onto when the user explicitly
/// picks `Browse…`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedSide {
    Video,
    Subtitle,
}

/// Snapshot of state sent to the background worker for a refresh.
struct RefreshRequest {
    videos: Vec<FileEntry>,
    subtitles: Vec<FileEntry>,
    video_regex: Option<String>,
    subtitle_regex: Option<String>,
    naming: NamingConfig,
    action_mode: ActionMode,
    epoch: u64,
}

/// Result produced by the background worker for a single refresh.
struct RefreshResult {
    epoch: u64,
    result: MatchResult,
    plan: Plan,
    /// Per-subtitle `(dir, checksum)` identities for history attribution.
    identities: Vec<(PathBuf, String)>,
}

/// Top-level GUI state.
#[derive(Debug)]
pub struct App {
    pub registry: ExtensionRegistry,
    pub config: UserConfig,
    pub history: HistoryDb,

    pub video_entries: Vec<FileEntry>,
    pub subtitle_entries: Vec<FileEntry>,
    pub unknown_entries: Vec<FileEntry>,

    pub match_result: Option<MatchResult>,
    pub plan: Option<Plan>,

    pub status_message: String,

    // Editable UI fields.
    pub template_input: String,
    pub auto_fill_lang_toggle: bool,
    pub case_sensitive_toggle: bool,
    pub mapping_editor: Vec<TokenMapping>,
    pub video_regex_input: String,
    pub subtitle_regex_input: String,

    // Modal / panel toggles.
    pub show_confirm: bool,
    pub show_history: bool,

    /// Initial left-panel width, computed once on the first layout pass
    /// from the available width. `None` until then; egui persists any
    /// user-dragged width for the rest of the session.
    sidebar_width: Option<f32>,

    // User-selected action policy: Auto (D5 default) or Copy (always
    // preserve originals). Persisted in the config so it survives restart.
    pub action_mode: ActionMode,

    /// Keep the window always on top. Persisted in the config.
    pub always_on_top: bool,

    // History hint banner (checksum-based provenance).
    pub history_hint: Vec<RenameRecord>,

    /// Per-session undo selection: session id -> per-unit selected flags.
    pub undo_selection: HashMap<i64, Vec<bool>>,

    /// Ambiguous `(dir, checksum)` identities detected on the subtitle
    /// side (content collision), for which history is not attributed.
    pub checksum_collision: Vec<(PathBuf, String)>,

    // Async refresh plumbing.
    refresh_tx: mpsc::Sender<RefreshRequest>,
    refresh_rx: mpsc::Receiver<RefreshResult>,
    /// Monotonic counter of the most recently requested refresh; results
    /// carrying a stale epoch are discarded (latest-wins).
    refresh_epoch: u64,
    /// True while a result for the current epoch is still in flight.
    refresh_pending: bool,
    /// One-shot summary of the last Apply/undo run, appended to the next
    /// refresh's status message so it survives the async round-trip.
    result_note: Option<String>,
}

impl App {
    pub fn new(config: UserConfig, history: HistoryDb) -> Self {
        let mut config = config;
        // Merge session-scoped mappings persisted in the db into the config.
        if let Ok(session) = history.session_mappings() {
            config.suffix.mappings.extend(session);
        }
        let mut registry = ExtensionRegistry::new();
        for ext in &config.custom_video_exts {
            registry.add_custom_video(ext);
        }
        for ext in &config.custom_subtitle_exts {
            registry.add_custom_subtitle(ext);
        }
        let (refresh_tx, refresh_rx) = spawn_refresh_worker();
        Self {
            template_input: config.suffix.template.clone(),
            auto_fill_lang_toggle: config.suffix.auto_fill_lang,
            case_sensitive_toggle: config.suffix.case_sensitive,
            mapping_editor: config.suffix.mappings.clone(),
            video_regex_input: config.video_regex.clone().unwrap_or_default(),
            subtitle_regex_input: config.subtitle_regex.clone().unwrap_or_default(),
            registry,
            action_mode: config.action_mode,
            always_on_top: config.always_on_top,
            config,
            history,
            video_entries: Vec::new(),
            subtitle_entries: Vec::new(),
            unknown_entries: Vec::new(),
            match_result: None,
            plan: None,
            status_message: String::from("Use Browse to load files, or start from a media folder."),
            show_confirm: false,
            show_history: false,
            sidebar_width: None,
            history_hint: Vec::new(),
            undo_selection: HashMap::new(),
            checksum_collision: Vec::new(),
            refresh_tx,
            refresh_rx,
            refresh_epoch: 0,
            refresh_pending: false,
            result_note: None,
        }
    }

    /// Ingest a list of paths: classify by extension and recurse into
    /// folders. Used by the `Browse (Auto)…` file picker.
    pub fn ingest_paths<I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        for path in paths {
            self.ingest_path(&path, None);
        }
        self.refresh_match_and_plan();
    }

    /// Ingest paths and force them onto a specific side, bypassing the
    /// extension-based categorizer. Used by the per-side `Browse…` buttons.
    pub fn ingest_paths_as<I>(&mut self, paths: I, side: ForcedSide)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        for path in paths {
            self.ingest_path(&path, Some(side));
        }
        self.refresh_match_and_plan();
    }

    fn ingest_path(&mut self, path: &Path, force: Option<ForcedSide>) {
        if path.is_dir() {
            if let Ok(rd) = std::fs::read_dir(path) {
                for entry in rd.flatten() {
                    self.ingest_path(&entry.path(), force);
                }
            }
            return;
        }
        let entry = FileEntry::from_path(path);
        let category = force.map_or_else(
            || self.registry.categorize(path),
            |s| match s {
                ForcedSide::Video => FileCategory::Video,
                ForcedSide::Subtitle => FileCategory::Subtitle,
            },
        );
        match category {
            FileCategory::Video => self.video_entries.push(entry),
            FileCategory::Subtitle => self.subtitle_entries.push(entry),
            FileCategory::Unknown => self.unknown_entries.push(entry),
        }
    }

    /// Ingest a directory non-recursively, classifying each file by
    /// extension via the registry. Used by the `Browse folder…` button and
    /// the CLI startup scan. Does nothing when the directory holds no
    /// recognized media.
    pub fn ingest_dir_auto(&mut self, dir: &Path) {
        let (videos, subtitles) = collect_media_files(&self.registry, dir);
        if videos.is_empty() && subtitles.is_empty() {
            return;
        }
        self.video_entries.extend(videos);
        self.subtitle_entries.extend(subtitles);
        self.refresh_match_and_plan();
    }

    /// Re-run matcher + plan from current state. The expensive work
    /// (matching, checksum hashing, plan generation) runs on a background
    /// thread; results are applied by [`App::drain_refresh`] when they
    /// arrive. Latest-wins: a newer request supersedes an older one.
    pub fn refresh_match_and_plan(&mut self) {
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        let request = RefreshRequest {
            videos: self.video_entries.clone(),
            subtitles: self.subtitle_entries.clone(),
            video_regex: non_empty(&self.video_regex_input),
            subtitle_regex: non_empty(&self.subtitle_regex_input),
            naming: self.current_naming_config(),
            action_mode: self.action_mode,
            epoch: self.refresh_epoch,
        };
        if self.refresh_tx.send(request).is_ok() {
            self.refresh_pending = true;
            self.status_message = "处理中…".into();
        } else {
            self.status_message = "后台刷新线程不可用".into();
        }
    }

    /// Drain any completed background refresh results for this frame,
    /// applying only the one matching the current epoch.
    fn drain_refresh(&mut self) {
        let mut applied = false;
        while let Ok(res) = self.refresh_rx.try_recv() {
            if res.epoch == self.refresh_epoch {
                self.apply_refresh_result(res);
                applied = true;
            }
            // Stale results (older epoch) are discarded.
        }
        if applied {
            self.refresh_pending = false;
        }
    }

    /// Commit a completed refresh result to the UI state.
    fn apply_refresh_result(&mut self, res: RefreshResult) {
        self.match_result = Some(res.result);
        self.plan = Some(res.plan);
        self.apply_history_hint(res.identities);
        let n_groups = self.match_result.as_ref().map_or(0, |m| m.paired().count());
        let n_subs = self.subtitle_entries.len();
        let n_vids = self.video_entries.len();
        let counts = format!("{n_vids} video(s), {n_subs} subtitle(s) → {n_groups} pair group(s)");
        self.status_message = match self.result_note.take() {
            Some(note) => format!("{note} · {counts}"),
            None => counts,
        };
    }

    /// Rebuild the drag-in history hint and collision warnings from the
    /// per-subtitle `(dir, checksum)` identities computed by the worker:
    /// look up each identity's naming history, flagging same-dir duplicates
    /// (content collisions) and excluding them from attribution.
    fn apply_history_hint(&mut self, identities: Vec<(PathBuf, String)>) {
        self.history_hint.clear();
        self.checksum_collision.clear();

        let mut counts: HashMap<(PathBuf, String), usize> = HashMap::new();
        for (dir, cs) in &identities {
            *counts.entry((dir.clone(), cs.clone())).or_default() += 1;
        }
        let ambiguous: std::collections::HashSet<(PathBuf, String)> =
            counts.into_iter().filter(|(_, n)| *n > 1).map(|(k, _)| k).collect();
        self.checksum_collision = ambiguous.iter().cloned().collect();

        for (dir, cs) in identities {
            if ambiguous.contains(&(dir.clone(), cs.clone())) {
                continue;
            }
            if let Ok(hits) = self.history.timeline_for(&dir, &cs) {
                self.history_hint.extend(hits);
            }
        }
    }

    fn current_naming_config(&self) -> NamingConfig {
        NamingConfig {
            template: self.template_input.clone(),
            auto_fill_lang: self.auto_fill_lang_toggle,
            case_sensitive: self.case_sensitive_toggle,
            mappings: self
                .mapping_editor
                .iter()
                .filter(|m| !m.token.is_empty() && !m.value.is_empty() && !m.var.is_empty())
                .cloned()
                .collect(),
        }
    }

    fn current_config(&self) -> UserConfig {
        let mut c = self.config.clone();
        c.suffix = self.current_naming_config();
        c.custom_video_exts = self.registry.custom_video().iter().cloned().collect();
        c.custom_subtitle_exts = self.registry.custom_subtitle().iter().cloned().collect();
        c.video_regex = non_empty(&self.video_regex_input);
        c.subtitle_regex = non_empty(&self.subtitle_regex_input);
        c.action_mode = self.action_mode;
        c.always_on_top = self.always_on_top;
        c
    }

    /// Apply the current plan. Synchronous, but errors per op are
    /// collected into a report rather than aborting the whole run.
    pub fn apply(&mut self) {
        if self.refresh_pending {
            self.status_message = "处理中,请稍候再执行。".into();
            return;
        }
        let plan = match self.plan.clone() {
            Some(p) if !p.has_conflicts() && !p.ops.is_empty() => p,
            _ => {
                self.status_message = "Plan empty or has conflicts.".into();
                return;
            }
        };
        // Find the working directory for the session record (parent of
        // first video, or cwd if no videos).
        let working_dir = self
            .video_entries
            .first()
            .and_then(|v| v.path.parent().map(std::path::Path::to_path_buf));

        let session_id = match self.history.create_session(working_dir.as_deref()) {
            Ok(id) => id,
            Err(e) => {
                self.status_message = format!("history session failed: {e}");
                return;
            }
        };

        let mut report = ExecuteReport::default();
        for op in &plan.ops {
            // Only renames produce a history row; copy leaves the source
            // untouched and is not recorded. Hash before mutating the
            // filesystem so the recorded identity matches the pre-rename
            // content.
            let checksum = if matches!(op.action, PlannedAction::Rename) {
                match sha256_hex(&op.subtitle.path) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        report.outcomes.push(OpOutcome {
                            op_index: report.outcomes.len(),
                            success: false,
                            error: Some(e.to_string()),
                            src_path: op.subtitle.path.clone(),
                            dst_path: op.target_path.clone(),
                            action: op.action,
                        });
                        continue;
                    }
                }
            } else {
                None
            };

            match execute_plan(&Plan { ops: vec![op.clone()] }) {
                Ok(r) => {
                    if r.all_ok()
                        && let Some(checksum) = &checksum
                    {
                        self.record_rename_for_op(session_id, op, checksum);
                    }
                    report.outcomes.extend(r.outcomes);
                }
                Err(e) => {
                    report.outcomes.push(OpOutcome {
                        op_index: report.outcomes.len(),
                        success: false,
                        error: Some(e.to_string()),
                        src_path: op.subtitle.path.clone(),
                        dst_path: op.target_path.clone(),
                        action: op.action,
                    });
                }
            }
        }
        let ok = report.outcomes.iter().filter(|o| o.success).count();
        let bad = report.outcomes.iter().filter(|o| !o.success).count();
        self.result_note = Some(format!("Executed: {ok} ok, {bad} failed"));
        self.refresh_match_and_plan();
    }

    /// Record a successful rename op as a history row for the given session.
    fn record_rename_for_op(&mut self, session_id: i64, op: &PlannedOp, checksum: &str) {
        let Some(dir) = op.subtitle.path.parent() else {
            return;
        };
        let (Some(old_name), Some(new_name)) = (
            op.subtitle.path.file_name().and_then(|n| n.to_str()),
            op.target_path.file_name().and_then(|n| n.to_str()),
        ) else {
            return;
        };
        if let Err(e) = self.history.record_rename(
            session_id,
            dir,
            checksum,
            op.unit_id.as_deref(),
            old_name,
            new_name,
        ) {
            self.status_message = format!("history write failed: {e}");
        }
    }

    /// Undo an entire session by id.
    pub fn undo_session(&mut self, session_id: i64) {
        if self.refresh_pending {
            self.status_message = "处理中,请稍候再还原。".into();
            return;
        }
        let renames = match self.history.renames_for_session(session_id) {
            Ok(r) => r,
            Err(e) => {
                self.status_message = format!("history read failed: {e}");
                return;
            }
        };
        if renames.is_empty() {
            self.status_message = "Nothing to undo in this session.".into();
            return;
        }
        let units = group_units(&renames);
        let selection =
            self.undo_selection.entry(session_id).or_insert_with(|| vec![true; units.len()]);
        if selection.len() != units.len() {
            selection.resize(units.len(), true);
        }

        let undo_session = match self.history.create_undo_session(session_id, None) {
            Ok(id) => id,
            Err(e) => {
                self.status_message = format!("history undo session failed: {e}");
                return;
            }
        };

        let mut ok = 0usize;
        let mut bad = 0usize;
        for (i, unit) in units.iter().enumerate() {
            if !selection[i] {
                continue;
            }
            let items: Vec<RollbackItem> = unit
                .iter()
                .map(|r| RollbackItem {
                    dir: PathBuf::from(&r.dir),
                    checksum: r.checksum.clone(),
                    old_name: r.old_name.clone(),
                    new_name: r.new_name.clone(),
                })
                .collect();
            match rollback_unit(&items) {
                RollbackOutcome::Ok => {
                    for r in unit {
                        if let Err(e) = self.history.record_rename(
                            undo_session,
                            Path::new(&r.dir),
                            &r.checksum,
                            r.unit_id.as_deref(),
                            &r.new_name,
                            &r.old_name,
                        ) {
                            self.status_message = format!("history write failed: {e}");
                        }
                    }
                    ok += 1;
                }
                _ => bad += 1,
            }
        }
        self.result_note = Some(format!("Undo: {ok} unit(s) ok, {bad} failed"));
        self.refresh_match_and_plan();
    }

    // -----------------------------------------------------------------
    // Side panel
    // -----------------------------------------------------------------

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        self.render_video_source(ui);
        self.render_subtitle_source(ui);

        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("📁 Browse (Auto)…").clicked()
                && let Some(paths) =
                    rfd::FileDialog::new().set_title("Select video and subtitle files").pick_files()
            {
                self.ingest_paths(paths);
            }
            if ui.button("📁 Browse folder…").clicked()
                && let Some(dir) =
                    rfd::FileDialog::new().set_title("Select a media folder").pick_folder()
            {
                self.ingest_dir_auto(&dir);
            }
        });

        if ui.button("🗑 Clear all").clicked() {
            self.video_entries.clear();
            self.subtitle_entries.clear();
            self.unknown_entries.clear();
            self.history_hint.clear();
            self.refresh_match_and_plan();
        }
    }

    fn render_video_source(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("📁 Video source").default_open(true).show(ui, |ui| {
            self.render_video_source_body(ui);
        });
    }

    fn render_subtitle_source(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("📁 Subtitle source").default_open(true).show(ui, |ui| {
            self.render_subtitle_source_body(ui);
        });
    }

    fn render_video_source_body(&mut self, ui: &mut egui::Ui) {
        let n = self.video_entries.len();
        if n == 0 {
            ui.label("No files yet — click Browse.");
        } else {
            let parents = unique_parents(&self.video_entries);
            if parents.len() == 1 {
                ui.label(format!("Dir: {}", parents[0].display()));
            } else {
                let tooltip: String =
                    parents.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
                ui.label(format!("{n} files from {} folders", parents.len()))
                    .on_hover_text(tooltip);
            }
        }
        ui.horizontal(|ui| {
            if ui.button("📁 Browse…").clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .set_title("Select video files")
                    .add_filter("Videos", &self.registry.video_exts())
                    .pick_files()
            {
                self.ingest_paths_as(paths, ForcedSide::Video);
            }
            if ui.button("🗑 Clear").clicked() {
                self.video_entries.clear();
                self.refresh_match_and_plan();
            }
        });
    }

    fn render_subtitle_source_body(&mut self, ui: &mut egui::Ui) {
        let n = self.subtitle_entries.len();
        if n == 0 {
            ui.label("No files yet — click Browse.");
        } else {
            let parents = unique_parents(&self.subtitle_entries);
            if parents.len() == 1 {
                ui.label(format!("Dir: {}", parents[0].display()));
            } else {
                let tooltip: String =
                    parents.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
                ui.label(format!("{n} files from {} folders", parents.len()))
                    .on_hover_text(tooltip);
            }
        }
        ui.horizontal(|ui| {
            if ui.button("📁 Browse…").clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .set_title("Select subtitle files")
                    .add_filter("Subtitles", &self.registry.subtitle_exts())
                    .pick_files()
            {
                self.ingest_paths_as(paths, ForcedSide::Subtitle);
            }
            if ui.button("🗑 Clear").clicked() {
                self.subtitle_entries.clear();
                self.refresh_match_and_plan();
            }
        });
    }

    // -----------------------------------------------------------------
    // Top toolbar
    // -----------------------------------------------------------------

    fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Action mode on the left of the toolbar (reading order).
            ui.label("Action:");
            egui::ComboBox::from_id_salt("action_mode")
                .selected_text(match self.action_mode {
                    ActionMode::Auto => "Auto",
                    ActionMode::Copy => "Always Copy",
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(matches!(self.action_mode, ActionMode::Auto), "Auto (D5)")
                        .clicked()
                    {
                        self.action_mode = ActionMode::Auto;
                        self.refresh_match_and_plan();
                    }
                    if ui
                        .selectable_label(
                            matches!(self.action_mode, ActionMode::Copy),
                            "Always Copy (safe)",
                        )
                        .on_hover_text(
                            "Subtitles are copied into the video folder; originals stay put.",
                        )
                        .clicked()
                    {
                        self.action_mode = ActionMode::Copy;
                        self.refresh_match_and_plan();
                    }
                });

            // Right-to-left cluster: Apply is rightmost (highest weight).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_apply = !self.refresh_pending
                    && self.plan.as_ref().is_some_and(|p| !p.ops.is_empty() && !p.has_conflicts());

                if ui
                    .add_enabled(
                        can_apply,
                        egui::Button::new("▶ Apply").fill(egui::Color32::from_rgb(60, 130, 60)),
                    )
                    .on_hover_text("Apply the rename/copy plan to disk")
                    .clicked()
                {
                    self.show_confirm = true;
                }

                ui.toggle_value(&mut self.show_history, "📜 History");

                if ui
                    .add_enabled(can_apply, egui::Button::new("📋 Copy mv"))
                    .on_hover_text("Copy the generated mv/cp script to the clipboard")
                    .clicked()
                    && let Some(plan) = &self.plan
                {
                    let script = mv_script(plan);
                    ui.ctx().copy_text(script);
                    self.status_message = "Copied mv script to clipboard.".into();
                }
            });
        });
    }

    // -----------------------------------------------------------------
    // Bottom status bar
    // -----------------------------------------------------------------

    fn render_status_bar(&self, ui: &mut egui::Ui) {
        ui.label(&self.status_message);
    }

    // -----------------------------------------------------------------
    // Central panel
    // -----------------------------------------------------------------

    fn render_central(&mut self, ui: &mut egui::Ui) {
        for (dir, checksum) in &self.checksum_collision {
            ui.colored_label(
                egui::Color32::RED,
                format!(
                    "Content collision in {}: multiple subtitles share checksum {} — history not attributed.",
                    dir.display(),
                    &checksum[..8]
                ),
            );
        }

        if !self.history_hint.is_empty() {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "{} dropped subtitle(s) match history records — see History panel to undo.",
                    self.history_hint.len()
                ),
            );
        }

        self.render_settings(ui);

        ui.separator();

        // Three-column table, flattened to one row per subtitle (plus one row
        // per unmatched video). Rows are precomputed so the body does not
        // borrow `self.match_result` / `self.plan` during render.
        let rows: Vec<(Option<FileEntry>, Option<FileEntry>, String, bool)> = {
            let mut rows = Vec::new();
            if let (Some(result), Some(plan)) = (&self.match_result, &self.plan) {
                for op in &plan.ops {
                    rows.push((
                        op.video.clone(),
                        Some(op.subtitle.clone()),
                        op.target_basename.clone(),
                        !op.conflicts.is_empty(),
                    ));
                }
                for v in result.unmatched_videos() {
                    rows.push((Some(v.clone()), None, String::new(), false));
                }
            }
            rows
        };

        TableBuilder::new(ui)
            .columns(egui_extras::Column::remainder().at_least(160.0), 3)
            .striped(true)
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Video");
                });
                header.col(|ui| {
                    ui.strong("Subtitle(s)");
                });
                header.col(|ui| {
                    ui.strong("Target filename");
                });
            })
            .body(|mut body| {
                if rows.is_empty() {
                    body.row(26.0, |mut row| {
                        row.col(|ui| {
                            ui.label("(no files yet)");
                        });
                        row.col(|_ui| {});
                        row.col(|_ui| {});
                    });
                } else {
                    let mut prev_video: Option<PathBuf> = None;
                    for (video, subtitle, target, conflict) in &rows {
                        body.row(26.0, |mut row| {
                            row.col(|ui| {
                                if let Some(v) = video {
                                    let repeat = prev_video.as_ref() == Some(&v.path);
                                    prev_video = Some(v.path.clone());
                                    if repeat {
                                        ui.weak(path_label(v));
                                    } else if subtitle.is_none() {
                                        ui.colored_label(egui::Color32::YELLOW, path_label(v));
                                    } else {
                                        ui.label(path_label(v));
                                    }
                                } else {
                                    prev_video = None;
                                    ui.label("—");
                                }
                            });
                            row.col(|ui| {
                                if let Some(sub) = subtitle {
                                    ui.horizontal(|ui| {
                                        ui.label(path_label(sub));
                                        // Per-row "remove" button: drop this
                                        // subtitle from the loaded set.
                                        if ui.small_button("x").clicked() {
                                            self.subtitle_entries.retain(|e| e.path != sub.path);
                                            self.refresh_match_and_plan();
                                        }
                                    });
                                } else {
                                    ui.label("—");
                                }
                            });
                            row.col(|ui| {
                                if subtitle.is_some() {
                                    if *conflict {
                                        ui.colored_label(egui::Color32::RED, target);
                                    } else {
                                        ui.label(target);
                                    }
                                } else {
                                    ui.label("—");
                                }
                            });
                        });
                    }
                }
            });

        ui.collapsing("Unmatched / unknown", |ui| {
            if let Some(result) = &self.match_result {
                let unmatched_v: Vec<&FileEntry> = result.unmatched_videos().collect();
                let unmatched_s: Vec<&FileEntry> = result.unmatched_subtitles().collect();
                ui.label(format!(
                    "Unmatched videos: {} · Unmatched subtitles: {} · Unknown extensions: {}",
                    unmatched_v.len(),
                    unmatched_s.len(),
                    self.unknown_entries.len()
                ));
                for v in unmatched_v {
                    ui.colored_label(egui::Color32::YELLOW, format!("  V  {}", path_label(v)));
                }
                for s in unmatched_s {
                    ui.colored_label(egui::Color32::YELLOW, format!("  S  {}", path_label(s)));
                }
                for u in &self.unknown_entries {
                    ui.label(format!("  ?  {}", path_label(u)));
                }
            }
        });
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        // The header is a plain collapsible header, like the
        // "Unmatched / unknown" section below.
        egui::CollapsingHeader::new("⚙ Settings ▾").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Template:");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.template_input)
                            .hint_text("${video}.${ext}"),
                    )
                    .changed()
                {
                    self.refresh_match_and_plan();
                }
                if ui.checkbox(&mut self.auto_fill_lang_toggle, "Auto-fill lang").changed() {
                    self.refresh_match_and_plan();
                }
                if ui.checkbox(&mut self.case_sensitive_toggle, "Case-sensitive").changed() {
                    self.refresh_match_and_plan();
                }
                if ui.checkbox(&mut self.always_on_top, "Always on top").changed() {
                    let level = if self.always_on_top {
                        egui::WindowLevel::AlwaysOnTop
                    } else {
                        egui::WindowLevel::Normal
                    };
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                }
                if ui.button("Save config").clicked() {
                    self.save_config();
                }
            });
            ui.label("Token → variable mappings:");
            let mut to_remove: Option<usize> = None;
            let mut edited = false;
            for (i, m) in self.mapping_editor.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    if ui.text_edit_singleline(&mut m.token).changed() {
                        edited = true;
                    }
                    ui.label("→");
                    if ui.text_edit_singleline(&mut m.value).changed() {
                        edited = true;
                    }
                    ui.label("var:");
                    if ui.text_edit_singleline(&mut m.var).changed() {
                        edited = true;
                    }
                    egui::ComboBox::from_id_salt(("scope", i))
                        .selected_text(match m.scope {
                            MappingScope::Global => "global",
                            MappingScope::Session => "session",
                        })
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(m.scope == MappingScope::Global, "global")
                                .clicked()
                            {
                                m.scope = MappingScope::Global;
                                edited = true;
                            }
                            if ui
                                .selectable_label(m.scope == MappingScope::Session, "session")
                                .clicked()
                            {
                                m.scope = MappingScope::Session;
                                edited = true;
                            }
                        });
                    if ui.button("x").clicked() {
                        to_remove = Some(i);
                    }
                });
            }
            if let Some(i) = to_remove {
                self.mapping_editor.remove(i);
                self.refresh_match_and_plan();
            }
            if edited {
                self.refresh_match_and_plan();
            }
            if ui.button("+ add mapping").clicked() {
                self.mapping_editor.push(TokenMapping {
                    token: String::new(),
                    value: String::new(),
                    var: "lang".into(),
                    scope: MappingScope::Global,
                });
                self.refresh_match_and_plan();
            }
            ui.horizontal(|ui| {
                ui.label("Video regex (fallback):");
                if ui.text_edit_singleline(&mut self.video_regex_input).changed() {
                    self.refresh_match_and_plan();
                }
                ui.label("Subtitle regex:");
                if ui.text_edit_singleline(&mut self.subtitle_regex_input).changed() {
                    self.refresh_match_and_plan();
                }
            });
        });
    }

    fn save_config(&mut self) {
        let cfg = self.current_config();
        // Global mappings -> TOML; session mappings -> SQLite.
        let (global, session): (Vec<TokenMapping>, Vec<TokenMapping>) =
            cfg.suffix.mappings.iter().cloned().partition(|m| m.scope == MappingScope::Global);
        let mut toml_cfg = cfg.clone();
        toml_cfg.suffix.mappings = global;
        match ConfigStore::save_default(&toml_cfg) {
            Ok(()) => self.config = cfg,
            Err(e) => self.status_message = format!("config save failed: {e}"),
        }
        if let Err(e) = self.history.replace_session_mappings(&session) {
            self.status_message = format!("session mappings save failed: {e}");
        }
        self.refresh_match_and_plan();
    }

    // -----------------------------------------------------------------
    // Modal windows
    // -----------------------------------------------------------------

    fn render_modals(&mut self, ctx: &egui::Context) {
        // Confirm dialog.
        if self.show_confirm {
            egui::Window::new("Confirm apply").collapsible(false).resizable(false).show(
                ctx,
                |ui| {
                    if let Some(plan) = &self.plan {
                        let n = plan.ops.len();
                        let copies = plan
                            .ops
                            .iter()
                            .filter(|o| matches!(o.action, PlannedAction::Copy))
                            .count();
                        let renames = plan
                            .ops
                            .iter()
                            .filter(|o| matches!(o.action, PlannedAction::Rename))
                            .count();
                        ui.label(format!(
                            "Apply {n} operations ({renames} rename, {copies} copy)?"
                        ));
                        ui.horizontal(|ui| {
                            if ui.button("Confirm").clicked() {
                                self.show_confirm = false;
                                self.apply();
                            }
                            if ui.button("Cancel").clicked() {
                                self.show_confirm = false;
                            }
                        });
                    }
                },
            );
        }

        // History window.
        if self.show_history {
            egui::Window::new("History")
                .resizable(true)
                .default_size(egui::vec2(600.0, 400.0))
                .show(ctx, |ui| {
                    let sessions = match self.history.list_sessions() {
                        Ok(s) => s,
                        Err(e) => {
                            ui.label(format!("history read failed: {e}"));
                            Vec::new()
                        }
                    };
                    if sessions.is_empty() {
                        ui.label("(no history yet)");
                    }
                    let mut to_undo: Option<i64> = None;
                    for s in &sessions {
                        let title = match s.undo_of {
                            Some(orig) => format!(
                                "session #{} — {} (undo of #{})",
                                s.id,
                                format_epoch(s.created_at),
                                orig
                            ),
                            None => {
                                format!("session #{} — {}", s.id, format_epoch(s.created_at))
                            }
                        };
                        ui.collapsing(title, |ui| match self.history.renames_for_session(s.id) {
                            Ok(renames) => {
                                let units = group_units(&renames);
                                if units.is_empty() {
                                    ui.label("(no renames — copy-only session)");
                                } else {
                                    let selection = self
                                        .undo_selection
                                        .entry(s.id)
                                        .or_insert_with(|| vec![true; units.len()]);
                                    if selection.len() != units.len() {
                                        selection.resize(units.len(), true);
                                    }
                                    for (i, unit) in units.iter().enumerate() {
                                        ui.checkbox(&mut selection[i], unit_label(unit));
                                    }
                                    if ui.button("Undo selected").clicked() {
                                        to_undo = Some(s.id);
                                    }
                                }
                            }
                            Err(e) => {
                                ui.label(format!("history read failed: {e}"));
                            }
                        });
                    }
                    if let Some(sid) = to_undo {
                        self.undo_session(sid);
                    }
                    if ui.button("Close").clicked() {
                        self.show_history = false;
                    }
                });
        }
    }
}

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

/// Spawn the single background refresh worker. Returns the request sender
/// (owned by `App`) and the result receiver (also owned by `App`). The
/// worker owns a [`ChecksumCache`] so unchanged subtitles are hashed only
/// once across refreshes.
fn spawn_refresh_worker() -> (mpsc::Sender<RefreshRequest>, mpsc::Receiver<RefreshResult>) {
    let (req_tx, req_rx) = mpsc::channel::<RefreshRequest>();
    let (res_tx, res_rx) = mpsc::channel::<RefreshResult>();
    std::thread::spawn(move || {
        let mut cache = ChecksumCache::new();
        while let Ok(req) = req_rx.recv() {
            let result = Matcher::new().match_files(
                &req.videos,
                &req.subtitles,
                req.video_regex.as_deref(),
                req.subtitle_regex.as_deref(),
            );
            let plan = generate_plan(&result, &req.naming, &StdFsProbe, req.action_mode);
            let mut identities = Vec::new();
            for sub in &req.subtitles {
                if let Some(dir) = sub.path.parent()
                    && let Some(cs) = cache.cached_sha256(&sub.path)
                {
                    identities.push((dir.to_path_buf(), cs));
                }
            }
            if res_tx.send(RefreshResult { epoch: req.epoch, result, plan, identities }).is_err() {
                break;
            }
        }
    });
    (req_tx, res_rx)
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// Group a session's rename records into rollback units: records sharing
/// a `unit_id` are one unit (`.idx`+`.sub`); `None` records are their own
/// singleton unit. Order is preserved.
fn group_units(renames: &[RenameRecord]) -> Vec<Vec<RenameRecord>> {
    let mut units: Vec<Vec<RenameRecord>> = Vec::new();
    let mut index_by_unit: HashMap<String, usize> = HashMap::new();
    for r in renames {
        match &r.unit_id {
            Some(uid) => {
                if let Some(&i) = index_by_unit.get(uid) {
                    units[i].push(r.clone());
                } else {
                    index_by_unit.insert(uid.clone(), units.len());
                    units.push(vec![r.clone()]);
                }
            }
            None => units.push(vec![r.clone()]),
        }
    }
    units
}

/// Human-readable one-line summary of a rollback unit.
fn unit_label(unit: &[RenameRecord]) -> String {
    unit.iter()
        .map(|r| format!("{} → {}", r.old_name, r.new_name))
        .collect::<Vec<_>>()
        .join("  ·  ")
}

/// Format an epoch-seconds timestamp as a UTC `YYYY-MM-DD HH:MM:SS`
/// string, without pulling in a date library.
fn format_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;

    // Howard Hinnant's `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Render a file as its basename only. The directory is shown in the
/// bottom status bar (per source side), not in every row.
fn path_label(p: &FileEntry) -> String {
    p.path
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| p.path.display().to_string(), std::string::ToString::to_string)
}

/// Distinct parent paths of an entry list, preserving first-seen order.
fn unique_parents(entries: &[FileEntry]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .filter_map(|e| e.path.parent().map(Path::to_path_buf))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

#[allow(dead_code)]
fn conflict_summary(c: &Conflict) -> &'static str {
    match c {
        Conflict::DuplicateTarget(_) => "duplicate",
        Conflict::TargetExists(_) => "exists",
        Conflict::ActionUnsupported(_) => "unsupported",
    }
}

fn mv_script(plan: &Plan) -> String {
    let mut out = String::from("#!/bin/sh\n# generated by subtitle-renamer\n");
    for op in &plan.ops {
        let cmd = match op.action {
            PlannedAction::Rename => "mv",
            PlannedAction::Copy => "cp",
        };
        let src = shell_quote(&op.subtitle.path.display().to_string());
        let dst = shell_quote(&op.target_path.display().to_string());
        let _ = writeln!(out, "{cmd} {src} {dst}");
    }
    out
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/-_=.,:@%+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply any completed background refresh before rendering.
        self.drain_refresh();

        // Left: sidebar with two source CollapsingHeaders + Auto entries.
        // Initial width is proportional to the window; egui persists any
        // user-dragged width for the rest of the session.
        let sidebar_default = *self.sidebar_width.get_or_insert_with(|| {
            let w = ui.available_width();
            if w > 0.0 { (w * 0.25).max(220.0) } else { 220.0 }
        });
        egui::Panel::left("source")
            .resizable(true)
            .default_size(sidebar_default)
            .min_size(220.0)
            .show(ui, |ui| {
                self.render_sidebar(ui);
            });

        // Top toolbar.
        egui::Panel::top("toolbar").show(ui, |ui| {
            self.render_toolbar(ui);
        });

        // Bottom status bar (multi-line, full paths).
        egui::Panel::bottom("status").show(ui, |ui| {
            self.render_status_bar(ui);
        });

        // Central panel — history hint, settings, table, unmatched.
        egui::CentralPanel::default().show(ui, |ui| {
            self.render_central(ui);
        });

        // Modals live on ctx so they survive panel restructuring.
        self.render_modals(ui.ctx());

        // Keep polling while a refresh is in flight so the UI stays
        // responsive and the result is applied promptly.
        if self.refresh_pending {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}
