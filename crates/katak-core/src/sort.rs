//! Sorting by a column: a permutation of rows built in the background.
//!
//! The file is never reordered: the result is a list of `(offset, record)`
//! pairs in sorted order that the grid shows as a view, and that "Export /
//! Save as" can write out. Keys are compact (8 bytes) so the working set is
//! [`BYTES_PER_ROW`] per row; text keys are the first 8 bytes of the value,
//! and runs of equal prefixes are re-read and ordered by the full value.

use std::cmp::Ordering;
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::slice::ParallelSliceMut;

use crate::chunked::ChunkedVec;
use crate::fields::split;
use crate::filter::{FilterData, Row, SharedFilter};
use crate::infer::{DateOrder, parse_date, parse_number};
use crate::job::{Job, Notify};
use crate::scan::Flow;
use crate::search::Target;
use crate::visit::{read_record, visit_records};

/// Default memory budget for a sort.
pub const DEFAULT_BUDGET: u64 = 512 << 20;
/// Working set per row (key, offset, record).
pub const BYTES_PER_ROW: u64 = 24;
/// Longer runs of equal text prefixes keep file order (bounded re-reads).
const TIE_RUN_LIMIT: usize = 1 << 20;
const MISSING: u64 = 1 << 63;

/// How values compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// Code-point (UTF-8 byte) order.
    Text,
    /// Numbers (`.` or `,` decimals); non-numbers sort last.
    Number,
    /// Dates; non-dates sort last.
    Date,
}

/// What to sort by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortSpec {
    /// Logical column.
    pub col: usize,
    pub key: SortKey,
    pub descending: bool,
}

/// A running (or finished) sort. `data.rows` is filled when done.
pub struct SortRun {
    pub job: Job,
    pub data: SharedFilter,
    pub spec: SortSpec,
}

#[derive(Clone, Copy)]
struct Entry {
    key: u64,
    offset: u64,
    /// Record number; bit 63 marks a missing key (sorts last).
    record: u64,
}

fn f64_key(x: f64) -> u64 {
    let b = x.to_bits();
    if b >> 63 == 1 { !b } else { b | (1 << 63) }
}

fn text_key(s: &str) -> u64 {
    let mut k = [0u8; 8];
    let b = s.as_bytes();
    k[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
    u64::from_be_bytes(k)
}

/// Checks the memory budget for `rows` rows.
pub fn check_budget(rows: u64, budget: u64) -> Result<(), String> {
    let need = rows.saturating_mul(BYTES_PER_ROW);
    if need > budget {
        return Err(format!(
            "Sorting {rows} rows needs about {} of memory, more than the {} budget. Filter the rows first, or raise the budget in View → Sort memory budget.",
            crate::sys::human_bytes(need),
            crate::sys::human_bytes(budget)
        ));
    }
    Ok(())
}

/// Starts sorting all data rows, or only the rows of `only` (a finished
/// filter). `est_rows` is used for the up-front budget check.
pub fn start(
    target: Target,
    spec: SortSpec,
    only: Option<SharedFilter>,
    budget: u64,
    est_rows: u64,
    notify: Notify,
) -> Result<SortRun, String> {
    let members: Option<Vec<u64>> = only.map(|f| {
        let d = f.lock();
        d.rows.iter().map(|r| r.offset).collect()
    });
    check_budget(
        members.as_ref().map_or(est_rows, |m| m.len() as u64),
        budget,
    )?;
    let data: SharedFilter = Arc::new(Mutex::new(FilterData {
        sorted: true,
        ..FilterData::default()
    }));
    let out = data.clone();
    let job = Job::spawn("sort", notify, move |ctl| {
        let max_rows = (budget / BYTES_PER_ROW) as usize;
        let mut entries: Vec<Entry> = Vec::new();
        let mut spans = Vec::new();
        let mut member = 0usize;
        let mut over = false;
        let res = visit_records(
            &target.src,
            &target.dialect,
            target.data_start,
            0,
            ctl,
            None,
            &[],
            |v| {
                if v.record < target.header_records || target.deleted(v.span.start) {
                    return Flow::Continue;
                }
                if let Some(m) = &members {
                    while member < m.len() && m[member] < v.span.start {
                        member += 1;
                    }
                    if m.get(member) != Some(&v.span.start) {
                        return Flow::Continue;
                    }
                }
                if entries.len() >= max_rows {
                    over = true;
                    return Flow::Stop;
                }
                spans.clear();
                split(&target.dialect, v.content, 0, usize::MAX, &mut spans);
                let values = target.values(v.span.start, v.content, &spans);
                let value = values.get(spec.col).map_or("", |(_, s)| s.as_ref());
                let key = match spec.key {
                    SortKey::Text => Some(text_key(value)),
                    SortKey::Number => parse_number(value).filter(|x| !x.is_nan()).map(f64_key),
                    SortKey::Date => {
                        parse_date(value, DateOrder::Auto).map(|d| (d as u64) ^ (1 << 63))
                    }
                };
                entries.push(Entry {
                    key: key.unwrap_or(0),
                    offset: v.span.start,
                    record: v.record | if key.is_none() { MISSING } else { 0 },
                });
                Flow::Continue
            },
        );
        if let Err(e) = res {
            out.lock().error = Some(format!("reading the file failed: {e}"));
            return Err(e.to_string());
        }
        if ctl.is_cancelled() {
            return Ok(());
        }
        if over {
            let msg = check_budget(entries.len() as u64 + 1, budget)
                .err()
                .unwrap_or_else(|| "too many rows to sort".into());
            out.lock().error = Some(msg.clone());
            return Err(msg);
        }
        let cmp = |a: &Entry, b: &Entry| -> Ordering {
            let (ma, mb) = (a.record & MISSING != 0, b.record & MISSING != 0);
            ma.cmp(&mb)
                .then_with(|| {
                    if spec.descending {
                        b.key.cmp(&a.key)
                    } else {
                        a.key.cmp(&b.key)
                    }
                })
                .then_with(|| a.offset.cmp(&b.offset))
        };
        entries.par_sort_unstable_by(cmp);
        if spec.key == SortKey::Text {
            refine_text_ties(&target, &spec, &mut entries);
        }
        let mut rows = ChunkedVec::new();
        for e in &entries {
            rows.push(Row {
                offset: e.offset,
                record: e.record & !MISSING,
            });
        }
        let mut d = out.lock();
        d.matched = rows.len() as u64;
        d.records_scanned = rows.len() as u64;
        d.rows = rows;
        d.done = true;
        Ok(())
    });
    Ok(SortRun { job, data, spec })
}

/// Orders runs of equal 8-byte text prefixes by the full values.
fn refine_text_ties(target: &Target, spec: &SortSpec, entries: &mut [Entry]) {
    let mut i = 0;
    let mut spans = Vec::new();
    while i < entries.len() {
        let mut j = i + 1;
        while j < entries.len()
            && entries[j].key == entries[i].key
            && (entries[j].record & MISSING) == (entries[i].record & MISSING)
        {
            j += 1;
        }
        // Only prefixes that fill all 8 bytes can hide a difference.
        if j - i > 1 && j - i <= TIE_RUN_LIMIT && entries[i].key & 0xFF != 0 {
            let mut keyed: Vec<(String, Entry)> = entries[i..j]
                .iter()
                .map(|e| {
                    let v = read_record(&target.src, &target.dialect, e.offset)
                        .ok()
                        .map(|(_, content)| {
                            spans.clear();
                            split(&target.dialect, &content, 0, usize::MAX, &mut spans);
                            target
                                .values(e.offset, &content, &spans)
                                .get(spec.col)
                                .map(|(_, s)| s.to_string())
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    (v, *e)
                })
                .collect();
            keyed.sort_by(|a, b| {
                let o = if spec.descending {
                    b.0.cmp(&a.0)
                } else {
                    a.0.cmp(&b.0)
                };
                o.then_with(|| a.1.offset.cmp(&b.1.offset))
            });
            for (k, (_, e)) in keyed.into_iter().enumerate() {
                entries[i + k] = e;
            }
        }
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::no_notify;
    use crate::source::{AccessMode, Source};

    fn sorted(data: &[u8], spec: SortSpec) -> Vec<u64> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, data).unwrap();
        let d = Dialect::default();
        let target = Target {
            src: Arc::new(Source::open(&p, AccessMode::Auto).unwrap()),
            dialect: Arc::new(d.compile().unwrap()),
            encoding: d.encoding,
            data_start: 0,
            header_records: 1,
            overlay: None,
        };
        let run = start(target, spec, None, DEFAULT_BUDGET, 10, no_notify()).unwrap();
        run.job.join();
        let d = run.data.lock();
        assert!(d.done && d.sorted);
        d.rows
            .slice(0, d.rows.len())
            .iter()
            .map(|r| r.record)
            .collect()
    }

    const DATA: &[u8] = b"name,n,when\nprefix-long-b,10,2024-01-02\nprefix-long-a,-2.5,1999-12-31\nzed,x,not a date\nalpha,3,2024-01-01\nprefix-long-a,7,2001-05-05\n";

    #[test]
    fn text_numbers_dates_and_missing_last() {
        let text = |descending| SortSpec {
            col: 0,
            key: SortKey::Text,
            descending,
        };
        // Equal 8-byte prefixes are ordered by the full value; equal values keep file order.
        assert_eq!(sorted(DATA, text(false)), vec![4, 2, 5, 1, 3]);
        assert_eq!(sorted(DATA, text(true)), vec![3, 1, 2, 5, 4]);
        let num = |descending| SortSpec {
            col: 1,
            key: SortKey::Number,
            descending,
        };
        assert_eq!(sorted(DATA, num(false)), vec![2, 4, 5, 1, 3]);
        assert_eq!(sorted(DATA, num(true)), vec![1, 5, 4, 2, 3]);
        let date = SortSpec {
            col: 2,
            key: SortKey::Date,
            descending: false,
        };
        assert_eq!(sorted(DATA, date), vec![2, 5, 4, 1, 3]);
    }

    #[test]
    fn budget_is_enforced() {
        assert!(check_budget(1_000, DEFAULT_BUDGET).is_ok());
        assert!(
            check_budget(100_000_000, DEFAULT_BUDGET)
                .unwrap_err()
                .contains("budget")
        );
    }

    #[test]
    fn float_keys_preserve_order() {
        let xs = [-1e9, -2.5, -0.0, 0.0, 1e-9, 3.0, 1e300];
        let keys: Vec<u64> = xs.iter().map(|&x| f64_key(x)).collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]));
    }
}
