//! Writing values and records: quoting rules, encoding, line endings.

use crate::dialect::{Delimiter, Dialect, Escape};
use crate::encoding::TextEncoding;
use crate::scan::Term;

/// When values are quoted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuotePolicy {
    /// Only values that need it (delimiter, quote, line break, …).
    #[default]
    Minimal,
    /// Every value.
    Always,
}

/// Encodes values and separators for one output dialect.
#[derive(Clone, Debug)]
pub struct Writer {
    enc: TextEncoding,
    delim_text: String,
    whitespace: bool,
    quote: char,
    /// The dialect has no quote character; `"` is used when one is needed.
    quote_fallback: bool,
    escape: Escape,
    policy: QuotePolicy,
    trim: bool,
    comment: Option<String>,
    delim: Vec<u8>,
    eol: Vec<u8>,
    /// Bytes that may make a value need quotes (a quick first pass).
    special: [bool; 256],
    /// Characters that could not be represented in the encoding.
    pub unmappable: usize,
    /// A value needed quotes although the dialect has no quote character.
    pub used_fallback_quote: bool,
}

impl Writer {
    pub fn new(d: &Dialect, line_ending: Term, policy: QuotePolicy) -> Writer {
        let delim_text = d.delimiter.write_str();
        let mut delim = Vec::new();
        d.encoding.encode(&delim_text, &mut delim);
        let eol_text = match line_ending {
            Term::CrLf => "\r\n",
            Term::Cr => "\r",
            Term::Lf | Term::None => "\n",
        };
        let mut eol = Vec::new();
        d.encoding.encode(eol_text, &mut eol);
        let quote = d.quote.unwrap_or('"');
        let mut special = [false; 256];
        let mut mark = |c: char| {
            if c.is_ascii() {
                special[c as usize] = true;
            } else {
                special[0x80..].iter_mut().for_each(|s| *s = true);
            }
        };
        for c in [quote, '\r', '\n'] {
            mark(c);
        }
        if let Some(c) = delim_text.chars().next() {
            mark(c);
        }
        if d.escape == Escape::Backslash {
            mark('\\');
        }
        if d.delimiter == Delimiter::Whitespace || d.trim {
            mark(' ');
            mark('\t');
        }
        Writer {
            enc: d.encoding,
            delim_text,
            whitespace: d.delimiter == Delimiter::Whitespace,
            quote,
            quote_fallback: d.quote.is_none(),
            escape: d.escape,
            policy,
            trim: d.trim,
            comment: d.comment_prefix.clone().filter(|c| !c.is_empty()),
            delim,
            eol,
            special,
            unmappable: 0,
            used_fallback_quote: false,
        }
    }

    /// Does `v` need quotes to read back as the same value?
    pub fn needs_quotes(&self, v: &str, first: bool) -> bool {
        if self.policy == QuotePolicy::Always {
            return true;
        }
        if (self.whitespace && v.is_empty())
            || (first && self.comment.as_deref().is_some_and(|c| v.starts_with(c)))
        {
            return true;
        }
        if !v.bytes().any(|b| self.special[b as usize]) {
            return false;
        }
        v.contains(self.delim_text.as_str())
            || v.contains(self.quote)
            || v.contains(['\r', '\n'])
            || (self.escape == Escape::Backslash && v.contains('\\'))
            || (self.whitespace && (v.is_empty() || v.contains([' ', '\t'])))
            || (self.trim && v != v.trim_matches([' ', '\t']))
            || (first && self.comment.as_deref().is_some_and(|c| v.starts_with(c)))
    }

    /// Appends one value, quoted if needed.
    pub fn value(&mut self, v: &str, first: bool, out: &mut Vec<u8>) {
        if !self.needs_quotes(v, first) {
            self.unmappable += self.enc.encode(v, out);
            return;
        }
        if self.quote_fallback {
            self.used_fallback_quote = true;
        }
        let q = self.quote;
        let mut s = String::with_capacity(v.len() + 2);
        s.push(q);
        for ch in v.chars() {
            if ch == q {
                s.push(if self.escape == Escape::Backslash {
                    '\\'
                } else {
                    q
                });
            } else if ch == '\\' && self.escape == Escape::Backslash {
                s.push('\\');
            }
            s.push(ch);
        }
        s.push(q);
        self.unmappable += self.enc.encode(&s, out);
    }

    pub fn delimiter(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.delim);
    }

    pub fn line_end(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.eol);
    }

    /// Appends a whole record (values, delimiters, line ending).
    pub fn record<'a>(&mut self, values: impl IntoIterator<Item = &'a str>, out: &mut Vec<u8>) {
        for (i, v) in values.into_iter().enumerate() {
            if i > 0 {
                self.delimiter(out);
            }
            self.value(v, i == 0, out);
        }
        self.line_end(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::{split_all, unquote};

    fn roundtrip(d: &Dialect, values: &[&str]) {
        let mut w = Writer::new(d, Term::Lf, QuotePolicy::Minimal);
        let mut out = Vec::new();
        w.record(values.iter().copied(), &mut out);
        let c = d.compile().unwrap();
        let recs = crate::scan::scan_all(&c, &out, 0, usize::MAX);
        assert_eq!(recs.len(), 1, "{:?}", String::from_utf8_lossy(&out));
        let content = &out[recs[0].start as usize..recs[0].content_end as usize];
        let got: Vec<String> = split_all(&c, content)
            .iter()
            .map(|f| {
                d.encoding
                    .decode(&unquote(&c, &content[f.start..f.end], f.quoted))
                    .into_owned()
            })
            .collect();
        assert_eq!(got, values, "{:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn minimal_quoting_reads_back() {
        let d = Dialect::default();
        roundtrip(
            &d,
            &[
                "plain",
                "a,b",
                "say \"hi\"",
                "multi\nline",
                "cr\r",
                "",
                " pad ",
            ],
        );
        roundtrip(
            &Dialect {
                escape: Escape::Backslash,
                ..Dialect::default()
            },
            &["a\"b", "back\\slash", "x"],
        );
        roundtrip(
            &Dialect {
                delimiter: Delimiter::Str("||".into()),
                ..Dialect::default()
            },
            &["a|b", "c||d"],
        );
        roundtrip(
            &Dialect {
                delimiter: Delimiter::Whitespace,
                ..Dialect::default()
            },
            &["a", "b c", "", "d"],
        );
        roundtrip(
            &Dialect {
                trim: true,
                ..Dialect::default()
            },
            &[" keep ", "x"],
        );
        roundtrip(
            &Dialect {
                comment_prefix: Some("#".into()),
                ..Dialect::default()
            },
            &["#not a comment", "#fine"],
        );
        roundtrip(
            &Dialect {
                encoding: TextEncoding::Utf16Le,
                ..Dialect::default()
            },
            &["имя", "a,b"],
        );
    }

    #[test]
    fn quoting_rules() {
        let w = Writer::new(&Dialect::default(), Term::Lf, QuotePolicy::Minimal);
        assert!(!w.needs_quotes("abc", false));
        assert!(w.needs_quotes("a,b", false));
        assert!(!w.needs_quotes(" pad ", false));
        let w = Writer::new(&Dialect::default(), Term::Lf, QuotePolicy::Always);
        assert!(w.needs_quotes("abc", false));
    }

    #[test]
    fn no_quote_char_falls_back_to_double_quote() {
        let d = Dialect {
            quote: None,
            ..Dialect::default()
        };
        let mut w = Writer::new(&d, Term::CrLf, QuotePolicy::Minimal);
        let mut out = Vec::new();
        w.record(["a,b", "c"], &mut out);
        assert_eq!(out, b"\"a,b\",c\r\n");
        assert!(w.used_fallback_quote);
    }

    #[test]
    fn unmappable_characters_are_counted() {
        let d = Dialect {
            encoding: TextEncoding::Other(encoding_rs::WINDOWS_1251),
            ..Dialect::default()
        };
        let mut w = Writer::new(&d, Term::Lf, QuotePolicy::Minimal);
        let mut out = Vec::new();
        w.record(["Да", "✓"], &mut out);
        assert_eq!(w.unmappable, 1);
    }
}
