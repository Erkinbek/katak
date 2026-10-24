//! Katak core: everything except the GUI.
//!
//! See `docs/ARCHITECTURE.md` for the design. In short: the file is
//! memory-mapped (or read in chunks), a background job builds a sparse
//! quote-aware checkpoint index, and every UI-side operation is bounded.
