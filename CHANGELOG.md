# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

## [0.3.0] — 2026-10-24 — Milestone 3: editing and saving

### Added

- Edit overlay: cell edits keyed by record offset, inserted rows, deleted
  record ranges, inserted/renamed/moved/deleted columns, and an undo/redo
  stack of invertible operations. The source file is never copied.
- Logical rows: inserted rows spliced in and deleted ranges skipped in
  both directions, with consecutive logical row numbers.
- In-place cell editor (double-click, F2, typing; Enter/Tab/arrows commit
  and move; Esc cancels), an editable cell viewer for large or multi-line
  values, edited-cell markers and green numbers for inserted rows.
- Copy / cut / paste as Excel-compatible TSV, clear, fill down, select all,
  insert/delete rows, insert/rename/move/delete columns, discard all edits,
  read-only mode, with bounded operation sizes and a confirmation for
  clipboard copies over 100 MB.
- Streaming save: untouched bytes copied verbatim, only edited records
  re-assembled (unchanged fields keep their raw bytes and quoting), temp
  file + fsync + atomic rename, optional `.bak` (hard link), Save As,
  clear error messages, progress and cancellation; unsaved-changes prompts
  when closing a tab or quitting.
- Search and filter see unsaved edits (deleted rows skipped, edited values
  matched, edited records bypass the raw prefilter).
- Checked-in fixtures in `testdata/fixtures` (marked `-text`) and an
  end-to-end round-trip test over all of them; `katak-bench --save`.

### Decisions

- Inserted rows are anchored before a record offset (or at the end), so
  "insert below" means "insert above the next row".
- New values use minimal quoting; when the dialect has no quote character
  and a value needs one, `"` is used and the user is told.
- Characters the file's encoding cannot represent are written as `?` and
  reported after saving.
- The final rename uses `std::fs::rename` (POSIX semantics on Windows),
  which replaces the file while Katak still has it open and mapped.
- Changing parsing options drops unsaved edits (the dialog says so).
- Search does not yet look into inserted rows.

### Measured (Core Ultra 9 285, Windows 11, cached files)

- Save without edits: byte-identical; 1 GB at 1.33 GB/s, 10 GB at
  1.54 GB/s; peak private memory 25–29 MB.
- Save with 5 edits, an inserted and a deleted row: 1 GB at 1.30 GB/s;
  10 GB at 554 MB/s right after another 10 GB write (disk-bound).
- UI frame times unchanged: 10 GB self-test with edits, max 5.6 ms.

## [0.2.0] — 2026-10-24 — Milestone 2: search and filter

### Added

- Search (Ctrl+F): literal or regex, match case with Unicode case folding,
  whole word, whole cell, current column only; background job with live
  counter, F3 / Shift+F3 before it finishes, virtualized results panel,
  highlighting of matches in visible cells, cap of 1,000,000 results with
  a "more exist" indicator.
- Filter (Ctrl+Shift+L or the column header menu): contains, equals,
  starts with, regex, empty, not empty and numeric range per column,
  combined with AND; the grid shows the matching rows live while the
  filter runs ("N of M rows match"), up to 4,000,000 rows.
- Record visitor with an exact raw-byte prefilter: records that cannot
  contain a match are skipped without being split. Records with escapes
  (`""`, `\"`, text after a closing quote) are flagged by the scanner and
  always checked on their decoded values.
- Literal prefilters are transcoded to the file's encoding, so UTF-16 and
  code-page files are searched efficiently; everything else decodes.
- `katak-bench --suite`: search and filter throughput.

### Changed

- Search hits and filtered rows live in chunked vectors: the UI never waits
  for a large reallocation while a job holds the results lock (filtered
  view: max frame 38 ms -> 9 ms, private memory 394 MB -> 201 MB).

### Decisions

- The header row is not searched or filtered.
- A column that a row does not have compares as an empty value.
- Cells larger than 64 MB are searched in their first 64 MB.
- Search ignores an active filter (it always covers the whole file).

### Measured (Core Ultra 9 285, Windows 11, cached files)

- Literal search 1.27–1.37 GB/s (1 GB file), 0.95–1.0 GB/s (10 GB);
  case-insensitive 809 MB/s; regex 1.14–1.31 GB/s (1 GB), 975 MB/s (10 GB).
- Filters: literal 1.17–1.28 GB/s, numeric range 771–810 MB/s.
- UI time per frame with search highlighting max 4.6 ms; filtered view
  (scrolling and jumping while indexing and filtering run) max 9.0 ms.

## [0.1.0] — 2026-10-24 — Milestone 1: open and view

### Added

- Memory-mapped file access with buffered positional reads for network and
  removable drives (and whenever mapping fails).
- Resumable quote-aware record scanner: a `memchr`/SIMD fast path for
  single-byte delimiters and a generic unit-by-unit path for multi-character
  delimiters, collapsed delimiters, the whitespace preset and UTF-16.
- Sparse incremental checkpoint index built by a background job (one
  16-byte checkpoint per ~64 KB), with ragged-row counting, line-ending
  statistics and warnings for unterminated or huge quoted fields.
- Record locator: exact positions in the indexed region, heuristic resync
  (approximate, marked `~`) beyond it, every UI-side call bounded by a
  4 MB scan budget.
- Dialect, encoding and header auto-detection from the first 1 MB.
- Document model with lazily decoded, bounded cells and field checkpoints
  for very wide records.
- egui application: virtualized grid (rows and columns), parsing dialog
  with live preview, tabs, drag and drop, go to row / byte offset, next
  ragged row, warning banner, cell viewer, themes, persisted settings.
- `katak-gen` test-data generator and `katak-bench` benchmark tool;
  `--perf`, `--self-test` and `--screenshot` instrumentation in the app.
- CI on Windows, Linux and macOS; release workflow with binaries for all
  platforms and an Inno Setup installer for Windows.

### Decisions

- A quote opens a quoted field only at the start of a field (like Excel,
  Python's `csv` and `csv-core`); elsewhere it is an ordinary character.
- Lone CR, LF and CRLF all end a record outside quotes. Empty lines are
  shown as rows with one empty field.
- Backslash escaping applies inside quoted fields; doubled quotes are still
  accepted in that mode.
- The parsing dialog's "skip first N lines" skips raw lines (no quote
  processing); comment lines are recognized only at record starts.
- Delimiters and comment prefixes are limited to 32 characters.
- The file is never mapped as a whole: 16 MB views are mapped on demand
  and bulk scans use positional reads (Windows charges page-table entries
  of mapped views to the process, ~2 MB per GB).

### Measured (Core Ultra 9 285, Windows 11, cached files)

- Startup to first frame with data: 198 ms (10 GB file), 231 ms (50 GB).
- Indexing: 1.66–1.75 GB/s (1 GB), 1.56 GB/s (10 GB in 6.6 s), 1.44 GB/s
  (50 GB in 35 s); whitespace preset 681–690 MB/s; UTF-16LE 810 MB/s;
  100 million one-character rows 655–660 MB/s.
- Jump to any indexed row and render 60×40 cells: avg 0.1–0.7 ms.
- UI time per frame: scrolling while indexing max 0.37 ms; scrollbar
  jumps into the un-indexed part max 5.1 ms; 10,000 columns max 1.9 ms;
  1,000,000 columns max 0.14 ms.
- Parsing-dialog preview: 0.03 ms.
- Index memory 0.024–0.048 % of the file; private memory with the 50 GB
  file indexed: 24 MB in the core, 127 MB for the whole app.
