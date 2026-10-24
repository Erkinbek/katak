# Katak

**Katak** (Uzbek for "cell") is a desktop viewer/editor for huge delimited
text files — CSV, TSV or any custom delimiter — of 1 GB, 10 GB, 50 GB and
more. Files open instantly and the window never freezes: the file is
memory-mapped, never read into RAM, and everything proportional to the file
size runs in the background.

* Windows 11 x64 (primary), Linux x64, macOS (arm64 and x64).
* One self-contained binary, no runtime dependencies.
* Written in Rust with [egui](https://github.com/emilk/egui); a custom
  virtualized grid over a sparse, quote-aware index of the memory map.

The design is described in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Status

| Milestone | Scope | State |
|-----------|-------|-------|
| 1 | open, parsing dialog, virtualized grid, scrolling, go to row/offset | **done** (v0.1.0) |
| 2 | search and filter | **done** (v0.2.0) |
| 3 | edit overlay, undo/redo, save with byte-identical round trip | **done** (v0.3.0) |
| 4 | find/replace, sort, export conversions | **done** (v0.4.0) |
| 5 | encodings, raw-text mode, polish | planned |

## Features

* Opens files of any size in milliseconds: only the first 1 MB is read to
  detect the format and show the first screen; indexing continues in the
  background with a progress bar. Until it finishes, row counts are shown
  as estimates (`~12,400,000`) and jumps into the un-indexed part are
  resynchronized heuristically and marked approximate.
* Auto-detection of delimiter, quote character, escape style, header row,
  encoding (BOM, UTF-8, UTF-16 LE/BE, legacy code pages via chardetng),
  line endings and Excel's `sep=;` line.
* Parsing options dialog with a live preview of the first 50 rows:
  comma, semicolon, tab, space, colon, pipe, any custom (multi-character)
  delimiter, "treat consecutive delimiters as one", a whitespace preset,
  quote `"` / `'` / none, doubled or backslash escapes, header yes/no,
  skip first N lines, comment prefix, trim (off by default), encoding.
* Quoted fields may contain delimiters, quotes and line breaks (LF, CR,
  CRLF); quotes are only special at the start of a field.
* Virtualized grid: only visible rows *and* columns are parsed and drawn,
  so files with 10,000 or 1,000,000 columns scroll smoothly. Frozen header,
  optional frozen columns, row numbers, resizable and auto-sized columns,
  numbers right-aligned (column types are inferred from a sample and never
  change the displayed text), hover tooltips, double-click to open a whole
  cell (the first 256 KB of a huge cell).
* Ragged rows are highlighted and counted; F8 jumps to the next one.
* Unterminated quotes are detected and reported with their byte offset,
  with a one-click "parse with quote = none".
* Go to row (Ctrl+G) or byte offset (Ctrl+Shift+G); the vertical scrollbar
  is byte-offset based and works before indexing completes.
* Tabs, drag and drop, command-line arguments (`katak data.csv`, which
  also makes Windows "Open with" work), light/dark theme, zoom, font size,
  monospace font.
* Network and removable drives are read with buffered I/O instead of a
  memory map, so a disconnected share produces an error, not a crash.
* **Search** (Ctrl+F): plain text or regex, match case (with Unicode case
  folding), whole word, whole cell, current column only. Runs in the
  background with a live counter; F3 / Shift+F3 work before it finishes;
  a virtualized results panel (row, column, highlighted preview); matches
  are highlighted in the visible cells. Results are capped at 1,000,000
  with a "more exist" indicator. Non-UTF-8 files (UTF-16, code pages) are
  searched too.
* **Filter** (Ctrl+Shift+L, or "Filter this column" in the header menu):
  contains / equals / starts with / regex / empty / not empty / numeric
  range per column, combined with AND. The grid shows the matching rows
  (with their original row numbers) while the filter is still running,
  with an "N of M rows match" counter; up to 4,000,000 rows are listed.
* **Editing**: double-click, F2 or just start typing; Enter / Tab / arrows
  commit and move, Esc cancels. Large or multi-line values open an editable
  cell viewer. Copy / cut / paste as TSV (Excel-compatible; paste fills a
  rectangle), Delete clears, Ctrl+D fills down. Insert rows above/below,
  delete rows, insert / rename / move / delete columns. Undo/redo for
  everything. Edited cells are marked and inserted rows numbered in green
  until saved; "Discard all edits" and a read-only mode are in the Edit menu.
* Edits are an **overlay** over the unchanged file: nothing is copied, and
  search and filter see the unsaved values.
* **Saving** (Ctrl+S) streams the file through the overlay into a temp file
  next to it, fsyncs it and atomically replaces the original; untouched
  rows are copied byte for byte (a save without edits is byte-identical),
  edited rows keep their original delimiter, quoting of unchanged fields
  and line ending. Optional `.bak` copy; Save As (Ctrl+Shift+S). A failed
  or cancelled save never touches the original, and "file is open in
  another program" / read-only / disk-full errors are explained.
* **Replace** (Ctrl+H): replace in the current cell and move to the next
  match, or Replace all — with regex capture groups (`$1`, `${name}`).
  Up to 50,000 changed cells become one undoable edit; above that Katak
  offers to rewrite the file in one streaming pass (only rows with a match
  are rewritten, a `.bak` is kept by default).
* **Sort** (right-click a column header): ascending or descending, as
  numbers (`.` or `,` decimals), dates or text depending on the column
  type; values that do not parse go last, equal values keep file order.
  The sort runs in the background and is shown as a view — the file is not
  reordered. A filtered view sorts only the matching rows. Memory is
  bounded by a budget (32 bytes per row, 512 MB by default, View → Sort
  memory budget).
* **Export / convert** (File menu, Ctrl+E): all rows, the filtered and/or
  sorted rows ("Save sorted as…" applies the sort order) or the selection
  (optionally only its columns), with another delimiter, quote character,
  quoting policy, escape style, encoding (with or without BOM) or line
  ending. Edits are included; the export streams with bounded memory and
  can be opened in a new tab.

## Building

Install Rust (stable) from <https://rustup.rs>. The repository pins the
stable channel in `rust-toolchain.toml`.

```sh
git clone <repository-url> katak
cd katak
cargo build --release -p katak
```

The binary is `target/release/katak` (`katak.exe` on Windows).

* **Windows**: use the MSVC toolchain (`x86_64-pc-windows-msvc`, the
  default) with the "Desktop development with C++" build tools. The
  resulting `katak.exe` is portable. The optional installer is built with
  [Inno Setup](https://jrsoftware.org/isinfo.php):
  `ISCC.exe /DAppVersion=0.4.0 packaging\windows\katak.iss`.
* **Linux**: install the X11/Wayland headers egui needs, e.g. on Debian or
  Ubuntu `sudo apt install libxcb-render0-dev libxcb-shape0-dev
  libxcb-xfixes0-dev libxkbcommon-dev libgl1-mesa-dev`. File dialogs use
  the XDG desktop portal, so GTK is not needed.
* **macOS**: Xcode command line tools. Build both architectures with
  `--target aarch64-apple-darwin` and `--target x86_64-apple-darwin`.

CI (GitHub Actions) builds and tests on Windows, Linux and macOS on every
push and pull request; pushing a `v*` tag builds release binaries for all
platforms plus the Windows installer and attaches them to a GitHub Release.

## Usage

```sh
katak data.csv other.tsv
```

Or start Katak and use File → Open, or drop files on the window.

### Keyboard shortcuts

| Keys | Action |
|------|--------|
| Ctrl+O | Open files |
| Ctrl+W | Close tab |
| Ctrl+Tab | Next tab |
| Ctrl+Shift+P | Parsing options |
| Ctrl+F | Find |
| Ctrl+H | Replace |
| F3 / Shift+F3 | Next / previous match |
| Ctrl+Shift+L | Filter bar |
| Ctrl+S / Ctrl+Shift+S | Save / Save as |
| Ctrl+E | Export / convert |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| F2, double-click, typing | Edit the cell |
| Enter / Tab / Esc (editing) | Commit and move down / right, cancel |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste (TSV) |
| Delete / Ctrl+D / Ctrl+A | Clear / fill down / select all |
| Ctrl+G | Go to row |
| Ctrl+Shift+G | Go to byte offset |
| F8 | Next ragged row |
| Arrow keys | Move the cursor (Shift extends the selection) |
| Page Up / Page Down | Page up / down |
| Home / End | First / last column |
| Ctrl+Home / Ctrl+End | First / last row |
| Shift+mouse wheel | Scroll horizontally |
| Ctrl+Plus / Ctrl+Minus / Ctrl+0 | Zoom in / out / reset |
| Right-click a cell | Rows, columns, clipboard, filter |
| Right-click a column header | Sort, filter this column, auto-fit, freeze, column type |
| F1 | Shortcut list |

## Performance

Measured on an Intel Core Ultra 9 285 (24 cores), 191 GB RAM, Windows 11,
files in the OS cache, release build. Files come from `tools/katak-gen`.

| Target | Goal | Measured |
|--------|------|----------|
| Startup to first frame with data (10 GB / 50 GB file) | < 300 ms | **198 ms / 231 ms** |
| Open + first screen in the core (any size) | < 300 ms | **2–7 ms** (cached) |
| Parsing-dialog preview update | < 100 ms | **0.03 ms** |
| Background indexing, 1 GB file | ≥ 500 MB/s | **1.66–1.75 GB/s** |
| Background indexing, 10 GB / 50 GB file | ≥ 500 MB/s | **1.56 GB/s (6.6 s) / 1.44 GB/s (35 s)** |
| Indexing, whitespace-aligned (generic path) | ≥ 500 MB/s | **681–690 MB/s** |
| Indexing, UTF-16LE | ≥ 500 MB/s | **810 MB/s** |
| Indexing, 100 million 1-character rows | ≥ 500 MB/s | **655–660 MB/s** |
| Jump to any indexed row + render 60×40 cells | < 50 ms | **avg 0.1–0.7 ms, worst 3.1 ms** |
| UI time per frame, scrolling while indexing runs | < 16 ms | **max 0.37 ms** |
| UI time per frame, scrollbar jumps into the un-indexed part | < 16 ms | **max 5.1 ms** |
| 10,000-column file, horizontal scroll | 60 fps | **max 1.9 ms/frame** |
| 1,000,000-column file, horizontal scroll | 60 fps | **max 0.14 ms/frame** |
| Index memory | < 1 % of file | **0.024–0.048 %** (50 GB file: 16 MB) |
| Private memory, 50 GB file open and indexed | < 300 MB | **24 MB core, 127 MB whole app** |
| Literal search, 1 GB / 10 GB file | ≥ 500 MB/s | **1.27–1.37 GB/s / 0.95–1.0 GB/s** |
| Case-insensitive literal search, 1 GB | ≥ 500 MB/s | **809 MB/s** |
| Regex search, 1 GB / 10 GB | ≥ 200 MB/s | **1.14–1.31 GB/s / 975 MB/s** |
| Filter (literal / numeric range), 1 GB | ≥ 500 MB/s | **1.17–1.28 GB/s / 810 MB/s** |
| Filter (literal / numeric range), 10 GB | ≥ 500 MB/s | **1.17–1.23 GB/s / 771 MB/s** |
| UI frame time with highlighting / filtered view | < 16 ms | **max 4.6 ms / 9.0 ms** |
| Save, zero edits (10 GB) | byte-identical | **identical, 1.54 GB/s** |
| Save with a handful of edits (10 GB): private memory | < 300 MB | **29 MB peak** |
| Save with a handful of edits (10 GB): throughput | disk-bound | **554 MB/s** (the disk was flushing the previous 10 GB write; 1.3 GB/s for 1 GB) |
| Sort 12 M rows (1 GB): numbers / few-valued text / all-distinct text | default 512 MB budget | **1.19 s / 1.59 s / 3.8 s**, 414 MB peak |
| Sort 119 M rows (10 GB), 8 GB budget: numbers / text | bounded memory | **14.1 s / 15.1 s**, 3.6 GB peak (refused at 512 MB with the size needed) |
| UI time per frame in a sorted 12 M-row view | < 16 ms | **max 3.7 ms** |
| Export to TSV + CRLF, 1 GB / 10 GB | streaming | **355–362 MB/s / 356–400 MB/s**, 53 MB peak |
| Export in sort order, 1 GB / 10 GB | no random reads | **7.0 s / 72 s** |
| Replace all as a streaming rewrite (~10 % of rows), 10 GB | streaming | **349–399 MB/s**, 62 MB peak |

Reproduce with:

```sh
cargo run --release -p katak-gen                       # writes testdata/generated/ (~12 GB)
cargo run --release -p katak-bench -- testdata/generated/big_10g.csv --suite --save --convert
target/release/katak testdata/generated/big_10g.csv --perf perf.log --self-test
```

`--self-test` opens the file, scrolls vertically, jumps with the scrollbar,
scrolls horizontally and jumps to random rows for a few seconds, appends
frame-time statistics to the log and exits.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass before every commit. Commits follow Conventional
Commits; each milestone is developed on a `milestone/N-name` branch, merged
into `main` when its performance targets are met and tagged `vX.Y.0`. See
[CHANGELOG.md](CHANGELOG.md).

| Path | Contents |
|------|----------|
| `crates/katak-core` | file access, quote-aware scanner, sparse index, locator, detection, document model (no GUI) |
| `crates/katak` | the egui application |
| `tools/katak-gen` | test-data generator |
| `tools/katak-bench` | throughput and memory benchmarks on real files |
| `docs/ARCHITECTURE.md` | design |
