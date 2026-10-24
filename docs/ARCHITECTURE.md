# Katak — architecture

Katak ("cell" in Uzbek) is a desktop viewer/editor for delimited text files of
any size. This document is the plan the code follows. It is short on purpose:
it fixes the data structures, the threading model and the invariants, and
leaves the details to the code and its tests.

## 1. Constraints that shape everything

* The file is never read into RAM. It is memory-mapped (or read in bounded
  chunks when mapping is unsafe) and only the byte ranges needed are touched.
* Memory is `O(visible cells + index + edits)`. The index is sparse: one
  16-byte checkpoint per ~64 KB of file (0.025 % of the file size).
* The UI thread never waits for I/O-sized work. Everything that is
  proportional to the file size runs on a worker thread with progress and
  cancellation. Work the UI thread does is bounded by a *byte budget* per call;
  when the budget is exhausted it returns a partial answer and the frame shows
  a placeholder.
* Bytes in, bytes out: cells are never reformatted. Saving copies every
  untouched byte verbatim; a save with zero edits is a byte-identical copy.

## 2. Workspace layout

```
crates/katak-core   library, no GUI dependencies (all logic + tests + benches)
crates/katak        the egui/eframe desktop app (binary `katak`)
tools/katak-gen     test-data generator (1 GB/10 GB CSVs, wide, ragged, …)
tools/katak-bench   throughput benchmarks over real files
```

`katak-core` modules:

| module      | responsibility |
|-------------|----------------|
| `source`    | file access: `Mmap` or buffered positional reads (+ block cache), chunked sequential reader |
| `units`     | code-unit abstraction: 1-byte units (UTF-8, single-byte code pages) or 2-byte units (UTF-16 LE/BE) |
| `dialect`   | parsing options (`Dialect`) and their compiled, unit-encoded form |
| `encoding`  | text encodings: BOM, decode/encode, detection |
| `detect`    | dialect auto-detection from the first ~1 MB |
| `scan`      | the quote-aware row-boundary state machine (fast memchr path + generic path) |
| `fields`    | splitting one record into fields; unquoting/unescaping/decoding values |
| `index`     | sparse checkpoint index built incrementally by a background job |
| `locate`    | "rows at / after / before X", resync heuristic, approximate positions |
| `overlay`   | sparse edit overlay + undo/redo |
| `document`  | ties source + dialect + index + overlay together; logical rows |
| `job`       | worker-thread jobs: progress, cancellation, change notification |
| `search`    | matcher (literal/regex, case, whole word/cell, columns) + streaming search |
| `filter`    | per-column predicates, streaming filter producing a bounded row list |
| `sort`      | permutation index over row offsets |
| `write`     | field/record writer: quoting rules, encoders, line endings |
| `save`      | streaming save/export, atomic replace, backups |
| `infer`     | column type inference from a sample (alignment/sort only) |

`katak` (app) modules: `app` (shell, tabs, menus, shortcuts), `tab`
(per-document UI state), `grid` (custom virtualized grid), `parse_dialog`,
`search_ui`, `filter_ui`, `status`, `theme`, `perf` (timing instrumentation and
the scripted UI self-test used to measure frame times).

## 3. File access (`source`)

`Source` is immutable and shared as `Arc<Source>`:

* **Mapped views** (local fixed drives): random access maps 16 MB views
  (overlapping by 8 MB, so any request up to 8 MB fits in one view) on
  demand and keeps 16 of them in an LRU. Slices are handed out as `Bytes`
  handles holding an `Arc` of their view. The file is never mapped as a
  whole: Windows charges the page-table entries of a mapped view to the
  process, about 2 MB per GB — measured: opening a 50 GB file with one
  whole-file view cost 102.7 MB of private memory before reading a byte;
  with on-demand views, 3.1 MB.
* **Buffered**: positional reads (`seek_read` / `read_at`) through a small LRU
  block cache (1 MB blocks). Used when mapping fails, for UNC/network paths and,
  on Windows, for remote or removable drives (`GetDriveTypeW`): a page fault on
  a mapping whose backing store disappears is an access violation, a failed
  `read` is just an error we can show.

Sequential consumers (indexer, search, filter, sort, save) use `ChunkReader`,
a `fill`/`consume` reader that refills a reusable buffer with positional
reads — for mapped files too, because copying from the OS cache is faster
than faulting pages in and does not grow page tables. It keeps the
unconsumed tail plus a small look-behind history so state machines can resume
across chunk boundaries. Matches and fields that span chunk boundaries are
therefore handled by the same code in every mode. UI-side reads use
`ChunkReader::cached`, which borrows mapped views or the block cache.

The source length is fixed at open time. Bytes appended later by another
process are ignored until the user reloads (external changes are detected by
polling size/mtime on a watcher thread).

## 4. Dialect and code units

```rust
struct Dialect {
    delimiter: Delimiter,        // Char(c) | Str("||") | Whitespace
    collapse_delimiters: bool,   // "treat consecutive delimiters as one"
    quote: Option<char>,         // '"', '\'' or none
    escape: Escape,              // Doubled ("") | Backslash (\")
    has_header: bool,
    skip_lines: u32,             // raw lines skipped before the data
    comment_prefix: Option<String>,
    trim: bool,                  // off by default
    encoding: TextEncoding,      // Utf8 | Utf16Le | Utf16Be | Latin1 | other code page
}
```

All structural characters are encoded into the file's *code units* once
(`CompiledDialect`). The scanners are generic over `Units`: `U8` (UTF-8 and
ASCII-compatible code pages; hot loops use `memchr`) and `U16Le`/`U16Be`
(simple unit loops). Rows and fields are always byte ranges into the source.

## 5. Quote-aware row scanning (`scan`)

A record ends at an unquoted LF, CR or CRLF, or at EOF. A quote character
opens a quoted field **only at the start of a field** (record start or right
after a delimiter; after leading blanks when trimming). That is decided by
looking *back* one delimiter-length from the quote, so the fast path only has
to visit quotes and line terminators:

```
outside quotes: memchr3(quote, LF, CR)  → terminator: record ends
                                          quote at field start: enter quotes
                                          quote elsewhere: literal byte
inside quotes:  memchr(quote) (or memchr2 with '\\' for backslash escapes)
                ""  → escaped quote, stay inside
                "x  → leave quotes (lenient: x is appended to the field)
```

Fields per record are counted in the same pass (SIMD byte counting of the
delimiter in the unquoted segments) to detect ragged rows. Multi-character
delimiters, collapsed delimiters and the whitespace preset use the generic
unit-at-a-time path, which is also the reference implementation the fast path
is tested against.

Raw lines at the top (`skip_lines`, an Excel `sep=;` line) and comment lines
(prefix at a record start) are not records. The scanner is a resumable state
machine (`ScanState`: position, record start, quote state, pending CR/quote
look-ahead), so it can stop at any chunk boundary and continue.

**Unterminated quotes.** If a quoted field grows past a sanity limit (64 MB),
the scanner reports a warning with the byte offset of the opening quote. It
keeps going (legitimate 500 MB cells exist), and if EOF is reached inside
quotes the warning becomes "unterminated quote at byte X". The UI shows a
banner suggesting quote = none. Nothing is swallowed silently.

## 6. Sparse index (`index`)

```rust
struct Checkpoint { offset: u64, record: u64 }   // a true record start
struct IndexData {
    checkpoints: Vec<Checkpoint>,  // first record start after every 64 KB
    scanned_to: u64,               // all records before this byte are known
    records: u64,                  // records fully scanned so far
    complete: bool,
    ragged: RaggedStats,           // count + bounded list of offsets
    max_fields: u64, line_endings: LineEndingCounts, warnings: Vec<_>,
}
```

* Built by one background job per document, published in batches into
  `Arc<RwLock<IndexData>>` (UI read locks are a binary search, microseconds).
* Memory: 16 bytes per 64 KB → 50 GB file ≈ 12.5 MB.
* Record *k* is found by binary-searching the last checkpoint ≤ k and
  scanning forward ≤ 64 KB. The offsets of all records of a checkpoint block
  are cached in a small LRU (`locate::BlockCache`) for smooth scrolling.
* If two consecutive checkpoints are one record apart, that record's extent is
  known without scanning (this is what makes a 500 MB cell cheap to display).
* Any parsing-option change discards the index and restarts the job.

## 7. Positions before the index is complete (`locate`)

A position in the file view is a `RowPos { offset, record, exact }`:

* The vertical scrollbar is byte-offset based (`offset / file_len`), so it works
  identically before and after indexing.
* Inside the indexed region positions are exact.
* Jumping past `scanned_to` (scrollbar drag, go-to row, Ctrl+End) **resyncs**:
  from the target byte, try successive line starts and accept the first one
  where the next few lines parse (quotes reset) with the header's field count.
  The record number is estimated from the average record size so far and the
  position is marked approximate (`~12,400,000`).
* When the indexer passes an approximate anchor, the anchor is re-resolved to
  the true record that contains its offset and becomes exact.
* Row-count totals are estimates (`~`) until `complete`.

## 8. Fields and display (`fields`)

`split_record` produces `FieldSpan { raw: Range, quoted, escaped }` for one
record. Values are decoded lazily and bounded: the grid asks for at most N
characters per cell; a field longer than that is shown truncated with its
size, and the full value is opened on demand (paged viewer / copy with
confirmation). Very wide records keep sparse *field checkpoints* (every 128
fields) in the row cache so horizontal scrolling over 10,000+ columns only
parses the visible fields.

Display never alters content; control characters are shown as visible symbols
(↵, →) in the grid only.

## 9. Edit overlay (`overlay`) and how it composes with the index

Identity of a source row is the **byte offset of its record start**. Offsets
are exact even before indexing, never change while the file is open and are
independent of row numbering — so edits do not care about indexing progress.

```rust
enum RowRef { Src(u64 /*record offset*/), New(u32 /*inserted row id*/) }
enum ColRef { Src(u32 /*field index*/),  New(u32 /*inserted column id*/) }

struct Overlay {
    cells:    BTreeMap<u64, RowEdits>,          // Src rows with edited cells
    new_rows: HashMap<u32, RowEdits>,           // content of inserted rows
    inserts:  BTreeMap<u64, Vec<u32>>,          // rows inserted *before* offset (u64::MAX = EOF)
    deleted:  RangeMap<u64, u64>,               // deleted record ranges [start,end) + record counts
    columns:  Option<Vec<ColRef>>,              // None = identity column mapping
    undo: Vec<Op>, redo: Vec<Op>, generation: u64,
}
```

* Logical rows = source records in file order, minus deleted ranges, with
  inserted rows spliced in before their anchor offset.
* Logical row number = source record number + inserted-before − deleted-before
  (exact whenever the source record number is exact).
* Logical column *j* maps to a `ColRef`; source fields past the mapped columns
  (ragged extras) pass through unchanged.
* Every mutation is an invertible `Op` (`SetCells`, `InsertRows`,
  `DeleteRows`, `InsertColumn`, `DeleteColumn`, `MoveColumn`, `Compound`).
  Undo/redo walk the stack; `generation` invalidates caches.
* Changing parsing options would move record boundaries, so it requires the
  overlay to be empty (the UI asks to save or discard first).
* Jobs (search, filter, save) receive an immutable `Arc<OverlaySnapshot>`; they
  merge it with the source by walking the sorted edit offsets alongside their
  scan (O(1) per record).

## 10. Views

The grid shows one of:

* **Natural**: logical rows in file order (positions from `locate`).
* **List**: an explicit list of `(offset, record)` rows — filter results
  (file order, growing while the filter runs) or a sort permutation (sort
  order, shown when the sort has finished). Scrolling is by list index; the
  gutter shows original row numbers. Jumps (go to row/offset, F3) bisect a
  file-order list and scan a permutation. A sort of a filtered view sorts
  the filter's rows; changing the filter re-sorts its new rows.

## 11. Threading model

* **UI thread** (eframe): input, layout, painting; owns `Document` and the
  overlay. Its data access is budgeted (§1).
* **Jobs** (`job`): one OS thread each — indexer, search, filter, sort,
  replace-all, save/export, column statistics. A job owns `Arc`s to the
  immutable source/dialect, a read handle on the index and an overlay snapshot.
  It reports `progress (done/total bytes)`, polls an `AtomicBool` cancel flag at
  least every ~1 MB, publishes results in batches and calls a `notify`
  callback (the app passes `request_repaint`) throttled to ~30 Hz.
* Dropping a `JobHandle` cancels the job; closing a tab or the app cancels all
  of its jobs. A cancelled save deletes its temp file; the original is never
  touched before the final rename.

## 12. Search, filter, replace, sort

* **Matcher**: plain or regex (`regex` crate: linear time, Unicode case
  folding), case sensitivity, whole word (`\b…\b`), whole cell (`^…$`),
  column scope. A *prefilter* is run on raw record bytes when that cannot lose
  matches (no anchors, no escaped quotes in the record, UTF-8/ASCII-compatible
  bytes); candidate records are then split and every cell is matched exactly
  on its decoded value. Non-UTF-8 files: the literal is transcoded for
  fixed-width encodings, otherwise records are decoded.
* **Search** streams through the file on a job; results `(RowRef, column)` are
  appended in file order to a capped list (default 1,000,000, "more exist").
  F3/Shift+F3 use whatever has been found so far. Only visible cells are
  highlighted.
* **Filter**: AND of per-column predicates (contains, equals, starts with,
  regex, empty, not empty, numeric range); produces a capped `RowRef` list
  that the grid shows live ("N of ~M rows match").
* **Replace** (Ctrl+H): "Replace" rewrites the matches in the cursor's cell
  and moves to the next match. **Replace all** runs a planning job (search
  prefilter, exact match per decoded cell, `$1` / `${name}` capture groups
  for regexes) that collects the new values. Up to a threshold (50,000
  cells) they become one undoable overlay step; only cells that already had
  an edit need their source value read (to drop edits that restore it).
  Above the threshold the user is offered a streaming rewrite instead: one
  pass that re-assembles only records with a match and copies everything
  else verbatim, into a temp file that replaces the original atomically
  (not undoable, `.bak` offered, unsaved edits must be saved first).
* **Sort**: one entry per row, `(key, fingerprint, offset, record)` = 32
  bytes, collected in one sequential pass into a vector sized from the row
  count (no doubling) and sorted in parallel. Numbers (`.` or `,` decimals)
  and dates sort by an exact 8-byte key; values that do not parse sort
  last; ties keep file order. Text sorts by its first 8 bytes; a run of
  equal prefixes is grouped by a 64-bit fingerprint of the whole value, one
  value per group is re-read (in file order) and the groups are ordered by
  those values — a column with few distinct long values costs a few reads,
  not one per row. The entries are then turned into the `(offset, record)`
  list in place (the allocation is reused). The memory budget (default
  512 MB, View → Sort memory budget) is checked up front and enforced
  while collecting. The file is never reordered; Export writes the sort
  order ("Save sorted as…").

## 13. Save and export (`save`, `write`)

* Walk the overlay's edit offsets in order. Between edits, copy source bytes
  verbatim in large blocks (zero edits = plain copy, disk-bound). Edited
  records are re-assembled field by field: unchanged fields keep their raw
  bytes (including original quotes), changed fields are encoded with minimal
  quoting, original separators and the record's own terminator are kept.
  Column mapping changes force the per-record path for every record.
* Write to `.<name>.XXXX.katak.tmp` in the same directory, `fsync`, then
  atomically rename over the original with `std::fs::rename`. On Windows
  it uses POSIX rename semantics, which replace the target even while
  Katak still has it open and mapped (the old views keep reading the old
  data); `MoveFileExW` would fail with "access denied". After the rename
  the document is re-read. Optional `.bak`
  is a hard link to the original (copy if links are unsupported). Any failure
  leaves the original untouched and maps OS errors (sharing violation, access
  denied, read-only, disk full) to clear messages.
* **Export** (File → Export / convert, Ctrl+E): delimiter, quote character,
  quote policy (minimal/always), escape style, encoding with optional BOM,
  line endings, header row; rows = all (inserted rows included, deleted
  skipped), the rows in view (filter and/or sort order) or the selection
  (optionally only its columns). Edits are applied. Records without quotes
  or edits whose bytes need no quoting in the output take a fast path that
  only swaps delimiters; everything else is decoded and re-encoded. A
  filter list is read in file order. A dense sort permutation is written
  without random reads: one sequential pass distributes the records into
  rank-range buckets (temporary files next to the output, ~128 MB of source
  each, or memory when one bucket suffices), then each bucket is ordered by
  rank in memory and written; the `(offset, rank)` table costs 16 bytes per
  row while it runs. The same temp-file/fsync/rename procedure as
  Save makes exports atomic; exporting over a file open in Katak is
  refused.

## 14. Memory budget (50 GB file, no edits)

| item                         | size |
|------------------------------|------|
| checkpoints                  | ~12.5 MB (measured: 16 MB capacity) |
| scan buffer of a running job | 16 MB |
| record block cache           | ≤ 256K records (~10 MB) |
| mapped views                 | 16 × 24 MB of address space, page tables only |
| detection sample             | 1 MB |
| egui + GPU buffers           | ~100 MB |
| search/filter result caps    | bounded, configurable |

Mapped pages are file-backed and do not count as private memory; their
page-table entries do, which is why views are small and bulk scans read.
Measured with the 50 GB file indexed: 24 MB private in the core, 127 MB for
the whole app.

## 15. Testing and measurement

* Differential tests: fast scanner vs generic scanner vs the `csv` crate on
  random inputs (quotes, CRLF/LF/CR, empty fields), every chunk split point.
* Index tests: checkpoint lookup + forward scan equals a full naive parse.
* Overlay/undo, save round-trip (byte-identical with zero edits on fixtures
  with CRLF, CR, mixed endings, BOM, UTF-16, no trailing newline), export
  quoting, encoding detection, search/replace (capture groups, empty matches).
* `katak-bench` measures indexing/search/filter/save throughput and memory on
  generated files; the app's `--perf` mode logs first paint and frame times
  while a scripted scroll runs.

## 16. Milestones

1. Open + parsing dialog + virtualized grid + scrolling + go-to over the sparse index.
2. Search + filter.
3. Edit overlay + save with byte-identical round trip.
4. Find/replace, sort, export conversions.
5. Encodings, raw-text mode, polish.
