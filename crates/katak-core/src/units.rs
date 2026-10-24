//! Code units.
//!
//! The scanners work on *code units* rather than bytes so the same state
//! machine handles UTF-8 / single-byte code pages (1-byte units) and UTF-16
//! (2-byte units). Every structural character (delimiter, quote, CR, LF,
//! comment prefix, escape) is a sequence of whole units, and all offsets
//! handed out by the scanners are byte offsets into the source.

/// Width and byte order of the code units of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UnitWidth {
    /// UTF-8 or an ASCII-compatible code page.
    #[default]
    One,
    /// UTF-16 little endian.
    TwoLe,
    /// UTF-16 big endian.
    TwoBe,
}

impl UnitWidth {
    /// Bytes per unit.
    pub const fn bytes(self) -> usize {
        match self {
            UnitWidth::One => 1,
            UnitWidth::TwoLe | UnitWidth::TwoBe => 2,
        }
    }
}

/// Access to the code units of a byte buffer.
pub trait Units: Copy + Default + Send + Sync + 'static {
    /// Bytes per unit.
    const W: usize;
    /// Unit at byte index `i`. The caller guarantees `i + W <= buf.len()`.
    fn get(buf: &[u8], i: usize) -> u32;
    /// Appends the encoding of unit `u`.
    fn put(u: u32, out: &mut Vec<u8>);
}

/// 1-byte units.
#[derive(Clone, Copy, Debug, Default)]
pub struct U8;

/// UTF-16 little-endian units.
#[derive(Clone, Copy, Debug, Default)]
pub struct U16Le;

/// UTF-16 big-endian units.
#[derive(Clone, Copy, Debug, Default)]
pub struct U16Be;

impl Units for U8 {
    const W: usize = 1;
    #[inline(always)]
    fn get(buf: &[u8], i: usize) -> u32 {
        buf[i] as u32
    }
    #[inline(always)]
    fn put(u: u32, out: &mut Vec<u8>) {
        out.push(u as u8);
    }
}

impl Units for U16Le {
    const W: usize = 2;
    #[inline(always)]
    fn get(buf: &[u8], i: usize) -> u32 {
        u16::from_le_bytes([buf[i], buf[i + 1]]) as u32
    }
    #[inline(always)]
    fn put(u: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&(u as u16).to_le_bytes());
    }
}

impl Units for U16Be {
    const W: usize = 2;
    #[inline(always)]
    fn get(buf: &[u8], i: usize) -> u32 {
        u16::from_be_bytes([buf[i], buf[i + 1]]) as u32
    }
    #[inline(always)]
    fn put(u: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&(u as u16).to_be_bytes());
    }
}

/// Runs `$body` with the type alias `$U` bound to the `Units` implementation
/// matching `$width`.
#[macro_export]
macro_rules! with_units {
    ($width:expr, $U:ident => $body:expr) => {
        match $width {
            $crate::units::UnitWidth::One => {
                type $U = $crate::units::U8;
                $body
            }
            $crate::units::UnitWidth::TwoLe => {
                type $U = $crate::units::U16Le;
                $body
            }
            $crate::units::UnitWidth::TwoBe => {
                type $U = $crate::units::U16Be;
                $body
            }
        }
    };
}

pub(crate) const LF: u32 = b'\n' as u32;
pub(crate) const CR: u32 = b'\r' as u32;
pub(crate) const SPACE: u32 = b' ' as u32;
pub(crate) const TAB: u32 = b'\t' as u32;
pub(crate) const BACKSLASH: u32 = b'\\' as u32;

#[inline(always)]
pub(crate) fn is_blank(u: u32) -> bool {
    u == SPACE || u == TAB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_access() {
        let le = [b'a', 0, 0x3B, 0x04];
        assert_eq!(U16Le::get(&le, 0), 'a' as u32);
        assert_eq!(U16Le::get(&le, 2), 0x043B);
        let be = [0, b'a', 0x04, 0x3B];
        assert_eq!(U16Be::get(&be, 0), 'a' as u32);
        assert_eq!(U16Be::get(&be, 2), 0x043B);
        let mut out = Vec::new();
        U16Le::put(0x043B, &mut out);
        U16Be::put(0x043B, &mut out);
        assert_eq!(out, [0x3B, 0x04, 0x04, 0x3B]);
    }

    #[test]
    fn dispatch() {
        fn width<U: Units>() -> usize {
            U::W
        }
        assert_eq!(with_units!(UnitWidth::One, U => width::<U>()), 1);
        assert_eq!(with_units!(UnitWidth::TwoBe, U => width::<U>()), 2);
    }
}
