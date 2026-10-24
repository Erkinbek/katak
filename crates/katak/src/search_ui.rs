//! The search bar (Ctrl+F), Replace (Ctrl+H), F3 navigation and the
//! results panel.

use std::sync::Arc;

use eframe::egui::{self, Key, RichText};
use katak_core::job::Notify;
use katak_core::locate::RowPos;
use katak_core::replace::{self, ReplacePlan};
use katak_core::search::{self, Hit, Matcher, Search, SearchOptions};

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
    /// The Replace row is shown.
    pub replace: bool,
    pub with: String,
    focus_with: bool,
    /// A running Replace All.
    pub plan: Option<ReplacePlan>,
    plan_generation: u64,
}

/// What a finished Replace All asks of the app.
pub enum ReplaceOutcome {
    Message(String),
    /// More cells match than edits are kept for: offer a rewrite of the
    /// file instead.
    TooMany {
        matcher: Arc<Matcher>,
        with: String,
    },
}

/// Largest cell "Replace" edits in place.
const MAX_REPLACE_CELL: usize = 16 << 20;

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

    /// Ctrl+H: the search bar with the Replace row.
    pub fn open_replace(&mut self) {
        self.open = true;
        self.replace = true;
        if self.query.is_empty() {
            self.focus = true;
        } else {
            self.focus_with = true;
        }
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
        .map_or((0, 0), |c| (c.rec.offset, tab.info.cursor_col));
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

/// "Replace": replaces the matches in the cursor's cell, then moves to
/// the next match. When the cursor's cell does not match, it only moves.
pub fn replace_current(tab: &mut Tab, notify: &Notify) -> Option<String> {
    let opts = tab.search.options(tab.info.cursor_col);
    if opts.pattern.is_empty() {
        return None;
    }
    if tab.grid.read_only || tab.save.is_some() {
        return Some("The file is read-only or being saved.".into());
    }
    let matcher = match Matcher::new(&opts) {
        Ok(m) => m,
        Err(e) => {
            tab.search.error = Some(e);
            return None;
        }
    };
    let col = tab.grid.cursor_col;
    if let Some(row) = tab.grid.current_row(&mut tab.doc)
        && matcher.wants_column(col)
        && let Ok(value) = tab.doc.value(&row, col, MAX_REPLACE_CELL + 1)
    {
        if value.len() > MAX_REPLACE_CELL {
            return Some("This cell is too large to edit here.".into());
        }
        if matcher.is_match(&value) {
            let new = matcher.replace(&value, &tab.search.with, true);
            if new != value {
                let new = new.into_owned();
                tab.doc.set_values(vec![(row.row_ref(), col, new)]);
            }
        }
    }
    jump(tab, true, notify);
    None
}

/// "Replace all": plans the replacement in the background.
pub fn replace_all(tab: &mut Tab, notify: &Notify) -> Option<String> {
    let opts = tab.search.options(tab.info.cursor_col);
    if opts.pattern.is_empty() {
        return None;
    }
    if tab.grid.read_only || tab.save.is_some() {
        return Some("The file is read-only or being saved.".into());
    }
    match replace::plan(
        tab.doc.target(),
        opts,
        tab.search.with.clone(),
        replace::DEFAULT_THRESHOLD,
        notify.clone(),
    ) {
        Ok(p) => {
            tab.search.plan = Some(p);
            tab.search.plan_generation = tab.doc.generation();
            tab.search.error = None;
        }
        Err(e) => tab.search.error = Some(e),
    }
    None
}

/// Applies a finished Replace All plan.
pub fn poll_replace(tab: &mut Tab) -> Option<ReplaceOutcome> {
    if tab.search.plan.as_ref()?.job.is_running() {
        return None;
    }
    let plan = tab.search.plan.take()?;
    if tab.search.plan_generation != tab.doc.generation() {
        return None;
    }
    let mut d = plan.data.lock();
    if let Some(e) = d.error.take().or_else(|| plan.job.error()) {
        return Some(ReplaceOutcome::Message(e));
    }
    if !d.done {
        return Some(ReplaceOutcome::Message(
            "Replace All was cancelled; nothing was changed.".into(),
        ));
    }
    if d.over {
        return Some(ReplaceOutcome::TooMany {
            matcher: plan.matcher.clone(),
            with: plan.with.clone(),
        });
    }
    let edits = std::mem::take(&mut d.edits);
    drop(d);
    if edits.is_empty() {
        return Some(ReplaceOutcome::Message("Nothing to replace.".into()));
    }
    let n = edits.len() as u64;
    tab.doc.set_changed_values(edits);
    // The hit list describes the old values.
    tab.search.reset();
    Some(ReplaceOutcome::Message(format!(
        "Replaced {} cells. Ctrl+Z undoes it.",
        group(n)
    )))
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
pub fn bar(ui: &mut egui::Ui, tab: &mut Tab, notify: &Notify, messages: &mut Vec<String>) {
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
            ui.toggle_value(&mut tab.search.replace, "Replace")
                .on_hover_text("Show the Replace row (Ctrl+H)");
        });
    });
    if tab.search.replace {
        replace_row(ui, tab, notify, messages);
    }
}

fn replace_row(ui: &mut egui::Ui, tab: &mut Tab, notify: &Notify, messages: &mut Vec<String>) {
    ui.horizontal(|ui| {
        ui.label("Replace");
        let hint = if tab.search.regex {
            "replacement ($1, ${name} for groups)"
        } else {
            "replacement"
        };
        let r = ui.add(
            egui::TextEdit::singleline(&mut tab.search.with)
                .desired_width(280.0)
                .hint_text(hint),
        );
        if tab.search.focus_with {
            r.request_focus();
            tab.search.focus_with = false;
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let busy = tab.search.plan.is_some();
        if ui
            .add_enabled(!busy, egui::Button::new("Replace"))
            .on_hover_text("Replace in the cursor's cell, then go to the next match")
            .clicked()
            || (enter && !busy)
        {
            messages.extend(replace_current(tab, notify));
            if enter {
                r.request_focus();
            }
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Replace all"))
            .on_hover_text(format!(
                "Up to {} cells become one undoable edit; more rewrite the file",
                group(replace::DEFAULT_THRESHOLD as u64)
            ))
            .clicked()
        {
            messages.extend(replace_all(tab, notify));
        }
        if let Some(p) = &tab.search.plan {
            let n = p.data.lock().edits.len();
            ui.add(
                egui::ProgressBar::new(p.job.fraction())
                    .desired_width(90.0)
                    .text(format!("{:.0}%", p.job.fraction() * 100.0)),
            );
            ui.label(format!("{} cells to replace so far", group(n as u64)));
            if ui.button("Cancel").clicked() {
                p.job.cancel();
            }
        } else if tab.search.regex {
            ui.weak("$1 or ${name} insert groups; $$ is a literal $");
        }
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
