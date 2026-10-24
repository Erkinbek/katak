//! File → Export…: writes all rows, the rows in view (filter or sort
//! order) or the selection with another delimiter, quoting, encoding or
//! line ending.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui;
use katak_core::dialect::{Delimiter, Dialect, Escape};
use katak_core::encoding::{self, TextEncoding};
use katak_core::export::{ExportOptions, Scope};
use katak_core::job::Notify;
use katak_core::scan::Term;
use katak_core::write::QuotePolicy;

use crate::grid::group;
use crate::tab::Tab;

/// Which rows to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rows {
    All,
    /// The filter or sort shown.
    View,
    Selection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delim {
    Comma,
    Semicolon,
    Tab,
    Pipe,
    Other,
}

pub struct ExportDialog {
    pub rows: Rows,
    delim: Delim,
    other: String,
    quote: char,
    policy: QuotePolicy,
    escape: Escape,
    encoding: TextEncoding,
    bom: bool,
    line_ending: Term,
    header: bool,
    selected_columns: bool,
    open_after: bool,
    picker: Option<Receiver<Option<PathBuf>>>,
    error: Option<String>,
}

pub enum ExportAction {
    None,
    Close,
    /// The export job was started.
    Started,
}

fn unicode(e: TextEncoding) -> bool {
    matches!(
        e,
        TextEncoding::Utf8 | TextEncoding::Utf16Le | TextEncoding::Utf16Be
    )
}

impl ExportDialog {
    /// Defaults: the file's own format, and the rows in view when a
    /// filter or sort is shown.
    pub fn new(tab: &Tab) -> Self {
        let d = tab.doc.dialect();
        let (delim, other) = match &d.delimiter {
            Delimiter::Char(',') => (Delim::Comma, String::new()),
            Delimiter::Char(';') => (Delim::Semicolon, String::new()),
            Delimiter::Char('\t') | Delimiter::Whitespace => (Delim::Tab, String::new()),
            Delimiter::Char('|') => (Delim::Pipe, String::new()),
            Delimiter::Char(c) => (Delim::Other, c.to_string()),
            Delimiter::Str(s) => (Delim::Other, s.clone()),
        };
        let line_ending = tab
            .doc
            .stats()
            .line_endings
            .dominant()
            .or(tab.doc.detection().line_endings.dominant())
            .unwrap_or(Term::Lf);
        ExportDialog {
            rows: if tab.grid.is_list() {
                Rows::View
            } else {
                Rows::All
            },
            delim,
            other,
            quote: d.quote.unwrap_or('"'),
            policy: QuotePolicy::Minimal,
            escape: d.escape,
            encoding: d.encoding,
            bom: matches!(d.encoding, TextEncoding::Utf16Le | TextEncoding::Utf16Be),
            line_ending,
            header: d.has_header,
            selected_columns: true,
            open_after: false,
            picker: None,
            error: None,
        }
    }

    fn delimiter(&self) -> Result<Delimiter, String> {
        Ok(match self.delim {
            Delim::Comma => Delimiter::Char(','),
            Delim::Semicolon => Delimiter::Char(';'),
            Delim::Tab => Delimiter::Char('\t'),
            Delim::Pipe => Delimiter::Char('|'),
            Delim::Other => {
                let mut chars = self.other.chars();
                match (chars.next(), chars.next()) {
                    (None, _) => return Err("Enter a delimiter.".into()),
                    (Some(c), None) => Delimiter::Char(c),
                    _ => Delimiter::Str(self.other.clone()),
                }
            }
        })
    }

    fn options(&self, tab: &Tab) -> Result<ExportOptions, String> {
        let delimiter = self.delimiter()?;
        if delimiter.write_str().contains(self.quote) {
            return Err("The delimiter cannot contain the quote character.".into());
        }
        let sel = tab.grid.selection();
        Ok(ExportOptions {
            dialect: Dialect {
                delimiter,
                quote: Some(self.quote),
                escape: self.escape,
                encoding: self.encoding,
                ..Dialect::default()
            },
            policy: self.policy,
            line_ending: self.line_ending,
            bom: self.bom && unicode(self.encoding),
            columns: (self.rows == Rows::Selection && self.selected_columns).then_some(sel.cols),
        })
    }

    fn scope(&self, tab: &Tab) -> Result<Scope, String> {
        let sel = tab.grid.selection();
        Ok(match (self.rows, tab.grid.list()) {
            (Rows::All, _) => Scope::All,
            (Rows::View, Some(l)) => {
                let partial = !l.lock().done;
                if partial {
                    return Err("The filter is still running; export when it has finished.".into());
                }
                Scope::List(l)
            }
            (Rows::View, None) => return Err("No filter or sort is shown.".into()),
            (Rows::Selection, Some(l)) => {
                let a = sel.first as usize;
                Scope::List(l.lock().part(a, a + sel.rows as usize))
            }
            (Rows::Selection, None) => Scope::Logical {
                start: sel.start,
                count: sel.rows,
            },
        })
    }

    fn default_name(tab: &Tab, delim: Delim) -> String {
        let stem = Path::new(&tab.title)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        let ext = if delim == Delim::Tab { "tsv" } else { "csv" };
        format!("{stem}-export.{ext}")
    }

    fn pick(&mut self, tab: &Tab, notify: &Notify) -> Option<PathBuf> {
        let dialog = rfd::FileDialog::new()
            .set_title("Export to")
            .set_file_name(Self::default_name(tab, self.delim));
        let dialog = match tab.path.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        if cfg!(target_os = "macos") {
            // macOS dialogs must run on the main thread.
            return dialog.save_file();
        }
        let (tx, rx) = channel();
        let n = notify.clone();
        std::thread::spawn(move || {
            let _ = tx.send(dialog.save_file());
            n();
        });
        self.picker = Some(rx);
        None
    }

    fn start(&mut self, tab: &mut Tab, path: PathBuf, open: &[PathBuf], notify: &Notify) -> bool {
        if open.iter().any(|p| same_file(p, &path)) {
            self.error = Some(
                "That file is open in Katak. Choose another name (Save writes edits to the open file)."
                    .into(),
            );
            return false;
        }
        let res = self
            .options(tab)
            .and_then(|o| self.scope(tab).map(|s| (s, o)));
        match res {
            Ok((scope, opts)) => {
                tab.start_export(
                    scope,
                    opts,
                    self.header,
                    path,
                    self.open_after,
                    notify.clone(),
                );
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        tab: &mut Tab,
        open: &[PathBuf],
        notify: &Notify,
    ) -> ExportAction {
        if let Some(rx) = &self.picker
            && let Ok(choice) = rx.try_recv()
        {
            self.picker = None;
            if let Some(path) = choice
                && self.start(tab, path, open, notify)
            {
                return ExportAction::Started;
            }
        }
        let waiting = self.picker.is_some();
        let has_header = tab.doc.dialect().has_header;
        let stats = tab.doc.stats();
        let total = if stats.complete {
            group(stats.rows)
        } else {
            format!("~{}", group(stats.rows))
        };
        let view = tab.grid.list().map(|l| {
            let d = l.lock();
            (d.rows.len() as u64, d.sorted)
        });
        let sel = tab.grid.selection();
        let filtered = tab.filter.run.is_some();
        let mut action = ExportAction::None;
        let mut open_window = true;
        egui::Window::new("Export")
            .open(&mut open_window)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!waiting, |ui| {
                    ui.strong("Rows");
                    ui.radio_value(&mut self.rows, Rows::All, format!("All rows ({total})"));
                    if let Some((n, sorted)) = view {
                        let what = match (filtered, sorted) {
                            (true, true) => "Filtered rows, in sort order",
                            (false, true) => "All rows, in sort order",
                            _ => "Filtered rows",
                        };
                        ui.radio_value(
                            &mut self.rows,
                            Rows::View,
                            format!("{what} ({})", group(n)),
                        );
                    }
                    ui.radio_value(
                        &mut self.rows,
                        Rows::Selection,
                        format!(
                            "Selection ({} × {})",
                            count(sel.rows, "row"),
                            count((sel.cols.1 - sel.cols.0 + 1) as u64, "column")
                        ),
                    );
                    if self.rows == Rows::Selection {
                        ui.checkbox(&mut self.selected_columns, "Only the selected columns");
                    }
                    if view.is_none() && self.rows == Rows::View {
                        self.rows = Rows::All;
                    }
                    ui.add_space(6.0);
                    ui.strong("Format");
                    egui::Grid::new("export-format")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Delimiter");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.delim, Delim::Comma, ",");
                                ui.radio_value(&mut self.delim, Delim::Semicolon, ";");
                                ui.radio_value(&mut self.delim, Delim::Tab, "Tab");
                                ui.radio_value(&mut self.delim, Delim::Pipe, "|");
                                ui.radio_value(&mut self.delim, Delim::Other, "Other:");
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut self.other).desired_width(40.0),
                                );
                                if r.changed() {
                                    self.delim = Delim::Other;
                                }
                            });
                            ui.end_row();
                            ui.label("Quote");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.quote, '"', "\"");
                                ui.radio_value(&mut self.quote, '\'', "'");
                                ui.separator();
                                ui.radio_value(
                                    &mut self.policy,
                                    QuotePolicy::Minimal,
                                    "only when needed",
                                );
                                ui.radio_value(&mut self.policy, QuotePolicy::Always, "always");
                            });
                            ui.end_row();
                            ui.label("Quotes inside values");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.escape, Escape::Doubled, "doubled (\"\")");
                                ui.radio_value(
                                    &mut self.escape,
                                    Escape::Backslash,
                                    "backslash (\\\")",
                                );
                            });
                            ui.end_row();
                            ui.label("Encoding");
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("export-encoding")
                                    .selected_text(self.encoding.name())
                                    .show_ui(ui, |ui| {
                                        for e in encoding::CHOICES {
                                            ui.selectable_value(&mut self.encoding, *e, e.name());
                                        }
                                    });
                                ui.add_enabled_ui(unicode(self.encoding), |ui| {
                                    ui.checkbox(&mut self.bom, "byte-order mark");
                                });
                            });
                            ui.end_row();
                            ui.label("Line endings");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut self.line_ending, Term::Lf, "LF");
                                ui.radio_value(&mut self.line_ending, Term::CrLf, "CRLF (Windows)");
                                ui.radio_value(&mut self.line_ending, Term::Cr, "CR");
                            });
                            ui.end_row();
                        });
                    ui.add_space(6.0);
                    if has_header {
                        ui.checkbox(&mut self.header, "Write the header row");
                    }
                    ui.checkbox(&mut self.open_after, "Open the exported file");
                    if tab.dirty() {
                        ui.weak("Unsaved edits are included.");
                    }
                    if let Some(e) = &self.error {
                        ui.colored_label(egui::Color32::from_rgb(0xD0, 0x3B, 0x3B), e);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let busy = tab.export.is_some();
                        if ui
                            .add_enabled(!busy, egui::Button::new("Export…"))
                            .on_disabled_hover_text("Another export is running")
                            .clicked()
                        {
                            self.error = None;
                            match self.options(tab).and_then(|_| self.scope(tab).map(|_| ())) {
                                Err(e) => self.error = Some(e),
                                Ok(()) => {
                                    if let Some(path) = self.pick(tab, notify)
                                        && self.start(tab, path, open, notify)
                                    {
                                        action = ExportAction::Started;
                                    }
                                }
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            action = ExportAction::Close;
                        }
                    });
                });
                if waiting {
                    ui.weak("Choose where to save the export…");
                }
            });
        if !open_window {
            return ExportAction::Close;
        }
        action
    }
}

/// "1 row", "2 rows", "12,000 rows".
fn count(n: u64, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{} {what}s", group(n))
    }
}

/// Same path, ignoring case on Windows and resolving `.`/`..` where the
/// file exists.
fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (canon(a), canon(b));
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}
