//! Dialect auto-detection from the first ~1 MB of a file.
//!
//! The sample is decoded to text, then every candidate (delimiter × quote)
//! is parsed and scored by how consistent the field counts are and how
//! "clean" the resulting cells look (numbers, dates, or text that does not
//! contain another candidate delimiter). The result is only a suggestion:
//! the parsing dialog shows it and the user can override everything.

use crate::dialect::{Delimiter, Dialect, Escape};
use crate::encoding::{self, DetectedEncoding, TextEncoding};
use crate::fields::{split_all, unquote};
use crate::index::LineEndings;
use crate::infer::{ValueKind, classify};
use crate::scan::scan_all;

/// Bytes read from the start of a file for detection and previews.
pub const SAMPLE_BYTES: usize = 1 << 20;
const SCORE_RECORDS: usize = 1000;
const SCORE_BYTES: usize = 256 << 10;
const CLEAN_RECORDS: usize = 200;

/// What was detected, with human-readable notes for the parsing dialog.
#[derive(Clone, Debug)]
pub struct Detection {
    pub dialect: Dialect,
    pub encoding: DetectedEncoding,
    /// An Excel `sep=X` line was found (and is skipped).
    pub sep_line: bool,
    pub line_endings: LineEndings,
    pub notes: Vec<String>,
}

struct Candidate {
    delimiter: Delimiter,
    collapse: bool,
    weight: f64,
}

fn candidates() -> Vec<Candidate> {
    let c = |d: char, w: f64| Candidate {
        delimiter: Delimiter::Char(d),
        collapse: false,
        weight: w,
    };
    vec![
        c(',', 1.0),
        c('\t', 1.0),
        c(';', 0.99),
        c('|', 0.98),
        Candidate {
            delimiter: Delimiter::Whitespace,
            collapse: false,
            weight: 0.9,
        },
        c(':', 0.85),
        c('^', 0.8),
        c('~', 0.8),
        c(' ', 0.7),
    ]
}

const OTHER_DELIMS: [char; 4] = [',', ';', '\t', '|'];

/// Detects the dialect of a file from its first bytes.
pub fn detect(sample: &[u8], whole_file: bool) -> Detection {
    let enc = encoding::detect(sample, whole_file);
    let mut notes =
        vec![format!("encoding {} ({:?})", enc.encoding.name(), enc.evidence).to_lowercase()];
    let body = &sample[enc.bom_len..];
    let text = enc.encoding.decode(body);
    let mut text: &str = &text;
    if text.len() > SCORE_BYTES {
        let mut cut = SCORE_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text = &text[..cut];
    }

    // Excel's "sep=;" first line.
    let mut sep_line = None;
    if let Some(first) = text.lines().next() {
        let f = first.trim_end_matches('\r');
        if f.len() >= 5
            && f.as_bytes()[..4].eq_ignore_ascii_case(b"sep=")
            && f[4..].chars().count() == 1
        {
            sep_line = f[4..].chars().next();
            let skip = first.len()
                + text[first.len()..]
                    .chars()
                    .take_while(|&c| c == '\r' || c == '\n')
                    .take(2)
                    .count();
            text = &text[skip.min(text.len())..];
            notes.push(format!(
                "Excel \"sep=\" line found: delimiter {}",
                Delimiter::Char(sep_line.unwrap_or(',')).label()
            ));
        }
    }

    let quote_choices = [(Some('"'), 1.0), (Some('\''), 0.95)];
    let mut best: Option<(f64, Dialect)> = None;
    let cands: Vec<Candidate> = match sep_line {
        Some(c) => vec![Candidate {
            delimiter: Delimiter::Char(c),
            collapse: false,
            weight: 1.0,
        }],
        None => candidates(),
    };
    for cand in &cands {
        for &(quote, qweight) in &quote_choices {
            let d = Dialect {
                delimiter: cand.delimiter.clone(),
                collapse_delimiters: cand.collapse,
                quote,
                encoding: TextEncoding::Utf8,
                ..Dialect::default()
            };
            let s = score(&d, text.as_bytes()) * cand.weight * qweight;
            if best.as_ref().is_none_or(|b| s > b.0) {
                best = Some((s, d));
            }
        }
    }
    let (best_score, mut dialect) = best.unwrap_or((0.0, Dialect::default()));
    if best_score <= 0.0 && sep_line.is_none() {
        // One column (or nothing recognizable): keep the comma.
        dialect = Dialect::default();
        notes.push("no consistent delimiter found; treating each line as one field".into());
    } else {
        notes.push(format!(
            "delimiter {} (score {:.2})",
            dialect.delimiter.label(),
            best_score
        ));
    }
    if dialect.quote == Some('"') && backslash_escaped(text) {
        dialect.escape = Escape::Backslash;
        notes.push("quotes are escaped with a backslash".into());
    }

    let c = dialect.compile().unwrap_or_else(|_| {
        Dialect::default()
            .compile()
            .expect("default dialect compiles")
    });
    let recs = scan_all(&c, text.as_bytes(), 0, SCORE_RECORDS);
    let mut line_endings = LineEndings::default();
    for r in &recs {
        line_endings.add(r.term);
    }
    let rows: Vec<Vec<String>> = recs
        .iter()
        .take(CLEAN_RECORDS)
        .map(|r| {
            let content = &text.as_bytes()[r.start as usize..r.content_end as usize];
            split_all(&c, content)
                .iter()
                .map(|f| {
                    String::from_utf8_lossy(&unquote(&c, &content[f.start..f.end], f.quoted))
                        .into_owned()
                })
                .collect()
        })
        .collect();
    dialect.has_header = has_header(&rows);
    notes.push(
        if dialect.has_header {
            "first row looks like a header"
        } else {
            "first row looks like data"
        }
        .into(),
    );

    dialect.encoding = enc.encoding;
    dialect.skip_lines = sep_line.is_some() as u32;
    Detection {
        dialect,
        encoding: enc,
        sep_line: sep_line.is_some(),
        line_endings,
        notes,
    }
}

/// True when quotes inside fields are escaped as `\"` and never doubled
/// (`""` inside a field, as opposed to an empty field or `\""`).
fn backslash_escaped(text: &str) -> bool {
    let b = text.as_bytes();
    let (mut backslash, mut doubled) = (0usize, 0usize);
    let boundary = |c: u8| matches!(c, b',' | b';' | b'\t' | b'|' | b'\n' | b'\r');
    for i in 1..b.len() {
        if b[i] != b'"' {
            continue;
        }
        if b[i - 1] == b'\\' {
            backslash += 1;
        } else if b[i - 1] == b'"' {
            let before = if i >= 2 { b[i - 2] } else { b'\n' };
            let after = b.get(i + 1).copied().unwrap_or(b'\n');
            if before != b'\\' && !boundary(before) && !boundary(after) {
                doubled += 1;
            }
        }
    }
    backslash > 0 && doubled == 0
}

/// Consistency × cleanliness of the field counts produced by `d`.
fn score(d: &Dialect, text: &[u8]) -> f64 {
    let Ok(c) = d.compile() else { return 0.0 };
    let recs = scan_all(&c, text, 0, SCORE_RECORDS);
    // Ignore a final record cut by the end of the sample.
    let recs = if recs.len() > 2
        && recs
            .last()
            .is_some_and(|r| r.term == crate::scan::Term::None)
    {
        &recs[..recs.len() - 1]
    } else {
        &recs[..]
    };
    let recs: Vec<_> = recs.iter().filter(|r| r.content_end > r.start).collect();
    if recs.is_empty() {
        return 0.0;
    }
    let mut counts: Vec<u64> = recs.iter().map(|r| r.fields).collect();
    counts.sort_unstable();
    let (mut mode, mut mode_n, mut run, mut prev) = (0, 0, 0, u64::MAX);
    for &k in &counts {
        run = if k == prev { run + 1 } else { 1 };
        prev = k;
        if run > mode_n {
            mode_n = run;
            mode = k;
        }
    }
    if mode <= 1 {
        return 0.0;
    }
    let consistency = mode_n as f64 / counts.len() as f64;
    let delim_char = match &d.delimiter {
        Delimiter::Char(ch) => Some(*ch),
        _ => None,
    };
    let (mut clean, mut total) = (0usize, 0usize);
    for r in recs.iter().take(CLEAN_RECORDS) {
        let content = &text[r.start as usize..r.content_end as usize];
        for f in split_all(&c, content) {
            let v = String::from_utf8_lossy(&unquote(&c, &content[f.start..f.end], f.quoted))
                .into_owned();
            total += 1;
            let kind = classify(&v);
            let has_other = OTHER_DELIMS
                .iter()
                .any(|&o| Some(o) != delim_char && v.contains(o));
            if kind != ValueKind::Text || !has_other {
                clean += 1;
            }
        }
    }
    let cleanliness = if total == 0 {
        0.0
    } else {
        clean as f64 / total as f64
    };
    consistency * cleanliness * (1.0 + 0.002 * mode.min(50) as f64)
}

/// Python-`csv.Sniffer`-style vote: a column whose data is numeric (or
/// dates) but whose first value is text suggests a header.
fn has_header(rows: &[Vec<String>]) -> bool {
    let Some((first, rest)) = rows.split_first() else {
        return true;
    };
    if rest.is_empty() {
        return first.iter().all(|v| classify(v) == ValueKind::Text);
    }
    let mut votes = 0i32;
    for (j, head) in first.iter().enumerate() {
        let kinds: Vec<ValueKind> = rest
            .iter()
            .filter_map(|r| r.get(j))
            .map(|v| classify(v))
            .filter(|k| *k != ValueKind::Empty)
            .collect();
        if kinds.is_empty() {
            continue;
        }
        let typed = kinds.iter().filter(|k| **k != ValueKind::Text).count();
        if typed * 10 >= kinds.len() * 8 {
            match classify(head) {
                ValueKind::Text => votes += 1,
                ValueKind::Empty => {}
                _ => votes -= 1,
            }
        }
    }
    if votes != 0 {
        return votes > 0;
    }
    // All-text columns: a header has unique, non-empty names that do not
    // repeat in the data below.
    let unique = first.iter().collect::<std::collections::HashSet<_>>().len() == first.len();
    let non_empty = first.iter().all(|v| !v.trim().is_empty());
    let repeats = first
        .iter()
        .enumerate()
        .any(|(j, h)| rest.iter().any(|r| r.get(j) == Some(h)));
    unique && non_empty && !repeats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(s: &str) -> Dialect {
        detect(s.as_bytes(), true).dialect
    }

    #[test]
    fn common_delimiters() {
        assert_eq!(det("a,b,c\n1,2,3\n4,5,6\n").delimiter, Delimiter::Char(','));
        assert_eq!(det("a\tb\tc\n1\t2\t3\n").delimiter, Delimiter::Char('\t'));
        assert_eq!(det("a|b|c\n1|2|3\n").delimiter, Delimiter::Char('|'));
        assert_eq!(
            det("name;city\nAnn;Paris\nBob;Rome\n").delimiter,
            Delimiter::Char(';')
        );
    }

    #[test]
    fn european_decimal_commas() {
        let d = det("id;price;qty\n1;3,50;2\n2;10,00;1\n3;0,99;7\n");
        assert_eq!(d.delimiter, Delimiter::Char(';'));
        assert!(d.has_header);
    }

    #[test]
    fn quoted_commas_do_not_confuse() {
        let d = det("name,comment\n\"Smith, J\",\"hi, there\"\n\"Doe, A\",ok\n");
        assert_eq!(d.delimiter, Delimiter::Char(','));
        assert_eq!(d.quote, Some('"'));
    }

    #[test]
    fn space_aligned() {
        let d = det(
            "id    name      value\n1     foo       3.5\n22    barbaz    10.25\n333   q         7\n",
        );
        assert_eq!(d.delimiter, Delimiter::Whitespace);
    }

    #[test]
    fn non_ascii_first_line() {
        // Byte 4 is inside a multi-byte character: must not panic.
        assert_eq!(
            det("имя,город\nАнна,Ташкент\n").delimiter,
            Delimiter::Char(',')
        );
    }

    #[test]
    fn sep_line() {
        let r = detect(b"sep=;\r\na;b\r\n1;2\r\n", true);
        assert!(r.sep_line);
        assert_eq!(r.dialect.delimiter, Delimiter::Char(';'));
        assert_eq!(r.dialect.skip_lines, 1);
    }

    #[test]
    fn header_detection() {
        assert!(det("id,amount\n1,10\n2,20\n").has_header);
        assert!(!det("1,10\n2,20\n3,30\n").has_header);
        assert!(det("first,last\nAnn,Lee\nBob,Ray\n").has_header);
        assert!(!det("Ann,Lee\nBob,Ray\nAnn,Fox\n").has_header);
    }

    #[test]
    fn single_column_and_empty() {
        assert_eq!(det("hello\nworld\n").delimiter, Delimiter::Char(','));
        assert_eq!(det("").delimiter, Delimiter::Char(','));
    }

    #[test]
    fn backslash_escapes() {
        let d = det("a,b\n\"x \\\"q\\\"\",2\n\"y\",3\n");
        assert_eq!(d.escape, Escape::Backslash);
    }

    #[test]
    fn utf16_file() {
        let mut bytes = vec![0xFF, 0xFE];
        TextEncoding::Utf16Le.encode("a;b\n1;2\n3;4\n", &mut bytes);
        let r = detect(&bytes, true);
        assert_eq!(r.dialect.encoding, TextEncoding::Utf16Le);
        assert_eq!(r.dialect.delimiter, Delimiter::Char(';'));
        assert_eq!(r.encoding.bom_len, 2);
    }
}
