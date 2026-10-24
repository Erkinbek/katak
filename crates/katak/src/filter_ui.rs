//! The filter bar: per-column quick filters combined with AND.

use eframe::egui::{self, Key};
use katak_core::filter::{self, ColumnFilter, FilterOp, FilterRun};
use katak_core::job::Notify;

use crate::grid::{View, group};
use crate::tab::Tab;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpKind {
    Contains,
    Equals,
    StartsWith,
    Regex,
    Empty,
    NotEmpty,
    Range,
}

impl OpKind {
    const ALL: [OpKind; 7] = [
        OpKind::Contains,
        OpKind::Equals,
        OpKind::StartsWith,
        OpKind::Regex,
        OpKind::Empty,
        OpKind::NotEmpty,
        OpKind::Range,
    ];

    fn label(self) -> &'static str {
        match self {
            OpKind::Contains => "contains",
            OpKind::Equals => "equals",
            OpKind::StartsWith => "starts with",
            OpKind::Regex => "matches regex",
            OpKind::Empty => "is empty",
            OpKind::NotEmpty => "is not empty",
            OpKind::Range => "number between",
        }
    }
}

/// One editable filter row.
#[derive(Clone, Debug)]
struct Line {
    col: usize,
    op: OpKind,
    value: String,
    min: String,
    max: String,
    case_sensitive: bool,
    focus: bool,
}

impl Line {
    fn new(col: usize) -> Self {
        Line {
            col,
            op: OpKind::Contains,
            value: String::new(),
            min: String::new(),
            max: String::new(),
            case_sensitive: false,
            focus: true,
        }
    }

    fn to_filter(&self) -> Result<ColumnFilter, String> {
        let num = |s: &str| -> Result<Option<f64>, String> {
            let t = s.trim();
            if t.is_empty() {
                Ok(None)
            } else {
                katak_core::infer::parse_number(t)
                    .map(Some)
                    .ok_or_else(|| format!("\"{t}\" is not a number"))
            }
        };
        let op = match self.op {
            OpKind::Contains => FilterOp::Contains(self.value.clone()),
            OpKind::Equals => FilterOp::Equals(self.value.clone()),
            OpKind::StartsWith => FilterOp::StartsWith(self.value.clone()),
            OpKind::Regex => FilterOp::Regex(self.value.clone()),
            OpKind::Empty => FilterOp::Empty,
            OpKind::NotEmpty => FilterOp::NotEmpty,
            OpKind::Range => FilterOp::Range(num(&self.min)?, num(&self.max)?),
        };
        Ok(ColumnFilter {
            col: self.col,
            op,
            case_sensitive: self.case_sensitive,
        })
    }
}

/// Filter bar state of one tab.
#[derive(Default)]
pub struct FilterBar {
    pub open: bool,
    lines: Vec<Line>,
    pub run: Option<FilterRun>,
    generation: u64,
    pub error: Option<String>,
}

impl FilterBar {
    /// Opens the bar with a filter line for column `col`.
    pub fn add_column(&mut self, col: usize) {
        self.open = true;
        if let Some(l) = self.lines.iter_mut().find(|l| l.col == col) {
            l.focus = true;
        } else {
            self.lines.push(Line::new(col));
        }
    }

    pub fn toggle(&mut self, cursor_col: usize) {
        if self.open {
            self.open = false;
        } else {
            self.open = true;
            if self.lines.is_empty() {
                self.lines.push(Line::new(cursor_col));
            }
        }
    }

    /// Status for the status bar: (matched, capped, running).
    pub fn status(&self) -> Option<(u64, bool, bool)> {
        let run = self.run.as_ref()?;
        let d = run.data.lock();
        Some((d.matched, d.capped, run.job.is_running()))
    }
}

/// Applies the filter lines (or clears the filter when there are none).
fn apply(tab: &mut Tab, notify: &Notify) {
    let filters: Result<Vec<ColumnFilter>, String> =
        tab.filter.lines.iter().map(Line::to_filter).collect();
    let filters = match filters {
        Ok(f) => f,
        Err(e) => {
            tab.filter.error = Some(e);
            return;
        }
    };
    if filters.is_empty() {
        clear(tab);
        return;
    }
    match filter::start(
        tab.doc.target(),
        filters,
        filter::DEFAULT_CAP,
        notify.clone(),
    ) {
        Ok(run) => {
            tab.grid.set_view(View::List(run.data.clone()));
            tab.filter.run = Some(run);
            tab.filter.generation = tab.doc.generation();
            tab.filter.error = None;
        }
        Err(e) => tab.filter.error = Some(e),
    }
}

/// Opens the bar with "column contains text" and applies it.
pub fn apply_quick(tab: &mut Tab, notify: &Notify, col: usize, text: &str) {
    tab.filter.open = true;
    tab.filter.lines = vec![Line {
        value: text.to_string(),
        focus: false,
        ..Line::new(col)
    }];
    apply(tab, notify);
}

/// Removes the filter and shows every row again.
pub fn clear(tab: &mut Tab) {
    tab.filter.run = None;
    tab.filter.error = None;
    tab.grid.set_view(View::Natural);
}

/// Per-frame housekeeping: a dialect change invalidates the filter.
pub fn tick(tab: &mut Tab) {
    if tab.filter.run.is_some() && tab.filter.generation != tab.doc.generation() {
        clear(tab);
    }
}

/// The filter bar.
pub fn bar(ui: &mut egui::Ui, tab: &mut Tab, notify: &Notify, columns: usize) {
    let header = tab.doc.header_record().ok().flatten();
    let name_of = |tab: &mut Tab, c: usize| -> String {
        header
            .and_then(|h| tab.doc.cells(&h, c..c + 1, 40).ok())
            .and_then(|v| v.into_iter().next())
            .filter(|v| !v.missing && !v.text.is_empty())
            .map(|v| format!("{} · {}", c + 1, v.text))
            .unwrap_or_else(|| format!("Column {}", c + 1))
    };
    let mut remove = None;
    let mut submit = false;
    let ncols = columns.max(1);
    for i in 0..tab.filter.lines.len() {
        let col_name = name_of(tab, tab.filter.lines[i].col);
        let names: Vec<String> = if ncols <= 300 {
            (0..ncols).map(|c| name_of(tab, c)).collect()
        } else {
            Vec::new()
        };
        let line = &mut tab.filter.lines[i];
        ui.horizontal(|ui| {
            ui.label(if i == 0 { "Filter" } else { "and" });
            if names.is_empty() {
                let mut c = line.col + 1;
                ui.add(
                    egui::DragValue::new(&mut c)
                        .range(1..=ncols)
                        .prefix("column "),
                );
                line.col = c - 1;
                ui.weak(&col_name);
            } else {
                egui::ComboBox::from_id_salt(("filter-col", i))
                    .selected_text(&col_name)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (c, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut line.col, c, n);
                        }
                    });
            }
            egui::ComboBox::from_id_salt(("filter-op", i))
                .selected_text(line.op.label())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for op in OpKind::ALL {
                        ui.selectable_value(&mut line.op, op, op.label());
                    }
                });
            match line.op {
                OpKind::Empty | OpKind::NotEmpty => {}
                OpKind::Range => {
                    let a = ui.add(
                        egui::TextEdit::singleline(&mut line.min)
                            .desired_width(80.0)
                            .hint_text("min"),
                    );
                    ui.label("…");
                    let b = ui.add(
                        egui::TextEdit::singleline(&mut line.max)
                            .desired_width(80.0)
                            .hint_text("max"),
                    );
                    if line.focus {
                        a.request_focus();
                        line.focus = false;
                    }
                    submit |= (a.lost_focus() || b.lost_focus())
                        && ui.input(|i| i.key_pressed(Key::Enter));
                }
                _ => {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut line.value)
                            .desired_width(200.0)
                            .hint_text("value"),
                    );
                    if line.focus {
                        r.request_focus();
                        line.focus = false;
                    }
                    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.toggle_value(&mut line.case_sensitive, "Aa")
                        .on_hover_text("Match case");
                }
            }
            if ui
                .small_button("×")
                .on_hover_text("Remove this filter")
                .clicked()
            {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        tab.filter.lines.remove(i);
        submit = true;
    }
    ui.horizontal(|ui| {
        if ui.button("+ Add filter").clicked() {
            tab.filter.lines.push(Line::new(tab.info.cursor_col));
        }
        if ui.button("Apply").clicked() {
            submit = true;
        }
        if ui.button("Clear").clicked() {
            tab.filter.lines.clear();
            clear(tab);
        }
        if let Some(e) = &tab.filter.error {
            ui.colored_label(egui::Color32::from_rgb(0xD0, 0x3B, 0x3B), e);
        } else if let Some(run) = &tab.filter.run {
            let (matched, capped, scanned) = {
                let d = run.data.lock();
                (d.matched, d.capped, d.records_scanned)
            };
            let total = tab.doc.stats().rows;
            if run.job.is_running() {
                ui.add(
                    egui::ProgressBar::new(run.job.fraction())
                        .desired_width(90.0)
                        .text(format!("{:.0}%", run.job.fraction() * 100.0)),
                );
                ui.label(format!(
                    "{} of {} rows scanned match",
                    group(matched),
                    group(scanned)
                ));
            } else {
                ui.label(format!("{} of {} rows match", group(matched), group(total)));
            }
            if capped {
                ui.weak(format!(
                    "(showing the first {})",
                    group(filter::DEFAULT_CAP as u64)
                ));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("×")
                .on_hover_text("Hide the filter bar (the filter stays active)")
                .clicked()
            {
                tab.filter.open = false;
            }
        });
    });
    if submit {
        apply(tab, notify);
    }
}
