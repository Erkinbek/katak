//! Sparse, incremental, quote-aware record index.
//!
//! A background job scans the data region once with the quote-aware
//! [`Scanner`] and records a [`Checkpoint`] (byte offset + record number) at
//! the first record start after every [`CHECKPOINT_SPACING`] bytes. Records
//! at least that large get a checkpoint of their own, so such a record is
//! alone in its block and its extent is known without scanning it again.
//! Checkpoints are published in batches; readers take a short read lock.

use std::sync::Arc;

use parking_lot::{RwLock, RwLockReadGuard};

use crate::dialect::Compiled;
use crate::job::JobCtl;
use crate::scan::{Feed, Flow, RecSpan, Scanner, Sink, Term};
use crate::source::{ChunkReader, Source};

/// Target distance between checkpoints.
pub const CHECKPOINT_SPACING: u64 = 64 * 1024;
/// Ragged record offsets kept for "jump to next ragged row".
pub const RAGGED_CAP: usize = 1 << 20;
const WARNING_CAP: usize = 64;
const CHUNK: usize = 16 << 20;

/// A true record start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Checkpoint {
    pub offset: u64,
    /// Number of the record starting at `offset` (0 = first record, which is
    /// the header when the file has one).
    pub record: u64,
}

/// Counts of record terminators.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineEndings {
    pub lf: u64,
    pub crlf: u64,
    pub cr: u64,
}

impl LineEndings {
    pub fn add(&mut self, term: Term) {
        match term {
            Term::Lf => self.lf += 1,
            Term::CrLf => self.crlf += 1,
            Term::Cr => self.cr += 1,
            Term::None => {}
        }
    }

    /// The most frequent terminator, if any was seen.
    pub fn dominant(&self) -> Option<Term> {
        let best = [
            (self.crlf, Term::CrLf),
            (self.lf, Term::Lf),
            (self.cr, Term::Cr),
        ]
        .into_iter()
        .max_by_key(|e| e.0)?;
        (best.0 > 0).then_some(best.1)
    }

    pub fn is_mixed(&self) -> bool {
        [self.lf, self.crlf, self.cr]
            .iter()
            .filter(|&&n| n > 0)
            .count()
            > 1
    }

    /// `CRLF`, `LF`, `CR`, `Mixed (CRLF+LF)` or `—`.
    pub fn label(&self) -> String {
        let names: Vec<&str> = [(self.crlf, "CRLF"), (self.lf, "LF"), (self.cr, "CR")]
            .iter()
            .filter(|e| e.0 > 0)
            .map(|e| e.1)
            .collect();
        match names.len() {
            0 => "—".into(),
            1 => names[0].into(),
            _ => format!("Mixed ({})", names.join("+")),
        }
    }
}

/// Problems found while indexing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexWarning {
    /// A quoted field starting at this offset grew past the sanity limit.
    LongQuotedField(u64),
    /// The file ends inside a quoted field opened at this offset.
    UnterminatedQuote(u64),
}

/// Everything the index knows. Read through [`Index::read`].
#[derive(Clone, Debug, Default)]
pub struct IndexData {
    pub data_start: u64,
    pub data_end: u64,
    pub checkpoints: Vec<Checkpoint>,
    /// First record start not yet scanned, and the number of records before
    /// it. Every record before `frontier.offset` is known exactly.
    pub frontier: Checkpoint,
    pub complete: bool,
    /// Field count of record 0, the reference for ragged-row detection.
    pub reference_fields: Option<u64>,
    pub max_fields: u64,
    pub ragged_count: u64,
    /// Offsets of ragged records (at most [`RAGGED_CAP`]).
    pub ragged: Vec<u64>,
    pub line_endings: LineEndings,
    pub comment_lines: u64,
    pub warnings: Vec<IndexWarning>,
    pub error: Option<String>,
}

impl IndexData {
    /// Index of the checkpoint block that contains `off`, if it is in the
    /// indexed region.
    pub fn block_of_offset(&self, off: u64) -> Option<usize> {
        if off >= self.frontier.offset
            || self.checkpoints.is_empty()
            || off < self.checkpoints[0].offset
        {
            return None;
        }
        Some(self.checkpoints.partition_point(|c| c.offset <= off) - 1)
    }

    /// Index of the checkpoint block that contains record `rec`.
    pub fn block_of_record(&self, rec: u64) -> Option<usize> {
        if rec >= self.frontier.record || self.checkpoints.is_empty() {
            return None;
        }
        Some(self.checkpoints.partition_point(|c| c.record <= rec) - 1)
    }

    /// Start and end of block `i`; the end is the next checkpoint or the
    /// frontier.
    pub fn block(&self, i: usize) -> (Checkpoint, Checkpoint) {
        let end = self
            .checkpoints
            .get(i + 1)
            .copied()
            .unwrap_or(self.frontier);
        (self.checkpoints[i], end)
    }

    /// True when block `i` will not change any more.
    pub fn block_is_final(&self, i: usize) -> bool {
        i + 1 < self.checkpoints.len() || self.complete
    }

    /// Average record size so far.
    pub fn avg_record_bytes(&self) -> Option<f64> {
        let bytes = self.frontier.offset.saturating_sub(self.data_start);
        (self.frontier.record > 0 && bytes > 0).then(|| bytes as f64 / self.frontier.record as f64)
    }

    /// Exact record count once complete, otherwise an extrapolation.
    pub fn estimated_records(&self, fallback_avg: Option<f64>) -> u64 {
        if self.complete {
            return self.frontier.record;
        }
        let rest = self.data_end.saturating_sub(self.frontier.offset);
        match self.avg_record_bytes().or(fallback_avg) {
            Some(avg) if avg > 0.0 => self.frontier.record + (rest as f64 / avg).round() as u64,
            _ => self.frontier.record,
        }
    }

    /// Bytes of the data region scanned so far.
    pub fn scanned_bytes(&self) -> u64 {
        self.frontier.offset.saturating_sub(self.data_start)
    }

    /// Heap memory used by the index.
    pub fn heap_bytes(&self) -> usize {
        self.checkpoints.capacity() * std::mem::size_of::<Checkpoint>() + self.ragged.capacity() * 8
    }
}

/// Shared index of one document under one dialect.
#[derive(Debug)]
pub struct Index {
    data: RwLock<IndexData>,
}

impl Index {
    /// An empty index for the data region `[data_start, data_end)`.
    pub fn new(data_start: u64, data_end: u64) -> Arc<Index> {
        Arc::new(Index {
            data: RwLock::new(IndexData {
                data_start,
                data_end,
                frontier: Checkpoint {
                    offset: data_start,
                    record: 0,
                },
                complete: data_start >= data_end,
                ..IndexData::default()
            }),
        })
    }

    pub fn read(&self) -> RwLockReadGuard<'_, IndexData> {
        self.data.read()
    }
}

struct IndexSink {
    new_checkpoints: Vec<Checkpoint>,
    last_checkpoint: Option<u64>,
    next_checkpoint_at: u64,
    records: u64,
    reference_fields: Option<u64>,
    max_fields: u64,
    ragged_count: u64,
    ragged_listed: usize,
    new_ragged: Vec<u64>,
    line_endings: LineEndings,
    comment_lines: u64,
    new_warnings: Vec<IndexWarning>,
    warnings: usize,
}

impl Sink for IndexSink {
    #[inline]
    fn record(&mut self, r: &RecSpan) -> Flow {
        let big = r.end - r.start >= CHECKPOINT_SPACING;
        if (r.start >= self.next_checkpoint_at || big) && self.last_checkpoint != Some(r.start) {
            self.new_checkpoints.push(Checkpoint {
                offset: r.start,
                record: self.records,
            });
            self.last_checkpoint = Some(r.start);
            self.next_checkpoint_at = r.start + CHECKPOINT_SPACING;
        }
        if big {
            // The next record gets a checkpoint too: this one is alone in its block.
            self.next_checkpoint_at = r.end;
        }
        self.records += 1;
        match self.reference_fields {
            None => self.reference_fields = Some(r.fields),
            Some(n) if n != r.fields => {
                self.ragged_count += 1;
                if self.ragged_listed < RAGGED_CAP {
                    self.ragged_listed += 1;
                    self.new_ragged.push(r.start);
                }
            }
            Some(_) => {}
        }
        self.max_fields = self.max_fields.max(r.fields);
        self.line_endings.add(r.term);
        Flow::Continue
    }

    fn comment(&mut self, _start: u64, _end: u64) -> Flow {
        self.comment_lines += 1;
        Flow::Continue
    }

    fn long_quote(&mut self, quote_offset: u64) {
        if self.warnings < WARNING_CAP {
            self.warnings += 1;
            self.new_warnings
                .push(IndexWarning::LongQuotedField(quote_offset));
        }
    }
}

/// Builds the index of `src` under dialect `d`. Runs as a job; returns early
/// (with `Ok`) when cancelled.
pub fn build(src: &Source, d: &Compiled, index: &Index, ctl: &JobCtl) -> Result<(), String> {
    let (start, end) = {
        let g = index.read();
        (g.data_start, g.data_end)
    };
    ctl.set_total(end.saturating_sub(start));
    let mut sink = IndexSink {
        new_checkpoints: Vec::new(),
        last_checkpoint: None,
        next_checkpoint_at: start,
        records: 0,
        reference_fields: None,
        max_fields: 0,
        ragged_count: 0,
        ragged_listed: 0,
        new_ragged: Vec::new(),
        line_endings: LineEndings::default(),
        comment_lines: 0,
        new_warnings: Vec::new(),
        warnings: 0,
    };
    let mut scanner = Scanner::new(d, start);
    let mut reader = ChunkReader::new(src, end, CHUNK);
    loop {
        if ctl.is_cancelled() {
            return Ok(());
        }
        let window = match reader.window(scanner.pos()) {
            Ok(w) => w,
            Err(e) => {
                let msg = format!("reading the file failed: {e}");
                index.data.write().error = Some(msg.clone());
                return Err(msg);
            }
        };
        let done = scanner.feed(window.buf, window.base, window.eof, &mut sink) == Feed::Done;
        {
            let mut g = index.data.write();
            g.checkpoints.append(&mut sink.new_checkpoints);
            g.frontier = Checkpoint {
                offset: if done { end } else { scanner.record_start() },
                record: sink.records,
            };
            g.reference_fields = sink.reference_fields;
            g.max_fields = sink.max_fields;
            g.ragged_count = sink.ragged_count;
            g.ragged.append(&mut sink.new_ragged);
            g.line_endings = sink.line_endings;
            g.comment_lines = sink.comment_lines;
            g.warnings.append(&mut sink.new_warnings);
            if done {
                if let Some(q) = scanner.unterminated_quote() {
                    g.warnings.push(IndexWarning::UnterminatedQuote(q));
                }
                g.complete = true;
            }
        }
        ctl.set_done(scanner.record_start().saturating_sub(start));
        if done {
            ctl.notify_now();
            return Ok(());
        }
        ctl.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::{Job, JobState, no_notify};
    use crate::scan::scan_all;
    use crate::source::AccessMode;

    pub(crate) fn build_sync(
        path: &std::path::Path,
        mode: AccessMode,
        d: &Dialect,
        start: u64,
    ) -> (Arc<Source>, Arc<Index>) {
        let src = Arc::new(Source::open(path, mode).unwrap());
        let c = Arc::new(d.compile().unwrap());
        let index = Index::new(start, src.len());
        let (s2, i2) = (src.clone(), index.clone());
        let job = Job::spawn("index", no_notify(), move |ctl| build(&s2, &c, &i2, ctl));
        assert_eq!(job.join(), JobState::Done);
        (src, index)
    }

    fn sample_file(rows: usize) -> Vec<u8> {
        let mut out = b"id,name,notes\r\n".to_vec();
        for i in 0..rows {
            if i % 7 == 0 {
                out.extend_from_slice(
                    format!("{i},\"multi\nline {i}\",\"q\"\"{i}\"\r\n").as_bytes(),
                );
            } else if i % 11 == 0 {
                out.extend_from_slice(format!("{i},ragged\n").as_bytes());
            } else {
                out.extend_from_slice(format!("{i},name {i},plain\r\n").as_bytes());
            }
        }
        out
    }

    #[test]
    fn checkpoints_are_true_record_starts() {
        let data = sample_file(40_000);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, &data).unwrap();
        let c = Dialect::default().compile().unwrap();
        let all = scan_all(&c, &data, 0, usize::MAX);
        for mode in [AccessMode::Mapped, AccessMode::Buffered] {
            let (_src, index) = build_sync(&p, mode, &Dialect::default(), 0);
            let g = index.read();
            assert!(g.complete);
            assert_eq!(g.frontier.record, all.len() as u64);
            assert_eq!(g.frontier.offset, data.len() as u64);
            assert!(g.checkpoints.len() > 10);
            for cp in &g.checkpoints {
                assert_eq!(all[cp.record as usize].start, cp.offset);
            }
            // Spacing: consecutive checkpoints are at least the spacing apart.
            for w in g.checkpoints.windows(2) {
                assert!(w[1].offset - w[0].offset >= CHECKPOINT_SPACING);
            }
            let ragged = all.iter().skip(1).filter(|r| r.fields != 3).count() as u64;
            assert_eq!(g.ragged_count, ragged);
            assert_eq!(g.max_fields, 3);
            assert!(g.line_endings.is_mixed());
            let checkpoint_bytes = g.checkpoints.len() * std::mem::size_of::<Checkpoint>();
            assert!(checkpoint_bytes < data.len() / 100);
        }
    }

    #[test]
    fn huge_record_gets_its_own_block() {
        let mut data = b"a,b\n1,2\n".to_vec();
        data.extend_from_slice(b"3,\"");
        data.extend(std::iter::repeat_n(b'x', 300_000));
        data.extend_from_slice(b"\"\n4,5\n");
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, &data).unwrap();
        let (_src, index) = build_sync(&p, AccessMode::Mapped, &Dialect::default(), 0);
        let g = index.read();
        let big = g
            .checkpoints
            .iter()
            .position(|c| c.offset == 8)
            .expect("checkpoint at the big record");
        let (a, b) = g.block(big);
        assert_eq!(b.record, a.record + 1);
        assert_eq!(b.offset, data.len() as u64 - 4);
    }

    #[test]
    fn unterminated_quote_is_reported() {
        let data = b"a,b\n\"never closed,1\n2,3\n".to_vec();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, &data).unwrap();
        let (_src, index) = build_sync(&p, AccessMode::Mapped, &Dialect::default(), 0);
        assert_eq!(
            index.read().warnings,
            vec![IndexWarning::UnterminatedQuote(4)]
        );
        assert_eq!(index.read().frontier.record, 2);
    }

    #[test]
    fn empty_and_tiny_files() {
        let dir = tempfile::tempdir().unwrap();
        for (bytes, records) in [
            (&b""[..], 0u64),
            (b"h", 1),
            (b"h\n", 1),
            (b"a,b,\nc,d,\n", 2),
        ] {
            let p = dir.path().join("t.csv");
            std::fs::write(&p, bytes).unwrap();
            let (_src, index) = build_sync(&p, AccessMode::Auto, &Dialect::default(), 0);
            let g = index.read();
            assert!(g.complete);
            assert_eq!(g.frontier.record, records, "{bytes:?}");
        }
    }
}
