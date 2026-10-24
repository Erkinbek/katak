//! The virtualized grid.
//!
//! Only the visible rows *and* the visible columns are fetched, decoded,
//! laid out and painted each frame. Vertical position is a record position
//! (`RowPos`) plus a sub-row pixel offset; the vertical scrollbar maps to
//! byte offsets, so it behaves the same before and after indexing.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, EventFilter, FontId, Key, Rect, Response,
    Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use eframe::epaint::text::{LayoutJob, TextFormat, TextWrapping};
use katak_core::document::{Cell, Document};
use katak_core::infer::{self, ColumnType};
use katak_core::locate::{FetchEnd, RowPos};
use katak_core::scan::RecSpan;

use crate::theme::GridColors;

const MIN_COL_WIDTH: f32 = 28.0;
const MAX_AUTO_WIDTH: f32 = 360.0;
const DEFAULT_COL_WIDTH: f32 = 110.0;
const SCROLLBAR: f32 = 14.0;
const CELL_PAD: f32 = 6.0;
const TOOLTIP_CHARS: usize = 4000;

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
    /// Go to data row (0-based).
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

/// What the grid tells the app after a frame.
#[derive(Clone, Debug, Default)]
pub struct GridInfo {
    pub cursor: Option<RowPos>,
    pub cursor_col: usize,
    pub columns: usize,
    pub visible_rows: usize,
    pub loading: bool,
    pub double_clicked: Option<(RecSpan, usize)>,
    pub painted_rows: usize,
}

/// Per-tab grid state.
pub struct Grid {
    pub top: RowPos,
    top_px: f32,
    pub scroll_x: f32,
    pub cursor: RowPos,
    pub cursor_col: usize,
    pub anchor: Option<(RowPos, usize)>,
    pub frozen: usize,
    pub cols: Columns,
    generation: u64,
    initialized: bool,
    rows: Vec<(RowPos, RecSpan)>,
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
}

impl Grid {
    pub fn new() -> Self {
        let p = RowPos {
            offset: 0,
            record: 0,
            exact: true,
        };
        Grid {
            top: p,
            top_px: 0.0,
            scroll_x: 0.0,
            cursor: p,
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
        }
    }

    pub fn command(&mut self, c: GridCommand) {
        self.commands.push(c);
    }

    /// Resets position and recomputes column widths/types from the sample.
    fn reset(&mut self, doc: &mut Document) {
        self.generation = doc.generation();
        self.top = doc.first_data_row().unwrap_or(RowPos {
            offset: doc.data_start(),
            record: 0,
            exact: true,
        });
        self.top_px = 0.0;
        self.scroll_x = 0.0;
        self.cursor = self.top;
        self.cursor_col = 0;
        self.anchor = None;
        self.cols = Columns::default();
        self.autosize(doc);
        self.initialized = true;
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
        self.cols.widths = widths
            .iter()
            .map(|&n| (n as f32 * char_w + 2.0 * CELL_PAD + 4.0).clamp(48.0, MAX_AUTO_WIDTH))
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

    fn header_records(doc: &Document) -> u64 {
        doc.header_records()
    }

    fn clamp_top(&mut self, doc: &mut Document) {
        if self.top.record < Self::header_records(doc) {
            if let Ok(p) = doc.first_data_row() {
                self.top = p;
            }
            self.top_px = 0.0;
        }
    }

    fn move_top(&mut self, doc: &mut Document, delta: i64) {
        if delta > 0 {
            if let Ok(f) = doc.locator().records_from(self.top, delta as usize + 1)
                && let Some(last) = f.rows.get(delta as usize).or(f.rows.last())
            {
                self.top = last.0;
            }
        } else if delta < 0
            && let Ok(b) = doc.locator().records_before(self.top, (-delta) as usize)
            && let Some(first) = b.first()
        {
            self.top = first.0;
        }
        self.clamp_top(doc);
    }

    fn move_cursor(&mut self, doc: &mut Document, delta: i64) {
        let hdr = Self::header_records(doc);
        if delta > 0 {
            if let Ok(f) = doc.locator().records_from(self.cursor, delta as usize + 1)
                && let Some(r) = f.rows.get(delta as usize).or(f.rows.last())
            {
                self.cursor = r.0;
            }
        } else if delta < 0
            && let Ok(b) = doc.locator().records_before(self.cursor, (-delta) as usize)
            && let Some(r) = b.iter().find(|r| r.0.record >= hdr).or(b.first())
            && r.0.record >= hdr
        {
            self.cursor = r.0;
        }
    }

    fn goto(&mut self, doc: &mut Document, pos: RowPos, visible: usize) {
        self.cursor = pos;
        // Keep a little context above the target.
        self.top = pos;
        self.top_px = 0.0;
        let ctx = (visible / 3) as i64;
        self.move_top(doc, -ctx);
    }

    fn ensure_cursor_visible(&mut self, doc: &mut Document, visible: usize) {
        if self.cursor.offset < self.top.offset {
            self.top = self.cursor;
            self.top_px = 0.0;
        } else if visible > 1 {
            let full = visible.saturating_sub(1).max(1);
            if let Ok(f) = doc.locator().records_from(self.top, full) {
                let inside = f.rows.iter().any(|r| r.0.offset == self.cursor.offset);
                if !inside && f.end != FetchEnd::Budget {
                    self.top = self.cursor;
                    self.top_px = 0.0;
                    self.move_top(doc, -(full as i64 - 1));
                }
            }
        }
    }

    fn apply_commands(&mut self, doc: &mut Document, row_h: f32, visible: usize, view_w: f32) {
        let cmds = std::mem::take(&mut self.commands);
        for c in cmds {
            match c {
                GridCommand::GotoRow(n) => {
                    let hdr = Self::header_records(doc);
                    if let Ok(p) = doc.locator().locate_record(n + hdr) {
                        self.goto(doc, p, visible);
                    }
                }
                GridCommand::GotoOffset(off) => {
                    if let Ok(p) = doc.locator().locate_offset(off) {
                        self.goto(doc, p, visible);
                        self.clamp_top(doc);
                    }
                }
                GridCommand::GotoRecord(p) => self.goto(doc, p, visible),
                GridCommand::Top => {
                    if let Ok(p) = doc.first_data_row() {
                        self.top = p;
                        self.cursor = p;
                        self.top_px = 0.0;
                    }
                }
                GridCommand::Bottom => {
                    if let Some(last) = self.go_bottom(doc, visible) {
                        self.cursor = last;
                    }
                }
                GridCommand::ScrollRows(n) => self.move_top(doc, n),
                GridCommand::ScrollX(px) => self.scroll_x = (self.scroll_x + px).max(0.0),
                GridCommand::JumpFraction(f) => {
                    let (a, b) = (doc.data_start(), doc.source().len());
                    let off = a + ((b - a) as f64 * f.clamp(0.0, 1.0) as f64) as u64;
                    if f >= 0.999 {
                        self.go_bottom(doc, visible);
                    } else if let Ok(p) = doc.locator().locate_offset(off) {
                        self.top = p;
                    }
                    self.top_px = 0.0;
                    self.clamp_top(doc);
                }
                GridCommand::AutoFit(c) => self.autofit(doc, c),
            }
        }
        // Normalize the sub-row offset into whole rows.
        if self.top_px >= row_h {
            let n = (self.top_px / row_h).floor() as i64;
            let before = self.top;
            self.move_top(doc, n);
            self.top_px = if self.top == before {
                0.0
            } else {
                self.top_px - n as f32 * row_h
            };
        } else if self.top_px < 0.0 {
            let n = (-self.top_px / row_h).ceil() as i64;
            let before = self.top;
            self.move_top(doc, -n);
            self.top_px = if self.top == before {
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

    /// Shows the last page; returns the last row.
    fn go_bottom(&mut self, doc: &mut Document, visible: usize) -> Option<RowPos> {
        let end = doc.locator().end_pos();
        let rows = doc
            .locator()
            .records_before(end, visible.saturating_sub(1).max(1))
            .ok()?;
        let (first, last) = (rows.first()?.0, rows.last()?.0);
        self.top = first;
        self.top_px = 0.0;
        self.clamp_top(doc);
        Some(last)
    }

    fn autofit(&mut self, doc: &mut Document, c: usize) {
        let mut chars = 4usize;
        let rows = self.rows.clone();
        for (_, r) in rows.iter().take(200) {
            if let Ok(cells) = doc.cells(r, c..c + 1, 400) {
                chars = chars.max(cells[0].text.chars().count());
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

    /// Draws the grid in all of `ui`'s available space.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        colors: &GridColors,
        font_size: f32,
        monospace: bool,
    ) -> GridInfo {
        let mut info = GridInfo::default();
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
        let ncols = self.cols.len().max(stats_fields).clamp(1, 10_000_000);
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
        info.visible_rows = visible;

        self.handle_input(ui, &response, doc, row_h, visible, cells_rect);
        self.apply_commands(doc, row_h, visible, cells_rect.width());
        if let Ok(p) = doc.locator().refine(self.top) {
            self.top = p;
        }
        if !self.cursor.exact
            && let Ok(p) = doc.locator().refine(self.cursor)
        {
            self.cursor = p;
        }

        // Fetch visible rows.
        let fetched = doc.locator().records_from(self.top, visible);
        let (rows, loading) = match fetched {
            Ok(f) => {
                let loading = f.end == FetchEnd::Budget;
                (f.rows, loading)
            }
            Err(_) => (Vec::new(), false),
        };
        // Do not leave empty space at the end of the file.
        if rows.len() + 1 < visible && !loading && self.top.record > Self::header_records(doc) {
            let missing = (visible - 1 - rows.len()) as i64;
            let before = self.top;
            self.move_top(doc, -missing);
            if self.top != before {
                ui.ctx().request_repaint();
            }
        }
        self.rows = rows;
        self.loading = loading;
        info.loading = loading;
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
        let hdr = Self::header_records(doc);
        let sel = self.selection();
        let char_px = font_size * 0.5;

        // Rows.
        let cell_painter = painter.with_clip_rect(cells_rect);
        let mut hovered: Option<(usize, usize)> = None;
        let pointer = ui.input(|i| i.pointer.hover_pos());
        for (ri, (pos, rec)) in self.rows.iter().enumerate() {
            let y = cells_rect.top() + ri as f32 * row_h - self.top_px;
            let row_rect =
                Rect::from_min_size(pos2(cells_rect.left(), y), vec2(cells_rect.width(), row_h));
            let ragged = expected.is_some_and(|e| rec.fields != 0 && rec.fields != e);
            if (pos.record - hdr) % 2 == 1 {
                cell_painter.rect_filled(row_rect, CornerRadius::ZERO, colors.stripe);
            }
            if ragged {
                cell_painter.rect_filled(row_rect, CornerRadius::ZERO, colors.ragged);
            }
            let ranges = [(0usize, frozen), (first_col, last_col)];
            for (a, b) in ranges {
                if a >= b {
                    continue;
                }
                let max_chars =
                    ((self.cols.width(a).max(self.cols.width(b - 1)) / char_px) as usize + 2)
                        .clamp(8, 1000);
                let cells = doc.cells(rec, a..b, max_chars).unwrap_or_default();
                for (k, cell) in cells.iter().enumerate() {
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
                            if (pos.record - hdr) % 2 == 1 {
                                colors.stripe
                            } else {
                                colors.background
                            },
                        );
                        if ragged {
                            cell_painter.rect_filled(rect, CornerRadius::ZERO, colors.ragged);
                        }
                    }
                    if let Some(((r0, c0), (r1, c1))) = sel
                        && pos.offset >= r0
                        && pos.offset <= r1
                        && c >= c0
                        && c <= c1
                    {
                        cell_painter.rect_filled(rect, CornerRadius::ZERO, colors.selection);
                    }
                    self.paint_cell(
                        &cell_painter,
                        rect,
                        cell,
                        &font,
                        colors,
                        self.cols.kind(c).is_numeric(),
                    );
                    if pointer.is_some_and(|p| rect.contains(p) && cells_rect.contains(p)) {
                        hovered = Some((ri, c));
                    }
                }
            }
            if pos.offset == self.cursor.offset {
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
        info.painted_rows = self.rows.len();
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
        let header_rec = doc.header_record().ok().flatten();
        let header_font = FontId::new(font_size, egui::FontFamily::Proportional);
        for (a, b) in [(0usize, frozen), (first_col, last_col)] {
            if a >= b {
                continue;
            }
            let names: Vec<Cell> = match &header_rec {
                Some(h) => doc.cells(h, a..b, 200).unwrap_or_default(),
                None => Vec::new(),
            };
            for c in a..b {
                let x = col_screen_x(&self.cols, c);
                let w = self.cols.width(c);
                let rect = Rect::from_min_size(pos2(x, full.top()), vec2(w, header_h));
                if c >= frozen && rect.right() < cells_rect.left() + frozen_w {
                    continue;
                }
                if c < frozen {
                    header_painter.rect_filled(rect, CornerRadius::ZERO, colors.header_bg);
                }
                let name = names
                    .get(c - a)
                    .filter(|n| !n.missing && !n.unknown)
                    .map(|n| n.text.clone())
                    .unwrap_or_else(|| format!("Column {}", c + 1));
                let job = single_line(
                    &name,
                    header_font.clone(),
                    colors.header_text,
                    w - 2.0 * CELL_PAD,
                );
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
        for (ri, (pos, rec)) in self.rows.iter().enumerate() {
            let y = cells_rect.top() + ri as f32 * row_h - self.top_px;
            let n = pos.record - hdr + 1;
            let label = if pos.exact {
                group(n)
            } else {
                format!("~{}", group(n))
            };
            let ragged = expected.is_some_and(|e| rec.fields != 0 && rec.fields != e);
            let color = if !pos.exact {
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
            doc,
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
        }
        if let Some(c) = self.menu_col {
            let name = self
                .header_names
                .iter()
                .find(|(i, _)| *i == c)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            response.context_menu(|ui| self.column_menu(ui, c, &name));
        }

        // Scrollbars.
        self.vscrollbar(ui, doc, full, cells_rect, colors);
        self.hscrollbar(ui, full, cells_rect, frozen_w, colors);

        // Hover tooltip and double click.
        if let Some((ri, c)) = hovered
            && let Some((_, rec)) = self.rows.get(ri).copied()
        {
            if response.double_clicked() {
                info.double_clicked = Some((rec, c));
            }
            if response.clicked() {
                let shift = ui.input(|i| i.modifiers.shift);
                let pos = self.rows[ri].0;
                if shift {
                    self.anchor.get_or_insert((self.cursor, self.cursor_col));
                } else {
                    self.anchor = None;
                }
                self.cursor = pos;
                self.cursor_col = c;
            }
            if !response.dragged() && ui.input(|i| i.pointer.time_since_last_movement() > 0.4) {
                let tip = doc.field_value(&rec, c, TOOLTIP_CHARS * 4).ok().flatten();
                if let Some((text, cut)) = tip
                    && !text.is_empty()
                    && (text.chars().count() as f32 * char_px * 1.15 > self.cols.width(c)
                        || text.contains(['\n', '\r', '\t']))
                {
                    let shown: String = text.chars().take(TOOLTIP_CHARS).collect();
                    let more = cut || text.chars().count() > TOOLTIP_CHARS;
                    response.clone().on_hover_ui_at_pointer(|ui| {
                        ui.set_max_width(520.0);
                        ui.label(egui::RichText::new(shown).font(font.clone()));
                        if more {
                            ui.weak("… (double-click to open the whole cell)");
                        }
                    });
                }
            }
        }

        if doc.cells_pending() {
            ui.ctx().request_repaint();
        }
        info.cursor = Some(self.cursor);
        info.cursor_col = self.cursor_col;
        info.columns = ncols;
        info
    }

    fn column_menu(&mut self, ui: &mut Ui, c: usize, name: &str) {
        ui.strong(format!("Column {}: {}", c + 1, name));
        ui.separator();
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
        ui.separator();
        let inferred = self.cols.types.get(c).copied().unwrap_or_default();
        let current = self.cols.type_overrides.get(c).copied().flatten();
        ui.label("Type (alignment and sort order only)");
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
    }

    fn paint_cell(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        cell: &Cell,
        font: &FontId,
        colors: &GridColors,
        numeric: bool,
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
        let job = single_line(&text, font.clone(), color, avail);
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

    /// Normalized selection: ((row offset lo, col lo), (row offset hi, col hi)).
    fn selection(&self) -> Option<((u64, usize), (u64, usize))> {
        let (a, ac) = self.anchor?;
        let (r0, r1) = if a.offset <= self.cursor.offset {
            (a.offset, self.cursor.offset)
        } else {
            (self.cursor.offset, a.offset)
        };
        Some(((r0, ac.min(self.cursor_col)), (r1, ac.max(self.cursor_col))))
    }

    fn handle_input(
        &mut self,
        ui: &mut Ui,
        response: &Response,
        doc: &mut Document,
        row_h: f32,
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
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = e
            else {
                continue;
            };
            let extend = modifiers.shift;
            let before = (self.cursor, self.cursor_col);
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
                Key::Escape => self.anchor = None,
                _ => continue,
            }
            if extend && key != Key::Tab {
                self.anchor.get_or_insert(before);
            } else if key != Key::Escape {
                self.anchor = None;
            }
        }
        if moved {
            self.ensure_cursor_visible(doc, visible);
        }
        if moved_col {
            self.ensure_col_visible(cells.width());
        }
        let _ = row_h;
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
        doc: &mut Document,
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
        // Click on a header cell selects the column's cell at the cursor row.
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
        let _ = doc;
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
        let frac = ((self.top.offset.saturating_sub(a)) as f64 / span).clamp(0.0, 1.0) as f32;
        let shown = self
            .rows
            .last()
            .map_or(0, |r| r.1.end.saturating_sub(self.top.offset));
        let thumb_frac = (shown as f64 / span).clamp(0.0, 1.0) as f32;
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
}
