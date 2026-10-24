//! Export / Save as with conversions: another delimiter, quoting, encoding
//! or line ending, for the whole file, a row list (filter or sort order)
//! or a selection. Every row is decoded and re-encoded with the edits
//! applied; rows are streamed, so memory stays bounded. Comment lines and
//! skipped preamble lines are not exported.

use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::sync::Arc;

use crate::dialect::{Delimiter, Dialect, Escape};
use crate::fields::{FieldSpan, split};
use crate::filter::SharedFilter;
use crate::job::JobCtl;
use crate::locate::{FetchEnd, Locator};
use crate::logical::{self, LKind, LPos};
use crate::overlay::{END, Overlay, RowRef};
use crate::save::{SaveError, SaveReport, save_with};
use crate::scan::{Flow, RecSpan, Term};
use crate::search::Target;
use crate::units::UnitWidth;
use crate::visit::{MAX_RECORD_BYTES, read_record, read_record_into, visit_records};
use crate::write::{QuotePolicy, Writer};

/// Which rows to export.
pub enum Scope {
    /// Every row in order (inserted rows included, deleted rows skipped).
    All,
    /// The rows of a filter or a sort, in their order.
    List(SharedFilter),
    /// `count` logical rows starting at `start` (a selection).
    Logical { start: LPos, count: u64 },
}

/// Output format.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// Delimiter, quote character, escape style and encoding of the output.
    pub dialect: Dialect,
    pub policy: QuotePolicy,
    pub line_ending: Term,
    /// Write a byte-order mark (UTF-8/UTF-16 only).
    pub bom: bool,
    /// Inclusive range of logical columns; `None` = all.
    pub columns: Option<(usize, usize)>,
}

/// Everything an export job needs.
pub struct ExportInput {
    pub target: Target,
    /// A locator of its own (for selections).
    pub locator: Locator,
    pub overlay: Arc<Overlay>,
    /// Offset of the header record, written first (`None`: no header line).
    pub header: Option<u64>,
    /// Logical column count.
    pub columns: usize,
    /// Approximate number of data rows (plans how a sort order is read).
    pub rows: u64,
}

/// Plain records (no quotes, no edits, nothing the output would have to
/// quote) are converted by swapping delimiters, without decoding.
#[derive(Clone, Copy)]
struct Plain {
    delim: u8,
    quote: Option<u8>,
    /// Bytes that send a record to the full path (up to 3).
    stops: [u8; 3],
    n_stops: usize,
}

impl Plain {
    fn new(t: &Target, opts: &ExportOptions, ov: &Overlay) -> Option<Plain> {
        let (d, out) = (&t.dialect, &opts.dialect);
        if !d.fast
            || d.whitespace
            || d.trim
            || t.encoding != out.encoding
            || t.encoding.width() != UnitWidth::One
            || opts.policy != QuotePolicy::Minimal
            || opts.columns.is_some()
            || ov.columns().is_some()
            || out.trim
            || out.delimiter == Delimiter::Whitespace
            || out.comment_prefix.is_some()
        {
            return None;
        }
        let delim = d.delim_byte();
        let mut stops = Vec::new();
        stops.push(out.quote.unwrap_or('"') as u8);
        let mut od = Vec::new();
        out.encoding.encode(&out.delimiter.write_str(), &mut od);
        if od != [delim] {
            stops.push(*od.first()?);
        }
        if out.escape == Escape::Backslash {
            stops.push(b'\\');
        }
        stops.sort_unstable();
        stops.dedup();
        if stops.len() > 3 {
            return None;
        }
        let mut s = [stops[0]; 3];
        s[..stops.len()].copy_from_slice(&stops);
        Some(Plain {
            delim,
            quote: d.quote.map(|q| q as u8),
            stops: s,
            n_stops: stops.len(),
        })
    }

    fn applies(&self, content: &[u8]) -> bool {
        if self
            .quote
            .is_some_and(|q| memchr::memchr(q, content).is_some())
        {
            return false;
        }
        let s = self.stops;
        match self.n_stops {
            1 => memchr::memchr(s[0], content).is_none(),
            2 => memchr::memchr2(s[0], s[1], content).is_none(),
            _ => memchr::memchr3(s[0], s[1], s[2], content).is_none(),
        }
    }
}

struct Exporter<'a, W: Write> {
    target: &'a Target,
    overlay: &'a Overlay,
    plain: Option<Plain>,
    columns: usize,
    range: Option<(usize, usize)>,
    w: Writer,
    out: W,
    buf: Vec<u8>,
    spans: Vec<FieldSpan>,
    rows: u64,
}

impl<W: Write> Exporter<'_, W> {
    fn range(&self, n: usize) -> (usize, usize) {
        self.range
            .unwrap_or((0, self.columns.max(n).saturating_sub(1)))
    }

    fn put<'s>(&mut self, values: impl IntoIterator<Item = &'s str>) -> io::Result<()> {
        self.buf.clear();
        self.w.record(values, &mut self.buf);
        self.out.write_all(&self.buf)?;
        self.rows += 1;
        Ok(())
    }

    /// A source record with its edits applied.
    fn record(&mut self, offset: u64, content: &[u8]) -> io::Result<()> {
        if let Some(p) = self.plain
            && self.overlay.row_edits(offset).is_none()
            && p.applies(content)
        {
            self.buf.clear();
            let mut fields = 1;
            let mut from = 0;
            for i in memchr::memchr_iter(p.delim, content) {
                self.buf.extend_from_slice(&content[from..i]);
                self.w.delimiter(&mut self.buf);
                from = i + 1;
                fields += 1;
            }
            self.buf.extend_from_slice(&content[from..]);
            for _ in fields..self.columns {
                self.w.delimiter(&mut self.buf);
            }
            self.w.line_end(&mut self.buf);
            self.out.write_all(&self.buf)?;
            self.rows += 1;
            return Ok(());
        }
        let mut spans = std::mem::take(&mut self.spans);
        spans.clear();
        split(&self.target.dialect, content, 0, usize::MAX, &mut spans);
        let values = self.target.values(offset, content, &spans);
        let (a, b) = self.range(values.len());
        let res = self.put((a..=b).map(|c| values.get(c).map_or("", |(_, v)| v.as_ref())));
        self.spans = spans;
        res
    }

    fn record_at(&mut self, offset: u64) -> io::Result<()> {
        let (_, content) = read_record(&self.target.src, &self.target.dialect, offset)?;
        self.record(offset, &content)
    }

    fn record_span(&mut self, span: &RecSpan) -> io::Result<()> {
        let len = ((span.content_end - span.start) as usize).min(MAX_RECORD_BYTES);
        let mut content = vec![0u8; len];
        self.target.src.read_at(span.start, &mut content)?;
        self.record(span.start, &content)
    }

    fn new_rows(&mut self, ids: &[u32]) -> io::Result<()> {
        let ov = self.overlay;
        for &id in ids {
            let (a, b) = self.range(0);
            let values: Vec<&str> = (a..=b)
                .map(|c| ov.cell(RowRef::New(id), ov.col_ref(c)).unwrap_or(""))
                .collect();
            self.put(values)?;
        }
        Ok(())
    }
}

/// Record bytes read per batch when exporting a row list.
const LIST_BATCH_BYTES: usize = 64 << 20;
/// A sort list with fewer rows than one per this many file bytes is read
/// row by row; denser ones are distributed in one sequential pass.
const SPARSE_GAP: u64 = 64 << 10;
/// Source bytes per bucket of a permutation export.
const BUCKET_BYTES: u64 = 128 << 20;
const MAX_BUCKETS: u64 = 256;

/// Exports a dense sort permutation without random reads: one sequential
/// pass distributes the listed records into buckets of consecutive ranks
/// (temporary files next to the output, or memory when one bucket does),
/// then each bucket is ordered by rank in memory and written.
fn export_permutation<W: Write>(
    e: &mut Exporter<'_, W>,
    t: &Target,
    list: &SharedFilter,
    rows_hint: u64,
    dir: &Path,
    ctl: &JobCtl,
    bucket_bytes: u64,
) -> io::Result<()> {
    // (offset, rank), in file order.
    let mut ranked: Vec<(u64, u64)> = {
        let d = list.lock();
        d.rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.offset, i as u64))
            .collect()
    };
    ranked.sort_unstable();
    let n = ranked.len() as u64;
    if n == 0 {
        return Ok(());
    }
    let data = t.src.len().saturating_sub(t.data_start);
    let listed = data / rows_hint.max(1) * n;
    let buckets = listed.div_ceil(bucket_bytes).clamp(1, MAX_BUCKETS);
    let mut files = Vec::new();
    if buckets > 1 {
        for _ in 0..buckets {
            files.push(io::BufWriter::with_capacity(
                256 << 10,
                tempfile::tempfile_in(dir)?,
            ));
        }
    }
    let mut memory: Vec<u8> = Vec::new();
    let mut k = 0usize;
    let mut err = None;
    let res = visit_records(&t.src, &t.dialect, t.data_start, 0, ctl, None, &[], |v| {
        while k < ranked.len() && ranked[k].0 < v.span.start {
            k += 1;
        }
        if k >= ranked.len() {
            return Flow::Stop;
        }
        if ranked[k].0 != v.span.start {
            return Flow::Continue;
        }
        let rank = ranked[k].1;
        k += 1;
        let mut head = [0u8; 20];
        head[..8].copy_from_slice(&rank.to_le_bytes());
        head[8..16].copy_from_slice(&v.span.start.to_le_bytes());
        head[16..].copy_from_slice(&(v.content.len() as u32).to_le_bytes());
        let res = if buckets == 1 {
            memory.extend_from_slice(&head);
            memory.extend_from_slice(v.content);
            Ok(())
        } else {
            let f = &mut files[(rank * buckets / n) as usize];
            f.write_all(&head).and_then(|_| f.write_all(v.content))
        };
        match res {
            Ok(()) => Flow::Continue,
            Err(x) => {
                err = Some(x);
                Flow::Stop
            }
        }
    });
    if let Some(x) = err {
        return Err(x);
    }
    res?;
    if ctl.is_cancelled() {
        return Err(cancelled());
    }
    drop(ranked);
    ctl.set_total(n);
    let mut written = 0u64;
    let mut write_bucket = |e: &mut Exporter<'_, W>, bytes: &[u8]| -> io::Result<()> {
        // (rank, offset, start, end) of each record.
        let mut index = Vec::new();
        let mut at = 0usize;
        while at + 20 <= bytes.len() {
            let num = |a: usize, b: usize| {
                let mut w = [0u8; 8];
                w[..b - a].copy_from_slice(&bytes[a..b]);
                u64::from_le_bytes(w)
            };
            let (rank, offset, len) = (
                num(at, at + 8),
                num(at + 8, at + 16),
                num(at + 16, at + 20) as usize,
            );
            index.push((rank, offset, at + 20, at + 20 + len));
            at += 20 + len;
        }
        index.sort_unstable_by_key(|x| x.0);
        for (_, offset, a, b) in index {
            e.record(offset, &bytes[a..b])?;
            written += 1;
            if written.is_multiple_of(1 << 16) {
                if ctl.is_cancelled() {
                    return Err(cancelled());
                }
                ctl.set_done(written);
                ctl.notify();
            }
        }
        Ok(())
    };
    if buckets == 1 {
        write_bucket(e, &memory)?;
    } else {
        let mut bytes = Vec::new();
        for f in files {
            let mut f = f.into_inner().map_err(|x| x.into_error())?;
            f.seek(io::SeekFrom::Start(0))?;
            bytes.clear();
            f.read_to_end(&mut bytes)?;
            drop(f);
            write_bucket(e, &bytes)?;
        }
    }
    Ok(())
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the export was cancelled")
}

/// Writes the export to `path` (temp file, fsync, atomic rename).
/// `records_rewritten` in the report is the number of data rows written.
pub fn export(
    input: ExportInput,
    scope: Scope,
    opts: &ExportOptions,
    path: &Path,
    ctl: &JobCtl,
) -> Result<SaveReport, SaveError> {
    export_with(input, scope, opts, path, ctl, BUCKET_BYTES)
}

fn export_with(
    mut input: ExportInput,
    scope: Scope,
    opts: &ExportOptions,
    path: &Path,
    ctl: &JobCtl,
    bucket_bytes: u64,
) -> Result<SaveReport, SaveError> {
    save_with(path, false, ctl, |out| {
        let mut e = Exporter {
            target: &input.target,
            overlay: &input.overlay,
            plain: Plain::new(&input.target, opts, &input.overlay),
            columns: input.columns,
            range: opts.columns,
            w: Writer::new(&opts.dialect, opts.line_ending, opts.policy),
            out,
            buf: Vec::new(),
            spans: Vec::new(),
            rows: 0,
        };
        if opts.bom {
            e.out.write_all(opts.dialect.encoding.bom())?;
        }
        if let Some(h) = input.header {
            e.record_at(h)?;
            // The report counts data rows.
            e.rows = 0;
        }
        let t = &input.target;
        match &scope {
            Scope::All => {
                let ov = &input.overlay;
                let mut prev: Option<u64> = None;
                let mut err = None;
                let res = visit_records(&t.src, &t.dialect, t.data_start, 0, ctl, None, &[], |v| {
                    if v.record >= t.header_records {
                        let step = (|| -> io::Result<()> {
                            for (_, ids) in ov.anchors_between(prev, v.span.start) {
                                e.new_rows(ids)?;
                            }
                            if !ov.is_deleted(v.span.start) {
                                e.record(v.span.start, v.content)?;
                            }
                            Ok(())
                        })();
                        if let Err(x) = step {
                            err = Some(x);
                            return Flow::Stop;
                        }
                    }
                    prev = Some(v.span.start);
                    Flow::Continue
                });
                if let Some(x) = err {
                    return Err(x);
                }
                if !res?.completed {
                    return Err(cancelled());
                }
                for (_, ids) in ov.anchors_between(prev, END) {
                    e.new_rows(ids)?;
                }
            }
            Scope::List(list)
                if list.lock().sorted
                    && list.lock().rows.len() as u64 * SPARSE_GAP > t.src.len() =>
            {
                let dir = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                export_permutation(&mut e, t, list, input.rows, dir, ctl, bucket_bytes)?;
            }
            Scope::List(list) => {
                // A filter is in file order; a short sort list is read in
                // batches in file order (so the mapped views are reused),
                // then written in list order.
                let n = list.lock().rows.len();
                ctl.set_total(n as u64);
                let (mut i, mut batch_rows) = (0usize, 1usize << 16);
                let (mut arena, mut at) = (Vec::new(), Vec::new());
                while i < n {
                    if ctl.is_cancelled() {
                        return Err(cancelled());
                    }
                    let batch = list.lock().rows.slice(i, i + batch_rows);
                    if batch.is_empty() {
                        break;
                    }
                    let mut order: Vec<usize> = (0..batch.len()).collect();
                    order.sort_unstable_by_key(|&k| batch[k].offset);
                    arena.clear();
                    at.clear();
                    at.resize(batch.len(), (0usize, 0usize));
                    for &k in &order {
                        let start = arena.len();
                        read_record_into(&t.src, &t.dialect, batch[k].offset, &mut arena)?;
                        at[k] = (start, arena.len());
                    }
                    for (k, r) in batch.iter().enumerate() {
                        e.record(r.offset, &arena[at[k].0..at[k].1])?;
                    }
                    i += batch.len();
                    ctl.set_done(i as u64);
                    ctl.notify();
                    // Aim for about 64 MB of records per batch.
                    let avg = (arena.len() / batch.len()).max(16);
                    batch_rows = ((LIST_BATCH_BYTES / avg).clamp(1 << 12, 1 << 20)).max(1);
                }
            }
            Scope::Logical { start, count } => {
                ctl.set_total(*count);
                let ov = input.overlay.clone();
                let mut pos = *start;
                let mut done = 0u64;
                while done < *count {
                    if ctl.is_cancelled() {
                        return Err(cancelled());
                    }
                    let want = (*count - done).min(4096) as usize;
                    // One extra row tells where the next batch starts.
                    let (rows, end) = logical::rows_from(&mut input.locator, &ov, pos, want + 1)?;
                    let last = end == FetchEnd::Eof && rows.len() <= want;
                    let take = if last {
                        rows.len()
                    } else {
                        rows.len().saturating_sub(1)
                    };
                    for r in &rows[..take] {
                        match r.kind {
                            LKind::Src(span) => e.record_span(&span)?,
                            LKind::New(id) => e.new_rows(&[id])?,
                        }
                    }
                    done += take as u64;
                    ctl.set_done(done);
                    ctl.notify();
                    match rows.get(take) {
                        Some(next) if !last => pos = next.pos,
                        _ => break,
                    }
                }
            }
        }
        e.out.flush()?;
        Ok(SaveReport {
            bytes_written: 0, // filled in by save_with
            records_rewritten: e.rows,
            unmappable: e.w.unmappable,
            used_fallback_quote: e.w.used_fallback_quote,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::encoding::TextEncoding;
    use crate::filter::{FilterData, Row};
    use crate::index::{Index, build};
    use crate::job::{Job, JobState, no_notify};
    use crate::overlay::{ColRef, Deleted};
    use crate::scan::scan_all;
    use crate::source::{AccessMode, Source};

    const DATA: &[u8] = b"id,name,note\n1,Ann,\"a, b\"\n2,Bob,\"say \"\"hi\"\"\"\n3,Cid,plain\n";

    fn offsets() -> Vec<u64> {
        let d = Dialect::default().compile().unwrap();
        scan_all(&d, DATA, 0, 10).iter().map(|r| r.start).collect()
    }

    fn run(ov: Overlay, scope: impl FnOnce(&mut Locator) -> Scope, opts: ExportOptions) -> Vec<u8> {
        run_on(DATA, ov, scope, opts)
    }

    fn run_on(
        data: &[u8],
        ov: Overlay,
        scope: impl FnOnce(&mut Locator) -> Scope,
        opts: ExportOptions,
    ) -> Vec<u8> {
        run_buckets(data, ov, scope, opts, BUCKET_BYTES)
    }

    fn run_buckets(
        data: &[u8],
        ov: Overlay,
        scope: impl FnOnce(&mut Locator) -> Scope,
        opts: ExportOptions,
        bucket_bytes: u64,
    ) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("in.csv");
        std::fs::write(&p, data).unwrap();
        let d = Dialect::default();
        let src = Arc::new(Source::open(&p, AccessMode::Auto).unwrap());
        let compiled = Arc::new(d.compile().unwrap());
        let index = Index::new(0, data.len() as u64);
        let (s, c, i) = (src.clone(), compiled.clone(), index.clone());
        Job::spawn("index", no_notify(), move |ctl| build(&s, &c, &i, ctl)).join();
        let ov = Arc::new(ov);
        let target = Target {
            src: src.clone(),
            dialect: compiled.clone(),
            encoding: d.encoding,
            data_start: 0,
            header_records: 1,
            overlay: Some(ov.clone()),
        };
        let mut locator = Locator::new(src, compiled, index, Some(3), None);
        let scope = scope(&mut locator);
        let input = ExportInput {
            target,
            locator,
            overlay: ov,
            header: Some(0),
            columns: 3,
            rows: 3,
        };
        let out = dir.path().join("out.txt");
        let o2 = out.clone();
        let job = Job::spawn("export", no_notify(), move |ctl| {
            export_with(input, scope, &opts, &o2, ctl, bucket_bytes)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        assert_eq!(job.join(), JobState::Done, "{:?}", job.error());
        std::fs::read(out).unwrap()
    }

    fn opts(delim: char) -> ExportOptions {
        ExportOptions {
            dialect: Dialect {
                delimiter: Delimiter::Char(delim),
                ..Dialect::default()
            },
            policy: QuotePolicy::Minimal,
            line_ending: Term::Lf,
            bom: false,
            columns: None,
        }
    }

    #[test]
    fn converts_delimiter_quoting_and_line_endings() {
        let crlf = ExportOptions {
            line_ending: Term::CrLf,
            ..opts(';')
        };
        let out = run(Overlay::new(), |_| Scope::All, crlf);
        assert_eq!(
            out,
            b"id;name;note\r\n1;Ann;a, b\r\n2;Bob;\"say \"\"hi\"\"\"\r\n3;Cid;plain\r\n"
        );
        let always = ExportOptions {
            policy: QuotePolicy::Always,
            ..opts('\t')
        };
        let out = run(Overlay::new(), |_| Scope::All, always);
        assert!(out.starts_with(b"\"id\"\t\"name\"\t\"note\"\n\"1\"\t\"Ann\"\t\"a, b\"\n"));
        let backslash = ExportOptions {
            dialect: Dialect {
                escape: Escape::Backslash,
                ..Dialect::default()
            },
            ..opts(',')
        };
        let out = run(Overlay::new(), |_| Scope::All, backslash);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\"say \\\"hi\\\"\"")
        );
    }

    #[test]
    fn encodes_and_applies_edits() {
        let offs = offsets();
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[2]), ColRef::Src(1), "Борис".into())],
            |_, _| None,
        );
        ov.delete_rows(
            &[],
            &[Deleted {
                start: offs[3],
                end: DATA.len() as u64,
                records: 1,
            }],
        );
        let ids = ov.insert_rows(END, 0, 1);
        ov.set_cells(
            vec![(RowRef::New(ids[0]), ColRef::Src(0), "4".into())],
            |_, _| None,
        );
        let utf16 = ExportOptions {
            dialect: Dialect {
                encoding: TextEncoding::Utf16Le,
                ..Dialect::default()
            },
            bom: true,
            columns: Some((0, 1)),
            ..opts(',')
        };
        let out = run(ov, |_| Scope::All, utf16);
        let mut want = vec![0xFF, 0xFE];
        TextEncoding::Utf16Le.encode("id,name\n1,Ann\n2,Борис\n4,\n", &mut want);
        assert_eq!(out, want);
    }

    #[test]
    fn exports_a_row_list_in_its_order() {
        let offs = offsets();
        let list: SharedFilter = Arc::new(parking_lot::Mutex::new(FilterData::default()));
        for i in [3usize, 1] {
            list.lock().rows.push(Row {
                offset: offs[i],
                record: i as u64,
            });
        }
        let out = run(Overlay::new(), move |_| Scope::List(list), opts(','));
        assert_eq!(out, b"id,name,note\n3,Cid,plain\n1,Ann,\"a, b\"\n");
    }

    #[test]
    fn a_sort_permutation_is_written_in_order() {
        // 40 rows, listed in reverse order: one in-memory bucket, then
        // (with tiny buckets) one temporary file per few rows.
        let mut data = b"id,name,note
"
        .to_vec();
        for i in 0..40 {
            data.extend_from_slice(
                format!(
                    "{i},n{i},\"q, {i}\"
"
                )
                .as_bytes(),
            );
        }
        let d = Dialect::default().compile().unwrap();
        let recs = scan_all(&d, &data, 0, 100);
        let list = || {
            let mut f = FilterData {
                sorted: true,
                ..FilterData::default()
            };
            for (i, r) in recs.iter().enumerate().skip(1).rev() {
                f.rows.push(Row {
                    offset: r.start,
                    record: i as u64,
                });
            }
            Arc::new(parking_lot::Mutex::new(f))
        };
        let mut want = b"id,name,note
"
        .to_vec();
        for i in (0..40).rev() {
            want.extend_from_slice(
                format!(
                    "{i},n{i},\"q, {i}\"
"
                )
                .as_bytes(),
            );
        }
        for bucket_bytes in [BUCKET_BYTES, 1] {
            let l = list();
            let out = run_buckets(
                &data,
                Overlay::new(),
                move |_| Scope::List(l),
                opts(','),
                bucket_bytes,
            );
            assert_eq!(
                String::from_utf8(out).unwrap(),
                String::from_utf8(want.clone()).unwrap()
            );
        }
    }

    #[test]
    fn plain_records_still_quote_what_the_output_needs() {
        // `a;b` holds the new delimiter, `q"x` the quote; short rows are padded.
        let data = b"x,y,z
a;b,c,d
q\"x,1,2
7
";
        let out = run_on(data, Overlay::new(), |_| Scope::All, opts(';'));
        assert_eq!(
            out,
            b"x;y;z
\"a;b\";c;d
\"q\"\"x\";1;2
7;;
"
        );
    }

    #[test]
    fn exports_a_selection_with_inserted_rows() {
        let offs = offsets();
        let mut ov = Overlay::new();
        let ids = ov.insert_rows(offs[2], 0, 1);
        ov.set_cells(
            vec![(RowRef::New(ids[0]), ColRef::Src(1), "New".into())],
            |_, _| None,
        );
        let start = LPos {
            rec: crate::locate::RowPos {
                offset: offs[1],
                record: 1,
                exact: true,
            },
            sub: 0,
        };
        let out = run(
            ov,
            move |_| Scope::Logical { start, count: 3 },
            ExportOptions {
                columns: Some((0, 1)),
                ..opts('|')
            },
        );
        assert_eq!(out, b"id|name\n1|Ann\n|New\n2|Bob\n");
    }
}
