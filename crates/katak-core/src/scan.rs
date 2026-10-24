//! Quote-aware record scanning.
//!
//! A record ends at an unquoted LF, CR or CRLF, or at the end of the input.
//! A quote opens a quoted field only at the start of a field (record start or
//! right after a delimiter, after leading blanks when trimming); anywhere else
//! it is an ordinary character. Inside quotes, a doubled quote is an escaped
//! quote; with backslash escaping, `\x` is also an escape.
//!
//! [`Scanner`] is a resumable state machine: it is fed windows of the source
//! and keeps every piece of state it needs across window boundaries. Bytes in
//! a window before [`Scanner::pos`] are *history*: the fast path looks back
//! into them to decide whether a quote is at a field start.
//!
//! Two implementations produce identical results:
//! * the **fast path** (1-byte units, single-byte delimiter, no collapsing)
//!   only visits quotes and line terminators with `memchr` and counts
//!   delimiters in the unquoted stretches with SIMD byte counting;
//! * the **generic path** walks unit by unit and handles every option
//!   (multi-unit delimiters, collapsed delimiters, whitespace mode, UTF-16).

use memchr::{memchr, memchr2, memchr3};

use crate::dialect::Compiled;
use crate::units::{BACKSLASH, CR, LF, Units, is_blank};
use crate::with_units;

/// Quoted fields larger than this trigger [`Sink::long_quote`] (they are most
/// likely an unterminated quote swallowing the rest of the file).
pub const LONG_FIELD_LIMIT: u64 = 64 << 20;

/// How a record was terminated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Term {
    /// End of input without a terminator.
    #[default]
    None,
    Lf,
    CrLf,
    Cr,
}

/// One record: `[start, content_end)` is its content, `[content_end, end)`
/// its terminator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RecSpan {
    pub start: u64,
    pub content_end: u64,
    pub end: u64,
    /// Number of fields (unquoted delimiters + 1).
    pub fields: u64,
    pub term: Term,
    /// The record contains an escaped quote, a backslash escape or text
    /// after a closing quote: some decoded value is not a contiguous run of
    /// its raw bytes. (Raw-byte prefilters are only exact without this.)
    pub escaped: bool,
}

impl RecSpan {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Whether the scanner should continue after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Receives scanner events.
pub trait Sink {
    fn record(&mut self, rec: &RecSpan) -> Flow;
    /// A comment line `[start, end)` (terminator included).
    fn comment(&mut self, _start: u64, _end: u64) -> Flow {
        Flow::Continue
    }
    /// A quoted field starting at `quote_offset` grew past
    /// [`LONG_FIELD_LIMIT`]. Reported once per field.
    fn long_quote(&mut self, _quote_offset: u64) {}
}

/// Result of [`Scanner::feed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// The window is exhausted; feed a window that contains [`Scanner::pos`].
    NeedMore,
    /// The sink returned [`Flow::Stop`]; [`Scanner::pos`] is the next record start.
    Stopped,
    /// End of input; every record has been emitted.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    RecStart,
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuoted,
    Blanks,
    Comment,
}

/// Resumable quote-aware record scanner.
#[derive(Clone, Debug)]
pub struct Scanner<'d> {
    d: &'d Compiled,
    generic: bool,
    pos: u64,
    rec_start: u64,
    mode: Mode,
    delims: u64,
    prev_delim: bool,
    quote_start: u64,
    long_reported: bool,
    comment_start: u64,
    unterminated: Option<u64>,
    escaped: bool,
    /// Bytes that end an unquoted run on the generic 1-byte path.
    special: [bool; 256],
    /// Mode entered after a record ends.
    after_record: Mode,
}

fn special_bytes(d: &Compiled) -> [bool; 256] {
    let mut t = [false; 256];
    t[b'\n' as usize] = true;
    t[b'\r' as usize] = true;
    if d.whitespace {
        t[b' ' as usize] = true;
        t[b'\t' as usize] = true;
    } else if let Some(&first) = d.delim.first() {
        t[(first & 0xFF) as usize] = true;
    }
    t
}

impl<'d> Scanner<'d> {
    /// A scanner positioned at `start`, which must be a record start.
    pub fn new(d: &'d Compiled, start: u64) -> Self {
        Scanner {
            d,
            generic: !d.fast,
            pos: start,
            rec_start: start,
            mode: Mode::RecStart,
            delims: 0,
            prev_delim: false,
            quote_start: 0,
            long_reported: false,
            comment_start: 0,
            unterminated: None,
            escaped: false,
            special: special_bytes(d),
            after_record: Self::after_record_mode(d, !d.fast),
        }
    }

    /// Without comments nothing needs deciding at a record start: go
    /// straight to the first field.
    fn after_record_mode(d: &Compiled, generic: bool) -> Mode {
        if !d.comment.is_empty() {
            Mode::RecStart
        } else if generic {
            Mode::FieldStart
        } else {
            Mode::Unquoted
        }
    }

    /// Forces the generic path (used to test it against the fast path).
    #[doc(hidden)]
    pub fn force_generic(mut self) -> Self {
        self.generic = true;
        self.after_record = Self::after_record_mode(self.d, true);
        self
    }

    /// Next byte the scanner will examine; windows must contain it.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Start of the record currently being scanned (or of the next one).
    pub fn record_start(&self) -> u64 {
        if self.mode == Mode::RecStart {
            self.pos
        } else {
            self.rec_start
        }
    }

    /// True while inside a quoted field.
    pub fn in_quotes(&self) -> bool {
        self.mode == Mode::Quoted
    }

    /// Offset of the opening quote of the current quoted field.
    pub fn quote_start(&self) -> Option<u64> {
        self.in_quotes().then_some(self.quote_start)
    }

    /// After [`Feed::Done`]: offset of the opening quote if the input ended
    /// inside a quoted field.
    pub fn unterminated_quote(&self) -> Option<u64> {
        self.unterminated
    }

    /// Scans `buf`, whose first byte is at absolute offset `base`. `eof` says
    /// that the input ends at the end of `buf`.
    pub fn feed<S: Sink>(&mut self, buf: &[u8], base: u64, eof: bool, sink: &mut S) -> Feed {
        debug_assert!(self.pos >= base && self.pos <= base + buf.len() as u64);
        if self.generic {
            with_units!(self.d.width, U => self.feed_generic::<U, S>(buf, base, eof, sink))
        } else {
            self.feed_fast(buf, base, eof, sink)
        }
    }

    fn end_record<S: Sink>(
        &mut self,
        content_end: u64,
        end: u64,
        term: Term,
        sink: &mut S,
    ) -> Option<Feed> {
        let rec = RecSpan {
            start: self.rec_start,
            content_end,
            end,
            fields: self.delims + 1,
            term,
            escaped: self.escaped,
        };
        self.mode = self.after_record;
        self.rec_start = end;
        self.delims = 0;
        self.prev_delim = false;
        self.escaped = false;
        if sink.record(&rec) == Flow::Stop {
            self.pos = end;
            return Some(Feed::Stopped);
        }
        None
    }

    fn finish<S: Sink>(&mut self, end: u64, sink: &mut S) -> Feed {
        match self.mode {
            Mode::RecStart => {}
            Mode::Comment => {
                sink.comment(self.comment_start, end);
            }
            // Nothing after the last terminator: no record.
            _ if self.rec_start >= end => {}
            mode => {
                if mode == Mode::Quoted {
                    self.unterminated = Some(self.quote_start);
                }
                let rec = RecSpan {
                    start: self.rec_start,
                    content_end: end,
                    end,
                    fields: self.delims + 1,
                    term: Term::None,
                    escaped: self.escaped,
                };
                sink.record(&rec);
            }
        }
        self.mode = Mode::RecStart;
        self.pos = end;
        self.rec_start = end;
        Feed::Done
    }

    #[inline]
    fn check_long<S: Sink>(&mut self, at: u64, sink: &mut S) {
        if !self.long_reported && at.saturating_sub(self.quote_start) > LONG_FIELD_LIMIT {
            self.long_reported = true;
            sink.long_quote(self.quote_start);
        }
    }

    #[inline]
    fn enter_quotes(&mut self, at: u64) {
        self.mode = Mode::Quoted;
        self.quote_start = at;
        self.long_reported = false;
    }

    // ----------------------------------------------------------------- fast

    /// Decides by looking back whether the quote at `buf[h]` starts a field.
    #[inline]
    fn quote_opens_fast(&self, buf: &[u8], base: u64, h: usize, delim: u8) -> bool {
        if base + h as u64 == self.rec_start {
            return true;
        }
        let mut k = h;
        if self.d.trim {
            while k > 0
                && base + k as u64 > self.rec_start
                && buf[k - 1] != delim
                && (buf[k - 1] == b' ' || buf[k - 1] == b'\t')
            {
                k -= 1;
            }
            if base + k as u64 == self.rec_start {
                return true;
            }
        }
        k > 0 && buf[k - 1] == delim
    }

    fn feed_fast<S: Sink>(&mut self, buf: &[u8], base: u64, eof: bool, sink: &mut S) -> Feed {
        let len = buf.len();
        let delim = self.d.delim_byte();
        let quote = self.d.quote.map(|q| q as u8);
        let mut p = (self.pos - base) as usize;
        loop {
            match self.mode {
                Mode::RecStart => {
                    if p >= len {
                        self.pos = base + len as u64;
                        return if eof {
                            self.finish(base + len as u64, sink)
                        } else {
                            Feed::NeedMore
                        };
                    }
                    let cl = self.d.comment.len();
                    if cl > 0 {
                        if len - p < cl && !eof {
                            self.pos = base + p as u64;
                            return Feed::NeedMore;
                        }
                        if len - p >= cl
                            && buf[p..p + cl]
                                .iter()
                                .zip(&self.d.comment)
                                .all(|(&b, &c)| b as u32 == c)
                        {
                            self.mode = Mode::Comment;
                            self.comment_start = base + p as u64;
                            p += cl;
                            continue;
                        }
                    }
                    self.rec_start = base + p as u64;
                    self.delims = 0;
                    self.mode = Mode::Unquoted;
                }
                Mode::Comment => match memchr2(b'\n', b'\r', &buf[p..]) {
                    None => {
                        self.pos = base + len as u64;
                        return if eof {
                            self.finish(base + len as u64, sink)
                        } else {
                            Feed::NeedMore
                        };
                    }
                    Some(i) => {
                        let h = p + i;
                        let end = if buf[h] == b'\r' {
                            if h + 1 >= len && !eof {
                                self.pos = base + h as u64;
                                return Feed::NeedMore;
                            }
                            if h + 1 < len && buf[h + 1] == b'\n' {
                                h + 2
                            } else {
                                h + 1
                            }
                        } else {
                            h + 1
                        };
                        self.mode = Mode::RecStart;
                        p = end;
                        if sink.comment(self.comment_start, base + end as u64) == Flow::Stop {
                            self.pos = base + end as u64;
                            return Feed::Stopped;
                        }
                    }
                },
                Mode::Unquoted => {
                    // Records are often short: a few scalar steps beat the
                    // setup cost of the SIMD searches.
                    let probe_end = (p + 16).min(len);
                    let qb = quote.unwrap_or(b'\n');
                    let mut k = p;
                    while k < probe_end {
                        let b = buf[k];
                        if b == b'\n' || b == b'\r' || b == qb {
                            break;
                        }
                        self.delims += (b == delim) as u64;
                        k += 1;
                    }
                    let hit = if k < probe_end {
                        Some(k - p)
                    } else {
                        let hay = &buf[k..];
                        let found = match quote {
                            Some(q) => memchr3(q, b'\n', b'\r', hay),
                            None => memchr2(b'\n', b'\r', hay),
                        };
                        let seg_end = found.map_or(len, |i| k + i);
                        self.delims += bytecount::count(&buf[k..seg_end], delim) as u64;
                        found.map(|i| k + i - p)
                    };
                    let Some(i) = hit else {
                        self.pos = base + len as u64;
                        return if eof {
                            self.finish(base + len as u64, sink)
                        } else {
                            Feed::NeedMore
                        };
                    };
                    let h = p + i;
                    match buf[h] {
                        b'\n' => {
                            if let Some(f) = self.end_record(
                                base + h as u64,
                                base + h as u64 + 1,
                                Term::Lf,
                                sink,
                            ) {
                                return f;
                            }
                            p = h + 1;
                        }
                        b'\r' => {
                            if h + 1 >= len && !eof {
                                self.pos = base + h as u64;
                                return Feed::NeedMore;
                            }
                            let (end, term) = if h + 1 < len && buf[h + 1] == b'\n' {
                                (h + 2, Term::CrLf)
                            } else {
                                (h + 1, Term::Cr)
                            };
                            if let Some(f) =
                                self.end_record(base + h as u64, base + end as u64, term, sink)
                            {
                                return f;
                            }
                            p = end;
                        }
                        _ => {
                            if self.quote_opens_fast(buf, base, h, delim) {
                                self.enter_quotes(base + h as u64);
                            }
                            p = h + 1;
                        }
                    }
                }
                Mode::Quoted => {
                    let q = quote.unwrap_or(b'"');
                    let hay = &buf[p..];
                    let hit = if self.d.backslash {
                        memchr2(q, b'\\', hay)
                    } else {
                        memchr(q, hay)
                    };
                    let Some(i) = hit else {
                        self.check_long(base + len as u64, sink);
                        self.pos = base + len as u64;
                        return if eof {
                            self.finish(base + len as u64, sink)
                        } else {
                            Feed::NeedMore
                        };
                    };
                    let h = p + i;
                    if h + 1 >= len && !eof {
                        self.pos = base + h as u64;
                        return Feed::NeedMore;
                    }
                    if buf[h] != q {
                        // Backslash: skip the escaped byte.
                        self.escaped = true;
                        p = (h + 2).min(len);
                        continue;
                    }
                    if h + 1 < len && buf[h + 1] == q {
                        self.escaped = true;
                        p = h + 2;
                        continue;
                    }
                    // Text right after the closing quote is appended to the value.
                    if h + 1 < len && !matches!(buf[h + 1], b'\n' | b'\r') && buf[h + 1] != delim {
                        self.escaped = true;
                    }
                    self.check_long(base + h as u64, sink);
                    self.mode = Mode::Unquoted;
                    p = h + 1;
                }
                Mode::FieldStart | Mode::AfterQuoted | Mode::Blanks => {
                    unreachable!("not used by the fast path")
                }
            }
        }
    }

    // -------------------------------------------------------------- generic

    #[inline]
    fn matches<U: Units>(buf: &[u8], full: usize, p: usize, pat: &[u32]) -> bool {
        p + pat.len() * U::W <= full
            && pat
                .iter()
                .enumerate()
                .all(|(k, &c)| U::get(buf, p + k * U::W) == c)
    }

    fn feed_generic<U: Units, S: Sink>(
        &mut self,
        buf: &[u8],
        base: u64,
        eof: bool,
        sink: &mut S,
    ) -> Feed {
        let w = U::W;
        let len = buf.len();
        // A trailing partial unit (odd byte count in UTF-16) is only
        // processed as part of the final record.
        let full = len - len % w;
        let la = self.d.lookahead_units() * w;
        let quote = self.d.quote;
        let dlen = self.d.delim.len() * w;
        let mut p = (self.pos - base) as usize;
        loop {
            if p >= full {
                self.pos = base + p.min(len) as u64;
                return if eof {
                    self.finish(base + len as u64, sink)
                } else {
                    Feed::NeedMore
                };
            }
            if !eof && p + la > full {
                self.pos = base + p as u64;
                return Feed::NeedMore;
            }
            let u = U::get(buf, p);
            match self.mode {
                Mode::RecStart => {
                    if !self.d.comment.is_empty()
                        && Self::matches::<U>(buf, full, p, &self.d.comment)
                    {
                        self.mode = Mode::Comment;
                        self.comment_start = base + p as u64;
                        p += self.d.comment.len() * w;
                        continue;
                    }
                    self.rec_start = base + p as u64;
                    self.delims = 0;
                    self.prev_delim = false;
                    self.mode = Mode::FieldStart;
                }
                Mode::Comment => {
                    if u == LF || u == CR {
                        let end = if u == CR && p + w < full && U::get(buf, p + w) == LF {
                            p + 2 * w
                        } else {
                            p + w
                        };
                        self.mode = Mode::RecStart;
                        p = end;
                        if sink.comment(self.comment_start, base + end as u64) == Flow::Stop {
                            self.pos = base + end as u64;
                            return Feed::Stopped;
                        }
                    } else {
                        p += w;
                    }
                }
                Mode::FieldStart | Mode::Unquoted | Mode::AfterQuoted if u == LF || u == CR => {
                    let (end, term) = if u == CR {
                        if p + w < full && U::get(buf, p + w) == LF {
                            (p + 2 * w, Term::CrLf)
                        } else {
                            (p + w, Term::Cr)
                        }
                    } else {
                        (p + w, Term::Lf)
                    };
                    if let Some(f) = self.end_record(base + p as u64, base + end as u64, term, sink)
                    {
                        return f;
                    }
                    p = end;
                }
                Mode::FieldStart => {
                    if Some(u) == quote {
                        self.enter_quotes(base + p as u64);
                        self.prev_delim = false;
                        p += w;
                    } else if !self.d.whitespace && Self::matches::<U>(buf, full, p, &self.d.delim)
                    {
                        if !(self.d.collapse && self.prev_delim) {
                            self.delims += 1;
                        }
                        self.prev_delim = true;
                        p += dlen;
                    } else if (self.d.whitespace || self.d.trim) && is_blank(u) {
                        self.prev_delim = false;
                        p += w;
                    } else {
                        self.mode = Mode::Unquoted;
                        self.prev_delim = false;
                        p += w;
                    }
                }
                Mode::Unquoted | Mode::AfterQuoted => {
                    if self.mode == Mode::AfterQuoted {
                        // Text right after the closing quote is appended to
                        // the value; from here on it behaves as unquoted.
                        let separator = if self.d.whitespace {
                            is_blank(u)
                        } else {
                            Self::matches::<U>(buf, full, p, &self.d.delim)
                        };
                        if !separator {
                            self.escaped = true;
                            self.mode = Mode::Unquoted;
                        }
                    }
                    if w == 1 && !self.special[(u & 0xFF) as usize] {
                        // Skip a run of ordinary bytes in one go.
                        let mut q = p + 1;
                        while q < full && !self.special[buf[q] as usize] {
                            q += 1;
                        }
                        p = q;
                        continue;
                    }
                    if self.d.whitespace {
                        if is_blank(u) {
                            self.mode = Mode::Blanks;
                        }
                        p += w;
                    } else if Self::matches::<U>(buf, full, p, &self.d.delim) {
                        self.delims += 1;
                        self.prev_delim = true;
                        self.mode = Mode::FieldStart;
                        p += dlen;
                    } else {
                        p += w;
                    }
                }
                Mode::Blanks => {
                    if is_blank(u) {
                        p += w;
                        while w == 1 && p < full && (buf[p] == b' ' || buf[p] == b'\t') {
                            p += 1;
                        }
                    } else if u == LF || u == CR {
                        // Trailing blanks are not a separator.
                        self.mode = Mode::Unquoted;
                    } else {
                        self.delims += 1;
                        self.mode = Mode::FieldStart;
                    }
                }
                Mode::Quoted => {
                    let q = quote.unwrap_or(b'"' as u32);
                    if w == 1 && !self.d.backslash {
                        // Skip to the next quote byte.
                        match memchr(q as u8, &buf[p..full]) {
                            Some(i) if i > 0 => {
                                p += i;
                                continue;
                            }
                            Some(_) => {}
                            None => {
                                p = full;
                                self.check_long(base + p as u64, sink);
                                continue;
                            }
                        }
                    }
                    if u == q {
                        if p + w < full && U::get(buf, p + w) == q {
                            self.escaped = true;
                            p += 2 * w;
                        } else {
                            self.check_long(base + p as u64, sink);
                            self.mode = Mode::AfterQuoted;
                            p += w;
                        }
                    } else if self.d.backslash && u == BACKSLASH {
                        self.escaped = true;
                        p = (p + 2 * w).min(full);
                    } else {
                        p += w;
                    }
                }
            }
        }
    }
}

/// Sink collecting records into a vector, stopping after `max`.
pub struct Collect<'a> {
    pub out: &'a mut Vec<RecSpan>,
    pub max: usize,
}

impl Sink for Collect<'_> {
    fn record(&mut self, rec: &RecSpan) -> Flow {
        self.out.push(*rec);
        if self.out.len() >= self.max {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// Scans a complete in-memory buffer from `start` (a record start), returning
/// every record. Convenience for tests, detection and previews.
pub fn scan_all(d: &Compiled, buf: &[u8], start: usize, max: usize) -> Vec<RecSpan> {
    let mut out = Vec::new();
    if max == 0 {
        return out;
    }
    let mut sc = Scanner::new(d, start as u64);
    sc.feed(buf, 0, true, &mut Collect { out: &mut out, max });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{Delimiter, Dialect, Escape};
    use crate::encoding::TextEncoding;

    fn records(d: &Dialect, input: &[u8]) -> Vec<(String, u64, Term)> {
        let c = d.compile().unwrap();
        scan_all(&c, input, 0, usize::MAX)
            .into_iter()
            .map(|r| {
                let s = String::from_utf8_lossy(&input[r.start as usize..r.content_end as usize])
                    .into_owned();
                (s, r.fields, r.term)
            })
            .collect()
    }

    fn rec(s: &str, fields: u64, term: Term) -> (String, u64, Term) {
        (s.to_string(), fields, term)
    }

    #[test]
    fn rfc4180_basics() {
        let d = Dialect::default();
        let got = records(
            &d,
            b"a,b,c\r\n1,\"x,y\",3\r\n\"multi\r\nline\",\"q\"\"q\",\r\nlast",
        );
        assert_eq!(
            got,
            vec![
                rec("a,b,c", 3, Term::CrLf),
                rec("1,\"x,y\",3", 3, Term::CrLf),
                rec("\"multi\r\nline\",\"q\"\"q\",", 3, Term::CrLf),
                rec("last", 1, Term::None),
            ]
        );
    }

    #[test]
    fn line_endings_and_empty_lines() {
        let d = Dialect::default();
        let got = records(&d, b"a\nb\rc\r\n\nd");
        assert_eq!(
            got,
            vec![
                rec("a", 1, Term::Lf),
                rec("b", 1, Term::Cr),
                rec("c", 1, Term::CrLf),
                rec("", 1, Term::Lf),
                rec("d", 1, Term::None),
            ]
        );
        assert!(records(&d, b"").is_empty());
        assert_eq!(records(&d, b"\n"), vec![rec("", 1, Term::Lf)]);
    }

    #[test]
    fn quote_only_special_at_field_start() {
        let d = Dialect::default();
        // The quote in `ab"c` is literal, so the newline ends the record.
        let got = records(&d, b"ab\"c,d\ne\"f\n");
        assert_eq!(
            got,
            vec![rec("ab\"c,d", 2, Term::Lf), rec("e\"f", 1, Term::Lf)]
        );
        // After a closing quote the rest of the field is literal.
        let got = records(&d, b"\"a\"x\"y,z\n");
        assert_eq!(got, vec![rec("\"a\"x\"y,z", 2, Term::Lf)]);
    }

    #[test]
    fn trim_allows_blanks_before_quote() {
        let d = Dialect {
            trim: true,
            ..Dialect::default()
        };
        let got = records(&d, b"a,  \"b\nc\",d\n");
        assert_eq!(got, vec![rec("a,  \"b\nc\",d", 3, Term::Lf)]);
        let d = Dialect::default();
        let got = records(&d, b"a,  \"b\nc\",d\n");
        assert_eq!(
            got,
            vec![rec("a,  \"b", 2, Term::Lf), rec("c\",d", 2, Term::Lf)]
        );
    }

    #[test]
    fn backslash_escape() {
        let d = Dialect {
            escape: Escape::Backslash,
            ..Dialect::default()
        };
        let got = records(&d, b"\"a\\\"b\nc\",d\nx\n");
        assert_eq!(
            got,
            vec![rec("\"a\\\"b\nc\",d", 2, Term::Lf), rec("x", 1, Term::Lf)]
        );
    }

    #[test]
    fn no_quote_char() {
        let d = Dialect {
            quote: None,
            ..Dialect::default()
        };
        let got = records(&d, b"\"a\nb\"\n");
        assert_eq!(got, vec![rec("\"a", 1, Term::Lf), rec("b\"", 1, Term::Lf)]);
    }

    #[test]
    fn comments() {
        let d = Dialect {
            comment_prefix: Some("#".into()),
            ..Dialect::default()
        };
        let got = records(&d, b"#c1\na,b\n#c2 \"x\nc,d\n");
        assert_eq!(got, vec![rec("a,b", 2, Term::Lf), rec("c,d", 2, Term::Lf)]);
    }

    #[test]
    fn multi_char_delimiter() {
        let d = Dialect {
            delimiter: Delimiter::Str("||".into()),
            ..Dialect::default()
        };
        let got = records(&d, b"a||b|c||\"d||e\"\n");
        assert_eq!(got, vec![rec("a||b|c||\"d||e\"", 3, Term::Lf)]);
    }

    #[test]
    fn collapse_and_whitespace() {
        let d = Dialect {
            delimiter: Delimiter::Char(' '),
            collapse_delimiters: true,
            ..Dialect::default()
        };
        assert_eq!(
            records(&d, b"a   b  c\n"),
            vec![rec("a   b  c", 3, Term::Lf)]
        );
        let d = Dialect {
            delimiter: Delimiter::Whitespace,
            ..Dialect::default()
        };
        assert_eq!(
            records(&d, b"  12   abc\t 45  \n"),
            vec![rec("  12   abc\t 45  ", 3, Term::Lf)]
        );
        assert_eq!(records(&d, b"   \n"), vec![rec("   ", 1, Term::Lf)]);
        assert_eq!(
            records(&d, b"\"a b\"   c\n"),
            vec![rec("\"a b\"   c", 2, Term::Lf)]
        );
    }

    #[test]
    fn unterminated_quote() {
        let d = Dialect::default().compile().unwrap();
        let input = b"a,b\n\"oops,c\nd,e\n";
        let mut out = Vec::new();
        let mut sc = Scanner::new(&d, 0);
        assert_eq!(
            sc.feed(
                input,
                0,
                true,
                &mut Collect {
                    out: &mut out,
                    max: usize::MAX
                }
            ),
            Feed::Done
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].start, 4);
        assert_eq!(out[1].end, input.len() as u64);
        assert_eq!(sc.unterminated_quote(), Some(4));
    }

    #[test]
    fn utf16() {
        let text = "a,\"b\r\nc\",d\r\ne,f\r\n";
        for enc in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
            let mut bytes = Vec::new();
            enc.encode(text, &mut bytes);
            let d = Dialect {
                encoding: enc,
                ..Dialect::default()
            }
            .compile()
            .unwrap();
            let recs = scan_all(&d, &bytes, 0, usize::MAX);
            assert_eq!(recs.len(), 2);
            assert_eq!(recs[0].fields, 3);
            assert_eq!(recs[0].term, Term::CrLf);
            assert_eq!(recs[1].start, 2 * "a,\"b\r\nc\",d\r\n".len() as u64);
            assert_eq!(recs[1].end, bytes.len() as u64);
        }
    }

    // ------------------------------------------------- differential testing

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_input(rng: &mut Rng, alphabet: &[u8], len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
            .collect()
    }

    /// Feeds `input` in windows split at `splits`, the way `ChunkReader`
    /// does: each window starts with some history before `pos`.
    fn scan_chunked(
        c: &Compiled,
        input: &[u8],
        step: usize,
        generic: bool,
    ) -> (Vec<RecSpan>, Option<u64>) {
        let mut out = Vec::new();
        let mut sc = Scanner::new(c, 0);
        if generic {
            sc = sc.force_generic();
        }
        let mut sink = Collect {
            out: &mut out,
            max: usize::MAX,
        };
        let mut ahead = step;
        loop {
            let pos = sc.pos() as usize;
            let lo = pos.saturating_sub(8);
            let hi = (pos + ahead).min(input.len());
            match sc.feed(&input[lo..hi], lo as u64, hi == input.len(), &mut sink) {
                Feed::Done => break,
                // Like ChunkReader, offer a bigger window when no progress
                // was possible (look-ahead at the window end).
                Feed::NeedMore if sc.pos() as usize == pos => ahead *= 2,
                Feed::NeedMore | Feed::Stopped => ahead = step,
            }
        }
        let u = sc.unterminated_quote();
        (out, u)
    }

    #[test]
    fn fast_and_generic_paths_agree_on_every_split() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
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
                comment_prefix: Some("#".into()),
                ..Dialect::default()
            },
            Dialect {
                quote: None,
                ..Dialect::default()
            },
            Dialect {
                delimiter: Delimiter::Char('\t'),
                trim: true,
                ..Dialect::default()
            },
        ];
        for d in &dialects {
            let c = d.compile().unwrap();
            for _ in 0..400 {
                let len = rng.below(60) as usize;
                let input = random_input(&mut rng, b"ab,,\t \"\"\\\r\n\n#", len);
                let whole = scan_all(&c, &input, 0, usize::MAX);
                for step in [1, 2, 3, 7, 64] {
                    let (fast, uf) = scan_chunked(&c, &input, step, false);
                    let (gen_, ug) = scan_chunked(&c, &input, step, true);
                    assert_eq!(
                        fast,
                        whole,
                        "fast/split {step} {d:?} input {:?}",
                        String::from_utf8_lossy(&input)
                    );
                    assert_eq!(
                        gen_,
                        whole,
                        "generic/split {step} {d:?} input {:?}",
                        String::from_utf8_lossy(&input)
                    );
                    assert_eq!(uf, ug);
                }
            }
        }
    }

    #[test]
    fn generic_dialects_are_split_invariant() {
        let mut rng = Rng(42);
        let dialects = [
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
            Dialect {
                delimiter: Delimiter::Whitespace,
                comment_prefix: Some("//".into()),
                ..Dialect::default()
            },
        ];
        for d in &dialects {
            let c = d.compile().unwrap();
            for _ in 0..400 {
                let len = rng.below(60) as usize;
                let input = random_input(&mut rng, b"ab|| \t\"\r\n/", len);
                let whole = scan_all(&c, &input, 0, usize::MAX);
                for step in [1, 2, 5, 64] {
                    let (got, _) = scan_chunked(&c, &input, step, true);
                    assert_eq!(
                        got,
                        whole,
                        "split {step} {d:?} input {:?}",
                        String::from_utf8_lossy(&input)
                    );
                }
            }
        }
    }

    #[test]
    fn escaped_flag_examples() {
        let flag = |d: &Dialect, s: &[u8]| -> Vec<bool> {
            scan_all(&d.compile().unwrap(), s, 0, usize::MAX)
                .iter()
                .map(|r| r.escaped)
                .collect()
        };
        let d = Dialect::default();
        assert_eq!(flag(&d, b"a,\"b\"\"c\",d\n\"ab\",c\n"), [true, false]);
        assert_eq!(flag(&d, b"\"ab\"cd,e\nx\"y,z\n"), [true, false]);
        let b = Dialect {
            escape: Escape::Backslash,
            ..Dialect::default()
        };
        assert_eq!(flag(&b, b"\"a\\\"b\",c\na\\b\n"), [true, false]);
    }

    #[test]
    fn unescaped_records_contain_their_values_verbatim() {
        // Without the flag, every decoded value is a contiguous run of the
        // record's raw bytes: what raw-byte prefilters rely on.
        let mut rng = Rng(1234);
        for d in [
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
                delimiter: Delimiter::Whitespace,
                ..Dialect::default()
            },
            Dialect {
                delimiter: Delimiter::Str("||".into()),
                ..Dialect::default()
            },
        ] {
            let c = d.compile().unwrap();
            for _ in 0..500 {
                let len = rng.below(50) as usize;
                let input = random_input(&mut rng, b"ab,|| \t\"\\\n", len);
                for r in scan_all(&c, &input, 0, usize::MAX) {
                    if r.escaped {
                        continue;
                    }
                    let content = &input[r.start as usize..r.content_end as usize];
                    for f in crate::fields::split_all(&c, content) {
                        let v = crate::fields::unquote(&c, &content[f.start..f.end], f.quoted);
                        let ok = v.is_empty() || content.windows(v.len()).any(|w| w == &v[..]);
                        assert!(
                            ok,
                            "{d:?}: value {:?} not in {:?}",
                            String::from_utf8_lossy(&v),
                            String::from_utf8_lossy(content)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn matches_csv_crate_on_rfc4180_input() {
        // Well-formed RFC 4180 input (quotes only around whole fields).
        let mut rng = Rng(7);
        let c = Dialect::default().compile().unwrap();
        for _ in 0..300 {
            let mut input = Vec::new();
            let rows = 1 + rng.below(6);
            for _ in 0..rows {
                let fields = 1 + rng.below(4);
                for f in 0..fields {
                    if f > 0 {
                        input.push(b',');
                    }
                    let n = rng.below(5) as usize;
                    if rng.below(2) == 0 {
                        input.push(b'"');
                        for b in random_input(&mut rng, b"ab,\r\n\"", n) {
                            input.push(b);
                            if b == b'"' {
                                input.push(b'"');
                            }
                        }
                        input.push(b'"');
                    } else {
                        input.extend(random_input(&mut rng, b"ab ", n));
                    }
                }
                input.extend_from_slice(if rng.below(2) == 0 { b"\r\n" } else { b"\n" });
            }
            let ours: Vec<u64> = scan_all(&c, &input, 0, usize::MAX)
                .iter()
                .map(|r| r.fields)
                .collect();
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .from_reader(&input[..]);
            let theirs: Vec<u64> = rdr
                .byte_records()
                .map(|r| r.unwrap().len() as u64)
                .collect();
            // The csv crate skips empty lines; ours yields them as one-field
            // records. Generated rows are never empty lines unless a row has
            // a single empty unquoted field.
            let ours_nonempty: Vec<u64> = scan_all(&c, &input, 0, usize::MAX)
                .iter()
                .filter(|r| r.content_end > r.start)
                .map(|r| r.fields)
                .collect();
            assert_eq!(
                ours_nonempty,
                theirs,
                "input {:?} (all ours {ours:?})",
                String::from_utf8_lossy(&input)
            );
        }
    }
}
