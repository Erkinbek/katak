//! Streaming record visitor for background jobs (search, filter, …).
//!
//! The visitor runs the quote-aware scanner over the file with bulk reads
//! and hands records to a callback together with their raw content. With a
//! raw-byte [`Prefilter`], records that cannot contain a match are skipped
//! without being split into fields: the prefilter is run lazily over the
//! rest of the current window, and every record that ends before the next
//! possible match is skipped. That is exact because a match inside a decoded
//! value is also a match of its raw bytes, unless the record has escapes
//! ([`RecSpan::escaped`]), which are always handed to the callback.

use std::io;

use memchr::memmem;

use crate::dialect::Compiled;
use crate::job::JobCtl;
use crate::scan::{Feed, Flow, RecSpan, Scanner, Sink};
use crate::source::{ChunkReader, Source};

/// Records larger than this are visited with their first 64 MB only.
pub const MAX_RECORD_BYTES: usize = 64 << 20;
const CHUNK: usize = 16 << 20;

/// A necessary condition on a record's raw bytes for a match.
pub enum Prefilter {
    Literal(Box<memmem::Finder<'static>>),
    Regex(regex::bytes::Regex),
}

impl Prefilter {
    /// A prefilter for a literal byte string.
    pub fn literal(needle: &[u8]) -> Prefilter {
        Prefilter::Literal(Box::new(memmem::Finder::new(needle).into_owned()))
    }

    /// A literal transcoded to the file's encoding: in any stateless
    /// encoding, a value containing the text contains these bytes. `None`
    /// when the text cannot be represented (then nothing can match).
    pub fn transcoded(text: &str, enc: crate::encoding::TextEncoding) -> Option<Prefilter> {
        let mut bytes = Vec::new();
        (enc.encode(text, &mut bytes) == 0 && !bytes.is_empty()).then(|| Prefilter::literal(&bytes))
    }

    /// Start of the first possible match in `hay`.
    pub fn find(&self, hay: &[u8]) -> Option<usize> {
        match self {
            Prefilter::Literal(f) => f.find(hay),
            Prefilter::Regex(r) => r.find(hay).map(|m| m.start()),
        }
    }
}

/// A record handed to the visitor callback.
pub struct Visit<'a> {
    pub span: RecSpan,
    /// Record number (0 = first record, the header if there is one).
    pub record: u64,
    /// Raw content (terminator excluded), clipped to [`MAX_RECORD_BYTES`].
    pub content: &'a [u8],
    pub clipped: bool,
}

/// How a visit ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Records seen (visited or skipped).
    pub records: u64,
    /// Records handed to the callback.
    pub visited: u64,
    /// Reached the end of the file.
    pub completed: bool,
}

struct State {
    records: u64,
    visited: u64,
    clear_until: u64,
    scratch: Vec<u8>,
    stop: bool,
    error: Option<io::Error>,
}

struct VisitSink<'w, 'f, F> {
    win: &'w [u8],
    base: u64,
    src: &'w Source,
    pre: Option<&'w Prefilter>,
    st: &'w mut State,
    f: &'f mut F,
}

impl<F: FnMut(&Visit) -> Flow> Sink for VisitSink<'_, '_, F> {
    fn record(&mut self, r: &RecSpan) -> Flow {
        let n = self.st.records;
        self.st.records += 1;
        let win_end = self.base + self.win.len() as u64;
        let in_window = r.start >= self.base;
        if let (Some(pre), false) = (self.pre, r.escaped) {
            if r.content_end <= self.st.clear_until {
                return Flow::Continue;
            }
            if in_window {
                let from = (r.start - self.base) as usize;
                match pre.find(&self.win[from..]) {
                    Some(i) => {
                        let hit = r.start + i as u64;
                        self.st.clear_until = hit;
                        if hit >= r.content_end {
                            return Flow::Continue;
                        }
                    }
                    None => {
                        // No match starts in the rest of the window, so no
                        // record that ends inside it can match.
                        self.st.clear_until = win_end;
                        if r.content_end <= win_end {
                            return Flow::Continue;
                        }
                    }
                }
            }
        }
        let full = (r.content_end - r.start) as usize;
        let len = full.min(MAX_RECORD_BYTES);
        let content: &[u8] = if in_window && r.start + len as u64 <= win_end {
            let a = (r.start - self.base) as usize;
            &self.win[a..a + len]
        } else {
            // The record started in an earlier window: read it separately.
            self.st.scratch.resize(len, 0);
            if let Err(e) = self.src.read_at(r.start, &mut self.st.scratch) {
                self.st.error = Some(e);
                self.st.stop = true;
                return Flow::Stop;
            }
            &self.st.scratch
        };
        self.st.visited += 1;
        let flow = (self.f)(&Visit {
            span: *r,
            record: n,
            content,
            clipped: len < full,
        });
        if flow == Flow::Stop {
            self.st.stop = true;
        }
        flow
    }
}

/// Visits the records starting at `start` (a record start whose number is
/// `first_record`) in file order, until the end, a `Stop` from `f`, or
/// cancellation. Reports byte progress to `ctl`.
pub fn visit_records<F>(
    src: &Source,
    d: &Compiled,
    start: u64,
    first_record: u64,
    ctl: &JobCtl,
    pre: Option<&Prefilter>,
    mut f: F,
) -> io::Result<Outcome>
where
    F: FnMut(&Visit) -> Flow,
{
    ctl.set_total(src.len().saturating_sub(start));
    let mut st = State {
        records: first_record,
        visited: 0,
        clear_until: 0,
        scratch: Vec::new(),
        stop: false,
        error: None,
    };
    let mut scanner = Scanner::new(d, start);
    let mut reader = ChunkReader::new(src, src.len(), CHUNK);
    loop {
        if ctl.is_cancelled() {
            return Ok(Outcome {
                records: st.records - first_record,
                visited: st.visited,
                completed: false,
            });
        }
        let w = reader.window(scanner.pos())?;
        let mut sink = VisitSink {
            win: w.buf,
            base: w.base,
            src,
            pre,
            st: &mut st,
            f: &mut f,
        };
        let feed = scanner.feed(w.buf, w.base, w.eof, &mut sink);
        if let Some(e) = st.error.take() {
            return Err(e);
        }
        ctl.set_done(scanner.record_start().saturating_sub(start));
        ctl.notify();
        let completed = feed == Feed::Done;
        if completed || st.stop {
            return Ok(Outcome {
                records: st.records - first_record,
                visited: st.visited,
                completed,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::{Job, no_notify};
    use crate::source::AccessMode;
    use std::sync::{Arc, Mutex};

    fn run(data: &[u8], pre: Option<Prefilter>) -> Vec<(u64, Vec<u8>)> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, data).unwrap();
        let src = Arc::new(Source::open(&p, AccessMode::Auto).unwrap());
        let d = Arc::new(Dialect::default().compile().unwrap());
        let out = Arc::new(Mutex::new(Vec::new()));
        let o2 = out.clone();
        let job = Job::spawn("visit", no_notify(), move |ctl| {
            visit_records(&src, &d, 0, 0, ctl, pre.as_ref(), |v| {
                o2.lock().unwrap().push((v.record, v.content.to_vec()));
                Flow::Continue
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        job.join();
        Arc::try_unwrap(out).unwrap().into_inner().unwrap()
    }

    #[test]
    fn visits_every_record_without_prefilter() {
        let got = run(b"a,b\n1,\"x\ny\"\n2,z\n", None);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], (1, b"1,\"x\ny\"".to_vec()));
    }

    #[test]
    fn prefilter_skips_only_records_that_cannot_match() {
        let mut data = b"h1,h2\n".to_vec();
        for i in 0..50_000 {
            if i % 1000 == 7 {
                data.extend_from_slice(format!("{i},needle here\n").as_bytes());
            } else if i % 1000 == 9 {
                // Escaped: always visited, even without the needle.
                data.extend_from_slice(format!("{i},\"q\"\"q\"\n").as_bytes());
            } else {
                data.extend_from_slice(format!("{i},hay\n").as_bytes());
            }
        }
        let got = run(&data, Some(Prefilter::literal(b"needle")));
        let needles = got
            .iter()
            .filter(|(_, c)| c.windows(6).any(|w| w == b"needle"))
            .count();
        assert_eq!(needles, 50);
        assert_eq!(got.len(), 100);
        assert!(
            got.iter()
                .all(|(n, c)| String::from_utf8_lossy(c).starts_with(&(n - 1).to_string()))
        );
    }
}
