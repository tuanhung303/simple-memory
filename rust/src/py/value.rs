//! Python value model for `yaml.safe_load` results and `json.loads` values that sm passes
//! through `str()`, with Python's `str()`, `repr()`, `isoformat()` and dict-key equality.

use crate::py::float;

/// A Python value. `Dict` keeps insertion order. `Int` is the canonical decimal text
/// (`"-12"`, `"0"`), since YAML ints have no size limit.
#[derive(Clone, Debug, PartialEq)]
pub enum PyValue {
    None,
    Bool(bool),
    Int(String),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date(Date),
    DateTime(DateTime),
    Time(Time),
    List(Vec<PyValue>),
    Tuple(Vec<PyValue>),
    Dict(Vec<(PyValue, PyValue)>),
    Set(Vec<PyValue>),
    /// A container met again inside its own repr (YAML `&a [*a]`): Python prints `[...]`
    /// for a list and `{...}` for a dict.
    Cycle(&'static str),
}

/// `type(value).__name__`. `PyValue::tp_name` is the C type's name: it differs for the datetime
/// types and a dict `Cycle`.
pub fn type_name(v: &PyValue) -> &'static str {
    match v {
        PyValue::None => "NoneType",
        PyValue::Bool(_) => "bool",
        PyValue::Int(_) => "int",
        PyValue::Float(_) => "float",
        PyValue::Str(_) => "str",
        PyValue::Bytes(_) => "bytes",
        PyValue::Date(_) => "date",
        PyValue::DateTime(_) => "datetime",
        PyValue::Time(_) => "time",
        PyValue::List(_) | PyValue::Cycle(_) => "list",
        PyValue::Tuple(_) => "tuple",
        PyValue::Dict(_) => "dict",
        PyValue::Set(_) => "set",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// A naive `datetime.time` (tomllib gives no other kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
}

/// `tz` is the UTC offset in seconds; `None` is a naive datetime, `Some(0)` is `timezone.utc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub date: Date,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
    pub tz: Option<i32>,
}

impl PyValue {
    pub fn int(n: i64) -> PyValue {
        PyValue::Int(n.to_string())
    }

    /// Python `str(value)`.
    pub fn py_str(&self) -> String {
        match self {
            PyValue::Str(s) => s.clone(),
            PyValue::Date(d) => d.isoformat(),
            PyValue::DateTime(dt) => dt.isoformat(' '),
            PyValue::Time(t) => t.isoformat(),
            _ => self.py_repr(),
        }
    }

    /// Python `str(value)` with its ValueError for an int of more than 4300 digits anywhere in it.
    pub fn try_py_str(&self) -> Result<String, crate::error::SmError> {
        self.str_digits_ok()?;
        Ok(self.py_str())
    }

    /// Python `repr(value)`.
    pub fn py_repr(&self) -> String {
        if self.nesting() > SHALLOW {
            return on_deep_stack(|| self.repr_inner());
        }
        self.repr_inner()
    }

    /// The deepest container nesting in `self` (a scalar is 0), counted without recursion.
    pub fn nesting(&self) -> usize {
        let mut max = 0;
        let mut stack = vec![(self, 0usize)];
        while let Some((v, d)) = stack.pop() {
            let kids: Box<dyn Iterator<Item = &PyValue>> = match v {
                PyValue::List(x) | PyValue::Tuple(x) | PyValue::Set(x) => Box::new(x.iter()),
                PyValue::Dict(p) => Box::new(p.iter().flat_map(|(k, v)| [k, v])),
                _ => continue,
            };
            max = max.max(d + 1);
            stack.extend(kids.map(|k| (k, d + 1)));
        }
        max
    }

    fn repr_inner(&self) -> String {
        match self {
            PyValue::None => "None".into(),
            PyValue::Bool(true) => "True".into(),
            PyValue::Bool(false) => "False".into(),
            PyValue::Int(s) => s.clone(),
            PyValue::Float(x) => float::repr(*x),
            PyValue::Str(s) => crate::py::str::repr(s),
            PyValue::Bytes(b) => repr_bytes(b),
            PyValue::Date(d) => format!("datetime.date({}, {}, {})", d.year, d.month, d.day),
            PyValue::DateTime(dt) => dt.repr(),
            PyValue::Time(t) => t.repr(),
            PyValue::List(v) => format!("[{}]", join_repr(v)),
            PyValue::Tuple(v) if v.len() == 1 => format!("({},)", v[0].repr_inner()),
            PyValue::Tuple(v) => format!("({})", join_repr(v)),
            PyValue::Set(v) if v.is_empty() => "set()".into(),
            PyValue::Set(v) => format!("{{{}}}", join_repr(v)),
            PyValue::Dict(pairs) => {
                let items: Vec<String> = pairs
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.repr_inner(), v.repr_inner()))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            PyValue::Cycle(s) => (*s).into(),
        }
    }

    /// Python `value.isoformat()` of a date, datetime or time; `None` for any other value.
    pub fn isoformat(&self) -> Option<String> {
        match self {
            PyValue::Date(d) => Some(d.isoformat()),
            PyValue::DateTime(dt) => Some(dt.isoformat('T')),
            PyValue::Time(t) => Some(t.isoformat()),
            _ => None,
        }
    }

    /// Python `isinstance(value, collections.abc.Hashable)` for the values this model holds.
    pub fn is_hashable(&self) -> bool {
        match self {
            PyValue::List(_) | PyValue::Dict(_) | PyValue::Set(_) | PyValue::Cycle(_) => false,
            PyValue::Tuple(v) => v.iter().all(PyValue::is_hashable),
            _ => true,
        }
    }

    /// Python `a == b` for two hashable values, as a dict compares keys. NaN equals NaN here,
    /// because every YAML `.nan` is the same float object (`SafeConstructor.nan_value`) and a
    /// dict checks identity first.
    pub fn key_eq(&self, other: &PyValue) -> bool {
        match (self.number(), other.number()) {
            (Some(a), Some(b)) => return a.eq(&b),
            (Some(_), Option::None) | (Option::None, Some(_)) => return false,
            (Option::None, Option::None) => {}
        }
        use PyValue::*;
        match (self, other) {
            (None, None) => true,
            (Str(a), Str(b)) => a == b,
            (Bytes(a), Bytes(b)) => a == b,
            (Date(a), Date(b)) => a == b,
            (Time(a), Time(b)) => a == b,
            (DateTime(a), DateTime(b)) => match (a.tz, b.tz) {
                (Option::None, Option::None) => a == b,
                (Some(_), Some(_)) => a.utc_micros() == b.utc_micros(),
                _ => false,
            },
            (Tuple(a), Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.key_eq(y)),
            _ => false,
        }
    }

    /// Python's ValueError when `str()` or `repr()` of `self` meets an int of more than
    /// `MAX_STR_DIGITS` digits (a hex, octal or binary TOML int can be that long); else Ok.
    pub fn str_digits_ok(&self) -> Result<(), crate::error::SmError> {
        let mut stack = vec![self];
        while let Some(v) = stack.pop() {
            match v {
                PyValue::Int(t) => {
                    crate::py::json::int_text(t)?;
                }
                PyValue::List(x) | PyValue::Tuple(x) | PyValue::Set(x) => stack.extend(x),
                PyValue::Dict(p) => stack.extend(p.iter().flat_map(|(k, v)| [k, v])),
                _ => {}
            }
        }
        Ok(())
    }

    /// The C type's `tp_name`, as a TypeError such as `'datetime.date' object is not iterable`
    /// names it.
    pub fn tp_name(&self) -> &'static str {
        match self {
            PyValue::None => "NoneType",
            PyValue::Bool(_) => "bool",
            PyValue::Int(_) => "int",
            PyValue::Float(_) => "float",
            PyValue::Str(_) => "str",
            PyValue::Bytes(_) => "bytes",
            PyValue::Date(_) => "datetime.date",
            PyValue::DateTime(_) => "datetime.datetime",
            PyValue::Time(_) => "datetime.time",
            PyValue::List(_) | PyValue::Cycle("[...]") => "list",
            PyValue::Tuple(_) => "tuple",
            PyValue::Dict(_) | PyValue::Cycle(_) => "dict",
            PyValue::Set(_) => "set",
        }
    }

    fn number(&self) -> Option<Num> {
        match self {
            PyValue::Bool(b) => Some(Num::Int(if *b { "1".into() } else { "0".into() })),
            PyValue::Int(s) => Some(Num::Int(s.clone())),
            PyValue::Float(x) => Some(Num::Float(*x)),
            _ => None,
        }
    }
}

enum Num {
    Int(String),
    Float(f64),
}

impl Num {
    fn eq(&self, other: &Num) -> bool {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a == b,
            (Num::Float(a), Num::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Num::Int(i), Num::Float(f)) | (Num::Float(f), Num::Int(i)) => {
                f.is_finite() && f.fract() == 0.0 && int_text_of_float(*f) == *i
            }
        }
    }
}

/// Exact decimal text of an integral finite float, in `PyValue::Int` form.
fn int_text_of_float(f: f64) -> String {
    let s = format!("{f:.0}");
    if s == "-0" { "0".into() } else { s }
}

/// A value nested deeper than this is walked on its own thread with `DEEP_STACK` bytes of stack.
pub const SHALLOW: usize = 1_000;
/// Room for json::MAX_DEPTH levels of the recursive walks (repr, dumps, loads) in a debug build.
pub const DEEP_STACK: usize = 64 << 20;

/// `f()` on a scoped thread with `DEEP_STACK` bytes of stack; a panic in it resumes here.
pub fn on_deep_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, f)
            .expect("the OS starts a thread")
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

fn join_repr(v: &[PyValue]) -> String {
    v.iter()
        .map(PyValue::repr_inner)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Date {
    pub fn isoformat(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Days since 0001-01-01 (proleptic Gregorian), as Python's `date.toordinal() - 1`.
    fn days(&self) -> i64 {
        let y = self.year as i64 - 1;
        let mut n = y * 365 + y / 4 - y / 100 + y / 400;
        for m in 1..self.month {
            n += days_in_month(self.year, m) as i64;
        }
        n + self.day as i64 - 1
    }
}

pub fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

impl DateTime {
    /// Python `isoformat(sep)`; `str(datetime)` uses `sep=' '`.
    pub fn isoformat(&self, sep: char) -> String {
        let mut s = format!(
            "{}{sep}{:02}:{:02}:{:02}",
            self.date.isoformat(),
            self.hour,
            self.minute,
            self.second
        );
        if self.microsecond != 0 {
            s.push_str(&format!(".{:06}", self.microsecond));
        }
        if let Some(off) = self.tz {
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.abs();
            s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60));
            if a % 60 != 0 {
                s.push_str(&format!(":{:02}", a % 60));
            }
        }
        s
    }

    fn repr(&self) -> String {
        let d = &self.date;
        let mut s = format!(
            "datetime.datetime({}, {}, {}, {}, {}",
            d.year, d.month, d.day, self.hour, self.minute
        );
        if self.microsecond != 0 {
            s.push_str(&format!(", {}, {}", self.second, self.microsecond));
        } else if self.second != 0 {
            s.push_str(&format!(", {}", self.second));
        }
        match self.tz {
            None => {}
            Some(0) => s.push_str(", tzinfo=datetime.timezone.utc"),
            Some(off) => s.push_str(&format!(
                ", tzinfo=datetime.timezone({})",
                timedelta_repr(off as i64)
            )),
        }
        s.push(')');
        s
    }

    fn utc_micros(&self) -> i128 {
        let secs = self.date.days() * 86400
            + (self.hour * 3600 + self.minute * 60 + self.second) as i64
            - self.tz.unwrap_or(0) as i64;
        secs as i128 * 1_000_000 + self.microsecond as i128
    }
}

impl Time {
    /// Python `time.isoformat()`: microseconds only when not 0.
    pub fn isoformat(&self) -> String {
        let mut s = format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second);
        if self.microsecond != 0 {
            s.push_str(&format!(".{:06}", self.microsecond));
        }
        s
    }

    fn repr(&self) -> String {
        let mut s = format!("datetime.time({}, {}", self.hour, self.minute);
        if self.microsecond != 0 {
            s.push_str(&format!(", {}, {}", self.second, self.microsecond));
        } else if self.second != 0 {
            s.push_str(&format!(", {}", self.second));
        }
        s.push(')');
        s
    }
}

/// Python `repr(timedelta(seconds=s))`.
pub fn timedelta_repr(s: i64) -> String {
    let days = s.div_euclid(86400);
    let secs = s.rem_euclid(86400);
    let mut parts = Vec::new();
    if days != 0 {
        parts.push(format!("days={days}"));
    }
    if secs != 0 {
        parts.push(format!("seconds={secs}"));
    }
    if parts.is_empty() {
        "datetime.timedelta(0)".into()
    } else {
        format!("datetime.timedelta({})", parts.join(", "))
    }
}

/// Python `repr(b)` of bytes.
pub fn repr_bytes(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::from("b");
    out.push(quote as char);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c as char);
            }
            c if !(0x20..0x7f).contains(&c) => out.push_str(&format!("\\x{c:02x}")),
            c => out.push(c as char),
        }
    }
    out.push(quote as char);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> PyValue {
        PyValue::Str(x.into())
    }

    #[test]
    fn str_repr_picks_quotes_and_escapes_like_python() {
        assert_eq!(s("it's").py_repr(), "\"it's\"");
        assert_eq!(s("a'b\"c").py_repr(), "'a\\'b\"c'");
        assert_eq!(
            s("\u{7}\u{200d}\u{a0}é😀\n").py_repr(),
            "'\\x07\\u200d\\xa0é😀\\n'"
        );
        assert_eq!(s("\u{e0001}").py_repr(), "'\\U000e0001'");
        assert_eq!(s("").py_repr(), "''");
        // Undecodable bytes are stored as private stand-in scalars corresponding to Python surrogateescape.
        assert_eq!(s("a\u{10FDFF}").py_repr(), "'a\\udcff'");
    }

    #[test]
    fn containers_and_dates_render_like_python() {
        let d = Date {
            year: 2020,
            month: 1,
            day: 2,
        };
        let dt = DateTime {
            date: d,
            hour: 3,
            minute: 4,
            second: 5,
            microsecond: 600,
            tz: Some(-19800),
        };
        let v = PyValue::Dict(vec![
            (
                s("a"),
                PyValue::List(vec![
                    PyValue::None,
                    PyValue::Bool(true),
                    PyValue::Float(1.0),
                ]),
            ),
            (PyValue::int(1), PyValue::Tuple(vec![s("x")])),
            (s("d"), PyValue::Date(d)),
            (s("t"), PyValue::DateTime(dt)),
            (s("b"), PyValue::Bytes(b"a'\x00".to_vec())),
            (s("e"), PyValue::Set(vec![])),
        ]);
        assert_eq!(
            v.py_str(),
            "{'a': [None, True, 1.0], 1: ('x',), 'd': datetime.date(2020, 1, 2), \
             't': datetime.datetime(2020, 1, 2, 3, 4, 5, 600, tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds=66600))), \
             'b': b\"a'\\x00\", 'e': set()}"
        );
        assert_eq!(
            PyValue::DateTime(dt).py_str(),
            "2020-01-02 03:04:05.000600-05:30"
        );
        assert_eq!(
            PyValue::DateTime(dt).isoformat().unwrap(),
            "2020-01-02T03:04:05.000600-05:30"
        );
        assert_eq!(PyValue::Date(d).isoformat().unwrap(), "2020-01-02");
        assert_eq!(s("x").isoformat(), None);
    }

    #[test]
    fn times_render_like_python() {
        // Python: repr/str of time(7, 32), time(7, 32, 5), time(0, 0, 0, 1)
        let t = |second, microsecond| {
            PyValue::Time(Time {
                hour: 7,
                minute: 32,
                second,
                microsecond,
            })
        };
        assert_eq!(t(0, 0).py_repr(), "datetime.time(7, 32)");
        assert_eq!(t(5, 0).py_repr(), "datetime.time(7, 32, 5)");
        assert_eq!(t(0, 1).py_repr(), "datetime.time(7, 32, 0, 1)");
        assert_eq!(t(0, 0).py_str(), "07:32:00");
        assert_eq!(t(5, 1).isoformat().unwrap(), "07:32:05.000001");
        assert_eq!(t(0, 0).tp_name(), "datetime.time");
        assert!(t(0, 0).key_eq(&t(0, 0)) && !t(0, 0).key_eq(&t(1, 0)));
        let big = PyValue::List(vec![PyValue::Dict(vec![(
            s("a"),
            PyValue::Int("9".repeat(4301)),
        )])]);
        assert_eq!(
            big.str_digits_ok().unwrap_err().python_text(),
            "ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        );
        assert!(
            PyValue::Int(format!("-{}", "9".repeat(4300)))
                .str_digits_ok()
                .is_ok()
        );
    }

    #[test]
    fn dict_keys_compare_across_number_types() {
        assert!(PyValue::int(1).key_eq(&PyValue::Bool(true)));
        assert!(PyValue::int(1).key_eq(&PyValue::Float(1.0)));
        assert!(!PyValue::int(1).key_eq(&s("1")));
        assert!(PyValue::Float(f64::NAN).key_eq(&PyValue::Float(f64::NAN)));
        let d = Date {
            year: 2020,
            month: 1,
            day: 1,
        };
        let utc = DateTime {
            date: d,
            hour: 7,
            minute: 0,
            second: 0,
            microsecond: 0,
            tz: Some(0),
        };
        let plus_five = DateTime {
            hour: 12,
            tz: Some(18000),
            ..utc
        };
        let naive = DateTime { tz: None, ..utc };
        assert!(PyValue::DateTime(utc).key_eq(&PyValue::DateTime(plus_five)));
        assert!(!PyValue::DateTime(utc).key_eq(&PyValue::DateTime(naive)));
        assert!(!PyValue::List(vec![]).is_hashable());
    }
}
