//! Python's `json.dumps` and `json.loads` as sm calls them (Python 3.15.1, C accelerators).
//!
//! `dumps` takes a `PyValue` (dict keys of any JSON-able type, big ints, tuples) or, through
//! `dumps_value`, a `serde_json::Value`. `loads` returns a `PyValue`: dicts keep the first position
//! of a repeated key and take its last value, ints have no size limit, `NaN` and `Infinity` parse.
//! A few call sites pass `indent=2` (cli/setup.rs), `sort_keys=True` (service/capture.rs) or the
//! defaults (service/recall.rs, service/mcp.rs); the rest pass `ensure_ascii=False`.

use crate::error::SmError;
use crate::py::float;
use crate::py::int::MAX_STR_DIGITS;
use crate::py::str::{STAND_IN_BASE, surrogate_byte};
use crate::py::value::{PyValue, type_name};

/// `json.dumps` keyword arguments. `Default` is Python's defaults.
#[derive(Clone, Debug)]
pub struct Opts {
    pub ensure_ascii: bool,
    pub allow_nan: bool,
    pub sort_keys: bool,
    /// The indent string (`indent=2` is two spaces; a negative int is ""); None is one line.
    pub indent: Option<String>,
    /// `(item_separator, key_separator)`; None is `(", ", ": ")`, or `(",", ": ")` with an indent.
    pub separators: Option<(String, String)>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            ensure_ascii: true,
            allow_nan: true,
            sort_keys: false,
            indent: None,
            separators: None,
        }
    }
}

impl Opts {
    /// `ensure_ascii=False`, the form most sm call sites use.
    pub fn unicode() -> Opts {
        Opts {
            ensure_ascii: false,
            ..Opts::default()
        }
    }

    /// `indent=n` as Python takes an int.
    pub fn with_indent(mut self, n: i64) -> Opts {
        self.indent = Some(" ".repeat(n.max(0) as usize));
        self
    }
}

/// `json.dumps(v, **opts)`.
pub fn dumps(v: &PyValue, opts: &Opts) -> Result<String, SmError> {
    let (item_sep, key_sep) = match &opts.separators {
        Some((a, b)) => (a.clone(), b.clone()),
        None if opts.indent.is_some() => (",".to_string(), ": ".to_string()),
        None => (", ".to_string(), ": ".to_string()),
    };
    let mut enc = Encoder {
        opts,
        item_sep,
        key_sep,
        out: String::new(),
    };
    if v.nesting() > SHALLOW {
        on_deep_stack(|| enc.value(v, 0))?;
    } else {
        enc.value(v, 0)?;
    }
    Ok(enc.out)
}

/// `json.dumps` of a serde_json value (keys are str; numbers keep int or float).
pub fn dumps_value(v: &serde_json::Value, opts: &Opts) -> String {
    dumps(&from_json(v), opts).expect("a serde_json value always has a JSON form")
}

/// A serde_json value as the Python value `json.loads` would give for it.
pub fn from_json(v: &serde_json::Value) -> PyValue {
    use serde_json::Value as J;
    match v {
        J::Null => PyValue::None,
        J::Bool(b) => PyValue::Bool(*b),
        J::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => PyValue::int(i),
            (None, Some(u)) => PyValue::Int(u.to_string()),
            _ => PyValue::Float(n.as_f64().expect("a serde_json number is i64, u64 or f64")),
        },
        J::String(s) => PyValue::Str(s.clone()),
        J::Array(a) => PyValue::List(a.iter().map(from_json).collect()),
        J::Object(m) => PyValue::Dict(
            m.iter()
                .map(|(k, v)| (PyValue::Str(k.clone()), from_json(v)))
                .collect(),
        ),
    }
}

struct Encoder<'a> {
    opts: &'a Opts,
    item_sep: String,
    key_sep: String,
    out: String,
}

/// `str(int)` of an int's decimal text, with Python's ValueError past `MAX_STR_DIGITS` digits.
pub fn int_text(s: &str) -> Result<String, SmError> {
    if s.trim_start_matches('-').len() > MAX_STR_DIGITS {
        return Err(SmError::value(format!(
            "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        )));
    }
    Ok(s.to_string())
}

impl Encoder<'_> {
    fn float(&self, x: f64) -> Result<String, SmError> {
        if x.is_finite() {
            return Ok(float::repr(x));
        }
        if !self.opts.allow_nan {
            return Err(SmError::value(format!(
                "Out of range float values are not JSON compliant: {}",
                float::repr(x)
            )));
        }
        Ok(if x.is_nan() {
            "NaN"
        } else if x > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_string())
    }

    fn newline(&mut self, depth: usize) {
        if let Some(ind) = &self.opts.indent {
            self.out.push('\n');
            for _ in 0..depth {
                self.out.push_str(ind);
            }
        }
    }

    fn value(&mut self, v: &PyValue, depth: usize) -> Result<(), SmError> {
        if depth >= MAX_DEPTH
            && matches!(v, PyValue::List(_) | PyValue::Tuple(_) | PyValue::Dict(_))
        {
            let kind = if matches!(v, PyValue::Dict(_)) {
                "object"
            } else {
                "array"
            };
            return Err(recursion("encoding", kind));
        }
        match v {
            PyValue::None => self.out.push_str("null"),
            PyValue::Bool(true) => self.out.push_str("true"),
            PyValue::Bool(false) => self.out.push_str("false"),
            PyValue::Int(s) => self.out.push_str(&int_text(s)?),
            PyValue::Float(x) => {
                let s = self.float(*x)?;
                self.out.push_str(&s);
            }
            PyValue::Str(s) => encode_str(s, self.opts.ensure_ascii, &mut self.out),
            PyValue::List(items) | PyValue::Tuple(items) => {
                if items.is_empty() {
                    self.out.push_str("[]");
                    return Ok(());
                }
                self.out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        let sep = self.item_sep.clone();
                        self.out.push_str(&sep);
                    }
                    self.newline(depth + 1);
                    self.value(it, depth + 1)?;
                }
                self.newline(depth);
                self.out.push(']');
            }
            PyValue::Dict(pairs) => {
                if pairs.is_empty() {
                    self.out.push_str("{}");
                    return Ok(());
                }
                let mut order: Vec<&(PyValue, PyValue)> = pairs.iter().collect();
                if self.opts.sort_keys {
                    sort_items(&mut order)?;
                }
                self.out.push('{');
                for (i, (k, val)) in order.into_iter().enumerate() {
                    let key = self.key(k)?;
                    if i > 0 {
                        let sep = self.item_sep.clone();
                        self.out.push_str(&sep);
                    }
                    self.newline(depth + 1);
                    encode_str(&key, self.opts.ensure_ascii, &mut self.out);
                    let sep = self.key_sep.clone();
                    self.out.push_str(&sep);
                    self.value(val, depth + 1)?;
                }
                self.newline(depth);
                self.out.push('}');
            }
            PyValue::Cycle(_) => return Err(SmError::value("Circular reference detected")),
            other => {
                return Err(SmError::type_(format!(
                    "Object of type {} is not JSON serializable",
                    type_name(other)
                )));
            }
        }
        Ok(())
    }

    /// A dict key as str: float repr, `true`/`false`/`null`, int text (C encoder order).
    fn key(&self, k: &PyValue) -> Result<String, SmError> {
        match k {
            PyValue::Str(s) => Ok(s.clone()),
            PyValue::Float(x) => self.float(*x),
            PyValue::Bool(b) => Ok(if *b { "true" } else { "false" }.into()),
            PyValue::None => Ok("null".into()),
            PyValue::Int(s) => int_text(s),
            other => Err(SmError::type_(format!(
                "keys must be str, int, float, bool or None, not {}",
                type_name(other)
            ))),
        }
    }
}

/// The order class of a key under Python's `<`: strs compare with strs, numbers with numbers.
#[derive(PartialEq)]
enum KeyKind {
    Str,
    Num,
    Other,
}

fn key_kind(k: &PyValue) -> KeyKind {
    match k {
        PyValue::Str(_) => KeyKind::Str,
        PyValue::Bool(_) | PyValue::Int(_) | PyValue::Float(_) => KeyKind::Num,
        _ => KeyKind::Other,
    }
}

fn num_of(k: &PyValue) -> f64 {
    match k {
        PyValue::Bool(b) => f64::from(u8::from(*b)),
        PyValue::Int(s) => s.parse::<f64>().unwrap_or(0.0),
        PyValue::Float(x) => *x,
        _ => 0.0,
    }
}

/// `sorted(dct.items())` (sort_keys). Keys of one kind sort; mixed kinds raise the TypeError of
/// the first pair `list.sort` compares, `items[i+1] < items[i]` along the opening run.
fn sort_items(order: &mut [&(PyValue, PyValue)]) -> Result<(), SmError> {
    for w in order.windows(2) {
        let (a, b) = (&w[0].0, &w[1].0);
        if key_kind(a) != key_kind(b) || key_kind(a) == KeyKind::Other {
            return Err(SmError::type_(format!(
                "'<' not supported between instances of '{}' and '{}'",
                type_name(b),
                type_name(a)
            )));
        }
    }
    match order.first().map(|p| key_kind(&p.0)) {
        Some(KeyKind::Str) => order.sort_by(|a, b| match (&a.0, &b.0) {
            (PyValue::Str(x), PyValue::Str(y)) => x.chars().cmp(y.chars()),
            _ => std::cmp::Ordering::Equal,
        }),
        Some(KeyKind::Num) => order.sort_by(|a, b| {
            num_of(&a.0)
                .partial_cmp(&num_of(&b.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => {}
    }
    Ok(())
}

/// One JSON string literal (`encode_basestring` or `encode_basestring_ascii`). An undecodable-byte
/// stand-in maps to Python's lone surrogate `\udcXX`: escaped under ensure_ascii, kept as the
/// stand-in otherwise (the UTF-8 write then fails as in Python).
pub fn encode_str(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if !ensure_ascii || (' '..='~').contains(&c) => out.push(c),
            c => {
                if let Some(b) = surrogate_byte(c) {
                    out.push_str(&format!("\\udc{b:02x}"));
                    continue;
                }
                let n = c as u32;
                if n < 0x10000 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    let v = n - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 | (v >> 10),
                        0xdc00 | (v & 0x3ff)
                    ));
                }
            }
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------- loads

/// `json.loads(s)` of a str.
/// The deepest array or object nesting `loads` and `dumps` take; one more raises RecursionError.
/// Python 3.15 has no fixed depth: its C scanner stops when the 8 MiB stack runs out (50,000 deep
/// parses, 100,000 does not), so the depth and the message are an accepted difference.
pub const MAX_DEPTH: usize = 10_000;

use crate::py::value::{SHALLOW, on_deep_stack};

fn recursion(what: &str, kind: &str) -> SmError {
    SmError::new(
        crate::error::Base::Runtime,
        "RecursionError",
        format!("maximum nesting depth ({MAX_DEPTH}) exceeded while {what} a JSON {kind}"),
    )
}

/// An upper bound of the nesting: brackets outside strings, never decremented below 0.
fn nesting(c: &[char]) -> usize {
    let (mut depth, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for &ch in c {
        match (in_str, esc, ch) {
            (true, true, _) => esc = false,
            (true, false, '\\') => esc = true,
            (true, false, '"') => in_str = false,
            (true, ..) => {}
            (false, _, '"') => in_str = true,
            (false, _, '[' | '{') => {
                depth += 1;
                max = max.max(depth);
            }
            (false, _, ']' | '}') => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

pub fn loads(s: &str) -> Result<PyValue, SmError> {
    let chars: Vec<char> = s.chars().collect();
    // A document nested past SHALLOW parses on a thread with room for MAX_DEPTH levels.
    if nesting(&chars) <= SHALLOW {
        return loads_chars(s, &chars);
    }
    on_deep_stack(|| loads_chars(s, &chars))
}

fn loads_chars(s: &str, chars: &[char]) -> Result<PyValue, SmError> {
    let d = Decoder {
        s,
        c: chars,
        depth: std::cell::Cell::new(0),
    };
    if chars.first() == Some(&'\u{feff}') {
        return Err(d.err("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    let idx = d.ws(0);
    let (obj, end) = match d.scan_once(idx)? {
        Ok(r) => r,
        Err(Stop(at)) => return Err(d.err("Expecting value", at)),
    };
    let end = d.ws(end);
    if end != chars.len() {
        return Err(d.err("Extra data", end));
    }
    Ok(obj)
}

/// `StopIteration(idx)`: no value starts at `idx`.
struct Stop(usize);

type Scan = Result<Result<(PyValue, usize), Stop>, SmError>;

struct Decoder<'a> {
    s: &'a str,
    c: &'a [char],
    depth: std::cell::Cell<usize>,
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl Decoder<'_> {
    fn err(&self, msg: &str, pos: usize) -> SmError {
        SmError::json_decode(msg, self.s, pos)
    }

    fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }

    fn ws(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(is_ws) {
            i += 1;
        }
        i
    }

    fn starts(&self, i: usize, word: &str) -> bool {
        let w: Vec<char> = word.chars().collect();
        self.c.get(i..i + w.len()) == Some(&w[..])
    }

    fn nested(
        &self,
        kind: &str,
        f: impl FnOnce() -> Result<(PyValue, usize), SmError>,
    ) -> Result<(PyValue, usize), SmError> {
        let depth = self.depth.get() + 1;
        if depth > MAX_DEPTH {
            return Err(recursion(
                "decoding",
                &format!("{kind} from a unicode string"),
            ));
        }
        self.depth.set(depth);
        let r = f();
        self.depth.set(depth - 1);
        r
    }

    fn scan_once(&self, idx: usize) -> Scan {
        let Some(c) = self.at(idx) else {
            return Ok(Err(Stop(idx)));
        };
        let done = |v: PyValue, end: usize| Ok(Ok((v, end)));
        match c {
            '"' => {
                let (s, end) = self.scanstring(idx + 1)?;
                done(PyValue::Str(s), end)
            }
            '{' => self.nested("object", || self.object(idx + 1)).map(Ok),
            '[' => self.nested("array", || self.array(idx + 1)).map(Ok),
            'n' if self.starts(idx, "null") => done(PyValue::None, idx + 4),
            't' if self.starts(idx, "true") => done(PyValue::Bool(true), idx + 4),
            'f' if self.starts(idx, "false") => done(PyValue::Bool(false), idx + 5),
            'N' if self.starts(idx, "NaN") => done(PyValue::Float(f64::NAN), idx + 3),
            'I' if self.starts(idx, "Infinity") => done(PyValue::Float(f64::INFINITY), idx + 8),
            '-' if self.starts(idx, "-Infinity") => {
                done(PyValue::Float(f64::NEG_INFINITY), idx + 9)
            }
            _ => self.number(idx),
        }
    }

    /// `_match_number_unicode`: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`, ASCII digits.
    fn number(&self, start: usize) -> Scan {
        let digit = |i: usize| self.at(i).is_some_and(|c| c.is_ascii_digit());
        let mut i = start;
        if self.at(i) == Some('-') {
            i += 1;
        }
        match self.at(i) {
            Some('1'..='9') => {
                i += 1;
                while digit(i) {
                    i += 1;
                }
            }
            Some('0') => i += 1,
            _ => return Ok(Err(Stop(start))),
        }
        let mut is_float = false;
        if self.at(i) == Some('.') && digit(i + 1) {
            is_float = true;
            i += 2;
            while digit(i) {
                i += 1;
            }
        }
        if matches!(self.at(i), Some('e' | 'E')) {
            let e_start = i;
            i += 1;
            if matches!(self.at(i), Some('-' | '+')) {
                i += 1;
            }
            let digits_from = i;
            while digit(i) {
                i += 1;
            }
            if i > digits_from {
                is_float = true;
            } else {
                i = e_start;
            }
        }
        let text: String = self.c[start..i].iter().collect();
        let v = if is_float {
            PyValue::Float(float::from_str(&text)?)
        } else {
            PyValue::Int(crate::py::int::from_str(&text)?.text())
        };
        Ok(Ok((v, i)))
    }

    /// `scanstring(s, end, strict=True)`: `end` is just past the opening quote.
    fn scanstring(&self, mut end: usize) -> Result<(String, usize), SmError> {
        let begin = end - 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.at(end) else {
                return Err(self.err("Unterminated string starting at", begin));
            };
            end += 1;
            match c {
                '"' => return Ok((out, end)),
                '\\' => {}
                c if (c as u32) < 0x20 => {
                    return Err(self.err("Invalid control character at", end - 1));
                }
                c => {
                    out.push(c);
                    continue;
                }
            }
            let Some(esc) = self.at(end) else {
                return Err(self.err("Unterminated string starting at", begin));
            };
            if esc != 'u' {
                let ch = match esc {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\x08',
                    'f' => '\x0c',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    // The C scanner points at the backslash and does not name the char.
                    _ => return Err(self.err("Invalid \\escape", end - 1)),
                };
                out.push(ch);
                end += 1;
                continue;
            }
            let mut uni = self.hex4(end)?;
            end += 5;
            if (0xd800..=0xdbff).contains(&uni)
                && self.at(end) == Some('\\')
                && self.at(end + 1) == Some('u')
            {
                let uni2 = self.hex4(end + 1)?;
                if (0xdc00..=0xdfff).contains(&uni2) {
                    uni = 0x10000 + (((uni - 0xd800) << 10) | (uni2 - 0xdc00));
                    end += 6;
                }
            }
            out.push(surrogate_char(uni));
        }
    }

    /// The 4 hex digits after the `u` at `pos`.
    fn hex4(&self, pos: usize) -> Result<u32, SmError> {
        let hex = self
            .c
            .get(pos + 1..pos + 5)
            .filter(|h| h.iter().all(|c| c.is_ascii_hexdigit()));
        match hex {
            Some(h) => {
                Ok(u32::from_str_radix(&h.iter().collect::<String>(), 16).expect("4 hex digits"))
            }
            None => Err(self.err("Invalid \\uXXXX escape", pos)),
        }
    }

    fn object(&self, mut end: usize) -> Result<(PyValue, usize), SmError> {
        let mut pairs: Vec<(PyValue, PyValue)> = Vec::new();
        let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut next = self.at(end);
        if next != Some('"') {
            if next.is_some_and(is_ws) {
                end = self.ws(end);
                next = self.at(end);
            }
            if next == Some('}') {
                return Ok((PyValue::Dict(pairs), end + 1));
            }
            if next != Some('"') {
                return Err(self.err("Expecting property name enclosed in double quotes", end));
            }
        }
        end += 1;
        loop {
            let (key, e) = self.scanstring(end)?;
            end = e;
            if self.at(end) != Some(':') {
                end = self.ws(end);
                if self.at(end) != Some(':') {
                    return Err(self.err("Expecting ':' delimiter", end));
                }
            }
            end = self.ws(end + 1);
            let (value, e) = match self.scan_once(end)? {
                Ok(r) => r,
                Err(Stop(at)) => return Err(self.err("Expecting value", at)),
            };
            end = e;
            // A repeated key keeps its first place and takes the last value (dict insertion).
            match slots.get(&key) {
                Some(&at) => pairs[at].1 = value,
                None => {
                    slots.insert(key.clone(), pairs.len());
                    pairs.push((PyValue::Str(key), value));
                }
            }
            end = self.ws(end);
            let next = self.at(end);
            end += 1;
            if next == Some('}') {
                break;
            }
            if next != Some(',') {
                return Err(self.err("Expecting ',' delimiter", end - 1));
            }
            let comma = end - 1;
            end = self.ws(end);
            let next = self.at(end);
            end += 1;
            if next != Some('"') {
                if next == Some('}') {
                    return Err(self.err("Illegal trailing comma before end of object", comma));
                }
                return Err(self.err("Expecting property name enclosed in double quotes", end - 1));
            }
        }
        Ok((PyValue::Dict(pairs), end))
    }

    fn array(&self, mut end: usize) -> Result<(PyValue, usize), SmError> {
        let mut values = Vec::new();
        end = self.ws(end);
        if self.at(end) == Some(']') {
            return Ok((PyValue::List(values), end + 1));
        }
        loop {
            let (value, e) = match self.scan_once(end)? {
                Ok(r) => r,
                Err(Stop(at)) => return Err(self.err("Expecting value", at)),
            };
            values.push(value);
            end = self.ws(e);
            let next = self.at(end);
            end += 1;
            if next == Some(']') {
                break;
            }
            if next != Some(',') {
                return Err(self.err("Expecting ',' delimiter", end - 1));
            }
            let comma = end - 1;
            end = self.ws(end);
            if self.at(end) == Some(']') {
                return Err(self.err("Illegal trailing comma before end of array", comma));
            }
        }
        Ok((PyValue::List(values), end))
    }
}

/// `chr(n)` for a `\u` escape: a lone surrogate U+DC80-U+DCFF maps to its stand-in byte scalar;
/// any other lone surrogate, which a Rust `char` cannot hold, becomes U+FFFD.
pub fn surrogate_char(n: u32) -> char {
    match n {
        0xdc80..=0xdcff => char::from_u32(STAND_IN_BASE + (n - 0xdc00)).expect("stand-in range"),
        _ => char::from_u32(n).unwrap_or('\u{fffd}'),
    }
}

// ---------------------------------------------------------------- parity

/// A parity argument as the Python value the Python parity reference decodes:
/// `{"__float": ...}`, `{"__int": text}`, `{"__bytes": hex}`, `{"__tuple": [...]}`,
/// `{"__set": [...]}`, `{"__dict": [[k, v], ...]}` (any key type); other objects are str-keyed dicts.
#[cfg(test)]
pub fn protocol_value(v: &serde_json::Value) -> Result<PyValue, SmError> {
    use serde_json::Value as J;
    let bad = || SmError::type_(format!("parity: no Python value for {v}"));
    Ok(match v {
        J::Array(a) => PyValue::List(a.iter().map(protocol_value).collect::<Result<_, _>>()?),
        J::Object(m) if m.len() == 1 => {
            let (k, inner) = m.iter().next().expect("one entry");
            let list = || inner.as_array().ok_or_else(bad);
            match k.as_str() {
                "__float" => PyValue::Float(crate::parity::value_float(v).ok_or_else(bad)?),
                "__int" => PyValue::Int(inner.as_str().ok_or_else(bad)?.to_string()),
                "__bytes" => {
                    let h = inner.as_str().ok_or_else(bad)?;
                    let b = (0..h.len() / 2)
                        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| bad()))
                        .collect::<Result<_, _>>()?;
                    PyValue::Bytes(b)
                }
                "__tuple" => PyValue::Tuple(
                    list()?
                        .iter()
                        .map(protocol_value)
                        .collect::<Result<_, _>>()?,
                ),
                "__set" => PyValue::Set(
                    list()?
                        .iter()
                        .map(protocol_value)
                        .collect::<Result<_, _>>()?,
                ),
                "__dict" => {
                    let mut pairs = Vec::new();
                    for p in list()? {
                        let kv = p.as_array().filter(|kv| kv.len() == 2).ok_or_else(bad)?;
                        pairs.push((protocol_value(&kv[0])?, protocol_value(&kv[1])?));
                    }
                    PyValue::Dict(pairs)
                }
                _ => PyValue::Dict(vec![(PyValue::Str(k.clone()), protocol_value(inner)?)]),
            }
        }
        J::Object(m) => PyValue::Dict(
            m.iter()
                .map(|(k, x)| Ok((PyValue::Str(k.clone()), protocol_value(x)?)))
                .collect::<Result<_, SmError>>()?,
        ),
        other => from_json(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: PyValue, o: &Opts) -> String {
        dumps(&v, o).unwrap_or_else(|e| e.python_text())
    }

    fn s(x: &str) -> PyValue {
        PyValue::Str(x.into())
    }

    fn l(x: &str) -> String {
        loads(x)
            .map(|v| v.py_repr())
            .unwrap_or_else(|e| e.python_text())
    }

    #[test]
    fn dumps_defaults_and_ensure_ascii() {
        let v = PyValue::Dict(vec![
            (s("c"), s("é\u{2028}\x7f\x00😀")),
            (
                s("a"),
                PyValue::List(vec![PyValue::int(1), PyValue::Float(0.1)]),
            ),
        ]);
        assert_eq!(
            d(v.clone(), &Opts::default()),
            r#"{"c": "\u00e9\u2028\u007f\u0000\ud83d\ude00", "a": [1, 0.1]}"#
        );
        assert_eq!(
            d(v, &Opts::unicode()),
            "{\"c\": \"é\u{2028}\x7f\\u0000😀\", \"a\": [1, 0.1]}"
        );
        assert_eq!(
            d(s("\"\\\n\r\t\x08\x0c\x1f"), &Opts::default()),
            r#""\"\\\n\r\t\b\f\u001f""#
        );
    }

    #[test]
    fn dumps_keys_floats_and_errors() {
        let v = PyValue::Dict(vec![
            (PyValue::Float(1.5), PyValue::int(1)),
            (PyValue::Bool(true), PyValue::int(2)),
            (PyValue::None, PyValue::int(3)),
            (
                PyValue::Int("100000000000000000000".into()),
                PyValue::int(4),
            ),
            (PyValue::Float(f64::NAN), PyValue::int(5)),
            (PyValue::Float(1e16), PyValue::int(6)),
        ]);
        assert_eq!(
            d(v, &Opts::default()),
            r#"{"1.5": 1, "true": 2, "null": 3, "100000000000000000000": 4, "NaN": 5, "1e+16": 6}"#
        );
        assert_eq!(
            d(
                PyValue::List(vec![PyValue::Float(f64::NEG_INFINITY)]),
                &Opts::default()
            ),
            "[-Infinity]"
        );
        let strict = Opts {
            allow_nan: false,
            ..Opts::default()
        };
        assert_eq!(
            d(PyValue::Float(f64::NAN), &strict),
            "ValueError: Out of range float values are not JSON compliant: nan"
        );
        let tk = PyValue::Dict(vec![(
            PyValue::Tuple(vec![PyValue::int(1)]),
            PyValue::int(1),
        )]);
        assert_eq!(
            d(tk, &Opts::default()),
            "TypeError: keys must be str, int, float, bool or None, not tuple"
        );
        assert_eq!(
            d(PyValue::Set(vec![]), &Opts::default()),
            "TypeError: Object of type set is not JSON serializable"
        );
        assert_eq!(
            d(PyValue::Bytes(vec![]), &Opts::default()),
            "TypeError: Object of type bytes is not JSON serializable"
        );
        assert!(
            d(PyValue::Int("1".repeat(4301)), &Opts::default())
                .starts_with("ValueError: Exceeds the limit (4300 digits)")
        );
        assert_eq!(
            d(PyValue::Int("1".repeat(4300)), &Opts::default()).len(),
            4300
        );
        assert_eq!(
            d(PyValue::Cycle("[...]"), &Opts::default()),
            "ValueError: Circular reference detected"
        );
    }

    #[test]
    fn dumps_indent_sort_and_separators() {
        let inner = PyValue::Dict(vec![(s("b"), PyValue::List(vec![]))]);
        let v = PyValue::Dict(vec![(
            s("a"),
            PyValue::List(vec![PyValue::int(1), inner, PyValue::Dict(vec![])]),
        )]);
        assert_eq!(
            d(v, &Opts::default().with_indent(2)),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": []\n    },\n    {}\n  ]\n}"
        );
        let two = PyValue::Dict(vec![(s("x"), PyValue::int(1))]);
        assert_eq!(
            d(two.clone(), &Opts::default().with_indent(-1)),
            "{\n\"x\": 1\n}"
        );
        assert_eq!(
            d(
                two,
                &Opts {
                    indent: Some("\t".into()),
                    ..Opts::default()
                }
            ),
            "{\n\t\"x\": 1\n}"
        );
        let compact = Opts {
            separators: Some((",".into(), ":".into())),
            ..Opts::default()
        };
        assert_eq!(
            d(
                PyValue::List(vec![PyValue::int(1), PyValue::List(vec![PyValue::int(2)])]),
                &compact
            ),
            "[1,[2]]"
        );
        let keys = PyValue::Dict(vec![
            (s("b"), PyValue::int(1)),
            (s("a"), PyValue::int(2)),
            (s("A"), PyValue::int(3)),
            (s("é"), PyValue::int(4)),
        ]);
        let sorted = Opts {
            sort_keys: true,
            ..Opts::default()
        };
        assert_eq!(d(keys, &sorted), r#"{"A": 3, "a": 2, "b": 1, "\u00e9": 4}"#);
        let mixed = PyValue::Dict(vec![
            (PyValue::int(1), PyValue::int(1)),
            (s("a"), PyValue::int(2)),
        ]);
        assert_eq!(
            d(mixed, &sorted),
            "TypeError: '<' not supported between instances of 'str' and 'int'"
        );
        let nums = PyValue::Dict(vec![
            (PyValue::int(2), PyValue::int(1)),
            (PyValue::int(1), PyValue::int(2)),
        ]);
        assert_eq!(d(nums, &sorted), r#"{"1": 2, "2": 1}"#);
    }

    #[test]
    fn dumps_stand_in_is_a_lone_surrogate() {
        let sb = char::from_u32(0x10FDFF).unwrap().to_string();
        assert_eq!(d(s(&sb), &Opts::default()), r#""\udcff""#);
        assert_eq!(d(s(&sb), &Opts::unicode()), format!("\"{sb}\""));
    }

    #[test]
    fn loads_values() {
        assert_eq!(
            l(r#"{"a": 1, "b": [1.5, -0.0, 1e400, true, null], "a": 2}"#),
            "{'a': 2, 'b': [1.5, -0.0, inf, True, None]}"
        );
        assert_eq!(l("[NaN, Infinity, -Infinity]"), "[nan, inf, -inf]");
        assert_eq!(
            l("123456789012345678901234567890"),
            "123456789012345678901234567890"
        );
        assert_eq!(l(r#""\ud83d\ude00\u00e9\/""#), "'😀é/'");
        assert_eq!(l(r#""\udcff""#), "'\\udcff'");
        assert_eq!(l(" \t\n\r1 \r\n"), "1");
        assert_eq!(l("1E+2"), "100.0");
        assert_eq!(l("-0"), "0");
    }

    #[test]
    fn loads_errors_match_cpython() {
        let cases = [
            ("", "Expecting value: line 1 column 1 (char 0)"),
            (
                "\u{feff}1",
                "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)",
            ),
            (
                "{x",
                "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            ),
            (
                "{\"a\" 1}",
                "Expecting ':' delimiter: line 1 column 6 (char 5)",
            ),
            (
                "{\"a\": 1 \"b\"}",
                "Expecting ',' delimiter: line 1 column 9 (char 8)",
            ),
            (
                "{\"a\": 1,}",
                "Illegal trailing comma before end of object: line 1 column 8 (char 7)",
            ),
            (
                "[1,]",
                "Illegal trailing comma before end of array: line 1 column 3 (char 2)",
            ),
            ("[1 2]", "Expecting ',' delimiter: line 1 column 4 (char 3)"),
            (
                "\"abc",
                "Unterminated string starting at: line 1 column 1 (char 0)",
            ),
            (
                "\"a\nb\"",
                "Invalid control character at: line 1 column 3 (char 2)",
            ),
            ("\"\\x\"", "Invalid \\escape: line 1 column 2 (char 1)"),
            (
                "\"\\u12\"",
                "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
            ),
            ("1 2", "Extra data: line 1 column 3 (char 2)"),
            ("-", "Expecting value: line 1 column 1 (char 0)"),
            ("[\n  ,", "Expecting value: line 2 column 3 (char 4)"),
            ("é[", "Expecting value: line 1 column 1 (char 0)"),
            ("[é", "Expecting value: line 1 column 2 (char 1)"),
        ];
        for (input, want) in cases {
            assert_eq!(l(input), format!("JSONDecodeError: {want}"), "{input:?}");
        }
        assert!(l(&"1".repeat(4301)).starts_with("ValueError: Exceeds the limit (4300 digits)"));
    }

    #[test]
    fn dumps_value_of_serde_json() {
        let v = serde_json::json!({"ts": "x", "ids": ["a"], "n": 1, "f": 2.0, "big": u64::MAX, "repo": null});
        assert_eq!(
            dumps_value(&v, &Opts::unicode()),
            r#"{"ts": "x", "ids": ["a"], "n": 1, "f": 2.0, "big": 18446744073709551615, "repo": null}"#
        );
    }

    #[test]
    fn loads_an_object_with_many_keys_in_linear_time() {
        // 20,000 distinct keys cost 1.85 s on a debug build while each key scanned the pairs so far.
        // Python takes milliseconds; the bound leaves room for a loaded machine.
        let body: Vec<String> = (0..20_000).map(|i| format!("\"k{i}\":{i}")).collect();
        let started = std::time::Instant::now();
        let PyValue::Dict(pairs) = loads(&format!("{{{}}}", body.join(","))).unwrap() else {
            panic!("a dict")
        };
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
        assert_eq!(pairs.len(), 20_000);
        assert_eq!(pairs[19_999], (s("k19999"), PyValue::int(19_999)));
    }

    #[test]
    fn a_repeated_key_keeps_its_first_place_and_its_last_value() {
        // Python 3.15.1: json.loads('{"b":1,"a":2,"b":3,"c":4,"a":5}') is {'b': 3, 'a': 5, 'c': 4}.
        assert_eq!(
            l(r#"{"b":1,"a":2,"b":3,"c":4,"a":5}"#),
            "{'b': 3, 'a': 5, 'c': 4}"
        );
        assert_eq!(l(r#"{"a":{"x":1,"x":2},"a":{"y":3}}"#), "{'a': {'y': 3}}");
    }
}

#[cfg(test)]
mod depth {
    use super::*;

    /// `f` on an 8 MiB thread, the main thread's stack, so a walk that needs more aborts here.
    fn on_8mib<T: Send>(f: impl FnOnce() -> T + Send) -> T {
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(8 << 20)
                .spawn_scoped(s, f)
                .unwrap()
                .join()
                .unwrap()
        })
    }

    fn nested(n: usize, open: &str, close: &str) -> String {
        format!("{}{}", open.repeat(n), close.repeat(n))
    }

    #[test]
    fn deep_documents_parse_repr_dump_and_drop_on_the_main_stack() {
        for n in [128, 129, 1_001, 5_000, MAX_DEPTH] {
            let doc = nested(n, "[", "]");
            let (repr_len, dumped) = on_8mib(|| {
                let v = loads(&doc).unwrap();
                (v.py_repr().len(), dumps(&v, &Opts::default()).unwrap())
            });
            assert_eq!(repr_len, 2 * n);
            assert_eq!(dumped, doc);
        }
        let obj = format!("{}1{}", "{\"a\": ".repeat(5_000), "}".repeat(5_000));
        assert!(on_8mib(|| loads(&obj).is_ok()));
    }

    #[test]
    fn past_max_depth_is_a_recursion_error_never_an_abort() {
        for n in [MAX_DEPTH + 1, 100_000, 1_000_000] {
            let doc = nested(n, "[", "]");
            let err = on_8mib(|| loads(&doc).unwrap_err());
            assert_eq!(
                err.python_text(),
                "RecursionError: maximum nesting depth (10000) exceeded while decoding a JSON array from a unicode string"
            );
        }
        let obj = format!("{}1", "{\"a\": ".repeat(MAX_DEPTH + 1));
        assert!(
            on_8mib(|| loads(&obj).unwrap_err())
                .msg
                .contains("a JSON object from")
        );
        // A value built deeper than MAX_DEPTH (not from loads) fails dumps the same way.
        // Built and dropped on the 8 MiB thread: the test thread's 2 MiB cannot drop it.
        let err = on_8mib(|| {
            let mut v = PyValue::List(vec![]);
            for _ in 0..MAX_DEPTH {
                v = PyValue::List(vec![v]);
            }
            dumps(&v, &Opts::default()).unwrap_err()
        });
        assert_eq!(
            err.python_text(),
            "RecursionError: maximum nesting depth (10000) exceeded while encoding a JSON array"
        );
        // Brackets inside strings do not count toward the nesting.
        assert_eq!(nesting(&"[\"[[[{\\\"\"]".chars().collect::<Vec<_>>()), 1);
    }
}
