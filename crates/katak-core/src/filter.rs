//! Per-column filters and the background job that applies them.
//!
//! Filters on several columns combine with AND. The job streams over the
//! file once and collects the matching rows (offset + exact record number)
//! into a capped list that the grid shows while it grows.

use std::sync::Arc;

use parking_lot::Mutex;
use regex::Regex;

use crate::chunked::ChunkedVec;
use crate::encoding::TextEncoding;
use crate::fields::{split, unquote};
use crate::infer::parse_number;
use crate::job::{Job, Notify};
use crate::scan::Flow;
use crate::search::Target;
use crate::visit::{Prefilter, visit_records};

/// Rows kept by default (16 bytes each, 64 MB).
pub const DEFAULT_CAP: usize = 4_000_000;

/// A test applied to one column's value.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    Contains(String),
    Equals(String),
    StartsWith(String),
    Regex(String),
    Empty,
    NotEmpty,
    /// Numeric range (inclusive); non-numeric values never match.
    Range(Option<f64>, Option<f64>),
}

impl FilterOp {
    pub fn label(&self) -> &'static str {
        match self {
            FilterOp::Contains(_) => "contains",
            FilterOp::Equals(_) => "equals",
            FilterOp::StartsWith(_) => "starts with",
            FilterOp::Regex(_) => "matches regex",
            FilterOp::Empty => "is empty",
            FilterOp::NotEmpty => "is not empty",
            FilterOp::Range(..) => "number between",
        }
    }
}

/// A filter on one column.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnFilter {
    pub col: usize,
    pub op: FilterOp,
    pub case_sensitive: bool,
}

enum Test {
    Contains(String),
    Equals(String),
    Starts(String),
    Re(Regex),
    Empty,
    NotEmpty,
    Range(f64, f64),
}

struct Compiled {
    col: usize,
    test: Test,
}

impl Compiled {
    fn matches(&self, v: &str) -> bool {
        match &self.test {
            Test::Contains(s) => v.contains(s.as_str()),
            Test::Equals(s) => v == s,
            Test::Starts(s) => v.starts_with(s.as_str()),
            Test::Re(r) => r.is_match(v),
            Test::Empty => v.is_empty(),
            Test::NotEmpty => !v.is_empty(),
            Test::Range(lo, hi) => parse_number(v).is_some_and(|x| x >= *lo && x <= *hi),
        }
    }
}

fn compile(f: &ColumnFilter) -> Result<Compiled, String> {
    let ci = |pat: String| -> Result<Test, String> {
        Regex::new(&format!("(?i){pat}"))
            .map(Test::Re)
            .map_err(|e| e.to_string())
    };
    let test = match &f.op {
        FilterOp::Contains(s) if f.case_sensitive => Test::Contains(s.clone()),
        FilterOp::Equals(s) if f.case_sensitive => Test::Equals(s.clone()),
        FilterOp::StartsWith(s) if f.case_sensitive => Test::Starts(s.clone()),
        FilterOp::Contains(s) => ci(regex::escape(s))?,
        FilterOp::Equals(s) => ci(format!("^{}$", regex::escape(s)))?,
        FilterOp::StartsWith(s) => ci(format!("^{}", regex::escape(s)))?,
        FilterOp::Regex(p) if f.case_sensitive => {
            Test::Re(Regex::new(p).map_err(|e| e.to_string())?)
        }
        FilterOp::Regex(p) => ci(format!("(?:{p})"))?,
        FilterOp::Empty => Test::Empty,
        FilterOp::NotEmpty => Test::NotEmpty,
        FilterOp::Range(lo, hi) => {
            Test::Range(lo.unwrap_or(f64::NEG_INFINITY), hi.unwrap_or(f64::INFINITY))
        }
    };
    Ok(Compiled { col: f.col, test })
}

/// A prefilter from the first literal filter (a necessary condition for the
/// whole AND).
fn prefilter(filters: &[ColumnFilter], enc: TextEncoding) -> Option<Prefilter> {
    filters.iter().find_map(|f| {
        let s = match &f.op {
            FilterOp::Contains(s) | FilterOp::Equals(s) | FilterOp::StartsWith(s)
                if !s.is_empty() =>
            {
                s
            }
            _ => return None,
        };
        match enc {
            TextEncoding::Utf8 if f.case_sensitive => Some(Prefilter::literal(s.as_bytes())),
            TextEncoding::Utf8 => regex::bytes::Regex::new(&format!("(?i){}", regex::escape(s)))
                .ok()
                .map(Prefilter::Regex),
            _ if f.case_sensitive => Prefilter::transcoded(s, enc),
            _ => None,
        }
    })
}

/// A matching row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row {
    pub offset: u64,
    pub record: u64,
}

/// Shared, growing filter results.
#[derive(Debug, Default)]
pub struct FilterData {
    /// Matching rows in file order (at most the cap).
    pub rows: ChunkedVec<Row>,
    /// Matching rows found, including those beyond the cap.
    pub matched: u64,
    pub capped: bool,
    /// Data rows examined so far.
    pub records_scanned: u64,
    pub done: bool,
    pub error: Option<String>,
}

/// Filter results shared between the job and the UI.
pub type SharedFilter = Arc<Mutex<FilterData>>;

/// A running (or finished) filter.
pub struct FilterRun {
    pub job: Job,
    pub data: SharedFilter,
    pub filters: Vec<ColumnFilter>,
}

/// Starts filtering. Fails only for an invalid regex.
pub fn start(
    target: Target,
    filters: Vec<ColumnFilter>,
    cap: usize,
    notify: Notify,
) -> Result<FilterRun, String> {
    let compiled: Vec<Compiled> = filters.iter().map(compile).collect::<Result<_, _>>()?;
    let data = Arc::new(Mutex::new(FilterData::default()));
    let out = data.clone();
    let pre = prefilter(&filters, target.encoding);
    let need = compiled.iter().map(|c| c.col + 1).max().unwrap_or(0);
    let job = Job::spawn("filter", notify, move |ctl| {
        let mut spans = Vec::new();
        let mut batch: Vec<Row> = Vec::new();
        let (mut matched, mut kept) = (0u64, 0usize);
        let mut last_record = 0u64;
        let flush = |batch: &mut Vec<Row>, matched: u64, records: u64, capped: bool| {
            let mut d = out.lock();
            d.rows.extend_from_slice(batch);
            batch.clear();
            d.matched = matched;
            d.records_scanned = records;
            d.capped |= capped;
        };
        let res = visit_records(
            &target.src,
            &target.dialect,
            target.data_start,
            0,
            ctl,
            pre.as_ref(),
            |v| {
                if v.record < target.header_records {
                    return Flow::Continue;
                }
                last_record = v.record;
                spans.clear();
                split(&target.dialect, v.content, 0, need, &mut spans);
                let pass = compiled.iter().all(|c| match spans.get(c.col) {
                    Some(f) => {
                        let raw = unquote(&target.dialect, &v.content[f.start..f.end], f.quoted);
                        c.matches(&target.encoding.decode(&raw))
                    }
                    None => c.matches(""),
                });
                if pass {
                    matched += 1;
                    if kept < cap {
                        kept += 1;
                        batch.push(Row {
                            offset: v.span.start,
                            record: v.record,
                        });
                    }
                }
                if batch.len() >= 8192 {
                    flush(
                        &mut batch,
                        matched,
                        v.record,
                        kept >= cap && matched > kept as u64,
                    );
                    ctl.notify();
                }
                Flow::Continue
            },
        );
        let records = match &res {
            Ok(o) => o.records.saturating_sub(target.header_records),
            Err(_) => last_record,
        };
        flush(&mut batch, matched, records, matched > kept as u64);
        let mut d = out.lock();
        match res {
            Ok(o) if o.completed => d.done = true,
            Ok(_) => {}
            Err(e) => {
                d.error = Some(format!("reading the file failed: {e}"));
                return Err(e.to_string());
            }
        }
        Ok(())
    });
    Ok(FilterRun { job, data, filters })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::no_notify;
    use crate::source::{AccessMode, Source};

    fn run(data: &[u8], filters: Vec<ColumnFilter>) -> Vec<u64> {
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
        };
        let f = start(target, filters, 100, no_notify()).unwrap();
        f.job.join();
        let d = f.data.lock();
        assert!(d.done);
        d.rows
            .slice(0, d.rows.len())
            .iter()
            .map(|r| r.record)
            .collect()
    }

    fn cf(col: usize, op: FilterOp) -> ColumnFilter {
        ColumnFilter {
            col,
            op,
            case_sensitive: true,
        }
    }

    const DATA: &[u8] =
        b"name,city,amount\nAnn,Tashkent,10\nbob,tashkent,2.5\nCid,,abc\nDan,\"Tash, kent\",100\n";

    #[test]
    fn single_filters() {
        assert_eq!(
            run(DATA, vec![cf(1, FilterOp::Contains("ash".into()))]),
            vec![1, 2, 4]
        );
        assert_eq!(
            run(DATA, vec![cf(1, FilterOp::Equals("Tashkent".into()))]),
            vec![1]
        );
        let ci = ColumnFilter {
            case_sensitive: false,
            ..cf(1, FilterOp::Equals("TASHKENT".into()))
        };
        assert_eq!(run(DATA, vec![ci]), vec![1, 2]);
        assert_eq!(
            run(DATA, vec![cf(0, FilterOp::StartsWith("D".into()))]),
            vec![4]
        );
        assert_eq!(run(DATA, vec![cf(1, FilterOp::Empty)]), vec![3]);
        assert_eq!(run(DATA, vec![cf(1, FilterOp::NotEmpty)]), vec![1, 2, 4]);
        assert_eq!(
            run(DATA, vec![cf(2, FilterOp::Range(Some(2.0), Some(50.0)))]),
            vec![1, 2]
        );
        assert_eq!(
            run(DATA, vec![cf(0, FilterOp::Regex("^[a-c]".into()))]),
            vec![2]
        );
        // A column missing from a row compares as empty.
        assert_eq!(run(DATA, vec![cf(7, FilterOp::Empty)]), vec![1, 2, 3, 4]);
    }

    #[test]
    fn filters_combine_with_and() {
        let both = vec![
            cf(1, FilterOp::Contains("ash".into())),
            cf(2, FilterOp::Range(Some(5.0), None)),
        ];
        assert_eq!(run(DATA, both), vec![1, 4]);
    }

    #[test]
    fn bad_regex_is_an_error() {
        let d = Dialect::default();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, DATA).unwrap();
        let target = Target {
            src: Arc::new(Source::open(&p, AccessMode::Auto).unwrap()),
            dialect: Arc::new(d.compile().unwrap()),
            encoding: d.encoding,
            data_start: 0,
            header_records: 1,
        };
        assert!(
            start(
                target,
                vec![cf(0, FilterOp::Regex("(".into()))],
                10,
                no_notify()
            )
            .is_err()
        );
    }
}
