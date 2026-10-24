//! The virtualized grid.
//!
//! Only the visible rows *and* the visible columns are fetched, decoded,
//! laid out and painted each frame. Rows are logical rows (source records
//! with the edit overlay applied); the vertical position is a logical
//! position plus a sub-row pixel offset, and the vertical scrollbar maps to
//! byte offsets, so it behaves the same before and after indexing.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, EventFilter, FontId, Key, Rect, Response,
    Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use eframe::epaint::text::{LayoutJob, TextFormat, TextWrapping};
use katak_core::document::{Cell, Document};
use katak_core::filter::{FilterData, Row, SharedFilter};
use katak_core::infer::{self, ColumnType};
use katak_core::locate::{FetchEnd, RowPos};
use katak_core::logical::{LKind, LPos, LRow};
use katak_core::overlay::{ColRef, Deleted, RowRef};
use katak_core::search::Matcher;
use katak_core::source::is_pending;

use crate::theme::GridColors;

const MIN_COL_WIDTH: f32 = 28.0;
const MAX_AUTO_WIDTH: f32 = 360.0;
const DEFAULT_COL_WIDTH: f32 = 110.0;
const SCROLLBAR: f32 = 14.0;
const CELL_PAD: f32 = 6.0;
const TOOLTIP_CHARS: usize = 4000;
/// Largest selection an edit operation (clear, fill, paste) may touch.
const MAX_EDIT_CELLS: u64 = 1_000_000;
/// Largest value edited in place; bigger ones open the cell viewer.
const MAX_INLINE_EDIT: usize = 64 << 10;
/// Clipboard copies above this ask for confirmation; above the hard limit
/// they are refused (use Export instead).
pub const COPY_CONFIRM_BYTES: usize = 100 << 20;
const COPY_MAX_BYTES: usize = 1 << 30;
/// Column operations rebuild the column order list.
const MAX_COLUMN_OPS: usize = 100_000;
/// Largest selection summarized in the status bar (read once, cached).
const MAX_STATS_CELLS: u64 = 20_000;

/// Count, sum, minimum and maximum of a selection, for the status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SelStats {
    pub rows: u64,
    pub cols: u64,
    /// Non-empty cells (`None`: too many cells to read).
    pub filled: Option<u64>,
    pub numbers: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// What a cached summary was computed for: selection box, list view,
/// document and overlay generations.
type StatsKey = (u64, u64, usize, usize, bool, u64, u64);

/// Column widths (and their prefix sums) and inferred types.
#[derive(Default)]
pub struct Columns {
    widths: Vec<f32>,
    prefix: Vec<f64>,
    pub types: Vec<ColumnType>,
    pub type_overrides: Vec<Option<ColumnType>>,
    dirty: bool,
}

impl Columns {
    fn ensure(&mut self, n: usize) {
        if self.widths.len() < n {
            self.widths.resize(n, DEFAULT_COL_WIDTH);
            self.dirty = true;
        }
    }

    fn rebuild(&mut self) {
        if self.dirty || self.prefix.len() != self.widths.len() + 1 {
            self.prefix.clear();
            self.prefix.reserve(self.widths.len() + 1);
            let mut x = 0.0f64;
            self.prefix.push(0.0);
            for w in &self.widths {
                x += *w as f64;
                self.prefix.push(x);
            }
            self.dirty = false;
        }
    }

    pub fn len(&self) -> usize {
        self.widths.len()
    }

    pub fn width(&self, c: usize) -> f32 {
        self.widths.get(c).copied().unwrap_or(DEFAULT_COL_WIDTH)
    }

    fn set_width(&mut self, c: usize, w: f32) {
        self.ensure(c + 1);
        self.widths[c] = w.max(MIN_COL_WIDTH);
        self.dirty = true;
    }

    fn x(&self, c: usize) -> f64 {
        self.prefix[c.min(self.prefix.len() - 1)]
    }

    pub fn kind(&self, c: usize) -> ColumnType {
        self.type_overrides
            .get(c)
            .copied()
            .flatten()
            .or_else(|| self.types.get(c).copied())
            .unwrap_or_default()
    }

    pub fn set_override(&mut self, c: usize, t: Option<ColumnType>) {
        if self.type_overrides.len() <= c {
            self.type_overrides.resize(c + 1, None);
        }
        self.type_overrides[c] = t;
    }

    /// Keeps per-column settings aligned when a column is inserted.
    fn insert(&mut self, at: usize) {
        if at <= self.widths.len() {
            self.widths.insert(at, DEFAULT_COL_WIDTH);
        }
        if at <= self.types.len() {
            self.types.insert(at, ColumnType::Text);
        }
        if at <= self.type_overrides.len() {
            self.type_overrides.insert(at, None);
        }
        self.dirty = true;
    }

    fn remove(&mut self, at: usize) {
        for v in [&mut self.widths] {
            if at < v.len() {
                v.remove(at);
            }
        }
        if at < self.types.len() {
            self.types.remove(at);
        }
        if at < self.type_overrides.len() {
            self.type_overrides.remove(at);
        }
        self.dirty = true;
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.ensure(a.max(b) + 1);
        self.widths.swap(a, b);
        if a.max(b) < self.types.len() {
            self.types.swap(a, b);
        }
        if a.max(b) < self.type_overrides.len() {
            self.type_overrides.swap(a, b);
        }
        self.dirty = true;
    }

    /// First column whose right edge is past `x` (relative to column 0).
    fn at_x(&self, x: f64) -> usize {
        self.prefix
            .partition_point(|&p| p <= x)
            .saturating_sub(1)
            .min(self.widths.len().saturating_sub(1))
    }
}

/// Commands from menus, dialogs, shortcuts or the self-test.
#[derive(Clone, Copy, Debug)]
pub enum GridCommand {
    /// Go to data row (0-based, logical).
    GotoRow(u64),
    GotoOffset(u64),
    GotoRecord(RowPos),
    Top,
    Bottom,
    ScrollRows(i64),
    ScrollX(f32),
    /// Jump to a fraction of the file (scrollbar).
    JumpFraction(f32),
    AutoFit(usize),
}

/// Editing operations (menus and shortcuts).
#[derive(Clone, Debug)]
pub enum EditAction {
    Copy,
    Cut,
    Paste(String),
    Clear,
    FillDown,
    SelectAll,
    InsertRowAbove,
    InsertRowBelow,
    DeleteRows,
    InsertColumnLeft,
    InsertColumnRight,
    DeleteColumn,
    MoveColumnLeft,
    MoveColumnRight,
    RenameColumn(usize, String),
    StartEdit,
}

/// What the grid tells the app after a frame.
#[derive(Clone, Debug, Default)]
pub struct GridInfo {
    pub cursor: Option<LPos>,
    /// Logical (or, in a filtered view, original) row number, 0-based.
    pub cursor_row: u64,
    pub cursor_exact: bool,
    pub cursor_col: usize,
    pub columns: usize,
    pub visible_rows: usize,
    pub loading: bool,
    /// Open this cell in the viewer.
    pub open_cell: Option<(LRow, usize)>,
    pub painted_rows: usize,
    /// "Filter this column" was chosen in the header menu.
    pub filter_column: Option<usize>,
    /// "Rename column" was chosen in the header menu.
    pub rename_column: Option<usize>,
    /// "Sort ascending/descending" was chosen: (column, descending).
    pub sort: Option<(usize, bool)>,
    /// "Clear sort" was chosen.
    pub clear_sort: bool,
    /// Text to put on the clipboard.
    pub copy: Option<String>,
    /// A large copy waiting for confirmation.
    pub confirm_copy: Option<String>,
    pub message: Option<String>,
    pub editing: bool,
    /// Summary of a multi-cell selection.
    pub selection: Option<SelStats>,
}

/// Which rows the grid shows.
#[derive(Clone, Default)]
pub enum View {
    /// Every row in order.
    #[default]
    Natural,
    /// The rows of a filter (file order, possibly still running) or of a
    /// sort (sort order).
    List(SharedFilter),
}

/// The selection (or the cursor cell), for Export.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    /// Position of the first selected row.
    pub start: LPos,
    /// First row index: logical row number, or list index in a list view.
    pub first: u64,
    pub rows: u64,
    /// Inclusive column range.
    pub cols: (usize, usize),
}

/// Index of the listed row with the smallest key at or after `target`, or
/// with the largest key when there is none. Lists in file order are
/// bisected; sort permutations are scanned (on explicit jumps only).
fn list_find(d: &FilterData, key: fn(&Row) -> u64, target: u64) -> usize {
    let n = d.rows.len();
    if !d.sorted {
        return d
            .rows
            .partition_point(|r| key(r) < target)
            .min(n.saturating_sub(1));
    }
    let mut after: Option<(u64, usize)> = None;
    let mut before: Option<(u64, usize)> = None;
    for (i, r) in d.rows.iter().enumerate() {
        let k = key(&r);
        if k >= target {
            if after.is_none_or(|(b, _)| k < b) {
                after = Some((k, i));
            }
        } else if before.is_none_or(|(b, _)| k > b) {
            before = Some((k, i));
        }
    }
    after.or(before).map_or(0, |(_, i)| i)
}

/// A row on screen.
#[derive(Clone, Copy, Debug)]
struct GRow {
    row: LRow,
    /// Logical row number (natural view) or list index (filtered view).
    index: u64,
    /// Number shown in the gutter (0-based).
    label: u64,
    exact: bool,
}

/// A selection end: position, its row index, and a column.
#[derive(Clone, Copy, Debug)]
struct Mark {
    pos: LPos,
    index: u64,
    col: usize,
}

struct Editor {
    row: LRow,
    col: usize,
    text: String,
    /// Started by typing: arrow keys commit and move.
    typing: bool,
    focus: bool,
}

/// Per-tab grid state.
pub struct Grid {
    pub top: LPos,
    top_px: f32,
    pub scroll_x: f32,
    pub cursor: LPos,
    cursor_index: u64,
    pub cursor_col: usize,
    anchor: Option<Mark>,
    pub frozen: usize,
    pub cols: Columns,
    generation: u64,
    initialized: bool,
    rows: Vec<GRow>,
    loading: bool,
    commands: Vec<GridCommand>,
    resize: Option<(usize, f32, f32)>,
    vdrag: Option<f32>,
    hdrag: Option<(f32, f32)>,
    font_size: f32,
    monospace: bool,
    header_names: Vec<(usize, String)>,
    /// Column the context menu was opened for.
    menu_col: Option<usize>,
    view: View,
    /// First visible list index and cursor list index (filtered view).
    ltop: usize,
    lcur: usize,
    /// Search matches are highlighted in visible cells.
    pub highlight: Option<Arc<Matcher>>,
    /// The sorted column and direction (descending), marked in the header.
    pub sort_mark: Option<(usize, bool)>,
    pub read_only: bool,
    editor: Option<Editor>,
    actions: Vec<EditAction>,
    out: GridInfo,
    /// Layout to restore after the document is re-read (save, Save As).
    restore: Option<(Columns, usize, f32, u64, usize)>,
    stats: Option<(StatsKey, SelStats)>,
}

impl Grid {
    pub fn new() -> Self {
        let p = LPos {
            rec: RowPos {
                offset: 0,
                record: 0,
                exact: true,
            },
            sub: 0,
        };
        Grid {
            top: p,
            top_px: 0.0,
            scroll_x: 0.0,
            cursor: p,
            cursor_index: 0,
            cursor_col: 0,
            anchor: None,
            frozen: 0,
            cols: Columns::default(),
            generation: u64::MAX,
            initialized: false,
            rows: Vec::new(),
            loading: false,
            commands: Vec::new(),
            resize: None,
            vdrag: None,
            hdrag: None,
            font_size: 14.0,
            monospace: false,
            header_names: Vec::new(),
            menu_col: None,
            view: View::Natural,
            ltop: 0,
            lcur: 0,
            highlight: None,
            sort_mark: None,
            read_only: false,
            editor: None,
            actions: Vec::new(),
            out: GridInfo::default(),
            restore: None,
            stats: None,
        }
    }

    /// The document is about to be re-read: keep column widths and the
    /// cursor row for the next frame.
    pub fn reload_keep_layout(&mut self) {
        let cols = std::mem::take(&mut self.cols);
        self.restore = Some((
            cols,
            self.frozen,
            self.scroll_x,
            self.cursor_index,
            self.cursor_col,
        ));
        self.initialized = false;
    }

    pub fn command(&mut self, c: GridCommand) {
        self.commands.push(c);
    }

    pub fn action(&mut self, a: EditAction) {
        self.actions.push(a);
    }

    pub fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    /// Switches between all rows and a filtered list. The cursor moves to
    /// the nearest listed row.
    pub fn set_view(&mut self, view: View) {
        let cursor = self.cursor;
        self.view = view;
        self.top_px = 0.0;
        self.anchor = None;
        self.editor = None;
        self.ltop = 0;
        self.lcur = 0;
        if let Some(l) = self.list() {
            // A filter keeps the cursor near its row; a sort starts at the top.
            let d = l.lock();
            if !d.rows.is_empty() && !d.sorted {
                self.lcur = list_find(&d, |r| r.offset, cursor.rec.offset);
                self.ltop = self.lcur;
            }
        } else {
            self.top = cursor;
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self.view, View::List(_))
    }

    /// The row list shown (filter or sort), if any.
    pub fn list(&self) -> Option<SharedFilter> {
        match &self.view {
            View::List(l) => Some(l.clone()),
            View::Natural => None,
        }
    }

    /// Resets position and recomputes column widths/types from the sample.
    fn reset(&mut self, doc: &mut Document) {
        self.generation = doc.generation();
        self.top = doc.first_lpos();
        self.top_px = 0.0;
        self.scroll_x = 0.0;
        self.cursor = self.top;
        self.cursor_col = 0;
        self.anchor = None;
        self.editor = None;
        self.view = View::Natural;
        self.cols = Columns::default();
        self.autosize(doc);
        self.initialized = true;
        if let Some((cols, frozen, scroll_x, row, col)) = self.restore.take() {
            self.cols = cols;
            self.frozen = frozen;
            self.scroll_x = scroll_x;
            self.cursor_col = col;
            self.commands.push(GridCommand::GotoRow(row));
        }
    }

    /// Widths and types from the first few thousand sampled records.
    fn autosize(&mut self, doc: &mut Document) {
        let rows = doc.sample_rows(3000, 200);
        let header = doc.dialect().has_header;
        let ncols = rows
            .iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(0)
            .min(1_000_000);
        let char_w = self.font_size * if self.monospace { 0.62 } else { 0.56 };
        let mut widths = vec![0usize; ncols];
        for r in &rows {
            for (c, v) in r.iter().enumerate() {
                widths[c] = widths[c].max(v.chars().count());
            }
        }
        // A single column (raw lines) may take more room.
        let max_w = if ncols == 1 {
            MAX_AUTO_WIDTH * 4.0
        } else {
            MAX_AUTO_WIDTH
        };
        self.cols.widths = widths
            .iter()
            .map(|&n| (n as f32 * char_w + 2.0 * CELL_PAD + 4.0).clamp(48.0, max_w))
            .collect();
        let data = if header && !rows.is_empty() {
            &rows[1..]
        } else {
            &rows[..]
        };
        self.cols.types = (0..ncols)
            .map(|c| infer::infer(data.iter().filter_map(|r| r.get(c).map(String::as_str))))
            .collect();
        self.cols.dirty = true;
    }

    // ------------------------------------------------------------ moving

    fn clamp_top(&mut self, doc: &mut Document) {
        if !self.is_list() && self.top.rec.record < doc.header_records() {
            self.top = doc.first_lpos();
            self.top_px = 0.0;
        }
    }

    /// Position `delta` logical rows away from `from` (clamped to the data).
    fn offset_pos(doc: &mut Document, from: LPos, delta: i64) -> LPos {
        if delta > 0 {
            if let Ok((rows, _)) = doc.lrows_from(from, delta as usize + 1)
                && let Some(r) = rows.get(delta as usize).or(rows.last())
            {
                return r.pos;
            }
        } else if delta < 0
            && let Ok(rows) = doc.lrows_before(from, (-delta) as usize)
            && let Some(r) = rows.first()
        {
            return r.pos;
        }
        from
    }

    fn move_top(&mut self, doc: &mut Document, delta: i64) {
        if let Some(l) = self.list() {
            let n = l.lock().rows.len();
            if n > 0 {
                self.ltop = (self.ltop as i64 + delta).clamp(0, n as i64 - 1) as usize;
            }
            return;
        }
        self.top = Self::offset_pos(doc, self.top, delta);
        self.clamp_top(doc);
    }

    fn move_cursor(&mut self, doc: &mut Document, delta: i64) {
        if let Some(l) = self.list() {
            let n = l.lock().rows.len();
            if n > 0 {
                self.lcur = (self.lcur as i64 + delta).clamp(0, n as i64 - 1) as usize;
            }
            return;
        }
        self.cursor = Self::offset_pos(doc, self.cursor, delta);
    }

    fn goto(&mut self, doc: &mut Document, pos: LPos, visible: usize) {
        if let Some(l) = self.list() {
            let d = l.lock();
            if !d.rows.is_empty() {
                self.lcur = list_find(&d, |r| r.offset, pos.rec.offset);
                self.ltop = self.lcur.saturating_sub(visible / 3);
                self.top_px = 0.0;
            }
            return;
        }
        self.cursor = pos;
        self.top = Self::offset_pos(doc, pos, -((visible / 3) as i64));
        self.top_px = 0.0;
        self.clamp_top(doc);
    }

    fn ensure_cursor_visible(&mut self, doc: &mut Document, visible: usize) {
        let full = visible.saturating_sub(1).max(1);
        if self.is_list() {
            if self.lcur < self.ltop {
                self.ltop = self.lcur;
            } else if self.lcur >= self.ltop + full {
                self.ltop = self.lcur + 1 - full;
            }
            return;
        }
        let (ci, ti) = (doc.lnumber(&self.cursor), doc.lnumber(&self.top));
        if ci < ti {
            self.top = self.cursor;
            self.top_px = 0.0;
        } else if ci >= ti + full as u64 {
            self.top = Self::offset_pos(doc, self.cursor, -(full as i64 - 1));
            self.top_px = 0.0;
        }
    }

    /// Shows the last page; returns the last row's position.
    fn go_bottom(&mut self, doc: &mut Document, visible: usize) -> Result<Option<LPos>, ()> {
        let end = doc.end_lpos();
        let rows = match doc.lrows_before(end, visible.saturating_sub(1).max(1)) {
            Ok(r) => r,
            Err(e) if is_pending(&e) => return Err(()),
            Err(_) => return Ok(None),
        };
        let (Some(first), Some(last)) = (rows.first().map(|r| r.pos), rows.last().map(|r| r.pos))
        else {
            return Ok(None);
        };
        self.top = first;
        self.top_px = 0.0;
        self.clamp_top(doc);
        Ok(Some(last))
    }

    /// Handles position commands in the filtered view; false for the others.
    fn list_command(&mut self, doc: &mut Document, c: GridCommand, visible: usize) -> bool {
        let Some(l) = self.list() else { return false };
        let n = l.lock().rows.len();
        let positional = matches!(
            c,
            GridCommand::GotoRow(_)
                | GridCommand::GotoOffset(_)
                | GridCommand::GotoRecord(_)
                | GridCommand::Top
                | GridCommand::Bottom
                | GridCommand::JumpFraction(_)
        );
        if n == 0 || !positional {
            return positional;
        }
        let page = visible.saturating_sub(1).max(1);
        let find = |key: fn(&Row) -> u64, target: u64| list_find(&l.lock(), key, target);
        let (top, cur) = match c {
            GridCommand::GotoRow(row) => {
                let i = find(|r| r.record, row + doc.header_records());
                (i.saturating_sub(visible / 3), i)
            }
            GridCommand::GotoOffset(off) => {
                let i = find(|r| r.offset, off);
                (i.saturating_sub(visible / 3), i)
            }
            GridCommand::GotoRecord(p) => {
                let i = find(|r| r.offset, p.offset);
                (i.saturating_sub(visible / 3), i)
            }
            GridCommand::Top => (0, 0),
            GridCommand::Bottom => (n.saturating_sub(page), n - 1),
            GridCommand::JumpFraction(f) => (
                ((n as f64 * f.clamp(0.0, 1.0) as f64) as usize).min(n.saturating_sub(page)),
                self.lcur,
            ),
            _ => return false,
        };
        self.ltop = top.min(n - 1);
        self.lcur = cur.min(n - 1);
        self.top_px = 0.0;
        true
    }

    fn apply_commands(&mut self, doc: &mut Document, row_h: f32, visible: usize, view_w: f32) {
        let cmds = std::mem::take(&mut self.commands);
        // Jumps into data that is still loading from a slow drive are
        // retried on the next frame.
        let mut retry = Vec::new();
        for c in cmds {
            if self.is_list() && self.list_command(doc, c, visible) {
                continue;
            }
            match c {
                GridCommand::GotoRow(n) => {
                    // Logical row n: locate the source record, then adjust
                    // for inserted/deleted rows before it.
                    let hdr = doc.header_records();
                    match doc.locator().locate_record(n + hdr) {
                        Ok(p) => {
                            let pos = LPos { rec: p, sub: 0 };
                            let diff = n as i64 - doc.lnumber(&pos) as i64;
                            let pos = Self::offset_pos(doc, pos, diff);
                            self.goto(doc, pos, visible);
                        }
                        Err(e) if is_pending(&e) => retry.push(c),
                        Err(_) => {}
                    }
                }
                GridCommand::GotoOffset(off) => match doc.locator().locate_offset(off) {
                    Ok(p) => {
                        let pos = doc.lpos_of(p);
                        self.goto(doc, pos, visible);
                    }
                    Err(e) if is_pending(&e) => retry.push(c),
                    Err(_) => {}
                },
                GridCommand::GotoRecord(p) => {
                    let pos = doc.lpos_of(p);
                    self.goto(doc, pos, visible);
                }
                GridCommand::Top => {
                    self.top = doc.first_lpos();
                    self.cursor = self.top;
                    self.top_px = 0.0;
                }
                GridCommand::Bottom => match self.go_bottom(doc, visible) {
                    Ok(Some(last)) => self.cursor = last,
                    Ok(None) => {}
                    Err(()) => retry.push(c),
                },
                GridCommand::ScrollRows(n) => self.move_top(doc, n),
                GridCommand::ScrollX(px) => self.scroll_x = (self.scroll_x + px).max(0.0),
                GridCommand::JumpFraction(f) => {
                    let (a, b) = (doc.data_start(), doc.source().len());
                    let off = a + ((b - a) as f64 * f.clamp(0.0, 1.0) as f64) as u64;
                    if f >= 0.999 {
                        if self.go_bottom(doc, visible).is_err() {
                            retry.push(c);
                        }
                    } else {
                        match doc.locator().locate_offset(off) {
                            Ok(p) => self.top = LPos { rec: p, sub: 0 },
                            Err(e) if is_pending(&e) => retry.push(c),
                            Err(_) => {}
                        }
                    }
                    self.top_px = 0.0;
                    self.clamp_top(doc);
                }
                GridCommand::AutoFit(c) => self.autofit(doc, c),
            }
        }
        self.commands.extend(retry);
        // Normalize the sub-row offset into whole rows.
        if self.top_px >= row_h {
            let n = (self.top_px / row_h).floor() as i64;
            let before = (self.top, self.ltop);
            self.move_top(doc, n);
            self.top_px = if (self.top, self.ltop) == before {
                0.0
            } else {
                self.top_px - n as f32 * row_h
            };
        } else if self.top_px < 0.0 {
            let n = (-self.top_px / row_h).ceil() as i64;
            let before = (self.top, self.ltop);
            self.move_top(doc, -n);
            self.top_px = if (self.top, self.ltop) == before {
                0.0
            } else {
                self.top_px + n as f32 * row_h
            };
        }
        self.top_px = self.top_px.clamp(0.0, row_h);
        let total_w = self.cols.x(self.cols.len()) as f32;
        let frozen_w = self.cols.x(self.frozen.min(self.cols.len())) as f32;
        self.scroll_x = self.scroll_x.clamp(
            0.0,
            (total_w - frozen_w - (view_w - frozen_w) * 0.5).max(0.0),
        );
    }

    fn autofit(&mut self, doc: &mut Document, c: usize) {
        let mut chars = 4usize;
        let rows: Vec<LRow> = self.rows.iter().take(200).map(|g| g.row).collect();
        for r in &rows {
            if let Ok(cells) = doc.display_cells(r, c..c + 1, 400) {
                chars = chars.max(cells[0].0.text.chars().count());
            }
        }
        if let Some((_, h)) = self.header_names.iter().find(|(i, _)| *i == c) {
            chars = chars.max(h.chars().count() + 2);
        }
        let char_w = self.font_size * if self.monospace { 0.62 } else { 0.56 };
        self.cols.set_width(
            c,
            (chars as f32 * char_w + 2.0 * CELL_PAD + 4.0).clamp(40.0, 900.0),
        );
    }

    // ----------------------------------------------------------- fetching

    /// Rows `[ltop, ltop + n)` of the filtered view.
    /// Rows of the list view; (previous rows, true) while some are still
    /// loading from a slow drive.
    fn fetch_list(&mut self, doc: &mut Document, l: &SharedFilter, n: usize) -> (Vec<GRow>, bool) {
        let hdr = doc.header_records();
        let picked: Vec<Row> = {
            let d = l.lock();
            if d.rows.is_empty() {
                return (Vec::new(), false);
            }
            self.ltop = self.ltop.min(d.rows.len() - 1);
            self.lcur = self.lcur.min(d.rows.len() - 1);
            d.rows.slice(self.ltop, self.ltop + n)
        };
        let mut out = Vec::with_capacity(picked.len());
        for (k, r) in picked.into_iter().enumerate() {
            let rp = RowPos {
                offset: r.offset,
                record: r.record,
                exact: true,
            };
            let f = match doc.locator().records_from(rp, 1) {
                Ok(f) => f,
                Err(e) if is_pending(&e) => return (self.rows.clone(), true),
                Err(_) => continue,
            };
            if let Some((p, span)) = f
                .rows
                .first()
                .copied()
                .filter(|(p, _)| p.offset == r.offset)
            {
                let pos = doc.lpos_of(p);
                out.push(GRow {
                    row: LRow {
                        pos,
                        kind: LKind::Src(span),
                    },
                    index: (self.ltop + k) as u64,
                    label: r.record.saturating_sub(hdr),
                    exact: true,
                });
            }
        }
        (out, false)
    }

    fn fetch_natural(&mut self, doc: &mut Document, n: usize) -> (Vec<GRow>, bool) {
        let (rows, end) = match doc.lrows_from(self.top, n) {
            Ok(r) => r,
            Err(e) if is_pending(&e) => return (self.rows.clone(), true),
            Err(_) => (Vec::new(), FetchEnd::Eof),
        };
        let first = rows.first().map(|r| doc.lnumber(&r.pos)).unwrap_or(0);
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(k, row)| {
                let exact = row.pos.rec.exact;
                GRow {
                    row,
                    index: first + k as u64,
                    label: first + k as u64,
                    exact,
                }
            })
            .collect();
        (rows, end == FetchEnd::Budget)
    }

    // ---------------------------------------------------------- selection

    fn cursor_mark(&self) -> Mark {
        Mark {
            pos: self.cursor,
            index: self.cursor_index,
            col: self.cursor_col,
        }
    }

    /// (first row index, last row index, first col, last col).
    fn selection_box(&self) -> (u64, u64, usize, usize) {
        let c = self.cursor_mark();
        let a = self.anchor.unwrap_or(c);
        (
            a.index.min(c.index),
            a.index.max(c.index),
            a.col.min(c.col),
            a.col.max(c.col),
        )
    }

    /// The selection (or the cursor cell).
    pub fn selection(&self) -> Selection {
        let (r0, r1, c0, c1) = self.selection_box();
        let c = self.cursor_mark();
        let start = match self.anchor {
            Some(a) if a.index < c.index => a.pos,
            _ => c.pos,
        };
        Selection {
            start,
            first: r0,
            rows: r1 - r0 + 1,
            cols: (c0, c1),
        }
    }

    /// The rows of the selection (bounded by `max`).
    fn selected_rows(&mut self, doc: &mut Document, max: u64) -> Result<Vec<LRow>, String> {
        let (r0, r1, _, _) = self.selection_box();
        let count = r1 - r0 + 1;
        if count > max {
            return Err(format!(
                "The selection has {} rows; at most {} can be changed or copied at once. Use Export for more.",
                group(count),
                group(max)
            ));
        }
        if let Some(l) = self.list() {
            let picked = l.lock().rows.slice(r0 as usize, r1 as usize + 1);
            let mut out = Vec::with_capacity(picked.len());
            for r in picked {
                let rp = RowPos {
                    offset: r.offset,
                    record: r.record,
                    exact: true,
                };
                if let Some((p, span)) = doc
                    .locator()
                    .records_from(rp, 1)
                    .map_err(|e| e.to_string())?
                    .rows
                    .first()
                    .copied()
                {
                    out.push(LRow {
                        pos: doc.lpos_of(p),
                        kind: LKind::Src(span),
                    });
                }
            }
            return Ok(out);
        }
        let c = self.cursor_mark();
        let start = match self.anchor {
            Some(a) if a.index < c.index => a.pos,
            _ => c.pos,
        };
        let (rows, _) = doc
            .lrows_from(start, count as usize)
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    // ------------------------------------------------------------ editing

    /// The row under the cursor (from its position, not from the rows of
    /// the last frame, which may be stale after an edit action).
    pub fn current_row(&mut self, doc: &mut Document) -> Option<LRow> {
        if let Some(l) = self.list() {
            let r = l.lock().rows.get(self.lcur)?;
            let rp = RowPos {
                offset: r.offset,
                record: r.record,
                exact: true,
            };
            let (p, span) = doc
                .locator()
                .records_from(rp, 1)
                .ok()?
                .rows
                .first()
                .copied()?;
            return Some(LRow {
                pos: doc.lpos_of(p),
                kind: LKind::Src(span),
            });
        }
        doc.lrows_from(self.cursor, 1).ok()?.0.into_iter().next()
    }

    fn start_edit(&mut self, doc: &mut Document, initial: Option<String>) {
        if self.read_only {
            self.out.message = Some("The file is open read-only (Edit → Read-only).".into());
            return;
        }
        let Some(row) = self.current_row(doc) else {
            return;
        };
        let col = self.cursor_col;
        let typing = initial.is_some();
        let text = match initial {
            Some(t) => t,
            None => {
                let v = doc
                    .value(&row, col, MAX_INLINE_EDIT + 1)
                    .unwrap_or_default();
                if v.len() > MAX_INLINE_EDIT || v.contains(['\n', '\r']) {
                    // Large or multi-line values are edited in the cell viewer.
                    self.out.open_cell = Some((row, col));
                    return;
                }
                v
            }
        };
        self.anchor = None;
        self.editor = Some(Editor {
            row,
            col,
            text,
            typing,
            focus: true,
        });
    }

    /// Commits an open cell edit (before saving).
    pub fn commit(&mut self, doc: &mut Document) {
        self.commit_edit(doc);
    }

    fn commit_edit(&mut self, doc: &mut Document) {
        if let Some(e) = self.editor.take() {
            doc.set_values(vec![(e.row.row_ref(), e.col, e.text)]);
        }
    }

    /// Runs queued edit actions.
    fn run_actions(&mut self, doc: &mut Document, visible: usize) {
        for a in std::mem::take(&mut self.actions) {
            if let Err(e) = self.run_action(doc, a, visible) {
                self.out.message = Some(e);
            }
        }
    }

    fn run_action(
        &mut self,
        doc: &mut Document,
        a: EditAction,
        visible: usize,
    ) -> Result<(), String> {
        let modifying = !matches!(a, EditAction::Copy | EditAction::SelectAll);
        if modifying && self.read_only {
            return Err("The file is open read-only (Edit → Read-only).".into());
        }
        let ncols = self.cols.len().max(1);
        match a {
            EditAction::StartEdit => self.start_edit(doc, None),
            EditAction::SelectAll => {
                let first = doc.first_lpos();
                let end = doc.end_lpos();
                let last = doc
                    .lrows_before(end, 1)
                    .ok()
                    .and_then(|r| r.first().map(|r| r.pos))
                    .unwrap_or(first);
                if self.is_list() {
                    let n = self.list().map_or(0, |l| l.lock().rows.len());
                    self.anchor = Some(Mark {
                        pos: first,
                        index: 0,
                        col: 0,
                    });
                    self.lcur = n.saturating_sub(1);
                } else {
                    self.anchor = Some(Mark {
                        pos: first,
                        index: doc.lnumber(&first),
                        col: 0,
                    });
                    self.cursor = last;
                }
                self.cursor_col = ncols - 1;
            }
            EditAction::Copy | EditAction::Cut => {
                let (_, _, c0, c1) = self.selection_box();
                let rows = self.selected_rows(doc, MAX_EDIT_CELLS / (c1 - c0 + 1) as u64)?;
                let mut text = String::new();
                for r in &rows {
                    for c in c0..=c1 {
                        if c > c0 {
                            text.push('\t');
                        }
                        tsv_field(
                            &doc.value(r, c, 64 << 20).map_err(|e| e.to_string())?,
                            &mut text,
                        );
                        if text.len() > COPY_MAX_BYTES {
                            return Err("The selection is larger than 1 GB; use Export to save it to a file.".into());
                        }
                    }
                    text.push('\n');
                }
                if text.len() > COPY_CONFIRM_BYTES {
                    self.out.confirm_copy = Some(text);
                } else {
                    self.out.copy = Some(text);
                }
                if matches!(a, EditAction::Cut) {
                    self.clear_selection(doc, &rows, c0, c1);
                }
            }
            EditAction::Clear => {
                let (_, _, c0, c1) = self.selection_box();
                let rows = self.selected_rows(doc, MAX_EDIT_CELLS / (c1 - c0 + 1) as u64)?;
                self.clear_selection(doc, &rows, c0, c1);
            }
            EditAction::FillDown => {
                let (_, _, c0, c1) = self.selection_box();
                let rows = self.selected_rows(doc, MAX_EDIT_CELLS / (c1 - c0 + 1) as u64)?;
                let Some(first) = rows.first().copied() else {
                    return Ok(());
                };
                let mut changes = Vec::new();
                for c in c0..=c1 {
                    let v = doc.value(&first, c, 64 << 20).map_err(|e| e.to_string())?;
                    changes.extend(rows.iter().skip(1).map(|r| (r.row_ref(), c, v.clone())));
                }
                doc.set_values(changes);
            }
            EditAction::Paste(text) => {
                let table = parse_tsv(&text);
                if table.is_empty() {
                    return Ok(());
                }
                let cells: u64 = table.iter().map(|r| r.len() as u64).sum();
                if cells > MAX_EDIT_CELLS {
                    return Err(format!(
                        "The clipboard has {} cells; at most {} can be pasted at once.",
                        group(cells),
                        group(MAX_EDIT_CELLS)
                    ));
                }
                let start = if self.anchor.is_some() {
                    let (r0, _, c0, _) = self.selection_box();
                    self.anchor = None;
                    self.cursor_col = c0;
                    if self.is_list() {
                        self.lcur = r0 as usize;
                    }
                    let c = self.cursor_mark();
                    c.pos
                } else {
                    self.cursor
                };
                let rows: Vec<LRow> = if let Some(l) = self.list() {
                    let picked = l.lock().rows.slice(self.lcur, self.lcur + table.len());
                    picked
                        .into_iter()
                        .filter_map(|r| {
                            let rp = RowPos {
                                offset: r.offset,
                                record: r.record,
                                exact: true,
                            };
                            let (p, span) = doc
                                .locator()
                                .records_from(rp, 1)
                                .ok()?
                                .rows
                                .first()
                                .copied()?;
                            Some(LRow {
                                pos: doc.lpos_of(p),
                                kind: LKind::Src(span),
                            })
                        })
                        .collect()
                } else {
                    doc.lrows_from(start, table.len())
                        .map_err(|e| e.to_string())?
                        .0
                };
                let mut changes = Vec::new();
                for (r, vals) in rows.iter().zip(&table) {
                    for (k, v) in vals.iter().enumerate() {
                        changes.push((r.row_ref(), self.cursor_col + k, v.clone()));
                    }
                }
                doc.set_values(changes);
                if rows.len() < table.len() {
                    self.out.message = Some(format!(
                        "Pasted {} of {} rows (the end of the table was reached).",
                        rows.len(),
                        table.len()
                    ));
                }
            }
            EditAction::InsertRowAbove | EditAction::InsertRowBelow => {
                if self.is_list() {
                    return Err("Clear the filter and the sort to insert rows.".into());
                }
                let below = matches!(a, EditAction::InsertRowBelow);
                let here = self.current_row(doc).map(|r| r.pos).unwrap_or(self.cursor);
                // Inserting "below" means "above the next row".
                let target = if below {
                    doc.lrows_from(here, 2)
                        .ok()
                        .and_then(|(r, _)| r.get(1).map(|r| r.pos))
                        .unwrap_or(doc.end_lpos())
                } else {
                    here
                };
                let data_end = doc.source().len();
                let key = katak_core::logical::anchor_key(&target.rec, data_end);
                let ids = doc.overlay.insert_rows(key, target.sub as usize, 1);
                if !ids.is_empty() {
                    self.cursor = LPos {
                        rec: target.rec,
                        sub: target.sub,
                    };
                    self.ensure_cursor_visible(doc, visible);
                }
            }
            EditAction::DeleteRows => {
                let rows = self.selected_rows(doc, 10_000_000)?;
                let mut ids = Vec::new();
                let mut ranges: Vec<Deleted> = Vec::new();
                for r in &rows {
                    match r.kind {
                        LKind::New(id) => ids.push(id),
                        LKind::Src(span) => match ranges.last_mut() {
                            Some(last) if last.end == span.start => {
                                last.end = span.end;
                                last.records += 1;
                            }
                            _ => ranges.push(Deleted {
                                start: span.start,
                                end: span.end,
                                records: 1,
                            }),
                        },
                    }
                }
                doc.overlay.delete_rows(&ids, &ranges);
                self.anchor = None;
                if self.is_list() {
                    self.out.message = Some("Deleted rows disappear from the filtered view when the filter is applied again.".into());
                }
            }
            EditAction::InsertColumnLeft | EditAction::InsertColumnRight => {
                let at = self.cursor_col + matches!(a, EditAction::InsertColumnRight) as usize;
                let mut cols = self.column_order(doc, ncols)?;
                let new = doc.overlay.new_column_id();
                cols.insert(at.min(cols.len()), new);
                doc.overlay.set_columns(Some(cols));
                self.cols.insert(at);
                self.cursor_col = at;
            }
            EditAction::DeleteColumn => {
                let mut cols = self.column_order(doc, ncols)?;
                if self.cursor_col < cols.len() && cols.len() > 1 {
                    cols.remove(self.cursor_col);
                    doc.overlay.set_columns(Some(cols));
                    self.cols.remove(self.cursor_col);
                    self.cursor_col = self.cursor_col.min(ncols.saturating_sub(2));
                }
            }
            EditAction::MoveColumnLeft | EditAction::MoveColumnRight => {
                let mut cols = self.column_order(doc, ncols)?;
                let c = self.cursor_col;
                let to = if matches!(a, EditAction::MoveColumnLeft) {
                    c.checked_sub(1)
                } else {
                    (c + 1 < cols.len()).then_some(c + 1)
                };
                if let Some(to) = to {
                    cols.swap(c, to);
                    doc.overlay.set_columns(Some(cols));
                    self.cols.swap(c, to);
                    self.cursor_col = to;
                }
            }
            EditAction::RenameColumn(c, name) => {
                let Some(h) = doc.header_record().map_err(|e| e.to_string())? else {
                    return Err("This file has no header row (see Parsing options) — there is no name to change.".into());
                };
                doc.set_values(vec![(RowRef::Src(h.start), c, name)]);
            }
        }
        Ok(())
    }

    fn clear_selection(&mut self, doc: &mut Document, rows: &[LRow], c0: usize, c1: usize) {
        let mut changes = Vec::new();
        for r in rows {
            for c in c0..=c1 {
                changes.push((r.row_ref(), c, String::new()));
            }
        }
        doc.set_values(changes);
    }

    /// The current column order, materialized for a column operation.
    fn column_order(&self, doc: &Document, ncols: usize) -> Result<Vec<ColRef>, String> {
        if ncols > MAX_COLUMN_OPS {
            return Err(format!(
                "Column operations are limited to files with at most {} columns.",
                group(MAX_COLUMN_OPS as u64)
            ));
        }
        Ok(match doc.overlay.columns() {
            Some(c) => {
                let mut c = c.to_vec();
                while c.len() < ncols {
                    c.push(doc.overlay.col_ref(c.len()));
                }
                c
            }
            None => (0..ncols as u32).map(ColRef::Src).collect(),
        })
    }

    // ------------------------------------------------------------- drawing

    /// Draws the grid in all of `ui`'s available space.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        colors: &GridColors,
        font_size: f32,
        monospace: bool,
    ) -> GridInfo {
        self.out = GridInfo::default();
        self.font_size = font_size;
        self.monospace = monospace;
        doc.begin_frame();
        if !self.initialized || self.generation != doc.generation() {
            self.reset(doc);
        }
        let font = crate::theme::cell_font(font_size, monospace);
        let row_h = (font_size * 1.65).round();
        let header_h = row_h + 4.0;

        let full = ui.available_rect_before_wrap();
        let id = ui.scope_id().with("katak-grid");
        let response = ui.interact(full, id, Sense::click_and_drag());
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        ui.memory_mut(|m| {
            if m.has_focus(id) {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: false,
                    },
                );
            }
        });

        // Columns known so far.
        let stats_fields = doc.index().read().max_fields as usize;
        let ncols = doc
            .logical_columns(self.cols.len().max(stats_fields))
            .clamp(1, 10_000_000);
        self.cols.ensure(ncols);
        self.cols.rebuild();

        // Gutter wide enough for the row numbers.
        let est_rows = doc.stats().rows.max(1);
        let digits = (est_rows as f64).log10().floor() as usize + 2;
        let gutter_w = (digits as f32 * font_size * 0.62 + 18.0).max(48.0);

        let cells_rect = Rect::from_min_max(
            pos2(full.left() + gutter_w, full.top() + header_h),
            pos2(full.right() - SCROLLBAR, full.bottom() - SCROLLBAR),
        );
        let visible = ((cells_rect.height() / row_h).ceil() as usize + 1).max(1);
        self.out.visible_rows = visible;

        if self.editor.is_none() {
            self.handle_input(ui, &response, doc, visible, cells_rect);
        }
        self.run_actions(doc, visible);
        self.apply_commands(doc, row_h, visible, cells_rect.width());
        if !self.top.rec.exact
            && let Ok(p) = doc.locator().refine(self.top.rec)
        {
            self.top.rec = p;
        }
        if !self.cursor.rec.exact
            && let Ok(p) = doc.locator().refine(self.cursor.rec)
        {
            self.cursor.rec = p;
        }

        // Fetch visible rows.
        let (rows, loading) = if let Some(l) = self.list() {
            let len = l.lock().rows.len();
            if len > 0 && self.ltop + visible > len + 1 {
                self.ltop = len.saturating_sub(visible.saturating_sub(1));
            }
            self.fetch_list(doc, &l, visible)
        } else {
            self.fetch_natural(doc, visible)
        };
        // Do not leave empty space at the end.
        if !self.is_list()
            && rows.len() + 1 < visible
            && !loading
            && rows.first().is_none_or(|r| r.index > 0)
        {
            let missing = (visible - 1 - rows.len()) as i64;
            let before = self.top;
            self.move_top(doc, -missing);
            if self.top != before {
                ui.ctx().request_repaint();
            }
        }
        self.rows = rows;
        self.cursor_index = if self.is_list() {
            self.lcur as u64
        } else {
            doc.lnumber(&self.cursor)
        };
        if self.is_list()
            && let Some(l) = self.list()
            && let Some(r) = l.lock().rows.get(self.lcur)
        {
            self.cursor = doc.lpos_of(RowPos {
                offset: r.offset,
                record: r.record,
                exact: true,
            });
        }
        self.loading = loading;
        self.out.loading = loading;
        if loading {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        let painter = ui.painter_at(full);
        painter.rect_filled(full, CornerRadius::ZERO, colors.background);

        // Visible columns: frozen ones, then the scrolled range.
        let frozen = self.frozen.min(ncols);
        let frozen_w = self.cols.x(frozen) as f32;
        let scroll_left = self.cols.x(frozen) + self.scroll_x as f64;
        let first_col = self.cols.at_x(scroll_left).max(frozen);
        let mut last_col = first_col;
        while last_col < ncols
            && (self.cols.x(last_col) - scroll_left) < (cells_rect.width() - frozen_w) as f64
        {
            last_col += 1;
        }
        let col_screen_x = |cols: &Columns, c: usize| -> f32 {
            if c < frozen {
                cells_rect.left() + cols.x(c) as f32
            } else {
                cells_rect.left() + frozen_w + (cols.x(c) - scroll_left) as f32
            }
        };

        let expected = doc.index().read().reference_fields;
        let (s_r0, s_r1, s_c0, s_c1) = self.selection_box();
        let has_sel = self.anchor.is_some();
        let char_px = font_size * 0.5;
        let stripe = |index: u64| index % 2 == 1;

        // Rows.
        let cell_painter = painter.with_clip_rect(cells_rect);
        let mut hovered: Option<(usize, usize)> = None;
        let mut editor_rect: Option<Rect> = None;
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let rows = std::mem::take(&mut self.rows);
        for (ri, g) in rows.iter().enumerate() {
            let y = cells_rect.top() + ri as f32 * row_h - self.top_px;
            let row_rect =
                Rect::from_min_size(pos2(cells_rect.left(), y), vec2(cells_rect.width(), row_h));
            let span = g.row.span();
            let ragged =
                expected.is_some_and(|e| span.is_some_and(|s| s.fields != 0 && s.fields != e));
            if stripe(g.index) {
                cell_painter.rect_filled(row_rect, CornerRadius::ZERO, colors.stripe);
            }
            if ragged {
                cell_painter.rect_filled(row_rect, CornerRadius::ZERO, colors.ragged);
            }
            for (a, b) in [(0usize, frozen), (first_col, last_col)] {
                if a >= b {
                    continue;
                }
                let max_chars =
                    ((self.cols.width(a).max(self.cols.width(b - 1)) / char_px) as usize + 2)
                        .clamp(8, 1000);
                let cells = doc
                    .display_cells(&g.row, a..b, max_chars)
                    .unwrap_or_default();
                for (k, (cell, edited)) in cells.iter().enumerate() {
                    let c = a + k;
                    let x = col_screen_x(&self.cols, c);
                    let w = self.cols.width(c);
                    let rect = Rect::from_min_size(pos2(x, y), vec2(w, row_h));
                    if c >= frozen && rect.right() < cells_rect.left() + frozen_w {
                        continue;
                    }
                    if c < frozen {
                        // Frozen cells are opaque over scrolled ones.
                        cell_painter.rect_filled(
                            rect,
                            CornerRadius::ZERO,
                            if stripe(g.index) {
                                colors.stripe
                            } else {
                                colors.background
                            },
                        );
                        if ragged {
                            cell_painter.rect_filled(rect, CornerRadius::ZERO, colors.ragged);
                        }
                    }
                    if has_sel && g.index >= s_r0 && g.index <= s_r1 && c >= s_c0 && c <= s_c1 {
                        cell_painter.rect_filled(rect, CornerRadius::ZERO, colors.selection);
                    }
                    let marks = match &self.highlight {
                        Some(m) if m.wants_column(c) && !cell.text.is_empty() => {
                            m.ranges(&cell.text, 16)
                        }
                        _ => Vec::new(),
                    };
                    self.paint_cell(
                        &cell_painter,
                        rect,
                        cell,
                        &font,
                        colors,
                        self.cols.kind(c).is_numeric(),
                        &marks,
                    );
                    if *edited {
                        let tri = Rect::from_min_size(rect.min + vec2(1.0, 1.0), vec2(5.0, 5.0));
                        cell_painter.rect_filled(tri, CornerRadius::ZERO, colors.edited);
                    }
                    if pointer.is_some_and(|p| rect.contains(p) && cells_rect.contains(p)) {
                        hovered = Some((ri, c));
                    }
                    if let Some(e) = &self.editor
                        && e.row.pos == g.row.pos
                        && e.col == c
                    {
                        editor_rect = Some(rect);
                    }
                }
            }
            if g.index == self.cursor_index && self.editor.is_none() {
                let x0 = col_screen_x(&self.cols, self.cursor_col);
                let r =
                    Rect::from_min_size(pos2(x0, y), vec2(self.cols.width(self.cursor_col), row_h));
                cell_painter.rect_stroke(
                    r.shrink(1.0),
                    CornerRadius::ZERO,
                    Stroke::new(2.0, colors.cursor),
                    StrokeKind::Inside,
                );
            }
        }
        self.rows = rows;
        self.out.painted_rows = self.rows.len();
        if self.rows.is_empty() {
            let msg = if loading { "Loading…" } else { "(no rows)" };
            cell_painter.text(
                cells_rect.center_top() + vec2(0.0, 30.0),
                Align2::CENTER_CENTER,
                msg,
                font.clone(),
                colors.dim,
            );
        } else if loading {
            let y = cells_rect.top() + self.rows.len() as f32 * row_h - self.top_px;
            cell_painter.text(
                pos2(cells_rect.left() + 8.0, y + row_h * 0.5),
                Align2::LEFT_CENTER,
                "Scanning a very large record…",
                font.clone(),
                colors.dim,
            );
        }

        // Vertical grid lines.
        for c in (0..frozen).chain(first_col..last_col) {
            let x = col_screen_x(&self.cols, c) + self.cols.width(c);
            if x > cells_rect.left() && x < cells_rect.right() {
                painter.vline(
                    x,
                    full.top()..=cells_rect.bottom(),
                    Stroke::new(1.0, colors.line),
                );
            }
        }
        if frozen > 0 {
            painter.vline(
                cells_rect.left() + frozen_w,
                full.top()..=cells_rect.bottom(),
                Stroke::new(2.0, colors.line),
            );
        }

        // Header row.
        let header_rect = Rect::from_min_max(
            pos2(cells_rect.left(), full.top()),
            pos2(cells_rect.right(), full.top() + header_h),
        );
        let header_painter = painter.with_clip_rect(header_rect);
        header_painter.rect_filled(header_rect, CornerRadius::ZERO, colors.header_bg);
        self.header_names.clear();
        let header_font = FontId::new(font_size, egui::FontFamily::Proportional);
        for c in (0..frozen).chain(first_col..last_col) {
            let x = col_screen_x(&self.cols, c);
            let w = self.cols.width(c);
            let rect = Rect::from_min_size(pos2(x, full.top()), vec2(w, header_h));
            if c >= frozen && rect.right() < cells_rect.left() + frozen_w {
                continue;
            }
            if c < frozen {
                header_painter.rect_filled(rect, CornerRadius::ZERO, colors.header_bg);
            }
            let name = doc
                .column_name(c)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Column {}", c + 1));
            let sorted = self.sort_mark.filter(|m| m.0 == c).map(|m| m.1);
            let mark_w = if sorted.is_some() { 12.0 } else { 0.0 };
            let job = single_line(
                &name,
                header_font.clone(),
                colors.header_text,
                w - 2.0 * CELL_PAD - mark_w,
            );
            if let Some(descending) = sorted {
                // A small triangle: apex up for ascending, down for descending.
                let (cx, cy) = (rect.right() - CELL_PAD - 4.0, rect.center().y);
                let (base, apex) = if descending {
                    (cy - 2.5, cy + 2.5)
                } else {
                    (cy + 2.5, cy - 2.5)
                };
                header_painter.add(egui::Shape::convex_polygon(
                    vec![pos2(cx - 4.0, base), pos2(cx + 4.0, base), pos2(cx, apex)],
                    colors.header_text,
                    Stroke::NONE,
                ));
            }
            let galley = header_painter.layout_job(job);
            header_painter.galley(
                pos2(
                    rect.left() + CELL_PAD,
                    rect.center().y - galley.size().y * 0.5,
                ),
                galley,
                colors.header_text,
            );
            header_painter.vline(rect.right(), rect.y_range(), Stroke::new(1.0, colors.line));
            self.header_names.push((c, name));
        }
        painter.hline(
            full.left()..=cells_rect.right(),
            full.top() + header_h,
            Stroke::new(1.0, colors.line),
        );

        // Gutter with row numbers.
        let gutter = Rect::from_min_max(
            pos2(full.left(), full.top() + header_h),
            pos2(full.left() + gutter_w, cells_rect.bottom()),
        );
        let gutter_painter = painter.with_clip_rect(gutter);
        gutter_painter.rect_filled(gutter, CornerRadius::ZERO, colors.gutter_bg);
        for (ri, g) in self.rows.iter().enumerate() {
            let y = cells_rect.top() + ri as f32 * row_h - self.top_px;
            let n = g.label + 1;
            let inserted = matches!(g.row.kind, LKind::New(_));
            let label = if inserted {
                format!("+{}", group(n))
            } else if g.exact {
                group(n)
            } else {
                format!("~{}", group(n))
            };
            let ragged = expected
                .is_some_and(|e| g.row.span().is_some_and(|s| s.fields != 0 && s.fields != e));
            let color = if inserted {
                colors.edited
            } else if !g.exact {
                colors.approximate
            } else if ragged {
                Color32::from_rgb(0xE0, 0x8A, 0x1E)
            } else {
                colors.gutter_text
            };
            gutter_painter.text(
                pos2(gutter.right() - 8.0, y + row_h * 0.5),
                Align2::RIGHT_CENTER,
                label,
                FontId::monospace(font_size * 0.86),
                color,
            );
        }
        painter.vline(
            gutter.right(),
            full.top()..=cells_rect.bottom(),
            Stroke::new(1.0, colors.line),
        );
        let corner = Rect::from_min_size(full.min, vec2(gutter_w, header_h));
        painter.rect_filled(corner, CornerRadius::ZERO, colors.header_bg);

        // Column resizing by dragging header borders.
        self.handle_header(
            ui,
            &response,
            header_rect,
            &col_screen_x,
            frozen,
            first_col,
            last_col,
        );
        if response.secondary_clicked()
            && let Some(p) = pointer
        {
            self.menu_col = (0..frozen).chain(first_col..last_col).find(|&c| {
                let x = col_screen_x(&self.cols, c);
                p.x >= x && p.x < x + self.cols.width(c)
            });
            if let Some(c) = self.menu_col
                && cells_rect.contains(p)
            {
                // Right-click in a cell also moves the cursor there.
                self.cursor_col = c;
            }
            if let Some((ri, _)) = hovered
                && let Some(g) = self.rows.get(ri).copied()
                && !(self.anchor.is_some() && g.index >= s_r0 && g.index <= s_r1)
            {
                self.anchor = None;
                self.set_cursor_row(&g);
            }
        }
        if let Some(c) = self.menu_col {
            let name = self
                .header_names
                .iter()
                .find(|(i, _)| *i == c)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            response.context_menu(|ui| self.context_menu(ui, c, &name));
        }

        // Scrollbars.
        self.vscrollbar(ui, doc, full, cells_rect, colors);
        self.hscrollbar(ui, full, cells_rect, frozen_w, colors);

        // Clicks, double clicks and tooltips.
        if let Some((ri, c)) = hovered
            && let Some(g) = self.rows.get(ri).copied()
            && self.editor.is_none()
        {
            if response.clicked() {
                let shift = ui.input(|i| i.modifiers.shift);
                if shift {
                    let m = self.cursor_mark();
                    self.anchor.get_or_insert(m);
                } else {
                    self.anchor = None;
                }
                self.set_cursor_row(&g);
                self.cursor_col = c;
            }
            if response.double_clicked() {
                self.set_cursor_row(&g);
                self.cursor_col = c;
                if self.read_only {
                    self.out.open_cell = Some((g.row, c));
                } else {
                    self.start_edit(doc, None);
                }
            }
            if !response.dragged() && ui.input(|i| i.pointer.time_since_last_movement() > 0.4) {
                let tip = doc.value(&g.row, c, TOOLTIP_CHARS * 4).ok();
                if let Some(text) = tip
                    && !text.is_empty()
                    && (text.chars().count() as f32 * char_px * 1.15 > self.cols.width(c)
                        || text.contains(['\n', '\r', '\t']))
                {
                    let shown: String = text.chars().take(TOOLTIP_CHARS).collect();
                    let more = text.chars().count() > TOOLTIP_CHARS;
                    response.clone().on_hover_ui_at_pointer(|ui| {
                        ui.set_max_width(520.0);
                        ui.label(egui::RichText::new(shown).font(font.clone()));
                        if more {
                            ui.weak("… (open the cell to see all of it)");
                        }
                    });
                }
            }
        }

        // The inline editor.
        if let Some(rect) = editor_rect {
            self.show_editor(ui, doc, rect, &font, visible);
        } else if self.editor.is_some()
            && !self
                .rows
                .iter()
                .any(|g| Some(g.row.pos) == self.editor.as_ref().map(|e| e.row.pos))
        {
            // Scrolled away: keep the edit.
            self.commit_edit(doc);
        }

        if doc.cells_pending() {
            ui.ctx().request_repaint();
        }
        self.out.cursor = Some(self.cursor);
        self.out.cursor_row = if self.is_list() {
            self.rows
                .iter()
                .find(|g| g.index == self.cursor_index)
                .map_or(
                    self.cursor.rec.record.saturating_sub(doc.header_records()),
                    |g| g.label,
                )
        } else {
            self.cursor_index
        };
        self.out.cursor_exact = self.cursor.rec.exact;
        self.out.cursor_col = self.cursor_col;
        self.out.columns = ncols;
        self.out.editing = self.editor.is_some();
        // Not while a mouse drag is still changing the selection.
        let settled = !ui.input(|i| i.pointer.any_down());
        self.out.selection = self.selection_stats(doc, settled);
        std::mem::take(&mut self.out)
    }

    /// Summary of the selection (cached until it or the data changes).
    fn selection_stats(&mut self, doc: &mut Document, settled: bool) -> Option<SelStats> {
        self.anchor?;
        let (r0, r1, c0, c1) = self.selection_box();
        let (rows, cols) = (r1 - r0 + 1, (c1 - c0 + 1) as u64);
        if rows * cols < 2 {
            return None;
        }
        let key = (
            r0,
            r1,
            c0,
            c1,
            self.is_list(),
            doc.generation(),
            doc.overlay.generation(),
        );
        if let Some((k, v)) = self.stats
            && k == key
        {
            return Some(v);
        }
        if !settled {
            return None;
        }
        let mut s = SelStats {
            rows,
            cols,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            ..SelStats::default()
        };
        if rows * cols <= MAX_STATS_CELLS {
            // Rows still loading from a slow drive: try again next frame.
            let list = self.selected_rows(doc, MAX_STATS_CELLS).ok()?;
            let mut filled = 0;
            for row in &list {
                let cells = doc.display_cells(row, c0..c1 + 1, 64).ok()?;
                for (cell, _) in cells {
                    if cell.missing || cell.text.is_empty() {
                        continue;
                    }
                    filled += 1;
                    if let Some(x) = infer::parse_number(&cell.text) {
                        s.numbers += 1;
                        s.sum += x;
                        s.min = s.min.min(x);
                        s.max = s.max.max(x);
                    }
                }
            }
            s.filled = Some(filled);
        }
        self.stats = Some((key, s));
        Some(s)
    }

    fn set_cursor_row(&mut self, g: &GRow) {
        if self.is_list() {
            self.lcur = g.index as usize;
        }
        self.cursor = g.row.pos;
        self.cursor_index = g.index;
    }

    fn show_editor(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        rect: Rect,
        font: &FontId,
        visible: usize,
    ) {
        let Some(e) = &mut self.editor else { return };
        // Keys that end editing are taken before the text field sees them.
        let typing = e.typing;
        let (enter, tab, esc, up, down, left, right, shift) = ui.input_mut(|i| {
            let shift = i.modifiers.shift;
            let m = i.modifiers;
            (
                i.consume_key(m, Key::Enter),
                i.consume_key(m, Key::Tab),
                i.consume_key(m, Key::Escape),
                i.consume_key(m, Key::ArrowUp),
                i.consume_key(m, Key::ArrowDown),
                typing && i.consume_key(m, Key::ArrowLeft),
                typing && i.consume_key(m, Key::ArrowRight),
                shift,
            )
        });
        if esc {
            self.editor = None;
            return;
        }
        let edit_rect = Rect::from_min_size(rect.min, vec2(rect.width().max(220.0), rect.height()));
        let id = ui.scope_id().with("katak-cell-editor");
        let r = ui.put(
            edit_rect,
            egui::TextEdit::singleline(&mut e.text)
                .id(id)
                .font(font.clone())
                .margin(egui::Margin::symmetric(4, 2)),
        );
        if e.focus {
            r.request_focus();
            e.focus = false;
        }
        let moves: Option<(i64, i64)> = if enter {
            Some((if shift { -1 } else { 1 }, 0))
        } else if tab {
            Some((0, if shift { -1 } else { 1 }))
        } else if up {
            Some((-1, 0))
        } else if down {
            Some((1, 0))
        } else if left {
            Some((0, -1))
        } else if right {
            Some((0, 1))
        } else if r.lost_focus() {
            Some((0, 0))
        } else {
            None
        };
        if let Some((dr, dc)) = moves {
            self.commit_edit(doc);
            if dr != 0 {
                self.move_cursor(doc, dr);
                self.ensure_cursor_visible(doc, visible);
            }
            if dc != 0 {
                self.cursor_col = (self.cursor_col as i64 + dc)
                    .clamp(0, self.cols.len().max(1) as i64 - 1)
                    as usize;
            }
            ui.memory_mut(|m| m.request_focus(ui.scope_id().with("katak-grid")));
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, c: usize, name: &str) {
        ui.strong(format!("Column {}: {}", c + 1, name));
        ui.separator();
        let ro = self.read_only;
        if ui.button("Copy  Ctrl+C").clicked() {
            self.actions.push(EditAction::Copy);
            ui.close();
        }
        if ui
            .add_enabled(!ro, egui::Button::new("Paste  Ctrl+V"))
            .clicked()
        {
            // The clipboard arrives as a paste event; ask the app for it.
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.close();
        }
        if ui
            .add_enabled(!ro, egui::Button::new("Clear  Delete"))
            .clicked()
        {
            self.actions.push(EditAction::Clear);
            ui.close();
        }
        if ui
            .add_enabled(!ro, egui::Button::new("Fill down  Ctrl+D"))
            .clicked()
        {
            self.actions.push(EditAction::FillDown);
            ui.close();
        }
        ui.separator();
        ui.add_enabled_ui(!ro, |ui| {
            ui.menu_button("Rows", |ui| {
                if ui.button("Insert row above").clicked() {
                    self.actions.push(EditAction::InsertRowAbove);
                    ui.close();
                }
                if ui.button("Insert row below").clicked() {
                    self.actions.push(EditAction::InsertRowBelow);
                    ui.close();
                }
                if ui.button("Delete selected rows").clicked() {
                    self.actions.push(EditAction::DeleteRows);
                    ui.close();
                }
            });
            ui.menu_button("Column", |ui| {
                if ui.button("Rename…").clicked() {
                    self.out.rename_column = Some(c);
                    ui.close();
                }
                if ui.button("Insert column left").clicked() {
                    self.cursor_col = c;
                    self.actions.push(EditAction::InsertColumnLeft);
                    ui.close();
                }
                if ui.button("Insert column right").clicked() {
                    self.cursor_col = c;
                    self.actions.push(EditAction::InsertColumnRight);
                    ui.close();
                }
                if ui.button("Move left").clicked() {
                    self.cursor_col = c;
                    self.actions.push(EditAction::MoveColumnLeft);
                    ui.close();
                }
                if ui.button("Move right").clicked() {
                    self.cursor_col = c;
                    self.actions.push(EditAction::MoveColumnRight);
                    ui.close();
                }
                if ui.button("Delete column").clicked() {
                    self.cursor_col = c;
                    self.actions.push(EditAction::DeleteColumn);
                    ui.close();
                }
            });
        });
        ui.separator();
        let how = match self.cols.kind(c) {
            ColumnType::Integer | ColumnType::Float => "numbers",
            ColumnType::Date => "dates",
            ColumnType::Text => "text",
        };
        if ui
            .button(format!("Sort ascending ({how})"))
            .on_hover_text(
                "Shows the rows sorted; the file is not changed. Export writes them in this order.",
            )
            .clicked()
        {
            self.out.sort = Some((c, false));
            ui.close();
        }
        if ui.button(format!("Sort descending ({how})")).clicked() {
            self.out.sort = Some((c, true));
            ui.close();
        }
        if self.sort_mark.is_some() && ui.button("Clear sort").clicked() {
            self.out.clear_sort = true;
            ui.close();
        }
        if ui.button("Filter this column…").clicked() {
            self.out.filter_column = Some(c);
            ui.close();
        }
        if ui.button("Auto-fit width").clicked() {
            self.commands.push(GridCommand::AutoFit(c));
            ui.close();
        }
        if ui.button("Freeze columns up to here").clicked() {
            self.frozen = c + 1;
            ui.close();
        }
        if self.frozen > 0 && ui.button("Unfreeze columns").clicked() {
            self.frozen = 0;
            ui.close();
        }
        ui.menu_button("Type (alignment and sort order only)", |ui| {
            let inferred = self.cols.types.get(c).copied().unwrap_or_default();
            let current = self.cols.type_overrides.get(c).copied().flatten();
            if ui
                .radio(current.is_none(), format!("Auto ({inferred})"))
                .clicked()
            {
                self.cols.set_override(c, None);
                ui.close();
            }
            for t in ColumnType::ALL {
                if ui.radio(current == Some(t), t.label()).clicked() {
                    self.cols.set_override(c, Some(t));
                    ui.close();
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_cell(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        cell: &Cell,
        font: &FontId,
        colors: &GridColors,
        numeric: bool,
        marks: &[Range<usize>],
    ) {
        if cell.missing {
            return;
        }
        let (text, color) = if cell.unknown {
            ("…".to_string(), colors.dim)
        } else {
            (cell.text.clone(), colors.text)
        };
        if text.is_empty() {
            return;
        }
        let avail = rect.width() - 2.0 * CELL_PAD;
        if avail <= 2.0 {
            return;
        }
        let job = if marks.is_empty() {
            single_line(&text, font.clone(), color, avail)
        } else {
            highlighted_line(&text, marks, font.clone(), color, colors.highlight, avail)
        };
        let galley = painter.layout_job(job);
        let y = rect.center().y - galley.size().y * 0.5;
        let x = if numeric {
            rect.right() - CELL_PAD - galley.size().x
        } else {
            rect.left() + CELL_PAD
        };
        painter.galley(pos2(x, y), galley, color);
        if cell.truncated {
            let r = Rect::from_min_size(pos2(rect.right() - 5.0, rect.top() + 2.0), vec2(3.0, 3.0));
            painter.rect_filled(r, CornerRadius::ZERO, colors.dim);
        }
    }

    fn handle_input(
        &mut self,
        ui: &mut Ui,
        response: &Response,
        doc: &mut Document,
        visible: usize,
        cells: Rect,
    ) {
        let hovered = response.hovered() || response.contains_pointer();
        if hovered {
            let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            if delta != Vec2::ZERO {
                if shift && delta.x == 0.0 {
                    self.scroll_x = (self.scroll_x - delta.y).max(0.0);
                } else {
                    self.top_px -= delta.y;
                    self.scroll_x = (self.scroll_x - delta.x).max(0.0);
                }
            }
        }
        if !response.has_focus() && ui.memory(|m| m.focused().is_some()) {
            return;
        }
        let page = visible.saturating_sub(2).max(1) as i64;
        let ncols = self.cols.len().max(1);
        let mut moved = false;
        let mut moved_col = false;
        let events = ui.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Copy => self.actions.push(EditAction::Copy),
                egui::Event::Cut => self.actions.push(EditAction::Cut),
                egui::Event::Paste(text) => self.actions.push(EditAction::Paste(text)),
                egui::Event::Text(t) if !t.chars().any(char::is_control) => {
                    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                    if !ctrl {
                        self.start_edit(doc, Some(t));
                        return;
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let before = self.cursor_mark();
                    let extend = modifiers.shift;
                    let ctrl = modifiers.command;
                    match key {
                        Key::ArrowDown => {
                            self.move_cursor(doc, 1);
                            moved = true;
                        }
                        Key::ArrowUp => {
                            self.move_cursor(doc, -1);
                            moved = true;
                        }
                        Key::Enter => {
                            self.move_cursor(doc, if extend { -1 } else { 1 });
                            self.anchor = None;
                            moved = true;
                            continue;
                        }
                        Key::PageDown => {
                            self.move_cursor(doc, page);
                            self.move_top(doc, page);
                            moved = true;
                        }
                        Key::PageUp => {
                            self.move_cursor(doc, -page);
                            self.move_top(doc, -page);
                            moved = true;
                        }
                        Key::ArrowRight | Key::Tab if !(key == Key::Tab && modifiers.shift) => {
                            self.cursor_col = (self.cursor_col + 1).min(ncols - 1);
                            moved_col = true;
                        }
                        Key::ArrowLeft | Key::Tab => {
                            self.cursor_col = self.cursor_col.saturating_sub(1);
                            moved_col = true;
                        }
                        Key::Home if ctrl => {
                            self.commands.push(GridCommand::Top);
                            self.cursor_col = 0;
                            moved_col = true;
                        }
                        Key::End if ctrl => {
                            self.commands.push(GridCommand::Bottom);
                            moved_col = true;
                        }
                        Key::Home => {
                            self.cursor_col = 0;
                            moved_col = true;
                        }
                        Key::End => {
                            self.cursor_col = ncols - 1;
                            moved_col = true;
                        }
                        Key::F2 => {
                            self.actions.push(EditAction::StartEdit);
                            continue;
                        }
                        Key::Delete => {
                            self.actions.push(EditAction::Clear);
                            continue;
                        }
                        Key::Backspace => {
                            self.start_edit(doc, Some(String::new()));
                            return;
                        }
                        Key::D if ctrl => {
                            self.actions.push(EditAction::FillDown);
                            continue;
                        }
                        Key::A if ctrl => {
                            self.actions.push(EditAction::SelectAll);
                            continue;
                        }
                        Key::Escape => {
                            self.anchor = None;
                            continue;
                        }
                        _ => continue,
                    }
                    if extend && key != Key::Tab {
                        self.anchor.get_or_insert(before);
                    } else {
                        self.anchor = None;
                    }
                }
                _ => {}
            }
        }
        if moved {
            self.ensure_cursor_visible(doc, visible);
        }
        if moved_col {
            self.ensure_col_visible(cells.width());
        }
    }

    fn ensure_col_visible(&mut self, view_w: f32) {
        self.cols.rebuild();
        let c = self.cursor_col;
        if c < self.frozen {
            return;
        }
        let frozen_w = self.cols.x(self.frozen) as f32;
        let left = (self.cols.x(c) - self.cols.x(self.frozen)) as f32;
        let right = left + self.cols.width(c);
        let avail = view_w - frozen_w;
        if left < self.scroll_x {
            self.scroll_x = left;
        } else if right > self.scroll_x + avail {
            self.scroll_x = (right - avail).max(0.0);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_header(
        &mut self,
        ui: &mut Ui,
        grid: &Response,
        header: Rect,
        col_x: &dyn Fn(&Columns, usize) -> f32,
        frozen: usize,
        first: usize,
        last: usize,
    ) {
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let mut near_border = None;
        if let Some(p) = pointer
            && header.contains(p)
        {
            for c in (0..frozen).chain(first..last) {
                let x = col_x(&self.cols, c) + self.cols.width(c);
                if (p.x - x).abs() <= 4.0 {
                    near_border = Some(c);
                }
            }
        }
        if near_border.is_some() || self.resize.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeColumn);
        }
        let (pressed, down, released, double) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer
                    .button_double_clicked(egui::PointerButton::Primary),
            )
        });
        if let (Some(c), true) = (near_border, double) {
            self.commands.push(GridCommand::AutoFit(c));
            self.resize = None;
            return;
        }
        if pressed && let (Some(c), Some(p)) = (near_border, pointer) {
            self.resize = Some((c, self.cols.width(c), p.x));
        }
        if let Some((c, w0, x0)) = self.resize {
            if down && let Some(p) = pointer {
                self.cols.set_width(c, w0 + p.x - x0);
            }
            if released || !down {
                self.resize = None;
            }
            return;
        }
        // Click on a header cell moves the cursor to that column.
        if grid.clicked()
            && let Some(p) = pointer
            && header.contains(p)
        {
            for c in (0..frozen).chain(first..last) {
                let x = col_x(&self.cols, c);
                if p.x >= x && p.x < x + self.cols.width(c) {
                    self.cursor_col = c;
                }
            }
        }
    }

    fn vscrollbar(
        &mut self,
        ui: &mut Ui,
        doc: &Document,
        full: Rect,
        cells: Rect,
        colors: &GridColors,
    ) {
        let track = Rect::from_min_max(
            pos2(cells.right(), cells.top()),
            pos2(full.right(), cells.bottom()),
        );
        let (a, b) = (
            doc.data_start(),
            doc.source().len().max(doc.data_start() + 1),
        );
        let span = (b - a) as f64;
        let (frac, thumb_frac) = match self.list() {
            Some(l) => {
                let n = l.lock().rows.len().max(1) as f32;
                (
                    (self.ltop as f32 / n).clamp(0.0, 1.0),
                    (self.rows.len() as f32 / n).clamp(0.0, 1.0),
                )
            }
            None => {
                let top = self.top.rec.offset;
                let frac = ((top.saturating_sub(a)) as f64 / span).clamp(0.0, 1.0) as f32;
                let shown = self
                    .rows
                    .iter()
                    .rev()
                    .find_map(|g| g.row.span())
                    .map_or(0, |s| s.end.saturating_sub(top));
                (frac, (shown as f64 / span).clamp(0.0, 1.0) as f32)
            }
        };
        let thumb_h = (track.height() * thumb_frac).max(24.0).min(track.height());
        let travel = (track.height() - thumb_h).max(1.0);
        let thumb = Rect::from_min_size(
            pos2(track.left() + 2.0, track.top() + travel * frac),
            vec2(SCROLLBAR - 4.0, thumb_h),
        );
        let painter = ui.painter_at(track);
        painter.rect_filled(track, CornerRadius::ZERO, colors.header_bg);
        let resp = ui.interact(
            track,
            ui.scope_id().with("katak-vscroll"),
            Sense::click_and_drag(),
        );
        let active = resp.dragged() || resp.hovered();
        painter.rect_filled(
            thumb,
            CornerRadius::same(4),
            if active {
                colors.gutter_text
            } else {
                colors.dim
            },
        );
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.vdrag = Some(if thumb.contains(p) {
                p.y - thumb.top()
            } else {
                thumb_h * 0.5
            });
        }
        if resp.dragged()
            && let (Some(grab), Some(p)) = (self.vdrag, resp.interact_pointer_pos())
        {
            let f = ((p.y - grab - track.top()) / travel).clamp(0.0, 1.0);
            if (f - frac).abs() > 0.5 / track.height() {
                self.commands.push(GridCommand::JumpFraction(f));
                ui.ctx().request_repaint();
            }
        }
        if resp.drag_stopped() {
            self.vdrag = None;
        }
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let page = (cells.height() / (self.font_size * 1.65)) as i64;
            self.commands
                .push(GridCommand::ScrollRows(if p.y < thumb.top() {
                    -page
                } else {
                    page
                }));
        }
    }

    fn hscrollbar(
        &mut self,
        ui: &mut Ui,
        full: Rect,
        cells: Rect,
        frozen_w: f32,
        colors: &GridColors,
    ) {
        let track = Rect::from_min_max(
            pos2(cells.left() + frozen_w, cells.bottom()),
            pos2(cells.right(), full.bottom()),
        );
        if track.width() <= 10.0 {
            return;
        }
        let total =
            (self.cols.x(self.cols.len()) - self.cols.x(self.frozen.min(self.cols.len()))) as f32;
        let view = cells.width() - frozen_w;
        let painter = ui.painter_at(Rect::from_min_max(
            pos2(full.left(), cells.bottom()),
            full.max,
        ));
        painter.rect_filled(
            Rect::from_min_max(pos2(full.left(), cells.bottom()), full.max),
            CornerRadius::ZERO,
            colors.header_bg,
        );
        if total <= view {
            return;
        }
        let thumb_w = (track.width() * view / total).clamp(24.0, track.width());
        let travel = (track.width() - thumb_w).max(1.0);
        let max_scroll = (total - view).max(1.0);
        let frac = (self.scroll_x / max_scroll).clamp(0.0, 1.0);
        let thumb = Rect::from_min_size(
            pos2(track.left() + travel * frac, track.top() + 2.0),
            vec2(thumb_w, SCROLLBAR - 4.0),
        );
        let resp = ui.interact(
            track,
            ui.scope_id().with("katak-hscroll"),
            Sense::click_and_drag(),
        );
        painter.rect_filled(
            thumb,
            CornerRadius::same(4),
            if resp.dragged() || resp.hovered() {
                colors.gutter_text
            } else {
                colors.dim
            },
        );
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.hdrag = Some((
                if thumb.contains(p) {
                    p.x - thumb.left()
                } else {
                    thumb_w * 0.5
                },
                0.0,
            ));
        }
        if resp.dragged()
            && let (Some((grab, _)), Some(p)) = (self.hdrag, resp.interact_pointer_pos())
        {
            let f = ((p.x - grab - track.left()) / travel).clamp(0.0, 1.0);
            self.scroll_x = f * max_scroll;
        }
        if resp.drag_stopped() {
            self.hdrag = None;
        }
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.scroll_x = (self.scroll_x + if p.x < thumb.left() { -view } else { view })
                .clamp(0.0, max_scroll);
        }
    }
}

/// Appends a value as a TSV field (Excel-compatible quoting).
fn tsv_field(v: &str, out: &mut String) {
    if v.contains(['\t', '\n', '\r', '"']) {
        out.push('"');
        out.push_str(&v.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(v);
    }
}

/// Parses TSV as Excel writes it to the clipboard: tab-separated fields,
/// one row per line, quoted fields may contain tabs, line breaks and `""`.
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut at_start = true;
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            '\t' => {
                row.push(std::mem::take(&mut field));
                at_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_start = true;
            }
            c => {
                field.push(c);
                at_start = false;
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A single-line layout job elided with `…` at `max_width`.
pub fn single_line(text: &str, font: FontId, color: Color32, max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, color));
    job.wrap = TextWrapping {
        max_width: max_width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job.break_on_newline = false;
    job
}

/// Like [`single_line`], with `marks` (byte ranges of `text`) highlighted.
pub fn highlighted_line(
    text: &str,
    marks: &[Range<usize>],
    font: FontId,
    color: Color32,
    bg: Color32,
    max_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = TextFormat::simple(font.clone(), color);
    let mark = TextFormat {
        background: bg,
        ..TextFormat::simple(font, color)
    };
    let mut at = 0;
    for r in marks {
        let (a, b) = (r.start.min(text.len()), r.end.min(text.len()));
        if a < at || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        job.append(&text[at..a], 0.0, plain.clone());
        job.append(&text[a..b], 0.0, mark.clone());
        at = b;
    }
    job.append(&text[at..], 0.0, plain);
    job.wrap = TextWrapping {
        max_width: max_width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job.break_on_newline = false;
    job
}

/// `1234567` -> `1,234,567`.
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(12_400_000), "12,400,000");
    }

    #[test]
    fn columns_prefix() {
        let mut c = Columns::default();
        c.ensure(3);
        c.set_width(1, 50.0);
        c.rebuild();
        assert_eq!(c.x(0), 0.0);
        assert_eq!(c.x(2), (DEFAULT_COL_WIDTH + 50.0) as f64);
        assert_eq!(c.at_x(DEFAULT_COL_WIDTH as f64 + 10.0), 1);
    }

    #[test]
    fn tsv_roundtrip() {
        let values = [
            vec!["a", "b\tc", "say \"hi\""],
            vec!["multi\nline", "", "x"],
        ];
        let mut text = String::new();
        for row in &values {
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    text.push('\t');
                }
                tsv_field(v, &mut text);
            }
            text.push('\n');
        }
        let parsed = parse_tsv(&text);
        assert_eq!(
            parsed,
            values
                .iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_tsv("1\t2\r\n3\t4"),
            vec![vec!["1", "2"], vec!["3", "4"]]
        );
        assert!(parse_tsv("").is_empty());
    }
}
