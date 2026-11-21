//! Port of Python 3.15.1's tomllib (Lib/tomllib/{_parser,_re,_types,__init__}.py): `loads` with
//! the default `parse_float=float`, giving the values and the TOMLDecodeError text Python gives,
//! except that a key reads as config.toml writes it (`a.b`, not `('a', 'b')`) and a table declared
//! twice and an invalid value say what to change.
//!
//! Positions: tomllib indexes its str by code point; this port keeps byte offsets into the same
//! text and steps by char, so each offset names the same char. An error turns its offset into
//! tomllib's line and column (counted in chars) when it is raised.
//!
//! Recursion: Python raises RecursionError when its frame count passes the recursion limit, which
//! depends on how deep the caller already is. The port counts the parser's own frames (2 per
//! array, 3 per inline table) and raises the same error past `FRAME_BUDGET`, so it never runs out
//! of stack; the exact depth is a listed difference.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::error::{Base, SmError};
use crate::py::str::PyRegex;
use crate::py::value::{Date, DateTime, PyValue, Time, days_in_month};

/// `sys.getrecursionlimit()` default, which tomllib takes as `MAX_KEY_PARTS`.
const MAX_KEY_PARTS: usize = 1000;
/// Parser frames allowed before RecursionError (module doc).
const FRAME_BUDGET: usize = 990;

type Key = Vec<String>;

/// A parsed value: tables and arrays stay mutable until `loads` returns.
#[derive(Debug)]
enum Node {
    Table(Table),
    Array(Vec<Node>),
    Leaf(PyValue),
}

type Table = IndexMap<String, Node>;

impl Node {
    fn is_container(&self) -> bool {
        matches!(self, Node::Table(_) | Node::Array(_))
    }

    fn into_py(self) -> PyValue {
        match self {
            Node::Table(t) => table_to_py(t),
            Node::Array(a) => PyValue::List(a.into_iter().map(Node::into_py).collect()),
            Node::Leaf(v) => v,
        }
    }
}

fn table_to_py(t: Table) -> PyValue {
    PyValue::Dict(
        t.into_iter()
            .map(|(k, v)| (PyValue::Str(k), v.into_py()))
            .collect(),
    )
}

/// A key as config.toml writes it: its parts joined by dots.
fn key_repr(key: &[String]) -> String {
    key.join(".")
}

fn is_ascii_ctrl(c: char) -> bool {
    (c as u32) < 32 || c == '\u{7f}'
}

fn illegal_basic(c: char) -> bool {
    is_ascii_ctrl(c) && c != '\t'
}

fn illegal_multiline(c: char) -> bool {
    is_ascii_ctrl(c) && c != '\t' && c != '\n'
}

fn toml_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn toml_ws_nl(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

fn bare_key(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn key_initial(c: char) -> bool {
    bare_key(c) || c == '"' || c == '\''
}

/// `TOMLDecodeError(msg, doc, pos)`: `str()` is `msg (at line L, column C)`, or
/// `msg (at end of document)` when `pos` is at or past the end.
fn decode_error(msg: impl Into<String>, doc: &str, pos: usize) -> SmError {
    let msg = msg.into();
    let at = if pos >= doc.len() {
        "end of document".to_string()
    } else {
        let before = &doc[..pos];
        let lineno = before.matches('\n').count() + 1;
        let colno = match before.rfind('\n') {
            None => before.chars().count() + 1,
            Some(nl) => before[nl..].chars().count(),
        };
        format!("line {lineno}, column {colno}")
    };
    SmError::new(Base::Value, "TOMLDecodeError", format!("{msg} (at {at})"))
}

fn recursion_error(msg: impl Into<String>) -> SmError {
    SmError::from_python("RecursionError", msg)
}

// ---------------------------------------------------------------- _re.py

const TIME_RE: &str = r"([01][0-9]|2[0-3]):([0-5][0-9])(?::([0-5][0-9])(?:\.([0-9]{1,6})[0-9]*)?)?";

/// `RE_NUMBER`, `RE_LOCALTIME` and `RE_DATETIME` without VERBOSE's spaces and comments, each
/// anchored with `\A` and run on the text from `pos` (`pattern.match(src, pos)`; none of them
/// looks behind).
static RE_NUMBER: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(
        r"\A(?:0(?:x[0-9A-Fa-f](?:_?[0-9A-Fa-f])*|b[01](?:_?[01])*|o[0-7](?:_?[0-7])*)|[+-]?(?:0|[1-9](?:_?[0-9])*)((?:\.[0-9](?:_?[0-9])*)?(?:[eE][+-]?[0-9](?:_?[0-9])*)?))",
    )
    .expect("RE_NUMBER compiles")
});
static RE_LOCALTIME: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(&format!(r"\A(?:{TIME_RE})")).expect("RE_LOCALTIME compiles"));
static RE_DATETIME: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(&format!(
        r"\A(?:([0-9]{{4}})-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])(?:[Tt ]{TIME_RE}(?:([Zz])|([+-])([01][0-9]|2[0-3]):([0-5][0-9]))?)?)"
    ))
    .expect("RE_DATETIME compiles")
});

fn num(s: &str) -> u32 {
    s.parse().expect("the pattern matched ASCII digits")
}

/// `int(micros_str.ljust(6, "0"))`.
fn micros(s: Option<&str>) -> u32 {
    s.map_or(0, |m| num(&format!("{m:0<6}")))
}

/// `match_to_datetime`: a date, or a datetime with its offset; `Err(())` where `date()` or
/// `datetime()` raises ValueError.
fn match_to_datetime(g: &[Option<&str>]) -> Result<PyValue, ()> {
    let (year, month, day) = (
        num(g[1].unwrap_or("0")) as i32,
        num(g[2].unwrap_or("0")),
        num(g[3].unwrap_or("0")),
    );
    if year < 1 || day > days_in_month(year, month) {
        return Err(());
    }
    let date = Date { year, month, day };
    let Some(hour) = g[4] else {
        return Ok(PyValue::Date(date));
    };
    let tz = if let Some(sign) = g[9] {
        let sign = if sign == "+" { 1 } else { -1 };
        Some(
            sign * (num(g[10].unwrap_or("0")) as i32 * 3600
                + num(g[11].unwrap_or("0")) as i32 * 60),
        )
    } else if g[8].is_some() {
        Some(0)
    } else {
        None
    };
    Ok(PyValue::DateTime(DateTime {
        date,
        hour: num(hour),
        minute: num(g[5].unwrap_or("0")),
        second: g[6].map_or(0, num),
        microsecond: micros(g[7]),
        tz,
    }))
}

/// `int(text, 0)` of a `RE_NUMBER` int: a 0x, 0o or 0b prefix sets the base, which has no digit
/// limit; base 10 goes through Python's `int()`, with its 4,300-digit ValueError.
fn int_base0(text: &str) -> Result<PyValue, SmError> {
    let radix = match text.get(..2) {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => return Ok(PyValue::Int(crate::py::int::from_str(text)?.text())),
    };
    // Little-endian base 10^9 limbs.
    let mut limbs: Vec<u64> = vec![0];
    for c in text[2..].chars().filter(|c| *c != '_') {
        let mut carry = u64::from(
            c.to_digit(radix)
                .expect("the pattern matched digits of the base"),
        );
        for l in limbs.iter_mut() {
            let v = *l * u64::from(radix) + carry;
            *l = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    let mut out = limbs.last().map(u64::to_string).unwrap_or_default();
    for l in limbs.iter().rev().skip(1) {
        out.push_str(&format!("{l:09}"));
    }
    Ok(PyValue::Int(out))
}

// ---------------------------------------------------------------- Flags, NestedDict

const FROZEN: u8 = 1;
const EXPLICIT_NEST: u8 = 2;

#[derive(Default)]
struct FlagNode {
    flags: u8,
    recursive_flags: u8,
    nested: HashMap<String, FlagNode>,
}

/// `Flags`: flags that map to parsed keys and namespaces.
#[derive(Default)]
struct Flags {
    root: HashMap<String, FlagNode>,
    pending: HashSet<(Key, u8)>,
}

impl Flags {
    fn add_pending(&mut self, key: Key, flag: u8) {
        self.pending.insert((key, flag));
    }

    fn finalize_pending(&mut self) {
        for (key, flag) in std::mem::take(&mut self.pending) {
            self.set(&key, flag, false);
        }
    }

    fn unset_all(&mut self, key: &[String]) {
        let mut cont = &mut self.root;
        for k in &key[..key.len() - 1] {
            match cont.get_mut(k) {
                Some(n) => cont = &mut n.nested,
                None => return,
            }
        }
        cont.remove(&key[key.len() - 1]);
    }

    fn set(&mut self, key: &[String], flag: u8, recursive: bool) {
        let mut cont = &mut self.root;
        for k in &key[..key.len() - 1] {
            cont = &mut cont.entry(k.clone()).or_default().nested;
        }
        let node = cont.entry(key[key.len() - 1].clone()).or_default();
        if recursive {
            node.recursive_flags |= flag;
        } else {
            node.flags |= flag;
        }
    }

    fn is(&self, key: &[String], flag: u8) -> bool {
        let Some((stem, parent)) = key.split_last() else {
            return false; // document root has no flags
        };
        let mut cont = &self.root;
        for k in parent {
            let Some(inner) = cont.get(k) else {
                return false;
            };
            if inner.recursive_flags & flag != 0 {
                return true;
            }
            cont = &inner.nested;
        }
        cont.get(stem)
            .is_some_and(|n| (n.flags | n.recursive_flags) & flag != 0)
    }
}

/// Why `get_or_create_nest` or `append_nest_to_list` failed: Python's KeyError (every caller
/// turns it into "Cannot overwrite a value"), or the IndexError of `cont[-1]` on an empty list.
enum NestError {
    Key,
    Index,
}

fn get_or_create_nest<'a>(
    root: &'a mut Table,
    key: &[String],
    access_lists: bool,
) -> Result<&'a mut Table, NestError> {
    let mut cont = root;
    for k in key {
        let mut node = cont
            .entry(k.clone())
            .or_insert_with(|| Node::Table(Table::new()));
        if access_lists && let Node::Array(items) = node {
            node = items.last_mut().ok_or(NestError::Index)?;
        }
        match node {
            Node::Table(t) => cont = t,
            _ => return Err(NestError::Key),
        }
    }
    Ok(cont)
}

fn append_nest_to_list(root: &mut Table, key: &[String]) -> Result<(), NestError> {
    let (last, parent) = key.split_last().expect("a key has at least one part");
    let cont = get_or_create_nest(root, parent, true)?;
    match cont.get_mut(last) {
        Some(Node::Array(items)) => items.push(Node::Table(Table::new())),
        Some(_) => return Err(NestError::Key),
        None => {
            cont.insert(last.clone(), Node::Array(vec![Node::Table(Table::new())]));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the parser

struct Parser<'s> {
    src: &'s str,
    frames: usize,
}

impl<'s> Parser<'s> {
    fn err(&self, msg: impl Into<String>, pos: usize) -> SmError {
        decode_error(msg, self.src, pos)
    }

    fn nest_err(&self, e: NestError, pos: usize) -> SmError {
        match e {
            NestError::Key => self.err("Cannot overwrite a value", pos),
            NestError::Index => SmError::index("list index out of range"),
        }
    }

    /// `src[pos]`, or None past the end.
    fn char_at(&self, pos: usize) -> Option<char> {
        self.src.get(pos..).and_then(|s| s.chars().next())
    }

    /// The offset `n` chars on from `pos`, stopping at the end (an offset past the end only ever
    /// reaches an error, which prints "end of document" either way).
    fn advance(&self, pos: usize, n: usize) -> usize {
        let rest = &self.src[pos.min(self.src.len())..];
        pos.min(self.src.len()) + rest.chars().take(n).map(char::len_utf8).sum::<usize>()
    }

    fn starts_with(&self, pos: usize, s: &str) -> bool {
        self.src.get(pos..).is_some_and(|r| r.starts_with(s))
    }

    /// `src[pos:pos + n]` by chars.
    fn slice(&self, pos: usize, n: usize) -> &'s str {
        &self.src[pos.min(self.src.len())..self.advance(pos, n)]
    }

    fn skip_chars(&self, mut pos: usize, chars: fn(char) -> bool) -> usize {
        while let Some(c) = self.char_at(pos).filter(|c| chars(*c)) {
            pos += c.len_utf8();
        }
        pos
    }

    fn skip_until(
        &self,
        pos: usize,
        expect: &str,
        error_on: fn(char) -> bool,
        error_on_eof: bool,
    ) -> Result<usize, SmError> {
        let new_pos = match self.src[pos..].find(expect) {
            Some(i) => pos + i,
            None => {
                if error_on_eof {
                    return Err(self.err(
                        format!("Expected {}", crate::py::str::repr(expect)),
                        self.src.len(),
                    ));
                }
                self.src.len()
            }
        };
        if let Some((i, c)) = self.src[pos..new_pos]
            .char_indices()
            .find(|(_, c)| error_on(*c))
        {
            return Err(self.err(
                format!(
                    "Found invalid character {}",
                    crate::py::str::repr(&c.to_string())
                ),
                pos + i,
            ));
        }
        Ok(new_pos)
    }

    fn skip_comment(&self, pos: usize) -> Result<usize, SmError> {
        if self.char_at(pos) == Some('#') {
            return self.skip_until(pos + 1, "\n", illegal_basic, false);
        }
        Ok(pos)
    }

    fn skip_comments_and_array_ws(&self, mut pos: usize) -> Result<usize, SmError> {
        loop {
            let before = pos;
            pos = self.skip_chars(pos, toml_ws_nl);
            pos = self.skip_comment(pos)?;
            if pos == before {
                return Ok(pos);
            }
        }
    }

    fn create_dict_rule(&self, pos: usize, out: &mut Output) -> Result<(usize, Key), SmError> {
        let pos = self.skip_chars(pos + 1, toml_ws);
        let (pos, key) = self.parse_key(pos)?;
        if out.flags.is(&key, EXPLICIT_NEST) || out.flags.is(&key, FROZEN) {
            return Err(self.err(
                format!(
                    "[{}] is declared twice; keep one header and put the keys under it",
                    key_repr(&key)
                ),
                pos,
            ));
        }
        out.flags.set(&key, EXPLICIT_NEST, false);
        if let Err(e) = get_or_create_nest(&mut out.data, &key, true) {
            return Err(self.nest_err(e, pos));
        }
        if !self.starts_with(pos, "]") {
            return Err(self.err("Expected ']' at the end of a table declaration", pos));
        }
        Ok((pos + 1, key))
    }

    fn create_list_rule(&self, pos: usize, out: &mut Output) -> Result<(usize, Key), SmError> {
        let pos = self.skip_chars(pos + 2, toml_ws);
        let (pos, key) = self.parse_key(pos)?;
        if out.flags.is(&key, FROZEN) {
            return Err(self.err(
                format!("Cannot mutate immutable namespace {}", key_repr(&key)),
                pos,
            ));
        }
        // Free the namespace now that it points to another empty list item...
        out.flags.unset_all(&key);
        // ...but this key precisely is still prohibited from table declaration
        out.flags.set(&key, EXPLICIT_NEST, false);
        if let Err(e) = append_nest_to_list(&mut out.data, &key) {
            return Err(self.nest_err(e, pos));
        }
        if !self.starts_with(pos, "]]") {
            return Err(self.err("Expected ']]' at the end of an array declaration", pos));
        }
        Ok((pos + 2, key))
    }

    fn key_value_rule(
        &mut self,
        pos: usize,
        out: &mut Output,
        header: &[String],
    ) -> Result<usize, SmError> {
        let (pos, key, value) = self.parse_key_value_pair(pos)?;
        let (key_stem, key_parent) = key.split_last().expect("a key has at least one part");
        let abs_key_parent: Key = header.iter().chain(key_parent).cloned().collect();

        for i in 1..key.len() {
            let cont_key: Key = header.iter().chain(&key[..i]).cloned().collect();
            // Check that dotted key syntax does not redefine an existing table
            if out.flags.is(&cont_key, EXPLICIT_NEST) {
                return Err(self.err(
                    format!("Cannot redefine namespace {}", key_repr(&cont_key)),
                    pos,
                ));
            }
            // Containers in the relative path can't be opened with the table syntax or dotted
            // key/value syntax in following table sections.
            out.flags.add_pending(cont_key, EXPLICIT_NEST);
        }

        if out.flags.is(&abs_key_parent, FROZEN) {
            return Err(self.err(
                format!(
                    "Cannot mutate immutable namespace {}",
                    key_repr(&abs_key_parent)
                ),
                pos,
            ));
        }
        let nest = match get_or_create_nest(&mut out.data, &abs_key_parent, true) {
            Ok(n) => n,
            Err(e) => return Err(self.nest_err(e, pos)),
        };
        if nest.contains_key(key_stem) {
            return Err(self.err("Cannot overwrite a value", pos));
        }
        let frozen = value.is_container();
        nest.insert(key_stem.clone(), value);
        // Mark inline table and array namespaces recursively immutable
        if frozen {
            let full: Key = header.iter().chain(&key).cloned().collect();
            out.flags.set(&full, FROZEN, true);
        }
        Ok(pos)
    }

    fn parse_key_value_pair(&mut self, pos: usize) -> Result<(usize, Key, Node), SmError> {
        let (pos, key) = self.parse_key(pos)?;
        if self.char_at(pos) != Some('=') {
            return Err(self.err("Expected '=' after a key in a key/value pair", pos));
        }
        let pos = self.skip_chars(pos + 1, toml_ws);
        let (pos, value) = self.parse_value(pos)?;
        Ok((pos, key, value))
    }

    fn parse_key(&self, pos: usize) -> Result<(usize, Key), SmError> {
        let (pos, part) = self.parse_key_part(pos)?;
        let mut key = vec![part];
        let mut pos = self.skip_chars(pos, toml_ws);
        loop {
            if self.char_at(pos) != Some('.') {
                return Ok((pos, key));
            }
            pos = self.skip_chars(pos + 1, toml_ws);
            let (p, part) = self.parse_key_part(pos)?;
            key.push(part);
            if key.len() > MAX_KEY_PARTS {
                return Err(recursion_error(format!(
                    "TOML key has more than the allowed {MAX_KEY_PARTS} parts"
                )));
            }
            pos = self.skip_chars(p, toml_ws);
        }
    }

    fn parse_key_part(&self, pos: usize) -> Result<(usize, String), SmError> {
        match self.char_at(pos) {
            Some(c) if bare_key(c) => {
                let end = self.skip_chars(pos, bare_key);
                Ok((end, self.src[pos..end].to_string()))
            }
            Some('\'') => self.parse_literal_str(pos),
            Some('"') => self.parse_basic_str(pos + 1, false),
            _ => Err(self.err("Invalid initial character for a key part", pos)),
        }
    }

    /// Python's frame check for the recursive calls (module doc).
    fn enter(&mut self, frames: usize) -> Result<(), SmError> {
        self.frames += frames;
        if self.frames > FRAME_BUDGET {
            return Err(recursion_error("maximum recursion depth exceeded"));
        }
        Ok(())
    }

    fn parse_array(&mut self, pos: usize) -> Result<(usize, Node), SmError> {
        self.enter(2)?;
        let r = self.parse_array_body(pos + 1);
        self.frames -= 2;
        r
    }

    fn parse_array_body(&mut self, pos: usize) -> Result<(usize, Node), SmError> {
        let mut array = Vec::new();
        let mut pos = self.skip_comments_and_array_ws(pos)?;
        if self.starts_with(pos, "]") {
            return Ok((pos + 1, Node::Array(array)));
        }
        loop {
            let (p, val) = self.parse_value(pos)?;
            array.push(val);
            pos = self.skip_comments_and_array_ws(p)?;
            match self.char_at(pos) {
                Some(']') => return Ok((pos + 1, Node::Array(array))),
                Some(',') => {}
                _ => return Err(self.err("Unclosed array", pos)),
            }
            pos = self.skip_comments_and_array_ws(pos + 1)?;
            if self.starts_with(pos, "]") {
                return Ok((pos + 1, Node::Array(array)));
            }
        }
    }

    fn parse_inline_table(&mut self, pos: usize) -> Result<(usize, Node), SmError> {
        self.enter(3)?;
        let r = self.parse_inline_table_body(pos + 1);
        self.frames -= 3;
        r
    }

    fn parse_inline_table_body(&mut self, pos: usize) -> Result<(usize, Node), SmError> {
        let mut nested = Table::new();
        let mut flags = Flags::default();
        let mut pos = self.skip_comments_and_array_ws(pos)?;
        if self.starts_with(pos, "}") {
            return Ok((pos + 1, Node::Table(nested)));
        }
        loop {
            let (p, key, value) = self.parse_key_value_pair(pos)?;
            pos = p;
            let (key_stem, key_parent) = key.split_last().expect("a key has at least one part");
            if flags.is(&key, FROZEN) {
                return Err(self.err(
                    format!("Cannot mutate immutable namespace {}", key_repr(&key)),
                    pos,
                ));
            }
            let nest = match get_or_create_nest(&mut nested, key_parent, false) {
                Ok(n) => n,
                Err(e) => return Err(self.nest_err(e, pos)),
            };
            if nest.contains_key(key_stem) {
                return Err(self.err(
                    format!(
                        "Duplicate inline table key {}",
                        crate::py::str::repr(key_stem)
                    ),
                    pos,
                ));
            }
            let container = value.is_container();
            nest.insert(key_stem.clone(), value);
            pos = self.skip_comments_and_array_ws(pos)?;
            match self.char_at(pos) {
                Some('}') => return Ok((pos + 1, Node::Table(nested))),
                Some(',') => {}
                _ => return Err(self.err("Unclosed inline table", pos)),
            }
            pos = self.skip_comments_and_array_ws(pos + 1)?;
            if self.starts_with(pos, "}") {
                return Ok((pos + 1, Node::Table(nested)));
            }
            if container {
                flags.set(&key, FROZEN, true);
            }
        }
    }

    fn parse_basic_str_escape(
        &self,
        pos: usize,
        multiline: bool,
    ) -> Result<(usize, String), SmError> {
        let escape_id = self.slice(pos, 2);
        let mut pos = self.advance(pos, 2);
        if multiline && matches!(escape_id, "\\ " | "\\\t" | "\\\n") {
            // Skip whitespace until next non-whitespace character or end of the doc. Error if
            // non-whitespace is found before newline.
            if escape_id != "\\\n" {
                pos = self.skip_chars(pos, toml_ws);
                match self.char_at(pos) {
                    None => return Ok((pos, String::new())),
                    Some('\n') => pos += 1,
                    Some(_) => return Err(self.err("Unescaped '\\' in a string", pos)),
                }
            }
            return Ok((self.skip_chars(pos, toml_ws_nl), String::new()));
        }
        let replacement = match escape_id {
            "\\x" => return self.parse_hex_char(pos, 2),
            "\\u" => return self.parse_hex_char(pos, 4),
            "\\U" => return self.parse_hex_char(pos, 8),
            "\\b" => '\u{8}',
            "\\t" => '\t',
            "\\n" => '\n',
            "\\f" => '\u{c}',
            "\\r" => '\r',
            "\\e" => '\u{1b}',
            "\\\"" => '"',
            "\\\\" => '\\',
            _ => return Err(self.err("Unescaped '\\' in a string", pos)),
        };
        Ok((pos, replacement.to_string()))
    }

    fn parse_hex_char(&self, pos: usize, hex_len: usize) -> Result<(usize, String), SmError> {
        let hex_str = self.slice(pos, hex_len);
        if hex_str.chars().count() != hex_len || !hex_str.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.err("Invalid hex value", pos));
        }
        let pos = pos + hex_len;
        let hex_int = u32::from_str_radix(hex_str, 16).expect("ASCII hex digits");
        match char::from_u32(hex_int) {
            Some(c) => Ok((pos, c.to_string())),
            None => Err(self.err("Escaped character is not a Unicode scalar value", pos)),
        }
    }

    fn parse_literal_str(&self, pos: usize) -> Result<(usize, String), SmError> {
        let start = pos + 1; // Skip starting apostrophe
        let end = self.skip_until(start, "'", illegal_basic, true)?;
        Ok((end + 1, self.src[start..end].to_string())) // Skip ending apostrophe
    }

    fn parse_multiline_str(&self, pos: usize, literal: bool) -> Result<(usize, String), SmError> {
        let mut pos = pos + 3;
        if self.starts_with(pos, "\n") {
            pos += 1;
        }
        let (delim, mut result) = if literal {
            let end = self.skip_until(pos, "'''", illegal_multiline, true)?;
            let r = self.src[pos..end].to_string();
            pos = end + 3;
            ("'", r)
        } else {
            let (p, r) = self.parse_basic_str(pos, true)?;
            pos = p;
            ("\"", r)
        };
        // Add at maximum two extra apostrophes/quotes if the end sequence is 4 or 5 chars long
        // instead of just 3.
        for _ in 0..2 {
            if !self.starts_with(pos, delim) {
                break;
            }
            pos += 1;
            result.push_str(delim);
        }
        Ok((pos, result))
    }

    fn parse_basic_str(&self, mut pos: usize, multiline: bool) -> Result<(usize, String), SmError> {
        let error_on: fn(char) -> bool = if multiline {
            illegal_multiline
        } else {
            illegal_basic
        };
        let mut result = String::new();
        let mut start = pos;
        loop {
            let Some(c) = self.char_at(pos) else {
                return Err(self.err("Unterminated string", pos));
            };
            if c == '"' {
                if !multiline {
                    result.push_str(&self.src[start..pos]);
                    return Ok((pos + 1, result));
                }
                if self.starts_with(pos, "\"\"\"") {
                    result.push_str(&self.src[start..pos]);
                    return Ok((pos + 3, result));
                }
                pos += 1;
                continue;
            }
            if c == '\\' {
                result.push_str(&self.src[start..pos]);
                let (p, escaped) = self.parse_basic_str_escape(pos, multiline)?;
                result.push_str(&escaped);
                pos = p;
                start = pos;
                continue;
            }
            if error_on(c) {
                return Err(self.err(
                    format!("Illegal character {}", crate::py::str::repr(&c.to_string())),
                    pos,
                ));
            }
            pos += c.len_utf8();
        }
    }

    fn parse_value(&mut self, pos: usize) -> Result<(usize, Node), SmError> {
        let leaf = |(p, v): (usize, String)| (p, Node::Leaf(PyValue::Str(v)));
        match self.char_at(pos) {
            // Basic strings
            Some('"') if self.starts_with(pos, "\"\"\"") => {
                return self.parse_multiline_str(pos, false).map(leaf);
            }
            Some('"') => return self.parse_basic_str(pos + 1, false).map(leaf),
            // Literal strings
            Some('\'') if self.starts_with(pos, "'''") => {
                return self.parse_multiline_str(pos, true).map(leaf);
            }
            Some('\'') => return self.parse_literal_str(pos).map(leaf),
            // Booleans
            Some('t') if self.starts_with(pos, "true") => {
                return Ok((pos + 4, Node::Leaf(PyValue::Bool(true))));
            }
            Some('f') if self.starts_with(pos, "false") => {
                return Ok((pos + 5, Node::Leaf(PyValue::Bool(false))));
            }
            // Arrays
            Some('[') => return self.parse_array(pos),
            // Inline tables
            Some('{') => return self.parse_inline_table(pos),
            _ => {}
        }
        let rest = &self.src[pos..];
        // Dates and times
        if let Some(m) = RE_DATETIME.search(rest) {
            let groups: Vec<Option<&str>> = (0..12).map(|i| m.group(i)).collect();
            return match match_to_datetime(&groups) {
                Ok(v) => Ok((pos + m.end(), Node::Leaf(v))),
                Err(()) => Err(self.err("Invalid date or datetime", pos)),
            };
        }
        if let Some(m) = RE_LOCALTIME.search(rest) {
            let t = Time {
                hour: num(m.group(1).unwrap_or("0")),
                minute: num(m.group(2).unwrap_or("0")),
                second: m.group(3).map_or(0, num),
                microsecond: micros(m.group(4)),
            };
            return Ok((pos + m.end(), Node::Leaf(PyValue::Time(t))));
        }
        // Integers and "normal" floats. The regex will greedily match any type starting with a
        // decimal char, so needs to be located after handling of dates and times.
        if let Some(m) = RE_NUMBER.search(rest) {
            let text = m.as_str();
            let v = if m.group(1).is_some_and(|f| !f.is_empty()) {
                PyValue::Float(crate::py::float::from_str(text)?)
            } else {
                int_base0(text)?
            };
            return Ok((pos + m.end(), Node::Leaf(v)));
        }
        // Special floats
        let first_three = self.slice(pos, 3);
        if matches!(first_three, "inf" | "nan") {
            return Ok((
                pos + 3,
                Node::Leaf(PyValue::Float(crate::py::float::from_str(first_three)?)),
            ));
        }
        let first_four = self.slice(pos, 4);
        if matches!(first_four, "-inf" | "+inf" | "-nan" | "+nan") {
            return Ok((
                pos + 4,
                Node::Leaf(PyValue::Float(crate::py::float::from_str(first_four)?)),
            ));
        }
        Err(self.err("invalid value: text needs quotes and a list needs [ ]", pos))
    }
}

struct Output {
    data: Table,
    flags: Flags,
}

/// A document with more `[`, `{` and `.` than this parses on its own deep-stack thread, since each
/// nesting level costs a few Rust frames (and a 2 MiB test thread holds about 100 in a debug build).
const SHALLOW_MARKS: usize = 64;

/// `tomllib.loads(s)`: the document as a dict.
pub fn loads(s: &str) -> Result<PyValue, SmError> {
    if s.bytes()
        .filter(|b| matches!(b, b'[' | b'{' | b'.'))
        .count()
        > SHALLOW_MARKS
    {
        return crate::py::value::on_deep_stack(|| loads_here(s));
    }
    loads_here(s)
}

fn loads_here(s: &str) -> Result<PyValue, SmError> {
    // The spec allows converting "\r\n" to "\n", even in string literals. Let's do so to
    // simplify parsing.
    let src = s.replace("\r\n", "\n");
    let mut p = Parser {
        src: &src,
        frames: 0,
    };
    let mut out = Output {
        data: Table::new(),
        flags: Flags::default(),
    };
    let mut header: Key = Vec::new();
    let mut pos = 0;
    // Parse one statement at a time (typically means one line in TOML source)
    loop {
        // 1. Skip line leading whitespace
        pos = p.skip_chars(pos, toml_ws);
        // 2. Parse rules: end of file, end of line, comment, key/value pair, append dict to list
        // (and move to its namespace), create dict (and move to its namespace). Skip trailing
        // whitespace when applicable.
        let Some(c) = p.char_at(pos) else { break };
        if c == '\n' {
            pos += 1;
            continue;
        }
        if key_initial(c) {
            pos = p.key_value_rule(pos, &mut out, &header)?;
            pos = p.skip_chars(pos, toml_ws);
        } else if c == '[' {
            out.flags.finalize_pending();
            (pos, header) = if p.char_at(pos + 1) == Some('[') {
                p.create_list_rule(pos, &mut out)?
            } else {
                p.create_dict_rule(pos, &mut out)?
            };
            pos = p.skip_chars(pos, toml_ws);
        } else if c != '#' {
            return Err(p.err("Invalid statement", pos));
        }
        // 3. Skip comment
        pos = p.skip_comment(pos)?;
        // 4. Expect end of line or end of file
        match p.char_at(pos) {
            None => break,
            Some('\n') => pos += 1,
            Some(_) => {
                return Err(p.err("Expected newline or end of document after a statement", pos));
            }
        }
    }
    Ok(table_to_py(out.data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> String {
        loads(s).unwrap().py_repr()
    }

    fn err(s: &str) -> String {
        loads(s).unwrap_err().python_text()
    }

    // Expected values: Python 3.15.1 `repr(tomllib.loads(s))` and `f"{type(e).__name__}: {e}"`.
    #[test]
    fn every_value_form_loads_as_python_gives_it() {
        assert_eq!(
            ok(
                "a = 0x1F\nb = -0\nc = 1e400\nd = -nan\ne = 'l\\n'\nf = \"\\e\\x41\\U0001F600\"\ng = [1, 'x', {h = 1.5}]"
            ),
            "{'a': 31, 'b': 0, 'c': inf, 'd': nan, 'e': 'l\\\\n', 'f': '\\x1bA😀', 'g': [1, 'x', {'h': 1.5}]}"
        );
        assert_eq!(
            ok(
                "t = 07:32\nu = 00:00:00.1234567\nd = 2000-02-29\nz = 1979-05-27T07:32:00-00:00\no = 1979-05-27 00:32:00.5+05:30"
            ),
            "{'t': datetime.time(7, 32), 'u': datetime.time(0, 0, 0, 123456), 'd': datetime.date(2000, 2, 29), \
             'z': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc), \
             'o': datetime.datetime(1979, 5, 27, 0, 32, 0, 500000, tzinfo=datetime.timezone(datetime.timedelta(seconds=19800)))}"
        );
        assert_eq!(
            ok("[[a]]\nb = 1\n[a.c]\n[[a]]\nx.y = 2\n"),
            "{'a': [{'b': 1, 'c': {}}, {'x': {'y': 2}}]}"
        );
        assert_eq!(
            ok("a = \"\"\"x \\\n   y\"\"\"\"\"\nb = '''z''''"),
            "{'a': 'x y\"\"', 'b': \"z'\"}"
        );
        assert_eq!(ok("a = 1\r\n"), "{'a': 1}");
        assert_eq!(ok(""), "{}");
    }

    #[test]
    fn errors_name_the_line_and_column_in_chars() {
        assert_eq!(
            err("x = 0x\n"),
            "TOMLDecodeError: Expected newline or end of document after a statement (at line 1, column 6)"
        );
        assert_eq!(
            err("\u{feff}x = 1"),
            "TOMLDecodeError: Invalid statement (at line 1, column 1)"
        );
        assert_eq!(
            err("é = 1"),
            "TOMLDecodeError: Invalid statement (at line 1, column 1)"
        );
        assert_eq!(
            err("a = 'é😀\u{7f}'"),
            "TOMLDecodeError: Found invalid character '\\x7f' (at line 1, column 8)"
        );
        assert_eq!(
            err("a = 1\nb = \"\\q\""),
            "TOMLDecodeError: Unescaped '\\' in a string (at line 2, column 8)"
        );
        assert_eq!(
            err("x = \"\\"),
            "TOMLDecodeError: Unescaped '\\' in a string (at end of document)"
        );
        assert_eq!(
            err("x = \"\\ud800\""),
            "TOMLDecodeError: Escaped character is not a Unicode scalar value (at line 1, column 12)"
        );
        assert_eq!(
            err("x = 2001-02-29"),
            "TOMLDecodeError: Invalid date or datetime (at line 1, column 5)"
        );
        assert_eq!(
            err("x = 0000-01-01"),
            "TOMLDecodeError: Invalid date or datetime (at line 1, column 5)"
        );
        assert_eq!(
            err("[a]\n[a]"),
            "TOMLDecodeError: [a] is declared twice; keep one header and put the keys under it (at line 2, column 3)"
        );
        assert_eq!(
            err("a.b = 1\n[a.b]"),
            "TOMLDecodeError: Cannot overwrite a value (at line 2, column 5)"
        );
        assert_eq!(
            err("[a.b]\n[a]\nb.c = 1"),
            "TOMLDecodeError: Cannot redefine namespace a.b (at end of document)"
        );
        assert_eq!(
            err("a = []\n[[a]]"),
            "TOMLDecodeError: Cannot mutate immutable namespace a (at line 2, column 4)"
        );
        assert_eq!(
            err("a = {b = 1}\na.c = 2"),
            "TOMLDecodeError: Cannot mutate immutable namespace a (at end of document)"
        );
        assert_eq!(
            err("a = {b = 1, b = 2}"),
            "TOMLDecodeError: Duplicate inline table key 'b' (at line 1, column 18)"
        );
        assert_eq!(
            err("a = {b = [], b.c = 1}"),
            "TOMLDecodeError: Cannot mutate immutable namespace b.c (at line 1, column 21)"
        );
        assert_eq!(
            err("a = 1\n[[a]]"),
            "TOMLDecodeError: Cannot overwrite a value (at line 2, column 4)"
        );
        assert_eq!(
            err("a = '''x"),
            "TOMLDecodeError: Expected \"'''\" (at end of document)"
        );
        assert_eq!(
            err("a = [1 2]"),
            "TOMLDecodeError: Unclosed array (at line 1, column 8)"
        );
        assert_eq!(
            err("a = {b = 1 c = 2}"),
            "TOMLDecodeError: Unclosed inline table (at line 1, column 12)"
        );
    }

    #[test]
    fn each_rule_holds_on_its_own_case() {
        // Python 3.15.1 tomllib on each input; one case per check the mutation run named.
        assert_eq!(ok("a = \"\"\"x\ny\"\"\""), "{'a': 'x\\ny'}");
        assert_eq!(ok("a = \"\"\"\nx\"\"\""), "{'a': 'x'}");
        assert_eq!(ok("a = \"x\ty\""), "{'a': 'x\\ty'}");
        assert_eq!(
            ok("a = 1979-05-27T07:32:00Z"),
            "{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc)}"
        );
        assert_eq!(
            ok("a = 1979-05-27T00:32:00-07:00"),
            "{'a': datetime.datetime(1979, 5, 27, 0, 32, tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds=61200)))}"
        );
        assert_eq!(
            ok("[[a]]\n[a.b]\n[[a]]\n[a.b]"),
            "{'a': [{'b': {}}, {'b': {}}]}"
        );
        assert_eq!(
            err("a = {}\n[a]"),
            "TOMLDecodeError: [a] is declared twice; keep one header and put the keys under it (at line 2, column 3)"
        );
        assert_eq!(
            err("[a]\nb.c = 1\n[a.b]"),
            "TOMLDecodeError: [a.b] is declared twice; keep one header and put the keys under it (at line 3, column 5)"
        );
        assert_eq!(
            err("a = \"x\u{0}\""),
            "TOMLDecodeError: Illegal character '\\x00' (at line 1, column 7)"
        );
        assert_eq!(
            err("a = \"\\x4"),
            "TOMLDecodeError: Invalid hex value (at line 1, column 8)"
        );
        assert_eq!(
            err("a = \"\\xg0\""),
            "TOMLDecodeError: Invalid hex value (at line 1, column 8)"
        );
        assert_eq!(
            err("a = 1\na = 2"),
            "TOMLDecodeError: Cannot overwrite a value (at end of document)"
        );
        assert_eq!(
            err("a = \"\"\"x \\  y\"\"\""),
            "TOMLDecodeError: Unescaped '\\' in a string (at line 1, column 13)"
        );
    }

    #[test]
    fn limits_raise_what_python_raises() {
        let digits = "1".repeat(4301);
        assert_eq!(
            err(&format!("a = {digits}")),
            "ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; \
             use sys.set_int_max_str_digits() to increase the limit"
        );
        // Hex has no digit limit on the way in.
        let PyValue::Dict(d) = loads(&format!("a = 0x{}", "f".repeat(4000))).unwrap() else {
            panic!("a dict")
        };
        assert!(matches!(&d[0].1, PyValue::Int(t) if t.len() == 4817 && t.starts_with("3019")));
        assert_eq!(
            err(&format!("a{} = 1", ".a".repeat(1000))),
            "RecursionError: TOML key has more than the allowed 1000 parts"
        );
        assert!(loads(&format!("a{} = 1", ".a".repeat(999))).is_ok());
        assert_eq!(
            err(&format!("a = {}{}", "[".repeat(5000), "]".repeat(5000))),
            "RecursionError: maximum recursion depth exceeded"
        );
        assert_eq!(
            err(&format!("a = {}1{}", "{b=".repeat(5000), "}".repeat(5000))),
            "RecursionError: maximum recursion depth exceeded"
        );
        assert!(loads(&format!("a = {}{}", "[".repeat(300), "]".repeat(300))).is_ok());
    }
}
