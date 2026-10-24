# Task: Build "Katak" — a fast, non-freezing large-CSV viewer/editor

## Goal
Build a desktop app called Katak (Uzbek for "cell") that opens huge delimited text files (CSV / TSV / any custom delimiter) of 1 GB, 10 GB, 50 GB+ instantly, lets the user choose the delimiter and parsing options by hand, shows the data as a clean, readable, scrollable table, and lets them search, filter, edit cells and save — without ever freezing or loading the whole file into memory.

## Target platforms
- Primary: Windows 11 x64 (first-class, tested most).
- Also build and run on Linux x64 and macOS (arm64 + x64) from a single codebase.
- Single self-contained binary, no runtime dependencies (no .NET/JVM/Python on the user's machine).
- Deliver a portable .exe for Windows plus an optional installer.

## Tech stack (recommended — justify if you deviate, but never violate the architecture rules below)
- Rust (stable), 64-bit targets only.
- memmap2 for memory-mapped file access.
- csv-core (streaming, allocation-free CSV state machine) or a custom SIMD-assisted parser; memchr for fast delimiter/newline/quote scanning; regex crate (linear time) for regex search.
- GUI: egui/eframe (alternatives: iced, Slint, or C++/Qt if you can argue it's better). No Electron, no Tauri/webview, no browser-based rendering.
- Do NOT use any generic table/grid widget that iterates all rows or all columns per frame, and do NOT use a DataFrame library (polars, arrow, pandas-style) as the document model — they load the data into RAM. Build a custom virtualized grid over an index into the mmap.

## Non-negotiable architecture rules
1. Never read the entire file into RAM. mmap it and touch only the byte ranges needed. On network/UNC drives or if mmap fails, fall back transparently to buffered chunked reads.
2. The grid is virtualized in BOTH directions: only visible rows AND only visible columns are parsed, laid out and drawn. Files with 10,000+ columns must stay smooth. Memory use is O(visible cells + index + edits), never O(file size).
3. The UI thread never blocks. Indexing, searching, filtering, sorting, replacing, saving, encoding detection all run on worker threads with progress reporting and cancellation. No single UI-thread task over ~16 ms.
4. Row index is sparse, incremental and quote-aware. A CSV row can contain newlines inside quoted fields, so row boundaries cannot be found by scanning for "\n" alone. Build the index in the background with a proper quote-state machine, placing checkpoints (byte offset + row number) only at true row starts, every ~64 KB. Rendering rows N..N+K means: jump to the nearest checkpoint, parse forward. Full per-row offset tables are forbidden for big files; index memory stays under ~1% of file size.
5. Before indexing finishes, the app must still work: the first screen shows immediately (parse only the first ~1 MB), row counts are shown as estimates ("~12,400,000 rows") until confirmed, the scrollbar is byte-offset based, and jumping into an un-indexed region resyncs to the next unquoted newline using a heuristic (a line whose field count matches the header) and marks the position as approximate until the index catches up.
6. Changing any parsing option (delimiter, quote char, header row, etc.) must be instant for the preview and must restart background indexing from scratch, discarding the old index.
7. Edits are an overlay, not a copy. Keep a sparse edit set — (row, column) → new value, inserted rows, deleted rows, inserted/deleted/renamed columns — layered over the immutable mmap'd source. Never materialize the whole table. Undo/redo stack over the overlay.
8. Saving streams through the source, applies the overlay, writes to a temp file in the same directory, fsyncs, then atomically renames over the original. If anything fails, the original is untouched. Untouched rows are written back byte-for-byte: no re-quoting, no re-encoding, no whitespace changes. Save with zero edits must produce a byte-identical file. Offer "Save As" and an optional .bak backup. Windows "file in use"/permission errors give clear messages, never crashes.
9. Never alter cell content on display or on save: no number formatting, no date parsing, no stripping of leading zeros, no trimming — unless the user explicitly turns an option on. Raw text in, raw text out.
10. Everything that can be large is bounded: result lists are capped with a "more exist" indicator, clipboard copies above a threshold (e.g. 100 MB) ask for confirmation, a single giant cell (hundreds of MB) is rendered as a bounded preview with "open full cell" on demand.

## Features — must have

Opening and the parsing dialog
- Open via File menu, drag-and-drop, command-line argument (`katak data.csv`) and Windows "Open with". Multiple tabs.
- On open, auto-detect delimiter, quote char, header presence and encoding by sampling the first ~1 MB (score candidate delimiters by consistent field counts across lines). Show the result in a parsing dialog the user can override at any time (also reachable later via a toolbar button).
- Parsing options, all with a live preview of the first ~50 rows that updates as options change (< 100 ms):
  - Delimiter: comma, semicolon, tab, space, colon, pipe, or a custom string (multi-character allowed, e.g. "||").
  - "Treat consecutive delimiters as one" (essential for space-aligned files) and a "whitespace" preset (any run of spaces/tabs).
  - Quote char: ", ', or none. Escape style: doubled quotes ("") or backslash.
  - Header row: yes/no; skip first N lines; comment-line prefix (e.g. #); Excel's "sep=;" first line handled automatically.
  - Trim whitespace around fields (off by default).
  - Encoding: auto (BOM; UTF-8; UTF-16 LE/BE; heuristic fallback Windows-1252 / Windows-1251 / Latin-1) with manual override and "Reopen with encoding…". Invalid bytes render as U+FFFD, never panic.
  - Line endings: auto-detect CRLF / LF / CR / mixed; shown in the status bar.
- Ragged rows (field count differs from header) must never crash: show the extra cells, highlight the row, count them in the status bar, and offer "jump to next ragged row".
- A "Raw text" toggle that shows the bytes around the current row as plain text, for debugging weird files.

Table view
- Frozen header row, optional frozen first N columns, row numbers, resizable columns, auto-sized column widths from a sample of the first few thousand rows (never from the whole file), ellipsis for long cells, hover tooltip / double-click to expand a cell.
- Smooth 60 fps scrolling in both directions, mouse-wheel, keyboard navigation, Ctrl+Home/End, Page Up/Down.
- Go to row (Ctrl+G) and go to byte offset. Status bar: row (estimated until indexed), column name + index, total rows, total columns, delimiter, quote char, encoding, line endings, file size, ragged-row count, indexing/search progress.
- Column type inference (text / integer / float / date) from a sample, used only for alignment and sort order — never to change content. Per-column override.
- Light/dark theme, zoom, optional monospace font, read-only mode toggle. Detect external file changes and offer reload.

Search, filter, replace
- Search (Ctrl+F): plain text or regex, options: case sensitive, whole word, whole cell, limit to selected column(s). Streams over the mmap in the background, results appear incrementally with a live counter, F3/Shift+F3 work before the full search finishes. Results panel (virtualized): row, column, cell preview; click to jump. Cap results (e.g. 1,000,000) with a "more exist" indicator. Highlight matches only in the visible viewport.
- Filter: per-column quick filter (contains / equals / starts with / regex / empty / not empty / numeric range). Runs in the background over the whole file, producing a bounded list of matching row offsets shown live in the grid with a "N of ~M rows match" counter. Multiple column filters combine with AND. Cancellable.
- Find & Replace (Ctrl+H): replace current / replace all, regex capture groups ($1, ${name}), scope = all columns or selected column(s). Replace All with a small match count (configurable threshold, e.g. 50,000) becomes overlay edits so undo works; above that, run as a streaming pass to a temp file with progress and cancel, warning that undo is unavailable and offering a backup.
- Sort by column (ascending/descending, numeric or lexical by inferred type): background job that builds a permutation index of row offsets. Allowed when rows × 8 bytes fits a configurable memory budget (default 512 MB); above that, show a clear message explaining the limit (external merge-sort to a temp file is a nice-to-have). Sorting never changes the file until the user saves with "apply sort order".
- Non-UTF-8 files must still be searchable (transcode the pattern for fixed-width encodings, otherwise stream-decode in chunks). Correct Unicode case folding.
- In the chunked-read fallback, handle matches that span chunk boundaries. Search must see unsaved overlay edits.

Editing
- Edit a cell (double-click / F2 / start typing), Enter/Tab/arrow navigation while editing, Esc to cancel.
- Insert/delete rows, insert/delete/rename/reorder columns, multi-cell selection, copy/cut/paste as TSV (paste fills a rectangular range), delete selection, fill down, undo/redo.
- Edited cells are visually marked until saved. "Discard all edits" command.
- Save (Ctrl+S) keeps the original delimiter, quoting, encoding and line endings. Save As / Export lets the user change delimiter (comma / semicolon / tab / custom), quote policy (minimal: quote only when needed; always), encoding and line endings; export the whole file, the filtered rows, or the selection to a new file. All exports stream with bounded memory.

## Features — nice to have (only after everything above works)
- Fixed-width column mode (set column boundaries by hand on a preview).
- Column statistics panel (count, empty count, distinct estimate, min/max, numeric sum) computed in the background.
- External merge-sort for files above the sort memory budget.
- Tail/follow mode for growing files, bookmarks, split view, recent files, session restore.

## Performance targets (measure them and report real numbers)
- Open a file of any size and show the first screen of the table in < 300 ms (parse only the first ~1 MB).
- Parsing-dialog preview updates in < 100 ms on every option change.
- Background indexing throughput ≥ 500 MB/s on cached data (quote-aware scan using SIMD byte search).
- Jump to any row in the indexed region in < 50 ms; scrolling at 60 fps with indexing/search running; UI frame time < 16 ms.
- Literal search/filter ≥ 500 MB/s on cached data; simple regex ≥ 200 MB/s.
- A 10,000-column file scrolls horizontally at 60 fps.
- Save of a 10 GB file with a handful of edits: private memory < 300 MB, throughput disk-bound.
- Idle private (non-file-backed) memory with a 50 GB file open and no edits: < 300 MB.
- Startup to first paint < 300 ms.

## Edge cases that must not crash or hang
- Empty file; header only; single column; single row; trailing delimiter on every line; no trailing newline.
- Quoted fields containing delimiters, quotes, CRLF and LF; a quoted field of 500 MB; an unterminated quote near the end of the file (detect when a "field" grows past a sanity limit, e.g. 64 MB, show a warning banner with the byte offset and suggest quote = none; never swallow the rest of the file silently).
- Mixed delimiters, mixed line endings, BOM in the middle, NUL bytes, invalid UTF-8, UTF-16 files, Windows-1251 files.
- Ragged rows, 1 million columns in one row, 100 million one-character rows.
- File being appended by another process; file deleted/moved while open; slow network drive or USB stick; read-only file; Unicode paths; paths > 260 characters on Windows.
- Regex matching the empty string; replace that makes cells contain the delimiter or newlines (must be quoted correctly on save).
- App closed mid-index, mid-search or mid-save → clean cancellation, original file untouched.

## Version control (Git) — mandatory
- Initialize a Git repository before writing the first line of code. Commit `Cargo.lock` (this is a binary, not a library) and a `rust-toolchain.toml` pinning the stable channel.
- `.gitignore`: `/target`, generated test data (e.g. `/testdata/generated/`), `*.bak`, `*.tmp`, editor/IDE folders, `.DS_Store`, `Thumbs.db`. Multi-GB generated test files are never committed — only the generator script is.
- `.gitattributes`: `* text=auto eol=lf` so the repo is LF-normalized on all platforms, AND mark every checked-in test fixture that intentionally contains CRLF, CR, mixed line endings, UTF-16 or binary bytes as `-text` so Git never rewrites them. A fixture mangled by line-ending normalization makes the tests lie.
- Small, atomic commits with Conventional Commits messages (`feat:`, `fix:`, `perf:`, `test:`, `docs:`, `refactor:`, `chore:`). The message body explains why, not just what. Perf-related commits include the before/after numbers.
- `main` must always build and pass tests. Before every commit run `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test`. Never commit broken code; never force-push or rewrite history on `main`.
- One short-lived branch per milestone (`milestone/1-open-and-view`, `milestone/2-search-filter`, …), merged into `main` when that milestone's performance targets are met. Tag each merge `v0.1.0`, `v0.2.0`, … with the measured numbers in the tag message. Keep a `CHANGELOG.md` updated per milestone.
- Before any large refactor or risky change, commit first so it can be reverted in one step.
- GitHub Actions: on every push and pull request, build and test on Windows, Linux and macOS; on every `v*` tag, build release binaries for all three platforms and attach them to a GitHub Release.
- At the end, provide the clone/build commands and a short `git log --oneline` summary showing the milestone history.

## Deliverables
1. A Git repository with complete source and a clean, readable history, with a clear module layout (file access, quote-aware index, parsing options, edit overlay, search/filter/sort, encoding, grid view, app shell).
2. Build instructions for Windows (MSVC toolchain), Linux and macOS, plus the GitHub Actions workflows described above (CI on push/PR, release on tag).
3. A script that generates test files: 1 GB and 10 GB CSVs with quoted multi-line fields; a 10,000-column file; a space-aligned file with runs of spaces; a European semicolon file with decimal commas; a TSV; UTF-16LE and Windows-1251 files; a ragged file; a file with an unterminated quote; a file with one 500 MB cell; a 100-million-short-rows file.
4. Unit tests: parser conformance (RFC 4180 plus every option variant), quote-aware index correctness, overlay + undo/redo, save round-trip byte-identical with zero edits, export quoting rules, encoding detection, search/replace (capture groups, empty matches). Benchmarks for indexing, search, filter and save throughput.
5. README with usage, keyboard shortcuts, the measured performance numbers, and a `CHANGELOG.md`.

## Process
- Start with a short written plan (modules, data structures, threading model, how the edit overlay composes with the index). Commit the plan as `docs/ARCHITECTURE.md` in the first commit. Get the architecture right before writing any UI code.
- Build in runnable milestones, each on its own branch and merged + tagged when done: (1) open + parsing dialog + virtualized grid + scroll + go-to over the quote-aware sparse index; (2) search + filter; (3) edit overlay + save with byte-identical round-trip; (4) find/replace, sort, export conversions; (5) encodings, raw-text mode, polish.
- After each milestone, run the generated test files and report real numbers against the targets. If a target is missed, fix it before merging.
- Ask only when something is genuinely ambiguous; otherwise pick a sensible default and note it in the commit message or CHANGELOG.
- Prefer boring, proven solutions over clever ones. Correctness and responsiveness beat feature count.