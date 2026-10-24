//! The application shell: tabs, menus, shortcuts, dialogs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText, ViewportCommand};
use katak_core::index::IndexWarning;
use katak_core::job::Notify;
use serde::{Deserialize, Serialize};

use crate::Args;
use crate::grid::{GridCommand, group};
use crate::parse_dialog::{ParseAction, ParseDialog};
use crate::perf::Perf;
use crate::tab::Tab;
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::System,
            font_size: 14.0,
            monospace: false,
            recent: Vec::new(),
        }
    }
}

/// `--screenshot`: what to capture and when.
struct ShotPlan {
    path: PathBuf,
    show: Option<String>,
    frames: usize,
    since: Option<Instant>,
    requested: bool,
    done: Arc<AtomicBool>,
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
}

impl KatakApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args, started: Instant) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        let ctx = cc.egui_ctx.clone();
        let notify: Notify = Arc::new(move || ctx.request_repaint());
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
                show: args.show.clone(),
                frames: 0,
                since: None,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
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
        match Tab::open(&path, self.notify.clone()) {
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
        if pressed(sc(Modifiers::COMMAND, Key::G)) {
            self.goto = Some(GotoDialog {
                focus: true,
                ..GotoDialog::default()
            });
        }
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::G)) {
            self.goto = Some(GotoDialog {
                focus: true,
                by_offset: true,
                ..GotoDialog::default()
            });
        }
        if pressed(sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::P)) {
            self.open_parse_dialog();
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

    fn open_parse_dialog(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.parse_dialog = Some(ParseDialog::new(tab.doc.dialect()));
        }
    }

    fn next_ragged(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let after = tab.info.cursor.map_or(0, |c| c.record);
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
        // Cell viewer.
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let mut close = false;
            if let Some(v) = &tab.cell_viewer {
                let mut open = true;
                egui::Window::new(format!("Cell — column {}", v.col + 1))
                    .open(&mut open)
                    .default_size([640.0, 420.0])
                    .show(ctx, |ui| {
                        if v.truncated {
                            ui.weak(format!(
                                "Showing the first {} KB of a {} cell.",
                                crate::tab::VIEWER_PAGE / 1024,
                                katak_core::sys::human_bytes(v.rec.content_end - v.rec.start)
                            ));
                        }
                        egui::ScrollArea::both()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let mut text = v.text.as_str();
                                ui.add(
                                    egui::TextEdit::multiline(&mut text)
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace),
                                );
                            });
                    });
                close = !open;
            }
            if close {
                tab.cell_viewer = None;
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
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        ctx.request_repaint();
        if !painted {
            return;
        }
        plan.frames += 1;
        let show = plan.show.clone();
        if plan.frames == 2 {
            match show.as_deref() {
                Some("parse") => self.open_parse_dialog(),
                Some("goto") => {
                    self.goto = Some(GotoDialog {
                        focus: true,
                        text: "12400000".into(),
                        ..GotoDialog::default()
                    })
                }
                Some("shortcuts") => self.show_shortcuts = true,
                _ => {}
            }
        }
        let Some(plan) = &mut self.shot else { return };
        let since = *plan.since.get_or_insert_with(Instant::now);
        // Let window animations finish.
        if since.elapsed().as_millis() >= 800 && !plan.requested {
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
}

const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+O", "Open files"),
    ("Ctrl+W", "Close tab"),
    ("Ctrl+Tab", "Next tab"),
    ("Ctrl+Shift+P", "Parsing options"),
    ("Ctrl+G", "Go to row"),
    ("Ctrl+Shift+G", "Go to byte offset"),
    ("F8", "Next ragged row"),
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
        if self.has_banner() {
            egui::Panel::top("banner")
                .show_separator_line(false)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.banner(ui));
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
            ctx.send_viewport_cmd(ViewportCommand::Title(format!("{} — Katak", tab.title)));
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
                if let Some((rec, col)) = info.double_clicked {
                    tab.open_cell(rec, col);
                }
                tab.info = info;
            } else {
                self.welcome(ui);
            }
        });

        self.dialogs(&ctx);

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
