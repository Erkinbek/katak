//! A document: one open file viewed under one dialect.
//!
//! Opening reads only a sample (the first [`SAMPLE_BYTES`]), detects the
//! dialect, and starts the background indexer. Everything the UI needs for a
//! frame (records around a position, the cells of visible columns) is
//! answered from the sample, the sparse index and bounded scans.

use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::detect::{self, Detection, SAMPLE_BYTES};
use crate::dialect::{Compiled, Dialect, DialectError};
use crate::fields::{FieldSpan, field_text, split, visible};
use crate::index::{self, Index, IndexWarning, LineEndings};
use crate::job::{Job, Notify};
use crate::locate::{FetchEnd, Locator, RowPos};
use crate::logical::{self, LKind, LPos, LRow};
use crate::overlay::{ColRef, END, Overlay, RowRef};
use crate::save::SaveInput;
use crate::scan::{RecSpan, scan_all};
use crate::source::{self, AccessMode, ChunkReader, Source};
use crate::units::{CR, LF, Units};
use crate::with_units;

/// At most this much of one record is parsed for display.
pub const ROW_PARSE_BUDGET: usize = 8 << 20;
const FIELD_STRIDE: usize = 64;
const FIELD_CACHE: usize = 512;
/// Fields split per frame while building the field index of wide records,
/// so even a record with a million fields never costs one long frame.
const FIELD_BUDGET: usize = 256 * 1024;
const SKIP_LINES_LIMIT: u64 = 64 << 20;

/// How to open a file.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub access: AccessMode,
    /// Use this dialect instead of auto-detecting one.
    pub dialect: Option<Dialect>,
}

/// One cell prepared for display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Single-line display text (control characters made visible), bounded.
    pub text: String,
    /// The value is longer than `text`.
    pub truncated: bool,
    /// Raw size of the field in bytes (including quotes).
    pub raw_len: usize,
    /// The record has no such field (short ragged row).
    pub missing: bool,
    /// Beyond the part of a huge record that was parsed.
    pub unknown: bool,
}

/// Snapshot of the indexing state for the status bar.
#[derive(Clone, Debug, Default)]
pub struct DocStats {
    pub file_len: u64,
    pub complete: bool,
    /// Data rows (header excluded): exact when `complete`, else estimated.
    pub rows: u64,
    pub indexed_fraction: f32,
    pub indexing: bool,
    pub index_elapsed: Duration,
    pub index_bytes: u64,
    pub ragged: u64,
    pub max_fields: u64,
    pub line_endings: LineEndings,
    pub warnings: Vec<IndexWarning>,
    pub error: Option<String>,
    pub index_heap: usize,
}

struct FieldIndex {
    /// Start of field `k * FIELD_STRIDE`, relative to the record start.
    starts: Vec<usize>,
    /// Every field of the (parsed part of the) record is covered.
    complete: bool,
}

/// Generations are unique across documents, so state tied to one (a
/// filter, a sort) is never mistaken for another's after Save As.
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// An open file.
pub struct Document {
    source: Arc<Source>,
    sample: Arc<[u8]>,
    detection: Detection,
    dialect: Dialect,
    compiled: Arc<Compiled>,
    data_start: u64,
    index: Arc<Index>,
    index_job: Option<Job>,
    locator: Locator,
    notify: Notify,
    field_cache: HashMap<u64, (FieldIndex, u64)>,
    field_budget: usize,
    fields_pending: bool,
    tick: u64,
    generation: u64,
    opened: Instant,
    /// Average record size in the sample (estimates before indexing).
    sample_avg: Option<f64>,
    access: AccessMode,
    /// Unsaved changes.
    pub overlay: Overlay,
}

/// Errors when opening a document.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("invalid parsing options: {0}")]
    Dialect(#[from] DialectError),
}

impl Document {
    /// Opens a file. Fast: reads the sample, detects, starts indexing.
    pub fn open(path: &Path, opts: OpenOptions, notify: Notify) -> Result<Document, OpenError> {
        let source = Arc::new(Source::open(path, opts.access)?);
        let n = (source.len() as usize).min(SAMPLE_BYTES);
        let mut sample = vec![0u8; n];
        source.read_at(0, &mut sample)?;
        source.prime(&sample);
        source.enable_background_fetch(notify.clone());
        let whole = n as u64 == source.len();
        let detection = detect::detect(&sample, whole);
        let dialect = opts.dialect.unwrap_or_else(|| detection.dialect.clone());
        let compiled = Arc::new(dialect.compile()?);
        let data_start =
            source::with_blocking_reads(|| data_start_for(&source, &sample, &dialect))?;
        let index = Index::new(data_start, source.len());
        let mut doc = Document {
            locator: Locator::new(source.clone(), compiled.clone(), index.clone(), None, None),
            source,
            sample: sample.into(),
            detection,
            dialect,
            compiled,
            data_start,
            index,
            index_job: None,
            notify,
            field_cache: HashMap::new(),
            field_budget: FIELD_BUDGET,
            fields_pending: false,
            tick: 0,
            generation: next_generation(),
            opened: Instant::now(),
            sample_avg: None,
            access: opts.access,
            overlay: Overlay::new(),
        };
        doc.restart_index();
        Ok(doc)
    }

    /// Switches to new parsing options: the index is discarded and rebuilt,
    /// and unsaved edits are dropped (record offsets change meaning).
    pub fn set_dialect(&mut self, dialect: Dialect) -> Result<(), OpenError> {
        let compiled = Arc::new(dialect.compile()?);
        self.overlay.clear();
        self.data_start =
            source::with_blocking_reads(|| data_start_for(&self.source, &self.sample, &dialect))?;
        self.dialect = dialect;
        self.compiled = compiled;
        self.generation = next_generation();
        self.restart_index();
        Ok(())
    }

    fn restart_index(&mut self) {
        if let Some(job) = self.index_job.take() {
            job.cancel();
        }
        self.field_cache.clear();
        self.index = Index::new(self.data_start, self.source.len());
        let (expected, avg) = self.sample_shape();
        self.sample_avg = avg;
        self.locator = Locator::new(
            self.source.clone(),
            self.compiled.clone(),
            self.index.clone(),
            expected,
            avg,
        );
        let (src, d, idx) = (
            self.source.clone(),
            self.compiled.clone(),
            self.index.clone(),
        );
        self.index_job = Some(Job::spawn("index", self.notify.clone(), move |ctl| {
            index::build(&src, &d, &idx, ctl)
        }));
    }

    /// Field count of the first record and average record size in the sample.
    fn sample_shape(&self) -> (Option<u64>, Option<f64>) {
        let start = self.data_start as usize;
        if start >= self.sample.len() {
            return (None, None);
        }
        let recs = scan_all(&self.compiled, &self.sample[..], start, 10_000);
        let complete: Vec<&RecSpan> = recs
            .iter()
            .filter(|r| {
                r.term != crate::scan::Term::None || self.sample.len() as u64 == self.source.len()
            })
            .collect();
        let expected = recs.first().map(|r| r.fields);
        let avg = match (complete.first(), complete.last()) {
            (Some(a), Some(b)) => Some((b.end - a.start) as f64 / complete.len() as f64),
            _ => None,
        };
        (expected, avg)
    }

    pub fn path(&self) -> &Path {
        self.source.path()
    }

    /// How the file was opened (mapped or buffered).
    pub fn access(&self) -> AccessMode {
        self.access
    }

    pub fn source(&self) -> &Arc<Source> {
        &self.source
    }

    pub fn sample(&self) -> &Arc<[u8]> {
        &self.sample
    }

    /// The sample covers the whole file.
    pub fn sample_is_whole(&self) -> bool {
        self.sample.len() as u64 == self.source.len()
    }

    pub fn detection(&self) -> &Detection {
        &self.detection
    }

    pub fn dialect(&self) -> &Dialect {
        &self.dialect
    }

    pub fn compiled(&self) -> &Arc<Compiled> {
        &self.compiled
    }

    pub fn index(&self) -> &Arc<Index> {
        &self.index
    }

    pub fn index_job(&self) -> Option<&Job> {
        self.index_job.as_ref()
    }

    pub fn locator(&mut self) -> &mut Locator {
        &mut self.locator
    }

    /// Bumped whenever the dialect changes (positions become invalid).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn data_start(&self) -> u64 {
        self.data_start
    }

    /// When the file was (re)opened.
    pub fn opened_at(&self) -> Instant {
        self.opened
    }

    /// Time since the document was opened.
    pub fn age(&self) -> Duration {
        self.opened.elapsed()
    }

    /// A locator of its own for a background job (export of a selection).
    pub fn job_locator(&self) -> Locator {
        let (expected, avg) = self.sample_shape();
        Locator::new(
            self.source.clone(),
            self.compiled.clone(),
            self.index.clone(),
            expected,
            avg,
        )
    }

    /// What background jobs (search, filter) need to scan this document.
    pub fn target(&self) -> crate::search::Target {
        crate::search::Target {
            src: self.source.clone(),
            dialect: self.compiled.clone(),
            encoding: self.dialect.encoding,
            data_start: self.data_start,
            header_records: self.header_records(),
            overlay: (!self.overlay.is_empty()).then(|| self.overlay.snapshot()),
        }
    }

    /// Records before the first data row (1 with a header row, else 0).
    pub fn header_records(&self) -> u64 {
        self.dialect.has_header as u64
    }

    /// Position of the first data row.
    pub fn first_data_row(&mut self) -> io::Result<RowPos> {
        if self.dialect.has_header {
            self.locator.locate_record(1)
        } else {
            Ok(self.locator.first())
        }
    }

    /// The header record, if the file has a header and any records.
    pub fn header_record(&mut self) -> io::Result<Option<RecSpan>> {
        if !self.dialect.has_header || self.data_start >= self.source.len() {
            return Ok(None);
        }
        let first = self.locator.first();
        Ok(self
            .locator
            .records_from(first, 1)?
            .rows
            .first()
            .map(|r| r.1))
    }

    /// Indexing state for the status bar.
    pub fn stats(&self) -> DocStats {
        let g = self.index.read();
        let records = g.estimated_records(self.sample_avg);
        let (indexing, elapsed, fraction) = match &self.index_job {
            Some(j) => (j.is_running(), j.elapsed(), j.fraction()),
            None => (false, Duration::ZERO, 1.0),
        };
        DocStats {
            file_len: self.source.len(),
            complete: g.complete,
            rows: records.saturating_sub(self.header_records()),
            indexed_fraction: if g.complete { 1.0 } else { fraction },
            indexing,
            index_elapsed: elapsed,
            index_bytes: g.scanned_bytes(),
            ragged: g.ragged_count,
            max_fields: g.max_fields,
            line_endings: g.line_endings,
            warnings: g.warnings.clone(),
            error: g.error.clone(),
            index_heap: g.heap_bytes(),
        }
    }

    /// Cells `cols` of record `rec`, each bounded to `max_chars` characters.
    pub fn cells(
        &mut self,
        rec: &RecSpan,
        cols: Range<usize>,
        max_chars: usize,
    ) -> io::Result<Vec<Cell>> {
        let content_len = (rec.content_end - rec.start) as usize;
        let parse_len = content_len.min(ROW_PARSE_BUDGET);
        let clipped = parse_len < content_len;
        let source = self.source.clone();
        let bytes = source.bytes(rec.start, parse_len)?;
        let (from, first) = if cols.start >= FIELD_STRIDE && parse_len > 4096 {
            let want = cols.start / FIELD_STRIDE;
            let (k, start, complete) = self.stride_start(rec.start, &bytes, want);
            if k < want && !complete {
                // Not reached within this frame's budget: show placeholders.
                self.fields_pending = true;
                return Ok(cols
                    .map(|_| Cell {
                        unknown: true,
                        ..Cell::default()
                    })
                    .collect());
            }
            (start, k * FIELD_STRIDE)
        } else {
            (0, 0)
        };
        let mut spans: Vec<FieldSpan> =
            Vec::with_capacity(cols.end.saturating_sub(first).min(4096));
        if cols.end > first {
            split(&self.compiled, &bytes, from, cols.end - first, &mut spans);
        }
        let enc = self.dialect.encoding;
        let max_bytes = max_chars.saturating_mul(4).max(16);
        Ok(cols
            .map(|c| match spans.get(c - first) {
                Some(f) => {
                    let raw = &bytes[f.start..f.end];
                    let (text, cut) = field_text(&self.compiled, enc, raw, f.quoted, max_bytes);
                    let vis = visible(&text);
                    let (text, cut2) = truncate_chars(&vis, max_chars);
                    let runs_past_clip = clipped && f.end == bytes.len();
                    Cell {
                        text,
                        truncated: cut || cut2 || runs_past_clip,
                        raw_len: f.len(),
                        missing: false,
                        unknown: false,
                    }
                }
                None => Cell {
                    missing: !clipped,
                    unknown: clipped,
                    ..Cell::default()
                },
            })
            .collect())
    }

    /// Full decoded value of one field (bounded to `max_bytes` of raw data).
    pub fn field_value(
        &mut self,
        rec: &RecSpan,
        col: usize,
        max_bytes: usize,
    ) -> io::Result<Option<(String, bool)>> {
        let content_len = (rec.content_end - rec.start) as usize;
        let source = self.source.clone();
        let bytes = source.bytes(rec.start, content_len.min(ROW_PARSE_BUDGET.max(max_bytes)))?;
        let mut spans = Vec::new();
        split(&self.compiled, &bytes, 0, col + 1, &mut spans);
        Ok(spans.get(col).map(|f| {
            field_text(
                &self.compiled,
                self.dialect.encoding,
                &bytes[f.start..f.end],
                f.quoted,
                max_bytes,
            )
        }))
    }

    /// Start of field `k * FIELD_STRIDE` of a record, extending the record's
    /// field index within this frame's budget. Returns the stride actually
    /// reached, its start offset and whether the index is complete.
    fn stride_start(&mut self, rec_start: u64, bytes: &[u8], k: usize) -> (usize, usize, bool) {
        self.tick += 1;
        if !self.field_cache.contains_key(&rec_start) {
            if self.field_cache.len() >= FIELD_CACHE
                && let Some(&old) = self.field_cache.iter().min_by_key(|e| (e.1).1).map(|e| e.0)
            {
                self.field_cache.remove(&old);
            }
            self.field_cache.insert(
                rec_start,
                (
                    FieldIndex {
                        starts: vec![0],
                        complete: false,
                    },
                    0,
                ),
            );
        }
        let compiled = self.compiled.clone();
        let Some((fi, used)) = self.field_cache.get_mut(&rec_start) else {
            return (0, 0, false);
        };
        *used = self.tick;
        let mut spans = Vec::new();
        while fi.starts.len() <= k && !fi.complete && self.field_budget > 0 {
            spans.clear();
            let from = fi.starts[fi.starts.len() - 1];
            match split(&compiled, bytes, from, FIELD_STRIDE, &mut spans) {
                Some(next) => fi.starts.push(next),
                None => fi.complete = true,
            }
            self.field_budget = self.field_budget.saturating_sub(FIELD_STRIDE);
        }
        let reached = k.min(fi.starts.len() - 1);
        (reached, fi.starts[reached], fi.complete)
    }

    /// Starts a UI frame: resets the per-frame parsing budget.
    pub fn begin_frame(&mut self) {
        self.field_budget = FIELD_BUDGET;
        self.fields_pending = false;
    }

    /// Some cells were left as placeholders this frame (budget exhausted);
    /// the caller should repaint soon.
    pub fn cells_pending(&self) -> bool {
        self.fields_pending
    }

    // ------------------------------------------------------- overlay

    /// Re-reads the file after it was saved (or changed on disk): same
    /// dialect, no edits, fresh index.
    pub fn reopen(&mut self) -> Result<(), OpenError> {
        let path = self.source.path().to_path_buf();
        let source = Arc::new(Source::open(&path, self.access)?);
        let n = (source.len() as usize).min(SAMPLE_BYTES);
        let mut sample = vec![0u8; n];
        source.read_at(0, &mut sample)?;
        source.prime(&sample);
        source.enable_background_fetch(self.notify.clone());
        self.data_start =
            source::with_blocking_reads(|| data_start_for(&source, &sample, &self.dialect))?;
        self.source = source;
        self.sample = sample.into();
        self.opened = Instant::now();
        self.overlay.clear();
        self.generation = next_generation();
        self.restart_index();
        Ok(())
    }

    /// Everything a save job needs (a snapshot of the overlay).
    pub fn save_input(&self, columns: usize) -> SaveInput {
        let line_ending = self
            .index
            .read()
            .line_endings
            .dominant()
            .or(self.detection.line_endings.dominant())
            .unwrap_or(crate::scan::Term::Lf);
        SaveInput {
            src: self.source.clone(),
            dialect: self.dialect.clone(),
            compiled: self.compiled.clone(),
            data_start: self.data_start,
            overlay: self.overlay.snapshot(),
            line_ending,
            columns,
        }
    }

    /// Position of the first data row.
    pub fn first_lpos(&mut self) -> LPos {
        let rec = self.first_data_row().unwrap_or(RowPos {
            offset: self.data_start,
            record: 0,
            exact: true,
        });
        // Rows inserted before the first record come first.
        LPos { rec, sub: 0 }
    }

    /// Position after the last row.
    pub fn end_lpos(&mut self) -> LPos {
        let rec = self.locator.end_pos();
        LPos {
            rec,
            sub: self.overlay.inserted_before(END).len() as u32,
        }
    }

    /// The logical position of a source record.
    pub fn lpos_of(&self, rec: RowPos) -> LPos {
        logical::at_record(&self.overlay, rec, self.source.len())
    }

    pub fn lrows_from(&mut self, from: LPos, n: usize) -> io::Result<(Vec<LRow>, FetchEnd)> {
        logical::rows_from(&mut self.locator, &self.overlay, from, n)
    }

    pub fn lrows_before(&mut self, before: LPos, n: usize) -> io::Result<Vec<LRow>> {
        let hdr = self.header_records();
        logical::rows_before(&mut self.locator, &self.overlay, before, n, hdr)
    }

    /// Logical data-row index (0-based) of a position.
    pub fn lnumber(&self, pos: &LPos) -> u64 {
        logical::number(&self.overlay, pos, self.header_records(), self.source.len())
    }

    /// Number of logical columns, given the source columns known so far.
    pub fn logical_columns(&self, source_columns: usize) -> usize {
        match self.overlay.columns() {
            None => source_columns,
            Some(cols) => {
                let mapped = cols
                    .iter()
                    .filter_map(|c| {
                        if let ColRef::Src(i) = c {
                            Some(*i as usize + 1)
                        } else {
                            None
                        }
                    })
                    .max()
                    .unwrap_or(0);
                cols.len() + source_columns.saturating_sub(mapped)
            }
        }
    }

    /// Cells `cols` (logical columns) of a logical row, with edits applied.
    /// The flag tells whether the cell was edited.
    pub fn display_cells(
        &mut self,
        row: &LRow,
        cols: Range<usize>,
        max_chars: usize,
    ) -> io::Result<Vec<(Cell, bool)>> {
        let rref = row.row_ref();
        let mut out = Vec::with_capacity(cols.len());
        let identity = self.overlay.columns().is_none();
        let base: Vec<Cell> = match row.kind {
            LKind::Src(span) if identity => self.cells(&span, cols.clone(), max_chars)?,
            LKind::Src(span) => {
                let mut v = Vec::with_capacity(cols.len());
                for c in cols.clone() {
                    v.push(match self.overlay.col_ref(c) {
                        ColRef::Src(i) => self
                            .cells(&span, i as usize..i as usize + 1, max_chars)?
                            .pop()
                            .unwrap_or_default(),
                        ColRef::New(_) => Cell::default(),
                    });
                }
                v
            }
            LKind::New(_) => cols.clone().map(|_| Cell::default()).collect(),
        };
        for (c, cell) in cols.zip(base) {
            match self.overlay.cell(rref, self.overlay.col_ref(c)) {
                Some(v) => {
                    let vis = visible(v);
                    let (text, cut) = truncate_chars(&vis, max_chars);
                    out.push((
                        Cell {
                            text,
                            truncated: cut,
                            raw_len: v.len(),
                            missing: false,
                            unknown: false,
                        },
                        true,
                    ));
                }
                None => out.push((cell, false)),
            }
        }
        Ok(out)
    }

    /// The full value of a cell (bounded to `max_bytes` for source cells),
    /// with edits applied.
    pub fn value(&mut self, row: &LRow, col: usize, max_bytes: usize) -> io::Result<String> {
        let cref = self.overlay.col_ref(col);
        if let Some(v) = self.overlay.cell(row.row_ref(), cref) {
            return Ok(v.to_string());
        }
        match (row.kind, cref) {
            (LKind::Src(span), ColRef::Src(i)) => Ok(self
                .field_value(&span, i as usize, max_bytes)?
                .map(|(t, _)| t)
                .unwrap_or_default()),
            _ => Ok(String::new()),
        }
    }

    /// The source value of a cell (before edits), for "no change" checks.
    pub fn original(&mut self, row: RowRef, col: ColRef) -> Option<String> {
        let (RowRef::Src(offset), ColRef::Src(i)) = (row, col) else {
            return None;
        };
        let pos = RowPos {
            offset,
            record: 0,
            exact: false,
        };
        let rec = self
            .locator
            .records_from(pos, 1)
            .ok()?
            .rows
            .first()
            .map(|r| r.1)
            .filter(|r| r.start == offset)?;
        self.field_value(&rec, i as usize, ROW_PARSE_BUDGET)
            .ok()
            .flatten()
            .map(|(t, _)| t)
    }

    /// Sets cells given by logical row and column (one undo step).
    pub fn set_values(&mut self, changes: Vec<(RowRef, usize, String)>) {
        let mapped: Vec<(RowRef, ColRef, String)> = changes
            .into_iter()
            .map(|(r, c, v)| (r, self.overlay.col_ref(c), v))
            .collect();
        let mut originals = HashMap::new();
        for (r, c, _) in &mapped {
            if let RowRef::Src(_) = r {
                originals.insert((*r, *c), self.original(*r, *c));
            }
        }
        self.overlay
            .set_cells(mapped, |r, c| originals.get(&(r, c)).cloned().flatten());
    }

    /// Like [`Document::set_values`] for values known to differ from the
    /// current ones (Replace All): only cells that already have an edit
    /// need their source value read, so that restoring it drops the edit.
    pub fn set_changed_values(&mut self, changes: Vec<(RowRef, usize, String)>) {
        let mapped: Vec<(RowRef, ColRef, String)> = changes
            .into_iter()
            .map(|(r, c, v)| (r, self.overlay.col_ref(c), v))
            .collect();
        let mut originals = HashMap::new();
        for (r, c, _) in &mapped {
            if matches!(r, RowRef::Src(_)) && self.overlay.cell(*r, *c).is_some() {
                originals.insert((*r, *c), self.original(*r, *c));
            }
        }
        self.overlay
            .set_cells(mapped, |r, c| originals.get(&(r, c)).cloned().flatten());
    }

    /// Display name of logical column `col` (with renames), if the file has
    /// a header.
    pub fn column_name(&mut self, col: usize) -> Option<String> {
        if !self.dialect.has_header {
            return None;
        }
        let header = self.header_record().ok().flatten()?;
        let cref = self.overlay.col_ref(col);
        if let Some(v) = self.overlay.cell(RowRef::Src(header.start), cref) {
            return Some(v.to_string());
        }
        match cref {
            ColRef::Src(i) => self
                .cells(&header, i as usize..i as usize + 1, 200)
                .ok()?
                .pop()
                .filter(|c| !c.missing && !c.unknown)
                .map(|c| c.text),
            ColRef::New(_) => Some(String::new()),
        }
    }

    /// Decoded cells of the first records in the sample (for column widths,
    /// type inference and previews). Values are bounded to `max_chars`.
    pub fn sample_rows(&self, max_records: usize, max_chars: usize) -> Vec<Vec<String>> {
        preview_rows(
            &self.sample,
            self.sample_is_whole(),
            &self.dialect,
            max_records,
            max_chars,
        )
        .unwrap_or_default()
    }
}

/// Parses the first `max_records` records of `sample` under `dialect`.
/// Used by the parsing dialog for its live preview.
pub fn preview_rows(
    sample: &[u8],
    whole: bool,
    dialect: &Dialect,
    max_records: usize,
    max_chars: usize,
) -> Result<Vec<Vec<String>>, DialectError> {
    let c = dialect.compile()?;
    let bom = dialect.encoding.bom();
    let mut start = if !bom.is_empty() && sample.starts_with(bom) {
        bom.len()
    } else {
        0
    };
    let (p, left) =
        with_units!(c.width, U => skip_units::<U>(sample, start, dialect.skip_lines, true));
    start = if left == 0 { p } else { sample.len() };
    let mut recs = scan_all(&c, sample, start, max_records);
    if !whole
        && recs.len() > 1
        && recs
            .last()
            .is_some_and(|r| r.term == crate::scan::Term::None)
    {
        recs.pop();
    }
    let max_bytes = max_chars.saturating_mul(4).max(16);
    Ok(recs
        .iter()
        .map(|r| {
            let content = &sample[r.start as usize..r.content_end as usize];
            let mut spans = Vec::new();
            split(&c, content, 0, 100_000, &mut spans);
            spans
                .iter()
                .map(|f| {
                    let (t, _) = field_text(
                        &c,
                        dialect.encoding,
                        &content[f.start..f.end],
                        f.quoted,
                        max_bytes,
                    );
                    truncate_chars(&visible(&t), max_chars).0
                })
                .collect()
        })
        .collect())
}

fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// Where the data begins: after this encoding's BOM and `skip_lines` raw lines.
fn data_start_for(src: &Source, sample: &[u8], d: &Dialect) -> io::Result<u64> {
    let bom = d.encoding.bom();
    let start = if !bom.is_empty() && sample.starts_with(bom) {
        bom.len() as u64
    } else {
        0
    };
    if d.skip_lines == 0 {
        return Ok(start);
    }
    let width = d.encoding.width();
    let mut reader = ChunkReader::cached(src, src.len().min(start + SKIP_LINES_LIMIT), 1 << 20);
    let mut pos = start;
    let mut left = d.skip_lines;
    loop {
        let w = reader.window(pos)?;
        let rel = (pos - w.base) as usize;
        let (p, rest) = with_units!(width, U => skip_units::<U>(w.buf, rel, left, w.eof));
        pos = w.base + p as u64;
        left = rest;
        if left == 0 || w.eof {
            return Ok(if left == 0 { pos } else { w.end() });
        }
    }
}

/// Skips up to `left` raw lines (LF, CR or CRLF) from `p`. Returns the new
/// position and the number of lines still to skip. Without `eof` the last
/// unit is kept back (a CR may be followed by an LF in the next window).
fn skip_units<U: Units>(buf: &[u8], mut p: usize, mut left: u32, eof: bool) -> (usize, u32) {
    let w = U::W;
    while left > 0 && p + w <= buf.len() && (eof || p + 2 * w <= buf.len()) {
        let u = U::get(buf, p);
        p += w;
        if u == LF || u == CR {
            if u == CR && p + w <= buf.len() && U::get(buf, p) == LF {
                p += w;
            }
            left -= 1;
        }
    }
    (p, left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Delimiter;
    use crate::job::{JobState, no_notify};

    fn open(bytes: &[u8]) -> (tempfile::TempDir, Document) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, bytes).unwrap();
        let doc = Document::open(&p, OpenOptions::default(), no_notify()).unwrap();
        (dir, doc)
    }

    #[test]
    fn opens_detects_and_indexes() {
        let mut data = b"id;name;price\n".to_vec();
        for i in 0..5000 {
            data.extend_from_slice(format!("{i};\"n;{i}\";{i},50\n").as_bytes());
        }
        let (_d, mut doc) = open(&data);
        assert_eq!(doc.dialect().delimiter, Delimiter::Char(';'));
        assert!(doc.dialect().has_header);
        assert_eq!(doc.index_job().unwrap().join(), JobState::Done);
        let st = doc.stats();
        assert!(st.complete);
        assert_eq!(st.rows, 5000);
        let first = doc.first_data_row().unwrap();
        let rows = doc.locator().records_from(first, 2).unwrap().rows;
        let cells = doc.cells(&rows[1].1, 0..4, 100).unwrap();
        assert_eq!(cells[0].text, "1");
        assert_eq!(cells[1].text, "n;1");
        assert_eq!(cells[2].text, "1,50");
        assert!(cells[3].missing);
        let header = doc.header_record().unwrap().unwrap();
        assert_eq!(doc.cells(&header, 0..3, 100).unwrap()[2].text, "price");
    }

    #[test]
    fn wide_rows_use_field_checkpoints() {
        let cols = 10_000;
        let mut data = (0..cols)
            .map(|c| format!("c{c}"))
            .collect::<Vec<_>>()
            .join(",");
        data.push('\n');
        for r in 0..3 {
            data.push_str(
                &(0..cols)
                    .map(|c| format!("{r}:{c}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            data.push('\n');
        }
        let (_d, mut doc) = open(data.as_bytes());
        doc.index_job().unwrap().join();
        let first = doc.first_data_row().unwrap();
        let rows = doc.locator().records_from(first, 3).unwrap().rows;
        let cells = doc.cells(&rows[2].1, 9_990..10_001, 50).unwrap();
        assert_eq!(cells[0].text, "2:9990");
        assert_eq!(cells[9].text, "2:9999");
        assert!(cells[10].missing);
    }

    #[test]
    fn dialect_change_restarts_indexing() {
        let (_d, mut doc) = open(b"a|b,c\n1|2,3\n");
        doc.index_job().unwrap().join();
        let before = doc.generation();
        let d = Dialect {
            delimiter: Delimiter::Char('|'),
            ..doc.dialect().clone()
        };
        doc.set_dialect(d).unwrap();
        assert_ne!(doc.generation(), before);
        doc.index_job().unwrap().join();
        let first = doc.first_data_row().unwrap();
        let rec = doc.locator().records_from(first, 1).unwrap().rows[0].1;
        assert_eq!(doc.cells(&rec, 0..2, 10).unwrap()[1].text, "2,3");
    }

    #[test]
    fn skip_lines_and_bom() {
        let (_d, mut doc) = open(b"\xEF\xBB\xBFtitle line\r\nsecond junk\r\na,b\r\n1,2\r\n");
        let d = Dialect {
            skip_lines: 2,
            has_header: true,
            ..doc.dialect().clone()
        };
        doc.set_dialect(d).unwrap();
        assert_eq!(doc.data_start(), 3 + 12 + 13);
        doc.index_job().unwrap().join();
        assert_eq!(doc.stats().rows, 1);
        let rows = preview_rows(doc.sample(), true, doc.dialect(), 10, 50).unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["a".to_string(), "b".into()],
                vec!["1".into(), "2".into()]
            ]
        );
    }

    #[test]
    fn empty_file() {
        let (_d, mut doc) = open(b"");
        assert!(doc.stats().complete);
        assert_eq!(doc.stats().rows, 0);
        assert!(doc.header_record().unwrap().is_none());
        let first = doc.first_data_row().unwrap();
        assert!(
            doc.locator()
                .records_from(first, 10)
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn huge_cell_is_bounded() {
        let mut data = b"a,b\n1,\"".to_vec();
        data.extend(std::iter::repeat_n(b'x', 20 << 20));
        data.extend_from_slice(b"\"\n2,3\n");
        let (_d, mut doc) = open(&data);
        doc.index_job().unwrap().join();
        let first = doc.first_data_row().unwrap();
        let rows = doc.locator().records_from(first, 2).unwrap().rows;
        assert_eq!(rows.len(), 2);
        let cells = doc.cells(&rows[0].1, 0..2, 100).unwrap();
        assert_eq!(cells[1].text.chars().count(), 100);
        assert!(cells[1].truncated);
        assert_eq!(doc.cells(&rows[1].1, 0..2, 100).unwrap()[1].text, "3");
    }
}
