# Contributing to Katak

Thanks for helping! Bug reports, ideas, documentation fixes and pull
requests are all welcome.

## Reporting a bug

[Open an issue](https://github.com/Erkinbek/katak/issues/new/choose) with
the Katak version (Help → About), your OS, and what the file looks like:
size, delimiter, quoting, encoding, line endings. Please do not attach
files with private data; a few made-up lines with the same structure are
usually enough to reproduce a parsing problem. A crash, a hang or a frozen
window on any input is always a bug.

## Development setup

Install Rust from <https://rustup.rs>; `rust-toolchain.toml` selects the
stable channel. On Linux, install the GUI headers listed in the
[README](README.md#building-from-source).

```sh
cargo run --release -p katak -- path/to/file.csv
```

`crates/katak-core` holds all file, parsing, index, edit and save logic and
has no GUI dependencies; `crates/katak` is the egui application. Read
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) before larger changes.

## Rules the code keeps

These are what make Katak work on 50 GB files; changes must not break them:

* Never read the whole file into memory. Memory is
  O(visible cells + index + edits).
* The UI thread never blocks: work proportional to the file size runs on a
  worker thread with progress and cancellation.
* Bytes in, bytes out: cell contents are never reformatted, and a save
  without edits is byte-identical.
* Everything that can be large is bounded (result lists, clipboard,
  previews of huge cells).

## Before every commit

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same checks on Windows, Linux and macOS.

## Commits and pull requests

* Branch from `main` and keep pull requests focused on one change.
* Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `perf:`, `docs:`, `test:`, `refactor:`, `chore:`); the
  body explains *why*.
* Performance changes include before/after numbers. Generate test files with
  `cargo run --release -p katak-gen` (several GB, written to the ignored
  `testdata/generated/`) and measure with
  `cargo run --release -p katak-bench -- FILE --suite`.
* Never commit generated test data. Small fixtures go in
  `testdata/fixtures/`; fixtures with CRLF, CR, UTF-16 or binary bytes must
  be marked `-text` in `.gitattributes`.
* Add an entry to [CHANGELOG.md](CHANGELOG.md) for user-visible changes.

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE), and to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).
