//! Parsing options and their compiled, unit-encoded form.

use std::fmt;

use crate::encoding::TextEncoding;
use crate::units::{CR, LF, SPACE, TAB, UnitWidth};

/// How fields are separated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Delimiter {
    /// A single character (`,` `;` `\t` `|` `:` ` ` or any other).
    Char(char),
    /// A multi-character string such as `||`.
    Str(String),
    /// Any run of spaces and tabs; leading and trailing blanks are ignored.
    Whitespace,
    /// No delimiter: every record is one value (with no quote character,
    /// every line — the raw-text view).
    Lines,
}

impl Delimiter {
    /// Short human-readable label (`,`, `Tab`, `Space`, `||`, `Whitespace`,
    /// `None`).
    pub fn label(&self) -> String {
        match self {
            Delimiter::Char('\t') => "Tab".into(),
            Delimiter::Char(' ') => "Space".into(),
            Delimiter::Char(c) => c.to_string(),
            Delimiter::Str(s) => s.clone(),
            Delimiter::Whitespace => "Whitespace".into(),
            Delimiter::Lines => "None".into(),
        }
    }

    /// The literal text inserted between fields when writing (empty for
    /// [`Delimiter::Lines`], which has one field per record).
    pub fn write_str(&self) -> String {
        match self {
            Delimiter::Char(c) => c.to_string(),
            Delimiter::Str(s) => s.clone(),
            Delimiter::Whitespace => " ".into(),
            Delimiter::Lines => String::new(),
        }
    }
}

impl fmt::Display for Delimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// How a quote character is escaped inside a quoted field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Escape {
    /// RFC 4180: `""` inside a quoted field is one quote.
    #[default]
    Doubled,
    /// `\"` escapes the next character inside a quoted field (doubled quotes
    /// are still accepted).
    Backslash,
}

/// All user-visible parsing options.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Dialect {
    pub delimiter: Delimiter,
    /// Treat consecutive delimiters as one.
    pub collapse_delimiters: bool,
    pub quote: Option<char>,
    pub escape: Escape,
    pub has_header: bool,
    /// Raw lines skipped at the start of the data (after the BOM).
    pub skip_lines: u32,
    /// Lines starting with this prefix (at a record start) are not records.
    pub comment_prefix: Option<String>,
    /// Trim blanks around unquoted values and outside quotes.
    pub trim: bool,
    pub encoding: TextEncoding,
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect {
            delimiter: Delimiter::Char(','),
            collapse_delimiters: false,
            quote: Some('"'),
            escape: Escape::Doubled,
            has_header: true,
            skip_lines: 0,
            comment_prefix: None,
            trim: false,
            encoding: TextEncoding::Utf8,
        }
    }
}

/// Invalid parsing options.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DialectError {
    #[error("the delimiter is empty")]
    EmptyDelimiter,
    #[error("the delimiter cannot contain line breaks")]
    DelimiterHasNewline,
    #[error("the quote character cannot be part of the delimiter")]
    QuoteInDelimiter,
    #[error("the quote character must be an ASCII character other than a line break")]
    BadQuote,
    #[error("the comment prefix cannot contain line breaks")]
    CommentHasNewline,
    #[error("delimiters and comment prefixes are limited to {MAX_TOKEN_UNITS} characters")]
    TooLong,
}

/// Longest delimiter / comment prefix, in code units.
pub const MAX_TOKEN_UNITS: usize = 32;

/// A dialect with every structural character encoded into code units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    pub width: UnitWidth,
    /// Delimiter units (empty in whitespace mode).
    pub delim: Vec<u32>,
    pub whitespace: bool,
    pub collapse: bool,
    pub quote: Option<u32>,
    pub backslash: bool,
    /// Comment prefix units (empty = no comments).
    pub comment: Vec<u32>,
    pub trim: bool,
    /// The memchr fast path applies: 1-byte units, single-unit delimiter,
    /// no collapsing.
    pub fast: bool,
}

impl Compiled {
    /// Largest number of units any decision needs to look ahead.
    pub fn lookahead_units(&self) -> usize {
        self.delim.len().max(self.comment.len()).max(2)
    }

    /// The delimiter byte on the fast path.
    #[inline]
    pub(crate) fn delim_byte(&self) -> u8 {
        self.delim[0] as u8
    }
}

impl Dialect {
    /// Validates the options and encodes them into code units.
    pub fn compile(&self) -> Result<Compiled, DialectError> {
        let width = self.encoding.width();
        let units = |s: &str| -> Vec<u32> {
            match width {
                UnitWidth::One => {
                    let mut out = Vec::new();
                    self.encoding.encode(s, &mut out);
                    out.into_iter().map(u32::from).collect()
                }
                UnitWidth::TwoLe | UnitWidth::TwoBe => s.encode_utf16().map(u32::from).collect(),
            }
        };
        let (delim, whitespace) = match &self.delimiter {
            Delimiter::Whitespace => (Vec::new(), true),
            Delimiter::Lines => (Vec::new(), false),
            Delimiter::Char(c) => (units(c.encode_utf8(&mut [0u8; 4])), false),
            Delimiter::Str(s) if s.is_empty() => return Err(DialectError::EmptyDelimiter),
            Delimiter::Str(s) => (units(s), false),
        };
        if delim.iter().any(|&u| u == LF || u == CR) {
            return Err(DialectError::DelimiterHasNewline);
        }
        let quote = match self.quote {
            None => None,
            Some(c) if !c.is_ascii() || c == '\n' || c == '\r' || c == '\0' => {
                return Err(DialectError::BadQuote);
            }
            Some(c) => Some(c as u32),
        };
        if let Some(q) = quote
            && (delim.contains(&q) || (whitespace && (q == SPACE || q == TAB)))
        {
            return Err(DialectError::QuoteInDelimiter);
        }
        let comment = match self.comment_prefix.as_deref() {
            None | Some("") => Vec::new(),
            Some(s) if s.contains(['\n', '\r']) => return Err(DialectError::CommentHasNewline),
            Some(s) => units(s),
        };
        if delim.len() > MAX_TOKEN_UNITS || comment.len() > MAX_TOKEN_UNITS {
            return Err(DialectError::TooLong);
        }
        let collapse = self.collapse_delimiters && !whitespace;
        let fast = width == UnitWidth::One && !whitespace && !collapse && delim.len() == 1;
        Ok(Compiled {
            width,
            delim,
            whitespace,
            collapse,
            quote,
            backslash: self.escape == Escape::Backslash,
            comment,
            trim: self.trim,
            fast,
        })
    }

    /// One-line summary for the status bar.
    pub fn summary(&self) -> String {
        let quote = match self.quote {
            Some(q) => q.to_string(),
            None => "none".into(),
        };
        format!(
            "{} | quote {} | {}",
            self.delimiter.label(),
            quote,
            self.encoding.name()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_default() {
        let c = Dialect::default().compile().unwrap();
        assert_eq!(c.delim, vec![b',' as u32]);
        assert_eq!(c.quote, Some(b'"' as u32));
        assert!(c.fast);
    }

    #[test]
    fn multi_char_and_whitespace_use_generic_path() {
        let d = Dialect {
            delimiter: Delimiter::Str("||".into()),
            ..Dialect::default()
        };
        let c = d.compile().unwrap();
        assert_eq!(c.delim.len(), 2);
        assert!(!c.fast);
        let d = Dialect {
            delimiter: Delimiter::Whitespace,
            ..Dialect::default()
        };
        assert!(d.compile().unwrap().whitespace);
        let d = Dialect {
            collapse_delimiters: true,
            ..Dialect::default()
        };
        assert!(!d.compile().unwrap().fast);
    }

    #[test]
    fn utf16_units() {
        let d = Dialect {
            delimiter: Delimiter::Char('§'),
            encoding: TextEncoding::Utf16Le,
            ..Dialect::default()
        };
        let c = d.compile().unwrap();
        assert_eq!(c.delim, vec![0xA7]);
        assert!(!c.fast);
        // In UTF-8 the same delimiter is two units.
        let d = Dialect {
            delimiter: Delimiter::Char('§'),
            ..Dialect::default()
        };
        assert_eq!(d.compile().unwrap().delim, vec![0xC2, 0xA7]);
    }

    #[test]
    fn rejects_bad_options() {
        let d = Dialect {
            delimiter: Delimiter::Str(String::new()),
            ..Dialect::default()
        };
        assert_eq!(d.compile(), Err(DialectError::EmptyDelimiter));
        let d = Dialect {
            delimiter: Delimiter::Char('"'),
            ..Dialect::default()
        };
        assert_eq!(d.compile(), Err(DialectError::QuoteInDelimiter));
        let d = Dialect {
            delimiter: Delimiter::Char('\n'),
            ..Dialect::default()
        };
        assert_eq!(d.compile(), Err(DialectError::DelimiterHasNewline));
        let d = Dialect {
            quote: Some('«'),
            ..Dialect::default()
        };
        assert_eq!(d.compile(), Err(DialectError::BadQuote));
    }
}
