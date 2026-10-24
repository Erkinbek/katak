//! One open document and its UI state.

use std::path::{Path, PathBuf};

use katak_core::document::{Document, OpenOptions};
use katak_core::job::Notify;
use katak_core::scan::RecSpan;

use crate::grid::{Grid, GridInfo};
use crate::parse_dialog::ParseDialog;

/// Bytes of a cell loaded into the cell viewer at once.
pub const VIEWER_PAGE: usize = 256 << 10;

/// A cell opened in full ("double-click to expand").
pub struct CellViewer {
    pub rec: RecSpan,
    pub col: usize,
    pub text: String,
    pub truncated: bool,
}

pub struct Tab {
    pub doc: Document,
    pub grid: Grid,
    pub path: PathBuf,
    pub title: String,
    pub parse_dialog: Option<ParseDialog>,
    pub cell_viewer: Option<CellViewer>,
    pub info: GridInfo,
    pub first_paint: Option<f64>,
    pub banner_dismissed: bool,
    pub search: crate::search_ui::SearchBar,
    pub filter: crate::filter_ui::FilterBar,
}

impl Tab {
    pub fn open(path: &Path, notify: Notify) -> Result<Tab, String> {
        let doc = Document::open(path, OpenOptions::default(), notify)
            .map_err(|e| describe_open_error(path, &e.to_string()))?;
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Ok(Tab {
            doc,
            grid: Grid::new(),
            path: path.to_path_buf(),
            title,
            parse_dialog: None,
            cell_viewer: None,
            info: GridInfo::default(),
            first_paint: None,
            banner_dismissed: false,
            search: Default::default(),
            filter: Default::default(),
        })
    }

    pub fn open_cell(&mut self, rec: RecSpan, col: usize) {
        let value = self.doc.field_value(&rec, col, VIEWER_PAGE).ok().flatten();
        let (text, truncated) = value.unwrap_or_default();
        self.cell_viewer = Some(CellViewer {
            rec,
            col,
            text,
            truncated,
        });
    }
}

fn describe_open_error(path: &Path, err: &str) -> String {
    let lower = err.to_lowercase();
    let hint = if lower.contains("used by another process") || lower.contains("os error 32") {
        " — the file is locked by another program."
    } else if lower.contains("denied") || lower.contains("os error 5") {
        " — permission denied."
    } else if lower.contains("cannot find")
        || lower.contains("not found")
        || lower.contains("os error 2")
        || lower.contains("os error 3")
    {
        " — the file does not exist (moved or deleted?)."
    } else {
        ""
    };
    format!("Cannot open {}: {err}{hint}", path.display())
}
