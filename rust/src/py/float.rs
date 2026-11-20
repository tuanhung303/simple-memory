//! Python float formatting: `repr`/`str`, `round(x, n)`, `format(x, ".Nf" | ".N%" | "g")` and `float(str)`.
//!
//! Python 3.15 formats through `_Py_dg_dtoa`, which rounds the exact binary value half to even.
//! Rust's `{:.N}` and `{:.Ne}` also round the exact value half to even, and `{:e}` gives the
//! shortest digits that round-trip, as `repr` does; this module only lays the digits out the
//! Python way.

use crate::error::SmError;

/// Shortest round-trip digits of a finite nonzero `|x|` and the decimal point position:
/// `x = 0.d1d2... * 10^decpt`.
fn digits(spec: &str) -> (String, i32) {
    let (mant, exp) = spec
        .split_once('e')
        .expect("`{:e}` output holds an exponent");
    let mant = mant.trim_start_matches('-');
    let mut d: String = mant.chars().filter(|c| *c != '.').collect();
    while d.len() > 1 && d.ends_with('0') {
        d.pop();
    }
    let exp: i32 = exp.parse().expect("`{:e}` exponent is an integer");
    (d, exp + 1)
}

fn special(x: f64) -> Option<&'static str> {
    if x.is_nan() {
        Some("nan")
    } else if x == f64::INFINITY {
        Some("inf")
    } else if x == f64::NEG_INFINITY {
        Some("-inf")
    } else {
        None
    }
}

fn exp_form(d: &str, decpt: i32) -> String {
    let e = decpt - 1;
    let mut s = d[..1].to_string();
    if d.len() > 1 {
        s.push('.');
        s.push_str(&d[1..]);
    }
    let sign = if e < 0 { '-' } else { '+' };
    s.push_str(&format!("e{sign}{:02}", e.abs()));
    s
}

/// Python `repr(x)`, which is also `str(x)` and the `json.dumps` text of a finite float.
/// The shortest digits that read back as `x` (`{:e}` form). When two such digit strings are
/// equally near the exact value, Rust's `{:e}` takes the upper one and Python's dtoa the even one
/// (1759256098135120.25 is 1759256098135120.2 in Python); the exact-mode `{:.Ne}` at the same
/// length rounds half to even, so it gives Python's choice whenever it reads back as `x`.
fn shortest(x: f64) -> String {
    let s = format!("{x:e}");
    let n = s
        .split_once('e')
        .map_or(0, |(m, _)| m.chars().filter(char::is_ascii_digit).count());
    let even = format!("{x:.prec$e}", prec = n.saturating_sub(1));
    if even.parse::<f64>() == Ok(x) {
        even
    } else {
        s
    }
}

pub fn repr(x: f64) -> String {
    if let Some(s) = special(x) {
        return s.to_string();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (d, decpt) = digits(&shortest(x.abs()));
    let body = if decpt <= -4 || decpt > 16 {
        exp_form(&d, decpt)
    } else if decpt <= 0 {
        format!("0.{}{d}", "0".repeat((-decpt) as usize))
    } else if decpt as usize >= d.len() {
        format!("{d}{}.0", "0".repeat(decpt as usize - d.len()))
    } else {
        format!("{}.{}", &d[..decpt as usize], &d[decpt as usize..])
    };
    format!("{sign}{body}")
}

/// Python `round(x, n)` for a float `x` and an int `n` (CPython `float___round___impl`): nan and
/// inf round to themselves, `n` past NDIGITS_MAX (323) returns `x`, below NDIGITS_MIN (-308) a
/// zero with the sign of `x`; else the exact value rounded half to even at `10**-n`, and a result
/// past the float range raises OverflowError.
pub fn round_ndigits(x: f64, n: i64) -> Result<f64, SmError> {
    if !x.is_finite() || n > 323 {
        return Ok(x);
    }
    if n < -308 {
        return Ok(0.0 * x);
    }
    if n >= 0 {
        return Ok(format!("{x:.prec$}", prec = n as usize)
            .parse()
            .expect("Rust float text parses"));
    }
    // n < 0: the integer part is exact in `{:.0}` of the truncated value; split off its last k
    // digits and round the quotient half to even (a nonzero fraction breaks a tie upward).
    let k = n.unsigned_abs() as usize;
    let ax = x.abs();
    let int = format!("{:.0}", ax.trunc());
    let (q, r) = if int.len() > k {
        (
            int[..int.len() - k].to_string(),
            int[int.len() - k..].to_string(),
        )
    } else {
        ("0".to_string(), format!("{int:0>k$}"))
    };
    let half = format!("5{}", "0".repeat(k - 1));
    let odd = q.as_bytes().last().is_some_and(|d| (d - b'0') % 2 == 1);
    let up = r > half || (r == half && (ax.fract() != 0.0 || odd));
    let mut digits = q.into_bytes();
    if up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let text = format!(
        "{}{}",
        String::from_utf8(digits).expect("ASCII digits"),
        "0".repeat(k)
    );
    let v: f64 = text.parse().expect("decimal digits parse");
    if v.is_infinite() {
        return Err(SmError::overflow("rounded value too large to represent"));
    }
    Ok(if x < 0.0 { -v } else { v })
}

/// Python `format(x, ".{prec}f")`.
pub fn format_fixed(x: f64, prec: usize) -> String {
    match special(x) {
        Some(s) => s.to_string(),
        None => format!("{x:.prec$}"),
    }
}

/// Python `format(x, ".{prec}%")`: `x * 100` in float arithmetic, then fixed, then `%`.
pub fn format_percent(x: f64, prec: usize) -> String {
    format!("{}%", format_fixed(x * 100.0, prec))
}

/// Python `format(x, "g")`: 6 significant digits, trailing zeros dropped.
pub fn format_g(x: f64) -> String {
    const P: i32 = 6;
    if let Some(s) = special(x) {
        return s.to_string();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0");
    }
    let (d, decpt) = digits(&format!("{:.*e}", (P - 1) as usize, x.abs()));
    let body = if decpt <= -4 || decpt > P {
        exp_form(&d, decpt)
    } else if decpt <= 0 {
        format!("0.{}{d}", "0".repeat((-decpt) as usize))
    } else if decpt as usize >= d.len() {
        format!("{d}{}", "0".repeat(decpt as usize - d.len()))
    } else {
        format!("{}.{}", &d[..decpt as usize], &d[decpt as usize..])
    };
    format!("{sign}{body}")
}

/// Python `float(s)` for a str: whitespace around is ignored, `_` may sit between digits,
/// and `inf`, `infinity` and `nan` take any case and a sign.
pub fn from_str(s: &str) -> Result<f64, SmError> {
    let fail = || {
        SmError::value(format!(
            "could not convert string to float: {}",
            crate::py::str::repr(s)
        ))
    };
    let t = ascii_number_text(s);
    let t = t.as_str();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let lower = body.to_ascii_lowercase();
    let v = if lower == "inf" || lower == "infinity" {
        f64::INFINITY
    } else if lower == "nan" {
        f64::NAN
    } else {
        let clean = strip_digit_underscores(body).ok_or_else(fail)?;
        if !is_decimal_literal(&clean) {
            return Err(fail());
        }
        clean.parse::<f64>().map_err(|_| fail())?
    };
    Ok(if neg { -v } else { v })
}

/// The text `float()` and `int()` parse: CPython's `_PyUnicode_TransformDecimalAndSpaceToASCII`
/// (a non-ASCII space becomes `' '`, a non-ASCII decimal digit its ASCII digit), then the strip of
/// ASCII whitespace. `\x1c`-`\x1f` are `isspace` in Python yet stay, so `float("\x1c1")` fails.
pub fn ascii_number_text(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if (c as u32) < 127 {
                c
            } else if crate::py::str::isspace(c) {
                ' '
            } else {
                decimal_digit(c).unwrap_or(c)
            }
        })
        .collect();
    mapped
        .trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
        .to_string()
}

/// The ASCII digit of a Unicode decimal digit (category Nd), from Python 3.15's tables.
fn decimal_digit(c: char) -> Option<char> {
    let c = c as u32;
    let i = DECIMAL_ZEROS.partition_point(|&z| z <= c);
    let zero = *DECIMAL_ZEROS.get(i.checked_sub(1)?)?;
    (c - zero < 10).then(|| char::from(b'0' + (c - zero) as u8))
}

/// Drops each `_` that sits between two ASCII digits; any other `_` makes the text invalid.
fn strip_digit_underscores(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in b.iter().enumerate() {
        if c == b'_' {
            let ok =
                i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit();
            if !ok {
                return None;
            }
        } else {
            out.push(c as char);
        }
    }
    Some(out)
}

/// `digits [. digits?] | . digits`, then an optional `e [sign] digits`.
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_len = i - int_start;
    let mut frac_len = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_len = i - f;
    }
    if int_len == 0 && frac_len == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

// Generated from Python 3.15.1 (unicodedata 17.0.0): every code point above 127 with
// `unicodedata.decimal(c) == 0`; each starts a run of the ten digits 0-9.
#[rustfmt::skip]
const DECIMAL_ZEROS: &[u32] = &[
    0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66, 0xde6,
    0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50,
    0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x10d40, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0,
    0x11650, 0x116c0, 0x116d0, 0x116da, 0x11730, 0x118e0, 0x11950, 0x11bf0, 0x11c50, 0x11d50,
    0x11da0, 0x11de0, 0x11f50, 0x16130, 0x16a60, 0x16ac0, 0x16b50, 0x16d70, 0x1ccf0, 0x1d7ce,
    0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e5f1, 0x1e950, 0x1fbf0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_switches_to_exponent_at_python_bounds() {
        assert_eq!(repr(1e16), "1e+16");
        assert_eq!(repr(1e15), "1000000000000000.0");
        assert_eq!(repr(0.0001), "0.0001");
        assert_eq!(repr(0.00001), "1e-05");
        assert_eq!(repr(-0.0), "-0.0");
        assert_eq!(repr(f64::NAN), "nan");
        assert_eq!(repr(5e-324), "5e-324");
    }

    #[test]
    fn repr_takes_the_even_digit_on_a_tie() {
        // Exact 1759256098135120.25: Rust's `{:e}` writes ...120.3, Python's repr ...120.2.
        assert_eq!(repr(1759256098135120.0 + 0.25), "1759256098135120.2");
        assert_eq!(repr(-(1759256098135120.0 + 0.25)), "-1759256098135120.2");
        // 2^50 + 0.25 and 2^50 + 0.75 (Python probe): the even digit is down for one, up for the other.
        assert_eq!(repr(2f64.powi(50) + 0.25), "1125899906842624.2");
        assert_eq!(repr(2f64.powi(50) + 0.75), "1125899906842624.8");
        assert_eq!(repr(0.1), "0.1");
    }

    #[test]
    fn round_and_fixed_use_the_exact_binary_value() {
        // 2.675 is stored as 2.67499999999999982236431605997495353221893310546875.
        assert_eq!(round_ndigits(2.675, 2).unwrap(), 2.67);
        assert_eq!(round_ndigits(14.95, 1).unwrap(), 14.9);
        assert_eq!(round_ndigits(0.125, 2).unwrap(), 0.12);
        assert_eq!(round_ndigits(0.375, 2).unwrap(), 0.38);
    }

    #[test]
    fn round_negative_ndigits_as_python() {
        // Python probe values.
        let r = |x: f64, n: i64| {
            round_ndigits(x, n)
                .map(repr)
                .unwrap_or_else(|e| e.python_text())
        };
        for (x, n, want) in [
            (123456789.0, -1, "123456790.0"),
            (15.0, -1, "20.0"),
            (25.0, -1, "20.0"),
            (5.0, -1, "0.0"),
            (5.5, -1, "10.0"),
            (-4.0, -1, "-0.0"),
            (50.0, -2, "0.0"),
            (60.0, -2, "100.0"),
            (1.7e308, -307, "1.7e+308"),
            (9.5e307, -308, "1e+308"),
            (1e300, -309, "0.0"),
            (-1e300, -400, "-0.0"),
            (1e22, -21, "1e+22"),
            (2.5, 0, "2.0"),
            (-0.4, 0, "-0.0"),
            (10000000000000002.0, -1, "1e+16"),
        ] {
            assert_eq!(r(x, n), want, "round({x}, {n})");
        }
        assert_eq!(
            r(1.7e308, -308),
            "OverflowError: rounded value too large to represent"
        );
        assert!(round_ndigits(f64::INFINITY, -400).unwrap().is_infinite());
        assert_eq!(format_fixed(-0.04, 1), "-0.0");
        assert_eq!(format_percent(0.125, 0), "12%");
    }

    #[test]
    fn g_drops_zeros_and_uses_two_digit_exponents() {
        assert_eq!(format_g(2.0), "2");
        assert_eq!(format_g(1234567.0), "1.23457e+06");
        assert_eq!(format_g(0.0001), "0.0001");
        assert_eq!(format_g(-0.0), "-0");
    }

    #[test]
    fn from_str_follows_python_float() {
        assert_eq!(from_str(" 1_0.5 \n").unwrap(), 10.5);
        assert_eq!(from_str("-Infinity").unwrap(), f64::NEG_INFINITY);
        assert_eq!(from_str("1.e5").unwrap(), 100000.0);
        assert_eq!(from_str("\u{661}\u{662}\u{a0}").unwrap(), 12.0);
        assert_eq!(
            from_str("\x1c1").unwrap_err().to_string(),
            "could not convert string to float: '\\x1c1'"
        );
        let err = from_str("1__0").unwrap_err();
        assert_eq!(err.to_string(), "could not convert string to float: '1__0'");
        // Rust's parser alone would take the second sign.
        assert_eq!(
            from_str("+-5").unwrap_err().to_string(),
            "could not convert string to float: '+-5'"
        );
        assert_eq!(from_str("0x10").unwrap_err().python_name(), "ValueError");
    }
}
