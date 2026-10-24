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
        if tab.info.cursor.is_some() {
            let n = tab.info.cursor_row + 1;
            let row = if tab.info.cursor_exact {
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
        if let Some((matched, capped, running)) = tab.filter.status() {
            sep(ui);
            let more = if capped { "+" } else { "" };
            let text = format!(
                "Filtered: {}{more} of {total} rows{}",
                group(matched),
                if running { " (scanning)" } else { "" }
            );
            ui.label(RichText::new(text).strong());
        }
        if let Some(s) = &tab.sort {
            sep(ui);
            if s.run.job.is_running() {
                ui.add(
                    egui::ProgressBar::new(s.run.job.fraction())
                        .desired_width(110.0)
                        .text(format!("Sorting {:.0}%", s.run.job.fraction() * 100.0)),
                );
            } else {
                ui.label(RichText::new(s.describe()).strong())
                    .on_hover_text("Right-click a column header to change or clear the sort. Export writes the rows in this order.");
            }
        } else if tab.sort_pending.is_some() {
            sep(ui);
            ui.label("Sorting after the filter finishes…");
        }
        sep(ui);
        let col = tab.info.cursor_col;
        match column_name {
            Some(name) if !name.is_empty() => {
                ui.label(format!("Col {} \"{}\"", col + 1, ellipsize(name, 24)))
            }
            _ => ui.label(format!("Col {}", col + 1)),
        };
        ui.label(format!("of {}", group(tab.info.columns as u64)));
        if let Some(s) = &tab.info.selection {
            sep(ui);
            ui.label(selection_text(s)).on_hover_text(
                "Selection: non-empty cells, and sum / average / min / max of the numbers in it",
            );
        }
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
        if tab.dirty() {
            sep(ui);
            let ov = &tab.doc.overlay;
            let mut parts = Vec::new();
            if ov.edited_cells() > 0 {
                parts.push(format!("{} cells edited", group(ov.edited_cells() as u64)));
            }
            if ov.inserted_rows() > 0 {
                parts.push(format!("+{} rows", group(ov.inserted_rows() as u64)));
            }
            if ov.deleted_records() > 0 {
                parts.push(format!("−{} rows", group(ov.deleted_records())));
            }
            if ov.columns().is_some() {
                parts.push("columns changed".into());
            }
            ui.label(
                RichText::new(format!("Unsaved: {}", parts.join(", ")))
                    .color(egui::Color32::from_rgb(0x3C, 0xC4, 0x7A)),
            )
            .on_hover_text("Ctrl+S saves; Edit → Discard all edits drops them.");
        }
        if tab.grid.read_only {
            sep(ui);
            ui.label("Read-only");
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
            for s in [&tab.save, &tab.export].into_iter().flatten() {
                ui.add(
                    egui::ProgressBar::new(s.job.fraction())
                        .desired_width(120.0)
                        .text(format!("{} {:.0}%", s.kind.verb(), s.job.fraction() * 100.0)),
                );
            }
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

fn selection_text(s: &crate::grid::SelStats) -> String {
    let size = format!("{} × {}", group(s.rows), group(s.cols));
    let Some(filled) = s.filled else {
        return format!("Selected {size} (too large to summarize)");
    };
    let mut t = format!("Selected {size}: {} filled", group(filled));
    if s.numbers > 0 {
        t.push_str(&format!(
            " · Sum {} · Avg {} · Min {} · Max {}",
            number(s.sum),
            number(s.sum / s.numbers as f64),
            number(s.min),
            number(s.max)
        ));
    }
    t
}

/// Up to 4 decimals, thousands grouped, no trailing zeros.
fn number(x: f64) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    let s = format!("{:.4}", x.abs());
    let s = s.trim_end_matches('0').trim_end_matches('.');
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = int
        .parse::<u64>()
        .map(group)
        .unwrap_or_else(|_| int.to_string());
    let sign = if x < 0.0 && s != "0" { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
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
