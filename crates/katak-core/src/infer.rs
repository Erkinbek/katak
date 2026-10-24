//! Value classification and column type inference.
//!
//! Types are inferred from a sample and are used only for alignment and sort
//! order. They never change what is displayed or saved.

use std::fmt;

/// Inferred type of a column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColumnType {
    #[default]
    Text,
    Integer,
    Float,
    Date,
}

impl ColumnType {
    pub const ALL: [ColumnType; 4] = [
        ColumnType::Text,
        ColumnType::Integer,
        ColumnType::Float,
        ColumnType::Date,
    ];

    pub fn is_numeric(self) -> bool {
        matches!(self, ColumnType::Integer | ColumnType::Float)
    }

    pub fn label(self) -> &'static str {
        match self {
            ColumnType::Text => "Text",
            ColumnType::Integer => "Integer",
            ColumnType::Float => "Float",
            ColumnType::Date => "Date",
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What a single value looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Empty,
    Integer,
    Float,
    Date,
    Text,
}

/// Classifies one value (surrounding blanks are ignored for classification).
pub fn classify(s: &str) -> ValueKind {
    let t = s.trim();
    if t.is_empty() {
        ValueKind::Empty
    } else if is_integer(t) {
        ValueKind::Integer
    } else if parse_number(t).is_some() {
        ValueKind::Float
    } else if parse_date(t, DateOrder::Auto).is_some() {
        ValueKind::Date
    } else {
        ValueKind::Text
    }
}

fn is_integer(t: &str) -> bool {
    let b = t.as_bytes();
    let digits = if b[0] == b'-' || b[0] == b'+' {
        &b[1..]
    } else {
        b
    };
    !digits.is_empty() && digits.len() <= 18 && digits.iter().all(u8::is_ascii_digit)
}

/// Parses a number with `.` or `,` as the decimal separator and an optional
/// exponent: `-12`, `3.5`, `3,5`, `1e-3`, `.5`. Thousands separators are not
/// accepted (they would make `1,234` ambiguous).
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let b = t.as_bytes();
    if b.is_empty() || b.len() > 64 {
        return None;
    }
    let mut i = 0;
    if b[0] == b'-' || b[0] == b'+' {
        i = 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    let mut comma = false;
    if i < b.len() && (b[i] == b'.' || b[i] == b',') {
        comma = b[i] == b',';
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - f;
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    if comma {
        t.replacen(',', ".", 1).parse().ok()
    } else {
        t.parse().ok()
    }
}

/// Day/month order for ambiguous `a/b/yyyy` dates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DateOrder {
    /// `/` means month first, `.` and `-` mean day first.
    #[default]
    Auto,
    DayFirst,
    MonthFirst,
}

/// Parses common date / date-time forms into a sortable key
/// `YYYYMMDDhhmmss`: `2024-03-01`, `2024/03/01`, `01.03.2024`,
/// `03/01/2024`, optionally followed by ` 12:30[:45[.123]]` or `T…`.
pub fn parse_date(s: &str, order: DateOrder) -> Option<i64> {
    let t = s.trim();
    if t.len() < 8 || t.len() > 40 {
        return None;
    }
    let (date, time) = match t.find([' ', 'T']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let sep = date.bytes().find(|b| matches!(b, b'-' | b'/' | b'.'))? as char;
    let parts: Vec<&str> = date.split(sep).collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let n: Vec<i64> = parts.iter().map(|p| p.parse().unwrap_or(-1)).collect();
    let (y, m, d) = if parts[0].len() == 4 {
        (n[0], n[1], n[2])
    } else if parts[2].len() == 4 {
        let day_first = match order {
            DateOrder::DayFirst => true,
            DateOrder::MonthFirst => false,
            DateOrder::Auto => sep != '/' || n[0] > 12,
        };
        if day_first {
            (n[2], n[1], n[0])
        } else {
            (n[2], n[0], n[1])
        }
    } else {
        return None;
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1000..=9999).contains(&y) {
        return None;
    }
    let (mut hh, mut mm, mut ss) = (0, 0, 0);
    if let Some(time) = time {
        let time = time.trim_end_matches('Z');
        let time = time.split(['+', '.']).next().unwrap_or("");
        let tp: Vec<&str> = time.split(':').collect();
        if tp.len() < 2
            || tp.len() > 3
            || tp
                .iter()
                .any(|p| p.is_empty() || p.len() > 2 || !p.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        hh = tp[0].parse().ok()?;
        mm = tp[1].parse().ok()?;
        ss = if tp.len() == 3 {
            tp[2].parse().ok()?
        } else {
            0
        };
        if hh > 23 || mm > 59 || ss > 60 {
            return None;
        }
    }
    Some(((((y * 100 + m) * 100 + d) * 100 + hh) * 100 + mm) * 100 + ss)
}

/// Infers a column type: the type every non-empty sampled value has (a
/// mix of integers and floats is `Float`). Columns with no values are text.
pub fn infer<'a>(values: impl IntoIterator<Item = &'a str>) -> ColumnType {
    let (mut ints, mut floats, mut dates, mut texts) = (0usize, 0usize, 0usize, 0usize);
    for v in values {
        match classify(v) {
            ValueKind::Empty => {}
            ValueKind::Integer => ints += 1,
            ValueKind::Float => floats += 1,
            ValueKind::Date => dates += 1,
            ValueKind::Text => texts += 1,
        }
    }
    if texts > 0 || ints + floats + dates == 0 {
        ColumnType::Text
    } else if dates > 0 {
        if ints + floats == 0 {
            ColumnType::Date
        } else {
            ColumnType::Text
        }
    } else if floats > 0 {
        ColumnType::Float
    } else {
        ColumnType::Integer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_number("3,25"), Some(3.25));
        assert_eq!(parse_number(" -2.5e3 "), Some(-2500.0));
        assert_eq!(parse_number(".5"), Some(0.5));
        assert_eq!(parse_number("1,234,5"), None);
        assert_eq!(parse_number("12a"), None);
        assert_eq!(parse_number("."), None);
        assert_eq!(classify("007"), ValueKind::Integer);
        assert_eq!(classify("1.0"), ValueKind::Float);
    }

    #[test]
    fn dates() {
        assert_eq!(
            parse_date("2024-03-01", DateOrder::Auto),
            Some(20240301000000)
        );
        assert_eq!(
            parse_date("01.03.2024 12:30", DateOrder::Auto),
            Some(20240301123000)
        );
        assert_eq!(
            parse_date("03/01/2024", DateOrder::Auto),
            Some(20240301000000)
        );
        assert_eq!(
            parse_date("13/01/2024", DateOrder::Auto),
            Some(20240113000000)
        );
        assert_eq!(
            parse_date("2024-03-01T10:00:05Z", DateOrder::Auto),
            Some(20240301100005)
        );
        assert_eq!(parse_date("2024-13-01", DateOrder::Auto), None);
        assert_eq!(parse_date("hello world", DateOrder::Auto), None);
    }

    #[test]
    fn inference() {
        assert_eq!(infer(["1", "2", "", "-3"]), ColumnType::Integer);
        assert_eq!(infer(["1", "2.5"]), ColumnType::Float);
        assert_eq!(infer(["2024-01-01", ""]), ColumnType::Date);
        assert_eq!(infer(["1", "x"]), ColumnType::Text);
        assert_eq!(infer(["", ""]), ColumnType::Text);
    }
}
