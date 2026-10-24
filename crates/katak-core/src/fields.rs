//! Splitting one record into fields and decoding field values.
//!
//! The rules are exactly those of [`crate::scan`]; the record's extent is
//! already known, so only delimiters and quotes matter here.

use std::borrow::Cow;

use memchr::{memchr, memchr2};

use crate::dialect::Compiled;
use crate::encoding::TextEncoding;
use crate::units::{BACKSLASH, Units, is_blank};
use crate::with_units;

/// A field's raw byte range, relative to the start of the record content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FieldSpan {
    pub start: usize,
    pub end: usize,
    /// The field starts with an opening quote (after blanks when trimming).
    pub quoted: bool,
}

impl FieldSpan {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Splits record content `rec` (terminator excluded), starting at `from`,
/// which must be a field start: 0 or a value previously returned. Appends at
/// most `max` fields to `out`. Returns the start of the next field if the
/// record has more fields, `None` once its last field was appended.
pub fn split(
    d: &Compiled,
    rec: &[u8],
    from: usize,
    max: usize,
    out: &mut Vec<FieldSpan>,
) -> Option<usize> {
    if d.fast {
        split_fast(d, rec, from, max, out)
    } else {
        with_units!(d.width, U => split_generic::<U>(d, rec, from, max, out))
    }
}

/// Splits the whole record.
pub fn split_all(d: &Compiled, rec: &[u8]) -> Vec<FieldSpan> {
    let mut out = Vec::new();
    split(d, rec, 0, usize::MAX, &mut out);
    out
}

fn split_fast(
    d: &Compiled,
    rec: &[u8],
    from: usize,
    max: usize,
    out: &mut Vec<FieldSpan>,
) -> Option<usize> {
    let delim = d.delim_byte();
    let quote = d.quote.map(|q| q as u8);
    let len = rec.len();
    let mut p = from;
    let mut n = 0;
    loop {
        if n >= max {
            return Some(p);
        }
        let start = p;
        let mut q = p;
        if d.trim {
            while q < len && rec[q] != delim && (rec[q] == b' ' || rec[q] == b'\t') {
                q += 1;
            }
        }
        let mut quoted = false;
        let mut scan_from = p;
        if let Some(qc) = quote
            && q < len
            && rec[q] == qc
        {
            quoted = true;
            scan_from = closing_quote_fast(rec, q + 1, qc, d.backslash);
        }
        match memchr(delim, &rec[scan_from.min(len)..]) {
            None => {
                out.push(FieldSpan {
                    start,
                    end: len,
                    quoted,
                });
                return None;
            }
            Some(i) => {
                let e = scan_from + i;
                out.push(FieldSpan {
                    start,
                    end: e,
                    quoted,
                });
                n += 1;
                p = e + 1;
            }
        }
    }
}

/// Returns the index just past the closing quote (or `rec.len()` if the
/// quote is not closed), scanning from `k` (just after the opening quote).
fn closing_quote_fast(rec: &[u8], mut k: usize, qc: u8, backslash: bool) -> usize {
    let len = rec.len();
    loop {
        let hit = if backslash {
            memchr2(qc, b'\\', &rec[k..])
        } else {
            memchr(qc, &rec[k..])
        };
        let Some(i) = hit else { return len };
        let h = k + i;
        if rec[h] != qc {
            k = (h + 2).min(len);
        } else if h + 1 < len && rec[h + 1] == qc {
            k = h + 2;
        } else {
            return h + 1;
        }
    }
}

#[inline]
fn matches<U: Units>(buf: &[u8], full: usize, p: usize, pat: &[u32]) -> bool {
    !pat.is_empty()
        && p + pat.len() * U::W <= full
        && pat
            .iter()
            .enumerate()
            .all(|(k, &c)| U::get(buf, p + k * U::W) == c)
}

fn split_generic<U: Units>(
    d: &Compiled,
    rec: &[u8],
    from: usize,
    max: usize,
    out: &mut Vec<FieldSpan>,
) -> Option<usize> {
    let w = U::W;
    let len = rec.len();
    let full = len - len % w;
    let dlen = d.delim.len() * w;
    let mut p = from;
    if d.whitespace && from == 0 {
        while p < full && is_blank(U::get(rec, p)) {
            p += w;
        }
    }
    let mut n = 0;
    loop {
        if n >= max {
            return Some(p);
        }
        let start = p;
        let mut k = p;
        if d.trim || d.whitespace {
            while k < full && is_blank(U::get(rec, k)) && !matches::<U>(rec, full, k, &d.delim) {
                k += w;
            }
        }
        let mut quoted = false;
        match d.quote {
            Some(q) if k < full && U::get(rec, k) == q => {
                quoted = true;
                k += w;
                while k < full {
                    let u = U::get(rec, k);
                    if u == q {
                        if k + w < full && U::get(rec, k + w) == q {
                            k += 2 * w;
                            continue;
                        }
                        k += w;
                        break;
                    } else if d.backslash && u == BACKSLASH {
                        k = (k + 2 * w).min(full);
                    } else {
                        k += w;
                    }
                }
            }
            _ => k = p,
        }
        loop {
            if k >= full {
                out.push(FieldSpan {
                    start,
                    end: len,
                    quoted,
                });
                return None;
            }
            let u = U::get(rec, k);
            if d.whitespace {
                if is_blank(u) {
                    let mut j = k;
                    while j < full && is_blank(U::get(rec, j)) {
                        j += w;
                    }
                    out.push(FieldSpan {
                        start,
                        end: k,
                        quoted,
                    });
                    if j >= full {
                        // Trailing blanks are not a separator.
                        return None;
                    }
                    n += 1;
                    p = j;
                    break;
                }
                k += w;
            } else if matches::<U>(rec, full, k, &d.delim) {
                out.push(FieldSpan {
                    start,
                    end: k,
                    quoted,
                });
                n += 1;
                p = k + dlen;
                if d.collapse {
                    while matches::<U>(rec, full, p, &d.delim) {
                        p += dlen;
                    }
                }
                break;
            } else {
                k += w;
            }
        }
    }
}

/// The value of a field in the file's encoding: quotes removed, escapes
/// resolved, blanks trimmed when the dialect says so. Unquoted values (and
/// quoted values without escapes) are borrowed.
pub fn unquote<'a>(d: &Compiled, raw: &'a [u8], quoted: bool) -> Cow<'a, [u8]> {
    with_units!(d.width, U => unquote_generic::<U>(d, raw, quoted))
}

fn trim_units<U: Units>(raw: &[u8]) -> &[u8] {
    let w = U::W;
    let full = raw.len() - raw.len() % w;
    let mut a = 0;
    while a < full && is_blank(U::get(raw, a)) {
        a += w;
    }
    let mut b = full;
    while b > a && is_blank(U::get(raw, b - w)) {
        b -= w;
    }
    // Keep a dangling partial unit (malformed input) so nothing is lost.
    if b == full { &raw[a..] } else { &raw[a..b] }
}

fn unquote_generic<'a, U: Units>(d: &Compiled, raw: &'a [u8], quoted: bool) -> Cow<'a, [u8]> {
    let Some(q) = d.quote.filter(|_| quoted) else {
        return Cow::Borrowed(if d.trim { trim_units::<U>(raw) } else { raw });
    };
    let w = U::W;
    let full = raw.len() - raw.len() % w;
    let mut k = 0;
    while k < full && U::get(raw, k) != q {
        k += w;
    }
    k += w;
    let body_start = k;
    let mut out: Option<Vec<u8>> = None;
    let mut run_start = k;
    while k < full {
        // Find the next special unit.
        if w == 1 {
            let hay = &raw[k..full];
            let hit = if d.backslash {
                memchr2(q as u8, b'\\', hay)
            } else {
                memchr(q as u8, hay)
            };
            match hit {
                Some(i) => k += i,
                None => break,
            }
        }
        let u = U::get(raw, k);
        if u == q {
            if k + w < full && U::get(raw, k + w) == q {
                // Escaped quote: keep one.
                let o = out.get_or_insert_with(Vec::new);
                o.extend_from_slice(&raw[run_start..k + w]);
                k += 2 * w;
                run_start = k;
                continue;
            }
            // Closing quote; anything after it is appended literally.
            let rest = &raw[k + w..];
            let rest = if d.trim { trim_units::<U>(rest) } else { rest };
            if out.is_none() && rest.is_empty() {
                return Cow::Borrowed(&raw[body_start..k]);
            }
            let mut o = out.unwrap_or_default();
            o.extend_from_slice(&raw[run_start..k]);
            o.extend_from_slice(rest);
            return Cow::Owned(o);
        }
        if d.backslash && u == BACKSLASH && k + w < full {
            let o = out.get_or_insert_with(Vec::new);
            o.extend_from_slice(&raw[run_start..k]);
            o.extend_from_slice(&raw[k + w..k + 2 * w]);
            k += 2 * w;
            run_start = k;
            continue;
        }
        k += w;
    }
    // Unterminated quote: the value runs to the end of the field.
    match out {
        None => Cow::Borrowed(&raw[body_start.min(raw.len())..]),
        Some(mut o) => {
            o.extend_from_slice(&raw[run_start.min(raw.len())..]);
            Cow::Owned(o)
        }
    }
}

/// Decoded, bounded field text for display or matching. At most `max_bytes`
/// of the raw field are looked at; returns the text and whether it was cut.
pub fn field_text(
    d: &Compiled,
    enc: TextEncoding,
    raw: &[u8],
    quoted: bool,
    max_bytes: usize,
) -> (String, bool) {
    let (raw, cut) = clip(raw, enc, max_bytes);
    let value = unquote(d, raw, quoted);
    (enc.decode(&value).into_owned(), cut)
}

/// Clips `raw` to at most `max` bytes without splitting a character.
pub fn clip(raw: &[u8], enc: TextEncoding, max: usize) -> (&[u8], bool) {
    if raw.len() <= max {
        return (raw, false);
    }
    let mut n = max;
    match enc {
        TextEncoding::Utf8 => {
            let mut back = 0;
            while n > 0 && back < 3 && raw[n] & 0xC0 == 0x80 {
                n -= 1;
                back += 1;
            }
        }
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            n &= !1;
            if n >= 2 {
                let unit = if enc == TextEncoding::Utf16Le {
                    u16::from_le_bytes([raw[n - 2], raw[n - 1]])
                } else {
                    u16::from_be_bytes([raw[n - 2], raw[n - 1]])
                };
                if (0xD800..0xDC00).contains(&unit) {
                    n -= 2;
                }
            }
        }
        _ => {}
    }
    (&raw[..n], true)
}

/// Makes control characters visible for single-line display: LF/CR become
/// `↵`, TAB becomes `→`, other C0 controls become their Control Pictures.
pub fn visible(s: &str) -> Cow<'_, str> {
    if !s.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev_cr = false;
    for c in s.chars() {
        match c {
            '\n' if prev_cr => {}
            '\n' | '\r' => out.push('↵'),
            '\t' => out.push('→'),
            '\u{7F}' => out.push('␡'),
            c if (c as u32) < 0x20 => out.push(char::from_u32(0x2400 + c as u32).unwrap_or('?')),
            c => out.push(c),
        }
        prev_cr = c == '\r';
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{Delimiter, Dialect, Escape};
    use crate::scan::scan_all;

    fn values(d: &Dialect, rec: &str) -> Vec<String> {
        let c = d.compile().unwrap();
        let mut bytes = Vec::new();
        d.encoding.encode(rec, &mut bytes);
        split_all(&c, &bytes)
            .iter()
            .map(|f| {
                d.encoding
                    .decode(&unquote(&c, &bytes[f.start..f.end], f.quoted))
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn rfc4180_values() {
        let d = Dialect::default();
        assert_eq!(
            values(&d, "a,\"b,c\",\"d\"\"e\",,\"\""),
            ["a", "b,c", "d\"e", "", ""]
        );
        assert_eq!(values(&d, ""), [""]);
        assert_eq!(values(&d, "a,"), ["a", ""]);
        assert_eq!(values(&d, "\"multi\r\nline\""), ["multi\r\nline"]);
        // Lenient: text after the closing quote is appended.
        assert_eq!(values(&d, "\"ab\"cd,e"), ["abcd", "e"]);
        // Quote in the middle of an unquoted field is literal.
        assert_eq!(values(&d, "ab\"c,d"), ["ab\"c", "d"]);
        // Unterminated quote runs to the end.
        assert_eq!(values(&d, "\"abc,d"), ["abc,d"]);
    }

    #[test]
    fn raw_text_is_untouched_by_default() {
        let d = Dialect::default();
        assert_eq!(values(&d, " 007 , 1.50 ,x "), [" 007 ", " 1.50 ", "x "]);
        let d = Dialect {
            trim: true,
            ..Dialect::default()
        };
        assert_eq!(values(&d, " 007 , \"q \" ,x "), ["007", "q ", "x"]);
    }

    #[test]
    fn backslash_values() {
        let d = Dialect {
            escape: Escape::Backslash,
            ..Dialect::default()
        };
        assert_eq!(
            values(&d, r#""a\"b","c\\d","e""f""#),
            ["a\"b", "c\\d", "e\"f"]
        );
    }

    #[test]
    fn generic_dialect_values() {
        let d = Dialect {
            delimiter: Delimiter::Str("||".into()),
            ..Dialect::default()
        };
        assert_eq!(values(&d, "a||b|c||\"d||e\"||"), ["a", "b|c", "d||e", ""]);
        let d = Dialect {
            delimiter: Delimiter::Char(';'),
            collapse_delimiters: true,
            ..Dialect::default()
        };
        assert_eq!(values(&d, "a;;;b;c"), ["a", "b", "c"]);
        let d = Dialect {
            delimiter: Delimiter::Whitespace,
            ..Dialect::default()
        };
        assert_eq!(values(&d, "  12   abc\t 45  "), ["12", "abc", "45"]);
        assert_eq!(values(&d, "\"a b\"   c"), ["a b", "c"]);
        assert_eq!(values(&d, "   "), [""]);
    }

    #[test]
    fn utf16_values() {
        for enc in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
            let d = Dialect {
                encoding: enc,
                ..Dialect::default()
            };
            assert_eq!(values(&d, "имя,\"a,\"\"b\",z"), ["имя", "a,\"b", "z"]);
        }
    }

    #[test]
    fn split_resumes_at_field_starts() {
        let c = Dialect::default().compile().unwrap();
        let rec = b"a,\"b,c\",d,e";
        let mut out = Vec::new();
        let next = split(&c, rec, 0, 2, &mut out).unwrap();
        assert_eq!(next, 8);
        assert_eq!(split(&c, rec, next, usize::MAX, &mut out), None);
        assert_eq!(out, split_all(&c, rec));
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn field_counts_agree_with_scanner() {
        let dialects = [
            Dialect::default(),
            Dialect {
                trim: true,
                ..Dialect::default()
            },
            Dialect {
                escape: Escape::Backslash,
                ..Dialect::default()
            },
            Dialect {
                delimiter: Delimiter::Str("||".into()),
                ..Dialect::default()
            },
            Dialect {
                delimiter: Delimiter::Char(' '),
                collapse_delimiters: true,
                ..Dialect::default()
            },
            Dialect {
                delimiter: Delimiter::Whitespace,
                ..Dialect::default()
            },
        ];
        let mut seed = 99u64;
        for d in &dialects {
            let c = d.compile().unwrap();
            for _ in 0..500 {
                let input: Vec<u8> = (0..40)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        b"ab,|| \t\"\\\n"[(seed % 10) as usize]
                    })
                    .collect();
                for r in scan_all(&c, &input, 0, usize::MAX) {
                    let content = &input[r.start as usize..r.content_end as usize];
                    let fields = split_all(&c, content);
                    assert_eq!(
                        fields.len() as u64,
                        r.fields,
                        "{d:?} {:?}",
                        String::from_utf8_lossy(content)
                    );
                }
            }
        }
    }

    #[test]
    fn clip_respects_characters() {
        let s = "aé".as_bytes(); // 61 C3 A9
        assert_eq!(clip(s, TextEncoding::Utf8, 2), (&s[..1], true));
        let mut u = Vec::new();
        TextEncoding::Utf16Le.encode("a😀", &mut u); // 2 + 4 bytes
        assert_eq!(clip(&u, TextEncoding::Utf16Le, 4).0.len(), 2);
    }

    #[test]
    fn visible_controls() {
        assert_eq!(visible("a\r\nb\tc\0"), "a↵b→c␀");
        assert!(matches!(visible("plain"), Cow::Borrowed(_)));
    }
}
