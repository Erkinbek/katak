//! Measures Katak's core operations on real files.
//!
//! ```text
//! cargo run --release -p katak-bench -- FILE... [--buffered] [--repeat N]
//!     [--suite] [--save] [--convert]
//! ```
//!
//! For each file: open + first screen (detection, first 60 rows x 40
//! columns decoded), background indexing throughput, random jumps into the
//! indexed region, and private memory. `--suite` adds search and filter,
//! `--save` saving, `--convert` sorting, export and Replace All (the
//! patterns suit the generated big_*.csv).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use katak_core::document::{Document, OpenOptions};
use katak_core::filter::{self, ColumnFilter, FilterOp};
use katak_core::job::no_notify;
use katak_core::search::{self, SearchOptions, Target};
use katak_core::source::AccessMode;
use katak_core::sys::{human_bytes, private_bytes};

struct Args {
    files: Vec<PathBuf>,
    access: AccessMode,
    repeat: usize,
    /// Run the search/filter suite (patterns suit the generated big_*.csv).
    suite: bool,
    /// Measure saving (zero edits and a handful of edits) to a temp file.
    save: bool,
    /// Measure sorting, export and Replace All.
    convert: bool,
    /// Sort memory budget in MB (`--sort-budget`).
    sort_budget_mb: u64,
}

fn parse_args() -> Args {
    let mut args = Args {
        files: Vec::new(),
        access: AccessMode::Auto,
        repeat: 1,
        suite: false,
        save: false,
        convert: false,
        sort_budget_mb: katak_core::sort::DEFAULT_BUDGET >> 20,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--buffered" => args.access = AccessMode::Buffered,
            "--repeat" => args.repeat = it.next().and_then(|n| n.parse().ok()).unwrap_or(1),
            "--suite" => args.suite = true,
            "--save" => args.save = true,
            "--convert" => args.convert = true,
            "--sort-budget" => {
                args.sort_budget_mb = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(args.sort_budget_mb)
            }
            _ => args.files.push(PathBuf::from(a)),
        }
    }
    if args.files.is_empty() {
        eprintln!(
            "usage: katak-bench FILE... [--buffered] [--repeat N] [--suite] [--save] [--convert] [--sort-budget MB]"
        );
        std::process::exit(2);
    }
    args
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mb_per_s(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / (1 << 20) as f64 / d.as_secs_f64().max(1e-9)
}

fn mem() -> String {
    private_bytes()
        .map(human_bytes)
        .unwrap_or_else(|| "n/a".into())
}

fn bench(
    path: &PathBuf,
    access: AccessMode,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let len = std::fs::metadata(path)?.len();
    println!("\n== {} ({})", path.display(), human_bytes(len));
    println!("   private memory before open: {}", mem());

    let t = Instant::now();
    let mut doc = Document::open(
        path,
        OpenOptions {
            access,
            dialect: None,
        },
        no_notify(),
    )?;
    let opened = t.elapsed();
    println!("   private memory right after open: {}", mem());
    let first = doc.first_data_row()?;
    let rows = doc.locator().records_from(first, 60)?.rows;
    let mut cells = 0;
    for (_, r) in &rows {
        cells += doc.cells(r, 0..40, 200)?.len();
    }
    let first_screen = t.elapsed();
    let d = doc.dialect().clone();
    println!(
        "   dialect: {} | header {} | {} | mapped {}",
        d.summary(),
        d.has_header,
        doc.detection().line_endings.label(),
        doc.source().is_mapped()
    );
    println!(
        "   open: {:.1} ms, first screen ({} rows, {cells} cells): {:.1} ms",
        ms(opened),
        rows.len(),
        ms(first_screen)
    );

    let job = doc.index_job().expect("indexing job");
    job.join();
    let took = job.elapsed();
    let st = doc.stats();
    let idx_bytes = st.index_bytes;
    let checkpoints = doc.index().read().checkpoints.len();
    println!(
        "   index: {} rows in {:.0} ms = {:.0} MB/s | {checkpoints} checkpoints, index heap {} ({:.3}% of file)",
        st.rows,
        ms(took),
        mb_per_s(idx_bytes, took),
        human_bytes(st.index_heap as u64),
        st.index_heap as f64 * 100.0 / len.max(1) as f64
    );
    println!(
        "   ragged rows: {} | max fields {} | line endings {} | warnings {:?}",
        st.ragged,
        st.max_fields,
        st.line_endings.label(),
        st.warnings
    );

    println!("   private memory after indexing (before jumps): {}", mem());

    // Random jumps in the indexed region: locate + 60 rows + 40 columns.
    let total = st.rows.max(1);
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut worst = Duration::ZERO;
    let mut sum = Duration::ZERO;
    let jumps = 200;
    for _ in 0..jumps {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let target = seed % total + doc.header_records();
        let t = Instant::now();
        let p = doc.locator().locate_record(target)?;
        let rows = doc.locator().records_from(p, 60)?.rows;
        for (_, r) in &rows {
            doc.cells(r, 0..40, 200)?;
        }
        let e = t.elapsed();
        worst = worst.max(e);
        sum += e;
    }
    println!(
        "   jump to row + render 60x40: avg {:.2} ms, worst {:.2} ms ({jumps} random jumps)",
        ms(sum) / jumps as f64,
        ms(worst)
    );
    println!("   private memory with file open and indexed: {}", mem());
    if args.suite {
        search_suite(&mut doc)?;
    }
    if args.save {
        save_bench(&mut doc, path)?;
    }
    if args.convert {
        convert_bench(&mut doc, path, args.sort_budget_mb << 20)?;
    }
    Ok(())
}

/// Runs `f` as a job, sampling private memory; returns (time, peak).
fn timed_job(
    f: impl FnOnce(&katak_core::job::JobCtl) -> Result<(), String> + Send + 'static,
) -> Result<(Duration, u64), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let job = katak_core::job::Job::spawn("bench", no_notify(), f);
    let mut peak = private_bytes().unwrap_or(0);
    while job.is_running() {
        std::thread::sleep(Duration::from_millis(20));
        peak = peak.max(private_bytes().unwrap_or(0));
    }
    job.join();
    if let Some(e) = job.error() {
        return Err(e.into());
    }
    Ok((start.elapsed(), peak))
}

/// Sorting, export and Replace All on the big_*.csv layout
/// (id,name,email,amount,date,city,notes).
fn convert_bench(
    doc: &mut Document,
    path: &Path,
    budget: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use katak_core::export::{self, ExportInput, ExportOptions, Scope};
    use katak_core::sort::{self, SortKey, SortSpec};
    let len = doc.source().len();
    let rows = doc.stats().rows;
    let mut sorted = None;
    for (name, col, key) in [
        ("amount (numbers)", 3, SortKey::Number),
        ("date (dates)", 4, SortKey::Date),
        ("city (text, few distinct values)", 5, SortKey::Text),
        ("email (text, all distinct)", 2, SortKey::Text),
    ] {
        let spec = SortSpec {
            col,
            key,
            descending: false,
        };
        let t = Instant::now();
        let run = match sort::start(doc.target(), spec, None, budget, rows, no_notify()) {
            Ok(run) => run,
            Err(e) => {
                println!("   sort by {name}: refused: {e}");
                break;
            }
        };
        let mut peak = private_bytes().unwrap_or(0);
        while run.job.is_running() {
            std::thread::sleep(Duration::from_millis(20));
            peak = peak.max(private_bytes().unwrap_or(0));
        }
        run.job.join();
        let took = t.elapsed();
        let n = run.data.lock().rows.len();
        println!(
            "   sort by {name}: {n} rows in {:.0} ms = {:.0} MB/s, peak private memory {}",
            ms(took),
            mb_per_s(len, took),
            human_bytes(peak)
        );
        if col == 3 {
            sorted = Some(run.data.clone());
        }
    }
    let out = path.with_extension("katak-bench-export.tmp.tsv");
    let tsv = ExportOptions {
        dialect: katak_core::dialect::Dialect {
            delimiter: katak_core::dialect::Delimiter::Char('\t'),
            ..katak_core::dialect::Dialect::default()
        },
        policy: katak_core::write::QuotePolicy::Minimal,
        line_ending: katak_core::scan::Term::CrLf,
        bom: false,
        columns: None,
    };
    let mut scopes: Vec<(&str, Scope)> = vec![("whole file to TSV + CRLF", Scope::All)];
    if let Some(l) = sorted {
        scopes.push(("sorted rows (random reads) to TSV", Scope::List(l)));
    }
    for (name, scope) in scopes {
        let input = ExportInput {
            target: doc.target(),
            locator: doc.job_locator(),
            overlay: doc.overlay.snapshot(),
            header: doc.header_record()?.map(|h| h.start),
            columns: 7,
            rows,
        };
        let (o, opts) = (out.clone(), tsv.clone());
        let (took, peak) = timed_job(move |ctl| {
            export::export(input, scope, &opts, &o, ctl)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?;
        println!(
            "   export {name}: {} in {:.0} ms = {:.0} MB/s of input, peak private memory {}",
            human_bytes(std::fs::metadata(&out)?.len()),
            ms(took),
            mb_per_s(len, took),
            human_bytes(peak)
        );
        let _ = std::fs::remove_file(&out);
    }
    // Replace All: a rare value (planned as undoable edits, full scan) and a
    // common one (streaming rewrite of the whole file).
    let opts = |p: &str| SearchOptions {
        pattern: p.into(),
        case_sensitive: true,
        ..SearchOptions::default()
    };
    let t = Instant::now();
    let plan = katak_core::replace::plan(
        doc.target(),
        opts("foxtrot foxtrot 7"),
        "FF7".into(),
        katak_core::replace::DEFAULT_THRESHOLD,
        no_notify(),
    )?;
    plan.job.join();
    let took = t.elapsed();
    println!(
        "   replace-all plan, rare value: {} cells in {:.0} ms = {:.0} MB/s",
        plan.data.lock().edits.len(),
        ms(took),
        mb_per_s(len, took)
    );
    let matcher = katak_core::search::Matcher::new(&opts("Samarkand"))?;
    let input = doc.save_input(7);
    let o = out.clone();
    let (took, peak) = timed_job(move |ctl| {
        katak_core::save::save_with(&o, false, ctl, |w| {
            katak_core::save::write_replaced(&input, &matcher, "Samarqand", w, ctl)
        })
        .map(|r| println!("   (rewrote {} records)", r.records_rewritten))
        .map_err(|e| e.to_string())
    })?;
    println!(
        "   replace-all rewrite, ~10% of rows: {} in {:.0} ms = {:.0} MB/s, peak private memory {}",
        human_bytes(std::fs::metadata(&out)?.len()),
        ms(took),
        mb_per_s(len, took),
        human_bytes(peak)
    );
    let _ = std::fs::remove_file(&out);
    Ok(())
}

/// Saves to a temp file next to the input: once without edits (a plain
/// copy) and once with 5 edited cells, one inserted and one deleted row.
/// Reports throughput and the peak private memory sampled during the save.
fn save_bench(doc: &mut Document, path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let target = path.with_extension("katak-bench-save.tmp.csv");
    let len = doc.source().len();
    for edits in [false, true] {
        if edits {
            let total = doc.stats().rows.max(10);
            let mut seed = 0xDEAD_BEEFu64;
            for k in 0..5 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let hdr = doc.header_records();
                if let Ok(p) = doc.locator().locate_record(seed % total + hdr) {
                    let pos = doc.lpos_of(p);
                    if let Ok((rows, _)) = doc.lrows_from(pos, 1)
                        && let Some(r) = rows.first()
                    {
                        doc.set_values(vec![(r.row_ref(), 1, format!("edited {k}, \"quoted\""))]);
                    }
                }
            }
            let first = doc.first_lpos();
            if let Ok((rows, _)) = doc.lrows_from(first, 3)
                && let Some(span) = rows.get(2).and_then(|r| r.span())
            {
                doc.overlay.insert_rows(span.start, 0, 1);
                doc.overlay.delete_rows(
                    &[],
                    &[katak_core::overlay::Deleted {
                        start: span.start,
                        end: span.end,
                        records: 1,
                    }],
                );
            }
        }
        let input = doc.save_input(8);
        let t = target.clone();
        let start = Instant::now();
        let job = katak_core::job::Job::spawn("save", no_notify(), move |ctl| {
            katak_core::save::save_file(&input, &t, false, ctl)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let mut peak = private_bytes().unwrap_or(0);
        while job.is_running() {
            std::thread::sleep(Duration::from_millis(20));
            peak = peak.max(private_bytes().unwrap_or(0));
        }
        job.join();
        let took = start.elapsed();
        if let Some(e) = job.error() {
            return Err(e.into());
        }
        let out_len = std::fs::metadata(&target)?.len();
        let identical = !edits && files_equal(path, &target)?;
        println!(
            "   save {}: {} in {:.0} ms = {:.0} MB/s, peak private memory {}{}",
            if edits {
                "with 5 edits + 1 inserted + 1 deleted row"
            } else {
                "without edits"
            },
            human_bytes(out_len),
            ms(took),
            mb_per_s(len, took),
            human_bytes(peak),
            if edits {
                String::new()
            } else if identical {
                ", byte-identical".into()
            } else {
                ", NOT IDENTICAL".into()
            }
        );
        let _ = std::fs::remove_file(&target);
    }
    doc.overlay.clear();
    Ok(())
}

fn files_equal(a: &PathBuf, b: &PathBuf) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 8 << 20], vec![0u8; 8 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Search and filter throughput on the generated big_*.csv layout
/// (id,name,email,amount,date,city,notes).
fn search_suite(doc: &mut Document) -> Result<(), Box<dyn std::error::Error>> {
    let target: Target = doc.target();
    let len = doc.source().len();
    let lit = |p: &str| SearchOptions {
        pattern: p.into(),
        case_sensitive: true,
        ..SearchOptions::default()
    };
    let cases: Vec<(&str, SearchOptions)> = vec![
        ("literal, absent", lit("zulu")),
        ("literal, rare", lit("foxtrot foxtrot 7")),
        ("literal, ~10% of rows", lit("Samarkand")),
        (
            "literal, case-insensitive",
            SearchOptions {
                case_sensitive: false,
                ..lit("SAMARKAND")
            },
        ),
        (
            "regex, dates 1999-12-0x",
            SearchOptions {
                regex: true,
                ..lit(r"1999-12-0\d")
            },
        ),
        (
            "regex, whole word",
            SearchOptions {
                regex: true,
                whole_word: true,
                ..lit("juliet")
            },
        ),
        (
            "literal in column 6 only",
            SearchOptions {
                columns: Some(vec![5]),
                ..lit("Samarkand")
            },
        ),
    ];
    for (name, o) in cases {
        let t = Instant::now();
        let s = search::start(target.clone(), o, search::DEFAULT_CAP, no_notify())
            .map_err(|e| e.to_string())?;
        s.job.join();
        let took = t.elapsed();
        let (done, _) = s.job.progress();
        let d = s.data.lock();
        println!(
            "   search {name}: {} hits{} in {:.0} ms = {:.0} MB/s",
            d.hits.len(),
            if d.capped { " (capped)" } else { "" },
            ms(took),
            mb_per_s(if d.capped || !d.done { done } else { len }, took)
        );
    }
    let f = |col: usize, op: FilterOp| ColumnFilter {
        col,
        op,
        case_sensitive: true,
    };
    let filters: Vec<(&str, Vec<ColumnFilter>)> = vec![
        (
            "city equals Samarkand",
            vec![f(5, FilterOp::Equals("Samarkand".into()))],
        ),
        (
            "name contains Lee",
            vec![f(1, FilterOp::Contains("Lee".into()))],
        ),
        (
            "amount between 1000 and 2000",
            vec![f(3, FilterOp::Range(Some(1000.0), Some(2000.0)))],
        ),
        (
            "city = Samarkand AND amount < 500",
            vec![
                f(5, FilterOp::Equals("Samarkand".into())),
                f(3, FilterOp::Range(None, Some(500.0))),
            ],
        ),
    ];
    for (name, fs) in filters {
        let t = Instant::now();
        let r = filter::start(target.clone(), fs, filter::DEFAULT_CAP, no_notify())
            .map_err(|e| e.to_string())?;
        r.job.join();
        let took = t.elapsed();
        let d = r.data.lock();
        println!(
            "   filter {name}: {} rows{} in {:.0} ms = {:.0} MB/s",
            d.matched,
            if d.capped { " (list capped)" } else { "" },
            ms(took),
            mb_per_s(len, took)
        );
    }
    Ok(())
}

fn main() {
    let args = parse_args();
    for _ in 0..args.repeat {
        for f in &args.files {
            if let Err(e) = bench(f, args.access, &args) {
                eprintln!("{}: {e}", f.display());
            }
        }
    }
}
