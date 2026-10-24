//! The status bar.

use eframe::egui::{self, RichText};
use katak_core::document::DocStats;
use katak_core::sys::human_bytes;

use crate::grid::group;
use crate::tab::Tab;

fn sep(ui: &mut egui::Ui) {
    ui.separator();
}

/// One line of status for the active tab.
pub fn show(ui: &mut egui::Ui, tab: &Tab, stats: &DocStats, column_name: Option<&str>) {
    let d = tab.doc.dialect();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let hdr = tab.doc.header_records();
        if let Some(c) = tab.info.cursor {
            let n = c.record.saturating_sub(hdr) + 1;
            let row = if c.exact {
                format!("Row {}", group(n))
            } else {
                format!("Row ~{}", group(n))
            };
            ui.label(row);
        }
        let total = if stats.complete {
            group(stats.rows)
        } else {
            format!("~{}", group(stats.rows))
        };
        ui.label(format!("of {total}"));
        sep(ui);
        let col = tab.info.cursor_col;
        match column_name {
            Some(name) if !name.is_empty() => {
                ui.label(format!("Col {} \"{}\"", col + 1, ellipsize(name, 24)))
            }
            _ => ui.label(format!("Col {}", col + 1)),
        };
        ui.label(format!("of {}", group(tab.info.columns as u64)));
        sep(ui);
        ui.label(format!("Delimiter {}", d.delimiter.label()))
            .on_hover_text("Parsing options: Ctrl+Shift+P");
        ui.label(match d.quote {
            Some(q) => format!("Quote {q}"),
            None => "No quotes".into(),
        });
        ui.label(d.encoding.name());
        let le = if stats.line_endings.lf + stats.line_endings.crlf + stats.line_endings.cr > 0 {
            stats.line_endings.label()
        } else {
            tab.doc.detection().line_endings.label()
        };
        ui.label(le);
        ui.label(human_bytes(stats.file_len));
        if stats.ragged > 0 {
            sep(ui);
            ui.label(
                RichText::new(format!("Ragged rows: {}", group(stats.ragged)))
                    .color(egui::Color32::from_rgb(0xE0, 0x8A, 0x1E)),
            )
            .on_hover_text(
                "Rows whose field count differs from the first row. F8 jumps to the next one.",
            );
        }
        if !tab.doc.source().is_mapped() {
            sep(ui);
            ui.label("Buffered").on_hover_text(
                tab.doc
                    .source()
                    .buffered_reason()
                    .unwrap_or("not memory-mapped"),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if stats.indexing {
                let secs = stats.index_elapsed.as_secs_f64().max(1e-3);
                let rate = stats.index_bytes as f64 / secs / (1 << 20) as f64;
                ui.add(
                    egui::ProgressBar::new(stats.indexed_fraction)
                        .desired_width(120.0)
                        .text(format!("Indexing {:.0}%", stats.indexed_fraction * 100.0)),
                );
                ui.weak(format!("{rate:.0} MB/s"));
            } else if stats.complete {
                ui.weak(format!(
                    "Indexed in {:.2} s",
                    stats.index_elapsed.as_secs_f64()
                ));
            }
            if let Some(e) = &stats.error {
                ui.colored_label(egui::Color32::from_rgb(0xD0, 0x3B, 0x3B), e);
            }
        });
    });
}

fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}
