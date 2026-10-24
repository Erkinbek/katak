//! Katak core: everything except the GUI.
//!
//! See `docs/ARCHITECTURE.md` for the design. In short: the file is
//! memory-mapped (or read in chunks), a background job builds a sparse
//! quote-aware checkpoint index, and every UI-side operation is bounded.

pub mod dialect;
pub mod encoding;
pub mod fields;
pub mod job;
pub mod scan;
pub mod source;
pub mod units;

pub use dialect::{Compiled, Delimiter, Dialect, DialectError, Escape};
pub use encoding::TextEncoding;
pub use scan::{RecSpan, Term};
pub use source::{AccessMode, Source};
