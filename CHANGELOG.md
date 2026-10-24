# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

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
