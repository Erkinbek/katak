//! Text encodings: BOM handling, detection, lossy decoding and encoding.
//!
//! Decoding never fails: invalid sequences become U+FFFD. Encoding reports
//! characters that the target encoding cannot represent (they are written as
//! `?`), so the UI can warn before saving.

use std::borrow::Cow;
use std::fmt;

use encoding_rs::{EncoderResult, Encoding};

use crate::units::UnitWidth;

/// A file's text encoding.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
    /// ISO-8859-1: every byte is the code point of the same value.
    Latin1,
    /// Any other ASCII-compatible encoding of the WHATWG encoding standard
    /// (windows-1252, windows-1251, KOI8-R, Shift_JIS, …).
    Other(&'static Encoding),
}

impl fmt::Debug for TextEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Display for TextEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Encodings offered in the UI, in display order.
pub const CHOICES: &[TextEncoding] = &[
    TextEncoding::Utf8,
    TextEncoding::Utf16Le,
    TextEncoding::Utf16Be,
    TextEncoding::Other(encoding_rs::WINDOWS_1252),
    TextEncoding::Other(encoding_rs::WINDOWS_1251),
    TextEncoding::Latin1,
    TextEncoding::Other(encoding_rs::WINDOWS_1250),
    TextEncoding::Other(encoding_rs::WINDOWS_1253),
    TextEncoding::Other(encoding_rs::WINDOWS_1254),
    TextEncoding::Other(encoding_rs::WINDOWS_1255),
    TextEncoding::Other(encoding_rs::WINDOWS_1256),
    TextEncoding::Other(encoding_rs::WINDOWS_1257),
    TextEncoding::Other(encoding_rs::WINDOWS_1258),
    TextEncoding::Other(encoding_rs::WINDOWS_874),
    TextEncoding::Other(encoding_rs::ISO_8859_2),
    TextEncoding::Other(encoding_rs::ISO_8859_5),
    TextEncoding::Other(encoding_rs::ISO_8859_15),
    TextEncoding::Other(encoding_rs::KOI8_R),
    TextEncoding::Other(encoding_rs::KOI8_U),
    TextEncoding::Other(encoding_rs::IBM866),
    TextEncoding::Other(encoding_rs::MACINTOSH),
    TextEncoding::Other(encoding_rs::SHIFT_JIS),
    TextEncoding::Other(encoding_rs::EUC_JP),
    TextEncoding::Other(encoding_rs::EUC_KR),
    TextEncoding::Other(encoding_rs::GBK),
    TextEncoding::Other(encoding_rs::GB18030),
    TextEncoding::Other(encoding_rs::BIG5),
];

impl TextEncoding {
    /// Display name, e.g. `UTF-8`, `windows-1251`.
    pub fn name(&self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf16Le => "UTF-16LE",
            TextEncoding::Utf16Be => "UTF-16BE",
            TextEncoding::Latin1 => "ISO-8859-1",
            TextEncoding::Other(e) => e.name(),
        }
    }

    /// Looks an encoding up by a label such as `utf-8`, `cp1251`, `latin1`.
    pub fn from_label(label: &str) -> Option<TextEncoding> {
        let l = label.trim().to_ascii_lowercase();
        match l.as_str() {
            "latin1" | "latin-1" | "iso-8859-1" | "iso8859-1" | "l1" => {
                return Some(TextEncoding::Latin1);
            }
            "cp1251" => return Some(TextEncoding::Other(encoding_rs::WINDOWS_1251)),
            "cp1252" => return Some(TextEncoding::Other(encoding_rs::WINDOWS_1252)),
            _ => {}
        }
        let enc = Encoding::for_label_no_replacement(l.as_bytes())?;
        Some(TextEncoding::from_encoding_rs(enc))
    }

    /// Maps an `encoding_rs` encoding, falling back to windows-1252 for
    /// encodings that are not ASCII compatible (other than UTF-16).
    pub fn from_encoding_rs(enc: &'static Encoding) -> TextEncoding {
        if enc == encoding_rs::UTF_8 {
            TextEncoding::Utf8
        } else if enc == encoding_rs::UTF_16LE {
            TextEncoding::Utf16Le
        } else if enc == encoding_rs::UTF_16BE {
            TextEncoding::Utf16Be
        } else if enc.is_ascii_compatible() {
            TextEncoding::Other(enc)
        } else {
            TextEncoding::Other(encoding_rs::WINDOWS_1252)
        }
    }

    /// Code-unit width used by the scanners.
    pub fn width(&self) -> UnitWidth {
        match self {
            TextEncoding::Utf16Le => UnitWidth::TwoLe,
            TextEncoding::Utf16Be => UnitWidth::TwoBe,
            _ => UnitWidth::One,
        }
    }

    /// Byte-order mark written for this encoding when a BOM is requested.
    pub fn bom(&self) -> &'static [u8] {
        match self {
            TextEncoding::Utf8 => &[0xEF, 0xBB, 0xBF],
            TextEncoding::Utf16Le => &[0xFF, 0xFE],
            TextEncoding::Utf16Be => &[0xFE, 0xFF],
            _ => &[],
        }
    }

    /// Lossy decode; invalid input becomes U+FFFD.
    pub fn decode<'a>(&self, bytes: &'a [u8]) -> Cow<'a, str> {
        match self {
            TextEncoding::Utf8 => String::from_utf8_lossy(bytes),
            TextEncoding::Utf16Le => encoding_rs::UTF_16LE.decode_without_bom_handling(bytes).0,
            TextEncoding::Utf16Be => encoding_rs::UTF_16BE.decode_without_bom_handling(bytes).0,
            TextEncoding::Latin1 => {
                if bytes.is_ascii() {
                    // ASCII is valid UTF-8; no copy needed.
                    Cow::Borrowed(std::str::from_utf8(bytes).unwrap_or_default())
                } else {
                    Cow::Owned(bytes.iter().map(|&b| b as char).collect())
                }
            }
            TextEncoding::Other(e) => e.decode_without_bom_handling(bytes).0,
        }
    }

    /// Encodes `s`, appending to `out`. Returns the number of characters that
    /// could not be represented (written as `?`).
    pub fn encode(&self, s: &str, out: &mut Vec<u8>) -> usize {
        match self {
            TextEncoding::Utf8 => {
                out.extend_from_slice(s.as_bytes());
                0
            }
            TextEncoding::Utf16Le => {
                for u in s.encode_utf16() {
                    out.extend_from_slice(&u.to_le_bytes());
                }
                0
            }
            TextEncoding::Utf16Be => {
                for u in s.encode_utf16() {
                    out.extend_from_slice(&u.to_be_bytes());
                }
                0
            }
            TextEncoding::Latin1 => {
                let mut bad = 0;
                for c in s.chars() {
                    if (c as u32) < 0x100 {
                        out.push(c as u32 as u8);
                    } else {
                        out.push(b'?');
                        bad += 1;
                    }
                }
                bad
            }
            TextEncoding::Other(e) => encode_with(e, s, out),
        }
    }

    /// True when every character of `s` can be encoded.
    pub fn can_encode(&self, s: &str) -> bool {
        match self {
            TextEncoding::Utf8 | TextEncoding::Utf16Le | TextEncoding::Utf16Be => true,
            _ => self.encode(s, &mut Vec::new()) == 0,
        }
    }
}

fn encode_with(enc: &'static Encoding, s: &str, out: &mut Vec<u8>) -> usize {
    if s.is_ascii() {
        out.extend_from_slice(s.as_bytes());
        return 0;
    }
    let mut encoder = enc.new_encoder();
    let mut src = s;
    let mut bad = 0;
    let mut buf = vec![0u8; 1024];
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(src, &mut buf, true);
        out.extend_from_slice(&buf[..written]);
        src = &src[read..];
        match result {
            EncoderResult::InputEmpty => return bad,
            EncoderResult::OutputFull => {
                let n = buf.len() * 2;
                buf.resize(n, 0);
            }
            EncoderResult::Unmappable(_) => {
                out.push(b'?');
                bad += 1;
            }
        }
    }
}

/// Recognizes a byte-order mark. Returns the encoding and the BOM length.
pub fn detect_bom(bytes: &[u8]) -> Option<(TextEncoding, usize)> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some((TextEncoding::Utf8, 3))
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Some((TextEncoding::Utf16Le, 2))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Some((TextEncoding::Utf16Be, 2))
    } else {
        None
    }
}

/// How an encoding was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodingEvidence {
    Bom,
    Utf16Pattern,
    ValidUtf8,
    Heuristic,
}

/// Result of [`detect`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetectedEncoding {
    pub encoding: TextEncoding,
    /// Length of the BOM at the start of the file (0 if none).
    pub bom_len: usize,
    pub evidence: EncodingEvidence,
}

/// Detects the encoding of a file from its first bytes. `whole_file` tells
/// whether `sample` is the entire file (a multi-byte sequence cut at the end
/// of a partial sample is not an error).
pub fn detect(sample: &[u8], whole_file: bool) -> DetectedEncoding {
    if let Some((encoding, bom_len)) = detect_bom(sample) {
        return DetectedEncoding {
            encoding,
            bom_len,
            evidence: EncodingEvidence::Bom,
        };
    }
    if let Some(encoding) = utf16_pattern(sample) {
        return DetectedEncoding {
            encoding,
            bom_len: 0,
            evidence: EncodingEvidence::Utf16Pattern,
        };
    }
    if is_utf8(sample, whole_file) {
        return DetectedEncoding {
            encoding: TextEncoding::Utf8,
            bom_len: 0,
            evidence: EncodingEvidence::ValidUtf8,
        };
    }
    // chardetng is accurate on far less than a megabyte and costs ~50 ms/MB.
    let head = &sample[..sample.len().min(256 << 10)];
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(head, whole_file && head.len() == sample.len());
    let guess = det.guess(None, chardetng::Utf8Detection::Deny);
    DetectedEncoding {
        encoding: TextEncoding::from_encoding_rs(guess),
        bom_len: 0,
        evidence: EncodingEvidence::Heuristic,
    }
}

/// UTF-8 validity, tolerating a sequence truncated by the end of the sample.
fn is_utf8(sample: &[u8], whole_file: bool) -> bool {
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(e) => !whole_file && e.error_len().is_none(),
    }
}

/// BOM-less UTF-16: text with mostly ASCII characters has a zero byte in
/// every other position.
fn utf16_pattern(sample: &[u8]) -> Option<TextEncoding> {
    let n = sample.len().min(64 * 1024) & !1;
    if n < 4 {
        return None;
    }
    let pairs = n / 2;
    let (mut even_zero, mut odd_zero) = (0usize, 0usize);
    for pair in sample[..n].chunks_exact(2) {
        even_zero += (pair[0] == 0) as usize;
        odd_zero += (pair[1] == 0) as usize;
    }
    let (hi, lo) = (pairs * 3 / 10, pairs / 50);
    if odd_zero >= hi && even_zero <= lo {
        Some(TextEncoding::Utf16Le)
    } else if even_zero >= hi && odd_zero <= lo {
        Some(TextEncoding::Utf16Be)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boms() {
        assert_eq!(detect_bom(b"\xEF\xBB\xBFa"), Some((TextEncoding::Utf8, 3)));
        assert_eq!(detect_bom(b"\xFF\xFEa\0"), Some((TextEncoding::Utf16Le, 2)));
        assert_eq!(detect_bom(b"\xFE\xFF\0a"), Some((TextEncoding::Utf16Be, 2)));
        assert_eq!(detect_bom(b"abc"), None);
    }

    #[test]
    fn detects_utf16_without_bom() {
        let le: Vec<u8> = "id,name\n1,Ann\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let be: Vec<u8> = "id,name\n1,Ann\n"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        assert_eq!(detect(&le, true).encoding, TextEncoding::Utf16Le);
        assert_eq!(detect(&be, true).encoding, TextEncoding::Utf16Be);
    }

    #[test]
    fn detects_utf8_and_truncated_sample() {
        let s = "имя,город\nАнна,Ташкент\n".as_bytes();
        assert_eq!(detect(s, true).encoding, TextEncoding::Utf8);
        // Cut in the middle of a 2-byte sequence: fine for a partial sample.
        let cut = &s[..s.len() - 2];
        assert_eq!(detect(cut, false).encoding, TextEncoding::Utf8);
    }

    #[test]
    fn detects_windows_1251() {
        let text =
            "Имя;Город;Комментарий\nАнна;Ташкент;Привет, как дела?\nБорис;Москва;Всё хорошо\n"
                .repeat(20);
        let mut bytes = Vec::new();
        TextEncoding::Other(encoding_rs::WINDOWS_1251).encode(&text, &mut bytes);
        assert_eq!(
            detect(&bytes, true).encoding,
            TextEncoding::Other(encoding_rs::WINDOWS_1251)
        );
    }

    #[test]
    fn detects_windows_1252() {
        let text = "name,city\nJosé,Málaga\nFrançois,Besançon\nZoë,Köln\n".repeat(20);
        let mut bytes = Vec::new();
        TextEncoding::Other(encoding_rs::WINDOWS_1252).encode(&text, &mut bytes);
        assert_eq!(
            detect(&bytes, true).encoding,
            TextEncoding::Other(encoding_rs::WINDOWS_1252)
        );
    }

    #[test]
    fn lossy_decode_never_panics() {
        assert_eq!(TextEncoding::Utf8.decode(b"a\xFFb"), "a\u{FFFD}b");
        assert_eq!(TextEncoding::Utf16Le.decode(b"a\0b"), "a\u{FFFD}");
        assert_eq!(TextEncoding::Latin1.decode(b"\xE9"), "é");
    }

    #[test]
    fn encode_reports_unmappable() {
        let mut out = Vec::new();
        let bad = TextEncoding::Other(encoding_rs::WINDOWS_1251).encode("Да ✓", &mut out);
        assert_eq!(bad, 1);
        assert_eq!(out, b"\xC4\xE0 ?");
        let mut out = Vec::new();
        assert_eq!(TextEncoding::Latin1.encode("é€", &mut out), 1);
        assert_eq!(out, b"\xE9?");
    }

    #[test]
    fn labels() {
        assert_eq!(
            TextEncoding::from_label("cp1251"),
            Some(TextEncoding::Other(encoding_rs::WINDOWS_1251))
        );
        assert_eq!(
            TextEncoding::from_label("latin1"),
            Some(TextEncoding::Latin1)
        );
        assert_eq!(
            TextEncoding::from_label("utf-16le"),
            Some(TextEncoding::Utf16Le)
        );
        assert_eq!(TextEncoding::from_label("nope"), None);
    }
}
