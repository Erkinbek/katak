//! Search: a cell matcher and a background job that streams over the file.
//!
//! Matching is always done on decoded cell values with the `regex` crate
//! (linear time, Unicode case folding). Literal searches are escaped
//! regexes; "whole word" wraps the pattern in `\b…\b`, "whole cell" in
//! `^…$`. Before records are split into cells, a raw-byte prefilter (see
//! [`crate::visit`]) skips the records that cannot contain a match.

use std::ops::Range;
use std::sync::Arc;

use parking_lot::Mutex;
use regex::Regex;

use crate::chunked::ChunkedVec;
use crate::dialect::Compiled;
use crate::encoding::TextEncoding;
use crate::fields::{FieldSpan, split, unquote};
use crate::job::{Job, Notify};
use crate::overlay::{ColRef, Overlay, RowRef};
use crate::scan::Flow;
use crate::source::Source;
use crate::visit::{Prefilter, visit_records};
use std::borrow::Cow;

/// Results kept by default; more are counted as "more exist".
pub const DEFAULT_CAP: usize = 1_000_000;

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub whole_cell: bool,
    /// Only these columns (`None` = all columns).
    pub columns: Option<Vec<usize>>,
}

/// Matches cell values against [`SearchOptions`].
#[derive(Debug)]
pub struct Matcher {
    re: Regex,
    opts: SearchOptions,
    max_col: Option<usize>,
}

impl Matcher {
    pub fn new(opts: &SearchOptions) -> Result<Matcher, String> {
        let core = core_pattern(opts);
        let mut pat = if opts.whole_word {
            format!(r"\b(?:{core})\b")
        } else {
            format!("(?:{core})")
        };
        if opts.whole_cell {
            pat = format!("^{pat}$");
        }
        if !opts.case_sensitive {
            pat = format!("(?i){pat}");
        }
        let re = regex::RegexBuilder::new(&pat)
            .size_limit(64 << 20)
            .build()
            .map_err(|e| e.to_string())?;
        let max_col = opts.columns.as_ref().and_then(|c| c.iter().max().copied());
        Ok(Matcher {
            re,
            opts: opts.clone(),
            max_col,
        })
    }

    pub fn options(&self) -> &SearchOptions {
        &self.opts
    }

    pub fn is_match(&self, cell: &str) -> bool {
        self.re.is_match(cell)
    }

    /// Byte ranges of (non-empty) matches in `cell`, at most `max`.
    pub fn ranges(&self, cell: &str, max: usize) -> Vec<Range<usize>> {
        self.re
            .find_iter(cell)
            .filter(|m| !m.is_empty())
            .take(max)
            .map(|m| m.range())
            .collect()
    }

    /// `cell` with its matches replaced by `with`. In regex mode `$1` and
    /// `${name}` expand to capture groups; otherwise `with` is literal.
    /// Empty matches are handled the regex way (no infinite loop).
    pub fn replace<'a>(&self, cell: &'a str, with: &str, all: bool) -> Cow<'a, str> {
        match (self.opts.regex, all) {
            (true, true) => self.re.replace_all(cell, with),
            (true, false) => self.re.replace(cell, with),
            (false, true) => self.re.replace_all(cell, regex::NoExpand(with)),
            (false, false) => self.re.replace(cell, regex::NoExpand(with)),
        }
    }

    /// Is column `c` searched?
    pub fn wants_column(&self, c: usize) -> bool {
        self.opts
            .columns
            .as_ref()
            .is_none_or(|cols| cols.contains(&c))
    }

    /// Fields to split per record (all, or up to the last searched column).
    fn fields_needed(&self) -> usize {
        self.max_col.map_or(usize::MAX, |m| m + 1)
    }

    /// A raw-byte prefilter for files in `enc`, when one is exact.
    pub fn prefilter(&self, enc: TextEncoding) -> Option<Prefilter> {
        prefilter_for(&self.opts, enc)
    }
}

fn core_pattern(opts: &SearchOptions) -> String {
    if opts.regex {
        opts.pattern.clone()
    } else {
        regex::escape(&opts.pattern)
    }
}

/// A raw-byte prefilter that never rejects a record containing a match.
///
/// * UTF-8 files: the pattern itself (anchors excluded: in raw bytes they
///   would refer to the record, not the cell). Whole word/cell are left to
///   the exact match.
/// * Other encodings (UTF-16, code pages): a case-sensitive literal,
///   transcoded to the file's encoding. Otherwise records are decoded.
pub fn prefilter_for(opts: &SearchOptions, enc: TextEncoding) -> Option<Prefilter> {
    if opts.pattern.is_empty() {
        return None;
    }
    let literal_cs = !opts.regex && opts.case_sensitive;
    match enc {
        TextEncoding::Utf8 if literal_cs => Some(Prefilter::literal(opts.pattern.as_bytes())),
        TextEncoding::Utf8 => {
            let core = core_pattern(opts);
            let hir = regex_syntax::parse(&core).ok()?;
            if hir.properties().look_set().contains_anchor() {
                return None;
            }
            let pat = if opts.case_sensitive {
                core
            } else {
                format!("(?i){core}")
            };
            regex::bytes::RegexBuilder::new(&pat)
                .size_limit(64 << 20)
                .build()
                .ok()
                .map(Prefilter::Regex)
        }
        _ if literal_cs => Prefilter::transcoded(&opts.pattern, enc),
        _ => None,
    }
}

/// One matching cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hit {
    /// Byte offset of the record.
    pub offset: u64,
    /// Record number (exact: the search scans from the first record).
    pub record: u64,
    pub col: u32,
}

/// Shared, growing search results.
#[derive(Debug, Default)]
pub struct SearchData {
    /// Hits in file order (at most the cap).
    pub hits: ChunkedVec<Hit>,
    /// More hits exist beyond the cap.
    pub capped: bool,
    pub records_scanned: u64,
    pub done: bool,
    pub error: Option<String>,
}

/// A running (or finished) search.
pub struct Search {
    pub job: Job,
    pub data: Arc<Mutex<SearchData>>,
    pub matcher: Arc<Matcher>,
}

impl Search {
    /// Index of the first hit after `(offset, col)`, wrapping around.
    pub fn next_after(&self, offset: u64, col: usize) -> Option<Hit> {
        let d = self.data.lock();
        let key = (offset, col as u32);
        let i = d.hits.partition_point(|h| (h.offset, h.col) <= key);
        d.hits.get(i).or(d.hits.first())
    }

    /// The last hit before `(offset, col)`, wrapping around.
    pub fn prev_before(&self, offset: u64, col: usize) -> Option<Hit> {
        let d = self.data.lock();
        let key = (offset, col as u32);
        let i = d.hits.partition_point(|h| (h.offset, h.col) < key);
        if i > 0 {
            d.hits.get(i - 1)
        } else {
            d.hits.last()
        }
    }
}

/// Everything a search job needs about the document.
#[derive(Clone)]
pub struct Target {
    pub src: Arc<Source>,
    pub dialect: Arc<Compiled>,
    pub encoding: TextEncoding,
    pub data_start: u64,
    /// Records before the first data row (the header is not searched).
    pub header_records: u64,
    /// Unsaved edits to take into account (deleted rows are skipped, edited
    /// cells are matched on their new values).
    pub overlay: Option<Arc<Overlay>>,
}

impl Target {
    /// Offsets of records with edits, for [`visit_records`]'s `forced`.
    pub(crate) fn forced(&self) -> Vec<u64> {
        self.overlay
            .as_ref()
            .map(|o| o.edited_offsets().collect())
            .unwrap_or_default()
    }

    /// Is the record at `offset` deleted?
    pub(crate) fn deleted(&self, offset: u64) -> bool {
        self.overlay.as_ref().is_some_and(|o| o.is_deleted(offset))
    }

    /// The values of a record in logical column order, with edits applied.
    /// `spans` must hold all fields of `content`.
    pub(crate) fn values<'a>(
        &self,
        offset: u64,
        content: &'a [u8],
        spans: &[FieldSpan],
    ) -> Vec<(usize, Cow<'a, str>)> {
        let raw = |i: usize| -> Cow<'a, str> {
            match spans.get(i) {
                Some(f) => match unquote(&self.dialect, &content[f.start..f.end], f.quoted) {
                    Cow::Borrowed(b) => self.encoding.decode(b),
                    Cow::Owned(v) => Cow::Owned(self.encoding.decode(&v).into_owned()),
                },
                None => Cow::Borrowed(""),
            }
        };
        let ov = match &self.overlay {
            Some(ov) if ov.row_edits(offset).is_some() || ov.columns().is_some() => ov,
            _ => return (0..spans.len()).map(|i| (i, raw(i))).collect(),
        };
        let n = match ov.columns() {
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
                cols.len() + spans.len().saturating_sub(mapped)
            }
            None => {
                let edited = ov
                    .row_edits(offset)
                    .into_iter()
                    .flat_map(|e| e.keys())
                    .filter_map(|c| {
                        if let ColRef::Src(i) = c {
                            Some(*i as usize + 1)
                        } else {
                            None
                        }
                    })
                    .max()
                    .unwrap_or(0);
                spans.len().max(edited)
            }
        };
        (0..n)
            .map(|c| {
                let cref = ov.col_ref(c);
                let v = match (ov.cell(RowRef::Src(offset), cref), cref) {
                    (Some(e), _) => Cow::Owned(e.to_string()),
                    (None, ColRef::Src(i)) => raw(i as usize),
                    (None, ColRef::New(_)) => Cow::Borrowed(""),
                };
                (c, v)
            })
            .collect()
    }
}

/// Starts a search job. Fails only for an invalid regex.
pub fn start(
    target: Target,
    opts: SearchOptions,
    cap: usize,
    notify: Notify,
) -> Result<Search, String> {
    let matcher = Arc::new(Matcher::new(&opts)?);
    let data = Arc::new(Mutex::new(SearchData::default()));
    let (m, out) = (matcher.clone(), data.clone());
    let job = Job::spawn("search", notify, move |ctl| {
        let pre = m.prefilter(target.encoding);
        let mut batch: Vec<Hit> = Vec::new();
        let mut found = 0usize;
        let mut spans = Vec::new();
        let flush = |batch: &mut Vec<Hit>, records: u64, capped: bool| {
            let mut d = out.lock();
            d.hits.extend_from_slice(batch);
            batch.clear();
            d.records_scanned = records;
            d.capped |= capped;
        };
        let forced = target.forced();
        let fields = if target.overlay.is_some() {
            usize::MAX
        } else {
            m.fields_needed()
        };
        let res = visit_records(
            &target.src,
            &target.dialect,
            target.data_start,
            0,
            ctl,
            pre.as_ref(),
            &forced,
            |v| {
                if v.record < target.header_records || target.deleted(v.span.start) {
                    return Flow::Continue;
                }
                spans.clear();
                split(&target.dialect, v.content, 0, fields, &mut spans);
                for (c, text) in target.values(v.span.start, v.content, &spans) {
                    if !m.wants_column(c) {
                        continue;
                    }
                    if m.is_match(&text) {
                        if found >= cap {
                            flush(&mut batch, v.record, true);
                            return Flow::Stop;
                        }
                        found += 1;
                        batch.push(Hit {
                            offset: v.span.start,
                            record: v.record,
                            col: c as u32,
                        });
                    }
                }
                if batch.len() >= 4096 {
                    flush(&mut batch, v.record, false);
                    ctl.notify();
                }
                Flow::Continue
            },
        );
        let mut d = out.lock();
        d.hits.extend_from_slice(&batch);
        batch.clear();
        match res {
            Ok(o) => d.records_scanned = o.records,
            Err(e) => {
                d.error = Some(format!("reading the file failed: {e}"));
                return Err(e.to_string());
            }
        }
        d.done = true;
        Ok(())
    });
    Ok(Search { job, data, matcher })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::no_notify;
    use crate::source::AccessMode;

    fn opts(p: &str) -> SearchOptions {
        SearchOptions {
            pattern: p.into(),
            case_sensitive: true,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn matcher_options() {
        assert!(Matcher::new(&opts("b.c")).unwrap().is_match("ab.cd"));
        assert!(!Matcher::new(&opts("b.c")).unwrap().is_match("abxcd"));
        let re = SearchOptions {
            regex: true,
            ..opts("b.c")
        };
        assert!(Matcher::new(&re).unwrap().is_match("abxcd"));
        let ci = SearchOptions {
            case_sensitive: false,
            ..opts("straße")
        };
        assert!(Matcher::new(&ci).unwrap().is_match("STRASSE straße"));
        let ci = SearchOptions {
            case_sensitive: false,
            ..opts("ПРИВЕТ")
        };
        assert!(Matcher::new(&ci).unwrap().is_match("привет мир"));
        let ww = SearchOptions {
            whole_word: true,
            ..opts("cat")
        };
        assert!(Matcher::new(&ww).unwrap().is_match("a cat sat"));
        assert!(!Matcher::new(&ww).unwrap().is_match("concatenate"));
        let wc = SearchOptions {
            whole_cell: true,
            ..opts("42")
        };
        assert!(Matcher::new(&wc).unwrap().is_match("42"));
        assert!(!Matcher::new(&wc).unwrap().is_match("142"));
        assert!(
            Matcher::new(&SearchOptions {
                regex: true,
                ..opts("(")
            })
            .is_err()
        );
    }

    #[test]
    fn empty_matches_are_fine() {
        let m = Matcher::new(&SearchOptions {
            regex: true,
            ..opts("x*")
        })
        .unwrap();
        assert!(m.is_match("abc"));
        assert!(m.ranges("abc", 10).is_empty());
        assert_eq!(m.ranges("axxb", 10), vec![1..3]);
    }

    #[test]
    fn prefilter_choice() {
        assert!(prefilter_for(&opts("x"), TextEncoding::Utf8).is_some());
        assert!(
            prefilter_for(
                &SearchOptions {
                    regex: true,
                    ..opts("^x")
                },
                TextEncoding::Utf8
            )
            .is_none()
        );
        assert!(
            prefilter_for(
                &SearchOptions {
                    case_sensitive: false,
                    ..opts("x")
                },
                TextEncoding::Utf8
            )
            .is_some()
        );
        assert!(prefilter_for(&opts("x"), TextEncoding::Utf16Le).is_some());
        let cp1251 = TextEncoding::Other(encoding_rs::WINDOWS_1251);
        assert!(prefilter_for(&opts("abc"), cp1251).is_some());
        assert!(prefilter_for(&opts("абв"), cp1251).is_some());
        assert!(
            prefilter_for(
                &SearchOptions {
                    case_sensitive: false,
                    ..opts("абв")
                },
                cp1251
            )
            .is_none()
        );
    }

    fn search_file(data: &[u8], d: &Dialect, o: SearchOptions, cap: usize) -> (Vec<Hit>, bool) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, data).unwrap();
        let target = Target {
            src: Arc::new(Source::open(&p, AccessMode::Auto).unwrap()),
            dialect: Arc::new(d.compile().unwrap()),
            encoding: d.encoding,
            data_start: 0,
            header_records: d.has_header as u64,
            overlay: None,
        };
        let s = start(target, o, cap, no_notify()).unwrap();
        s.job.join();
        let data = s.data.lock();
        assert!(data.done);
        (data.hits.slice(0, data.hits.len()), data.capped)
    }

    #[test]
    fn finds_cells_in_order_and_respects_options() {
        let data =
            b"name,note\nann,\"has cat\"\nbob,dog\n\"c\"\"at\",cat\nzed,\"multi\ncat line\"\n";
        let d = Dialect::default();
        let (hits, capped) = search_file(data, &d, opts("cat"), 100);
        assert!(!capped);
        let cells: Vec<(u64, u32)> = hits.iter().map(|h| (h.record, h.col)).collect();
        assert_eq!(cells, vec![(1, 1), (3, 1), (4, 1)]);
        // The escaped record's first cell is `c"at`: no plain "cat" there,
        // but a regex across the decoded quote finds it.
        let (hits, _) = search_file(
            data,
            &d,
            SearchOptions {
                regex: true,
                ..opts("c.at")
            },
            100,
        );
        assert_eq!(
            hits.iter().map(|h| (h.record, h.col)).collect::<Vec<_>>(),
            vec![(3, 0)]
        );
        // Column scope.
        let (hits, _) = search_file(
            data,
            &d,
            SearchOptions {
                columns: Some(vec![0]),
                ..opts("a")
            },
            100,
        );
        assert_eq!(
            hits.iter().map(|h| h.record).collect::<Vec<_>>(),
            vec![1, 3]
        );
        // Header row is not searched.
        let (hits, _) = search_file(data, &d, opts("name"), 100);
        assert!(hits.is_empty());
    }

    #[test]
    fn non_utf8_files_are_searchable() {
        let text = "id,имя\n1,Анна\n2,Борис\n3,анна\n";
        for enc in [
            TextEncoding::Utf16Le,
            TextEncoding::Utf16Be,
            TextEncoding::Other(encoding_rs::WINDOWS_1251),
        ] {
            let mut bytes = Vec::new();
            enc.encode(text, &mut bytes);
            let d = Dialect {
                encoding: enc,
                ..Dialect::default()
            };
            let (hits, _) = search_file(&bytes, &d, opts("Анна"), 100);
            assert_eq!(
                hits.iter().map(|h| h.record).collect::<Vec<_>>(),
                vec![1],
                "{enc:?}"
            );
            let ci = SearchOptions {
                case_sensitive: false,
                ..opts("АННА")
            };
            let (hits, _) = search_file(&bytes, &d, ci, 100);
            assert_eq!(
                hits.iter().map(|h| h.record).collect::<Vec<_>>(),
                vec![1, 3],
                "{enc:?}"
            );
        }
    }

    #[test]
    fn search_sees_unsaved_edits() {
        use crate::overlay::{Deleted, Overlay};
        let data = b"h\nalpha\nbeta\nneedle here\n";
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, data).unwrap();
        let d = Dialect::default();
        let offs: Vec<u64> = crate::scan::scan_all(&d.compile().unwrap(), data, 0, usize::MAX)
            .iter()
            .map(|r| r.start)
            .collect();
        let mut ov = Overlay::new();
        ov.set_cells(
            vec![(RowRef::Src(offs[2]), ColRef::Src(0), "a needle".into())],
            |_, _| None,
        );
        ov.delete_rows(
            &[],
            &[Deleted {
                start: offs[3],
                end: data.len() as u64,
                records: 1,
            }],
        );
        let target = Target {
            src: Arc::new(Source::open(&p, AccessMode::Auto).unwrap()),
            dialect: Arc::new(d.compile().unwrap()),
            encoding: d.encoding,
            data_start: 0,
            header_records: 1,
            overlay: Some(Arc::new(ov)),
        };
        let s = start(target, opts("needle"), 100, no_notify()).unwrap();
        s.job.join();
        let hits = s.data.lock().hits.slice(0, 10);
        assert_eq!(
            hits.iter().map(|h| h.offset).collect::<Vec<_>>(),
            vec![offs[2]]
        );
    }

    #[test]
    fn cap_sets_more_exist() {
        let mut data = b"a\n".to_vec();
        for _ in 0..100 {
            data.extend_from_slice(b"x\n");
        }
        let (hits, capped) = search_file(&data, &Dialect::default(), opts("x"), 10);
        assert_eq!(hits.len(), 10);
        assert!(capped);
    }

    #[test]
    fn prefiltered_and_exhaustive_searches_agree() {
        // Random CSV with quotes, escapes and multi-line fields: the result
        // must not depend on whether the prefilter is used.
        let mut seed = 77u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut data = b"h1,h2,h3\n".to_vec();
        for _ in 0..3000 {
            for c in 0..3 {
                if c > 0 {
                    data.push(b',');
                }
                let words = ["ab", "abc", "x\"y", "a b", "AB", "c,d", "e\nf"];
                let w = words[rnd(words.len() as u64) as usize];
                if w.contains(['"', ',', '\n']) || rnd(3) == 0 {
                    data.push(b'"');
                    data.extend_from_slice(w.replace('"', "\"\"").as_bytes());
                    data.push(b'"');
                } else {
                    data.extend_from_slice(w.as_bytes());
                }
            }
            data.push(b'\n');
        }
        let d = Dialect::default();
        for o in [
            opts("ab"),
            opts("x\"y"),
            SearchOptions {
                case_sensitive: false,
                ..opts("ab")
            },
            SearchOptions {
                regex: true,
                ..opts("b.")
            },
            SearchOptions {
                whole_cell: true,
                ..opts("ab")
            },
            SearchOptions {
                whole_word: true,
                case_sensitive: false,
                ..opts("b")
            },
            opts("c,d"),
            opts("e\nf"),
        ] {
            let (fast, _) = search_file(&data, &d, o.clone(), usize::MAX);
            // Exhaustive reference: split every record.
            let c = d.compile().unwrap();
            let m = Matcher::new(&o).unwrap();
            let mut want = Vec::new();
            for r in crate::scan::scan_all(&c, &data, 0, usize::MAX)
                .iter()
                .skip(1)
            {
                let content = &data[r.start as usize..r.content_end as usize];
                for (i, f) in crate::fields::split_all(&c, content).iter().enumerate() {
                    let v = unquote(&c, &content[f.start..f.end], f.quoted);
                    if m.is_match(&String::from_utf8_lossy(&v)) {
                        want.push((r.start, i as u32));
                    }
                }
            }
            let got: Vec<(u64, u32)> = fast.iter().map(|h| (h.offset, h.col)).collect();
            assert_eq!(got, want, "{o:?}");
        }
    }
}
