//! Python-compatible `str` helpers and regex character-class translators (`\w`, `\W`, `\s`, `\S`,
//! `\d`, `\b`) backed by generated Unicode 17.0.0 character tables (`src/py/tables.rs`).

use std::sync::LazyLock;

use crate::error::SmError;
#[cfg(any(test, feature = "parity"))]
use crate::parity::{Call, Reply};
use crate::py::tables;

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
    let cp = c as u32;
    table
        .binary_search_by(|&(a, b)| {
            if b < cp {
                std::cmp::Ordering::Less
            } else if a > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `c.isspace()`.
pub fn isspace(c: char) -> bool {
    if c.is_ascii() {
        return matches!(c, '\t'..='\r' | '\x1c'..=' ');
    }
    in_ranges(tables::ISSPACE, c)
}

/// `c.isdecimal()`.
pub fn isdecimal(c: char) -> bool {
    in_ranges(tables::ISDECIMAL, c)
}

/// `c.isalpha()`.
pub fn isalpha(c: char) -> bool {
    in_ranges(tables::ISALPHA, c)
}

/// `c.isprintable()`.
pub fn isprintable(c: char) -> bool {
    in_ranges(tables::ISPRINTABLE, c)
}

/// Python's `s.isX()` over a whole string: true when `s` is not empty and every char passes.
pub fn all_chars(s: &str, pred: fn(char) -> bool) -> bool {
    !s.is_empty() && s.chars().all(pred)
}

/// Is `c` matched by `\w` in a Python str pattern (`isalnum() or "_"`)?
pub fn is_word(c: char) -> bool {
    in_ranges(tables::RE_WORD, c)
}

/// `len(s)`: a count of code points.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// `s[start:end]` by code point, with Python's negative and out-of-range index rules.
pub fn slice(s: &str, start: Option<isize>, end: Option<isize>) -> &str {
    let n = len(s) as isize;
    let fix = |i: isize| if i < 0 { (i + n).max(0) } else { i.min(n) };
    let a = start.map_or(0, fix);
    let b = end.map_or(n, fix);
    if a >= b {
        return "";
    }
    let byte = |i: isize| s.char_indices().nth(i as usize).map_or(s.len(), |(p, _)| p);
    &s[byte(a)..byte(b)]
}

/// `s.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(isspace)
}

/// `s.lstrip()`.
pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(isspace)
}

/// `s.rstrip()`.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(isspace)
}

/// `s.strip(chars)`.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// `s.rstrip(chars)`.
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_end_matches(|c| chars.contains(c))
}

/// `s.split()` and `s.split(None, maxsplit)`: runs of whitespace separate, and the rest after
/// `maxsplit` splits keeps its trailing whitespace. `None` is Python's default of -1.
pub fn split_ws(s: &str, maxsplit: Option<usize>) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        rest = lstrip(rest);
        if rest.is_empty() {
            break;
        }
        if maxsplit.is_some_and(|m| out.len() == m) {
            out.push(rest);
            break;
        }
        let end = rest.find(isspace).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

/// `s.split()`.
pub fn split(s: &str) -> Vec<&str> {
    split_ws(s, None)
}

fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `s.splitlines(keepends)`: `\r\n` is one break; a final break adds no empty line.
pub fn splitlines(s: &str, keepends: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if !is_line_break(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r' && it.peek().is_some_and(|&(_, n)| n == '\n') {
            it.next();
            end += 1;
        }
        out.push(if keepends {
            &s[start..end]
        } else {
            &s[start..i]
        });
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn lower_char(c: char, out: &mut String) {
    let cp = c as u32;
    match tables::LOWER.binary_search_by_key(&cp, |&(k, _)| k) {
        Ok(i) => out.push_str(tables::LOWER[i].1),
        Err(_) => out.push(c),
    }
}

/// `s.lower()`, with Python's final-sigma rule for U+03A3.
pub fn lower(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c != '\u{3a3}' {
            lower_char(c, &mut out);
            continue;
        }
        // Objects/unicodeobject.c handle_capital_sigma.
        let before = chars[..i]
            .iter()
            .rev()
            .find(|&&x| !in_ranges(tables::SIGMA_IGNORABLE, x));
        let mut final_sigma = before.is_some_and(|&x| in_ranges(tables::SIGMA_CASED, x));
        if final_sigma {
            let after = chars[i + 1..]
                .iter()
                .find(|&&x| !in_ranges(tables::SIGMA_IGNORABLE, x));
            final_sigma = !after.is_some_and(|&x| in_ranges(tables::SIGMA_CASED, x));
        }
        out.push(if final_sigma { '\u{3c2}' } else { '\u{3c3}' });
    }
    out
}

/// A byte that is not part of valid UTF-8 (0x80-0xFF), held as this code point plus the byte,
/// where Python holds the lone surrogate U+DC00 plus the byte (surrogateescape; a Rust str cannot
/// hold a surrogate). U+10FD80-U+10FDFF is in the private use area.
pub const STAND_IN_BASE: u32 = 0x10_FD00;

/// The byte a surrogateescape stand-in char holds, if `c` is one.
pub fn surrogate_byte(c: char) -> Option<u8> {
    let cp = c as u32;
    (STAND_IN_BASE + 0x80..=STAND_IN_BASE + 0xFF)
        .contains(&cp)
        .then(|| (cp - STAND_IN_BASE) as u8)
}

/// `bytes.decode("utf-8", "surrogateescape")` (`os.fsdecode`), each undecodable byte as its
/// stand-in char.
pub fn decode_surrogateescape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(tail) => {
                s.push_str(tail);
                return s;
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                s.push_str(std::str::from_utf8(good).expect("valid up to here"));
                let n = e.error_len().unwrap_or(bad.len());
                for &b in &bad[..n] {
                    s.push(
                        char::from_u32(STAND_IN_BASE + u32::from(b))
                            .expect("a plane 16 code point"),
                    );
                }
                rest = &bad[n..];
            }
        }
    }
}

fn push_escaped(c: char, quote: char, out: &mut String) {
    if let Some(b) = surrogate_byte(c) {
        out.push_str(&format!("\\udc{b:02x}"));
        return;
    }
    match c {
        '\\' => out.push_str("\\\\"),
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        c if c == quote => {
            out.push('\\');
            out.push(c);
        }
        c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
        c if c.is_ascii() || isprintable(c) => out.push(c),
        c => {
            let cp = c as u32;
            if cp < 0x100 {
                out.push_str(&format!("\\x{cp:02x}"));
            } else if cp < 0x10000 {
                out.push_str(&format!("\\u{cp:04x}"));
            } else {
                out.push_str(&format!("\\U{cp:08x}"));
            }
        }
    }
}

fn quote_for(has_single: bool, has_double: bool) -> char {
    if has_single && !has_double { '"' } else { '\'' }
}

/// `repr(s)`.
pub fn repr(s: &str) -> String {
    let quote = quote_for(s.contains('\''), s.contains('"'));
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        push_escaped(c, quote, &mut out);
    }
    out.push(quote);
    out
}

/// `repr(os.fsdecode(bytes))`: a byte that is not part of valid UTF-8 shows as `\udcXX`.
pub fn repr_surrogateescape(bytes: &[u8]) -> String {
    repr(&decode_surrogateescape(bytes))
}

// ---------------------------------------------------------------- regex fragments

fn class_inner(table: &[(u32, u32)]) -> String {
    let mut s = String::new();
    for &(a, b) in table {
        if a == b {
            s.push_str(&format!("\\x{{{a:X}}}"));
        } else {
            s.push_str(&format!("\\x{{{a:X}}}-\\x{{{b:X}}}"));
        }
    }
    s
}

/// The inside of a bracket class for Python's `\w`, to combine with other members: `[{W_INNER}-]`.
pub static W_INNER: LazyLock<String> = LazyLock::new(|| class_inner(tables::RE_WORD));
/// The inside of a bracket class for Python's `\s`.
pub static S_INNER: LazyLock<String> = LazyLock::new(|| class_inner(tables::RE_SPACE));
/// The inside of a bracket class for Python's `\d`.
pub static D_INNER: LazyLock<String> = LazyLock::new(|| class_inner(tables::RE_DIGIT));
/// Python's `\w` as a regex class.
pub static RE_W: LazyLock<String> = LazyLock::new(|| format!("[{}]", *W_INNER));
/// Python's `\W`.
pub static RE_NW: LazyLock<String> = LazyLock::new(|| format!("[^{}]", *W_INNER));
/// Python's `\s`.
pub static RE_S: LazyLock<String> = LazyLock::new(|| format!("[{}]", *S_INNER));
/// Python's `\S`.
pub static RE_NS: LazyLock<String> = LazyLock::new(|| format!("[^{}]", *S_INNER));
/// Python's `\d`.
pub static RE_D: LazyLock<String> = LazyLock::new(|| format!("[{}]", *D_INNER));
/// Python's `\D`.
pub static RE_ND: LazyLock<String> = LazyLock::new(|| format!("[^{}]", *D_INNER));

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// The escape after a `\`: a class fragment, `\z` for `\Z`, or the escape as written, with a
/// `\x{..}`, `\xHH`, `\uHHHH` or `\UHHHHHHHH` kept whole so it can end a range.
fn escape_item(it: &mut Chars, in_class: bool) -> String {
    let Some(n) = it.next() else {
        return "\\".into();
    };
    let frag: &str = match n {
        'w' => &RE_W,
        'W' => &RE_NW,
        's' => &RE_S,
        'S' => &RE_NS,
        'd' => &RE_D,
        'D' => &RE_ND,
        'Z' if !in_class => "\\z",
        // Inside a class, Python's `\b` is the backspace char.
        'b' if in_class => "\\x08",
        'b' | 'B' => panic!(
            "a Python \\b or \\B inside a pattern has no form without lookaround: use PyRegex"
        ),
        _ => {
            let mut out = format!("\\{n}");
            let width = match n {
                'x' if it.peek() == Some(&'{') => usize::MAX,
                'x' => 2,
                'u' => 4,
                'U' => 8,
                _ => 0,
            };
            if width == usize::MAX {
                for c in it.by_ref() {
                    out.push(c);
                    if c == '}' {
                        break;
                    }
                }
            } else {
                for _ in 0..width {
                    if let Some(c) = it.next_if(char::is_ascii_hexdigit) {
                        out.push(c);
                    }
                }
            }
            return out;
        }
    };
    frag.to_string()
}

/// One member of a Python bracket class. Python reads `[`, `&&`, `--` and `~~` there as
/// literal chars, and the regex crate reads them as set syntax, so they are escaped.
fn class_item(c: char, it: &mut Chars) -> String {
    match c {
        '\\' => escape_item(it, true),
        '[' | ']' | '^' | '-' | '&' | '~' => format!("\\{c}"),
        c => c.to_string(),
    }
}

/// The rest of a bracket class after its `[`, read item by item as sre_parse reads it: an item
/// followed by `-` and a char other than `]` is a range.
fn translate_class(it: &mut Chars, out: &mut String) {
    out.push('[');
    if it.next_if_eq(&'^').is_some() {
        out.push('^');
    }
    let mut first = true;
    while let Some(c) = it.next() {
        if c == ']' && !first {
            out.push(']');
            return;
        }
        first = false;
        out.push_str(&class_item(c, it));
        let mut look = it.clone();
        if look.next() == Some('-') && look.next().is_some_and(|n| n != ']') {
            it.next();
            let hi = it.next().expect("the look-ahead saw a char");
            out.push('-');
            out.push_str(&class_item(hi, it));
        }
    }
    out.push(']');
}

/// A Python pattern with its `\w \W \s \S \d \D` escapes and `\Z` replaced by the fragments
/// above, and its bracket classes read by Python's rules. Inside a bracket class a class escape
/// becomes a nested class, which the regex crate unions: `[^\w\s]` becomes `[^[..w..][..s..]]`.
/// `\b` and `\B` panic here: `PyRegex` takes a `\b` at either end of a pattern.
pub fn translate(py_pattern: &str) -> String {
    let mut out = String::new();
    let mut it = py_pattern.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => out.push_str(&escape_item(&mut it, false)),
            '[' => translate_class(&mut it, &mut out),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- re.IGNORECASE as sre does it

/// Lib/re/_casefix.py `_EXTRA_CASES` (Python 3.15.1): lowercase chars sre also takes as equal.
const EXTRA_CASES: &[(u32, &[u32])] = &[
    (105, &[305]),
    (115, &[383]),
    (181, &[956]),
    (305, &[105]),
    (383, &[115]),
    (837, &[953, 8126]),
    (912, &[8147]),
    (944, &[8163]),
    (946, &[976]),
    (949, &[1013]),
    (952, &[977]),
    (953, &[837, 8126]),
    (954, &[1008]),
    (956, &[181]),
    (960, &[982]),
    (961, &[1009]),
    (962, &[963]),
    (963, &[962]),
    (966, &[981]),
    (976, &[946]),
    (977, &[952]),
    (981, &[966]),
    (982, &[960]),
    (1008, &[954]),
    (1009, &[961]),
    (1013, &[949]),
    (1074, &[7296]),
    (1076, &[7297]),
    (1086, &[7298]),
    (1089, &[7299]),
    (1090, &[7300, 7301]),
    (1098, &[7302]),
    (1123, &[7303]),
    (7296, &[1074]),
    (7297, &[1076]),
    (7298, &[1086]),
    (7299, &[1089]),
    (7300, &[1090, 7301]),
    (7301, &[1090, 7300]),
    (7302, &[1098]),
    (7303, &[1123]),
    (7304, &[42571]),
    (7777, &[7835]),
    (7835, &[7777]),
    (8126, &[837, 953]),
    (8147, &[912]),
    (8163, &[944]),
    (42571, &[7304]),
    (64261, &[64262]),
    (64262, &[64261]),
];

/// The `_EXTRA_CASES` of the simple lowercase `lo`, empty when it has none.
pub fn extra_cases(lo: char) -> &'static [u32] {
    EXTRA_CASES
        .iter()
        .find(|(k, _)| *k == lo as u32)
        .map_or(&[], |(_, x)| x)
}

/// `_sre.unicode_tolower(c)`: the simple lowercase. U+0130 is the one char whose full lowercase
/// is longer than one char (checked over every code point against Python 3.15.1).
pub fn sre_lower(c: char) -> char {
    if c == '\u{130}' {
        return 'i';
    }
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// `(sre_lower(c), c)` for the 1,488 chars `c` whose simple lowercase is another char, sorted.
/// A map of all 1,112,064 code points took 160-200 ms to build and ran on every `sm query`.
static LOWERED: LazyLock<Vec<(char, char)>> = LazyLock::new(|| {
    let mut v: Vec<(char, char)> = (0..=0x10FFFFu32)
        .filter_map(char::from_u32)
        .filter_map(|c| {
            let l = sre_lower(c);
            (l != c).then_some((l, c))
        })
        .collect();
    v.sort();
    v
});

/// Every char whose simple lowercase is `l`, for an `l` that is its own simple lowercase (every
/// `sre_lower` result and every `_EXTRA_CASES` member is; test `by_lower_is_every_char_by_its_lowercase`).
fn by_lower(l: char) -> impl Iterator<Item = char> {
    let start = LOWERED.partition_point(|p| p.0 < l);
    std::iter::once(l).chain(
        LOWERED[start..]
            .iter()
            .take_while(move |p| p.0 == l)
            .map(|p| p.1),
    )
}

/// The chars that an IGNORECASE pattern's literal `x` (or class member `x`) matches, sorted:
/// every char whose simple lowercase is `lower(x)` or one of its `_EXTRA_CASES`. sre matches an
/// uncased `x` as itself, and this rule gives `[x]` for every uncased code point too (checked
/// over all of them against 3.15.1).
pub fn ci_chars(x: char) -> Vec<char> {
    let lo = sre_lower(x);
    let mut lows = vec![lo];
    lows.extend(extra_cases(lo).iter().filter_map(|&u| char::from_u32(u)));
    let mut out: Vec<char> = lows.iter().flat_map(|&l| by_lower(l)).collect();
    out.sort();
    out.dedup();
    out
}

fn push_class_chars(set: &[char], out: &mut String) {
    out.push('[');
    for m in set {
        out.push_str(&format!("\\x{{{:X}}}", *m as u32));
    }
    out.push(']');
}

/// The literal text `s` under `re.IGNORECASE`, as a `translate`/`PyRegex` pattern fragment:
/// each cased char becomes the class of `ci_chars`, and every other char is escaped.
pub fn ci_literal(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let set = ci_chars(c);
        if set.len() == 1 {
            out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4])));
        } else {
            push_class_chars(&set, &mut out);
        }
    }
    out
}

/// The sorted chars of the class whose members are `members` under `re.IGNORECASE`: sre matches
/// `c` when `lower(c)` is the lowercase of a member or one of its `_EXTRA_CASES`, which is the
/// union of `ci_chars` over the members (Lib/re/_compiler.py `_optimize_charset`).
pub fn ci_set(members: impl IntoIterator<Item = char>) -> Vec<char> {
    let mut out: Vec<char> = members.into_iter().flat_map(ci_chars).collect();
    out.sort();
    out.dedup();
    out
}

/// `ci_set(members)` as a bracket class fragment, for example `[0-9a-f]` under IGNORECASE is
/// `ci_class(('0'..='9').chain('a'..='f'))`.
pub fn ci_class(members: impl IntoIterator<Item = char>) -> String {
    let mut out = String::new();
    push_class_chars(&ci_set(members), &mut out);
    out
}

// ---------------------------------------------------------------- PyRegex

/// A zero-width check at the start of a pattern, made on the chars around the match start.
#[derive(Clone, Debug)]
enum Lead {
    /// `\b`: one side is a Python `\w` char and the other is not.
    Boundary,
    /// `(?<!X)` (`negate`) or `(?<=X)`: whether the char before is in the one-char class X.
    Before { class: regex::Regex, negate: bool },
}

/// A compiled Python pattern that runs on the regex crate, with no backtracking limit.
///
/// It takes Python's syntax through `translate`, plus one zero-width check at each end:
/// - at the start, `\b`, `(?<!X)` or `(?<=X)`, where X matches one char. A zero-width check
///   depends only on the text at its position, so it is checked in code at each match start,
///   and a failed start moves the search on to the next char, as sre does.
/// - at the end, `(?!X)` or `(?=X)` with X one char, or `\b` after an element that always ends
///   on a `\w` char (`\d+`, `[0-9a-f]{4}`, a word char; checked when built). The check becomes a
///   consumed `(?:[^X]|\z)` (or `X`) in its own last group, so the regex crate's leftmost-first
///   order, which is a backtracking engine's order, finds the same end sre finds.
///
/// `finditer`, `sub` and `split` follow Python 3.7+ on empty matches: an empty match may follow a
/// non-empty one at the same position, and after an empty match at `p` the next match starts
/// past `p`. This equals sre's rule (a non-empty match may still start at `p`) for every pattern
/// whose best match at a position is empty only when no non-empty match starts there, which
/// holds for patterns without lazy quantifiers, empty-first alternations and zero-width checks.
#[derive(Clone, Debug)]
pub struct PyRegex {
    re: regex::Regex,
    lead: Option<Lead>,
    /// The group that holds the consumed end check, when the pattern has one.
    trail: Option<usize>,
}

/// One match: Python's `m.start()`, `m.end()`, `m.group(i)`, as byte offsets into the text.
#[derive(Debug)]
pub struct PyMatch<'t> {
    caps: regex::Captures<'t>,
    text: &'t str,
    end: usize,
    /// The first group index that is not the Python pattern's (the consumed end check).
    groups: usize,
}

impl<'t> PyMatch<'t> {
    pub fn start(&self) -> usize {
        self.caps.get(0).map_or(0, |m| m.start())
    }

    pub fn end(&self) -> usize {
        self.end
    }

    /// `m.group(0)`.
    pub fn as_str(&self) -> &'t str {
        &self.text[self.start()..self.end]
    }

    /// `m.group(i)`: None for a group that did not take part or does not exist.
    pub fn group(&self, i: usize) -> Option<&'t str> {
        if i == 0 {
            return Some(self.as_str());
        }
        if i >= self.groups {
            return None;
        }
        self.caps.get(i).map(|m| m.as_str())
    }
}

/// The index just past the top-level `)` that closes the group opened at `open` (a `(`).
fn group_end(p: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut it = p[open..].char_indices().peekable();
    let mut in_class = false;
    while let Some((i, c)) = it.next() {
        match c {
            '\\' => {
                it.next();
            }
            '[' if !in_class => {
                in_class = true;
                if it.peek().is_some_and(|&(_, n)| n == '^') {
                    it.next();
                }
                if it.peek().is_some_and(|&(_, n)| n == ']') {
                    it.next();
                }
            }
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `p` holds a `|` outside every group and class.
fn has_top_level_alt(p: &str) -> bool {
    let mut it = p.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '\\' => {
                it.next();
            }
            '|' => return true,
            '(' | '[' => {
                let end = if c == '(' {
                    group_end(p, i)
                } else {
                    class_end(p, i)
                };
                let Some(end) = end else { return false };
                while it.peek().is_some_and(|&(j, _)| j < end) {
                    it.next();
                }
            }
            _ => {}
        }
    }
    false
}

/// The index just past the `]` that closes the class opened at `open`.
fn class_end(p: &str, open: usize) -> Option<usize> {
    let mut it = p[open + 1..].char_indices().peekable();
    it.next_if(|&(_, c)| c == '^');
    it.next_if(|&(_, c)| c == ']');
    while let Some((i, c)) = it.next() {
        match c {
            '\\' => {
                it.next();
            }
            ']' => return Some(open + 1 + i + 1),
            _ => {}
        }
    }
    None
}

/// Whether `p` holds a `\b` or `\B` outside a class.
fn has_inner_boundary(p: &str) -> bool {
    let mut it = p.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '\\' if it.next().is_some_and(|(_, n)| n == 'b' || n == 'B') => return true,
            '[' => {
                let Some(end) = class_end(p, i) else {
                    return false;
                };
                while it.peek().is_some_and(|&(j, _)| j < end) {
                    it.next();
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether `x` is one char, one escape for one char or class, or one bracket class.
fn is_one_char(x: &str) -> bool {
    if let Some(rest) = x.strip_prefix('\\') {
        let mut cs = rest.chars();
        return matches!((cs.next(), cs.next()), (Some(c), None) if "wWsSdD".contains(c) || c.is_ascii_punctuation());
    }
    let mut cs = x.chars();
    if let (Some(c), None) = (cs.next(), cs.next()) {
        return !".^$*+?{}[]\\|()".contains(c);
    }
    x.starts_with('[') && class_end(x, 0) == Some(x.len())
}

/// Is every char the one-char element `e` matches a Python `\w` char?
fn word_only_element(e: &str) -> bool {
    if e == "\\d" || e == "\\w" {
        return true;
    }
    if e.starts_with('[') && !e.starts_with("[^") {
        let inner = &e[1..e.len() - 1];
        let mut it = inner.chars().peekable();
        let mut prev: Option<char> = None;
        while let Some(c) = it.next() {
            let one = match c {
                '\\' => match it.next() {
                    Some('d' | 'w') => {
                        prev = None;
                        continue;
                    }
                    Some('x') if it.peek() == Some(&'{') => {
                        it.next();
                        let hex: String = it.by_ref().take_while(|&h| h != '}').collect();
                        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                    }
                    _ => None,
                },
                '-' if prev.is_some() && it.peek().is_some() => {
                    // A range: both ends in one of 0-9, a-z, A-Z.
                    let lo = prev.take().expect("checked");
                    let Some(hi) = it.next() else { return false };
                    let same =
                        |r: std::ops::RangeInclusive<char>| r.contains(&lo) && r.contains(&hi);
                    if !(same('0'..='9') || same('a'..='z') || same('A'..='Z')) {
                        return false;
                    }
                    continue;
                }
                c => Some(c),
            };
            match one {
                Some(c) if is_word(c) => prev = Some(c),
                _ => return false,
            }
        }
        return true;
    }
    let mut cs = e.chars();
    matches!((cs.next(), cs.next()), (Some(c), None) if is_word(c))
}

/// Does every match of `body` end on a `\w` char? True when its last element is word-only and
/// is not optional (no `?`, `*` or `{0,..}` after it).
fn ends_on_word_char(body: &str) -> bool {
    // Split off a trailing quantifier.
    let mut core = body;
    let mut min_ok = true;
    if let Some(b) = core
        .strip_suffix('?')
        .filter(|b| b.ends_with('+') || b.ends_with('}'))
    {
        core = b;
    }
    if let Some(b) = core.strip_suffix('+') {
        core = b;
    } else if core.ends_with('}') && !core.ends_with("\\}") {
        let Some(open) = core.rfind('{') else {
            return false;
        };
        let min: String = core[open + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        min_ok = min.parse::<u32>().is_ok_and(|n| n >= 1);
        core = &core[..open];
    } else if core.ends_with('?') || core.ends_with('*') {
        return false;
    }
    if !min_ok {
        return false;
    }
    // The last element: a class, an escape or one char.
    if core.ends_with(']') {
        let mut i = 0;
        let mut last = None;
        while i < core.len() {
            let c = core[i..].chars().next().expect("in range");
            if c == '\\' {
                i += 1 + core[i + 1..].chars().next().map_or(0, char::len_utf8);
                continue;
            }
            if c == '[' {
                let Some(end) = class_end(core, i) else {
                    return false;
                };
                if end == core.len() {
                    last = Some(i);
                }
                i = end;
                continue;
            }
            i += c.len_utf8();
        }
        return last.is_some_and(|s| word_only_element(&core[s..]));
    }
    let n = core.len();
    if n >= 2 && core.as_bytes()[n - 2] == b'\\' && (n < 3 || core.as_bytes()[n - 3] != b'\\') {
        return word_only_element(&core[n - 2..]);
    }
    core.chars()
        .next_back()
        .is_some_and(|c| word_only_element(&c.to_string()))
}

impl PyRegex {
    /// Compile a Python pattern (see the type's doc for what it takes). A pattern this type
    /// cannot run exactly is a ValueError naming the reason.
    pub fn new(py_pattern: &str) -> Result<PyRegex, SmError> {
        let bad = |why: &str| SmError::value(format!("PyRegex cannot run {py_pattern:?}: {why}"));
        let mut body = py_pattern;
        let mut lead = None;
        if let Some(rest) = body.strip_prefix("\\b") {
            lead = Some(Lead::Boundary);
            body = rest;
        } else if body.starts_with("(?<!") || body.starts_with("(?<=") {
            let end = group_end(body, 0).ok_or_else(|| bad("an open lookbehind"))?;
            let x = &body[4..end - 1];
            if !is_one_char(x) {
                return Err(bad("a lookbehind wider than one char"));
            }
            let class = regex::Regex::new(&format!("\\A(?:{})\\z", translate(x)))
                .map_err(|e| bad(&e.to_string()))?;
            lead = Some(Lead::Before {
                class,
                negate: body.as_bytes()[3] == b'!',
            });
            body = &body[end..];
        }
        let mut tail = None;
        if body.ends_with("\\b") && !body.ends_with("\\\\b") {
            let b = &body[..body.len() - 2];
            if !ends_on_word_char(b) {
                return Err(bad(
                    "a closing \\b after an element that may end on a non-word char",
                ));
            }
            tail = Some(format!("(?:{}|\\z)", *RE_NW));
            body = b;
        } else if body.ends_with(')')
            && let Some(open) = ["(?!", "(?="].iter().filter_map(|p| body.rfind(p)).max()
            && group_end(body, open) == Some(body.len())
        {
            let x = &body[open + 3..body.len() - 1];
            if !is_one_char(x) {
                return Err(bad("a lookahead wider than one char"));
            }
            let t = translate(x);
            tail = Some(if body.as_bytes()[open + 2] == b'!' {
                format!("(?:[^{t}]|\\z)")
            } else {
                format!("(?:{t})")
            });
            body = &body[..open];
        }
        if has_inner_boundary(body) {
            return Err(bad("a \\b or \\B that is not at an end"));
        }
        if (lead.is_some() || tail.is_some()) && has_top_level_alt(body) {
            return Err(bad("a top-level | next to a zero-width check"));
        }
        let mut src = format!("(?:{})", translate(body));
        if let Some(t) = &tail {
            src.push_str(&format!("({t})"));
        }
        let re = regex::Regex::new(&src).map_err(|e| bad(&e.to_string()))?;
        let trail = tail.is_some().then(|| re.captures_len() - 1);
        Ok(PyRegex { re, lead, trail })
    }

    fn lead_ok(&self, text: &str, at: usize) -> bool {
        let before = text[..at].chars().next_back();
        match &self.lead {
            None => true,
            Some(Lead::Boundary) => {
                before.is_some_and(is_word) != text[at..].chars().next().is_some_and(is_word)
            }
            Some(Lead::Before { class, negate }) => {
                let hit = before.is_some_and(|c| class.is_match(c.encode_utf8(&mut [0; 4])));
                hit != *negate
            }
        }
    }

    /// sre's search from `pos`; `must_advance` forbids an empty match at `pos` (_sre.c
    /// `state->must_advance`).
    fn search_from<'t>(
        &self,
        text: &'t str,
        mut pos: usize,
        mut must_advance: bool,
    ) -> Option<PyMatch<'t>> {
        let next = |i: usize| text[i..].chars().next().map(|c| i + c.len_utf8());
        loop {
            let caps = self.re.captures_at(text, pos)?;
            let start = caps.get(0).map_or(pos, |m| m.start());
            let end = match self.trail {
                Some(g) => caps.get(g).map_or(start, |m| m.start()),
                None => caps.get(0).map_or(start, |m| m.end()),
            };
            if !self.lead_ok(text, start) {
                // The next start where the check holds.
                let mut p = next(start)?;
                while !self.lead_ok(text, p) {
                    p = next(p)?;
                }
                pos = p;
                must_advance = false;
                continue;
            }
            if must_advance && start == pos && end == start {
                pos = next(pos)?;
                must_advance = false;
                continue;
            }
            return Some(PyMatch {
                caps,
                text,
                end,
                groups: self.trail.unwrap_or(usize::MAX),
            });
        }
    }

    /// `pattern.search(text)`.
    pub fn search<'t>(&self, text: &'t str) -> Option<PyMatch<'t>> {
        self.search_from(text, 0, false)
    }

    /// `list(pattern.finditer(text))`.
    pub fn finditer<'t>(&self, text: &'t str) -> Vec<PyMatch<'t>> {
        let mut out = Vec::new();
        let mut pos = 0;
        let mut must_advance = false;
        while pos <= text.len() {
            let Some(m) = self.search_from(text, pos, must_advance) else {
                break;
            };
            must_advance = m.start() == m.end();
            pos = m.end();
            out.push(m);
        }
        out
    }

    /// `pattern.sub(f, text, count)` with `count` 0 for all; `f` gives each replacement.
    pub fn sub_with(
        &self,
        text: &str,
        count: usize,
        mut f: impl FnMut(&PyMatch) -> String,
    ) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (n, m) in self.finditer(text).into_iter().enumerate() {
            if count != 0 && n == count {
                break;
            }
            out.push_str(&text[last..m.start()]);
            out.push_str(&f(&m));
            last = m.end();
        }
        out.push_str(&text[last..]);
        out
    }

    /// `pattern.sub(lambda m: repl, text, count)`: `repl` is literal text.
    pub fn sub(&self, text: &str, repl: &str, count: usize) -> String {
        self.sub_with(text, count, |_| repl.to_string())
    }
}

// ---------------------------------------------------------------- parity

#[cfg(any(test, feature = "parity"))]
fn arg_str(call: &Call, i: usize) -> Result<&str, SmError> {
    call.args
        .get(i)
        .and_then(|v| v.as_str())
        .ok_or_else(|| SmError::type_(format!("parity: argument {i} must be a str")))
}

/// Parity names `py.str.<method>`: the receiver is args[0], as in `str.<method>(s, ...)`.
#[cfg(any(test, feature = "parity"))]
pub fn parity(f: &str, call: &Call) -> Option<Reply> {
    use serde_json::Value;
    Some(match f {
        "py.str.lower" => arg_str(call, 0).map(|s| Value::from(lower(s))),
        "py.str.repr" => arg_str(call, 0).map(|s| Value::from(repr(s))),
        "py.str.len" => arg_str(call, 0).map(|s| Value::from(len(s))),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_whitespace_differs_from_rust() {
        assert!(isspace('\x1c') && isspace('\x1f') && isspace('\u{85}') && isspace('\u{3000}'));
        assert!(!isspace('\u{200b}'));
        assert_eq!(strip("\x1f a \u{3000}"), "a");
        assert_eq!(split(" a\x1cb  c\u{85}"), vec!["a", "b", "c"]);
        assert_eq!(split_ws("  a b  c  ", Some(1)), vec!["a", "b  c  "]);
        assert_eq!(split_ws("", Some(0)), Vec::<&str>::new());
        assert_eq!(split_ws("  a  ", Some(0)), vec!["a  "]);
    }

    #[test]
    fn splitlines_breaks() {
        assert_eq!(
            splitlines("a\r\nb\rc\x0bd\x1ce\u{2028}f\n", false),
            vec!["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(splitlines("a\r\nb\n\n", true), vec!["a\r\n", "b\n", "\n"]);
        assert_eq!(splitlines("", false), Vec::<&str>::new());
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(
            repr("tab\t\x01\x7f\u{85}\u{a0}é\u{200b}😀\u{e000}"),
            "'tab\\t\\x01\\x7f\\x85\\xa0é\\u200b😀\\ue000'"
        );
        assert_eq!(repr(""), "''");
        // A surrogateescape stand-in shows as Python shows the lone surrogate.
        assert_eq!(repr(&decode_surrogateescape(b"a\xff'")), "\"a\\udcff'\"");
        assert_eq!(
            repr_surrogateescape(b"\xc3(\xe2\x82"),
            "'\\udcc3(\\udce2\\udc82'"
        );
        assert_eq!(surrogate_byte('\u{10fd7f}'), None);
    }

    #[test]
    fn lower_matches_python() {
        assert_eq!(lower("İ"), "i\u{307}");
        assert_eq!(lower("ΟΔΟΣ"), "οδος");
        assert_eq!(lower("Σ"), "σ");
        assert_eq!(lower("AΣ'"), "aς'");
        assert_eq!(lower("AΣB"), "aσb");
        assert_eq!(lower("ẞ"), "ß");
    }

    #[test]
    fn slice_by_code_point() {
        assert_eq!(slice("héllo", Some(1), Some(3)), "él");
        assert_eq!(slice("héllo", Some(-2), None), "lo");
        assert_eq!(slice("abc", Some(5), None), "");
        assert_eq!(len("Đorđ"), 4);
    }

    fn findall(p: &str, s: &str) -> Vec<String> {
        PyRegex::new(p)
            .unwrap()
            .finditer(s)
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }

    #[test]
    fn translated_classes_follow_python() {
        let w = regex::Regex::new(&format!("^{}$", *RE_W)).unwrap();
        assert!(w.is_match("²") && w.is_match("_"));
        assert!(!w.is_match("\u{301}") && !w.is_match("\u{203f}"));
        let s = regex::Regex::new(&format!("^{}$", *RE_S)).unwrap();
        assert!(s.is_match("\x1c") && !s.is_match("\u{200b}"));
        assert_eq!(findall(r"\bab\b", "x ab. ab² ab\u{301}"), ["ab", "ab"]);
        assert_eq!(translate(r"[]a]\Z"), "[\\]a]\\z");
        // Class escapes inside brackets (the dedup, redact and recall patterns).
        assert_eq!(
            findall(r"[^\w\s]|_", "a_b,c ²\u{301}\x1c"),
            ["_", ",", "\u{301}"]
        );
        assert_eq!(findall(r"x(?![\w-])", "x. x- x² x"), ["x", "x"]);
        assert_eq!(findall(r"(?<!\w)ab", "ab ²ab _ab .ab"), ["ab", "ab"]);
    }

    #[test]
    fn class_set_syntax_is_literal_as_in_python() {
        // Python 3.15.1: re.findall('[a&&b]+', 'a&&b&c') == ['a&&b&']; the regex crate would
        // read `&&` as an intersection, `--` as a difference and `[` as a nested class.
        assert_eq!(findall("[a&&b]+", "a&&b&c"), ["a&&b&"]);
        assert_eq!(findall("[~~x]+", "~x~y"), ["~x~"]);
        assert_eq!(findall("[[a]+", "[a]["), ["[a", "["]);
        // `--/` is the range '-' to '/', and a `-` after a range is literal.
        assert_eq!(findall("[--/]+", "-./0"), ["-./"]);
        assert_eq!(findall("[a-c-e]+", "b-e d"), ["b-e"]);
        assert_eq!(findall("[a-]+", "a-b"), ["a-"]);
        assert_eq!(findall(r"[\b]", "a\x08"), ["\x08"]);
        assert_eq!(translate("[a&&b]"), "[a\\&\\&b]");
    }

    #[test]
    fn empty_matches_follow_python() {
        // Python 3.7+: re.findall(r'\s*', 'a  b') == ['', '  ', '', ''].
        assert_eq!(findall(r"\s*", "a  b"), ["", "  ", "", ""]);
        let re = PyRegex::new("x*").unwrap();
        assert_eq!(re.sub("abxd", "-", 0), "-a-b--d-");
        assert_eq!(re.sub("abxd", "-", 2), "-a-bxd");
        assert_eq!(findall("", "ab"), ["", "", ""]);
    }

    #[test]
    fn end_checks_take_python_semantics() {
        // A match may start with '-' after a word char: '\b' sits between 'é' and '-'.
        assert_eq!(findall(r"\b[a-z-]+\d\b", "é-ab1 x"), ["-ab1"]);
        // The end check backtracks into the body, as sre does: "ab12" fails before "c".
        assert_eq!(
            findall(r"\b[a-c0-9]+\d(?![a-z])", "ab12cd ab12"),
            ["ab1", "ab12"]
        );
        assert_eq!(
            findall(r"\b[0-9a-f]{3,4}\b", "abcde abc abcd"),
            ["abc", "abcd"]
        );
        assert_eq!(findall(r"(?<=[:/])\d+", "a:12 b3 /4"), ["12", "4"]);
        let re = PyRegex::new(r"^a(b)(?=c)").unwrap();
        let m = re.search("abc").unwrap();
        assert_eq!(
            (m.start(), m.end(), m.group(1), m.group(2)),
            (0, 2, Some("b"), None)
        );
    }

    #[test]
    fn patterns_it_cannot_run_exactly_are_refused() {
        let err = |p: &str| PyRegex::new(p).unwrap_err().python_text();
        assert!(err(r"a\bb").contains("not at an end"));
        assert!(err(r"\ba|b").contains("top-level |"));
        assert!(err(r"\b[a-z-]+\b").contains("non-word char"));
        assert!(err(r"\ba?\b").contains("non-word char"));
        assert!(err(r"\b\d{0,3}\b").contains("non-word char"));
        assert!(err(r"(?<!ab)c").contains("wider than one char"));
        assert!(err(r"c(?!ab)").contains("wider than one char"));
        assert!(PyRegex::new(r"\b\d+?\b").is_ok() && PyRegex::new(r"\b[A-Z]+\d+\b").is_ok());
    }

    #[test]
    fn long_inputs_run_without_a_backtrack_limit() {
        // 100 KB runs that made the lookaround `\b` hit fancy-regex's backtrack limit.
        let kebab = "a-".repeat(50_000);
        assert_eq!(
            findall(r"\b[a-z0-9-]+-\d{8}-\d{6}-[0-9a-f]{6,12}\b", &kebab).len(),
            0
        );
        let hex = "ab12345-".repeat(12_500);
        assert_eq!(findall(r"\b[0-9a-f]{7,40}\b", &hex).len(), 12_500);
        let words = "é".repeat(100_000) + " abcdef1";
        assert_eq!(findall(r"\b[0-9a-f]{7,40}\b", &words), ["abcdef1"]);
    }

    #[test]
    fn by_lower_is_every_char_by_its_lowercase() {
        // Reference: every code point grouped by its simple lowercase (the map by_lower replaced).
        let mut groups: std::collections::HashMap<char, Vec<char>> =
            std::collections::HashMap::new();
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            groups.entry(sre_lower(c)).or_default().push(c);
        }
        assert_eq!(groups.len(), 1_112_064 - 1_488);
        for (&l, want) in &groups {
            let mut got: Vec<char> = by_lower(l).collect();
            got.sort();
            assert_eq!(got, *want, "chars that lower to U+{:04X}", l as u32);
        }
        let extra: Vec<char> = EXTRA_CASES
            .iter()
            .flat_map(|(_, x)| x.iter().filter_map(|&u| char::from_u32(u)))
            .collect();
        assert!(
            extra.iter().all(|c| groups.contains_key(c)),
            "every _EXTRA_CASES member is its own lowercase"
        );
    }

    #[test]
    fn ignorecase_chars_follow_sre() {
        assert_eq!(ci_chars('i'), ['I', 'i', '\u{130}', '\u{131}']);
        assert_eq!(ci_chars('k'), ['K', 'k', '\u{212A}']);
        assert_eq!(ci_chars('s'), ['S', 's', '\u{17F}']);
        assert_eq!(
            ci_chars('ß'),
            ['ß', 'ẞ'],
            "cased by its full uppercase SS, as in Python"
        );
        assert_eq!(ci_chars('²'), ['²'], "uncased: matches itself only");
        assert_eq!(ci_chars('ẞ'), ['ß', 'ẞ']);
        assert_eq!(ci_chars('σ'), ['Σ', 'ς', 'σ']);
        assert_eq!(ci_chars('-'), ['-']);
        assert!(
            regex::Regex::new(&format!("^{}$", ci_literal("Written:")))
                .unwrap()
                .is_match("wrİTTEN:")
        );
        // re.findall('[0-9a-f]', 'aFİK', re.I) == ['a', 'F'].
        assert_eq!(ci_set(('0'..='9').chain('a'..='f')).len(), 22);
        assert_eq!(
            findall(&format!("{}+", ci_class("ik".chars())), "aİıKK k"),
            ["İıKK", "k"]
        );
    }

    #[test]
    fn unicode_versions_agree() {
        // Generated tables, Rust std, and unicode-normalization all ship Unicode 17.0.0.
        assert_eq!(tables::PY_UNIDATA_VERSION, "17.0.0");
        assert_eq!(std::char::UNICODE_VERSION, (17, 0, 0));
        assert_eq!(unicode_normalization::UNICODE_VERSION, (17, 0, 0));
    }
}
