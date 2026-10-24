//! Generates the large and pathological test files Katak is measured with.
//!
//! ```text
//! cargo run --release -p katak-gen -- [--out DIR] [--only a,b,...] [--list]
//! ```
//!
//! Output goes to `testdata/generated/` by default (ignored by Git). Files
//! are deterministic: the same seed always produces the same bytes.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const FIRST: &[&str] = &[
    "Ann", "Bob", "Dilnoza", "Erkin", "Fatima", "Gulnora", "Hasan", "Ivan", "Jamshid", "Kamola",
    "Lena", "Mansur", "Nodira", "Olga", "Pavel", "Rustam", "Sitora", "Timur", "Umida", "Zafar",
];
const LAST: &[&str] = &[
    "Karimov",
    "Smith",
    "Ivanova",
    "Rahimova",
    "O'Neil",
    "Müller",
    "García",
    "Tashkentova",
    "Lee",
    "Nguyen",
];
const CITY: &[&str] = &[
    "Tashkent",
    "Samarkand",
    "Bukhara",
    "New York",
    "São Paulo",
    "Zürich",
    "Kraków",
    "Almaty",
    "Seoul",
    "Lagos",
];
const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima",
];
const CYR: &[&str] = &[
    "Привет",
    "мир",
    "данные",
    "таблица",
    "Ташкент",
    "строка",
    "значение",
    "Ёлка",
    "Щука",
    "Юла",
];

fn out_file(dir: &Path, name: &str) -> io::Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(
        8 << 20,
        File::create(dir.join(name))?,
    ))
}

fn date(rng: &mut Rng, out: &mut Vec<u8>, european: bool) {
    let (y, m, d) = (1990 + rng.below(35), 1 + rng.below(12), 1 + rng.below(28));
    if european {
        write!(out, "{d:02}.{m:02}.{y}").unwrap();
    } else {
        write!(out, "{y}-{m:02}-{d:02}").unwrap();
    }
}

/// One record of the "big" CSV: id,name,email,amount,date,city,notes.
/// Every 20th note is quoted and spans lines; some contain commas and
/// doubled quotes; some use CRLF inside the quotes.
fn big_record(rng: &mut Rng, id: u64, out: &mut Vec<u8>) {
    let (f, l) = (rng.pick(FIRST), rng.pick(LAST));
    write!(
        out,
        "{id},{f} {l},{}.{}@example.com,{}.{:02},",
        f.to_lowercase(),
        id % 977,
        rng.below(100_000),
        rng.below(100)
    )
    .unwrap();
    date(rng, out, false);
    out.push(b',');
    out.extend_from_slice(rng.pick(CITY).as_bytes());
    out.push(b',');
    match rng.below(20) {
        0 => {
            let nl = if rng.below(4) == 0 { "\r\n" } else { "\n" };
            write!(
                out,
                "\"line one, with comma{nl}line \"\"two\"\" {}{nl}line three\"",
                rng.pick(WORDS)
            )
            .unwrap();
        }
        1 => write!(out, "\"{}, {}\"", rng.pick(WORDS), rng.pick(WORDS)).unwrap(),
        2 => {}
        _ => write!(
            out,
            "{} {} {}",
            rng.pick(WORDS),
            rng.pick(WORDS),
            rng.below(1000)
        )
        .unwrap(),
    }
    out.push(b'\n');
}

fn big_csv(dir: &Path, name: &str, target: u64) -> io::Result<u64> {
    let mut w = out_file(dir, name)?;
    let mut rng = Rng(0x5EED_0000 ^ target);
    let mut buf = Vec::with_capacity(1 << 20);
    buf.extend_from_slice(b"id,name,email,amount,date,city,notes\n");
    let (mut written, mut id) = (0u64, 0u64);
    while written < target {
        big_record(&mut rng, id, &mut buf);
        id += 1;
        if buf.len() >= 1 << 20 {
            w.write_all(&buf)?;
            written += buf.len() as u64;
            buf.clear();
        }
    }
    w.write_all(&buf)?;
    w.flush()?;
    Ok(id)
}

fn wide(dir: &Path, cols: usize, rows: usize, name: &str) -> io::Result<u64> {
    let mut w = out_file(dir, name)?;
    let mut rng = Rng(77);
    let mut line = Vec::new();
    for c in 0..cols {
        if c > 0 {
            line.push(b',');
        }
        write!(line, "col{c}").unwrap();
    }
    line.push(b'\n');
    w.write_all(&line)?;
    for r in 0..rows {
        line.clear();
        for c in 0..cols {
            if c > 0 {
                line.push(b',');
            }
            write!(
                line,
                "{}",
                (r as u64 * 31 + c as u64 * 7 + rng.below(10)) % 100_000
            )
            .unwrap();
        }
        line.push(b'\n');
        w.write_all(&line)?;
    }
    w.flush()?;
    Ok(rows as u64)
}

fn space_aligned(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "space_aligned.txt")?;
    let mut rng = Rng(3);
    writeln!(
        w,
        "{:<10}{:<14}{:>12}{:>8}  {:<12}",
        "id", "name", "value", "qty", "city"
    )?;
    let rows = 1_000_000;
    for i in 0..rows {
        let name = format!("{}_{}", rng.pick(WORDS), rng.below(100));
        let value = format!("{}.{:02}", rng.below(1_000_000), rng.below(100));
        writeln!(
            w,
            "{:<10}{:<14}{:>12}{:>8}  {:<12}",
            i,
            name,
            value,
            rng.below(500),
            rng.pick(CITY).replace(' ', "_")
        )?;
    }
    w.flush()?;
    Ok(rows)
}

fn european(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "european.csv")?;
    let mut rng = Rng(4);
    w.write_all(b"id;name;price;qty;date;city\r\n")?;
    let rows = 1_000_000;
    let mut line = Vec::new();
    for i in 0..rows {
        line.clear();
        write!(
            line,
            "{i};{} {};{},{:02};{};",
            rng.pick(FIRST),
            rng.pick(LAST),
            rng.below(10_000),
            rng.below(100),
            rng.below(50)
        )
        .unwrap();
        date(&mut rng, &mut line, true);
        line.push(b';');
        line.extend_from_slice(rng.pick(CITY).as_bytes());
        line.extend_from_slice(b"\r\n");
        w.write_all(&line)?;
    }
    w.flush()?;
    Ok(rows)
}

fn tsv(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "data.tsv")?;
    let mut rng = Rng(5);
    w.write_all(b"id\tword\tnumber\tcity\n")?;
    let rows = 1_000_000;
    for i in 0..rows {
        writeln!(
            w,
            "{i}\t{}\t{}\t{}",
            rng.pick(WORDS),
            rng.below(1_000_000),
            rng.pick(CITY)
        )?;
    }
    w.flush()?;
    Ok(rows)
}

fn cyrillic_rows(rng: &mut Rng, rows: u64) -> String {
    let mut s = String::from("id,имя,город,комментарий\r\n");
    for i in 0..rows {
        s.push_str(&format!(
            "{i},{} {},{},\"{}, {}\"\r\n",
            rng.pick(CYR),
            rng.pick(CYR),
            rng.pick(CYR),
            rng.pick(CYR),
            rng.pick(CYR)
        ));
    }
    s
}

fn utf16le(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "utf16le.csv")?;
    let rows = 200_000;
    let text = cyrillic_rows(&mut Rng(6), rows);
    w.write_all(&[0xFF, 0xFE])?;
    let bytes: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(rows)
}

fn cp1251(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "cp1251.csv")?;
    let rows = 200_000;
    let text = cyrillic_rows(&mut Rng(7), rows);
    let (bytes, _, bad) = encoding_rs_lite_cp1251(&text);
    assert!(!bad);
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(rows)
}

/// Windows-1251 encoder for the characters the generator uses (ASCII plus
/// the Russian alphabet), so the tool has no dependencies.
fn encoding_rs_lite_cp1251(s: &str) -> (Vec<u8>, (), bool) {
    let mut out = Vec::with_capacity(s.len());
    let mut bad = false;
    for c in s.chars() {
        let u = c as u32;
        let b = match u {
            0..=0x7F => u as u8,
            0x410..=0x44F => (u - 0x410 + 0xC0) as u8,
            0x401 => 0xA8,
            0x451 => 0xB8,
            _ => {
                bad = true;
                b'?'
            }
        };
        out.push(b);
    }
    (out, (), bad)
}

fn ragged(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "ragged.csv")?;
    let mut rng = Rng(8);
    w.write_all(b"a,b,c,d\n")?;
    let rows = 1_000_000;
    for i in 0..rows {
        let n = if i % 13 == 0 {
            4 + rng.below(4)
        } else if i % 17 == 0 {
            1 + rng.below(3)
        } else {
            4
        };
        let cells: Vec<String> = (0..n).map(|c| format!("{i}.{c}")).collect();
        writeln!(w, "{}", cells.join(","))?;
    }
    w.flush()?;
    Ok(rows)
}

fn unterminated(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "unterminated_quote.csv")?;
    let mut rng = Rng(9);
    let mut buf = b"id,name,email,amount,date,city,notes\n".to_vec();
    let rows = 500_000;
    for id in 0..rows {
        big_record(&mut rng, id, &mut buf);
    }
    // Near the end: a quote that is never closed, then quote-free rows.
    buf.extend_from_slice(format!("{rows},broken,\"this quote is never closed,1,2\n").as_bytes());
    for id in rows + 1..rows + 50 {
        buf.extend_from_slice(
            format!("{id},tail,row {id},1.00,2024-01-01,Tashkent,plain\n").as_bytes(),
        );
    }
    w.write_all(&buf)?;
    w.flush()?;
    Ok(rows)
}

fn giant_cell(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "giant_cell.csv")?;
    w.write_all(b"id,payload,after\n1,small,x\n2,\"")?;
    let chunk: Vec<u8> = (0..(1 << 20))
        .map(|i| {
            if i % 1000 == 999 {
                b'\n'
            } else {
                b'a' + (i % 26) as u8
            }
        })
        .collect();
    for _ in 0..500 {
        w.write_all(&chunk)?;
    }
    w.write_all(b"\",y\n3,small again,z\n")?;
    w.flush()?;
    Ok(3)
}

fn short_rows(dir: &Path) -> io::Result<u64> {
    let mut w = out_file(dir, "short_rows_100m.csv")?;
    let rows: u64 = 100_000_000;
    let block: Vec<u8> = (0..1_000_000u32)
        .flat_map(|i| [b'a' + (i % 26) as u8, b'\n'])
        .collect();
    for _ in 0..rows / 1_000_000 {
        w.write_all(&block)?;
    }
    w.flush()?;
    Ok(rows)
}

type Generator = fn(&Path) -> io::Result<u64>;

/// Generators that only run when named with `--only` (they are huge).
const OPT_IN: &[&str] = &["big_50g"];

fn generators() -> Vec<(&'static str, &'static str, Generator)> {
    vec![
        (
            "big_1g",
            "big_1g.csv: 1 GB, quoted multi-line fields",
            |d| big_csv(d, "big_1g.csv", GB),
        ),
        (
            "big_10g",
            "big_10g.csv: 10 GB, quoted multi-line fields",
            |d| big_csv(d, "big_10g.csv", 10 * GB),
        ),
        (
            "big_50g",
            "big_50g.csv: 50 GB, quoted multi-line fields (only with --only)",
            |d| big_csv(d, "big_50g.csv", 50 * GB),
        ),
        (
            "wide_10k",
            "wide_10k.csv: 10,000 columns x 2,000 rows",
            |d| wide(d, 10_000, 2_000, "wide_10k.csv"),
        ),
        (
            "million_cols",
            "million_cols.csv: 1,000,000 columns x 3 rows",
            |d| wide(d, 1_000_000, 3, "million_cols.csv"),
        ),
        (
            "space_aligned",
            "space_aligned.txt: runs of spaces, 1M rows",
            space_aligned,
        ),
        (
            "european",
            "european.csv: semicolons, decimal commas, CRLF",
            european,
        ),
        ("tsv", "data.tsv: tab separated, 1M rows", tsv),
        (
            "utf16le",
            "utf16le.csv: UTF-16LE with BOM, Cyrillic",
            utf16le,
        ),
        ("cp1251", "cp1251.csv: Windows-1251 Cyrillic", cp1251),
        ("ragged", "ragged.csv: varying field counts", ragged),
        (
            "unterminated",
            "unterminated_quote.csv: a quote never closed near the end",
            unterminated,
        ),
        (
            "giant_cell",
            "giant_cell.csv: one 500 MB quoted cell",
            giant_cell,
        ),
        (
            "short_rows",
            "short_rows_100m.csv: 100,000,000 one-character rows",
            short_rows,
        ),
    ]
}

fn main() -> io::Result<()> {
    let mut out = PathBuf::from("testdata/generated");
    let mut only: Option<Vec<String>> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(args.next().expect("--out needs a directory")),
            "--only" => {
                only = Some(
                    args.next()
                        .expect("--only needs names")
                        .split(',')
                        .map(str::to_string)
                        .collect(),
                )
            }
            "--list" => {
                for (name, desc, _) in generators() {
                    println!("{name:<14} {desc}");
                }
                return Ok(());
            }
            other => {
                eprintln!("unknown argument {other:?}; use --out DIR, --only a,b or --list");
                std::process::exit(2);
            }
        }
    }
    std::fs::create_dir_all(&out)?;
    for (name, desc, generate) in generators() {
        let selected = match &only {
            Some(o) => o.iter().any(|n| n == name),
            None => !OPT_IN.contains(&name),
        };
        if !selected {
            continue;
        }
        let t = Instant::now();
        let rows = generate(&out)?;
        let secs = t.elapsed().as_secs_f64();
        println!("{desc}  — {rows} rows in {secs:.1} s");
    }
    let total: u64 = std::fs::read_dir(&out)?
        .filter_map(|e| e.ok()?.metadata().ok())
        .map(|m| m.len())
        .sum();
    println!("{} MB in {}", total / MB, out.display());
    Ok(())
}
