//! Locating records by number, by byte offset or relative to a position.
//!
//! Inside the indexed region (before the index frontier) every answer is
//! exact: jump to a checkpoint, scan forward at most one block. Past the
//! frontier, positions near it are scanned exactly from it; anything further
//! is found by *resyncing*: from the target byte, try successive line starts
//! and accept the first one where the next few records parse with the
//! expected field count. Such positions are marked approximate and
//! [`Locator::refine`] makes them exact once the indexer has caught up.
//!
//! Every call scans at most [`UI_SCAN_BUDGET`] bytes, so it is safe on the UI
//! thread.

use std::io;
use std::sync::Arc;

use crate::dialect::Compiled;
use crate::index::{Checkpoint, Index, IndexWarning};
use crate::scan::{Feed, Flow, RecSpan, Scanner, Sink, Term};
use crate::source::{ChunkReader, Source};
use crate::units::{CR, LF, Units};
use crate::with_units;

/// Maximum bytes scanned by one call.
pub const UI_SCAN_BUDGET: usize = 4 << 20;
const RESYNC_WINDOW: usize = 1 << 20;
const RESYNC_CANDIDATES: usize = 256;
const RESYNC_PROBE: usize = 8;
const PROBE_BYTES: usize = 256 << 10;
/// Records kept in the block cache (~40 bytes each).
const BLOCK_CACHE_RECORDS: usize = 256 << 10;
const SINGLE_RECORD_SYNTH: u64 = 1 << 20;

/// A record position. `record` counts from 0 (the header, if any).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowPos {
    pub offset: u64,
    pub record: u64,
    /// False when found by the resync heuristic or extrapolated.
    pub exact: bool,
}

/// Why [`Locator::records_from`] returned fewer records than asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchEnd {
    Filled,
    Eof,
    /// The scan budget ran out; `partial` (if any) is a record that started
    /// but did not end within the budget.
    Budget,
}

/// Records returned by [`Locator::records_from`].
#[derive(Clone, Debug)]
pub struct Fetched {
    pub rows: Vec<(RowPos, RecSpan)>,
    pub end: FetchEnd,
    pub partial: Option<RowPos>,
}

/// Finds records; owns a small cache of decoded checkpoint blocks.
pub struct Locator {
    src: Arc<Source>,
    d: Arc<Compiled>,
    index: Arc<Index>,
    data_start: u64,
    len: u64,
    /// Field count records are expected to have (header / first record).
    expected_fields: Option<u64>,
    /// Average record size from the open-time sample, used before the
    /// indexer has seen anything.
    sample_avg: Option<f64>,
    blocks: Vec<(usize, Arc<Vec<RecSpan>>, u64)>,
    cached_records: usize,
    tick: u64,
}

struct Until<'a> {
    out: &'a mut Vec<RecSpan>,
    max: usize,
    /// Stop (without collecting) at a record starting at or after this.
    stop_start_at: u64,
    /// Stop after collecting a record ending after this.
    stop_end_after: u64,
}

impl Sink for Until<'_> {
    fn record(&mut self, r: &RecSpan) -> Flow {
        if r.start >= self.stop_start_at {
            return Flow::Stop;
        }
        self.out.push(*r);
        if self.out.len() >= self.max || r.end > self.stop_end_after {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

struct ScanOut {
    recs: Vec<RecSpan>,
    /// Start of the record after the last one collected.
    next: u64,
    outcome: Feed,
}

impl Locator {
    pub fn new(
        src: Arc<Source>,
        d: Arc<Compiled>,
        index: Arc<Index>,
        expected_fields: Option<u64>,
        sample_avg: Option<f64>,
    ) -> Self {
        let (data_start, len) = {
            let g = index.read();
            (g.data_start, g.data_end)
        };
        Locator {
            src,
            d,
            index,
            data_start,
            len,
            expected_fields,
            sample_avg,
            blocks: Vec::new(),
            cached_records: 0,
            tick: 0,
        }
    }

    pub fn data_start(&self) -> u64 {
        self.data_start
    }

    pub fn data_end(&self) -> u64 {
        self.len
    }

    /// The first record.
    pub fn first(&self) -> RowPos {
        RowPos {
            offset: self.data_start,
            record: 0,
            exact: true,
        }
    }

    /// End-of-data position (one past the last record).
    pub fn end_pos(&self) -> RowPos {
        let g = self.index.read();
        RowPos {
            offset: self.len,
            record: g.estimated_records(self.sample_avg),
            exact: g.complete,
        }
    }

    fn expected(&self) -> Option<u64> {
        self.index.read().reference_fields.or(self.expected_fields)
    }

    fn estimate_record(&self, off: u64) -> u64 {
        let g = self.index.read();
        let fr = g.frontier;
        if off <= fr.offset {
            return fr.record;
        }
        match g.avg_record_bytes().or(self.sample_avg) {
            Some(avg) if avg > 0.0 => fr.record + ((off - fr.offset) as f64 / avg) as u64,
            _ => fr.record,
        }
    }

    // ------------------------------------------------------------ scanning

    fn scan_forward(
        &self,
        start: u64,
        max: usize,
        budget: usize,
        stop_start_at: u64,
        stop_end_after: u64,
    ) -> io::Result<ScanOut> {
        let mut recs = Vec::new();
        if start >= self.len || max == 0 {
            return Ok(ScanOut {
                recs,
                next: start,
                outcome: if start >= self.len {
                    Feed::Done
                } else {
                    Feed::Stopped
                },
            });
        }
        let mut sink = Until {
            out: &mut recs,
            max,
            stop_start_at,
            stop_end_after,
        };
        let mut reader = ChunkReader::cached(&self.src, self.len, 64 << 10);
        let mut sc = Scanner::new(&self.d, start);
        let outcome = loop {
            let w = reader.window(sc.pos())?;
            let f = sc.feed(w.buf, w.base, w.eof, &mut sink);
            if f != Feed::NeedMore || sc.pos() - start >= budget as u64 {
                break f;
            }
        };
        Ok(ScanOut {
            next: sc.record_start(),
            recs,
            outcome,
        })
    }

    fn block_records(
        &mut self,
        i: usize,
        a: Checkpoint,
        b: Checkpoint,
        is_final: bool,
    ) -> io::Result<Arc<Vec<RecSpan>>> {
        self.tick += 1;
        if is_final && let Some(e) = self.blocks.iter_mut().find(|e| e.0 == i) {
            e.2 = self.tick;
            return Ok(e.1.clone());
        }
        let count = (b.record - a.record) as usize;
        let recs = if count == 1 && b.offset - a.offset > SINGLE_RECORD_SYNTH {
            vec![self.single_record(a.offset, b.offset)?]
        } else {
            let bytes = self.src.bytes(a.offset, (b.offset - a.offset) as usize)?;
            let mut out = Vec::with_capacity(count);
            let mut sc = Scanner::new(&self.d, a.offset);
            let mut sink = Until {
                out: &mut out,
                max: count.max(1),
                stop_start_at: b.offset,
                stop_end_after: u64::MAX,
            };
            sc.feed(&bytes, a.offset, true, &mut sink);
            out
        };
        let recs = Arc::new(recs);
        if is_final {
            self.cached_records += recs.len();
            while self.cached_records > BLOCK_CACHE_RECORDS && !self.blocks.is_empty() {
                let old = self
                    .blocks
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.2)
                    .map_or(0, |(k, _)| k);
                self.cached_records -= self.blocks.swap_remove(old).1.len();
            }
            self.blocks.push((i, recs.clone(), self.tick));
        }
        Ok(recs)
    }

    /// The extent of a record known to span exactly `[start, end)`, without
    /// scanning it. The field count is unknown (0).
    fn single_record(&self, start: u64, end: u64) -> io::Result<RecSpan> {
        let w = self.d.width.bytes() as u64;
        let tail_start = end.saturating_sub(2 * w).max(start);
        let tail = self.src.bytes(tail_start, (end - tail_start) as usize)?;
        let units: Vec<u32> = with_units!(self.d.width, U => (0..tail.len() / U::W).map(|k| U::get(&tail, k * U::W)).collect());
        let last = units.last().copied();
        let prev = if units.len() >= 2 {
            Some(units[units.len() - 2])
        } else {
            None
        };
        let unterminated =
            self.index.read().warnings.iter().any(
                |wn| matches!(wn, IndexWarning::UnterminatedQuote(q) if (start..end).contains(q)),
            );
        let (content_end, term) = match (prev, last) {
            _ if unterminated => (end, Term::None),
            (Some(p), Some(l)) if p == CR && l == LF => (end - 2 * w, Term::CrLf),
            (_, Some(l)) if l == LF => (end - w, Term::Lf),
            (_, Some(l)) if l == CR => (end - w, Term::Cr),
            _ => (end, Term::None),
        };
        // Not scanned, so unknown: report escapes to stay conservative.
        Ok(RecSpan {
            start,
            content_end,
            end,
            fields: 0,
            term,
            escaped: true,
        })
    }

    /// Indexed block containing `off` (when `off` is before the frontier).
    fn indexed_block(&self, off: u64) -> Option<(usize, Checkpoint, Checkpoint, bool)> {
        let g = self.index.read();
        let i = g.block_of_offset(off)?;
        let (a, b) = g.block(i);
        Some((i, a, b, g.block_is_final(i)))
    }

    // --------------------------------------------------------------- API

    /// Up to `n` records starting with the one at `from`.
    pub fn records_from(&mut self, from: RowPos, n: usize) -> io::Result<Fetched> {
        let mut rows = Vec::with_capacity(n.min(4096));
        let mut cur = self.refine(from)?;
        let mut budget = UI_SCAN_BUDGET;
        while rows.len() < n {
            if cur.offset >= self.len {
                return Ok(Fetched {
                    rows,
                    end: FetchEnd::Eof,
                    partial: None,
                });
            }
            if cur.exact
                && let Some((i, a, b, fin)) = self.indexed_block(cur.offset)
            {
                let recs = self.block_records(i, a, b, fin)?;
                let k = recs.partition_point(|r| r.start < cur.offset);
                for (j, r) in recs.iter().enumerate().skip(k) {
                    if rows.len() >= n {
                        break;
                    }
                    rows.push((
                        RowPos {
                            offset: r.start,
                            record: a.record + j as u64,
                            exact: true,
                        },
                        *r,
                    ));
                }
                cur = RowPos {
                    offset: b.offset,
                    record: b.record,
                    exact: true,
                };
                continue;
            }
            if budget == 0 {
                return Ok(Fetched {
                    rows,
                    end: FetchEnd::Budget,
                    partial: Some(cur),
                });
            }
            let out = self.scan_forward(cur.offset, n - rows.len(), budget, u64::MAX, u64::MAX)?;
            budget = budget.saturating_sub((out.next - cur.offset).max(1) as usize);
            for (j, r) in out.recs.iter().enumerate() {
                rows.push((
                    RowPos {
                        offset: r.start,
                        record: cur.record + j as u64,
                        exact: cur.exact,
                    },
                    *r,
                ));
            }
            let next = RowPos {
                offset: out.next,
                record: cur.record + out.recs.len() as u64,
                exact: cur.exact,
            };
            match out.outcome {
                Feed::Done => {
                    return Ok(Fetched {
                        rows,
                        end: FetchEnd::Eof,
                        partial: None,
                    });
                }
                Feed::NeedMore => {
                    return Ok(Fetched {
                        rows,
                        end: FetchEnd::Budget,
                        partial: Some(next),
                    });
                }
                Feed::Stopped => cur = next,
            }
        }
        Ok(Fetched {
            rows,
            end: FetchEnd::Filled,
            partial: None,
        })
    }

    /// Up to `n` records ending just before `before`, in file order.
    pub fn records_before(
        &mut self,
        before: RowPos,
        n: usize,
    ) -> io::Result<Vec<(RowPos, RecSpan)>> {
        let mut rev: Vec<(RowPos, RecSpan)> = Vec::new();
        let mut cur = self.refine(before)?;
        let mut attempts = 0;
        while rev.len() < n && cur.offset > self.data_start && attempts < 64 {
            attempts += 1;
            let fr = self.index.read().frontier;
            if cur.exact && cur.offset <= fr.offset {
                let Some((i, a, b, fin)) = self.indexed_block(cur.offset - 1) else {
                    break;
                };
                let recs = self.block_records(i, a, b, fin)?;
                let k = recs.partition_point(|r| r.start < cur.offset);
                for j in (0..k).rev() {
                    if rev.len() >= n {
                        break;
                    }
                    rev.push((
                        RowPos {
                            offset: recs[j].start,
                            record: a.record + j as u64,
                            exact: true,
                        },
                        recs[j],
                    ));
                }
                cur = RowPos {
                    offset: a.offset,
                    record: a.record,
                    exact: true,
                };
                continue;
            }
            // Past the frontier: scan from the frontier when it is close,
            // otherwise from a resync point further and further back.
            let mut found = Vec::new();
            let mut from_frontier = false;
            for back in [64u64 << 10, 1 << 20, UI_SCAN_BUDGET as u64] {
                let probe = cur.offset.saturating_sub(back);
                let start = if probe <= fr.offset {
                    from_frontier = true;
                    fr.offset
                } else {
                    self.resync(probe)?
                };
                if start >= cur.offset {
                    continue;
                }
                let out =
                    self.scan_forward(start, usize::MAX, UI_SCAN_BUDGET, cur.offset, u64::MAX)?;
                if !out.recs.is_empty() {
                    found = out.recs;
                    break;
                }
                if from_frontier {
                    break;
                }
            }
            if found.is_empty() {
                break;
            }
            let exact = from_frontier && cur.exact;
            let first_record = if from_frontier {
                fr.record
            } else {
                cur.record.saturating_sub(found.len() as u64)
            };
            for (j, r) in found.iter().enumerate().rev() {
                if rev.len() >= n {
                    break;
                }
                rev.push((
                    RowPos {
                        offset: r.start,
                        record: first_record + j as u64,
                        exact,
                    },
                    *r,
                ));
            }
            cur = RowPos {
                offset: found[0].start,
                record: first_record,
                exact,
            };
        }
        rev.reverse();
        Ok(rev)
    }

    /// Record number `record`.
    pub fn locate_record(&mut self, record: u64) -> io::Result<RowPos> {
        let (blk, fr, complete, avg) = {
            let g = self.index.read();
            let blk = g.block_of_record(record).map(|i| {
                let (a, b) = g.block(i);
                (i, a, b, g.block_is_final(i))
            });
            (
                blk,
                g.frontier,
                g.complete,
                g.avg_record_bytes().or(self.sample_avg),
            )
        };
        if let Some((i, a, b, fin)) = blk {
            let recs = self.block_records(i, a, b, fin)?;
            let k = ((record - a.record) as usize).min(recs.len().saturating_sub(1));
            if let Some(r) = recs.get(k) {
                return Ok(RowPos {
                    offset: r.start,
                    record: a.record + k as u64,
                    exact: true,
                });
            }
        }
        if complete {
            return Ok(self.last()?.unwrap_or_else(|| self.first()));
        }
        let ahead = record.saturating_sub(fr.record);
        let est = fr
            .offset
            .saturating_add((ahead as f64 * avg.unwrap_or(100.0)) as u64)
            .min(self.len.saturating_sub(1));
        if est.saturating_sub(fr.offset) <= UI_SCAN_BUDGET as u64 {
            let out = self.scan_forward(
                fr.offset,
                ahead as usize + 1,
                UI_SCAN_BUDGET,
                u64::MAX,
                u64::MAX,
            )?;
            if let Some(r) = out.recs.get(ahead as usize) {
                return Ok(RowPos {
                    offset: r.start,
                    record,
                    exact: true,
                });
            }
            if let Some(r) = out.recs.last()
                && out.outcome == Feed::Done
            {
                return Ok(RowPos {
                    offset: r.start,
                    record: fr.record + out.recs.len() as u64 - 1,
                    exact: true,
                });
            }
        }
        let at = self.resync(est)?;
        Ok(RowPos {
            offset: at,
            record,
            exact: false,
        })
    }

    /// The record containing byte `off` (clamped to the data region).
    pub fn locate_offset(&mut self, off: u64) -> io::Result<RowPos> {
        let off = off.clamp(
            self.data_start,
            self.len.saturating_sub(1).max(self.data_start),
        );
        if let Some((i, a, b, fin)) = self.indexed_block(off) {
            let recs = self.block_records(i, a, b, fin)?;
            let k = recs
                .partition_point(|r| r.end <= off)
                .min(recs.len().saturating_sub(1));
            if let Some(r) = recs.get(k) {
                return Ok(RowPos {
                    offset: r.start,
                    record: a.record + k as u64,
                    exact: true,
                });
            }
        }
        let fr = self.index.read().frontier;
        if off >= fr.offset && off - fr.offset <= UI_SCAN_BUDGET as u64 {
            let out = self.scan_forward(fr.offset, usize::MAX, UI_SCAN_BUDGET, u64::MAX, off)?;
            if let Some(r) = out.recs.last()
                && (r.end > off || out.outcome == Feed::Done)
            {
                return Ok(RowPos {
                    offset: r.start,
                    record: fr.record + out.recs.len() as u64 - 1,
                    exact: true,
                });
            }
        }
        let at = self.resync(off)?;
        Ok(RowPos {
            offset: at,
            record: self.estimate_record(at),
            exact: false,
        })
    }

    /// Makes an approximate position exact when the index (or a short scan
    /// from its frontier) covers it.
    pub fn refine(&mut self, pos: RowPos) -> io::Result<RowPos> {
        if pos.exact || pos.offset >= self.len {
            return Ok(pos);
        }
        let fr = self.index.read().frontier;
        if pos.offset < fr.offset || pos.offset - fr.offset <= (1 << 20) {
            return self.locate_offset(pos.offset);
        }
        Ok(pos)
    }

    /// The first ragged record (field count differs from record 0) after
    /// record `after`, searching the indexed region only.
    pub fn next_ragged(&mut self, after: u64) -> io::Result<Option<RowPos>> {
        let (start_block, reference) = {
            let g = self.index.read();
            (g.block_of_record(after), g.reference_fields)
        };
        let (Some(mut from), Some(reference)) = (start_block, reference) else {
            return Ok(None);
        };
        loop {
            let next = {
                let g = self.index.read();
                g.next_ragged_block(from).map(|i| {
                    let (a, b) = g.block(i);
                    (i, a, b, g.block_is_final(i))
                })
            };
            let Some((i, a, b, fin)) = next else {
                return Ok(None);
            };
            let recs = self.block_records(i, a, b, fin)?;
            for (j, r) in recs.iter().enumerate() {
                let rec = a.record + j as u64;
                if rec > after && r.fields != 0 && r.fields != reference {
                    return Ok(Some(RowPos {
                        offset: r.start,
                        record: rec,
                        exact: true,
                    }));
                }
            }
            from = i + 1;
        }
    }

    /// The last record, if there is any.
    pub fn last(&mut self) -> io::Result<Option<RowPos>> {
        let end = self.end_pos();
        Ok(self.records_before(end, 1)?.pop().map(|(p, _)| p))
    }

    /// A plausible record start at or after `off` (see module docs).
    pub fn resync(&self, off: u64) -> io::Result<u64> {
        if off <= self.data_start {
            return Ok(self.data_start);
        }
        if off >= self.len {
            return Ok(self.len);
        }
        let w = self.d.width.bytes() as u64;
        let off = self.data_start + (off - self.data_start) / w * w;
        let from = off - w;
        let bytes = self.src.bytes(from, RESYNC_WINDOW)?;
        let mut candidates = Vec::new();
        with_units!(self.d.width, U => {
            let n = bytes.len() / U::W;
            let mut prev = U::get(&bytes, 0);
            for k in 1..n {
                let u = U::get(&bytes, k * U::W);
                if prev == LF || (prev == CR && u != LF) {
                    candidates.push(from + (k * U::W) as u64);
                    if candidates.len() >= RESYNC_CANDIDATES {
                        break;
                    }
                }
                prev = u;
            }
        });
        for &c in &candidates {
            if self.probe(c)? {
                return Ok(c);
            }
        }
        Ok(candidates.first().copied().unwrap_or(off))
    }

    /// Do the records starting at `at` look like real records?
    fn probe(&self, at: u64) -> io::Result<bool> {
        let Some(expect) = self.expected() else {
            return Ok(true);
        };
        let out = self.scan_forward(at, RESYNC_PROBE, PROBE_BYTES, u64::MAX, u64::MAX)?;
        let enough = out.recs.len() >= RESYNC_PROBE || out.outcome == Feed::Done;
        Ok(enough && !out.recs.is_empty() && out.recs.iter().all(|r| r.fields == expect))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::index::build;
    use crate::job::{Job, JobState, no_notify};
    use crate::scan::scan_all;
    use crate::source::AccessMode;

    fn file_with_multiline(rows: usize) -> Vec<u8> {
        let mut out = b"id,text,n\n".to_vec();
        for i in 0..rows {
            if i % 5 == 0 {
                out.extend_from_slice(
                    format!("{i},\"line a\nline b, still {i}\nline c\",{i}\n").as_bytes(),
                );
            } else {
                out.extend_from_slice(format!("{i},plain text {i},{}\n", i * 3).as_bytes());
            }
        }
        out
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        data: Vec<u8>,
        all: Vec<RecSpan>,
        src: Arc<Source>,
        d: Arc<Compiled>,
    }

    fn fixture(rows: usize) -> Fixture {
        let data = file_with_multiline(rows);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, &data).unwrap();
        let d = Arc::new(Dialect::default().compile().unwrap());
        let all = scan_all(&d, &data, 0, usize::MAX);
        let src = Arc::new(Source::open(&p, AccessMode::Mapped).unwrap());
        Fixture {
            _dir: dir,
            data,
            all,
            src,
            d,
        }
    }

    fn full_index(f: &Fixture) -> Arc<Index> {
        let index = Index::new(0, f.data.len() as u64);
        let (s, d, i) = (f.src.clone(), f.d.clone(), index.clone());
        assert_eq!(
            Job::spawn("i", no_notify(), move |c| build(&s, &d, &i, c)).join(),
            JobState::Done
        );
        index
    }

    #[test]
    fn exact_navigation_in_indexed_region() {
        let f = fixture(20_000);
        let index = full_index(&f);
        let mut loc = Locator::new(f.src.clone(), f.d.clone(), index, Some(3), None);
        for rec in [0u64, 1, 2, 777, 5_000, 19_999, 20_000] {
            let p = loc.locate_record(rec).unwrap();
            assert!(p.exact);
            assert_eq!(p.offset, f.all[rec as usize].start, "record {rec}");
            let got = loc.records_from(p, 30).unwrap();
            for (k, (rp, r)) in got.rows.iter().enumerate() {
                assert_eq!(*r, f.all[rec as usize + k]);
                assert_eq!(rp.record, rec + k as u64);
            }
            let back = loc.records_before(p, 25).unwrap();
            let lo = (rec as usize).saturating_sub(25);
            assert_eq!(
                back.iter().map(|x| x.1).collect::<Vec<_>>(),
                f.all[lo..rec as usize].to_vec()
            );
        }
        // Offsets inside a multi-line record resolve to its start.
        let r = f.all[501];
        assert_eq!(loc.locate_offset(r.start + 3).unwrap().offset, r.start);
        let last = loc.last().unwrap().unwrap();
        assert_eq!(last.record, f.all.len() as u64 - 1);
    }

    #[test]
    fn approximate_positions_resync_to_real_records_and_refine() {
        let f = fixture(400_000);
        // An index that has not started yet.
        let index = Index::new(0, f.data.len() as u64);
        let mut loc = Locator::new(
            f.src.clone(),
            f.d.clone(),
            index.clone(),
            Some(3),
            Some(25.0),
        );
        let starts: std::collections::HashSet<u64> = f.all.iter().map(|r| r.start).collect();
        for frac in [0.5, 0.77, 0.99] {
            let off = (f.data.len() as f64 * frac) as u64;
            let p = loc.locate_offset(off).unwrap();
            assert!(!p.exact);
            assert!(
                starts.contains(&p.offset),
                "resync at {off} landed inside a record"
            );
            let got = loc.records_from(p, 20).unwrap();
            assert!(got.rows.iter().all(|(_, r)| starts.contains(&r.start)));
            let back = loc.records_before(p, 10).unwrap();
            assert!(!back.is_empty());
            assert!(back.iter().all(|(_, r)| starts.contains(&r.start)));
        }
        // Once indexed, the same position becomes exact with the right number.
        let (s, d, i) = (f.src.clone(), f.d.clone(), index.clone());
        Job::spawn("i", no_notify(), move |c| build(&s, &d, &i, c)).join();
        let approx = RowPos {
            offset: f.all[300_001].start,
            record: 290_000,
            exact: false,
        };
        let exact = loc.refine(approx).unwrap();
        assert!(exact.exact);
        assert_eq!(exact.record, 300_001);
    }

    #[test]
    fn locate_record_past_frontier_is_marked_approximate() {
        let f = fixture(400_000);
        let index = Index::new(0, f.data.len() as u64);
        let mut loc = Locator::new(
            f.src.clone(),
            f.d.clone(),
            index,
            Some(3),
            Some(f.data.len() as f64 / 400_001.0),
        );
        let p = loc.locate_record(300_000).unwrap();
        assert!(!p.exact);
        assert_eq!(p.record, 300_000);
        // Resync lands on a real record start close to the estimate.
        let starts: std::collections::HashSet<u64> = f.all.iter().map(|r| r.start).collect();
        assert!(starts.contains(&p.offset));
        // Near the start, an exact scan from the frontier is used.
        let p = loc.locate_record(10).unwrap();
        assert!(p.exact);
        assert_eq!(p.offset, f.all[10].start);
    }
}
