//! Sorting by a column: a permutation of rows built in the background.
//!
//! The file is never reordered: the result is a list of `(offset, record)`
//! pairs in sorted order that the grid shows as a view, and that "Export /
//! Save as" can write out. Entries are compact ([`BYTES_PER_ROW`] per row).
//! Numbers and dates sort by an exact 8-byte key. Text sorts by its first
//! 8 bytes plus a fingerprint of the whole value: runs of equal prefixes
//! are grouped by fingerprint, one value per group is re-read, and the
//! groups are ordered by those full values. A run of a few distinct values
//! (a "city" column) costs a few reads, not one per row.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::slice::ParallelSliceMut;

use crate::chunked::ChunkedVec;
use crate::fields::split;
use crate::filter::{FilterData, Row, SharedFilter};
use crate::infer::{DateOrder, parse_date, parse_number};
use crate::job::{Job, JobCtl, Notify};
use crate::scan::Flow;
use crate::search::Target;
use crate::visit::{read_record, visit_records};

/// Default memory budget for a sort.
pub const DEFAULT_BUDGET: u64 = 512 << 20;
/// Working set per row (key, fingerprint, offset, record).
pub const BYTES_PER_ROW: u64 = 32;
/// Full values re-read for one run of equal prefixes; beyond this the
/// remaining groups of that run keep file order.
const RUN_VALUE_BYTES: usize = 64 << 20;
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
    /// Text: fingerprint of the whole value (0 for other keys).
    fp: u64,
    offset: u64,
    /// Record number; bit 63 marks a missing key (sorts last).
    record: u64,
}

impl Entry {
    fn missing(&self) -> bool {
        self.record & MISSING != 0
    }
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

/// A fast 64-bit fingerprint (equal values, equal fingerprints).
fn fingerprint(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = 0xCBF2_9CE4_8422_2325 ^ (b.len() as u64).wrapping_mul(K);
    let (words, rest) = b.as_chunks::<8>();
    for &w in words {
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(K).rotate_left(29);
    }
    let mut w = [0u8; 8];
    w[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(w)).wrapping_mul(K);
    h ^ (h >> 32)
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
/// filter, in file order). `est_rows` is used for the up-front budget
/// check; the job enforces the budget exactly.
pub fn start(
    target: Target,
    spec: SortSpec,
    only: Option<SharedFilter>,
    budget: u64,
    est_rows: u64,
    notify: Notify,
) -> Result<SortRun, String> {
    let members: Option<Vec<u64>> = only.map(|f| f.lock().rows.iter().map(|r| r.offset).collect());
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
        // Sized from the row count up front: a Vec that doubles would need
        // up to three times the memory while it reallocates.
        let cap = (est_rows as usize)
            .saturating_add(est_rows as usize / 64 + 1024)
            .min(max_rows);
        let mut entries: Vec<Entry> = Vec::with_capacity(cap);
        let fields = target.fields_for(spec.col);
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
                if entries.len() == entries.capacity() {
                    let grow = (entries.len() / 4).max(1 << 16);
                    entries.reserve_exact(grow.min(max_rows - entries.len()).max(1));
                }
                spans.clear();
                split(&target.dialect, v.content, 0, fields, &mut spans);
                let value = target.value(v.span.start, v.content, &spans, spec.col);
                let value = value.as_ref();
                let (key, fp) = match spec.key {
                    SortKey::Text => (Some(text_key(value)), fingerprint(value.as_bytes())),
                    SortKey::Number => {
                        (parse_number(value).filter(|x| !x.is_nan()).map(f64_key), 0)
                    }
                    SortKey::Date => (
                        parse_date(value, DateOrder::Auto).map(|d| (d as u64) ^ (1 << 63)),
                        0,
                    ),
                };
                entries.push(Entry {
                    key: key.unwrap_or(0),
                    fp,
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
            a.missing()
                .cmp(&b.missing())
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
            refine_text_ties(&target, &spec, &mut entries, ctl);
        }
        // Collected in place (the rows reuse the entries' allocation).
        let mut rows: Vec<Row> = entries
            .into_iter()
            .map(|e| Row {
                offset: e.offset,
                record: e.record & !MISSING,
            })
            .collect();
        rows.shrink_to_fit();
        let rows = ChunkedVec::from_vec(rows);
        let mut d = out.lock();
        d.matched = rows.len() as u64;
        d.records_scanned = rows.len() as u64;
        d.rows = rows;
        d.done = !ctl.is_cancelled();
        Ok(())
    });
    Ok(SortRun { job, data, spec })
}

/// Orders runs of equal 8-byte text prefixes by the full values.
fn refine_text_ties(target: &Target, spec: &SortSpec, entries: &mut [Entry], ctl: &JobCtl) {
    let mut i = 0;
    while i < entries.len() {
        if ctl.is_cancelled() {
            return;
        }
        let mut j = i + 1;
        while j < entries.len()
            && entries[j].key == entries[i].key
            && entries[j].missing() == entries[i].missing()
        {
            j += 1;
        }
        // Only prefixes that fill all 8 bytes can hide a difference.
        if j - i > 1 && entries[i].key & 0xFF != 0 {
            refine_run(target, spec, &mut entries[i..j]);
        }
        i = j;
    }
}

/// One run of equal prefixes: group by fingerprint, read one value per
/// group, order the groups by value; rows of a group keep file order.
fn refine_run(target: &Target, spec: &SortSpec, run: &mut [Entry]) {
    run.sort_unstable_by_key(|e| (e.fp, e.offset));
    // (fingerprint, offset of its first row) per group.
    let mut groups: Vec<(u64, u64)> = Vec::new();
    for e in run.iter() {
        if groups.last().is_none_or(|g| g.0 != e.fp) {
            groups.push((e.fp, e.offset));
        }
    }
    if groups.len() == 1 {
        return;
    }
    // Read the representatives in file order (sequential access).
    groups.sort_unstable_by_key(|g| g.1);
    let mut spans = Vec::new();
    let mut held = 0usize;
    let mut valued: Vec<(Option<String>, u64, u64)> = Vec::with_capacity(groups.len());
    for &(fp, offset) in &groups {
        let value = (held < RUN_VALUE_BYTES)
            .then(|| read_record(&target.src, &target.dialect, offset).ok())
            .flatten()
            .map(|(_, content)| {
                spans.clear();
                split(&target.dialect, &content, 0, usize::MAX, &mut spans);
                target
                    .values(offset, &content, &spans)
                    .get(spec.col)
                    .map(|(_, s)| s.to_string())
                    .unwrap_or_default()
            });
        held += value.as_ref().map_or(0, String::len);
        valued.push((value, fp, offset));
    }
    // Unread groups (over the byte limit) go last, in file order.
    valued.sort_by(|a, b| {
        let by_value = match (&a.0, &b.0) {
            (Some(x), Some(y)) if spec.descending => y.cmp(x),
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_value.then_with(|| a.2.cmp(&b.2))
    });
    let rank: HashMap<u64, usize> = valued
        .iter()
        .enumerate()
        .map(|(r, &(_, fp, _))| (fp, r))
        .collect();
    run.sort_unstable_by_key(|e| (rank.get(&e.fp).copied().unwrap_or(usize::MAX), e.offset));
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
        d.rows.iter().map(|r| r.record).collect()
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
    fn long_shared_prefixes_order_by_full_value() {
        // Many rows, few distinct values, all sharing a 12-byte prefix.
        let mut data = b"v\n".to_vec();
        let names = ["Samarkand-c", "Samarkand-a", "Samarkand-b", "Samarkand"];
        for i in 0..400 {
            data.extend_from_slice(names[i % 4].as_bytes());
            data.push(b'\n');
        }
        let rows = sorted(
            &data,
            SortSpec {
                col: 0,
                key: SortKey::Text,
                descending: false,
            },
        );
        let order: Vec<&str> = rows.iter().map(|&r| names[(r as usize - 1) % 4]).collect();
        let mut want = order.clone();
        want.sort();
        assert_eq!(order, want);
        // Within one value, file order.
        let first: Vec<u64> = rows.iter().copied().take(100).collect();
        assert!(first.windows(2).all(|w| w[0] < w[1]));
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

    #[test]
    fn fingerprints_differ_for_close_values() {
        let a = fingerprint(b"Samarkand-a");
        assert_eq!(a, fingerprint(b"Samarkand-a"));
        assert_ne!(a, fingerprint(b"Samarkand-b"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abc\0"));
    }
}
