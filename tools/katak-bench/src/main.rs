//! Measures Katak's core operations on real files.
//!
//! ```text
//! cargo run --release -p katak-bench -- FILE... [--buffered] [--repeat N]
//! ```
//!
//! For each file: open + first screen (detection, first 60 rows x 40
//! columns decoded), background indexing throughput, random jumps into the
//! indexed region, and private memory.

use std::path::PathBuf;
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
}

fn parse_args() -> Args {
    let mut args = Args {
        files: Vec::new(),
        access: AccessMode::Auto,
        repeat: 1,
        suite: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--buffered" => args.access = AccessMode::Buffered,
            "--repeat" => args.repeat = it.next().and_then(|n| n.parse().ok()).unwrap_or(1),
            "--suite" => args.suite = true,
            _ => args.files.push(PathBuf::from(a)),
        }
    }
    if args.files.is_empty() {
        eprintln!("usage: katak-bench FILE... [--buffered] [--repeat N] [--suite]");
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
    suite: bool,
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
    if suite {
        search_suite(&mut doc)?;
    }
    Ok(())
}

/// Search and filter throughput on the generated big_*.csv layout
/// (id,name,email,amount,date,city,notes).
fn search_suite(doc: &mut Document) -> Result<(), Box<dyn std::error::Error>> {
    let target = Target {
        src: doc.source().clone(),
        dialect: doc.compiled().clone(),
        encoding: doc.dialect().encoding,
        data_start: doc.data_start(),
        header_records: doc.header_records(),
    };
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
            if let Err(e) = bench(f, args.access, args.suite) {
                eprintln!("{}: {e}", f.display());
            }
        }
    }
}
