//! Sorting by a column (header menu): a background permutation shown as
//! a row list. The file is never reordered; Export writes the sort order.

use katak_core::infer::ColumnType;
use katak_core::job::Notify;
use katak_core::sort::{self, SortKey, SortRun, SortSpec};

use crate::grid::View;
use crate::tab::Tab;

/// The sort of one tab.
pub struct SortState {
    pub run: SortRun,
    /// Column name for the status bar.
    name: String,
    generation: u64,
    /// The sorted rows are on screen (the job has finished).
    shown: bool,
}

impl SortState {
    pub fn describe(&self) -> String {
        let spec = self.run.spec;
        let how = match spec.key {
            SortKey::Text => "text",
            SortKey::Number => "numbers",
            SortKey::Date => "dates",
        };
        let dir = if spec.descending {
            "descending"
        } else {
            "ascending"
        };
        format!("Sorted by {} ({how}, {dir})", self.name)
    }

    pub fn is_shown(&self) -> bool {
        self.shown
    }
}

/// Sorts by `col`, comparing values the way the column's type suggests.
/// The current view stays until the sorted rows are ready.
pub fn request(tab: &mut Tab, col: usize, descending: bool) {
    let key = match tab.grid.cols.kind(col) {
        ColumnType::Integer | ColumnType::Float => SortKey::Number,
        ColumnType::Date => SortKey::Date,
        ColumnType::Text => SortKey::Text,
    };
    if let Some(s) = tab.sort.take() {
        s.run.job.cancel();
    }
    tab.sort_pending = Some(SortSpec {
        col,
        key,
        descending,
    });
}

/// Removes the sort; the filter (if any) or all rows are shown again.
pub fn clear(tab: &mut Tab) {
    tab.sort = None;
    tab.sort_pending = None;
    tab.grid.sort_mark = None;
    let view = match &tab.filter.run {
        Some(f) => View::List(f.data.clone()),
        None => View::Natural,
    };
    tab.grid.set_view(view);
}

/// The filter changed: sort its new rows (or all rows) the same way.
pub fn refilter(tab: &mut Tab) {
    if let Some(s) = tab.sort.take() {
        s.run.job.cancel();
        tab.sort_pending = Some(s.run.spec);
    }
    tab.grid.sort_mark = None;
}

/// Per-frame housekeeping. Returns a message for the user.
pub fn tick(tab: &mut Tab, budget: u64, notify: &Notify) -> Option<String> {
    // A dialect change or a reload invalidates the row offsets.
    if tab
        .sort
        .as_ref()
        .is_some_and(|s| s.generation != tab.doc.generation())
    {
        tab.sort = None;
        tab.sort_pending = None;
        tab.grid.sort_mark = None;
    }
    if let Some(spec) = tab.sort_pending {
        let filter = tab.filter.run.as_ref();
        if filter.is_some_and(|f| f.job.is_running()) {
            return None;
        }
        tab.sort_pending = None;
        let only = filter.map(|f| f.data.clone());
        let est = match &only {
            Some(f) => f.lock().rows.len() as u64,
            None => tab.doc.stats().rows,
        };
        match sort::start(tab.doc.target(), spec, only, budget, est, notify.clone()) {
            Ok(run) => {
                let name = tab
                    .doc
                    .column_name(spec.col)
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("column {}", spec.col + 1));
                tab.sort = Some(SortState {
                    run,
                    name,
                    generation: tab.doc.generation(),
                    shown: false,
                });
            }
            Err(e) => {
                tab.grid.sort_mark = None;
                return Some(e);
            }
        }
    }
    let s = tab.sort.as_mut()?;
    if s.shown || s.run.job.is_running() {
        return None;
    }
    let (done, error) = {
        let d = s.run.data.lock();
        (d.done, d.error.clone())
    };
    if done {
        s.shown = true;
        tab.grid.set_view(View::List(s.run.data.clone()));
        tab.grid.sort_mark = Some((s.run.spec.col, s.run.spec.descending));
        return None;
    }
    let msg = error.or_else(|| s.run.job.error());
    tab.sort = None;
    msg
}
