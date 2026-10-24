//! The parsing-options dialog with a live preview of the first rows.

use std::time::Instant;

use eframe::egui::{self, RichText, ScrollArea};
use katak_core::detect::Detection;
use katak_core::dialect::{Delimiter, Dialect, Escape};
use katak_core::document::preview_rows;
use katak_core::encoding::{self, TextEncoding};

const PREVIEW_ROWS: usize = 50;
const PREVIEW_COLS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DelimChoice {
    Comma,
    Semicolon,
    Tab,
    Space,
    Colon,
    Pipe,
    Whitespace,
    Custom,
}

impl DelimChoice {
    const ALL: [(DelimChoice, &'static str); 8] = [
        (DelimChoice::Comma, "Comma ,"),
        (DelimChoice::Semicolon, "Semicolon ;"),
        (DelimChoice::Tab, "Tab"),
        (DelimChoice::Space, "Space"),
        (DelimChoice::Colon, "Colon :"),
        (DelimChoice::Pipe, "Pipe |"),
        (
            DelimChoice::Whitespace,
            "Whitespace (any run of spaces/tabs)",
        ),
        (DelimChoice::Custom, "Custom:"),
    ];

    fn of(d: &Delimiter) -> (DelimChoice, String) {
        match d {
            Delimiter::Char(',') => (DelimChoice::Comma, String::new()),
            Delimiter::Char(';') => (DelimChoice::Semicolon, String::new()),
            Delimiter::Char('\t') => (DelimChoice::Tab, String::new()),
            Delimiter::Char(' ') => (DelimChoice::Space, String::new()),
            Delimiter::Char(':') => (DelimChoice::Colon, String::new()),
            Delimiter::Char('|') => (DelimChoice::Pipe, String::new()),
            Delimiter::Whitespace => (DelimChoice::Whitespace, String::new()),
            Delimiter::Char(c) => (DelimChoice::Custom, c.to_string()),
            Delimiter::Str(s) => (DelimChoice::Custom, s.clone()),
        }
    }
}

/// What the dialog wants done.
pub enum ParseAction {
    None,
    Apply(Dialect),
    Close,
}

pub struct ParseDialog {
    draft: Dialect,
    choice: DelimChoice,
    custom: String,
    comment_on: bool,
    comment: String,
    preview_for: Option<Dialect>,
    preview: Result<Vec<Vec<String>>, String>,
    pub preview_ms: f64,
}

impl ParseDialog {
    pub fn new(current: &Dialect) -> Self {
        let (choice, custom) = DelimChoice::of(&current.delimiter);
        ParseDialog {
            draft: current.clone(),
            choice,
            custom,
            comment_on: current.comment_prefix.is_some(),
            comment: current.comment_prefix.clone().unwrap_or_else(|| "#".into()),
            preview_for: None,
            preview: Ok(Vec::new()),
            preview_ms: 0.0,
        }
    }

    fn sync_draft(&mut self) {
        self.draft.delimiter = match self.choice {
            DelimChoice::Comma => Delimiter::Char(','),
            DelimChoice::Semicolon => Delimiter::Char(';'),
            DelimChoice::Tab => Delimiter::Char('\t'),
            DelimChoice::Space => Delimiter::Char(' '),
            DelimChoice::Colon => Delimiter::Char(':'),
            DelimChoice::Pipe => Delimiter::Char('|'),
            DelimChoice::Whitespace => Delimiter::Whitespace,
            DelimChoice::Custom => {
                let s = self.custom.replace("\\t", "\t");
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Delimiter::Char(c),
                    _ => Delimiter::Str(s),
                }
            }
        };
        self.draft.comment_prefix =
            (self.comment_on && !self.comment.is_empty()).then(|| self.comment.clone());
    }

    fn refresh_preview(&mut self, sample: &[u8], whole: bool) {
        if self.preview_for.as_ref() == Some(&self.draft) {
            return;
        }
        let t = Instant::now();
        self.preview =
            preview_rows(sample, whole, &self.draft, PREVIEW_ROWS, 80).map_err(|e| e.to_string());
        self.preview_ms = t.elapsed().as_secs_f64() * 1000.0;
        self.preview_for = Some(self.draft.clone());
    }

    /// Shows the dialog; `open` is cleared when it is closed.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        sample: &[u8],
        whole: bool,
        detection: &Detection,
        has_edits: bool,
    ) -> ParseAction {
        let mut action = ParseAction::None;
        let mut open = true;
        let screen = ctx.content_rect();
        egui::Window::new("Parsing options")
            .open(&mut open)
            .default_size([900.0, 560.0])
            .max_height(screen.height() - 60.0)
            .max_width(screen.width() - 40.0)
            .default_pos(screen.center() - egui::vec2(450.0, 280.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                // Buttons first, pinned to the bottom, so they never scroll away.
                egui::Panel::bottom("parse-buttons").show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let valid = self.preview.is_ok();
                        if ui
                            .add_enabled(valid, egui::Button::new(RichText::new("Apply").strong()))
                            .on_hover_text("Re-parse the file with these options")
                            .clicked()
                        {
                            action = ParseAction::Apply(self.draft.clone());
                        }
                        if ui.button("Reset to detected").clicked() {
                            *self = ParseDialog::new(&detection.dialect);
                        }
                        if ui.button("Cancel").clicked() {
                            action = ParseAction::Close;
                        }
                        if has_edits {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xE0, 0x8A, 0x1E),
                                "Applying discards unsaved edits.",
                            );
                        }
                    });
                });
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(290.0);
                        ScrollArea::vertical()
                            .id_salt("parse-options")
                            .show(ui, |ui| self.options(ui, detection));
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        self.sync_draft();
                        self.refresh_preview(sample, whole);
                        ui.horizontal(|ui| {
                            ui.strong("Preview");
                            ui.weak(format!(
                                "first {PREVIEW_ROWS} rows, parsed in {:.2} ms",
                                self.preview_ms
                            ));
                        });
                        self.preview_table(ui);
                    });
                });
            });
        if !open {
            action = ParseAction::Close;
        }
        action
    }

    fn options(&mut self, ui: &mut egui::Ui, detection: &Detection) {
        ui.strong("Delimiter");
        for (choice, label) in DelimChoice::ALL {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.choice, choice, label);
                if choice == DelimChoice::Custom {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.custom)
                            .desired_width(80.0)
                            .hint_text("e.g. || or \\t"),
                    );
                    if r.changed() {
                        self.choice = DelimChoice::Custom;
                    }
                }
            });
        }
        ui.add_enabled(
            self.choice != DelimChoice::Whitespace,
            egui::Checkbox::new(
                &mut self.draft.collapse_delimiters,
                "Treat consecutive delimiters as one",
            ),
        );
        ui.add_space(6.0);
        ui.strong("Quoting");
        ui.horizontal(|ui| {
            ui.label("Quote:");
            ui.radio_value(&mut self.draft.quote, Some('"'), "\"");
            ui.radio_value(&mut self.draft.quote, Some('\''), "'");
            ui.radio_value(&mut self.draft.quote, None, "none");
        });
        ui.horizontal(|ui| {
            ui.label("Escape:");
            ui.radio_value(&mut self.draft.escape, Escape::Doubled, "\"\" (doubled)");
            ui.radio_value(
                &mut self.draft.escape,
                Escape::Backslash,
                "\\\" (backslash)",
            );
        });
        ui.add_space(6.0);
        ui.strong("Rows");
        ui.checkbox(&mut self.draft.has_header, "First row is a header");
        ui.horizontal(|ui| {
            ui.label("Skip first");
            ui.add(egui::DragValue::new(&mut self.draft.skip_lines).range(0..=10_000));
            ui.label("lines");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.comment_on, "Comment lines start with");
            ui.add_enabled(
                self.comment_on,
                egui::TextEdit::singleline(&mut self.comment).desired_width(40.0),
            );
        });
        ui.checkbox(&mut self.draft.trim, "Trim whitespace around fields");
        ui.add_space(6.0);
        ui.strong("Encoding");
        egui::ComboBox::from_id_salt("encoding")
            .selected_text(self.draft.encoding.name())
            .show_ui(ui, |ui| {
                for e in encoding::CHOICES {
                    ui.selectable_value(&mut self.draft.encoding, *e, e.name());
                }
            });
        let detected: TextEncoding = detection.encoding.encoding;
        ui.weak(format!(
            "Detected: {} ({:?})",
            detected.name(),
            detection.encoding.evidence
        ));
        ui.weak(format!(
            "Line endings in sample: {}",
            detection.line_endings.label()
        ));
        if detection.sep_line {
            ui.weak("An Excel \"sep=\" line is skipped.");
        }
    }

    fn preview_table(&self, ui: &mut egui::Ui) {
        match &self.preview {
            Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(0xD0, 0x3B, 0x3B), e);
            }
            Ok(rows) => {
                let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
                let shown = ncols.min(PREVIEW_COLS);
                ui.weak(format!(
                    "{} columns{}",
                    ncols,
                    if ncols > shown {
                        format!(" (showing the first {shown})")
                    } else {
                        String::new()
                    }
                ));
                ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("preview")
                            .striped(true)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                ui.weak("#");
                                for c in 0..shown {
                                    if self.draft.has_header {
                                        let name = rows
                                            .first()
                                            .and_then(|r| r.get(c))
                                            .cloned()
                                            .unwrap_or_default();
                                        ui.strong(name);
                                    } else {
                                        ui.strong(format!("Column {}", c + 1));
                                    }
                                }
                                ui.end_row();
                                let data = if self.draft.has_header && !rows.is_empty() {
                                    &rows[1..]
                                } else {
                                    &rows[..]
                                };
                                for (i, r) in data.iter().enumerate() {
                                    let ragged = r.len() != ncols;
                                    let num = RichText::new((i + 1).to_string()).weak();
                                    ui.label(if ragged {
                                        num.color(egui::Color32::from_rgb(0xE0, 0x8A, 0x1E))
                                    } else {
                                        num
                                    });
                                    for c in 0..shown {
                                        let v = r.get(c).map(String::as_str).unwrap_or("");
                                        ui.label(v);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
    }
}
