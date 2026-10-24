//! Logical rows: source records with the overlay applied (deleted ranges
//! skipped, inserted rows spliced in).
//!
//! A position is a source record (or the end of the data) plus an index
//! into the rows inserted before it: `sub < inserted.len()` is an inserted
//! row, `sub == inserted.len()` is the record itself. Inserted rows are
//! always anchored at a record offset or at [`END`].

use std::io;

use crate::locate::{FetchEnd, Locator, RowPos};
use crate::overlay::{END, Overlay, RowRef};
use crate::scan::RecSpan;

/// A logical position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LPos {
    /// The source record (or the end-of-data position).
    pub rec: RowPos,
    pub sub: u32,
}

/// What a logical row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LKind {
    Src(RecSpan),
    New(u32),
}

/// A logical row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LRow {
    pub pos: LPos,
    pub kind: LKind,
}

impl LRow {
    pub fn row_ref(&self) -> RowRef {
        match self.kind {
            LKind::Src(r) => RowRef::Src(r.start),
            LKind::New(id) => RowRef::New(id),
        }
    }

    pub fn span(&self) -> Option<RecSpan> {
        match self.kind {
            LKind::Src(r) => Some(r),
            LKind::New(_) => None,
        }
    }
}

/// Overlay key of a record position (the end position anchors at END).
pub fn anchor_key(rec: &RowPos, data_end: u64) -> u64 {
    if rec.offset >= data_end {
        END
    } else {
        rec.offset
    }
}

/// Position of the source record `rec` itself (after its inserted rows).
pub fn at_record(ov: &Overlay, rec: RowPos, data_end: u64) -> LPos {
    LPos {
        rec,
        sub: ov.inserted_before(anchor_key(&rec, data_end)).len() as u32,
    }
}

/// Logical data-row index (0-based) of a position.
pub fn number(ov: &Overlay, pos: &LPos, header_records: u64, data_end: u64) -> u64 {
    let key = anchor_key(&pos.rec, data_end);
    let base = pos.rec.record.saturating_sub(header_records) + ov.inserted_count_before(key)
        - ov.deleted_count_before(key);
    base + pos.sub as u64
}

/// Up to `n` logical rows starting at `from`.
pub fn rows_from(
    loc: &mut Locator,
    ov: &Overlay,
    from: LPos,
    n: usize,
) -> io::Result<(Vec<LRow>, FetchEnd)> {
    let data_end = loc.data_end();
    let mut out = Vec::with_capacity(n.min(4096));
    let mut cur = from;
    while out.len() < n {
        let key = anchor_key(&cur.rec, data_end);
        let ins = ov.inserted_before(key);
        while (cur.sub as usize) < ins.len() && out.len() < n {
            out.push(LRow {
                pos: cur,
                kind: LKind::New(ins[cur.sub as usize]),
            });
            cur.sub += 1;
        }
        if out.len() >= n {
            break;
        }
        if key == END {
            return Ok((out, FetchEnd::Eof));
        }
        if let Some(d) = ov.deleted_range(cur.rec.offset) {
            // Skip the deleted records in one step.
            let skipped = if cur.rec.offset == d.start {
                d.records
            } else {
                1
            };
            cur = LPos {
                rec: RowPos {
                    offset: d.end,
                    record: cur.rec.record + skipped,
                    exact: cur.rec.exact,
                },
                sub: 0,
            };
            continue;
        }
        let fetched = loc.records_from(cur.rec, 2)?;
        let Some((p, span)) = fetched.rows.first().copied() else {
            return Ok((
                out,
                if fetched.end == FetchEnd::Budget {
                    FetchEnd::Budget
                } else {
                    FetchEnd::Eof
                },
            ));
        };
        out.push(LRow {
            pos: LPos {
                rec: p,
                sub: cur.sub,
            },
            kind: LKind::Src(span),
        });
        cur = match fetched.rows.get(1) {
            Some((next, _)) => LPos { rec: *next, sub: 0 },
            None if fetched.end == FetchEnd::Budget => return Ok((out, FetchEnd::Budget)),
            None => LPos {
                rec: RowPos {
                    offset: data_end,
                    record: p.record + 1,
                    exact: p.exact,
                },
                sub: 0,
            },
        };
    }
    Ok((out, FetchEnd::Filled))
}

/// Up to `n` logical rows ending just before `before`, in order. Stops at
/// the first data record (`header_records`).
pub fn rows_before(
    loc: &mut Locator,
    ov: &Overlay,
    before: LPos,
    n: usize,
    header_records: u64,
) -> io::Result<Vec<LRow>> {
    let data_end = loc.data_end();
    let mut rev = Vec::with_capacity(n.min(4096));
    let mut cur = before;
    while rev.len() < n {
        if cur.sub > 0 {
            cur.sub -= 1;
            let ins = ov.inserted_before(anchor_key(&cur.rec, data_end));
            if let Some(&id) = ins.get(cur.sub as usize) {
                rev.push(LRow {
                    pos: cur,
                    kind: LKind::New(id),
                });
            }
            continue;
        }
        let prev = loc.records_before(cur.rec, 1)?;
        let Some((p, span)) = prev.first().copied() else {
            break;
        };
        if p.record < header_records {
            break;
        }
        if let Some(d) = ov.deleted_range(p.offset) {
            // Jump to the start of the deleted range; only its inserted rows show.
            let first = RowPos {
                offset: d.start,
                record: (p.record + 1).saturating_sub(d.records),
                exact: p.exact,
            };
            cur = LPos {
                rec: first,
                sub: ov.inserted_before(d.start).len() as u32,
            };
            if first.record < header_records {
                break;
            }
            continue;
        }
        let sub = ov.inserted_before(p.offset).len() as u32;
        rev.push(LRow {
            pos: LPos { rec: p, sub },
            kind: LKind::Src(span),
        });
        cur = LPos { rec: p, sub };
    }
    rev.reverse();
    Ok(rev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::index::{Index, build};
    use crate::job::{Job, no_notify};
    use crate::overlay::{ColRef, Deleted};
    use crate::scan::scan_all;
    use crate::source::{AccessMode, Source};
    use std::sync::Arc;

    fn setup(rows: usize) -> (tempfile::TempDir, Locator, Vec<RecSpan>) {
        let mut data = b"h\n".to_vec();
        for i in 0..rows {
            data.extend_from_slice(format!("{i}\n").as_bytes());
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, &data).unwrap();
        let d = Arc::new(Dialect::default().compile().unwrap());
        let src = Arc::new(Source::open(&p, AccessMode::Auto).unwrap());
        let index = Index::new(0, data.len() as u64);
        let (s, dd, i) = (src.clone(), d.clone(), index.clone());
        Job::spawn("i", no_notify(), move |c| build(&s, &dd, &i, c)).join();
        let all = scan_all(&d, &data, 0, usize::MAX);
        (dir, Locator::new(src, d, index, Some(1), None), all)
    }

    fn labels(rows: &[LRow], ov: &Overlay, all: &[RecSpan]) -> Vec<String> {
        rows.iter()
            .map(|r| match r.kind {
                LKind::Src(s) => all
                    .iter()
                    .position(|a| a.start == s.start)
                    .map(|i| (i - 1).to_string())
                    .unwrap_or_default(),
                LKind::New(id) => format!(
                    "+{}",
                    ov.cell(RowRef::New(id), ColRef::Src(0)).unwrap_or("")
                ),
            })
            .collect()
    }

    #[test]
    fn splices_inserts_and_skips_deletes_both_ways() {
        let (_d, mut loc, all) = setup(10);
        let mut ov = Overlay::new();
        let a = ov.insert_rows(all[3].start, 0, 2); // before data row 2
        ov.set_cells(
            vec![
                (RowRef::New(a[0]), ColRef::Src(0), "a".into()),
                (RowRef::New(a[1]), ColRef::Src(0), "b".into()),
            ],
            |_, _| None,
        );
        let e = ov.insert_rows(END, 0, 1);
        ov.set_cells(
            vec![(RowRef::New(e[0]), ColRef::Src(0), "z".into())],
            |_, _| None,
        );
        ov.delete_rows(
            &[],
            &[Deleted {
                start: all[5].start,
                end: all[8].start,
                records: 3,
            }],
        ); // rows 4,5,6
        let first = at_record(
            &ov,
            RowPos {
                offset: all[1].start,
                record: 1,
                exact: true,
            },
            loc.data_end(),
        );
        let (rows, end) = rows_from(&mut loc, &ov, first, 100).unwrap();
        assert_eq!(end, FetchEnd::Eof);
        let got = labels(&rows, &ov, &all);
        assert_eq!(got, ["0", "1", "+a", "+b", "2", "3", "7", "8", "9", "+z"]);
        // Logical numbers are consecutive.
        let nums: Vec<u64> = rows
            .iter()
            .map(|r| number(&ov, &r.pos, 1, loc.data_end()))
            .collect();
        assert_eq!(nums, (0..10).collect::<Vec<u64>>());
        // Walking back from the end gives the same rows.
        let end_pos = LPos {
            rec: RowPos {
                offset: loc.data_end(),
                record: 11,
                exact: true,
            },
            sub: 1,
        };
        let back = rows_before(&mut loc, &ov, end_pos, 100, 1).unwrap();
        assert_eq!(labels(&back, &ov, &all), got);
        // And from the middle.
        let mid = rows[6].pos; // row "7"
        let back = rows_before(&mut loc, &ov, mid, 3, 1).unwrap();
        assert_eq!(labels(&back, &ov, &all), ["+b", "2", "3"]);
        let (fwd, _) = rows_from(&mut loc, &ov, rows[3].pos, 3).unwrap();
        assert_eq!(labels(&fwd, &ov, &all), ["+b", "2", "3"]);
    }
}
