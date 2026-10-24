//! Katak — a fast, non-freezing viewer/editor for huge delimited text files.
//!
//! ```text
//! katak [FILE...] [--perf LOG] [--self-test] [--screenshot PNG [--show DIALOG]] [--buffered]
//! ```
//!
//! `--perf LOG` appends timing measurements (startup, first paint, frame
//! times) to LOG. `--self-test` additionally scrolls the first file
//! programmatically for a few seconds and exits; it is how the frame-time
//! targets are measured. `--screenshot PNG` saves the window contents
//! once the first file is shown (optionally with a dialog open or an
//! action run: `parse`, `parse-raw`, `goto`, `shortcuts`, `edit`, `export`, `exportto=PATH`, `selectall`,
//! `search=TEXT`, `filter=COL:TEXT`, `replace=FIND:WITH`, `replaceall=FIND:WITH`,
//! `sort=COL:asc|desc`) and exits. It is taken 800 ms after the first
//! paint; `KATAK_SHOT_DELAY_MS` waits longer (e.g. for a sort to finish).

// No console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod export_ui;
mod filter_ui;
mod fonts;
mod grid;
mod parse_dialog;
mod perf;
mod search_ui;
mod sort_ui;
mod status;
mod tab;
mod theme;
mod watch;

use std::path::PathBuf;
use std::time::Instant;

/// Command-line arguments.
#[derive(Debug, Default)]
pub struct Args {
    pub files: Vec<PathBuf>,
    pub perf_log: Option<PathBuf>,
    pub self_test: bool,
    pub screenshot: Option<PathBuf>,
    pub show: Option<String>,
    /// `--buffered`: read files like a network drive (no memory map).
    pub buffered: bool,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--perf") => args.perf_log = it.next().map(PathBuf::from),
            Some("--self-test") => args.self_test = true,
            Some("--screenshot") => args.screenshot = it.next().map(PathBuf::from),
            Some("--show") => args.show = it.next().and_then(|s| s.into_string().ok()),
            Some("--buffered") => args.buffered = true,
            _ => args.files.push(PathBuf::from(a)),
        }
    }
    args
}

fn main() -> eframe::Result {
    let started = Instant::now();
    let args = parse_args();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Katak")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true)
            .with_icon(theme::app_icon()),
        // Scripted runs (screenshots, self-test) use the default size and
        // leave the user's window geometry alone.
        persist_window: args.screenshot.is_none() && !args.self_test,
        ..Default::default()
    };
    eframe::run_native(
        "Katak",
        options,
        Box::new(move |cc| Ok(Box::new(app::KatakApp::new(cc, args, started)))),
    )
}
