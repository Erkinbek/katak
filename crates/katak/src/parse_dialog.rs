//! The parsing-options dialog with a live preview of the first rows.

use std::time::Instant;

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Color32, RichText, ScrollArea, TextFormat};
use katak_core::detect::Detection;
use katak_core::dialect::{Delimiter, Dialect, Escape};
use katak_core::document::preview_rows;
use katak_core::encoding::{self, TextEncoding};

const PREVIEW_ROWS: usize = 50;
const PREVIEW_COLS: usize = 40;
/// Sample bytes and characters per line shown as raw text.
const RAW_BYTES: usize = 64 << 10;
const RAW_LINE_CHARS: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DelimChoice {
    Comma,
    Semicolon,
    Tab,
    Space,
    Colon,
    Pipe,
    Whitespace,
    Lines,
    Custom,
}

impl DelimChoice {
    const ALL: [(DelimChoice, &'static str); 9] = [
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
        (DelimChoice::Lines, "None: one value per line (raw text)"),
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
            Delimiter::Lines => (DelimChoice::Lines, String::new()),
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
    /// Show the sample's characters instead of the parsed table.
    pub raw_preview: bool,
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
            raw_preview: false,
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
            DelimChoice::Lines => Delimiter::Lines,
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
                            ui.selectable_value(&mut self.raw_preview, false, "Table");
                            ui.selectable_value(&mut self.raw_preview, true, "Raw text")
                                .on_hover_text(
                                    "The file's characters as they are: tabs, line breaks and other control characters are shown as escapes, the delimiter and quote are colored",
                                );
                            ui.weak(format!(
                                "first {PREVIEW_ROWS} rows, parsed in {:.2} ms",
                                self.preview_ms
                            ));
                        });
                        if self.raw_preview {
                            self.raw_text(ui, sample);
                        } else {
                            self.preview_table(ui);
                        }
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
        let before = self.choice;
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
        if self.choice == DelimChoice::Lines && before != DelimChoice::Lines {
            // Raw text: whole lines, nothing special.
            self.draft.quote = None;
            self.draft.has_header = false;
            self.draft.collapse_delimiters = false;
            self.draft.trim = false;
        }
        ui.add_enabled(
            !matches!(self.choice, DelimChoice::Whitespace | DelimChoice::Lines),
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

    /// The first lines of the sample as text, with control characters
    /// escaped and the delimiter and quote colored.
    fn raw_text(&self, ui: &mut egui::Ui, sample: &[u8]) {
        let enc = self.draft.encoding;
        let mut n = sample.len().min(RAW_BYTES);
        if matches!(enc, TextEncoding::Utf16Le | TextEncoding::Utf16Be) {
            n &= !1;
        }
        let text = enc.decode(&sample[..n]);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let delim = match &self.draft.delimiter {
            Delimiter::Char(c) => Some(*c),
            _ => None,
        };
        let quote = self.draft.quote;
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let plain = ui.visuals().text_color();
        let escape = ui.visuals().weak_text_color();
        let (delim_color, quote_color) = if ui.visuals().dark_mode {
            (
                Color32::from_rgb(0xF0, 0xB4, 0x4C),
                Color32::from_rgb(0x7F, 0xB8, 0xF0),
            )
        } else {
            (
                Color32::from_rgb(0xB0, 0x60, 0x00),
                Color32::from_rgb(0x1F, 0x5F, 0xB0),
            )
        };
        ScrollArea::both()
            .id_salt("raw-preview")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut lines = text.split_inclusive('\n').take(PREVIEW_ROWS);
                for (i, line) in lines.by_ref().enumerate() {
                    let mut job = LayoutJob::default();
                    let mut put = |s: &str, color: Color32| {
                        job.append(s, 0.0, TextFormat::simple(font.clone(), color));
                    };
                    put(&format!("{:>4}  ", i + 1), escape);
                    for (k, c) in line.chars().enumerate() {
                        if k == RAW_LINE_CHARS {
                            put(" …", escape);
                            break;
                        }
                        match c {
                            '\t' => put("\\t", escape),
                            '\r' => put("\\r", escape),
                            '\n' => put("\\n", escape),
                            c if c.is_control() => put(&format!("\\x{:02X}", c as u32), escape),
                            '\u{feff}' => put("<BOM>", escape),
                            c if Some(c) == delim => put(c.encode_utf8(&mut [0; 4]), delim_color),
                            c if Some(c) == quote => put(c.encode_utf8(&mut [0; 4]), quote_color),
                            c => put(c.encode_utf8(&mut [0; 4]), plain),
                        }
                    }
                    ui.label(job);
                }
            });
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
