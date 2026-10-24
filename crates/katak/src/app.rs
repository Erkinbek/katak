//! The application shell: tabs, menus, shortcuts, dialogs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText, ViewportCommand};
use katak_core::index::IndexWarning;
use katak_core::job::Notify;
use katak_core::search::Matcher;
use serde::{Deserialize, Serialize};

use crate::Args;
use crate::export_ui::{ExportAction, ExportDialog, Rows};
use crate::grid::{EditAction, GridCommand, group};
use crate::parse_dialog::{ParseAction, ParseDialog};
use crate::perf::Perf;
use crate::search_ui::ReplaceOutcome;
use crate::tab::{Tab, WriteKind};
use crate::theme::GridColors;

const SETTINGS_KEY: &str = "katak-settings";
const MAX_RECENT: usize = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub font_size: f32,
    pub monospace: bool,
    pub recent: Vec<PathBuf>,
    /// Keep the previous version as `name.bak` when saving.
    pub backup: bool,
    /// Memory a sort may use, in MB.
    pub sort_budget_mb: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::System,
            font_size: 14.0,
            monospace: false,
            recent: Vec::new(),
            backup: false,
            sort_budget_mb: katak_core::sort::DEFAULT_BUDGET >> 20,
        }
    }
}

impl Settings {
    fn sort_budget(&self) -> u64 {
        self.sort_budget_mb.clamp(64, 1 << 20) << 20
    }
}

/// `--screenshot`: what to capture and when.
struct ShotPlan {
    path: PathBuf,
    /// Time from the first paint to the capture.
    delay: Duration,
    frames: usize,
    since: Option<Instant>,
    requested: bool,
    done: Arc<AtomicBool>,
}

/// A question that needs an answer before going on.
enum Confirm {
    /// Close tab `i` with unsaved changes.
    CloseTab(usize),
    /// Quit with unsaved changes.
    Quit,
    /// Drop all edits of the active tab.
    Discard,
    /// Reload tab `i` from disk, dropping its edits.
    Reload(usize),
    /// Replace All found more cells than are kept as edits: rewrite the
    /// file of tab `tab` instead.
    Rewrite {
        tab: usize,
        matcher: Arc<Matcher>,
        with: String,
        backup: bool,
    },
}

#[derive(Default)]
struct GotoDialog {
    text: String,
    by_offset: bool,
    focus: bool,
}

pub struct KatakApp {
    tabs: Vec<Tab>,
    active: usize,
    settings: Settings,
    applied_theme: Option<ThemeChoice>,
    notify: Notify,
    messages: Vec<String>,
    goto: Option<GotoDialog>,
    show_shortcuts: bool,
    show_about: bool,
    picker: Option<Receiver<Vec<PathBuf>>>,
    perf: Perf,
    shot: Option<ShotPlan>,
    /// `--show` action, run once the first file is painted.
    pending_show: Option<String>,
    confirm: Option<Confirm>,
    rename: Option<(usize, String)>,
    pending_copy: Option<String>,
    save_as_picker: Option<Receiver<Option<PathBuf>>>,
    quit_after_save: bool,
    allow_close: bool,
    /// How files are opened (`--buffered` forces network-drive behaviour).
    access: katak_core::source::AccessMode,
    watcher: crate::watch::Watcher,
}

impl KatakApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args, started: Instant) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        let ctx = cc.egui_ctx.clone();
        let notify: Notify = Arc::new(move || ctx.request_repaint());
        // The UI thread never waits on a slow (network) drive: missing
        // blocks load in the background and the grid shows "loading".
        katak_core::source::set_nonblocking_reads(true);
        let watcher = crate::watch::Watcher::start(notify.clone());
        crate::fonts::load_fallbacks(cc.egui_ctx.clone());
        let mut app = KatakApp {
            tabs: Vec::new(),
            active: 0,
            settings,
            applied_theme: None,
            notify,
            messages: Vec::new(),
            goto: None,
            show_shortcuts: false,
            show_about: false,
            picker: None,
            perf: Perf::new(args.perf_log.clone(), args.self_test, started),
            shot: args.screenshot.clone().map(|path| ShotPlan {
                path,
                delay: Duration::from_millis(
                    std::env::var("KATAK_SHOT_DELAY_MS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(800),
                ),
                frames: 0,
                since: None,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
            pending_show: args.show.clone(),
            confirm: None,
            rename: None,
            pending_copy: None,
            save_as_picker: None,
            quit_after_save: false,
            allow_close: false,
            watcher,
            access: if args.buffered {
                katak_core::source::AccessMode::Buffered
            } else {
                katak_core::source::AccessMode::Auto
            },
        };
        let since = app.perf.since_start();
        app.perf.note(format!(
            "katak {} — startup to app constructed: {:.1} ms",
            env!("CARGO_PKG_VERSION"),
            since.as_secs_f64() * 1000.0
        ));
        for f in &args.files {
            app.open(f.clone());
        }
        app
    }

    fn open(&mut self, path: PathBuf) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            return;
        }
        let t = Instant::now();
        let opts = katak_core::document::OpenOptions {
            access: self.access,
            ..Default::default()
        };
        match Tab::open_with(&path, opts, self.notify.clone()) {
            Ok(tab) => {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                self.perf.note(format!(
                    "open {}: Document::open {ms:.1} ms ({})",
                    path.display(),
                    tab.doc.dialect().summary()
                ));
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.settings.recent.retain(|p| p != &path);
                self.settings.recent.insert(0, path);
                self.settings.recent.truncate(MAX_RECENT);
            }
            Err(e) => self.messages.push(e),
        }
    }

    fn pick_files(&mut self) {
        let dialog = rfd::FileDialog::new()
            .set_title("Open delimited text files")
            .add_filter(
                "Delimited text",
                &["csv", "tsv", "tab", "txt", "psv", "dat", "log"],
            )
            .add_filter("All files", &["*"]);
        if cfg!(target_os = "macos") {
            // macOS dialogs must run on the main thread.
            for f in dialog.pick_files().unwrap_or_default() {
                self.open(f);
            }
        } else {
            let (tx, rx) = channel();
            let notify = self.notify.clone();
            std::thread::spawn(move || {
                let _ = tx.send(dialog.pick_files().unwrap_or_default());
                notify();
            });
            self.picker = Some(rx);
        }
    }

    fn close_tab(&mut self, i: usize) {
        if self.tabs.get(i).is_some_and(Tab::dirty) {
            self.confirm = Some(Confirm::CloseTab(i));
            return;
        }
        self.close_tab_now(i);
    }

    fn close_tab_now(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.tabs.remove(i);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.settings.theme) {
            return;
        }
        ctx.set_theme(match self.settings.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        });
        self.applied_theme = Some(self.settings.theme);
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        if pressed(sc(Modifiers::COMMAND, Key::O)) {
            self.pick_files();
        }
        if pressed(sc(Modifiers::COMMAND, Key::W)) {
            self.close_tab(self.active);
        }
        // Shift variants first: plain shortcuts also match with Shift held.
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::G)) {
            self.goto = Some(GotoDialog {
                focus: true,
                by_offset: true,
                ..GotoDialog::default()
            });
        }
        if pressed(sc(Modifiers::COMMAND, Key::G)) {
            self.goto = Some(GotoDialog {
                focus: true,
                ..GotoDialog::default()
            });
        }
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S)) {
            self.save_active(true);
        }
        if pressed(sc(Modifiers::COMMAND, Key::S)) {
            self.save_active(false);
        }
        // Undo/redo of edits, unless a text field has the keyboard.
        if !ctx.egui_wants_keyboard_input() {
            if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z))
                || pressed(sc(Modifiers::COMMAND, Key::Y))
            {
                self.undo_redo(false);
            }
            if pressed(sc(Modifiers::COMMAND, Key::Z)) {
                self.undo_redo(true);
            }
        }
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::P)) {
            self.open_parse_dialog();
        }
        if pressed(sc(Modifiers::COMMAND, Key::F))
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.search.open();
        }
        if pressed(sc(Modifiers::COMMAND, Key::H))
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.search.open_replace();
        }
        if pressed(sc(Modifiers::COMMAND, Key::E)) {
            self.open_export(None);
        }
        let notify = self.notify.clone();
        if pressed(sc(Modifiers::SHIFT, Key::F3))
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            crate::search_ui::jump(tab, false, &notify);
        }
        if pressed(sc(Modifiers::NONE, Key::F3))
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            crate::search_ui::jump(tab, true, &notify);
        }
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::L))
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            let c = tab.info.cursor_col;
            tab.filter.toggle(c);
        }
        if pressed(sc(Modifiers::NONE, Key::F8)) {
            self.next_ragged();
        }
        if pressed(sc(Modifiers::NONE, Key::F1)) {
            self.show_shortcuts = true;
        }
        if !self.tabs.is_empty() && pressed(sc(Modifiers::CTRL, Key::Tab)) {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    fn undo_redo(&mut self, undo: bool) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.grid.is_editing() || tab.save.is_some() {
                return;
            }
            let done = if undo {
                tab.doc.overlay.undo()
            } else {
                tab.doc.overlay.redo()
            };
            if !done {
                self.messages.push(
                    if undo {
                        "Nothing to undo."
                    } else {
                        "Nothing to redo."
                    }
                    .into(),
                );
            }
        }
    }

    /// Ctrl+S (or Save As with a file dialog).
    fn save_active(&mut self, save_as: bool) {
        let notify = self.notify.clone();
        let backup = self.settings.backup;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.save.is_some() {
            return;
        }
        tab.grid.commit(&mut tab.doc);
        if save_as {
            let dialog = rfd::FileDialog::new()
                .set_title("Save as")
                .set_file_name(tab.title.clone());
            let dialog = match tab.path.parent() {
                Some(dir) => dialog.set_directory(dir),
                None => dialog,
            };
            if cfg!(target_os = "macos") {
                if let Some(p) = dialog.save_file() {
                    tab.start_save(p, false, notify);
                }
            } else {
                let (tx, rx) = channel();
                let n2 = notify.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(dialog.save_file());
                    n2();
                });
                self.save_as_picker = Some(rx);
            }
            return;
        }
        if !tab.dirty() {
            self.messages.push("There are no changes to save.".into());
            return;
        }
        let target = tab.path.clone();
        tab.start_save(target, backup, notify);
    }

    /// Finishes saves, rewrites and exports whose jobs have ended.
    fn poll_saves(&mut self) {
        let notify = self.notify.clone();
        let mut to_open = Vec::new();
        for tab in &mut self.tabs {
            let finished: Vec<crate::tab::SaveJob> = [&mut tab.save, &mut tab.export]
                .into_iter()
                .filter_map(|slot| slot.take_if(|j| !j.job.is_running()))
                .collect();
            for sj in finished {
                let res = sj.result.lock().ok().and_then(|mut r| r.take());
                let job_error = sj.job.error();
                let report = match res {
                    Some(Ok(report)) => report,
                    Some(Err(e)) => {
                        self.messages.push(match sj.kind {
                            WriteKind::Export { .. } => format!("Export failed: {e}"),
                            _ => e,
                        });
                        continue;
                    }
                    None => {
                        self.messages
                            .push(job_error.unwrap_or_else(|| {
                                match sj.kind {
                                    WriteKind::Export { .. } => "The export was cancelled.".into(),
                                    _ => {
                                        "The save was cancelled. The original file was not changed."
                                            .into()
                                    }
                                }
                            }));
                        continue;
                    }
                };
                match sj.kind {
                    WriteKind::Save | WriteKind::Rewrite => {
                        let outcome = if sj.target == tab.path {
                            tab.grid.reload_keep_layout();
                            tab.doc.reopen().map_err(|e| e.to_string())
                        } else {
                            tab.retarget(&sj.target, notify.clone())
                        };
                        if let Err(e) = outcome {
                            self.messages
                                .push(format!("Saved, but reopening the file failed: {e}"));
                        }
                        if sj.kind == WriteKind::Rewrite {
                            tab.search.reset();
                            self.messages.push(format!(
                                "Replace All rewrote {} rows.",
                                group(report.records_rewritten)
                            ));
                        }
                    }
                    WriteKind::Export { open } => {
                        self.messages.push(format!(
                            "Exported {} rows ({}) to {}.",
                            group(report.records_rewritten),
                            katak_core::sys::human_bytes(report.bytes_written),
                            sj.target.display()
                        ));
                        if open {
                            to_open.push(sj.target.clone());
                        }
                    }
                }
                if report.unmappable > 0 {
                    self.messages.push(format!(
                        "{} characters cannot be represented in the target encoding and were written as \"?\".",
                        group(report.unmappable as u64)
                    ));
                }
                if report.used_fallback_quote {
                    self.messages.push("Some values needed quoting but the file has no quote character; \" was used.".into());
                }
                self.perf.note(format!(
                    "{} {}: {} bytes, {} records written in {:.2} s",
                    sj.kind.verb(),
                    sj.target.display(),
                    report.bytes_written,
                    report.records_rewritten,
                    sj.job.elapsed().as_secs_f64()
                ));
            }
        }
        for p in to_open {
            self.open(p);
        }
        if self.quit_after_save && self.tabs.iter().all(|t| t.save.is_none()) {
            self.quit_after_save = false;
            if self.tabs.iter().any(Tab::dirty) {
                self.messages
                    .push("Not every file could be saved; Katak stays open.".into());
            } else {
                self.allow_close = true;
                self.notify.as_ref()();
            }
        }
    }

    fn open_parse_dialog(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.parse_dialog = Some(ParseDialog::new(tab.doc.dialect()));
        }
    }

    /// File → Export (optionally with the rows preselected).
    fn open_export(&mut self, rows: Option<Rows>) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.grid.commit(&mut tab.doc);
            let mut d = ExportDialog::new(tab);
            if let Some(r) = rows {
                d.rows = r;
            }
            tab.export_dialog = Some(d);
        }
    }

    fn next_ragged(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let after = tab.info.cursor.map_or(0, |c| c.rec.record);
            match tab.doc.locator().next_ragged(after) {
                Ok(Some(p)) => tab.grid.command(GridCommand::GotoRecord(p)),
                Ok(None) => {
                    let indexing = tab.doc.stats().indexing;
                    self.messages.push(if indexing {
                        "No more ragged rows in the indexed part yet (indexing is still running)."
                            .into()
                    } else {
                        "No more ragged rows.".into()
                    });
                }
                Err(e) => self.messages.push(e.to_string()),
            }
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open…  Ctrl+O").clicked() {
                    self.pick_files();
                }
                ui.menu_button("Open recent", |ui| {
                    if self.settings.recent.is_empty() {
                        ui.weak("(none)");
                    }
                    for p in self.settings.recent.clone() {
                        if ui.button(p.display().to_string()).clicked() {
                            self.open(p);
                        }
                    }
                });
                let current = self.tabs.get(self.active).map(|t| t.doc.dialect().encoding);
                let mut reopen_with = None;
                ui.add_enabled_ui(current.is_some(), |ui| {
                    ui.menu_button("Reopen with encoding", |ui| {
                        for e in katak_core::encoding::CHOICES {
                            if ui.radio(current == Some(*e), e.name()).clicked() {
                                reopen_with = Some(*e);
                                ui.close();
                            }
                        }
                    });
                });
                if let Some(e) = reopen_with {
                    self.reopen_with_encoding(e);
                }
                ui.separator();
                let has = !self.tabs.is_empty();
                if ui
                    .add_enabled(has, egui::Button::new("Save  Ctrl+S"))
                    .clicked()
                {
                    self.save_active(false);
                }
                if ui
                    .add_enabled(has, egui::Button::new("Save as…  Ctrl+Shift+S"))
                    .clicked()
                {
                    self.save_active(true);
                }
                ui.checkbox(&mut self.settings.backup, "Keep a .bak copy when saving");
                ui.separator();
                if ui
                    .add_enabled(has, egui::Button::new("Export / convert…  Ctrl+E"))
                    .on_hover_text("Write all rows, the filtered or sorted rows, or the selection with another delimiter, quoting, encoding or line ending")
                    .clicked()
                {
                    self.open_export(None);
                }
                let sorted = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|t| t.sort.as_ref().is_some_and(|s| s.is_shown()));
                if sorted && ui.button("Save sorted as…").clicked() {
                    self.open_export(Some(Rows::View));
                }
                ui.separator();
                if ui
                    .add_enabled(
                        !self.tabs.is_empty(),
                        egui::Button::new("Close tab  Ctrl+W"),
                    )
                    .clicked()
                {
                    self.close_tab(self.active);
                }
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                let mut undo = None;
                let mut discard = false;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let ro = tab.grid.read_only || tab.save.is_some();
                    if ui
                        .add_enabled(
                            !ro && tab.doc.overlay.can_undo(),
                            egui::Button::new("Undo  Ctrl+Z"),
                        )
                        .clicked()
                    {
                        undo = Some(true);
                    }
                    if ui
                        .add_enabled(
                            !ro && tab.doc.overlay.can_redo(),
                            egui::Button::new("Redo  Ctrl+Y"),
                        )
                        .clicked()
                    {
                        undo = Some(false);
                    }
                    ui.separator();
                    let actions: [(&str, EditAction, bool); 5] = [
                        ("Cut  Ctrl+X", EditAction::Cut, true),
                        ("Copy  Ctrl+C", EditAction::Copy, false),
                        ("Clear  Delete", EditAction::Clear, true),
                        ("Fill down  Ctrl+D", EditAction::FillDown, true),
                        ("Select all  Ctrl+A", EditAction::SelectAll, false),
                    ];
                    for (label, a, modifies) in actions {
                        if ui
                            .add_enabled(!(modifies && ro), egui::Button::new(label))
                            .clicked()
                        {
                            tab.grid.action(a);
                        }
                    }
                    if ui
                        .add_enabled(!ro, egui::Button::new("Paste  Ctrl+V"))
                        .clicked()
                    {
                        ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
                    }
                    ui.separator();
                    let structural: [(&str, EditAction); 8] = [
                        ("Insert row above", EditAction::InsertRowAbove),
                        ("Insert row below", EditAction::InsertRowBelow),
                        ("Delete selected rows", EditAction::DeleteRows),
                        ("Insert column left", EditAction::InsertColumnLeft),
                        ("Insert column right", EditAction::InsertColumnRight),
                        ("Move column left", EditAction::MoveColumnLeft),
                        ("Move column right", EditAction::MoveColumnRight),
                        ("Delete column", EditAction::DeleteColumn),
                    ];
                    for (i, (label, a)) in structural.into_iter().enumerate() {
                        if i == 3
                            && ui
                                .add_enabled(!ro, egui::Button::new("Rename column…"))
                                .clicked()
                        {
                            let c = tab.info.cursor_col;
                            let name = tab.doc.column_name(c).unwrap_or_default();
                            self.rename = Some((c, name));
                        }
                        if ui.add_enabled(!ro, egui::Button::new(label)).clicked() {
                            tab.grid.action(a);
                        }
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            tab.dirty() && tab.save.is_none(),
                            egui::Button::new("Discard all edits…"),
                        )
                        .clicked()
                    {
                        discard = true;
                    }
                    ui.checkbox(&mut tab.grid.read_only, "Read-only");
                } else {
                    ui.weak("Open a file first");
                }
                if let Some(u) = undo {
                    self.undo_redo(u);
                }
                if discard {
                    self.confirm = Some(Confirm::Discard);
                }
            });
            ui.menu_button("View", |ui| {
                let has = !self.tabs.is_empty();
                if ui
                    .add_enabled(has, egui::Button::new("Parsing options…  Ctrl+Shift+P"))
                    .clicked()
                {
                    self.open_parse_dialog();
                }
                if ui
                    .add_enabled(has, egui::Button::new("Go to row…  Ctrl+G"))
                    .clicked()
                {
                    self.goto = Some(GotoDialog {
                        focus: true,
                        ..GotoDialog::default()
                    });
                }
                if ui
                    .add_enabled(has, egui::Button::new("Go to byte offset…  Ctrl+Shift+G"))
                    .clicked()
                {
                    self.goto = Some(GotoDialog {
                        focus: true,
                        by_offset: true,
                        ..GotoDialog::default()
                    });
                }
                if ui
                    .add_enabled(has, egui::Button::new("Next ragged row  F8"))
                    .clicked()
                {
                    self.next_ragged();
                }
                ui.separator();
                ui.label("Theme");
                ui.radio_value(
                    &mut self.settings.theme,
                    ThemeChoice::System,
                    "Follow system",
                );
                ui.radio_value(&mut self.settings.theme, ThemeChoice::Light, "Light");
                ui.radio_value(&mut self.settings.theme, ThemeChoice::Dark, "Dark");
                ui.separator();
                ui.checkbox(&mut self.settings.monospace, "Monospace font");
                ui.horizontal(|ui| {
                    ui.label("Font size");
                    ui.add(
                        egui::Slider::new(&mut self.settings.font_size, 9.0..=28.0).step_by(1.0),
                    );
                });
                ui.weak("Zoom: Ctrl + / Ctrl - / Ctrl 0");
                ui.separator();
                ui.menu_button("Sort memory budget", |ui| {
                    for mb in [256u64, 512, 1024, 2048, 4096, 8192] {
                        let label = format!(
                            "{} (about {} rows)",
                            katak_core::sys::human_bytes(mb << 20),
                            group((mb << 20) / katak_core::sort::BYTES_PER_ROW)
                        );
                        ui.radio_value(&mut self.settings.sort_budget_mb, mb, label);
                    }
                });
            });
            ui.menu_button("Search", |ui| {
                let notify = self.notify.clone();
                let Some(tab) = self.tabs.get_mut(self.active) else {
                    ui.weak("Open a file first");
                    return;
                };
                if ui.button("Find…  Ctrl+F").clicked() {
                    tab.search.open();
                }
                if ui.button("Find next  F3").clicked() {
                    crate::search_ui::jump(tab, true, &notify);
                }
                if ui.button("Find previous  Shift+F3").clicked() {
                    crate::search_ui::jump(tab, false, &notify);
                }
                if ui.button("Replace…  Ctrl+H").clicked() {
                    tab.search.open_replace();
                }
                ui.separator();
                if ui.button("Filter…  Ctrl+Shift+L").clicked() {
                    let c = tab.info.cursor_col;
                    tab.filter.toggle(c);
                }
                if ui
                    .add_enabled(tab.filter.run.is_some(), egui::Button::new("Clear filter"))
                    .clicked()
                {
                    crate::filter_ui::clear(tab);
                }
                if ui
                    .add_enabled(
                        tab.sort.is_some() || tab.sort_pending.is_some(),
                        egui::Button::new("Clear sort"),
                    )
                    .clicked()
                {
                    crate::sort_ui::clear(tab);
                }
                ui.weak("Sort: right-click a column header");
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts  F1").clicked() {
                    self.show_shortcuts = true;
                }
                if ui.button("About Katak").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut close = None;
            for (i, t) in self.tabs.iter().enumerate() {
                let label = RichText::new(&t.title);
                let r = ui
                    .selectable_label(i == self.active, label)
                    .on_hover_text(t.path.display().to_string());
                if r.clicked() {
                    self.active = i;
                }
                if r.middle_clicked() {
                    close = Some(i);
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Close (Ctrl+W)")
                    .clicked()
                {
                    close = Some(i);
                }
                ui.separator();
            }
            if ui.button("Open…").clicked() {
                self.pick_files();
            }
            if let Some(i) = close {
                self.close_tab(i);
            }
            if let Some(tab) = self.tabs.get_mut(self.active) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Parsing options…").clicked() {
                        tab.parse_dialog = Some(ParseDialog::new(tab.doc.dialect()));
                    }
                    let mut frozen = tab.grid.frozen;
                    ui.add(egui::DragValue::new(&mut frozen).range(0..=50))
                        .on_hover_text("Number of frozen columns on the left");
                    ui.label("Frozen columns");
                    tab.grid.frozen = frozen;
                });
            }
        });
    }

    /// Re-parses the active file in another encoding.
    fn reopen_with_encoding(&mut self, e: katak_core::encoding::TextEncoding) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.dirty() || tab.save.is_some() {
            self.messages
                .push("Save or discard your edits first, then change the encoding.".into());
            return;
        }
        let mut d = tab.doc.dialect().clone();
        d.encoding = e;
        match tab.doc.set_dialect(d) {
            Ok(()) => tab.banner_dismissed = false,
            Err(err) => self.messages.push(err.to_string()),
        }
    }

    /// Reloads tab `i` from disk (its edits are dropped).
    fn reload(&mut self, i: usize) {
        if let Some(tab) = self.tabs.get_mut(i) {
            tab.grid.reload_keep_layout();
            tab.change_dismissed = None;
            if let Err(e) = tab.doc.reopen() {
                self.messages
                    .push(format!("Reloading {} failed: {e}", tab.title));
            }
        }
    }

    /// "The file changed on disk" for the active tab.
    fn change_banner(&mut self, ui: &mut egui::Ui) {
        let i = self.active;
        let Some(tab) = self.tabs.get_mut(i) else {
            return;
        };
        let Some(change) = self.watcher.change(&tab.doc) else {
            return;
        };
        if tab.change_dismissed == Some(change) || tab.save.is_some() {
            return;
        }
        let text = match change {
            crate::watch::Change::Gone => "The file was moved or deleted on disk. Katak still shows the contents it had when it was opened.".to_string(),
            crate::watch::Change::Modified { len } => {
                let before = tab.doc.source().len();
                let size = if len > before {
                    format!(" (it grew by {})", katak_core::sys::human_bytes(len - before))
                } else if len < before {
                    format!(" (it shrank by {})", katak_core::sys::human_bytes(before - len))
                } else {
                    String::new()
                };
                format!("Another program changed the file on disk{size}. Katak shows the version it opened.")
            }
        };
        let mut reload = false;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x1E, 0x4A, 0x6E))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::WHITE, text);
                    if matches!(change, crate::watch::Change::Modified { .. })
                        && ui.button("Reload").clicked()
                    {
                        reload = true;
                    }
                    if ui.button("Keep this version").clicked() {
                        tab.change_dismissed = Some(change);
                    }
                });
            });
        if reload {
            if tab.dirty() {
                self.confirm = Some(Confirm::Reload(i));
            } else {
                self.reload(i);
            }
        }
    }

    fn has_banner(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|t| !t.banner_dismissed && !t.doc.index().read().warnings.is_empty())
    }

    fn banner(&mut self, ui: &mut egui::Ui) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.banner_dismissed {
            return;
        }
        let warnings = tab.doc.index().read().warnings.clone();
        let Some(w) = warnings.first().copied() else {
            return;
        };
        let (offset, text) = match w {
            IndexWarning::UnterminatedQuote(o) => (
                o,
                format!(
                    "The file ends inside a quoted field opened at byte {}. The quote is probably never closed.",
                    group(o)
                ),
            ),
            IndexWarning::LongQuotedField(o) => (
                o,
                format!(
                    "A quoted field starting at byte {} is larger than 64 MB — possibly an unterminated quote.",
                    group(o)
                ),
            ),
        };
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x5A, 0x45, 0x10))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xFF, 0xE0, 0x9A),
                        RichText::new("Warning:").strong(),
                    );
                    ui.colored_label(egui::Color32::from_rgb(0xFF, 0xE0, 0x9A), text);
                    if ui.button("Go to it").clicked() {
                        tab.grid.command(GridCommand::GotoOffset(offset));
                    }
                    if ui.button("Parse with quote = none").clicked() {
                        let mut d = tab.doc.dialect().clone();
                        d.quote = None;
                        if let Err(e) = tab.doc.set_dialect(d) {
                            self.messages.push(e.to_string());
                        }
                    }
                    if ui.button("Dismiss").clicked() {
                        tab.banner_dismissed = true;
                    }
                });
            });
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading("Katak");
            ui.label("Open huge CSV / TSV files instantly. Nothing is loaded into memory.");
            ui.add_space(12.0);
            if ui
                .button(RichText::new("Open a file…").size(16.0))
                .clicked()
            {
                self.pick_files();
            }
            ui.add_space(6.0);
            ui.weak("or drop files here");
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // Parsing options.
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(dlg) = &mut tab.parse_dialog
        {
            let sample = tab.doc.sample().clone();
            let whole = tab.doc.sample_is_whole();
            let detection = tab.doc.detection().clone();
            match dlg.show(ctx, &sample, whole, &detection, false) {
                ParseAction::None => {}
                ParseAction::Close => tab.parse_dialog = None,
                ParseAction::Apply(d) => {
                    let ms = dlg.preview_ms;
                    match tab.doc.set_dialect(d) {
                        Ok(()) => {
                            tab.parse_dialog = None;
                            tab.banner_dismissed = false;
                            self.perf
                                .note(format!("parse options applied (last preview {ms:.1} ms)"));
                        }
                        Err(e) => self.messages.push(e.to_string()),
                    }
                }
            }
        }
        // Go to.
        let mut close_goto = false;
        if let Some(g) = &mut self.goto {
            let mut open = true;
            egui::Window::new("Go to")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut g.by_offset, false, "Row number");
                        ui.radio_value(&mut g.by_offset, true, "Byte offset");
                    });
                    let r = ui.add(egui::TextEdit::singleline(&mut g.text).hint_text(
                        if g.by_offset {
                            "e.g. 1073741824"
                        } else {
                            "e.g. 12400000"
                        },
                    ));
                    if g.focus {
                        r.request_focus();
                        g.focus = false;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Go").clicked() || enter {
                        let n: Option<u64> = g
                            .text
                            .chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse()
                            .ok();
                        if let (Some(n), Some(tab)) = (n, self.tabs.get_mut(self.active)) {
                            tab.grid.command(if g.by_offset {
                                GridCommand::GotoOffset(n)
                            } else {
                                GridCommand::GotoRow(n.saturating_sub(1))
                            });
                            close_goto = true;
                        }
                    }
                });
            if !open {
                close_goto = true;
            }
        }
        if close_goto {
            self.goto = None;
        }
        // Cell viewer / multi-line editor.
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let mut close = false;
            let mut apply = None;
            if let Some(v) = &mut tab.cell_viewer {
                let mut open = true;
                let title = format!(
                    "Cell — row {}, column {}",
                    tab.info.cursor_row + 1,
                    v.col + 1
                );
                egui::Window::new(title)
                    .open(&mut open)
                    .default_size([640.0, 420.0])
                    .show(ctx, |ui| {
                        if v.truncated {
                            ui.weak(format!(
                                "Showing the first {} KB of a larger cell (read-only).",
                                crate::tab::VIEWER_PAGE / 1024
                            ));
                        }
                        egui::Panel::bottom("viewer-buttons").show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if v.editable {
                                    if ui
                                        .add_enabled(
                                            v.text != v.original,
                                            egui::Button::new("Apply"),
                                        )
                                        .clicked()
                                    {
                                        apply = Some((v.row, v.col, v.text.clone()));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        close = true;
                                    }
                                }
                                if ui.button("Copy").clicked() {
                                    ui.ctx().copy_text(v.text.clone());
                                }
                            });
                        });
                        egui::ScrollArea::both()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if v.editable {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut v.text)
                                            .desired_width(f32::INFINITY)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                } else {
                                    let mut text = v.text.as_str();
                                    ui.add(
                                        egui::TextEdit::multiline(&mut text)
                                            .desired_width(f32::INFINITY)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                }
                            });
                    });
                close |= !open;
            }
            if let Some((row, col, text)) = apply {
                tab.doc.set_values(vec![(row.row_ref(), col, text)]);
                close = true;
            }
            if close {
                tab.cell_viewer = None;
            }
        }
        // Rename column.
        if let Some((c, name)) = &mut self.rename {
            let mut open = true;
            let mut done = None;
            egui::Window::new(format!("Rename column {}", *c + 1))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(260.0));
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() || enter {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            done = Some(false);
                        }
                    });
                });
            if done == Some(true)
                && let Some(tab) = self.tabs.get_mut(self.active)
            {
                tab.grid.action(EditAction::RenameColumn(*c, name.clone()));
            }
            if done.is_some() || !open {
                self.rename = None;
            }
        }
        // Export.
        let open_paths: Vec<PathBuf> = self.tabs.iter().map(|t| t.path.clone()).collect();
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(mut dlg) = tab.export_dialog.take()
        {
            match dlg.show(ctx, tab, &open_paths, &self.notify) {
                ExportAction::None => tab.export_dialog = Some(dlg),
                ExportAction::Close | ExportAction::Started => {}
            }
        }
        // Replace All rewrite.
        if let Some(Confirm::Rewrite { tab, backup, .. }) = &mut self.confirm {
            let i = *tab;
            let name = self.tabs.get(i).map_or("the file", |t| t.title.as_str());
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("katak-rewrite")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(format!(
                    "More than {} cells match. Instead of keeping that many undoable edits, Replace All can rewrite {name} on disk in one pass.",
                    group(katak_core::replace::DEFAULT_THRESHOLD as u64)
                ));
                ui.label("Only rows with a match are rewritten; everything else is copied byte for byte. This cannot be undone with Ctrl+Z.");
                ui.checkbox(backup, "Keep the current version as a .bak file");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Rewrite the file").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if let Some(a) = answer
                && let Some(Confirm::Rewrite {
                    tab,
                    matcher,
                    with,
                    backup,
                }) = self.confirm.take()
                && a
            {
                let notify = self.notify.clone();
                match self.tabs.get_mut(tab) {
                    Some(t) if t.dirty() => self.messages.push(
                        "Save or discard your edits first, then run Replace All again.".into(),
                    ),
                    Some(t) if t.save.is_some() => {
                        self.messages.push("A save is still running.".into())
                    }
                    Some(t) => t.start_rewrite(matcher, with, backup, notify),
                    None => {}
                }
            }
        }
        // Large clipboard copy.
        if let Some(text) = &self.pending_copy {
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("katak-copy-confirm")).show(ctx, |ui| {
                ui.label(format!(
                    "Copy {} to the clipboard?",
                    katak_core::sys::human_bytes(text.len() as u64)
                ));
                ui.horizontal(|ui| {
                    if ui.button("Copy").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if let Some(a) = answer {
                if a {
                    ctx.copy_text(text.clone());
                }
                self.pending_copy = None;
            }
        }
        // Unsaved changes.
        if let Some(c) = &self.confirm
            && !matches!(c, Confirm::Rewrite { .. })
        {
            let (question, save_label, drop_label) = match c {
                Confirm::CloseTab(i) => (
                    format!(
                        "Save changes to {} before closing?",
                        self.tabs.get(*i).map_or("the file", |t| t.title.as_str())
                    ),
                    "Save",
                    "Close without saving",
                ),
                Confirm::Quit => (
                    "Some files have unsaved changes. Save them before quitting?".to_string(),
                    "Save all",
                    "Quit without saving",
                ),
                Confirm::Discard => (
                    "Discard all unsaved edits of this file? This cannot be undone.".to_string(),
                    "",
                    "Discard",
                ),
                Confirm::Reload(_) => (
                    "Reload the file from disk? Your unsaved edits will be lost.".to_string(),
                    "",
                    "Reload",
                ),
                Confirm::Rewrite { .. } => unreachable!(),
            };
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("katak-confirm")).show(ctx, |ui| {
                ui.label(question);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if !save_label.is_empty() && ui.button(save_label).clicked() {
                        answer = Some(1);
                    }
                    if ui.button(drop_label).clicked() {
                        answer = Some(2);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(0);
                    }
                });
            });
            if let Some(a) = answer {
                let c = self.confirm.take();
                match (c, a) {
                    (Some(Confirm::CloseTab(i)), 1) => {
                        self.active = i.min(self.tabs.len().saturating_sub(1));
                        self.save_active(false);
                    }
                    (Some(Confirm::CloseTab(i)), 2) => self.close_tab_now(i),
                    (Some(Confirm::Quit), 1) => {
                        let (notify, backup) = (self.notify.clone(), self.settings.backup);
                        for tab in self
                            .tabs
                            .iter_mut()
                            .filter(|t| t.dirty() && t.save.is_none())
                        {
                            tab.grid.commit(&mut tab.doc);
                            let target = tab.path.clone();
                            tab.start_save(target, backup, notify.clone());
                        }
                        self.quit_after_save = true;
                    }
                    (Some(Confirm::Quit), 2) => {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    (Some(Confirm::Reload(i)), 2) => self.reload(i),
                    (Some(Confirm::Discard), 2) => {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            tab.doc.overlay.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
        // Messages.
        if !self.messages.is_empty() {
            let mut open = true;
            egui::Window::new("Katak")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    for m in &self.messages {
                        ui.label(m);
                    }
                    if ui.button("OK").clicked() {
                        self.messages.clear();
                    }
                });
            if !open {
                self.messages.clear();
            }
        }
        if self.show_shortcuts {
            egui::Window::new("Keyboard shortcuts")
                .open(&mut self.show_shortcuts)
                .collapsible(false)
                .show(ctx, |ui| {
                    egui::Grid::new("keys").striped(true).show(ui, |ui| {
                        for (k, v) in SHORTCUTS {
                            ui.monospace(*k);
                            ui.label(*v);
                            ui.end_row();
                        }
                    });
                });
        }
        if self.show_about {
            egui::Window::new("About Katak")
                .open(&mut self.show_about)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.heading(format!("Katak {}", env!("CARGO_PKG_VERSION")));
                    ui.label("A fast, non-freezing viewer/editor for huge delimited text files.");
                    ui.label("Katak means \"cell\" in Uzbek.");
                    ui.hyperlink_to(
                        "github.com/Erkinbek/katak",
                        "https://github.com/Erkinbek/katak",
                    );
                    ui.weak("MIT License");
                    if let Some(b) = katak_core::sys::private_bytes() {
                        ui.weak(format!(
                            "Private memory: {}",
                            katak_core::sys::human_bytes(b)
                        ));
                    }
                });
        }
    }
}

impl KatakApp {
    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let painted = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.first_paint.is_some())
            || self.tabs.is_empty();
        let Some(plan) = &mut self.shot else { return };
        if plan.done.load(Ordering::Relaxed) {
            // Scripted run: quit even with unsaved edits.
            self.allow_close = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        ctx.request_repaint();
        if !painted {
            return;
        }
        plan.frames += 1;
        let Some(plan) = &mut self.shot else { return };
        let since = *plan.since.get_or_insert_with(Instant::now);
        // Let window animations finish.
        if since.elapsed() >= plan.delay && !plan.requested {
            plan.requested = true;
            let (path, done) = (plan.path.clone(), plan.done.clone());
            let repaint = ctx.clone();
            ctx.request_screenshot(move |img| {
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                let _ = crate::perf::write_png(&path, img.size[0], img.size[1], &rgba);
                done.store(true, Ordering::Relaxed);
                repaint.request_repaint();
            });
        }
    }

    /// `--show`: open a dialog or run a search/filter (screenshots, self-test).
    fn run_show(&mut self, show: Option<&str>) {
        {
            match show {
                Some("parse") => self.open_parse_dialog(),
                Some("parse-raw") => {
                    self.open_parse_dialog();
                    if let Some(d) = self
                        .tabs
                        .get_mut(self.active)
                        .and_then(|t| t.parse_dialog.as_mut())
                    {
                        d.raw_preview = true;
                    }
                }
                Some("goto") => {
                    self.goto = Some(GotoDialog {
                        focus: true,
                        text: "12400000".into(),
                        ..GotoDialog::default()
                    })
                }
                Some("shortcuts") => self.show_shortcuts = true,
                Some("edit") => {
                    // Exercise the overlay: an edited cell, a deleted row,
                    // an inserted row and the inline editor.
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        let first = tab.doc.first_lpos();
                        if let Ok((rows, _)) = tab.doc.lrows_from(first, 5)
                            && rows.len() == 5
                        {
                            tab.doc.set_values(vec![(
                                rows[1].row_ref(),
                                1,
                                "Edited, with a comma".into(),
                            )]);
                            if let Some(span) = rows[3].span() {
                                tab.doc.overlay.delete_rows(
                                    &[],
                                    &[katak_core::overlay::Deleted {
                                        start: span.start,
                                        end: span.end,
                                        records: 1,
                                    }],
                                );
                            }
                        }
                        tab.grid.action(EditAction::InsertRowBelow);
                        tab.grid.action(EditAction::StartEdit);
                    }
                }
                Some(s) if s.starts_with("search=") => {
                    let notify = self.notify.clone();
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.search.open();
                        tab.search.query = s["search=".len()..].to_string();
                        crate::search_ui::jump(tab, true, &notify);
                    }
                }
                Some("export") => self.open_export(None),
                Some("selectall") => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.grid.action(EditAction::SelectAll);
                    }
                }
                Some(s) if s.starts_with("exportto=") => {
                    let notify = self.notify.clone();
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        let path = PathBuf::from(&s["exportto=".len()..]);
                        let mut d = ExportDialog::new(tab);
                        self.messages.extend(d.export_tsv(tab, path, &notify));
                    }
                }
                Some(s) if s.starts_with("replace=") => {
                    let notify = self.notify.clone();
                    if let Some(tab) = self.tabs.get_mut(self.active)
                        && let Some((find, with)) = s["replace=".len()..].split_once(':')
                    {
                        tab.search.open_replace();
                        tab.search.query = find.to_string();
                        tab.search.with = with.to_string();
                        crate::search_ui::jump(tab, true, &notify);
                    }
                }
                Some(s) if s.starts_with("replaceall=") => {
                    let notify = self.notify.clone();
                    if let Some(tab) = self.tabs.get_mut(self.active)
                        && let Some((find, with)) = s["replaceall=".len()..].split_once(':')
                    {
                        tab.search.open_replace();
                        tab.search.query = find.to_string();
                        tab.search.with = with.to_string();
                        self.messages
                            .extend(crate::search_ui::replace_all(tab, &notify));
                    }
                }
                Some(s) if s.starts_with("sort=") => {
                    if let Some(tab) = self.tabs.get_mut(self.active)
                        && let Some((col, dir)) = s["sort=".len()..].split_once(':')
                    {
                        let col = col.parse::<usize>().unwrap_or(1).saturating_sub(1);
                        crate::sort_ui::request(tab, col, dir == "desc");
                    }
                }
                Some(s) if s.starts_with("filter=") => {
                    let notify = self.notify.clone();
                    if let Some(tab) = self.tabs.get_mut(self.active)
                        && let Some((col, text)) = s["filter=".len()..].split_once(':')
                    {
                        let col = col.parse::<usize>().unwrap_or(1).saturating_sub(1);
                        crate::filter_ui::apply_quick(tab, &notify, col, text);
                    }
                }
                _ => {}
            }
        }
    }
}

const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+O", "Open files"),
    ("Ctrl+W", "Close tab"),
    ("Ctrl+Tab", "Next tab"),
    ("Ctrl+Shift+P", "Parsing options"),
    ("Ctrl+G", "Go to row"),
    ("Ctrl+Shift+G", "Go to byte offset"),
    ("Ctrl+F", "Find"),
    ("Ctrl+H", "Replace"),
    ("F3 / Shift+F3", "Next / previous match"),
    ("Ctrl+Shift+L", "Filter bar"),
    ("F8", "Next ragged row"),
    ("Ctrl+E", "Export / convert"),
    ("Ctrl+S / Ctrl+Shift+S", "Save / Save as"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
    ("Right-click a header", "Sort, filter, column operations"),
    ("Arrows", "Move the cursor (Shift extends the selection)"),
    ("PgUp / PgDn", "Page up / down"),
    ("Home / End", "First / last column"),
    ("Ctrl+Home / Ctrl+End", "First / last row"),
    ("Shift+Wheel", "Scroll horizontally"),
    ("Ctrl+Plus / Ctrl+Minus / Ctrl+0", "Zoom in / out / reset"),
    ("Double-click", "Open the whole cell"),
    ("F1", "This list"),
];

impl eframe::App for KatakApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.perf.begin_frame();
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);

        if let Some(rx) = &self.picker
            && let Ok(files) = rx.try_recv()
        {
            self.picker = None;
            for f in files {
                self.open(f);
            }
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.tabs.iter().any(Tab::dirty)
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }
        if self.allow_close
            && !ctx.input(|i| i.viewport().close_requested())
            && self.confirm.is_none()
            && !self.quit_after_save
        {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if let Some(rx) = &self.save_as_picker
            && let Ok(choice) = rx.try_recv()
        {
            self.save_as_picker = None;
            let notify = self.notify.clone();
            if let (Some(p), Some(tab)) = (choice, self.tabs.get_mut(self.active)) {
                tab.start_save(p, false, notify);
            }
        }
        self.poll_saves();
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for p in dropped {
            self.open(p);
        }
        self.shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui));
        egui::Panel::top("tabs").show(ui, |ui| self.tab_bar(ui));
        self.watcher
            .watch(self.tabs.iter().map(|t| t.path.clone()).collect());
        let changed = self.tabs.get(self.active).is_some_and(|t| {
            self.watcher
                .change(&t.doc)
                .is_some_and(|c| t.change_dismissed != Some(c) && t.save.is_none())
        });
        if changed {
            egui::Panel::top("changed")
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.change_banner(ui));
        }
        if self.has_banner() {
            egui::Panel::top("banner")
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.banner(ui));
        }
        let notify = self.notify.clone();
        let budget = self.settings.sort_budget();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            // The sort first: on a reload it must forget its rows before
            // the filter is cleared (which would ask for a re-sort).
            self.messages
                .extend(crate::sort_ui::tick(tab, budget, &notify));
            match crate::search_ui::poll_replace(tab) {
                Some(ReplaceOutcome::Message(m)) => self.messages.push(m),
                Some(ReplaceOutcome::TooMany { matcher, with }) => {
                    self.confirm = Some(Confirm::Rewrite {
                        tab: i,
                        matcher,
                        with,
                        backup: true,
                    })
                }
                None => {}
            }
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            crate::search_ui::tick(tab, &notify);
            crate::filter_ui::tick(tab);
            if tab.search.open {
                let messages = &mut self.messages;
                egui::Panel::top("search")
                    .show(ui, |ui| crate::search_ui::bar(ui, tab, &notify, messages));
            }
            if tab.filter.open {
                let columns = tab.info.columns;
                egui::Panel::top("filter")
                    .show(ui, |ui| crate::filter_ui::bar(ui, tab, &notify, columns));
            }
        }

        let colors = GridColors::from_visuals(&ctx.global_style().visuals);
        let (font_size, mono) = (self.settings.font_size, self.settings.monospace);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let stats = tab.doc.stats();
            let col = tab.info.cursor_col;
            let name = tab
                .doc
                .header_record()
                .ok()
                .flatten()
                .and_then(|h| tab.doc.cells(&h, col..col + 1, 64).ok())
                .and_then(|c| c.into_iter().next())
                .filter(|c| !c.missing)
                .map(|c| c.text);
            egui::Panel::bottom("status").show(ui, |ui| {
                crate::status::show(ui, tab, &stats, name.as_deref())
            });
            if tab.search.open && tab.search.show_results && tab.search.run.is_some() {
                egui::Panel::bottom("results")
                    .resizable(true)
                    .default_size(180.0)
                    .show(ui, |ui| crate::search_ui::results(ui, tab));
            }
            let dirty = if tab.dirty() { "* " } else { "" };
            ctx.send_viewport_cmd(ViewportCommand::Title(format!(
                "{dirty}{} — Katak",
                tab.title
            )));
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                let info = tab.grid.show(ui, &mut tab.doc, &colors, font_size, mono);
                if tab.first_paint.is_none() && info.painted_rows > 0 {
                    let ms = self.perf.since_start().as_secs_f64() * 1000.0;
                    tab.first_paint = Some(ms);
                    self.perf.note(format!(
                        "first frame with data: {ms:.1} ms after process start ({} rows painted)",
                        info.painted_rows
                    ));
                }
                if let Some((row, col)) = info.open_cell {
                    tab.open_cell(row, col);
                }
                if let Some(text) = &info.copy {
                    ui.ctx().copy_text(text.clone());
                }
                if let Some(text) = info.confirm_copy.clone() {
                    self.pending_copy = Some(text);
                }
                if let Some(c) = info.rename_column {
                    self.rename = Some((c, tab.doc.column_name(c).unwrap_or_default()));
                }
                if let Some(m) = info.message.clone() {
                    self.messages.push(m);
                }
                if let Some(c) = info.filter_column {
                    tab.filter.add_column(c);
                }
                if let Some((c, descending)) = info.sort {
                    crate::sort_ui::request(tab, c, descending);
                }
                if info.clear_sort {
                    crate::sort_ui::clear(tab);
                }
                tab.info = info;
            } else {
                self.welcome(ui);
            }
        });

        self.dialogs(&ctx);
        if self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.first_paint.is_some())
            && let Some(show) = self.pending_show.take()
        {
            self.run_show(Some(&show));
        }

        // Scripted self-test.
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.first_paint.is_some()
        {
            let wide = tab.info.columns > 100;
            let indexing = tab.doc.stats().indexing;
            if let Some(cmds) = self.perf.self_test_step(wide, indexing) {
                for c in cmds {
                    tab.grid.command(c);
                }
                ctx.request_repaint();
            }
        }
        self.perf.end_frame();
        if self.perf.done {
            self.allow_close = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        self.screenshot_step(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(f) = self.perf.first_frame {
            self.perf.note(format!(
                "first frame (any): {:.1} ms after process start",
                f.as_secs_f64() * 1000.0
            ));
        }
        if let Some(b) = katak_core::sys::private_bytes() {
            self.perf.note(format!(
                "private memory at exit: {}",
                katak_core::sys::human_bytes(b)
            ));
        }
        self.perf.flush();
        // Dropping the tabs cancels their background jobs.
        self.tabs.clear();
    }
}
