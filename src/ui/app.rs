//! eframe App implementation for the subtitle-renamer GUI.
//!
//! Layout (eframe 4-zone):
//!   - `SidePanel`: two `CollapsingHeader`s (`📁 Video source` /
//!     `📁 Subtitle source`) with Browse / Clear buttons; their rects
//!     are captured each frame so the drop router can force-side drops
//!     landing on them. A `🗑 Clear all` button at the bottom clears
//!     both sides + unknowns + history hint.
//!   - `TopBottomPanel::top` (toolbar): `Action:` `ComboBox` on the left,
//!     `▶ Apply` (green, rightmost), `📜 History`, `⚙ Settings`,
//!     `📋 Copy mv` packed right-to-left.
//!   - `TopBottomPanel::bottom` (status): multi-line `Video dir:` /
//!     `Subtitle dir:` (full paths in monospace, never truncated;
//!     multi-folder shows `N folders` with hover listing all paths)
//!     + `status_message`.
//!   - `CentralPanel`: history hint banner (if any), inline `⚙ Settings`
//!     collapsing header, three-column `TableBuilder`,
//!     `Unmatched / unknown` collapsing.
//!
//! Drop handling is centralized in [`App::process_drops`]: it runs
//! between sidebar render (which populates `sidebar_video_rect` /
//! `sidebar_subtitle_rect`) and toolbar render.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use eframe::egui;
use egui_extras::TableBuilder;

use crate::core::config::{ConfigStore, UserConfig};
use crate::core::execute::{ExecuteReport, OpOutcome, execute_plan, snapshot};
use crate::core::history::{HistoryDb, OperationRecord};
use crate::core::matcher::{FileEntry, MatchResult, Matcher, PairGroup};
use crate::core::parse::{ExtensionRegistry, FileCategory};
use crate::core::plan::{ActionMode, Conflict, Plan, PlannedAction, SuffixConfig, generate_plan};

/// Which side a path should be ingested onto when the user explicitly
/// picks `Browse…` or drops onto a sidebar source region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedSide {
    Video,
    Subtitle,
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
    pub global_suffix_input: String,
    pub auto_extract_toggle: bool,
    pub token_map_editor: Vec<(String, String)>,
    pub video_regex_input: String,
    pub subtitle_regex_input: String,

    // Modal / panel toggles.
    pub show_confirm: bool,
    pub show_history: bool,
    /// Whether the inline Settings `CollapsingHeader` in `CentralPanel`
    /// is forced open. Toggled by the `⚙ Settings` button in the top
    /// toolbar.
    pub show_settings: bool,

    // User-selected action policy: Auto (D5 default), Copy (always
    // preserve originals), or Move (rename/move the subtitle into the
    // video directory). Persisted in the config so it survives restart.
    pub action_mode: ActionMode,

    // History hint banner.
    pub history_hint: Vec<OperationRecord>,

    /// Rect of the Video source `CollapsingHeader` in the sidebar,
    /// captured each frame so the drop router can detect "drop on
    /// Video source" via `pointer.hover_pos()`.
    pub sidebar_video_rect: Option<egui::Rect>,
    /// Rect of the Subtitle source `CollapsingHeader` in the sidebar.
    pub sidebar_subtitle_rect: Option<egui::Rect>,
}

impl App {
    pub fn new(config: UserConfig, history: HistoryDb) -> Self {
        let mut registry = ExtensionRegistry::new();
        for ext in &config.custom_video_exts {
            registry.add_custom_video(ext);
        }
        for ext in &config.custom_subtitle_exts {
            registry.add_custom_subtitle(ext);
        }
        Self {
            global_suffix_input: config.suffix.global.clone(),
            auto_extract_toggle: config.suffix.auto_extract_language_token,
            token_map_editor: config
                .suffix
                .token_map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            video_regex_input: config.video_regex.clone().unwrap_or_default(),
            subtitle_regex_input: config.subtitle_regex.clone().unwrap_or_default(),
            registry,
            action_mode: config.action_mode,
            config,
            history,
            video_entries: Vec::new(),
            subtitle_entries: Vec::new(),
            unknown_entries: Vec::new(),
            match_result: None,
            plan: None,
            status_message: String::from("Drop files or a folder into the window to start."),
            show_confirm: false,
            show_history: false,
            show_settings: false,
            history_hint: Vec::new(),
            sidebar_video_rect: None,
            sidebar_subtitle_rect: None,
        }
    }

    /// Ingest a list of dropped paths: classify by extension and recurse
    /// into folders. Used for drops that land outside the sidebar source
    /// regions (auto-classify).
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
    /// extension-based categorizer. Used by sidebar `Browse…` buttons
    /// and by the drop router when a drop lands on a sidebar source
    /// rect.
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

    /// Re-run matcher + plan from current state.
    pub fn refresh_match_and_plan(&mut self) {
        let matcher = Matcher::new();
        let v_regex = non_empty(&self.video_regex_input);
        let s_regex = non_empty(&self.subtitle_regex_input);
        let result = matcher.match_files(
            &self.video_entries,
            &self.subtitle_entries,
            v_regex.as_deref(),
            s_regex.as_deref(),
        );

        // History hint: any dropped subtitle whose path equals a non-undone dst?
        self.history_hint.clear();
        for sub in &self.subtitle_entries {
            if let Ok(hits) = self.history.find_by_dst(&sub.path) {
                self.history_hint.extend(hits);
            }
        }

        let suffix = self.current_suffix_config();
        let plan =
            generate_plan(&result, &suffix, &crate::core::plan::StdFsProbe, self.action_mode);
        self.match_result = Some(result);
        self.plan = Some(plan);
        let n_groups = self.match_result.as_ref().map_or(0, |m| m.paired().count());
        let n_subs = self.subtitle_entries.len();
        let n_vids = self.video_entries.len();
        self.status_message =
            format!("{n_vids} video(s), {n_subs} subtitle(s) → {n_groups} pair group(s)");
    }

    fn current_suffix_config(&self) -> SuffixConfig {
        let mut token_map = HashMap::new();
        for (k, v) in &self.token_map_editor {
            if !k.is_empty() && !v.is_empty() {
                token_map.insert(k.clone(), v.clone());
            }
        }
        SuffixConfig {
            global: self.global_suffix_input.clone(),
            token_map,
            auto_extract_language_token: self.auto_extract_toggle,
        }
    }

    fn current_config(&self) -> UserConfig {
        let mut c = self.config.clone();
        c.suffix = self.current_suffix_config();
        c.custom_video_exts = self.registry.custom_video().iter().cloned().collect();
        c.custom_subtitle_exts = self.registry.custom_subtitle().iter().cloned().collect();
        c.video_regex = non_empty(&self.video_regex_input);
        c.subtitle_regex = non_empty(&self.subtitle_regex_input);
        c.action_mode = self.action_mode;
        c
    }

    /// Apply the current plan. Synchronous, but errors per op are
    /// collected into a report rather than aborting the whole run.
    pub fn apply(&mut self) {
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
            let (mtime, size, checksum) = match snapshot(&op.subtitle.path) {
                Ok(v) => v,
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
            };
            match execute_plan(&Plan { ops: vec![op.clone()] }) {
                Ok(r) => {
                    let ok = r.all_ok();
                    if ok
                        && let Err(e) = self.history.record_operation(
                            session_id,
                            op.action,
                            &op.subtitle.path,
                            &op.target_path,
                            mtime,
                            size,
                            &checksum,
                        )
                    {
                        self.status_message = format!("history write failed: {e}");
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
        self.status_message = format!("Executed: {ok} ok, {bad} failed");
        self.refresh_match_and_plan();
    }

    /// Undo an entire session by id.
    pub fn undo_session(&mut self, session_id: i64) {
        let ops = match self.history.operations_for_session(session_id) {
            Ok(o) => o,
            Err(e) => {
                self.status_message = format!("history read failed: {e}");
                return;
            }
        };
        let mut ok = 0usize;
        let mut bad = 0usize;
        for op in ops {
            let hist = op.into_historical();
            let outcome = crate::core::execute::undo_one(&hist);
            match outcome {
                crate::core::execute::UndoOutcome::Ok => {
                    if let Err(e) = self.history.mark_undone(hist.id) {
                        self.status_message = format!("history update failed: {e}");
                    }
                    ok += 1;
                }
                _ => bad += 1,
            }
        }
        self.status_message = format!("Undo: {ok} ok, {bad} failed");
        self.refresh_match_and_plan();
    }

    // -----------------------------------------------------------------
    // Drop router
    // -----------------------------------------------------------------

    /// Route any dropped files for this frame. Called after the
    /// `SidePanel` has rendered (so the source rects are populated) and
    /// before the toolbar renders. Drops whose pointer position lands
    /// on a sidebar source rect are routed to that side; everything
    /// else falls through to extension auto-classify via
    /// [`App::ingest_paths`].
    fn process_drops(&mut self, ctx: &egui::Context) {
        let video_rect = self.sidebar_video_rect;
        let subtitle_rect = self.sidebar_subtitle_rect;
        let (paths, hover) = ctx.input(|i| {
            let paths: Vec<PathBuf> =
                i.raw.dropped_files.iter().filter_map(|d| d.path.clone()).collect();
            (paths, i.pointer.hover_pos())
        });
        if paths.is_empty() {
            return;
        }
        let forced = hover.and_then(|p| {
            if video_rect.is_some_and(|r| r.contains(p)) {
                Some(ForcedSide::Video)
            } else if subtitle_rect.is_some_and(|r| r.contains(p)) {
                Some(ForcedSide::Subtitle)
            } else {
                None
            }
        });
        match forced {
            Some(side) => self.ingest_paths_as(paths, side),
            None => self.ingest_paths(paths),
        }
    }

    // -----------------------------------------------------------------
    // Side panel
    // -----------------------------------------------------------------

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        self.render_video_source(ui);
        self.render_subtitle_source(ui);

        ui.separator();

        if ui.button("🗑 Clear all").clicked() {
            self.video_entries.clear();
            self.subtitle_entries.clear();
            self.unknown_entries.clear();
            self.history_hint.clear();
            self.refresh_match_and_plan();
        }
    }

    fn render_video_source(&mut self, ui: &mut egui::Ui) {
        let pre = ui.min_rect();
        egui::CollapsingHeader::new("📁 Video source").default_open(true).show(ui, |ui| {
            self.render_video_source_body(ui);
        });
        let post = ui.min_rect();
        self.sidebar_video_rect = Some(egui::Rect::from_min_max(pre.min, post.max));
    }

    fn render_subtitle_source(&mut self, ui: &mut egui::Ui) {
        let pre = ui.min_rect();
        egui::CollapsingHeader::new("📁 Subtitle source").default_open(true).show(ui, |ui| {
            self.render_subtitle_source_body(ui);
        });
        let post = ui.min_rect();
        self.sidebar_subtitle_rect = Some(egui::Rect::from_min_max(pre.min, post.max));
    }

    fn render_video_source_body(&mut self, ui: &mut egui::Ui) {
        let n = self.video_entries.len();
        if n == 0 {
            ui.label("No files yet — drop here or click Browse.");
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
                && let Some(paths) =
                    rfd::FileDialog::new().set_title("Select video files").pick_files()
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
            ui.label("No files yet — drop here or click Browse.");
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
                && let Some(paths) =
                    rfd::FileDialog::new().set_title("Select subtitle files").pick_files()
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
                    ActionMode::Move => "Always Move",
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
                    if ui
                        .selectable_label(
                            matches!(self.action_mode, ActionMode::Move),
                            "Always Move (destructive)",
                        )
                        .on_hover_text(
                            "Subtitles are renamed across directories; the source file is removed.",
                        )
                        .clicked()
                    {
                        self.action_mode = ActionMode::Move;
                        self.refresh_match_and_plan();
                    }
                });

            // Right-to-left cluster: Apply is rightmost (highest weight).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_apply =
                    self.plan.as_ref().is_some_and(|p| !p.ops.is_empty() && !p.has_conflicts());

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
                ui.toggle_value(&mut self.show_settings, "⚙ Settings");

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
        ui.vertical(|ui| {
            ui.label("Video dir:");
            render_side_dirs(ui, &self.video_entries);
            ui.label("Subtitle dir:");
            render_side_dirs(ui, &self.subtitle_entries);
            ui.label(&self.status_message);
        });
    }

    // -----------------------------------------------------------------
    // Central panel
    // -----------------------------------------------------------------

    fn render_central(&mut self, ui: &mut egui::Ui) {
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

        // Three-column table.
        let result = self.match_result.clone();
        let plan = self.plan.clone();
        TableBuilder::new(ui)
            .columns(egui_extras::Column::remainder().at_least(160.0), 3)
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
                if let Some(result) = &result {
                    if result.groups.is_empty() {
                        body.row(24.0, |mut row| {
                            row.col(|ui| {
                                ui.label("(no files yet)");
                            });
                            row.col(|_ui| {});
                            row.col(|_ui| {});
                        });
                    }
                    for (group_index, group) in result.groups.iter().enumerate() {
                        let video_label =
                            group.video.as_ref().map_or_else(|| "—".into(), path_label);
                        let preview = preview_for_group(group, plan.as_ref());
                        let conflict = plan.as_ref().is_some_and(|p| group_has_conflict(group, p));
                        let group_video_unmatched = group.video.is_some()
                            && group.subtitles.is_empty()
                            && !preview.contains("(no plan)");
                        body.row(24.0, |mut row| {
                            row.col(|ui| {
                                if group_video_unmatched {
                                    ui.colored_label(egui::Color32::YELLOW, &video_label);
                                } else {
                                    ui.label(&video_label);
                                }
                            });
                            row.col(|ui| {
                                if group.subtitles.is_empty() {
                                    ui.label("—");
                                } else {
                                    for sub in &group.subtitles {
                                        ui.horizontal(|ui| {
                                            ui.label(path_label(sub));
                                            // Per-row "detach" button:
                                            // re-pairing is exposed via
                                            // `Matcher::manual_detach` /
                                            // `manual_attach`. The actual
                                            // reassignment UI uses a row
                                            // index; here we just offer a
                                            // detach for the first sub.
                                            if ui.small_button("x").clicked()
                                                && let Some(r) = self.match_result.as_mut()
                                            {
                                                Matcher::new().manual_detach(r, group_index, 0);
                                                self.refresh_match_and_plan();
                                            }
                                        });
                                    }
                                }
                            });
                            row.col(|ui| {
                                if conflict {
                                    ui.colored_label(egui::Color32::RED, &preview);
                                } else {
                                    ui.label(&preview);
                                }
                            });
                        });
                    }
                } else {
                    body.row(24.0, |mut row| {
                        row.col(|ui| {
                            ui.label("(no files yet)");
                        });
                        row.col(|_ui| {});
                        row.col(|_ui| {});
                    });
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
        // The header's open state is locked to `show_settings`, which
        // is toggled by the `⚙ Settings` button in the toolbar — so
        // users can't bypass the toolbar by clicking the header.
        egui::CollapsingHeader::new("⚙ Settings ▾").open(Some(self.show_settings)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Global suffix:");
                if ui.text_edit_singleline(&mut self.global_suffix_input).changed() {
                    self.refresh_match_and_plan();
                }
                ui.checkbox(&mut self.auto_extract_toggle, "Auto-detect language token");
                if ui.button("Save config").clicked() {
                    let cfg = self.current_config();
                    if let Err(e) = ConfigStore::save_default(&cfg) {
                        self.status_message = format!("config save failed: {e}");
                    } else {
                        self.config = cfg;
                    }
                }
            });
            ui.label("Token → suffix map:");
            let mut to_remove: Option<usize> = None;
            for (i, (k, v)) in self.token_map_editor.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(k);
                    ui.label("→");
                    ui.text_edit_singleline(v);
                    if ui.button("x").clicked() {
                        to_remove = Some(i);
                    }
                });
            }
            if let Some(i) = to_remove {
                self.token_map_editor.remove(i);
                self.refresh_match_and_plan();
            }
            if ui.button("+ add mapping").clicked() {
                self.token_map_editor.push((String::new(), String::new()));
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
                    match self.history.list_sessions() {
                        Ok(sessions) => {
                            if sessions.is_empty() {
                                ui.label("(no history yet)");
                            }
                            for s in &sessions {
                                ui.collapsing(
                                    format!("session #{} — {}", s.id, s.created_at),
                                    |ui| {
                                        if let Ok(ops) = self.history.operations_for_session(s.id) {
                                            ui.label(format!("{} operation(s)", ops.len()));
                                            for o in &ops {
                                                ui.label(format!(
                                                    "  [{}] {} → {} ({}{})",
                                                    if o.undone { "x" } else { " " },
                                                    o.src_path,
                                                    o.dst_path,
                                                    o.action,
                                                    if o.undone { " undone" } else { "" }
                                                ));
                                            }
                                            if ui.button("Undo session").clicked() {
                                                self.undo_session(s.id);
                                            }
                                        }
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            ui.label(format!("history read failed: {e}"));
                        }
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

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
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

/// Render a single line in the bottom status bar: full monospace path
/// for one directory, or `N folders` with a hover listing all paths.
fn render_side_dirs(ui: &mut egui::Ui, entries: &[FileEntry]) {
    if entries.is_empty() {
        ui.monospace("(none)");
        return;
    }
    let parents = unique_parents(entries);
    if parents.len() == 1 {
        ui.monospace(parents[0].display().to_string());
        return;
    }
    let tooltip = parents.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
    ui.monospace(format!("{} folders", parents.len())).on_hover_text(tooltip);
}

fn preview_for_group(group: &PairGroup, plan: Option<&Plan>) -> String {
    let Some(plan) = plan else {
        return String::new();
    };
    let mut previews = Vec::new();
    for op in &plan.ops {
        let same_video = group
            .video
            .as_ref()
            .is_some_and(|v| Some(&v.path) == op.video.as_ref().map(|o| &o.path));
        let in_subs = group.subtitles.iter().any(|s| s.path == op.subtitle.path);
        if same_video || in_subs {
            previews.push(op.target_basename.clone());
        }
    }
    if previews.is_empty() { "(no plan)".into() } else { previews.join(" | ") }
}

fn group_has_conflict(group: &PairGroup, plan: &Plan) -> bool {
    plan.ops.iter().any(|o| {
        let same_video = group
            .video
            .as_ref()
            .is_some_and(|v| Some(&v.path) == o.video.as_ref().map(|vv| &vv.path));
        let in_subs = group.subtitles.iter().any(|s| s.path == o.subtitle.path);
        (same_video || in_subs) && !o.conflicts.is_empty()
    })
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
        // Reset sidebar rects each frame; they're repopulated by
        // `render_sidebar` below.
        self.sidebar_video_rect = None;
        self.sidebar_subtitle_rect = None;

        // Left: sidebar with two source CollapsingHeaders (captures rects).
        egui::Panel::left("source").resizable(true).min_size(220.0).max_size(320.0).show(
            ui,
            |ui| {
                self.render_sidebar(ui);
            },
        );

        // Drop router — sidebar rects are now populated.
        self.process_drops(ui.ctx());

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
    }
}
