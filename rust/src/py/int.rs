//! Python `int()` of a str, a float and the other values sm passes it (config values, argparse
//! flags), with CPython 3.15.1's ValueError, OverflowError and TypeError texts.

use crate::error::SmError;
use crate::py::value::PyValue;

/// `sys.get_int_max_str_digits()` default: a str of more digits raises ValueError.
pub const MAX_STR_DIGITS: usize = 4300;

/// A Python int: `I64` when it fits, else its canonical decimal text (`"-123..."`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Int {
    I64(i64),
    Big(String),
}

impl Int {
    /// The value as i64, or None when it does not fit.
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            Int::I64(i) => Some(*i),
            Int::Big(_) => None,
        }
    }

    /// `str(int)`.
    pub fn text(&self) -> String {
        match self {
            Int::I64(i) => i.to_string(),
            Int::Big(s) => s.clone(),
        }
    }

    fn from_text(text: String) -> Int {
        match text.parse::<i64>() {
            Ok(i) => Int::I64(i),
            Err(_) => Int::Big(text),
        }
    }
}

/// `"%.200R"`: the repr, cut to its first 200 code points (the closing quote can go).
fn repr_200(repr: String) -> String {
    match repr.char_indices().nth(200) {
        Some((i, _)) => repr[..i].to_string(),
        None => repr,
    }
}

fn invalid_literal(repr: String) -> SmError {
    SmError::value(format!(
        "invalid literal for int() with base 10: {}",
        repr_200(repr)
    ))
}

/// Canonical decimal text of ASCII `body` (sign already split off), or None when `PyLong_FromString`
/// rejects it: digits with single `_` between two digits; leading zeros are fine in base 10.
fn digits_of(body: &[u8]) -> Option<String> {
    if body.is_empty() || !body[0].is_ascii_digit() || !body[body.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut digits = String::with_capacity(body.len());
    for (i, &c) in body.iter().enumerate() {
        match c {
            b'0'..=b'9' => digits.push(c as char),
            b'_' if body[i - 1].is_ascii_digit() && body[i + 1].is_ascii_digit() => {}
            _ => return None,
        }
    }
    Some(digits)
}

/// `int(text)` once the text is ASCII and stripped; `bad` builds the invalid-literal error.
fn parse_ascii(t: &[u8], bad: impl Fn() -> SmError) -> Result<Int, SmError> {
    let (neg, body) = match t.first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits = digits_of(body).ok_or_else(&bad)?;
    if digits.len() > MAX_STR_DIGITS {
        return Err(SmError::value(format!(
            "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion: value has {} digits; \
             use sys.set_int_max_str_digits() to increase the limit",
            digits.len()
        )));
    }
    let trimmed = digits.trim_start_matches('0');
    let canonical = if trimmed.is_empty() { "0" } else { trimmed };
    Ok(Int::from_text(if neg && canonical != "0" {
        format!("-{canonical}")
    } else {
        canonical.to_string()
    }))
}

fn is_ascii_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// Python `int(s)` of a str in base 10. Non-ASCII spaces and Unicode decimal digits count
/// (`_PyUnicode_TransformDecimalAndSpaceToASCII`); `\x1c`-`\x1f` do not strip.
pub fn from_str(s: &str) -> Result<Int, SmError> {
    let t = crate::py::float::ascii_number_text(s);
    if !t.is_ascii() {
        return Err(invalid_literal(crate::py::str::repr(s)));
    }
    parse_ascii(t.as_bytes(), || invalid_literal(crate::py::str::repr(s)))
}

/// Python `int(b)` of bytes: ASCII only, stripped of ASCII whitespace.
pub fn from_bytes(b: &[u8]) -> Result<Int, SmError> {
    let start = b
        .iter()
        .position(|&c| !is_ascii_space(c))
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|&c| !is_ascii_space(c))
        .map_or(start, |i| i + 1);
    parse_ascii(&b[start..end], || {
        invalid_literal(crate::py::value::repr_bytes(b))
    })
}

/// Python `int(x)` of a float: truncation toward zero.
pub fn from_float(x: f64) -> Result<Int, SmError> {
    if x.is_nan() {
        return Err(SmError::value("cannot convert float NaN to integer"));
    }
    if x.is_infinite() {
        return Err(SmError::overflow(
            "cannot convert float infinity to integer",
        ));
    }
    let t = x.trunc();
    if t.abs() < 9_223_372_036_854_775_808.0 {
        return Ok(Int::I64(t as i64));
    }
    // |t| >= 2^63: an integer mantissa times a power of two, written out exactly.
    let bits = t.abs().to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1075;
    let mant = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    let mut digits = mant.to_string().into_bytes();
    for _ in 0..exp {
        let mut carry = 0u8;
        for d in digits.iter_mut().rev() {
            let v = (*d - b'0') * 2 + carry;
            *d = b'0' + v % 10;
            carry = v / 10;
        }
        if carry > 0 {
            digits.insert(0, b'0' + carry);
        }
    }
    let text = String::from_utf8(digits).expect("ASCII digits");
    Ok(Int::Big(if t < 0.0 { format!("-{text}") } else { text }))
}

/// Python `int(v)` of any value sm passes it.
pub fn from_value(v: &PyValue) -> Result<Int, SmError> {
    match v {
        PyValue::Bool(b) => Ok(Int::I64(i64::from(*b))),
        PyValue::Int(s) => Ok(Int::from_text(s.clone())),
        PyValue::Float(x) => from_float(*x),
        PyValue::Str(s) => from_str(s),
        PyValue::Bytes(b) => from_bytes(b),
        other => Err(SmError::type_(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            other.tp_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> String {
        match from_str(s) {
            Ok(i) => i.text(),
            Err(e) => e.python_text(),
        }
    }

    #[test]
    fn str_forms_python_accepts() {
        assert_eq!(text("٣"), "3");
        assert_eq!(text("1_0"), "10");
        assert_eq!(text("١_٢"), "12");
        assert_eq!(text(" \u{a0}12\u{2003}"), "12");
        assert_eq!(text("\u{85}5"), "5");
        assert_eq!(text("-0"), "0");
        assert_eq!(text("+007"), "7");
        assert_eq!(text("-9223372036854775809"), "-9223372036854775809");
        assert_eq!(from_str("9223372036854775807").unwrap(), Int::I64(i64::MAX));
        assert_eq!(
            from_str("9223372036854775808").unwrap(),
            Int::Big("9223372036854775808".into())
        );
    }

    #[test]
    fn str_forms_python_rejects() {
        for s in [
            "",
            "-",
            "+-1",
            "1.5",
            "0x10",
            "1 2",
            "\x1c5",
            "_1",
            "1_",
            "1__0",
            "²",
            "1\u{10FDFF}",
        ] {
            assert!(
                text(s).starts_with("ValueError: invalid literal for int() with base 10: '"),
                "{s:?}"
            );
        }
        assert_eq!(
            text("\x1c5"),
            "ValueError: invalid literal for int() with base 10: '\\x1c5'"
        );
        // %.200R: 200 code points of the repr, so no closing quote (40 + 200 = 240 chars).
        let long = text(&"a".repeat(250));
        assert_eq!(long.chars().count(), "ValueError: ".len() + 240);
        assert!(long.ends_with("aaaa"));
    }

    #[test]
    fn digit_limit_counts_leading_zeros_and_skips_underscores() {
        let msg = "ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; \
                   use sys.set_int_max_str_digits() to increase the limit";
        assert_eq!(text(&format!("{}1", "0".repeat(4300))), msg);
        assert_eq!(text(&"1".repeat(4300)).len(), 4300);
        assert_eq!(text(&format!("{}1", "1_".repeat(4299))).len(), 4300);
        // The syntax check comes first.
        assert!(text(&format!("{}x", "1".repeat(4301))).contains("invalid literal"));
    }

    #[test]
    fn floats_truncate_and_specials_raise() {
        let f = |x: f64| {
            from_float(x)
                .map(|i| i.text())
                .unwrap_or_else(|e| e.python_text())
        };
        assert_eq!(f(1.9), "1");
        assert_eq!(f(-1.9), "-1");
        assert_eq!(f(-0.5), "0");
        assert_eq!(f(2f64.powi(63)), "9223372036854775808");
        // 1e23 is 99999999999999991611392 exactly (Python: int(1e23)).
        assert_eq!(f(1e23), "99999999999999991611392");
        assert_eq!(f(-1e23), "-99999999999999991611392");
        assert_eq!(
            f(f64::NAN),
            "ValueError: cannot convert float NaN to integer"
        );
        assert_eq!(
            f(f64::NEG_INFINITY),
            "OverflowError: cannot convert float infinity to integer"
        );
    }

    #[test]
    fn other_values() {
        assert_eq!(from_value(&PyValue::Bool(true)).unwrap(), Int::I64(1));
        assert_eq!(
            from_value(&PyValue::Bytes(b" 12\n".to_vec())).unwrap(),
            Int::I64(12)
        );
        assert_eq!(
            from_value(&PyValue::Bytes(b"x".to_vec())).unwrap_err().msg,
            "invalid literal for int() with base 10: b'x'"
        );
        assert_eq!(
            from_value(&PyValue::None).unwrap_err().python_text(),
            "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
        assert_eq!(
            from_value(&PyValue::List(vec![])).unwrap_err().msg,
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
        );
    }
}
