//! Saving: stream the source through the overlay into a temp file, then
//! atomically replace the target.
//!
//! Without column changes, only edited records are re-assembled; all other
//! bytes (preamble, comment lines, untouched records, separators and line
//! endings of edited records) are copied verbatim in large blocks, so a save
//! with no edits is a byte-identical copy. Unchanged fields of an edited
//! record keep their raw bytes, original quoting included. With a column
//! order change every record is re-assembled from its raw fields.
//!
//! The temp file lives next to the target, is fsynced, and only then
//! renamed over the target. Any failure (or cancellation) leaves the
//! target untouched; the temp file is removed.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::dialect::{Compiled, Dialect};
use crate::fields::split_all;
use crate::job::JobCtl;
use crate::overlay::{ColRef, END, Overlay};
use crate::scan::{Collect, Feed, RecSpan, Scanner, Term};
use crate::source::{ChunkReader, Source};
use crate::visit::visit_records;
use crate::write::{QuotePolicy, Writer};

const COPY_CHUNK: usize = 8 << 20;
const SMALL_COPY: u64 = 1 << 20;

/// Everything needed to write a document.
#[derive(Clone)]
pub struct SaveInput {
    pub src: Arc<Source>,
    pub dialect: Dialect,
    pub compiled: Arc<Compiled>,
    pub data_start: u64,
    pub overlay: Arc<Overlay>,
    /// Line ending for inserted rows (the file's dominant one).
    pub line_ending: Term,
    /// Logical column count, used to give inserted rows a full width.
    pub columns: usize,
}

/// What a save did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SaveReport {
    pub bytes_written: u64,
    pub records_rewritten: u64,
    /// Characters that the file's encoding cannot represent (written as `?`).
    pub unmappable: usize,
    /// Values needed quotes but the dialect has none; `"` was used.
    pub used_fallback_quote: bool,
}

/// Output that remembers its last bytes (to know whether it ends with a
/// line break before appending rows).
struct Out<W: Write> {
    w: W,
    tail: Vec<u8>,
    written: u64,
}

impl<W: Write> Out<W> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        self.w.write_all(b)?;
        self.written += b.len() as u64;
        self.tail.extend_from_slice(&b[b.len().saturating_sub(4)..]);
        let n = self.tail.len();
        if n > 4 {
            self.tail.drain(..n - 4);
        }
        Ok(())
    }
}

struct Saver<'a, W: Write> {
    input: &'a SaveInput,
    ctl: &'a JobCtl,
    out: Out<W>,
    w: Writer,
    buf: Vec<u8>,
    lf: Vec<u8>,
    cr: Vec<u8>,
    report: SaveReport,
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the save was cancelled")
}

impl<W: Write> Saver<'_, W> {
    /// Copies source bytes `[a, b)` verbatim.
    fn copy(&mut self, a: u64, b: u64) -> io::Result<()> {
        // Short gaps (between rewritten records) come from the mapped
        // views: one system call each would dominate.
        if b > a && b - a <= SMALL_COPY && self.input.src.is_mapped() {
            let bytes = self.input.src.bytes(a, (b - a) as usize)?;
            self.out.put(&bytes)?;
            self.ctl.set_done(b);
            self.ctl.notify();
            return Ok(());
        }
        let mut pos = a;
        let mut chunk = vec![0u8; COPY_CHUNK.min((b.saturating_sub(a)) as usize)];
        while pos < b {
            if self.ctl.is_cancelled() {
                return Err(cancelled());
            }
            let n = ((b - pos) as usize).min(chunk.len());
            self.input.src.read_at(pos, &mut chunk[..n])?;
            self.out.put(&chunk[..n])?;
            pos += n as u64;
            self.ctl.set_done(pos);
            self.ctl.notify();
        }
        Ok(())
    }

    fn ends_with_line_break(&self) -> bool {
        self.out.written == 0
            || self.out.tail.ends_with(&self.lf)
            || self.out.tail.ends_with(&self.cr)
    }

    fn logical_columns(&self) -> Vec<ColRef> {
        match self.input.overlay.columns() {
            Some(c) => c.to_vec(),
            None => (0..self.input.columns.max(1) as u32)
                .map(ColRef::Src)
                .collect(),
        }
    }

    fn write_inserted(&mut self, ids: &[u32]) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if !self.ends_with_line_break() {
            self.buf.clear();
            self.w.line_end(&mut self.buf);
            let b = std::mem::take(&mut self.buf);
            self.out.put(&b)?;
            self.buf = b;
        }
        let cols = self.logical_columns();
        for &id in ids {
            let empty = BTreeMap::new();
            let cells = self.input.overlay.new_row(id).unwrap_or(&empty);
            self.buf.clear();
            let values: Vec<&str> = cols
                .iter()
                .map(|c| cells.get(c).map_or("", String::as_str))
                .collect();
            self.w.record(values, &mut self.buf);
            let b = std::mem::take(&mut self.buf);
            self.out.put(&b)?;
            self.buf = b;
        }
        Ok(())
    }

    /// The record starting at `offset` (its extent, scanned).
    fn record_at(&self, offset: u64) -> io::Result<RecSpan> {
        let src = &self.input.src;
        let mut reader = ChunkReader::new(src, src.len(), 1 << 20);
        let mut sc = Scanner::new(&self.input.compiled, offset);
        let mut out = Vec::new();
        loop {
            let w = reader.window(sc.pos())?;
            let f = sc.feed(
                w.buf,
                w.base,
                w.eof,
                &mut Collect {
                    out: &mut out,
                    max: 1,
                },
            );
            if !out.is_empty() || f == Feed::Done {
                break;
            }
            if self.ctl.is_cancelled() {
                return Err(cancelled());
            }
        }
        out.pop().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no record at byte {offset}"),
            )
        })
    }

    fn read(&self, a: u64, b: u64) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; (b - a) as usize];
        self.input.src.read_at(a, &mut v)?;
        Ok(v)
    }

    /// Re-assembles one record in source column order with its edits.
    fn rewrite_in_place(
        &mut self,
        rec: &RecSpan,
        edits: &BTreeMap<ColRef, String>,
    ) -> io::Result<()> {
        let content = self.read(rec.start, rec.content_end)?;
        let term = self.read(rec.content_end, rec.end)?;
        let fields = split_all(&self.input.compiled, &content);
        let max_edit = edits
            .keys()
            .filter_map(|c| {
                if let ColRef::Src(i) = c {
                    Some(*i as usize + 1)
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);
        let mut b = std::mem::take(&mut self.buf);
        b.clear();
        b.extend_from_slice(&content[..fields[0].start]);
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                b.extend_from_slice(&content[fields[i - 1].end..f.start]);
            }
            match edits.get(&ColRef::Src(i as u32)) {
                Some(v) => self.w.value(v, i == 0, &mut b),
                None => b.extend_from_slice(&content[f.start..f.end]),
            }
        }
        b.extend_from_slice(&content[fields[fields.len() - 1].end..]);
        // Edits past the end of a short (ragged) row extend it.
        for i in fields.len()..max_edit {
            self.w.delimiter(&mut b);
            if let Some(v) = edits.get(&ColRef::Src(i as u32)) {
                self.w.value(v, false, &mut b);
            }
        }
        b.extend_from_slice(&term);
        self.out.put(&b)?;
        self.buf = b;
        self.report.records_rewritten += 1;
        Ok(())
    }

    /// Fast path: no column changes.
    fn save_in_place(&mut self) -> io::Result<()> {
        let ov = self.input.overlay.clone();
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum Ev {
            Insert,
            Delete,
            Edit,
        }
        let mut events: Vec<(u64, Ev)> = Vec::new();
        events.extend(
            ov.anchors()
                .filter(|(a, _)| *a != END)
                .map(|(a, _)| (a, Ev::Insert)),
        );
        events.extend(ov.deleted_ranges().map(|d| (d.start, Ev::Delete)));
        events.extend(ov.edited_offsets().map(|o| (o, Ev::Edit)));
        events.sort_unstable();
        let mut pos = 0u64;
        for (x, ev) in events {
            match ev {
                Ev::Insert => {
                    if x > pos {
                        self.copy(pos, x)?;
                        pos = x;
                    }
                    let ids = ov.inserted_before(x).to_vec();
                    self.write_inserted(&ids)?;
                }
                Ev::Delete => {
                    if x >= pos
                        && let Some(d) = ov.deleted_range(x)
                    {
                        self.copy(pos, x)?;
                        pos = d.end;
                    }
                }
                Ev::Edit => {
                    if x >= pos && !ov.is_deleted(x) {
                        self.copy(pos, x)?;
                        let rec = self.record_at(x)?;
                        if let Some(edits) = ov.row_edits(x) {
                            self.rewrite_in_place(&rec, edits)?;
                        }
                        pos = rec.end;
                    }
                }
            }
        }
        self.copy(pos, self.input.src.len())?;
        let tail = ov.inserted_before(END).to_vec();
        self.write_inserted(&tail)
    }

    /// Column changes: every record is re-assembled in the new order.
    fn save_reordered(&mut self, cols: &[ColRef]) -> io::Result<()> {
        let ov = self.input.overlay.clone();
        let input = self.input;
        self.copy(0, input.data_start)?;
        let max_mapped = cols
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
        let mut prev: Option<(u64, u64)> = None;
        let mut err: Option<io::Error> = None;
        let mut term_bytes: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        for (k, t) in [(0u8, ""), (1, "\n"), (2, "\r\n"), (3, "\r")] {
            let mut v = Vec::new();
            input.dialect.encoding.encode(t, &mut v);
            term_bytes.insert(k, v);
        }
        let res = visit_records(
            &input.src,
            &input.compiled,
            input.data_start,
            0,
            self.ctl,
            None,
            &[],
            |v| {
                let r = v.span;
                let step = (|| -> io::Result<()> {
                    let gap_from = prev.map_or(input.data_start, |p| p.1);
                    if r.start > gap_from {
                        let gap = self.read(gap_from, r.start)?;
                        self.out.put(&gap)?;
                    }
                    let ids: Vec<u32> = ov
                        .anchors_between(prev.map(|p| p.0), r.start)
                        .flat_map(|(_, ids)| ids.iter().copied())
                        .collect();
                    self.write_inserted(&ids)?;
                    if ov.is_deleted(r.start) {
                        return Ok(());
                    }
                    let fields = split_all(&input.compiled, v.content);
                    let empty = BTreeMap::new();
                    let edits = ov.row_edits(r.start).unwrap_or(&empty);
                    let mut b = std::mem::take(&mut self.buf);
                    b.clear();
                    for (j, c) in cols.iter().enumerate() {
                        if j > 0 {
                            self.w.delimiter(&mut b);
                        }
                        match (edits.get(c), c) {
                            (Some(val), _) => self.w.value(val, j == 0, &mut b),
                            (None, ColRef::Src(i)) => {
                                if let Some(f) = fields.get(*i as usize) {
                                    b.extend_from_slice(&v.content[f.start..f.end]);
                                }
                            }
                            (None, ColRef::New(_)) => {}
                        }
                    }
                    // Ragged extras past the mapped columns pass through.
                    for f in fields.iter().skip(max_mapped) {
                        self.w.delimiter(&mut b);
                        b.extend_from_slice(&v.content[f.start..f.end]);
                    }
                    let key = match r.term {
                        Term::None => 0,
                        Term::Lf => 1,
                        Term::CrLf => 2,
                        Term::Cr => 3,
                    };
                    b.extend_from_slice(&term_bytes[&key]);
                    self.out.put(&b)?;
                    self.buf = b;
                    self.report.records_rewritten += 1;
                    Ok(())
                })();
                prev = Some((r.start, r.end));
                match step {
                    Ok(()) => crate::scan::Flow::Continue,
                    Err(e) => {
                        err = Some(e);
                        crate::scan::Flow::Stop
                    }
                }
            },
        );
        if let Some(e) = err {
            return Err(e);
        }
        let outcome = res?;
        if !outcome.completed {
            return Err(cancelled());
        }
        let gap_from = prev.map_or(input.data_start, |p| p.1);
        self.copy(gap_from, input.src.len())?;
        let rest: Vec<u32> = ov
            .anchors_between(prev.map(|p| p.0), END)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
        self.write_inserted(&rest)
    }
}

/// Writes the document (source + overlay) to `out`.
pub fn write_document<W: Write>(input: &SaveInput, out: W, ctl: &JobCtl) -> io::Result<SaveReport> {
    let mut lf = Vec::new();
    let mut cr = Vec::new();
    input.dialect.encoding.encode("\n", &mut lf);
    input.dialect.encoding.encode("\r", &mut cr);
    ctl.set_total(input.src.len());
    let mut s = Saver {
        input,
        ctl,
        out: Out {
            w: out,
            tail: Vec::new(),
            written: 0,
        },
        w: Writer::new(&input.dialect, input.line_ending, QuotePolicy::Minimal),
        buf: Vec::new(),
        lf,
        cr,
        report: SaveReport::default(),
    };
    match input.overlay.columns() {
        None => s.save_in_place()?,
        Some(cols) => {
            let cols = cols.to_vec();
            s.save_reordered(&cols)?;
        }
    }
    s.out.w.flush()?;
    s.report.bytes_written = s.out.written;
    s.report.unmappable = s.w.unmappable;
    s.report.used_fallback_quote = s.w.used_fallback_quote;
    Ok(s.report)
}

/// Writes the source with every match of `matcher` replaced by `with`,
/// for a Replace All too large to keep as undoable edits. Only records
/// with a match are re-assembled; everything else is copied verbatim.
/// The overlay must be empty (save edits first).
pub fn write_replaced<W: Write>(
    input: &SaveInput,
    matcher: &crate::search::Matcher,
    with: &str,
    out: W,
    ctl: &JobCtl,
) -> io::Result<SaveReport> {
    let mut lf = Vec::new();
    let mut cr = Vec::new();
    input.dialect.encoding.encode("\n", &mut lf);
    input.dialect.encoding.encode("\r", &mut cr);
    let mut s = Saver {
        input,
        ctl,
        out: Out {
            w: out,
            tail: Vec::new(),
            written: 0,
        },
        w: Writer::new(&input.dialect, input.line_ending, QuotePolicy::Minimal),
        buf: Vec::new(),
        lf,
        cr,
        report: SaveReport::default(),
    };
    let header_records = input.dialect.has_header as u64;
    let pre = matcher.prefilter(input.dialect.encoding);
    let mut pos = 0u64;
    let mut err: Option<io::Error> = None;
    let res = visit_records(
        &input.src,
        &input.compiled,
        input.data_start,
        0,
        ctl,
        pre.as_ref(),
        &[],
        |v| {
            if v.record < header_records || v.clipped {
                return crate::scan::Flow::Continue;
            }
            let fields = split_all(&input.compiled, v.content);
            let mut changed: Vec<Option<String>> = vec![None; fields.len()];
            let mut any = false;
            for (i, f) in fields.iter().enumerate() {
                if !matcher.wants_column(i) {
                    continue;
                }
                let raw =
                    crate::fields::unquote(&input.compiled, &v.content[f.start..f.end], f.quoted);
                let text = input.dialect.encoding.decode(&raw);
                if matcher.is_match(&text) {
                    let new = matcher.replace(&text, with, true);
                    if new != text {
                        changed[i] = Some(new.into_owned());
                        any = true;
                    }
                }
            }
            if !any {
                return crate::scan::Flow::Continue;
            }
            let r = v.span;
            let step = (|| -> io::Result<()> {
                s.copy(pos, r.start)?;
                let mut b = std::mem::take(&mut s.buf);
                b.clear();
                b.extend_from_slice(&v.content[..fields[0].start]);
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        b.extend_from_slice(&v.content[fields[i - 1].end..f.start]);
                    }
                    match &changed[i] {
                        Some(val) => s.w.value(val, i == 0, &mut b),
                        None => b.extend_from_slice(&v.content[f.start..f.end]),
                    }
                }
                b.extend_from_slice(&v.content[fields[fields.len() - 1].end..]);
                s.out.put(&b)?;
                s.buf = b;
                s.report.records_rewritten += 1;
                pos = r.content_end;
                Ok(())
            })();
            match step {
                Ok(()) => crate::scan::Flow::Continue,
                Err(e) => {
                    err = Some(e);
                    crate::scan::Flow::Stop
                }
            }
        },
    );
    if let Some(e) = err {
        return Err(e);
    }
    if !res?.completed {
        return Err(cancelled());
    }
    s.copy(pos, input.src.len())?;
    s.out.w.flush()?;
    s.report.bytes_written = s.out.written;
    s.report.unmappable = s.w.unmappable;
    s.report.used_fallback_quote = s.w.used_fallback_quote;
    Ok(s.report)
}

/// A save failure with a message for the user. The target is unchanged.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SaveError {
    pub message: String,
    #[source]
    pub source: Option<io::Error>,
}

impl SaveError {
    fn new(what: &str, e: io::Error) -> SaveError {
        let reason = match (e.kind(), e.raw_os_error()) {
            (io::ErrorKind::Interrupted, _) => "The save was cancelled.".to_string(),
            (_, Some(32 | 33)) => "The file is open in another program (for example Excel). Close it there and save again, or use Save As.".to_string(),
            (_, Some(1224)) => "The file is locked by a memory mapping in another program. Close that program, or use Save As.".to_string(),
            (io::ErrorKind::PermissionDenied, _) | (_, Some(5)) => {
                "Permission denied: the file or its folder is read-only, or you are not allowed to write there. Use Save As to save elsewhere.".to_string()
            }
            (io::ErrorKind::StorageFull, _) | (_, Some(112)) => "There is not enough free disk space.".to_string(),
            _ => format!("{what}: {e}"),
        };
        SaveError {
            message: format!("{reason} The original file was not changed."),
            source: Some(e),
        }
    }
}

/// Writes the document to a temp file next to `target`, fsyncs it, makes
/// an optional `.bak` of the old target, and renames the temp file over
/// `target`.
pub fn save_file(
    input: &SaveInput,
    target: &Path,
    backup: bool,
    ctl: &JobCtl,
) -> Result<SaveReport, SaveError> {
    save_with(target, backup, ctl, |w| write_document(input, w, ctl))
}

/// The temp-file / fsync / backup / atomic-rename procedure around any
/// writer (save, streaming replace, export).
pub fn save_with<F>(
    target: &Path,
    backup: bool,
    ctl: &JobCtl,
    write: F,
) -> Result<SaveReport, SaveError>
where
    F: FnOnce(&mut BufWriter<&mut fs::File>) -> io::Result<SaveReport>,
{
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".katak.tmp")
        .tempfile_in(&dir)
        .map_err(|e| SaveError::new("Cannot create a temporary file next to the target", e))?;
    let report = {
        let file = tmp.as_file_mut();
        let mut w = BufWriter::with_capacity(8 << 20, file);
        let mut report = write(&mut w).map_err(|e| SaveError::new("Writing failed", e))?;
        w.flush().map_err(|e| SaveError::new("Writing failed", e))?;
        drop(w);
        if let Ok(m) = tmp.as_file().metadata() {
            report.bytes_written = m.len();
        }
        report
    };
    tmp.as_file()
        .sync_all()
        .map_err(|e| SaveError::new("Flushing to disk failed", e))?;
    if let Ok(meta) = fs::metadata(target) {
        let _ = fs::set_permissions(tmp.path(), meta.permissions());
        if backup {
            make_backup(target).map_err(|e| SaveError::new("Creating the backup failed", e))?;
        }
    }
    if ctl.is_cancelled() {
        return Err(SaveError::new("", cancelled()));
    }
    // std::fs::rename uses POSIX semantics on Windows, so it replaces the
    // target even while this process still has it open and mapped (the
    // old views keep reading the old data). MoveFileExW, which tempfile's
    // persist uses, fails with "access denied" in that case.
    let tmp_path = tmp.into_temp_path();
    fs::rename(&tmp_path, target).map_err(|e| SaveError::new("Replacing the file failed", e))?;
    // The temp name no longer exists; don't try to delete it.
    let _ = tmp_path.keep();
    Ok(report)
}

/// `name.ext` -> `name.ext.bak`: a hard link to the current file (cheap,
/// and the old contents survive the rename), or a copy where links are
/// not supported.
fn make_backup(target: &Path) -> io::Result<()> {
    let mut bak = target.as_os_str().to_owned();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    if bak.exists() {
        fs::remove_file(&bak)?;
    }
    if fs::hard_link(target, &bak).is_err() {
        fs::copy(target, &bak)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Delimiter;
    use crate::encoding::TextEncoding;
    use crate::job::{Job, JobState, no_notify};
    use crate::overlay::{Deleted, RowRef};
    use crate::scan::scan_all;
    use crate::source::AccessMode;

    struct Fx {
        _dir: tempfile::TempDir,
        path: PathBuf,
        src: Arc<Source>,
        d: Dialect,
    }

    fn fixture(bytes: &[u8], d: Dialect) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.csv");
        fs::write(&path, bytes).unwrap();
        let src = Arc::new(Source::open(&path, AccessMode::Auto).unwrap());
        Fx {
            _dir: dir,
            path,
            src,
            d,
        }
    }

    fn input(fx: &Fx, ov: Overlay, columns: usize) -> SaveInput {
        let bom = fx.d.encoding.bom();
        let all = fs::read(&fx.path).unwrap();
        let data_start = if !bom.is_empty() && all.starts_with(bom) {
            bom.len() as u64
        } else {
            0
        };
        SaveInput {
            src: fx.src.clone(),
            dialect: fx.d.clone(),
            compiled: Arc::new(fx.d.compile().unwrap()),
            data_start,
            overlay: Arc::new(ov),
            line_ending: Term::Lf,
            columns,
        }
    }

    fn render(inp: &SaveInput) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        let inp = inp.clone();
        let job = Job::spawn("save", no_notify(), move |ctl| {
            let mut out = Vec::new();
            write_document(&inp, &mut out, ctl).map_err(|e| e.to_string())?;
            tx.send(out).unwrap();
            Ok(())
        });
        assert_eq!(job.join(), JobState::Done, "{:?}", job.error());
        rx.recv().unwrap()
    }

    fn record_offsets(bytes: &[u8], d: &Dialect) -> Vec<u64> {
        scan_all(&d.compile().unwrap(), bytes, 0, usize::MAX)
            .iter()
            .map(|r| r.start)
            .collect()
    }

    #[test]
    fn zero_edits_is_byte_identical() {
        let cases: Vec<(&[u8], Dialect)> = vec![
            (b"a,b\r\n1,\"x\r\ny\"\n2,3\r4,5", Dialect::default()),
            (
                b"\xEF\xBB\xBFh1;h2\n\"q\"\"q\";2\n",
                Dialect {
                    delimiter: Delimiter::Char(';'),
                    ..Dialect::default()
                },
            ),
            (b"", Dialect::default()),
            (b"only header", Dialect::default()),
            (
                b"#c\na\n\n\nb\n",
                Dialect {
                    comment_prefix: Some("#".into()),
                    ..Dialect::default()
                },
            ),
            (
                b"  a   b  \n c d\n",
                Dialect {
                    delimiter: Delimiter::Whitespace,
                    ..Dialect::default()
                },
            ),
            (b"x\xFF\x00y,\"bad\n", Dialect::default()),
        ];
        for (bytes, d) in cases {
            let fx = fixture(bytes, d);
            assert_eq!(render(&input(&fx, Overlay::new(), 2)), bytes);
        }
        // UTF-16 too.
        let mut u = vec![0xFF, 0xFE];
        TextEncoding::Utf16Le.encode("a,b\r\n1,2\r\n", &mut u);
        let fx = fixture(
            &u,
            Dialect {
                encoding: TextEncoding::Utf16Le,
                ..Dialect::default()
            },
        );
        assert_eq!(render(&input(&fx, Overlay::new(), 2)), u);
    }

    #[test]
    fn edits_touch_only_their_records() {
        let src = b"id,name,note\r\n1,\"Ann\",x\r\n2,Bob,\"keep, quoted\"\r\n3,Cid,z\r\n";
        let d = Dialect::default();
        let fx = fixture(src, d.clone());
        let offs = record_offsets(src, &d);
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[2]), ColRef::Src(1), "Robert, Jr.".into())],
            |_, _| None,
        );
        let out = render(&input(&fx, ov, 3));
        assert_eq!(
            out,
            b"id,name,note\r\n1,\"Ann\",x\r\n2,\"Robert, Jr.\",\"keep, quoted\"\r\n3,Cid,z\r\n"
        );
    }

    #[test]
    fn edit_past_the_end_of_a_short_row_extends_it() {
        let src = b"a,b,c\n1\n";
        let fx = fixture(src, Dialect::default());
        let offs = record_offsets(src, &fx.d);
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[1]), ColRef::Src(2), "z".into())],
            |_, _| None,
        );
        assert_eq!(render(&input(&fx, ov, 3)), b"a,b,c\n1,,z\n");
    }

    #[test]
    fn inserts_and_deletes() {
        let src = b"h\n1\n2\n3\n4";
        let fx = fixture(src, Dialect::default());
        let offs = record_offsets(src, &fx.d);
        let mut ov = Overlay::new();
        let ids = ov.insert_rows(offs[2], 0, 1);
        ov.set_cells(
            vec![(RowRef::New(ids[0]), ColRef::Src(0), "new, row".into())],
            |_, _| None,
        );
        let end = ov.insert_rows(END, 0, 1);
        ov.set_cells(
            vec![(RowRef::New(end[0]), ColRef::Src(0), "last".into())],
            |_, _| None,
        );
        ov.delete_rows(
            &[],
            &[Deleted {
                start: offs[3],
                end: offs[4],
                records: 1,
            }],
        );
        // No trailing newline in the source: one is added before appended rows.
        assert_eq!(
            render(&input(&fx, ov, 1)),
            b"h\n1\n\"new, row\"\n2\n4\nlast\n"
        );
    }

    #[test]
    fn column_changes_rewrite_every_record() {
        let src = b"a,b,c\n1,\"2,x\",3\n#comment\n4,5,6,extra\n";
        let d = Dialect {
            comment_prefix: Some("#".into()),
            ..Dialect::default()
        };
        let fx = fixture(src, d.clone());
        let offs = record_offsets(src, &d);
        let mut ov = Overlay::new();
        let nc = ov.new_column_id();
        ov.set_columns(Some(vec![ColRef::Src(2), nc, ColRef::Src(0)]));
        ov.set_cells(
            vec![
                (RowRef::Src(offs[0]), nc, "new".into()),
                (RowRef::Src(offs[2]), nc, "n2".into()),
            ],
            |_, _| None,
        );
        assert_eq!(
            render(&input(&fx, ov, 3)),
            b"c,new,a\n3,,1\n#comment\n6,n2,4,extra\n"
        );
    }

    #[test]
    fn utf16_edit() {
        let mut src = vec![0xFF, 0xFE];
        TextEncoding::Utf16Le.encode("a,b\r\nx,y\r\n", &mut src);
        let d = Dialect {
            encoding: TextEncoding::Utf16Le,
            ..Dialect::default()
        };
        let fx = fixture(&src, d.clone());
        let c = d.compile().unwrap();
        let offs: Vec<u64> = scan_all(&c, &src, 2, usize::MAX)
            .iter()
            .map(|r| r.start)
            .collect();
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[1]), ColRef::Src(1), "я,z".into())],
            |_, _| None,
        );
        let mut want = vec![0xFF, 0xFE];
        TextEncoding::Utf16Le.encode("a,b\r\nx,\"я,z\"\r\n", &mut want);
        assert_eq!(render(&input(&fx, ov, 2)), want);
    }

    #[test]
    fn save_file_replaces_atomically_while_the_source_is_open() {
        let src = b"a,b\n1,2\n";
        let fx = fixture(src, Dialect::default());
        // Keep a mapped view of the original alive during the save.
        let held = fx.src.bytes(0, 4).unwrap();
        let offs = record_offsets(src, &fx.d);
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[1]), ColRef::Src(0), "9".into())],
            |_, _| None,
        );
        let inp = input(&fx, ov, 2);
        let target = fx.path.clone();
        let job = Job::spawn("save", no_notify(), move |ctl| {
            save_file(&inp, &target, true, ctl)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let state = job.join();
        drop(held);
        assert_eq!(state, JobState::Done, "{:?}", job.error());
        assert_eq!(fs::read(&fx.path).unwrap(), b"a,b\n9,2\n");
        let mut bak = fx.path.as_os_str().to_owned();
        bak.push(".bak");
        assert_eq!(fs::read(PathBuf::from(bak)).unwrap(), src);
        // No temp files left behind.
        let leftovers = fs::read_dir(fx.path.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".katak.tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn failed_save_leaves_the_target_untouched() {
        let src = b"a\n1\n";
        let fx = fixture(src, Dialect::default());
        let inp = input(&fx, Overlay::new(), 1);
        // Target in a folder that does not exist.
        let target = fx.path.parent().unwrap().join("missing").join("out.csv");
        let job = Job::spawn("save", no_notify(), move |ctl| {
            save_file(&inp, &target, false, ctl)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        assert_eq!(job.join(), JobState::Failed);
        assert!(
            job.error()
                .unwrap()
                .contains("original file was not changed")
        );
        assert_eq!(fs::read(&fx.path).unwrap(), src);
    }
}
