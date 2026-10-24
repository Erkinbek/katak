//! Export / Save as with conversions: another delimiter, quoting, encoding
//! or line ending, for the whole file, a row list (filter or sort order)
//! or a selection. Every row is decoded and re-encoded with the edits
//! applied; rows are streamed, so memory stays bounded. Comment lines and
//! skipped preamble lines are not exported.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::dialect::Dialect;
use crate::fields::{FieldSpan, split};
use crate::filter::SharedFilter;
use crate::job::JobCtl;
use crate::locate::{FetchEnd, Locator};
use crate::logical::{self, LKind, LPos};
use crate::overlay::{END, Overlay, RowRef};
use crate::save::{SaveError, SaveReport, save_with};
use crate::scan::{Flow, RecSpan, Term};
use crate::search::Target;
use crate::visit::{MAX_RECORD_BYTES, read_record, visit_records};
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
}

struct Exporter<'a, W: Write> {
    target: &'a Target,
    overlay: &'a Overlay,
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

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the export was cancelled")
}

/// Writes the export to `path` (temp file, fsync, atomic rename).
/// `records_rewritten` in the report is the number of rows written.
pub fn export(
    mut input: ExportInput,
    scope: Scope,
    opts: &ExportOptions,
    path: &Path,
    ctl: &JobCtl,
) -> Result<SaveReport, SaveError> {
    save_with(path, false, ctl, |out| {
        let mut e = Exporter {
            target: &input.target,
            overlay: &input.overlay,
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
            Scope::List(list) => {
                let n = list.lock().rows.len();
                ctl.set_total(n as u64);
                for i in 0..n {
                    if ctl.is_cancelled() {
                        return Err(cancelled());
                    }
                    let Some(r) = list.lock().rows.get(i) else {
                        break;
                    };
                    e.record_at(r.offset)?;
                    if i % 4096 == 4095 {
                        ctl.set_done(i as u64 + 1);
                        ctl.notify();
                    }
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
    use crate::dialect::{Delimiter, Escape};
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
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("in.csv");
        std::fs::write(&p, DATA).unwrap();
        let d = Dialect::default();
        let src = Arc::new(Source::open(&p, AccessMode::Auto).unwrap());
        let compiled = Arc::new(d.compile().unwrap());
        let index = Index::new(0, DATA.len() as u64);
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
        };
        let out = dir.path().join("out.txt");
        let o2 = out.clone();
        let job = Job::spawn("export", no_notify(), move |ctl| {
            export(input, scope, &opts, &o2, ctl)
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
