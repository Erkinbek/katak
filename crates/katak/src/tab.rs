//! One open document and its UI state.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use katak_core::document::{Document, OpenOptions};
use katak_core::job::{Job, Notify};
use katak_core::logical::LRow;
use katak_core::save::{self, SaveReport};

use crate::grid::{Grid, GridInfo};
use crate::parse_dialog::ParseDialog;

/// Bytes of a cell loaded into the cell viewer.
pub const VIEWER_PAGE: usize = 256 << 10;

/// A cell opened in full (double-click on a large or multi-line value).
pub struct CellViewer {
    pub row: LRow,
    pub col: usize,
    pub text: String,
    /// Only the first [`VIEWER_PAGE`] bytes are shown (and editing is off).
    pub truncated: bool,
    pub editable: bool,
    pub original: String,
}

/// A save in progress.
pub struct SaveJob {
    pub job: Job,
    pub target: PathBuf,
    pub result: Arc<Mutex<Option<Result<SaveReport, String>>>>,
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
    pub save: Option<SaveJob>,
}

impl Tab {
    pub fn open(path: &Path, notify: Notify) -> Result<Tab, String> {
        Self::open_with(path, OpenOptions::default(), notify)
    }

    pub fn open_with(path: &Path, opts: OpenOptions, notify: Notify) -> Result<Tab, String> {
        let doc = Document::open(path, opts, notify)
            .map_err(|e| describe_open_error(path, &e.to_string()))?;
        Ok(Tab {
            doc,
            grid: Grid::new(),
            path: path.to_path_buf(),
            title: title_of(path),
            parse_dialog: None,
            cell_viewer: None,
            info: GridInfo::default(),
            first_paint: None,
            banner_dismissed: false,
            search: Default::default(),
            filter: Default::default(),
            save: None,
        })
    }

    /// Unsaved changes exist.
    pub fn dirty(&self) -> bool {
        !self.doc.overlay.is_empty()
    }

    pub fn open_cell(&mut self, row: LRow, col: usize) {
        let text = self
            .doc
            .value(&row, col, VIEWER_PAGE + 1)
            .unwrap_or_default();
        let truncated = text.len() > VIEWER_PAGE;
        let text = if truncated {
            let mut cut = VIEWER_PAGE;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text[..cut].to_string()
        } else {
            text
        };
        let editable = !truncated && !self.grid.read_only && self.save.is_none();
        self.cell_viewer = Some(CellViewer {
            row,
            col,
            original: text.clone(),
            text,
            truncated,
            editable,
        });
    }

    /// Starts writing the document to `target` in the background.
    pub fn start_save(&mut self, target: PathBuf, backup: bool, notify: Notify) {
        let input = self.doc.save_input(self.info.columns.max(1));
        let result = Arc::new(Mutex::new(None));
        let (r, t) = (result.clone(), target.clone());
        let job = Job::spawn("save", notify, move |ctl| {
            let res = save::save_file(&input, &t, backup, ctl).map_err(|e| e.to_string());
            if let Ok(mut slot) = r.lock() {
                *slot = Some(res);
            }
            Ok(())
        });
        self.save = Some(SaveJob {
            job,
            target,
            result,
        });
    }

    /// Makes the tab show `path` (after Save As).
    pub fn retarget(&mut self, path: &Path, notify: Notify) -> Result<(), String> {
        let opts = OpenOptions {
            dialect: Some(self.doc.dialect().clone()),
            ..OpenOptions::default()
        };
        let doc = Document::open(path, opts, notify)
            .map_err(|e| describe_open_error(path, &e.to_string()))?;
        self.doc = doc;
        self.path = path.to_path_buf();
        self.title = title_of(path);
        self.grid.reload_keep_layout();
        Ok(())
    }
}

fn title_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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
