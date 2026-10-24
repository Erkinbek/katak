//! Replace All, planned in the background.
//!
//! The job finds every matching cell and computes its new value. Up to a
//! threshold the result is applied as one undoable overlay step; above it
//! the caller offers a streaming rewrite ([`crate::save::write_replaced`])
//! instead, because millions of edits would not fit the overlay's purpose.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::fields::split;
use crate::job::{Job, Notify};
use crate::overlay::RowRef;
use crate::scan::Flow;
use crate::search::{Matcher, SearchOptions, Target};
use crate::visit::visit_records;

/// Replacements kept as undoable edits by default.
pub const DEFAULT_THRESHOLD: usize = 50_000;

/// What the planning job found.
#[derive(Debug, Default)]
pub struct PlanData {
    /// (row, logical column, new value).
    pub edits: Vec<(RowRef, usize, String)>,
    /// More cells would change than the threshold allows.
    pub over: bool,
    pub done: bool,
    pub error: Option<String>,
}

/// A running Replace All plan.
pub struct ReplacePlan {
    pub job: Job,
    pub data: Arc<Mutex<PlanData>>,
    pub matcher: Arc<Matcher>,
    pub with: String,
}

/// Starts planning a Replace All. Fails only for an invalid regex.
pub fn plan(
    target: Target,
    opts: SearchOptions,
    with: String,
    threshold: usize,
    notify: Notify,
) -> Result<ReplacePlan, String> {
    let matcher = Arc::new(Matcher::new(&opts)?);
    let data = Arc::new(Mutex::new(PlanData::default()));
    let (m, out, w) = (matcher.clone(), data.clone(), with.clone());
    let job = Job::spawn("replace", notify, move |ctl| {
        let pre = m.prefilter(target.encoding);
        let forced = target.forced();
        let mut spans = Vec::new();
        let mut edits = Vec::new();
        let mut over = false;
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
                split(&target.dialect, v.content, 0, usize::MAX, &mut spans);
                for (c, text) in target.values(v.span.start, v.content, &spans) {
                    if !m.wants_column(c) || !m.is_match(&text) {
                        continue;
                    }
                    let new = m.replace(&text, &w, true);
                    if new != text {
                        if edits.len() >= threshold {
                            over = true;
                            return Flow::Stop;
                        }
                        edits.push((RowRef::Src(v.span.start), c, new.into_owned()));
                    }
                }
                Flow::Continue
            },
        );
        let mut d = out.lock();
        d.edits = edits;
        d.over = over;
        match res {
            Ok(_) => d.done = true,
            Err(e) => {
                d.error = Some(format!("reading the file failed: {e}"));
                return Err(e.to_string());
            }
        }
        Ok(())
    });
    Ok(ReplacePlan {
        job,
        data,
        matcher,
        with,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::job::no_notify;
    use crate::save::{SaveInput, write_document, write_replaced};
    use crate::scan::Term;
    use crate::source::{AccessMode, Source};

    fn setup(data: &[u8]) -> (tempfile::TempDir, Target, SaveInput) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.csv");
        std::fs::write(&p, data).unwrap();
        let d = Dialect::default();
        let src = Arc::new(Source::open(&p, AccessMode::Auto).unwrap());
        let compiled = Arc::new(d.compile().unwrap());
        let target = Target {
            src: src.clone(),
            dialect: compiled.clone(),
            encoding: d.encoding,
            data_start: 0,
            header_records: 1,
            overlay: None,
        };
        let input = SaveInput {
            src,
            dialect: d,
            compiled,
            data_start: 0,
            overlay: Arc::new(Default::default()),
            line_ending: Term::Lf,
            columns: 2,
        };
        (dir, target, input)
    }

    fn regex(p: &str) -> SearchOptions {
        SearchOptions {
            pattern: p.into(),
            regex: true,
            case_sensitive: true,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn capture_groups_and_threshold() {
        let (_d, target, _) = setup(b"name,date\nAnn,2024-03-01\nBob,1999-12-31\n");
        let p = plan(
            target.clone(),
            regex(r"(\d{4})-(\d{2})-(\d{2})"),
            "$3.$2.$1".into(),
            10,
            no_notify(),
        )
        .unwrap();
        p.job.join();
        let d = p.data.lock();
        assert!(!d.over);
        let values: Vec<&str> = d.edits.iter().map(|e| e.2.as_str()).collect();
        assert_eq!(values, ["01.03.2024", "31.12.1999"]);
        drop(d);
        let p = plan(target, regex(r"\d"), "#".into(), 1, no_notify()).unwrap();
        p.job.join();
        assert!(p.data.lock().over);
    }

    #[test]
    fn named_groups_and_empty_matches() {
        let (_d, target, _) = setup(b"a\nabc\n");
        let p = plan(
            target.clone(),
            regex("(?P<first>a)"),
            "[${first}]".into(),
            10,
            no_notify(),
        )
        .unwrap();
        p.job.join();
        assert_eq!(p.data.lock().edits[0].2, "[a]bc");
        // A pattern that matches the empty string must terminate.
        let p = plan(target, regex("x*"), "-".into(), 10, no_notify()).unwrap();
        p.job.join();
        assert_eq!(p.data.lock().edits[0].2, "-a-b-c-");
    }

    #[test]
    fn streaming_rewrite_quotes_what_needs_it() {
        let src = b"name,note\nAnn,plain\nBob,\"keep, this\"\n";
        let (_d, _t, input) = setup(src);
        let m = Matcher::new(&regex("plain")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let inp = input.clone();
        let job = Job::spawn("rw", no_notify(), move |ctl| {
            let mut out = Vec::new();
            write_replaced(&inp, &m, "now, with a comma\nand a line", &mut out, ctl)
                .map_err(|e| e.to_string())?;
            tx.send(out).unwrap();
            Ok(())
        });
        job.join();
        let out = rx.recv().unwrap();
        assert_eq!(
            out,
            b"name,note\nAnn,\"now, with a comma\nand a line\"\nBob,\"keep, this\"\n"
        );
        // Nothing to replace: byte-identical.
        let m = Matcher::new(&regex("zzz")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let job = Job::spawn("rw", no_notify(), move |ctl| {
            let mut out = Vec::new();
            write_replaced(&input, &m, "x", &mut out, ctl).map_err(|e| e.to_string())?;
            let mut plain = Vec::new();
            write_document(&input, &mut plain, ctl).map_err(|e| e.to_string())?;
            tx.send((out, plain)).unwrap();
            Ok(())
        });
        job.join();
        let (out, plain) = rx.recv().unwrap();
        assert_eq!(out, src);
        assert_eq!(plain, src);
    }
}
