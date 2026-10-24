//! The search bar (Ctrl+F), F3 navigation and the results panel.

use eframe::egui::{self, Key, RichText};
use katak_core::job::Notify;
use katak_core::locate::RowPos;
use katak_core::search::{self, Hit, Search, SearchOptions};

use crate::grid::{GridCommand, group};
use crate::tab::Tab;

/// Search bar state of one tab.
#[derive(Default)]
pub struct SearchBar {
    pub open: bool,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub whole_cell: bool,
    pub current_column: bool,
    pub focus: bool,
    pub show_results: bool,
    pub run: Option<Search>,
    run_options: Option<SearchOptions>,
    generation: u64,
    pub error: Option<String>,
    /// Jump to the first hit after the cursor once results arrive.
    pending_jump: Option<bool>,
    pub selected: Option<usize>,
}

impl SearchBar {
    fn options(&self, cursor_col: usize) -> SearchOptions {
        SearchOptions {
            pattern: self.query.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            whole_cell: self.whole_cell,
            columns: self.current_column.then(|| vec![cursor_col]),
        }
    }

    pub fn open(&mut self) {
        self.open = true;
        self.focus = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.pending_jump = None;
    }

    /// Discards results (e.g. after parsing options changed).
    pub fn reset(&mut self) {
        self.run = None;
        self.run_options = None;
        self.selected = None;
        self.pending_jump = None;
    }
}

/// Starts (or restarts) the search for the current query.
pub fn start(tab: &mut Tab, notify: &Notify) {
    let opts = tab.search.options(tab.info.cursor_col);
    if opts.pattern.is_empty() {
        tab.search.reset();
        tab.grid.highlight = None;
        return;
    }
    match search::start(
        tab.doc.target(),
        opts.clone(),
        search::DEFAULT_CAP,
        notify.clone(),
    ) {
        Ok(s) => {
            tab.grid.highlight = Some(s.matcher.clone());
            tab.search.run = Some(s);
            tab.search.run_options = Some(opts);
            tab.search.generation = tab.doc.generation();
            tab.search.error = None;
            tab.search.selected = None;
            tab.search.show_results = true;
        }
        Err(e) => tab.search.error = Some(e),
    }
}

/// F3 / Shift+F3: next or previous hit relative to the cursor.
pub fn jump(tab: &mut Tab, forward: bool, notify: &Notify) {
    let opts = tab.search.options(tab.info.cursor_col);
    if tab.search.run.is_none() || tab.search.run_options.as_ref() != Some(&opts) {
        start(tab, notify);
    }
    let Some(run) = &tab.search.run else { return };
    let (offset, col) = tab
        .info
        .cursor
        .map_or((0, 0), |c| (c.offset, tab.info.cursor_col));
    let hit = if forward {
        run.next_after(offset, col)
    } else {
        run.prev_before(offset, col)
    };
    match hit {
        Some(h) => {
            go_to_hit(tab, h);
            tab.search.pending_jump = None;
        }
        // Nothing found yet: jump as soon as a hit arrives.
        None if run.job.is_running() => tab.search.pending_jump = Some(forward),
        None => {}
    }
}

fn go_to_hit(tab: &mut Tab, h: Hit) {
    tab.grid.command(GridCommand::GotoRecord(RowPos {
        offset: h.offset,
        record: h.record,
        exact: true,
    }));
    tab.grid.cursor_col = h.col as usize;
    let index = tab.search.run.as_ref().and_then(|r| {
        let d = r.data.lock();
        let i = d.hits.partition_point(|x| *x < h);
        (d.hits.get(i) == Some(h)).then_some(i)
    });
    tab.search.selected = index;
}

/// Per-frame housekeeping: invalidation and pending jumps.
pub fn tick(tab: &mut Tab, notify: &Notify) {
    if tab.search.run.is_some() && tab.search.generation != tab.doc.generation() {
        tab.search.reset();
        tab.grid.highlight = None;
    }
    if let Some(forward) = tab.search.pending_jump {
        let has_hits = tab
            .search
            .run
            .as_ref()
            .is_some_and(|r| !r.data.lock().hits.is_empty());
        let running = tab.search.run.as_ref().is_some_and(|r| r.job.is_running());
        if has_hits {
            jump(tab, forward, notify);
        } else if !running {
            tab.search.pending_jump = None;
        }
    }
}

/// The search bar.
pub fn bar(ui: &mut egui::Ui, tab: &mut Tab, notify: &Notify) {
    ui.horizontal(|ui| {
        ui.label("Find");
        let edit = egui::TextEdit::singleline(&mut tab.search.query)
            .desired_width(280.0)
            .hint_text(if tab.search.regex {
                "regular expression"
            } else {
                "text"
            });
        let r = ui.add(edit);
        if tab.search.focus {
            r.request_focus();
            tab.search.focus = false;
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let shift = ui.input(|i| i.modifiers.shift);
            jump(tab, !shift, notify);
            r.request_focus();
        }
        if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            tab.search.close();
        }
        let s = &mut tab.search;
        ui.toggle_value(&mut s.case_sensitive, "Aa")
            .on_hover_text("Match case");
        ui.toggle_value(&mut s.whole_word, "W")
            .on_hover_text("Whole word");
        ui.toggle_value(&mut s.whole_cell, "Cell")
            .on_hover_text("Whole cell");
        ui.toggle_value(&mut s.regex, ".*")
            .on_hover_text("Regular expression");
        ui.toggle_value(&mut s.current_column, "Column")
            .on_hover_text("Only the cursor's column");
        if ui.button("Find all").clicked() {
            start(tab, notify);
        }
        if ui
            .button("Prev")
            .on_hover_text("Previous match (Shift+F3)")
            .clicked()
        {
            jump(tab, false, notify);
        }
        if ui.button("Next").on_hover_text("Next match (F3)").clicked() {
            jump(tab, true, notify);
        }
        if let Some(e) = &tab.search.error {
            ui.colored_label(egui::Color32::from_rgb(0xD0, 0x3B, 0x3B), e);
        } else if let Some(run) = &tab.search.run {
            let (n, capped, done) = {
                let d = run.data.lock();
                (d.hits.len(), d.capped, d.done)
            };
            let count = if capped {
                format!("{}+ matches (more exist)", group(n as u64))
            } else {
                format!("{} matches", group(n as u64))
            };
            if run.job.is_running() {
                ui.add(
                    egui::ProgressBar::new(run.job.fraction())
                        .desired_width(90.0)
                        .text(format!("{:.0}%", run.job.fraction() * 100.0)),
                );
                ui.label(count);
            } else if done && n == 0 {
                ui.label("No matches");
            } else {
                ui.label(count);
            }
            ui.toggle_value(&mut tab.search.show_results, "Results");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
                tab.search.close();
            }
        });
    });
}

/// The virtualized results list.
pub fn results(ui: &mut egui::Ui, tab: &mut Tab) {
    let Some(run) = &tab.search.run else { return };
    let data = run.data.clone();
    let matcher = run.matcher.clone();
    let n = data.lock().hits.len();
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let hdr = tab.doc.header_records();
    let header = tab.doc.header_record().ok().flatten();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_h, n, |ui, range| {
            let hits: Vec<Hit> = data.lock().hits.slice(range.start, range.end);
            for (k, h) in hits.iter().enumerate() {
                let i = range.start + k;
                let pos = RowPos {
                    offset: h.offset,
                    record: h.record,
                    exact: true,
                };
                let col = h.col as usize;
                let rec = tab
                    .doc
                    .locator()
                    .records_from(pos, 1)
                    .ok()
                    .and_then(|f| f.rows.into_iter().next())
                    .map(|r| r.1);
                let preview = rec
                    .and_then(|r| tab.doc.cells(&r, col..col + 1, 160).ok())
                    .and_then(|c| c.into_iter().next())
                    .map(|c| c.text)
                    .unwrap_or_default();
                let name = header
                    .and_then(|hr| tab.doc.cells(&hr, col..col + 1, 40).ok())
                    .and_then(|c| c.into_iter().next())
                    .filter(|c| !c.missing)
                    .map(|c| c.text)
                    .unwrap_or_else(|| format!("Column {}", col + 1));
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    let sel = tab.search.selected == Some(i);
                    let label = format!("Row {}", group(h.record.saturating_sub(hdr) + 1));
                    let label = egui::Button::selectable(sel, RichText::new(label).monospace());
                    if ui.add_sized([120.0, row_h], label).clicked() {
                        clicked = Some((i, *h));
                    }
                    ui.add_sized(
                        [150.0, row_h],
                        egui::Label::new(RichText::new(name).weak()).truncate(),
                    );
                    let marks = matcher.ranges(&preview, 8);
                    let mut job = egui::text::LayoutJob::default();
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let color = ui.visuals().text_color();
                    let hl = ui.visuals().selection.bg_fill;
                    let mut at = 0;
                    for m in marks {
                        if m.start < at
                            || !preview.is_char_boundary(m.start)
                            || !preview.is_char_boundary(m.end)
                        {
                            continue;
                        }
                        job.append(
                            &preview[at..m.start],
                            0.0,
                            egui::TextFormat::simple(font.clone(), color),
                        );
                        job.append(
                            &preview[m.start..m.end],
                            0.0,
                            egui::TextFormat {
                                background: hl,
                                ..egui::TextFormat::simple(font.clone(), color)
                            },
                        );
                        at = m.end;
                    }
                    job.append(
                        &preview[at..],
                        0.0,
                        egui::TextFormat::simple(font.clone(), color),
                    );
                    ui.label(job);
                });
            }
        });
    if let Some((i, h)) = clicked {
        go_to_hit(tab, h);
        tab.search.selected = Some(i);
    }
}
