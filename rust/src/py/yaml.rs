//! Port of PyYAML 6.0.3 `yaml.safe_load` for a str input (the pure-Python loader: reader,
//! scanner, parser, composer, YAML 1.1 implicit resolvers and `SafeConstructor`).
//!
//! The port follows the Python code step by step, so the same inputs fail, they fail with the
//! same exception class and `str(exc)`, and the values match. YAML errors carry PyYAML's
//! message with its marks and snippet; the Python errors that escape `safe_load` from a
//! constructor (a `ValueError` from `datetime`, a `KeyError` from `!!bool`) keep their class.

use crate::py::str::repr as repr_str;
use crate::py::value::{Date, DateTime, PyValue, days_in_month};
use regex::Regex;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::LazyLock;

/// Why `safe_load` failed. `str(exc)` is the `Display` text.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadError {
    /// The Python exception class: `ScannerError`, `ParserError`, `ComposerError`,
    /// `ConstructorError`, `ReaderError`, or `ValueError`, `KeyError`, `IndexError`,
    /// `AttributeError` from a constructor.
    pub class: &'static str,
    pub message: String,
}

impl LoadError {
    /// True for `yaml.YAMLError` subclasses, the errors sm's `except yaml.YAMLError` catches.
    pub fn is_yaml_error(&self) -> bool {
        matches!(
            self.class,
            "ScannerError" | "ParserError" | "ComposerError" | "ConstructorError" | "ReaderError"
        )
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoadError {}

/// Python `yaml.safe_load(text)`.
pub fn safe_load(text: &str) -> Result<PyValue, LoadError> {
    let chars: Vec<char> = text.chars().collect();
    check_printable(&chars)?;
    let mut buf = chars;
    buf.push('\0');
    let mut composer = Composer {
        p: Parser::new(Scanner::new(buf)),
        anchors: HashMap::new(),
        nodes: Vec::new(),
    };
    let root = composer.get_single_node();
    let buf = std::mem::take(&mut composer.p.s.buf);
    let mut nodes = std::mem::take(&mut composer.nodes);
    let result = match root {
        Ok(None) => Ok(PyValue::None),
        Ok(Some(root)) => construct_document(&mut nodes, root),
        Err(e) => Err(e),
    };
    result.map_err(|e| e.render(&buf))
}

// ---------------------------------------------------------------- errors and marks

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub index: usize,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug)]
pub(crate) enum Fail {
    Marked(Box<MarkedFail>),
    Py {
        class: &'static str,
        message: String,
    },
}

/// The fields of PyYAML's `MarkedYAMLError` (boxed: errors are rare and `R<T>` stays small).
#[derive(Debug)]
pub(crate) struct MarkedFail {
    class: &'static str,
    context: Option<String>,
    context_mark: Option<Mark>,
    problem: String,
    problem_mark: Option<Mark>,
}

type R<T> = Result<T, Fail>;

fn marked(
    class: &'static str,
    context: Option<&str>,
    context_mark: Option<Mark>,
    problem: String,
    problem_mark: Mark,
) -> Fail {
    Fail::Marked(Box::new(MarkedFail {
        class,
        context: context.map(str::to_string),
        context_mark,
        problem,
        problem_mark: Some(problem_mark),
    }))
}

fn scanner_err(
    context: Option<&str>,
    context_mark: Option<Mark>,
    problem: String,
    mark: Mark,
) -> Fail {
    marked("ScannerError", context, context_mark, problem, mark)
}

fn py_err(class: &'static str, message: String) -> Fail {
    Fail::Py { class, message }
}

const BREAKS_AND_NUL: &str = "\0\r\n\u{85}\u{2028}\u{2029}";

/// PyYAML `Mark.__str__` (`get_snippet` with indent 4 and max_length 75).
fn mark_str(m: &Mark, buf: &[char]) -> String {
    let pointer = m.index;
    let mut head = "";
    let mut start = pointer;
    while start > 0 && !BREAKS_AND_NUL.contains(buf[start - 1]) {
        start -= 1;
        if (pointer - start) as f64 > 75.0 / 2.0 - 1.0 {
            head = " ... ";
            start += 5;
            break;
        }
    }
    let mut tail = "";
    let mut end = pointer;
    while end < buf.len() && !BREAKS_AND_NUL.contains(buf[end]) {
        end += 1;
        if (end - pointer) as f64 > 75.0 / 2.0 - 1.0 {
            tail = " ... ";
            end -= 5;
            break;
        }
    }
    let snippet: String = buf[start..end].iter().collect();
    format!(
        "  in \"<unicode string>\", line {}, column {}:\n    {head}{snippet}{tail}\n{}^",
        m.line + 1,
        m.column + 1,
        " ".repeat(4 + pointer - start + head.len())
    )
}

impl Fail {
    /// PyYAML `MarkedYAMLError.__str__` (no error sets `note` in the safe loader).
    pub(crate) fn render(self, buf: &[char]) -> LoadError {
        match self {
            Fail::Py { class, message } => LoadError { class, message },
            Fail::Marked(m) => {
                let MarkedFail {
                    class,
                    context,
                    context_mark,
                    problem,
                    problem_mark,
                } = *m;
                let mut lines = Vec::new();
                if let Some(c) = context {
                    lines.push(c);
                }
                if let Some(cm) = context_mark
                    && problem_mark.is_none_or(|pm| cm.line != pm.line || cm.column != pm.column)
                {
                    lines.push(mark_str(&cm, buf));
                }
                lines.push(problem);
                if let Some(pm) = problem_mark {
                    lines.push(mark_str(&pm, buf));
                }
                LoadError {
                    class,
                    message: lines.join("\n"),
                }
            }
        }
    }
}

/// PyYAML `Reader.check_printable` on a str input.
fn check_printable(chars: &[char]) -> Result<(), LoadError> {
    let allowed = |c: char| {
        matches!(c, '\t' | '\n' | '\r' | '\x20'..='\x7e' | '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}')
            || c as u32 >= 0x10000
    };
    match chars.iter().position(|&c| !allowed(c)) {
        None => Ok(()),
        Some(pos) => Err(LoadError {
            class: "ReaderError",
            message: format!(
                "unacceptable character #x{:04x}: special characters are not allowed\n  in \"<unicode string>\", position {pos}",
                chars[pos] as u32
            ),
        }),
    }
}

// ---------------------------------------------------------------- scanner

#[derive(Clone, Debug)]
enum Tok {
    StreamStart,
    StreamEnd,
    Directive {
        name: String,
        value: Option<DirValue>,
    },
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowMappingStart,
    FlowSequenceEnd,
    FlowMappingEnd,
    Key,
    Value,
    BlockEntry,
    FlowEntry,
    Alias(String),
    Anchor(String),
    Tag(Option<String>, String),
    Scalar {
        value: String,
        plain: bool,
    },
}

#[derive(Clone, Debug)]
enum DirValue {
    /// `%YAML major.minor`; `major_is_one` is Python's `major == 1` on the parsed int.
    Yaml {
        major_is_one: bool,
    },
    Tag(String, String),
}

impl Tok {
    fn id(&self) -> &'static str {
        match self {
            Tok::StreamStart => "<stream start>",
            Tok::StreamEnd => "<stream end>",
            Tok::Directive { .. } => "<directive>",
            Tok::DocumentStart => "<document start>",
            Tok::DocumentEnd => "<document end>",
            Tok::BlockSequenceStart => "<block sequence start>",
            Tok::BlockMappingStart => "<block mapping start>",
            Tok::BlockEnd => "<block end>",
            Tok::FlowSequenceStart => "[",
            Tok::FlowMappingStart => "{",
            Tok::FlowSequenceEnd => "]",
            Tok::FlowMappingEnd => "}",
            Tok::Key => "?",
            Tok::Value => ":",
            Tok::BlockEntry => "-",
            Tok::FlowEntry => ",",
            Tok::Alias(_) => "<alias>",
            Tok::Anchor(_) => "<anchor>",
            Tok::Tag(..) => "<tag>",
            Tok::Scalar { .. } => "<scalar>",
        }
    }
}

#[derive(Clone, Debug)]
struct Token {
    kind: Tok,
    start: Mark,
    end: Mark,
}

#[derive(Clone, Copy)]
struct SimpleKey {
    token_number: usize,
    required: bool,
    index: usize,
    line: usize,
    column: usize,
    mark: Mark,
}

const WS_BREAKS_NUL: &str = "\0 \t\r\n\u{85}\u{2028}\u{2029}";
const SP_BREAKS_NUL: &str = "\0 \r\n\u{85}\u{2028}\u{2029}";
const BREAKS: &str = "\r\n\u{85}\u{2028}\u{2029}";

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

struct Scanner {
    buf: Vec<char>,
    pointer: usize,
    line: usize,
    column: usize,
    done: bool,
    flow_level: usize,
    tokens: VecDeque<Token>,
    tokens_taken: usize,
    indent: isize,
    indents: Vec<isize>,
    allow_simple_key: bool,
    /// Python dict keyed by flow level; a Vec keeps its insertion order.
    possible_simple_keys: Vec<(usize, SimpleKey)>,
}

impl Scanner {
    fn new(buf: Vec<char>) -> Scanner {
        let mut s = Scanner {
            buf,
            pointer: 0,
            line: 0,
            column: 0,
            done: false,
            flow_level: 0,
            tokens: VecDeque::new(),
            tokens_taken: 0,
            indent: -1,
            indents: Vec::new(),
            allow_simple_key: true,
            possible_simple_keys: Vec::new(),
        };
        let m = s.mark();
        s.tokens.push_back(Token {
            kind: Tok::StreamStart,
            start: m,
            end: m,
        });
        s
    }

    // reader

    fn peek(&self, i: usize) -> char {
        self.buf.get(self.pointer + i).copied().unwrap_or('\0')
    }

    fn ch(&self) -> char {
        self.peek(0)
    }

    fn prefix(&self, n: usize) -> String {
        let end = (self.pointer + n).min(self.buf.len());
        self.buf[self.pointer..end].iter().collect()
    }

    fn forward(&mut self, n: usize) {
        for _ in 0..n {
            let ch = self.buf[self.pointer];
            self.pointer += 1;
            if "\n\u{85}\u{2028}\u{2029}".contains(ch) || (ch == '\r' && self.ch() != '\n') {
                self.line += 1;
                self.column = 0;
            } else if ch != '\u{feff}' {
                self.column += 1;
            }
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            index: self.pointer,
            line: self.line,
            column: self.column,
        }
    }

    // token queue

    fn check_token(&mut self) -> R<&Token> {
        while self.need_more_tokens()? {
            self.fetch_more_tokens()?;
        }
        Ok(self
            .tokens
            .front()
            .expect("the scanner queues StreamEnd before it stops"))
    }

    fn get_token(&mut self) -> R<Token> {
        self.check_token()?;
        self.tokens_taken += 1;
        Ok(self.tokens.pop_front().expect("check_token left a token"))
    }

    fn need_more_tokens(&mut self) -> R<bool> {
        if self.done {
            return Ok(false);
        }
        if self.tokens.is_empty() {
            return Ok(true);
        }
        self.stale_possible_simple_keys()?;
        Ok(self.next_possible_simple_key() == Some(self.tokens_taken))
    }

    fn push(&mut self, kind: Tok, start: Mark, end: Mark) {
        self.tokens.push_back(Token { kind, start, end });
    }

    fn fetch_more_tokens(&mut self) -> R<()> {
        self.scan_to_next_token();
        self.stale_possible_simple_keys()?;
        self.unwind_indent(self.column as isize);
        let ch = self.ch();
        let next_is_blank = WS_BREAKS_NUL.contains(self.peek(1));
        match ch {
            '\0' => return self.fetch_stream_end(),
            '%' if self.column == 0 => return self.fetch_directive(),
            '-' if self.check_document_indicator("---") => {
                return self.fetch_document_indicator(Tok::DocumentStart);
            }
            '.' if self.check_document_indicator("...") => {
                return self.fetch_document_indicator(Tok::DocumentEnd);
            }
            '[' => return self.fetch_flow_collection_start(Tok::FlowSequenceStart),
            '{' => return self.fetch_flow_collection_start(Tok::FlowMappingStart),
            ']' => return self.fetch_flow_collection_end(Tok::FlowSequenceEnd),
            '}' => return self.fetch_flow_collection_end(Tok::FlowMappingEnd),
            ',' => return self.fetch_flow_entry(),
            '-' if next_is_blank => return self.fetch_block_entry(),
            '?' if self.flow_level > 0 || next_is_blank => return self.fetch_key(),
            ':' if self.flow_level > 0 || next_is_blank => return self.fetch_value(),
            '*' => return self.fetch_anchor(true),
            '&' => return self.fetch_anchor(false),
            '!' => return self.fetch_tag(),
            '|' if self.flow_level == 0 => return self.fetch_block_scalar(false),
            '>' if self.flow_level == 0 => return self.fetch_block_scalar(true),
            '\'' => return self.fetch_flow_scalar(false),
            '"' => return self.fetch_flow_scalar(true),
            _ => {}
        }
        if self.check_plain() {
            return self.fetch_plain();
        }
        Err(scanner_err(
            Some("while scanning for the next token"),
            None,
            format!(
                "found character {} that cannot start any token",
                repr_str(&ch.to_string())
            ),
            self.mark(),
        ))
    }

    fn next_possible_simple_key(&self) -> Option<usize> {
        self.possible_simple_keys
            .iter()
            .map(|(_, k)| k.token_number)
            .min()
    }

    fn stale_possible_simple_keys(&mut self) -> R<()> {
        let mut i = 0;
        while i < self.possible_simple_keys.len() {
            let key = self.possible_simple_keys[i].1;
            if key.line != self.line || self.pointer - key.index > 1024 {
                if key.required {
                    return Err(scanner_err(
                        Some("while scanning a simple key"),
                        Some(key.mark),
                        "could not find expected ':'".into(),
                        self.mark(),
                    ));
                }
                self.possible_simple_keys.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    fn save_possible_simple_key(&mut self) -> R<()> {
        let required = self.flow_level == 0 && self.indent == self.column as isize;
        if self.allow_simple_key {
            self.remove_possible_simple_key()?;
            let key = SimpleKey {
                token_number: self.tokens_taken + self.tokens.len(),
                required,
                index: self.pointer,
                line: self.line,
                column: self.column,
                mark: self.mark(),
            };
            self.possible_simple_keys.push((self.flow_level, key));
        }
        Ok(())
    }

    fn remove_possible_simple_key(&mut self) -> R<()> {
        if let Some(i) = self
            .possible_simple_keys
            .iter()
            .position(|(l, _)| *l == self.flow_level)
        {
            let key = self.possible_simple_keys[i].1;
            if key.required {
                return Err(scanner_err(
                    Some("while scanning a simple key"),
                    Some(key.mark),
                    "could not find expected ':'".into(),
                    self.mark(),
                ));
            }
            self.possible_simple_keys.remove(i);
        }
        Ok(())
    }

    fn unwind_indent(&mut self, column: isize) {
        if self.flow_level > 0 {
            return;
        }
        while self.indent > column {
            let m = self.mark();
            self.indent = self
                .indents
                .pop()
                .expect("an indent above -1 has a saved parent");
            self.push(Tok::BlockEnd, m, m);
        }
    }

    fn add_indent(&mut self, column: isize) -> bool {
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            true
        } else {
            false
        }
    }

    fn fetch_stream_end(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        self.possible_simple_keys.clear();
        let m = self.mark();
        self.push(Tok::StreamEnd, m, m);
        self.done = true;
        Ok(())
    }

    fn fetch_directive(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_directive()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn check_document_indicator(&self, s: &str) -> bool {
        self.column == 0 && self.prefix(3) == s && WS_BREAKS_NUL.contains(self.peek(3))
    }

    fn fetch_document_indicator(&mut self, kind: Tok) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let start = self.mark();
        self.forward(3);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, kind: Tok) -> R<()> {
        self.save_possible_simple_key()?;
        self.flow_level += 1;
        self.allow_simple_key = true;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, kind: Tok) -> R<()> {
        self.remove_possible_simple_key()?;
        // Python's `flow_level -= 1` may go below 0 on a stray `]`; the parser then fails.
        self.flow_level = self.flow_level.saturating_sub(1);
        self.allow_simple_key = false;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::FlowEntry, start, end);
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(scanner_err(
                    None,
                    None,
                    "sequence entries are not allowed here".into(),
                    self.mark(),
                ));
            }
            if self.add_indent(self.column as isize) {
                let m = self.mark();
                self.push(Tok::BlockSequenceStart, m, m);
            }
        }
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::BlockEntry, start, end);
        Ok(())
    }

    fn fetch_key(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(scanner_err(
                    None,
                    None,
                    "mapping keys are not allowed here".into(),
                    self.mark(),
                ));
            }
            if self.add_indent(self.column as isize) {
                let m = self.mark();
                self.push(Tok::BlockMappingStart, m, m);
            }
        }
        self.allow_simple_key = self.flow_level == 0;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::Key, start, end);
        Ok(())
    }

    fn fetch_value(&mut self) -> R<()> {
        if let Some(i) = self
            .possible_simple_keys
            .iter()
            .position(|(l, _)| *l == self.flow_level)
        {
            let key = self.possible_simple_keys.remove(i).1;
            let at = key.token_number - self.tokens_taken;
            self.tokens.insert(
                at,
                Token {
                    kind: Tok::Key,
                    start: key.mark,
                    end: key.mark,
                },
            );
            if self.flow_level == 0 && self.add_indent(key.column as isize) {
                self.tokens.insert(
                    at,
                    Token {
                        kind: Tok::BlockMappingStart,
                        start: key.mark,
                        end: key.mark,
                    },
                );
            }
            self.allow_simple_key = false;
        } else {
            if self.flow_level == 0 {
                if !self.allow_simple_key {
                    return Err(scanner_err(
                        None,
                        None,
                        "mapping values are not allowed here".into(),
                        self.mark(),
                    ));
                }
                if self.add_indent(self.column as isize) {
                    let m = self.mark();
                    self.push(Tok::BlockMappingStart, m, m);
                }
            }
            self.allow_simple_key = self.flow_level == 0;
            self.remove_possible_simple_key()?;
        }
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::Value, start, end);
        Ok(())
    }

    fn fetch_anchor(&mut self, alias: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_anchor(alias)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_tag(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_tag()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, folded: bool) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let t = self.scan_block_scalar(folded)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, double: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_flow_scalar(double)?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn fetch_plain(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let t = self.scan_plain()?;
        self.tokens.push_back(t);
        Ok(())
    }

    fn check_plain(&self) -> bool {
        let ch = self.ch();
        !"\0 \t\r\n\u{85}\u{2028}\u{2029}-?:,[]{}#&*!|>'\"%@`".contains(ch)
            || (!WS_BREAKS_NUL.contains(self.peek(1))
                && (ch == '-' || (self.flow_level == 0 && "?:".contains(ch))))
    }

    fn scan_to_next_token(&mut self) {
        if self.pointer == 0 && self.ch() == '\u{feff}' {
            self.forward(1);
        }
        loop {
            while self.ch() == ' ' {
                self.forward(1);
            }
            if self.ch() == '#' {
                while !BREAKS_AND_NUL.contains(self.ch()) {
                    self.forward(1);
                }
            }
            if self.scan_line_break().is_empty() {
                break;
            }
            if self.flow_level == 0 {
                self.allow_simple_key = true;
            }
        }
    }

    fn directive_err(&self, start: Mark, problem: String) -> Fail {
        scanner_err(
            Some("while scanning a directive"),
            Some(start),
            problem,
            self.mark(),
        )
    }

    fn scan_directive(&mut self) -> R<Token> {
        let start = self.mark();
        self.forward(1);
        let name = self.scan_directive_name(start)?;
        let value;
        let end;
        if name == "YAML" {
            value = Some(self.scan_yaml_directive_value(start)?);
            end = self.mark();
        } else if name == "TAG" {
            value = Some(self.scan_tag_directive_value(start)?);
            end = self.mark();
        } else {
            value = None;
            end = self.mark();
            while !BREAKS_AND_NUL.contains(self.ch()) {
                self.forward(1);
            }
        }
        self.scan_directive_ignored_line(start)?;
        Ok(Token {
            kind: Tok::Directive { name, value },
            start,
            end,
        })
    }

    fn scan_directive_name(&mut self, start: Mark) -> R<String> {
        let mut n = 0;
        while is_word(self.peek(n)) {
            n += 1;
        }
        if n == 0 {
            let p = format!(
                "expected alphabetic or numeric character, but found {}",
                repr_char(self.peek(n))
            );
            return Err(self.directive_err(start, p));
        }
        let v = self.prefix(n);
        self.forward(n);
        if !SP_BREAKS_NUL.contains(self.ch()) {
            let p = format!(
                "expected alphabetic or numeric character, but found {}",
                repr_char(self.ch())
            );
            return Err(self.directive_err(start, p));
        }
        Ok(v)
    }

    fn scan_yaml_directive_value(&mut self, start: Mark) -> R<DirValue> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        let major = self.scan_yaml_directive_number(start)?;
        if self.ch() != '.' {
            let p = format!(
                "expected a digit or '.', but found {}",
                repr_char(self.ch())
            );
            return Err(self.directive_err(start, p));
        }
        self.forward(1);
        self.scan_yaml_directive_number(start)?;
        if !SP_BREAKS_NUL.contains(self.ch()) {
            let p = format!(
                "expected a digit or ' ', but found {}",
                repr_char(self.ch())
            );
            return Err(self.directive_err(start, p));
        }
        Ok(DirValue::Yaml {
            major_is_one: major.trim_start_matches('0') == "1",
        })
    }

    fn scan_yaml_directive_number(&mut self, start: Mark) -> R<String> {
        if !self.ch().is_ascii_digit() {
            let p = format!("expected a digit, but found {}", repr_char(self.ch()));
            return Err(self.directive_err(start, p));
        }
        let mut n = 0;
        while self.peek(n).is_ascii_digit() {
            n += 1;
        }
        let v = self.prefix(n);
        self.forward(n);
        Ok(v)
    }

    fn scan_tag_directive_value(&mut self, start: Mark) -> R<DirValue> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        let handle = self.scan_tag_handle("directive", start)?;
        if self.ch() != ' ' {
            let p = format!("expected ' ', but found {}", repr_char(self.ch()));
            return Err(self.directive_err(start, p));
        }
        while self.ch() == ' ' {
            self.forward(1);
        }
        let prefix = self.scan_tag_uri("directive", start)?;
        if !SP_BREAKS_NUL.contains(self.ch()) {
            let p = format!("expected ' ', but found {}", repr_char(self.ch()));
            return Err(self.directive_err(start, p));
        }
        Ok(DirValue::Tag(handle, prefix))
    }

    fn scan_directive_ignored_line(&mut self, start: Mark) -> R<()> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        if self.ch() == '#' {
            while !BREAKS_AND_NUL.contains(self.ch()) {
                self.forward(1);
            }
        }
        if !BREAKS_AND_NUL.contains(self.ch()) {
            let p = format!(
                "expected a comment or a line break, but found {}",
                repr_char(self.ch())
            );
            return Err(self.directive_err(start, p));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_anchor(&mut self, alias: bool) -> R<Token> {
        let start = self.mark();
        let name = if alias { "alias" } else { "anchor" };
        self.forward(1);
        let mut n = 0;
        while is_word(self.peek(n)) {
            n += 1;
        }
        let ctx = format!("while scanning an {name}");
        if n == 0 {
            let p = format!(
                "expected alphabetic or numeric character, but found {}",
                repr_char(self.peek(n))
            );
            return Err(scanner_err(Some(&ctx), Some(start), p, self.mark()));
        }
        let value = self.prefix(n);
        self.forward(n);
        if !"\0 \t\r\n\u{85}\u{2028}\u{2029}?:,]}%@`".contains(self.ch()) {
            let p = format!(
                "expected alphabetic or numeric character, but found {}",
                repr_char(self.ch())
            );
            return Err(scanner_err(Some(&ctx), Some(start), p, self.mark()));
        }
        let end = self.mark();
        let kind = if alias {
            Tok::Alias(value)
        } else {
            Tok::Anchor(value)
        };
        Ok(Token { kind, start, end })
    }

    fn scan_tag(&mut self) -> R<Token> {
        let start = self.mark();
        let mut ch = self.peek(1);
        let handle;
        let suffix;
        if ch == '<' {
            handle = None;
            self.forward(2);
            suffix = self.scan_tag_uri("tag", start)?;
            if self.ch() != '>' {
                let p = format!("expected '>', but found {}", repr_char(self.ch()));
                return Err(scanner_err(
                    Some("while parsing a tag"),
                    Some(start),
                    p,
                    self.mark(),
                ));
            }
            self.forward(1);
        } else if WS_BREAKS_NUL.contains(ch) {
            handle = None;
            suffix = "!".to_string();
            self.forward(1);
        } else {
            let mut n = 1;
            let mut use_handle = false;
            while !SP_BREAKS_NUL.contains(ch) {
                if ch == '!' {
                    use_handle = true;
                    break;
                }
                n += 1;
                ch = self.peek(n);
            }
            if use_handle {
                handle = Some(self.scan_tag_handle("tag", start)?);
            } else {
                handle = Some("!".to_string());
                self.forward(1);
            }
            suffix = self.scan_tag_uri("tag", start)?;
        }
        if !SP_BREAKS_NUL.contains(self.ch()) {
            let p = format!("expected ' ', but found {}", repr_char(self.ch()));
            return Err(scanner_err(
                Some("while scanning a tag"),
                Some(start),
                p,
                self.mark(),
            ));
        }
        let end = self.mark();
        Ok(Token {
            kind: Tok::Tag(handle, suffix),
            start,
            end,
        })
    }

    fn scan_block_scalar(&mut self, folded: bool) -> R<Token> {
        let mut chunks = String::new();
        let start = self.mark();
        self.forward(1);
        let (chomping, increment) = self.scan_block_scalar_indicators(start)?;
        self.scan_block_scalar_ignored_line(start)?;
        let min_indent = (self.indent + 1).max(1) as usize;
        let (mut breaks, mut end, indent) = match increment {
            None => {
                let (b, max_indent, e) = self.scan_block_scalar_indentation();
                (b, e, min_indent.max(max_indent))
            }
            Some(inc) => {
                let indent = min_indent + inc - 1;
                let (b, e) = self.scan_block_scalar_breaks(indent);
                (b, e, indent)
            }
        };
        let mut line_break = String::new();
        while self.column == indent && self.ch() != '\0' {
            chunks.push_str(&breaks.concat());
            let leading_non_space = !" \t".contains(self.ch());
            let mut n = 0;
            while !BREAKS_AND_NUL.contains(self.peek(n)) {
                n += 1;
            }
            chunks.push_str(&self.prefix(n));
            self.forward(n);
            line_break = self.scan_line_break();
            let (b, e) = self.scan_block_scalar_breaks(indent);
            breaks = b;
            end = e;
            if self.column == indent && self.ch() != '\0' {
                if folded && line_break == "\n" && leading_non_space && !" \t".contains(self.ch()) {
                    if breaks.is_empty() {
                        chunks.push(' ');
                    }
                } else {
                    chunks.push_str(&line_break);
                }
            } else {
                break;
            }
        }
        if chomping != Some(false) {
            chunks.push_str(&line_break);
        }
        if chomping == Some(true) {
            chunks.push_str(&breaks.concat());
        }
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: false,
            },
            start,
            end,
        })
    }

    fn scan_block_scalar_indicators(&mut self, start: Mark) -> R<(Option<bool>, Option<usize>)> {
        let ctx = Some("while scanning a block scalar");
        let zero = "expected indentation indicator in the range 1-9, but found 0";
        let mut chomping = None;
        let mut increment = None;
        let ch = self.ch();
        if ch == '+' || ch == '-' {
            chomping = Some(ch == '+');
            self.forward(1);
            let ch = self.ch();
            if ch.is_ascii_digit() {
                if ch == '0' {
                    return Err(scanner_err(ctx, Some(start), zero.into(), self.mark()));
                }
                increment = Some(ch as usize - '0' as usize);
                self.forward(1);
            }
        } else if ch.is_ascii_digit() {
            if ch == '0' {
                return Err(scanner_err(ctx, Some(start), zero.into(), self.mark()));
            }
            increment = Some(ch as usize - '0' as usize);
            self.forward(1);
            let ch = self.ch();
            if ch == '+' || ch == '-' {
                chomping = Some(ch == '+');
                self.forward(1);
            }
        }
        if !SP_BREAKS_NUL.contains(self.ch()) {
            let p = format!(
                "expected chomping or indentation indicators, but found {}",
                repr_char(self.ch())
            );
            return Err(scanner_err(ctx, Some(start), p, self.mark()));
        }
        Ok((chomping, increment))
    }

    fn scan_block_scalar_ignored_line(&mut self, start: Mark) -> R<()> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        if self.ch() == '#' {
            while !BREAKS_AND_NUL.contains(self.ch()) {
                self.forward(1);
            }
        }
        if !BREAKS_AND_NUL.contains(self.ch()) {
            let p = format!(
                "expected a comment or a line break, but found {}",
                repr_char(self.ch())
            );
            return Err(scanner_err(
                Some("while scanning a block scalar"),
                Some(start),
                p,
                self.mark(),
            ));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_block_scalar_indentation(&mut self) -> (Vec<String>, usize, Mark) {
        let mut chunks = Vec::new();
        let mut max_indent = 0;
        let mut end = self.mark();
        while " \r\n\u{85}\u{2028}\u{2029}".contains(self.ch()) {
            if self.ch() != ' ' {
                chunks.push(self.scan_line_break());
                end = self.mark();
            } else {
                self.forward(1);
                max_indent = max_indent.max(self.column);
            }
        }
        (chunks, max_indent, end)
    }

    fn scan_block_scalar_breaks(&mut self, indent: usize) -> (Vec<String>, Mark) {
        let mut chunks = Vec::new();
        let mut end = self.mark();
        while self.column < indent && self.ch() == ' ' {
            self.forward(1);
        }
        while BREAKS.contains(self.ch()) {
            chunks.push(self.scan_line_break());
            end = self.mark();
            while self.column < indent && self.ch() == ' ' {
                self.forward(1);
            }
        }
        (chunks, end)
    }

    fn scan_flow_scalar(&mut self, double: bool) -> R<Token> {
        let mut chunks = String::new();
        let start = self.mark();
        let quote = self.ch();
        self.forward(1);
        self.scan_flow_scalar_non_spaces(double, start, &mut chunks)?;
        while self.ch() != quote {
            self.scan_flow_scalar_spaces(double, start, &mut chunks)?;
            self.scan_flow_scalar_non_spaces(double, start, &mut chunks)?;
        }
        self.forward(1);
        let end = self.mark();
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: false,
            },
            start,
            end,
        })
    }

    fn scan_flow_scalar_non_spaces(
        &mut self,
        double: bool,
        start: Mark,
        chunks: &mut String,
    ) -> R<()> {
        let ctx = Some("while scanning a double-quoted scalar");
        loop {
            let mut n = 0;
            while !"'\"\\\0 \t\r\n\u{85}\u{2028}\u{2029}".contains(self.peek(n)) {
                n += 1;
            }
            if n > 0 {
                chunks.push_str(&self.prefix(n));
                self.forward(n);
            }
            let ch = self.ch();
            if !double && ch == '\'' && self.peek(1) == '\'' {
                chunks.push('\'');
                self.forward(2);
            } else if (double && ch == '\'') || (!double && (ch == '"' || ch == '\\')) {
                chunks.push(ch);
                self.forward(1);
            } else if double && ch == '\\' {
                self.forward(1);
                let ch = self.ch();
                if let Some(r) = escape_replacement(ch) {
                    chunks.push(r);
                    self.forward(1);
                } else if let Some(len) = match ch {
                    'x' => Some(2),
                    'u' => Some(4),
                    'U' => Some(8),
                    _ => None,
                } {
                    self.forward(1);
                    for k in 0..len {
                        if !self.peek(k).is_ascii_hexdigit() {
                            let p = format!(
                                "expected escape sequence of {len} hexadecimal numbers, but found {}",
                                repr_char(self.peek(k))
                            );
                            return Err(scanner_err(ctx, Some(start), p, self.mark()));
                        }
                    }
                    let code =
                        u32::from_str_radix(&self.prefix(len), 16).expect("checked hex digits");
                    chunks.push(chr(code)?);
                    self.forward(len);
                } else if BREAKS.contains(ch) {
                    self.scan_line_break();
                    self.scan_flow_scalar_breaks(start, chunks)?;
                } else {
                    let p = format!("found unknown escape character {}", repr_char(ch));
                    return Err(scanner_err(ctx, Some(start), p, self.mark()));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn scan_flow_scalar_spaces(
        &mut self,
        _double: bool,
        start: Mark,
        chunks: &mut String,
    ) -> R<()> {
        let mut n = 0;
        while " \t".contains(self.peek(n)) {
            n += 1;
        }
        let whitespaces = self.prefix(n);
        self.forward(n);
        let ch = self.ch();
        if ch == '\0' {
            return Err(scanner_err(
                Some("while scanning a quoted scalar"),
                Some(start),
                "found unexpected end of stream".into(),
                self.mark(),
            ));
        } else if BREAKS.contains(ch) {
            let line_break = self.scan_line_break();
            let mut breaks = String::new();
            self.scan_flow_scalar_breaks(start, &mut breaks)?;
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if breaks.is_empty() {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else {
            chunks.push_str(&whitespaces);
        }
        Ok(())
    }

    fn scan_flow_scalar_breaks(&mut self, start: Mark, chunks: &mut String) -> R<()> {
        loop {
            let prefix = self.prefix(3);
            if (prefix == "---" || prefix == "...") && WS_BREAKS_NUL.contains(self.peek(3)) {
                return Err(scanner_err(
                    Some("while scanning a quoted scalar"),
                    Some(start),
                    "found unexpected document separator".into(),
                    self.mark(),
                ));
            }
            while " \t".contains(self.ch()) {
                self.forward(1);
            }
            if BREAKS.contains(self.ch()) {
                chunks.push_str(&self.scan_line_break());
            } else {
                return Ok(());
            }
        }
    }

    fn scan_plain(&mut self) -> R<Token> {
        let mut chunks = String::new();
        let start = self.mark();
        let mut end = start;
        let indent = self.indent + 1;
        let mut spaces: Option<String> = Some(String::new());
        loop {
            let mut n = 0;
            if self.ch() == '#' {
                break;
            }
            loop {
                let ch = self.peek(n);
                let after = self.peek(n + 1);
                if WS_BREAKS_NUL.contains(ch)
                    || (ch == ':'
                        && (WS_BREAKS_NUL.contains(after)
                            || (self.flow_level > 0 && ",[]{}".contains(after))))
                    || (self.flow_level > 0 && ",?[]{}".contains(ch))
                {
                    break;
                }
                n += 1;
            }
            if n == 0 {
                break;
            }
            self.allow_simple_key = false;
            chunks.push_str(spaces.as_deref().unwrap_or(""));
            chunks.push_str(&self.prefix(n));
            self.forward(n);
            end = self.mark();
            spaces = self.scan_plain_spaces();
            match &spaces {
                None => break,
                Some(s) if s.is_empty() => break,
                _ => {}
            }
            if self.ch() == '#' || (self.flow_level == 0 && (self.column as isize) < indent) {
                break;
            }
        }
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: true,
            },
            start,
            end,
        })
    }

    /// `None` is Python's bare `return` (a document separator follows); `Some("")` is `[]`.
    fn scan_plain_spaces(&mut self) -> Option<String> {
        let mut chunks = String::new();
        let mut n = 0;
        while self.peek(n) == ' ' {
            n += 1;
        }
        let whitespaces = self.prefix(n);
        self.forward(n);
        let ch = self.ch();
        let separator = |s: &Scanner| {
            let p = s.prefix(3);
            (p == "---" || p == "...") && WS_BREAKS_NUL.contains(s.peek(3))
        };
        if BREAKS.contains(ch) {
            let line_break = self.scan_line_break();
            self.allow_simple_key = true;
            if separator(self) {
                return None;
            }
            let mut breaks = String::new();
            while " \r\n\u{85}\u{2028}\u{2029}".contains(self.ch()) {
                if self.ch() == ' ' {
                    self.forward(1);
                } else {
                    breaks.push_str(&self.scan_line_break());
                    if separator(self) {
                        return None;
                    }
                }
            }
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if breaks.is_empty() {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else if !whitespaces.is_empty() {
            chunks.push_str(&whitespaces);
        }
        Some(chunks)
    }

    fn scan_tag_handle(&mut self, name: &str, start: Mark) -> R<String> {
        let ctx = format!("while scanning a {name}");
        if self.ch() != '!' {
            let p = format!("expected '!', but found {}", repr_char(self.ch()));
            return Err(scanner_err(Some(&ctx), Some(start), p, self.mark()));
        }
        let mut n = 1;
        let mut ch = self.peek(n);
        if ch != ' ' {
            while is_word(ch) {
                n += 1;
                ch = self.peek(n);
            }
            if ch != '!' {
                self.forward(n);
                let p = format!("expected '!', but found {}", repr_char(ch));
                return Err(scanner_err(Some(&ctx), Some(start), p, self.mark()));
            }
            n += 1;
        }
        let v = self.prefix(n);
        self.forward(n);
        Ok(v)
    }

    fn scan_tag_uri(&mut self, name: &str, start: Mark) -> R<String> {
        let mut chunks = String::new();
        let mut n = 0;
        let mut ch = self.peek(n);
        while ch.is_ascii_alphanumeric() || "-;/?:@&=+$,_.!~*'()[]%".contains(ch) {
            if ch == '%' {
                chunks.push_str(&self.prefix(n));
                self.forward(n);
                n = 0;
                chunks.push_str(&self.scan_uri_escapes(name, start)?);
            } else {
                n += 1;
            }
            ch = self.peek(n);
        }
        if n > 0 {
            chunks.push_str(&self.prefix(n));
            self.forward(n);
        }
        if chunks.is_empty() {
            let p = format!("expected URI, but found {}", repr_char(ch));
            return Err(scanner_err(
                Some(&format!("while parsing a {name}")),
                Some(start),
                p,
                self.mark(),
            ));
        }
        Ok(chunks)
    }

    fn scan_uri_escapes(&mut self, name: &str, start: Mark) -> R<String> {
        let ctx = format!("while scanning a {name}");
        let mut codes = Vec::new();
        let mark = self.mark();
        while self.ch() == '%' {
            self.forward(1);
            for k in 0..2 {
                if !self.peek(k).is_ascii_hexdigit() {
                    let p = format!(
                        "expected URI escape sequence of 2 hexadecimal numbers, but found {}",
                        repr_char(self.peek(k))
                    );
                    return Err(scanner_err(Some(&ctx), Some(start), p, self.mark()));
                }
            }
            codes.push(u8::from_str_radix(&self.prefix(2), 16).expect("checked hex digits"));
            self.forward(2);
        }
        String::from_utf8(codes.clone())
            .map_err(|e| scanner_err(Some(&ctx), Some(start), utf8_decode_error(&codes, &e), mark))
    }

    fn scan_line_break(&mut self) -> String {
        let ch = self.ch();
        if "\r\n\u{85}".contains(ch) {
            if self.prefix(2) == "\r\n" {
                self.forward(2);
            } else {
                self.forward(1);
            }
            "\n".into()
        } else if ch == '\u{2028}' || ch == '\u{2029}' {
            self.forward(1);
            ch.to_string()
        } else {
            String::new()
        }
    }
}

fn repr_char(c: char) -> String {
    repr_str(&c.to_string())
}

fn escape_replacement(c: char) -> Option<char> {
    Some(match c {
        '0' => '\0',
        'a' => '\x07',
        'b' => '\x08',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\x0b',
        'f' => '\x0c',
        'r' => '\r',
        'e' => '\x1b',
        ' ' => ' ',
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        _ => return None,
    })
}

/// Python `chr(code)` for an escape. A lone surrogate U+DC80-U+DCFF maps to its stand-in byte
/// scalar, as in `json.loads`; any other lone surrogate is a valid Python str but not a Rust
/// `char`, so it becomes U+FFFD.
fn chr(code: u32) -> R<char> {
    if code > 0x10ffff {
        return Err(py_err(
            "ValueError",
            "chr() arg not in range(0x110000)".into(),
        ));
    }
    Ok(crate::py::json::surrogate_char(code))
}

/// Python's `str(UnicodeDecodeError)` for `bytes(codes).decode('utf-8')`.
fn utf8_decode_error(codes: &[u8], e: &std::string::FromUtf8Error) -> String {
    let u = e.utf8_error();
    let start = u.valid_up_to();
    let reason = match u.error_len() {
        None => "unexpected end of data",
        Some(_) if (0x80..0xc2).contains(&codes[start]) || codes[start] > 0xf4 => {
            "invalid start byte"
        }
        Some(_) => "invalid continuation byte",
    };
    let end = start + u.error_len().unwrap_or(codes.len() - start);
    if end - start == 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            codes[start]
        )
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    }
}

// ---------------------------------------------------------------- parser

#[derive(Clone, Debug)]
pub(crate) enum Ev {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias(String),
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        implicit: (bool, bool),
        value: String,
    },
    SequenceStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    SequenceEnd,
    MappingStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    MappingEnd,
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub kind: Ev,
    pub start: Mark,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    ImplicitDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentContent,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
}

struct Parser {
    s: Scanner,
    current: Option<Event>,
    started: bool,
    tag_handles: Vec<(String, String)>,
    states: Vec<State>,
    marks: Vec<Mark>,
    state: Option<State>,
}

fn default_tags() -> Vec<(String, String)> {
    vec![
        ("!".into(), "!".into()),
        ("!!".into(), "tag:yaml.org,2002:".into()),
    ]
}

fn parser_err(
    context: Option<&str>,
    context_mark: Option<Mark>,
    problem: String,
    mark: Mark,
) -> Fail {
    marked("ParserError", context, context_mark, problem, mark)
}

impl Parser {
    fn new(s: Scanner) -> Parser {
        Parser {
            s,
            current: None,
            started: false,
            tag_handles: Vec::new(),
            states: Vec::new(),
            marks: Vec::new(),
            state: None,
        }
    }

    fn peek_event(&mut self) -> R<Option<&Event>> {
        if self.current.is_none() {
            self.current = self.next_state()?;
        }
        Ok(self.current.as_ref())
    }

    fn get_event(&mut self) -> R<Option<Event>> {
        if self.current.is_none() {
            self.current = self.next_state()?;
        }
        Ok(self.current.take())
    }

    fn next_state(&mut self) -> R<Option<Event>> {
        if !self.started {
            self.started = true;
            let t = self.s.get_token()?;
            self.state = Some(State::ImplicitDocumentStart);
            return Ok(Some(Event {
                kind: Ev::StreamStart,
                start: t.start,
            }));
        }
        let Some(state) = self.state else {
            return Ok(None);
        };
        let ev = match state {
            State::ImplicitDocumentStart => self.parse_implicit_document_start()?,
            State::DocumentStart => self.parse_document_start()?,
            State::DocumentEnd => self.parse_document_end()?,
            State::DocumentContent => self.parse_document_content()?,
            State::BlockNode => self.parse_node(true, false)?,
            State::BlockSequenceFirstEntry => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_block_sequence_entry()?
            }
            State::BlockSequenceEntry => self.parse_block_sequence_entry()?,
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry()?,
            State::BlockMappingFirstKey => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_block_mapping_key()?
            }
            State::BlockMappingKey => self.parse_block_mapping_key()?,
            State::BlockMappingValue => self.parse_block_mapping_value()?,
            State::FlowSequenceFirstEntry => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_flow_sequence_entry(true)?
            }
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false)?,
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key()?,
            State::FlowSequenceEntryMappingValue => {
                self.parse_flow_sequence_entry_mapping_value()?
            }
            State::FlowSequenceEntryMappingEnd => {
                self.state = Some(State::FlowSequenceEntry);
                let t = self.peek()?;
                Event {
                    kind: Ev::MappingEnd,
                    start: t.start,
                }
            }
            State::FlowMappingFirstKey => {
                let t = self.s.get_token()?;
                self.marks.push(t.start);
                self.parse_flow_mapping_key(true)?
            }
            State::FlowMappingKey => self.parse_flow_mapping_key(false)?,
            State::FlowMappingValue => self.parse_flow_mapping_value()?,
            State::FlowMappingEmptyValue => {
                self.state = Some(State::FlowMappingKey);
                let m = self.peek()?.start;
                empty_scalar(m)
            }
        };
        Ok(Some(ev))
    }

    fn peek(&mut self) -> R<Token> {
        Ok(self.s.check_token()?.clone())
    }

    fn is(&mut self, f: fn(&Tok) -> bool) -> R<bool> {
        Ok(f(&self.s.check_token()?.kind))
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop();
    }

    fn parse_implicit_document_start(&mut self) -> R<Event> {
        if !self.is(|t| {
            matches!(
                t,
                Tok::Directive { .. } | Tok::DocumentStart | Tok::StreamEnd
            )
        })? {
            self.tag_handles = default_tags();
            let t = self.peek()?;
            self.states.push(State::DocumentEnd);
            self.state = Some(State::BlockNode);
            Ok(Event {
                kind: Ev::DocumentStart,
                start: t.start,
            })
        } else {
            self.parse_document_start()
        }
    }

    fn parse_document_start(&mut self) -> R<Event> {
        while self.is(|t| matches!(t, Tok::DocumentEnd))? {
            self.s.get_token()?;
        }
        if !self.is(|t| matches!(t, Tok::StreamEnd))? {
            let start = self.peek()?.start;
            self.process_directives()?;
            if !self.is(|t| matches!(t, Tok::DocumentStart))? {
                let t = self.peek()?;
                return Err(parser_err(
                    None,
                    None,
                    format!(
                        "expected '<document start>', but found {}",
                        repr_str(t.kind.id())
                    ),
                    t.start,
                ));
            }
            self.s.get_token()?;
            self.states.push(State::DocumentEnd);
            self.state = Some(State::DocumentContent);
            Ok(Event {
                kind: Ev::DocumentStart,
                start,
            })
        } else {
            let t = self.s.get_token()?;
            self.state = None;
            Ok(Event {
                kind: Ev::StreamEnd,
                start: t.start,
            })
        }
    }

    fn parse_document_end(&mut self) -> R<Event> {
        let start = self.peek()?.start;
        if self.is(|t| matches!(t, Tok::DocumentEnd))? {
            self.s.get_token()?;
        }
        self.state = Some(State::DocumentStart);
        Ok(Event {
            kind: Ev::DocumentEnd,
            start,
        })
    }

    fn parse_document_content(&mut self) -> R<Event> {
        if self.is(|t| {
            matches!(
                t,
                Tok::Directive { .. } | Tok::DocumentStart | Tok::DocumentEnd | Tok::StreamEnd
            )
        })? {
            let m = self.peek()?.start;
            self.pop_state();
            Ok(empty_scalar(m))
        } else {
            self.parse_node(true, false)
        }
    }

    fn process_directives(&mut self) -> R<()> {
        let mut yaml_seen = false;
        self.tag_handles = Vec::new();
        while self.is(|t| matches!(t, Tok::Directive { .. }))? {
            let t = self.s.get_token()?;
            let Tok::Directive { name, value } = t.kind else {
                unreachable!("checked a directive")
            };
            match (name.as_str(), value) {
                ("YAML", Some(DirValue::Yaml { major_is_one })) => {
                    if yaml_seen {
                        return Err(parser_err(
                            None,
                            None,
                            "found duplicate YAML directive".into(),
                            t.start,
                        ));
                    }
                    if !major_is_one {
                        return Err(parser_err(
                            None,
                            None,
                            "found incompatible YAML document (version 1.* is required)".into(),
                            t.start,
                        ));
                    }
                    yaml_seen = true;
                }
                ("TAG", Some(DirValue::Tag(handle, prefix))) => {
                    if self.tag_handles.iter().any(|(h, _)| *h == handle) {
                        return Err(parser_err(
                            None,
                            None,
                            format!("duplicate tag handle {}", repr_str(&handle)),
                            t.start,
                        ));
                    }
                    self.tag_handles.push((handle, prefix));
                }
                _ => {}
            }
        }
        for (k, v) in default_tags() {
            if !self.tag_handles.iter().any(|(h, _)| *h == k) {
                self.tag_handles.push((k, v));
            }
        }
        Ok(())
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> R<Event> {
        if self.is(|t| matches!(t, Tok::Alias(_)))? {
            let t = self.s.get_token()?;
            let Tok::Alias(name) = t.kind else {
                unreachable!("checked an alias")
            };
            self.pop_state();
            return Ok(Event {
                kind: Ev::Alias(name),
                start: t.start,
            });
        }
        let mut anchor = None;
        let mut tag: Option<(Option<String>, String)> = None;
        let mut start_mark = None;
        let mut tag_mark = None;
        if self.is(|t| matches!(t, Tok::Anchor(_)))? {
            let t = self.s.get_token()?;
            start_mark = Some(t.start);
            if let Tok::Anchor(a) = t.kind {
                anchor = Some(a);
            }
            if self.is(|t| matches!(t, Tok::Tag(..)))? {
                let t = self.s.get_token()?;
                tag_mark = Some(t.start);
                if let Tok::Tag(h, s) = t.kind {
                    tag = Some((h, s));
                }
            }
        } else if self.is(|t| matches!(t, Tok::Tag(..)))? {
            let t = self.s.get_token()?;
            start_mark = Some(t.start);
            tag_mark = Some(t.start);
            if let Tok::Tag(h, s) = t.kind {
                tag = Some((h, s));
            }
            if self.is(|t| matches!(t, Tok::Anchor(_)))? {
                let t = self.s.get_token()?;
                if let Tok::Anchor(a) = t.kind {
                    anchor = Some(a);
                }
            }
        }
        let tag: Option<String> = match tag {
            None => None,
            Some((None, suffix)) => Some(suffix),
            Some((Some(handle), suffix)) => {
                match self.tag_handles.iter().find(|(h, _)| *h == handle) {
                    Some((_, prefix)) => Some(format!("{prefix}{suffix}")),
                    None => {
                        return Err(parser_err(
                            Some("while parsing a node"),
                            start_mark,
                            format!("found undefined tag handle {}", repr_str(&handle)),
                            tag_mark.expect("a tag token sets tag_mark"),
                        ));
                    }
                }
            }
        };
        let start = match start_mark {
            Some(m) => m,
            None => self.peek()?.start,
        };
        if indentless_sequence && self.is(|t| matches!(t, Tok::BlockEntry))? {
            self.state = Some(State::IndentlessSequenceEntry);
            return Ok(Event {
                kind: Ev::SequenceStart { anchor, tag },
                start,
            });
        }
        let t = self.peek()?;
        let ev = match &t.kind {
            Tok::Scalar { plain, .. } => {
                let plain = *plain;
                let t = self.s.get_token()?;
                let Tok::Scalar { value, .. } = t.kind else {
                    unreachable!("checked a scalar")
                };
                let implicit = if (plain && tag.is_none()) || tag.as_deref() == Some("!") {
                    (true, false)
                } else if tag.is_none() {
                    (false, true)
                } else {
                    (false, false)
                };
                self.pop_state();
                Ev::Scalar {
                    anchor,
                    tag,
                    implicit,
                    value,
                }
            }
            Tok::FlowSequenceStart => {
                self.state = Some(State::FlowSequenceFirstEntry);
                Ev::SequenceStart { anchor, tag }
            }
            Tok::FlowMappingStart => {
                self.state = Some(State::FlowMappingFirstKey);
                Ev::MappingStart { anchor, tag }
            }
            Tok::BlockSequenceStart if block => {
                self.state = Some(State::BlockSequenceFirstEntry);
                Ev::SequenceStart { anchor, tag }
            }
            Tok::BlockMappingStart if block => {
                self.state = Some(State::BlockMappingFirstKey);
                Ev::MappingStart { anchor, tag }
            }
            _ if anchor.is_some() || tag.is_some() => {
                let implicit = (tag.is_none() || tag.as_deref() == Some("!"), false);
                self.pop_state();
                Ev::Scalar {
                    anchor,
                    tag,
                    implicit,
                    value: String::new(),
                }
            }
            other => {
                let node = if block { "block" } else { "flow" };
                return Err(parser_err(
                    Some(&format!("while parsing a {node} node")),
                    Some(start),
                    format!(
                        "expected the node content, but found {}",
                        repr_str(other.id())
                    ),
                    t.start,
                ));
            }
        };
        Ok(Event { kind: ev, start })
    }

    fn parse_block_sequence_entry(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::BlockEntry))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::BlockEntry | Tok::BlockEnd))? {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(State::BlockSequenceEntry);
            return Ok(empty_scalar(t.end));
        }
        if !self.is(|t| matches!(t, Tok::BlockEnd))? {
            let t = self.peek()?;
            return Err(parser_err(
                Some("while parsing a block collection"),
                self.marks.last().copied(),
                format!("expected <block end>, but found {}", repr_str(t.kind.id())),
                t.start,
            ));
        }
        let t = self.s.get_token()?;
        self.pop_state();
        self.marks.pop();
        Ok(Event {
            kind: Ev::SequenceEnd,
            start: t.start,
        })
    }

    fn parse_indentless_sequence_entry(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::BlockEntry))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::BlockEntry | Tok::Key | Tok::Value | Tok::BlockEnd))? {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(State::IndentlessSequenceEntry);
            return Ok(empty_scalar(t.end));
        }
        let t = self.peek()?;
        self.pop_state();
        Ok(Event {
            kind: Ev::SequenceEnd,
            start: t.start,
        })
    }

    fn parse_block_mapping_key(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::Key))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = Some(State::BlockMappingValue);
            return Ok(empty_scalar(t.end));
        }
        if !self.is(|t| matches!(t, Tok::BlockEnd))? {
            let t = self.peek()?;
            return Err(parser_err(
                Some("while parsing a block mapping"),
                self.marks.last().copied(),
                format!("expected <block end>, but found {}", repr_str(t.kind.id())),
                t.start,
            ));
        }
        let t = self.s.get_token()?;
        self.pop_state();
        self.marks.pop();
        Ok(Event {
            kind: Ev::MappingEnd,
            start: t.start,
        })
    }

    fn parse_block_mapping_value(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::Value))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(true, true);
            }
            self.state = Some(State::BlockMappingKey);
            return Ok(empty_scalar(t.end));
        }
        self.state = Some(State::BlockMappingKey);
        let m = self.peek()?.start;
        Ok(empty_scalar(m))
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> R<Event> {
        if !self.is(|t| matches!(t, Tok::FlowSequenceEnd))? {
            if !first {
                if self.is(|t| matches!(t, Tok::FlowEntry))? {
                    self.s.get_token()?;
                } else {
                    let t = self.peek()?;
                    return Err(parser_err(
                        Some("while parsing a flow sequence"),
                        self.marks.last().copied(),
                        format!("expected ',' or ']', but got {}", repr_str(t.kind.id())),
                        t.start,
                    ));
                }
            }
            if self.is(|t| matches!(t, Tok::Key))? {
                let t = self.peek()?;
                self.state = Some(State::FlowSequenceEntryMappingKey);
                return Ok(Event {
                    kind: Ev::MappingStart {
                        anchor: None,
                        tag: None,
                    },
                    start: t.start,
                });
            } else if !self.is(|t| matches!(t, Tok::FlowSequenceEnd))? {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        let t = self.s.get_token()?;
        self.pop_state();
        self.marks.pop();
        Ok(Event {
            kind: Ev::SequenceEnd,
            start: t.start,
        })
    }

    fn parse_flow_sequence_entry_mapping_key(&mut self) -> R<Event> {
        let t = self.s.get_token()?;
        if !self.is(|t| matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowSequenceEnd))? {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(false, false);
        }
        self.state = Some(State::FlowSequenceEntryMappingValue);
        Ok(empty_scalar(t.end))
    }

    fn parse_flow_sequence_entry_mapping_value(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::Value))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::FlowEntry | Tok::FlowSequenceEnd))? {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(false, false);
            }
            self.state = Some(State::FlowSequenceEntryMappingEnd);
            return Ok(empty_scalar(t.end));
        }
        self.state = Some(State::FlowSequenceEntryMappingEnd);
        let m = self.peek()?.start;
        Ok(empty_scalar(m))
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> R<Event> {
        if !self.is(|t| matches!(t, Tok::FlowMappingEnd))? {
            if !first {
                if self.is(|t| matches!(t, Tok::FlowEntry))? {
                    self.s.get_token()?;
                } else {
                    let t = self.peek()?;
                    return Err(parser_err(
                        Some("while parsing a flow mapping"),
                        self.marks.last().copied(),
                        format!("expected ',' or '}}', but got {}", repr_str(t.kind.id())),
                        t.start,
                    ));
                }
            }
            if self.is(|t| matches!(t, Tok::Key))? {
                let t = self.s.get_token()?;
                if !self.is(|t| matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowMappingEnd))? {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(State::FlowMappingValue);
                return Ok(empty_scalar(t.end));
            } else if !self.is(|t| matches!(t, Tok::FlowMappingEnd))? {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        let t = self.s.get_token()?;
        self.pop_state();
        self.marks.pop();
        Ok(Event {
            kind: Ev::MappingEnd,
            start: t.start,
        })
    }

    fn parse_flow_mapping_value(&mut self) -> R<Event> {
        if self.is(|t| matches!(t, Tok::Value))? {
            let t = self.s.get_token()?;
            if !self.is(|t| matches!(t, Tok::FlowEntry | Tok::FlowMappingEnd))? {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(false, false);
            }
            self.state = Some(State::FlowMappingKey);
            return Ok(empty_scalar(t.end));
        }
        self.state = Some(State::FlowMappingKey);
        let m = self.peek()?.start;
        Ok(empty_scalar(m))
    }
}

fn empty_scalar(m: Mark) -> Event {
    Event {
        kind: Ev::Scalar {
            anchor: None,
            tag: None,
            implicit: (true, false),
            value: String::new(),
        },
        start: m,
    }
}

// ---------------------------------------------------------------- composer and resolver

#[derive(Clone, Debug)]
pub(crate) enum NodeKind {
    Scalar(String),
    Seq(Vec<usize>),
    Map(Vec<(usize, usize)>),
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub tag: String,
    pub kind: NodeKind,
    pub start: Mark,
}

impl Node {
    fn id(&self) -> &'static str {
        match self.kind {
            NodeKind::Scalar(_) => "scalar",
            NodeKind::Seq(_) => "sequence",
            NodeKind::Map(_) => "mapping",
        }
    }
}

const STR: &str = "tag:yaml.org,2002:str";
const MERGE: &str = "tag:yaml.org,2002:merge";
const VALUE: &str = "tag:yaml.org,2002:value";

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($pat).expect("valid resolver pattern"));
    };
}

// PyYAML's resolver patterns with Python's `$` (end, or before a final "\n") written `\n?\z`.
re!(
    BOOL_RE,
    r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)\n?\z"
);
re!(
    FLOAT_RE,
    r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\n?\z"
);
re!(
    INT_RE,
    r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)\n?\z"
);
re!(MERGE_RE, r"^(?:<<)\n?\z");
re!(NULL_RE, r"^(?:~|null|Null|NULL|)\n?\z");
re!(
    TIMESTAMP_RE,
    r"^(?:[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?)\n?\z"
);
re!(VALUE_RE, r"^(?:=)\n?\z");
re!(YAML_RE, r"^(?:!|&|\*)\n?\z");
re!(
    TIMESTAMP_PARTS_RE,
    r"^([0-9][0-9][0-9][0-9])-([0-9][0-9]?)-([0-9][0-9]?)(?:(?:[Tt]|[ \t]+)([0-9][0-9]?):([0-9][0-9]):([0-9][0-9])(?:\.([0-9]*))?(?:[ \t]*(Z|([-+])([0-9][0-9]?)(?::([0-9][0-9]))?))?)?\n?\z"
);

/// PyYAML `Resolver.resolve` for a scalar: the implicit resolvers keyed by the first char, in
/// registration order (bool, float, int, merge, null, timestamp, value, yaml).
pub(crate) fn resolve_scalar(value: &str, implicit: (bool, bool)) -> String {
    if implicit.0 {
        let first = value.chars().next();
        let order: &[(&str, &LazyLock<Regex>)] = match first {
            None => &[("null", &NULL_RE)],
            Some('y' | 'Y' | 't' | 'T' | 'f' | 'F' | 'o' | 'O') => &[("bool", &BOOL_RE)],
            Some('n' | 'N') => &[("bool", &BOOL_RE), ("null", &NULL_RE)],
            Some('-' | '+') => &[("float", &FLOAT_RE), ("int", &INT_RE)],
            Some('.') => &[("float", &FLOAT_RE)],
            Some('0'..='9') => &[
                ("float", &FLOAT_RE),
                ("int", &INT_RE),
                ("timestamp", &TIMESTAMP_RE),
            ],
            Some('<') => &[("merge", &MERGE_RE)],
            Some('~') => &[("null", &NULL_RE)],
            Some('=') => &[("value", &VALUE_RE)],
            Some('!' | '&' | '*') => &[("yaml", &YAML_RE)],
            _ => &[],
        };
        for (tag, re) in order {
            if re.is_match(value) {
                return format!("tag:yaml.org,2002:{tag}");
            }
        }
    }
    STR.into()
}

/// The deepest `compose_node` nesting that Python 3.15.1 (recursion limit 1000) loads through
/// `sources.tasklog.parse_tasklog_file` or `py.yaml.safe_load` under the bridge: PyYAML's
/// composer takes two frames a level, so a flow sequence or mapping 493 levels deep raises
/// `RecursionError` (bisected: `a: [[...]]` loads with 491 brackets and fails with 492).
const MAX_COMPOSE_DEPTH: usize = 492;

/// The deepest value `safe_load` builds. Only aliases nest a value deeper than the composer's
/// limit; Python shares the aliased objects and loads such a chain at any depth, while this port
/// copies them, and the copies, `py_str`, `py_repr` and drop recurse on the Rust stack. Past this
/// depth `safe_load` raises RecursionError instead of overflowing the stack (an accepted
/// difference: no task log or auto-memory file holds an anchor).
const MAX_VALUE_DEPTH: usize = 1000;

struct Composer {
    p: Parser,
    anchors: HashMap<String, usize>,
    nodes: Vec<Node>,
}

/// A collection `compose_node` is filling: a sequence, or a mapping waiting for a key or for
/// the value of key `k`.
#[derive(Clone, Copy)]
enum Open {
    Seq(usize),
    Key(usize),
    Value(usize, usize),
}

enum Started {
    Leaf(usize),
    Seq(usize),
    Map(usize),
}

fn composer_err(
    context: Option<&str>,
    context_mark: Option<Mark>,
    problem: String,
    mark: Mark,
) -> Fail {
    marked("ComposerError", context, context_mark, problem, mark)
}

impl Composer {
    fn next(&mut self) -> R<Event> {
        Ok(self
            .p
            .get_event()?
            .expect("the composer reads no further than StreamEnd"))
    }

    fn peek_kind(&mut self) -> R<Ev> {
        Ok(self
            .p
            .peek_event()?
            .expect("the composer reads no further than StreamEnd")
            .kind
            .clone())
    }

    fn at_stream_end(&mut self) -> R<bool> {
        Ok(matches!(
            self.p.peek_event()?.map(|e| &e.kind),
            Some(Ev::StreamEnd)
        ))
    }

    fn get_single_node(&mut self) -> R<Option<usize>> {
        self.next()?;
        let mut document = None;
        if !self.at_stream_end()? {
            document = Some(self.compose_document()?);
        }
        if !self.at_stream_end()? {
            let ev = self.next()?;
            let doc_start = document.map(|d| self.nodes[d].start);
            return Err(composer_err(
                Some("expected a single document in the stream"),
                doc_start,
                "but found another document".into(),
                ev.start,
            ));
        }
        self.next()?;
        Ok(document)
    }

    fn compose_document(&mut self) -> R<usize> {
        self.next()?;
        let node = self.compose_node()?;
        self.next()?;
        self.anchors.clear();
        Ok(node)
    }

    fn add(&mut self, node: Node, anchor: Option<String>) -> usize {
        self.nodes.push(node);
        let id = self.nodes.len() - 1;
        if let Some(a) = anchor {
            self.anchors.insert(a, id);
        }
        id
    }

    /// PyYAML `compose_node` with its recursion into `compose_sequence_node` and
    /// `compose_mapping_node` kept on an explicit stack: the same events in the same order, and
    /// RecursionError where Python's recursion limit stops it (`MAX_COMPOSE_DEPTH`), so deep input
    /// never reaches the end of the Rust stack.
    fn compose_node(&mut self) -> R<usize> {
        let mut open: Vec<Open> = Vec::new();
        'node: loop {
            if open.len() == MAX_COMPOSE_DEPTH {
                return Err(py_err(
                    "RecursionError",
                    "maximum recursion depth exceeded".into(),
                ));
            }
            let mut done = match self.start_node()? {
                Started::Leaf(id) => id,
                Started::Seq(id) => {
                    if !matches!(self.peek_kind()?, Ev::SequenceEnd) {
                        open.push(Open::Seq(id));
                        continue 'node;
                    }
                    self.next()?;
                    id
                }
                Started::Map(id) => {
                    if !matches!(self.peek_kind()?, Ev::MappingEnd) {
                        open.push(Open::Key(id));
                        continue 'node;
                    }
                    self.next()?;
                    id
                }
            };
            loop {
                let Some(top) = open.last_mut() else {
                    return Ok(done);
                };
                match *top {
                    Open::Seq(id) => {
                        if let NodeKind::Seq(v) = &mut self.nodes[id].kind {
                            v.push(done);
                        }
                        if !matches!(self.peek_kind()?, Ev::SequenceEnd) {
                            continue 'node;
                        }
                    }
                    Open::Key(id) => {
                        *top = Open::Value(id, done);
                        continue 'node;
                    }
                    Open::Value(id, k) => {
                        *top = Open::Key(id);
                        if let NodeKind::Map(pairs) = &mut self.nodes[id].kind {
                            pairs.push((k, done));
                        }
                        if !matches!(self.peek_kind()?, Ev::MappingEnd) {
                            continue 'node;
                        }
                    }
                }
                self.next()?;
                done = match open.pop() {
                    Some(Open::Seq(id) | Open::Key(id) | Open::Value(id, _)) => id,
                    None => unreachable!("a collection was open"),
                };
            }
        }
    }

    /// The node event's half of `compose_node`: an alias or a scalar is complete, and a
    /// collection is added empty for `compose_node` to fill.
    fn start_node(&mut self) -> R<Started> {
        let ev = self.next()?;
        let anchor = match &ev.kind {
            Ev::Alias(name) => {
                return match self.anchors.get(name) {
                    Some(&id) => Ok(Started::Leaf(id)),
                    None => Err(composer_err(
                        None,
                        None,
                        format!("found undefined alias {}", repr_str(name)),
                        ev.start,
                    )),
                };
            }
            Ev::Scalar { anchor, .. }
            | Ev::SequenceStart { anchor, .. }
            | Ev::MappingStart { anchor, .. } => anchor.clone(),
            _ => unreachable!("the parser emits a node event here"),
        };
        if let Some(a) = &anchor
            && let Some(&first) = self.anchors.get(a)
        {
            return Err(composer_err(
                Some(&format!(
                    "found duplicate anchor {}; first occurrence",
                    repr_str(a)
                )),
                Some(self.nodes[first].start),
                "second occurrence".into(),
                ev.start,
            ));
        }
        match ev.kind {
            Ev::Scalar {
                tag,
                implicit,
                value,
                ..
            } => {
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolve_scalar(&value, implicit),
                };
                Ok(Started::Leaf(self.add(
                    Node {
                        tag,
                        kind: NodeKind::Scalar(value),
                        start: ev.start,
                    },
                    anchor,
                )))
            }
            Ev::SequenceStart { tag, .. } => {
                let tag = tag
                    .filter(|t| t != "!")
                    .unwrap_or_else(|| "tag:yaml.org,2002:seq".into());
                let id = self.add(
                    Node {
                        tag,
                        kind: NodeKind::Seq(Vec::new()),
                        start: ev.start,
                    },
                    anchor,
                );
                Ok(Started::Seq(id))
            }
            Ev::MappingStart { tag, .. } => {
                let tag = tag
                    .filter(|t| t != "!")
                    .unwrap_or_else(|| "tag:yaml.org,2002:map".into());
                let id = self.add(
                    Node {
                        tag,
                        kind: NodeKind::Map(Vec::new()),
                        start: ev.start,
                    },
                    anchor,
                );
                Ok(Started::Map(id))
            }
            _ => unreachable!("matched above"),
        }
    }
}

// ---------------------------------------------------------------- constructor

/// A constructed object. Collections start empty and are filled from a FIFO queue, as
/// PyYAML's generator constructors are (`state_generators`), so errors surface in its order
/// and an alias inside its own collection makes a cycle.
enum Obj {
    Val(PyValue),
    List(Vec<usize>),
    Dict(Vec<(usize, usize)>),
    Set(Vec<usize>),
    Pairs(Vec<(usize, usize)>),
}

#[derive(Clone, Copy)]
enum Fill {
    Seq,
    Map,
    Set,
    Omap(&'static str),
}

struct Ctor<'a> {
    nodes: &'a mut [Node],
    objs: Vec<Obj>,
    built: HashMap<usize, usize>,
    queue: VecDeque<(usize, usize, Fill)>,
}

fn ctor_err(context_mark: Option<Mark>, problem: String, mark: Mark) -> Fail {
    let context = context_mark.map(|_| "while constructing a mapping");
    marked("ConstructorError", context, context_mark, problem, mark)
}

/// PyYAML `BaseConstructor.construct_document` with `SafeConstructor`'s constructors.
pub(crate) fn construct_document(nodes: &mut [Node], root: usize) -> R<PyValue> {
    let mut c = Ctor {
        nodes,
        objs: Vec::new(),
        built: HashMap::new(),
        queue: VecDeque::new(),
    };
    let top = c.construct_object(root)?;
    while let Some((node, obj, fill)) = c.queue.pop_front() {
        c.fill(node, obj, fill)?;
    }
    let mut on_path = vec![false; c.objs.len()];
    c.materialize(top, 0, &mut on_path)
}

impl Ctor<'_> {
    fn push(&mut self, o: Obj) -> usize {
        self.objs.push(o);
        self.objs.len() - 1
    }

    fn construct_object(&mut self, id: usize) -> R<usize> {
        if let Some(&o) = self.built.get(&id) {
            return Ok(o);
        }
        let tag = self.nodes[id].tag.clone();
        let fill = match tag.as_str() {
            "tag:yaml.org,2002:seq" => Some((Obj::List(Vec::new()), Fill::Seq)),
            "tag:yaml.org,2002:map" => Some((Obj::Dict(Vec::new()), Fill::Map)),
            "tag:yaml.org,2002:set" => Some((Obj::Set(Vec::new()), Fill::Set)),
            "tag:yaml.org,2002:omap" => Some((
                Obj::Pairs(Vec::new()),
                Fill::Omap("while constructing an ordered map"),
            )),
            "tag:yaml.org,2002:pairs" => Some((
                Obj::Pairs(Vec::new()),
                Fill::Omap("while constructing pairs"),
            )),
            _ => None,
        };
        let obj = match fill {
            Some((empty, fill)) => {
                let o = self.push(empty);
                self.queue.push_back((id, o, fill));
                o
            }
            None => {
                let v = self.construct_scalar_tag(id, &tag)?;
                self.push(Obj::Val(v))
            }
        };
        self.built.insert(id, obj);
        Ok(obj)
    }

    /// `SafeConstructor.construct_scalar`: a mapping node stands for the value of its `=` key.
    fn scalar_text(&self, id: usize) -> R<String> {
        let node = &self.nodes[id];
        if let NodeKind::Map(pairs) = &node.kind {
            for &(k, v) in pairs {
                if self.nodes[k].tag == VALUE {
                    return self.scalar_text(v);
                }
            }
        }
        match &node.kind {
            NodeKind::Scalar(s) => Ok(s.clone()),
            _ => Err(marked(
                "ConstructorError",
                None,
                None,
                format!("expected a scalar node, but found {}", node.id()),
                node.start,
            )),
        }
    }

    fn construct_scalar_tag(&mut self, id: usize, tag: &str) -> R<PyValue> {
        let start = self.nodes[id].start;
        match tag {
            "tag:yaml.org,2002:null" => self.scalar_text(id).map(|_| PyValue::None),
            "tag:yaml.org,2002:bool" => {
                let v = self.scalar_text(id)?.to_lowercase();
                match v.as_str() {
                    "yes" | "true" | "on" => Ok(PyValue::Bool(true)),
                    "no" | "false" | "off" => Ok(PyValue::Bool(false)),
                    _ => Err(py_err("KeyError", repr_str(&v))),
                }
            }
            "tag:yaml.org,2002:int" => construct_int(&self.scalar_text(id)?).map(PyValue::Int),
            "tag:yaml.org,2002:float" => {
                construct_float(&self.scalar_text(id)?).map(PyValue::Float)
            }
            "tag:yaml.org,2002:binary" => {
                construct_binary(&self.scalar_text(id)?, start).map(PyValue::Bytes)
            }
            "tag:yaml.org,2002:timestamp" => {
                self.scalar_text(id)?;
                match &self.nodes[id].kind {
                    NodeKind::Scalar(s) => construct_timestamp(s),
                    _ => Err(py_err(
                        "TypeError",
                        "expected string or bytes-like object, got 'list'".into(),
                    )),
                }
            }
            STR => self.scalar_text(id).map(PyValue::Str),
            _ => Err(marked(
                "ConstructorError",
                None,
                None,
                format!(
                    "could not determine a constructor for the tag {}",
                    repr_str(tag)
                ),
                start,
            )),
        }
    }

    fn flatten_mapping(&mut self, id: usize) -> R<()> {
        let mut merge: Vec<(usize, usize)> = Vec::new();
        let mut index = 0;
        loop {
            let NodeKind::Map(pairs) = &self.nodes[id].kind else {
                return Ok(());
            };
            if index >= pairs.len() {
                break;
            }
            let (k, v) = pairs[index];
            if self.nodes[k].tag == MERGE {
                if let NodeKind::Map(pairs) = &mut self.nodes[id].kind {
                    pairs.remove(index);
                }
                match self.nodes[v].kind.clone() {
                    NodeKind::Map(_) => {
                        self.flatten_mapping(v)?;
                        if let NodeKind::Map(p) = &self.nodes[v].kind {
                            merge.extend(p.iter().copied());
                        }
                    }
                    NodeKind::Seq(items) => {
                        let mut submerge = Vec::new();
                        for sub in items {
                            if !matches!(self.nodes[sub].kind, NodeKind::Map(_)) {
                                return Err(ctor_err(
                                    Some(self.nodes[id].start),
                                    format!(
                                        "expected a mapping for merging, but found {}",
                                        self.nodes[sub].id()
                                    ),
                                    self.nodes[sub].start,
                                ));
                            }
                            self.flatten_mapping(sub)?;
                            if let NodeKind::Map(p) = &self.nodes[sub].kind {
                                submerge.push(p.clone());
                            }
                        }
                        for p in submerge.into_iter().rev() {
                            merge.extend(p);
                        }
                    }
                    NodeKind::Scalar(_) => {
                        return Err(ctor_err(
                            Some(self.nodes[id].start),
                            format!(
                                "expected a mapping or list of mappings for merging, but found {}",
                                self.nodes[v].id()
                            ),
                            self.nodes[v].start,
                        ));
                    }
                }
            } else if self.nodes[k].tag == VALUE {
                self.nodes[k].tag = STR.into();
                index += 1;
            } else {
                index += 1;
            }
        }
        if !merge.is_empty()
            && let NodeKind::Map(pairs) = &mut self.nodes[id].kind
        {
            merge.append(pairs);
            *pairs = merge;
        }
        Ok(())
    }

    fn hashable_key(&self, o: usize) -> Option<&PyValue> {
        match &self.objs[o] {
            Obj::Val(v) if v.is_hashable() => Some(v),
            _ => None,
        }
    }

    /// `construct_mapping`: the pairs of a fresh dict, a later equal key replacing the value.
    fn construct_mapping(&mut self, id: usize) -> R<Vec<(usize, usize)>> {
        if matches!(self.nodes[id].kind, NodeKind::Map(_)) {
            self.flatten_mapping(id)?;
        }
        let NodeKind::Map(pairs) = self.nodes[id].kind.clone() else {
            let n = &self.nodes[id];
            return Err(marked(
                "ConstructorError",
                None,
                None,
                format!("expected a mapping node, but found {}", n.id()),
                n.start,
            ));
        };
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (k, v) in pairs {
            let ko = self.construct_object(k)?;
            let Some(key) = self.hashable_key(ko).cloned() else {
                return Err(ctor_err(
                    Some(self.nodes[id].start),
                    "found unhashable key".into(),
                    self.nodes[k].start,
                ));
            };
            let vo = self.construct_object(v)?;
            match out
                .iter()
                .position(|(e, _)| self.hashable_key(*e).is_some_and(|ek| ek.key_eq(&key)))
            {
                Some(i) => out[i].1 = vo,
                None => out.push((ko, vo)),
            }
        }
        Ok(out)
    }

    fn fill(&mut self, id: usize, obj: usize, fill: Fill) -> R<()> {
        match fill {
            Fill::Seq => {
                let NodeKind::Seq(items) = self.nodes[id].kind.clone() else {
                    let n = &self.nodes[id];
                    return Err(marked(
                        "ConstructorError",
                        None,
                        None,
                        format!("expected a sequence node, but found {}", n.id()),
                        n.start,
                    ));
                };
                let mut out = Vec::new();
                for child in items {
                    out.push(self.construct_object(child)?);
                }
                self.objs[obj] = Obj::List(out);
            }
            Fill::Map => {
                let pairs = self.construct_mapping(id)?;
                self.objs[obj] = Obj::Dict(pairs);
            }
            Fill::Set => {
                let pairs = self.construct_mapping(id)?;
                self.objs[obj] = Obj::Set(pairs.into_iter().map(|(k, _)| k).collect());
            }
            Fill::Omap(context) => {
                let node_start = self.nodes[id].start;
                let NodeKind::Seq(items) = self.nodes[id].kind.clone() else {
                    let p = format!("expected a sequence, but found {}", self.nodes[id].id());
                    return Err(marked(
                        "ConstructorError",
                        Some(context),
                        Some(node_start),
                        p,
                        node_start,
                    ));
                };
                let mut out = Vec::new();
                for sub in items {
                    let n = &self.nodes[sub];
                    let NodeKind::Map(pairs) = &n.kind else {
                        let p = format!("expected a mapping of length 1, but found {}", n.id());
                        return Err(marked(
                            "ConstructorError",
                            Some(context),
                            Some(node_start),
                            p,
                            n.start,
                        ));
                    };
                    if pairs.len() != 1 {
                        let p = format!(
                            "expected a single mapping item, but found {} items",
                            pairs.len()
                        );
                        return Err(marked(
                            "ConstructorError",
                            Some(context),
                            Some(node_start),
                            p,
                            n.start,
                        ));
                    }
                    let (k, v) = pairs[0];
                    let ko = self.construct_object(k)?;
                    let vo = self.construct_object(v)?;
                    out.push((ko, vo));
                }
                self.objs[obj] = Obj::Pairs(out);
            }
        }
        Ok(())
    }

    /// The value of object `o` at nesting `depth`; `on_path[o]` marks the objects being built
    /// above it, so an alias back into one of them is a cycle.
    fn materialize(&self, o: usize, depth: usize, on_path: &mut [bool]) -> R<PyValue> {
        if on_path[o] {
            return Ok(PyValue::Cycle(match self.objs[o] {
                Obj::Dict(_) => "{...}",
                _ => "[...]",
            }));
        }
        if depth == MAX_VALUE_DEPTH {
            return Err(py_err(
                "RecursionError",
                "maximum recursion depth exceeded".into(),
            ));
        }
        on_path[o] = true;
        let d = depth + 1;
        let v = match &self.objs[o] {
            Obj::Val(v) => PyValue::clone(v),
            Obj::List(items) => PyValue::List(
                items
                    .iter()
                    .map(|&i| self.materialize(i, d, on_path))
                    .collect::<R<_>>()?,
            ),
            Obj::Set(items) => PyValue::Set(
                items
                    .iter()
                    .map(|&i| self.materialize(i, d, on_path))
                    .collect::<R<_>>()?,
            ),
            Obj::Dict(pairs) => PyValue::Dict(
                pairs
                    .iter()
                    .map(|&(k, v)| {
                        Ok((
                            self.materialize(k, d, on_path)?,
                            self.materialize(v, d, on_path)?,
                        ))
                    })
                    .collect::<R<_>>()?,
            ),
            Obj::Pairs(pairs) => PyValue::List(
                pairs
                    .iter()
                    .map(|&(k, v)| {
                        Ok(PyValue::Tuple(vec![
                            self.materialize(k, d, on_path)?,
                            self.materialize(v, d, on_path)?,
                        ]))
                    })
                    .collect::<R<_>>()?,
            ),
        };
        on_path[o] = false;
        Ok(v)
    }
}

// ---------------------------------------------------------------- scalar constructors

/// Little-endian base-1e9 digits of a non-negative integer.
fn big_from_digits(digits: &str, base: u32) -> Vec<u32> {
    let mut n: Vec<u32> = vec![0];
    for c in digits.chars() {
        let mut carry = c.to_digit(base).expect("caller checked the digits") as u64;
        for limb in n.iter_mut() {
            let x = *limb as u64 * base as u64 + carry;
            *limb = (x % 1_000_000_000) as u32;
            carry = x / 1_000_000_000;
        }
        if carry > 0 {
            n.push(carry as u32);
        }
    }
    n
}

fn big_to_string(n: &[u32], neg: bool) -> String {
    let mut s = n.last().copied().unwrap_or(0).to_string();
    for limb in n.iter().rev().skip(1) {
        s.push_str(&format!("{limb:09}"));
    }
    if neg && s != "0" { format!("-{s}") } else { s }
}

fn big_mul_add(n: &mut Vec<u32>, mul: u64, add: &[u32]) {
    let mut carry = 0u64;
    for (i, limb) in n.iter_mut().enumerate() {
        let x = *limb as u64 * mul + carry + add.get(i).copied().unwrap_or(0) as u64;
        *limb = (x % 1_000_000_000) as u32;
        carry = x / 1_000_000_000;
    }
    for &a in add.iter().skip(n.len()) {
        let x = a as u64 + carry;
        n.push((x % 1_000_000_000) as u32);
        carry = x / 1_000_000_000;
    }
    while carry > 0 {
        n.push((carry % 1_000_000_000) as u32);
        carry /= 1_000_000_000;
    }
}

/// Python `int(s, base)` for an ASCII str, as decimal text; the error is `int()`'s ValueError.
fn py_int(s: &str, base: u32) -> R<String> {
    let fail = || {
        let mut r = repr_str(s);
        if r.chars().count() > 200 {
            r = r.chars().take(200).collect();
        }
        py_err(
            "ValueError",
            format!("invalid literal for int() with base {base}: {r}"),
        )
    };
    let t = crate::py::float::ascii_number_text(s);
    let t = t.as_str();
    let (neg, mut body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = match base {
        2 => Some("0b"),
        8 => Some("0o"),
        16 => Some("0x"),
        _ => None,
    };
    let mut after_prefix = false;
    if let Some(p) = prefix
        && body.len() >= 2
        && body[..2].eq_ignore_ascii_case(p)
    {
        body = &body[2..];
        after_prefix = true;
    }
    let b = body.as_bytes();
    let mut digits = String::new();
    for (i, &c) in b.iter().enumerate() {
        if c == b'_' {
            let prev_ok = (i > 0 && b[i - 1] != b'_') || (i == 0 && after_prefix);
            if !prev_ok || i + 1 >= b.len() || b[i + 1] == b'_' {
                return Err(fail());
            }
        } else if (c as char).is_digit(base) {
            digits.push(c as char);
        } else {
            return Err(fail());
        }
    }
    if digits.is_empty() {
        return Err(fail());
    }
    // int(text) in base 10 (not a power of 2) refuses more than 4300 digits
    if base == 10 && digits.len() > crate::py::int::MAX_STR_DIGITS {
        return Err(py_err(
            "ValueError",
            format!(
                "Exceeds the limit ({} digits) for integer string conversion: value has {} digits; \
                 use sys.set_int_max_str_digits() to increase the limit",
                crate::py::int::MAX_STR_DIGITS,
                digits.len()
            ),
        ));
    }
    Ok(big_to_string(&big_from_digits(&digits, base), neg))
}

/// `SafeConstructor.construct_yaml_int`.
fn construct_int(text: &str) -> R<String> {
    let value: String = text.replace('_', "");
    let first = value
        .chars()
        .next()
        .ok_or_else(|| py_err("IndexError", "string index out of range".into()))?;
    let neg = first == '-';
    let v = if first == '-' || first == '+' {
        &value[1..]
    } else {
        &value[..]
    };
    let with_sign = |s: String| -> String {
        if neg && s != "0" {
            if let Some(rest) = s.strip_prefix('-') {
                rest.to_string()
            } else {
                format!("-{s}")
            }
        } else {
            s
        }
    };
    if v == "0" {
        return Ok("0".into());
    }
    if let Some(rest) = v.strip_prefix("0b") {
        return py_int(rest, 2).map(with_sign);
    }
    if let Some(rest) = v.strip_prefix("0x") {
        return py_int(rest, 16).map(with_sign);
    }
    if v.starts_with('0') {
        return py_int(v, 8).map(with_sign);
    }
    if v.contains(':') {
        let parts = v
            .split(':')
            .map(|p| py_int(p, 10))
            .collect::<R<Vec<String>>>()?;
        if parts.iter().any(|p| p.starts_with('-')) {
            // A signed part only comes from an explicit `!!int` tag; i128 holds any such case
            // short of 38 digits.
            let mut acc: i128 = 0;
            for p in &parts {
                acc = acc * 60
                    + p.parse::<i128>()
                        .map_err(|_| py_err("OverflowError", "int too large".into()))?;
            }
            return Ok(with_sign(acc.to_string()));
        }
        let mut acc: Vec<u32> = vec![0];
        for p in &parts {
            big_mul_add(&mut acc, 60, &big_from_digits(p, 10));
        }
        return Ok(with_sign(big_to_string(&acc, false)));
    }
    if v.is_empty() {
        return Err(py_err(
            "ValueError",
            "invalid literal for int() with base 10: ''".into(),
        ));
    }
    py_int(v, 10).map(with_sign)
}

fn py_float(s: &str) -> R<f64> {
    crate::py::float::from_str(s).map_err(|e| py_err("ValueError", e.to_string()))
}

/// `SafeConstructor.construct_yaml_float`.
fn construct_float(text: &str) -> R<f64> {
    let value = text.replace('_', "").to_lowercase();
    let first = value
        .chars()
        .next()
        .ok_or_else(|| py_err("IndexError", "string index out of range".into()))?;
    let sign = if first == '-' { -1.0 } else { 1.0 };
    let v = if first == '-' || first == '+' {
        &value[1..]
    } else {
        &value[..]
    };
    if v == ".inf" {
        return Ok(sign * f64::INFINITY);
    }
    if v == ".nan" {
        return Ok(f64::NAN);
    }
    if v.contains(':') {
        let mut digits = v.split(':').map(py_float).collect::<R<Vec<f64>>>()?;
        digits.reverse();
        // `base` is a Python int (60**k, base-1e9 limbs); `digit*base` converts it to the nearest
        // float, and past the float range that conversion raises OverflowError.
        let mut base: Vec<u32> = vec![1];
        let mut acc = 0.0f64;
        for d in digits {
            let b: f64 = big_to_string(&base, false).parse().expect("decimal digits");
            if b.is_infinite() {
                return Err(py_err(
                    "OverflowError",
                    "int too large to convert to float".into(),
                ));
            }
            acc += d * b;
            big_mul_add(&mut base, 60, &[]);
        }
        return Ok(sign * acc);
    }
    Ok(sign * py_float(v)?)
}

/// `SafeConstructor.construct_yaml_binary`: ASCII encode, then `base64.decodebytes`
/// (`binascii.a2b_base64` in non-strict mode).
fn construct_binary(text: &str, start: Mark) -> R<Vec<u8>> {
    let chars: Vec<char> = text.chars().collect();
    if let Some(i) = chars.iter().position(|c| !c.is_ascii()) {
        let mut j = i;
        while j < chars.len() && !chars[j].is_ascii() {
            j += 1;
        }
        let what = if j - i == 1 {
            let c = chars[i] as u32;
            let esc = if c <= 0xff {
                format!("\\x{c:02x}")
            } else if c <= 0xffff {
                format!("\\u{c:04x}")
            } else {
                format!("\\U{c:08x}")
            };
            format!("character '{esc}' in position {i}")
        } else {
            format!("characters in position {i}-{}", j - 1)
        };
        let p = format!(
            "failed to convert base64 data into ascii: 'ascii' codec can't encode {what}: ordinal not in range(128)"
        );
        return Err(marked("ConstructorError", None, None, p, start));
    }
    let fail = |m: String| {
        marked(
            "ConstructorError",
            None,
            None,
            format!("failed to decode base64 data: {m}"),
            start,
        )
    };
    let mut out = Vec::new();
    let mut quad = 0;
    let mut left: u32 = 0;
    let mut pads = 0;
    for c in text.bytes() {
        if c == b'=' {
            pads += 1;
            if quad >= 2 && quad + pads >= 4 {
                return Ok(out);
            }
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        pads = 0;
        match quad {
            0 => {
                quad = 1;
                left = v;
            }
            1 => {
                quad = 2;
                out.push(((left << 2) | (v >> 4)) as u8);
                left = v & 0x0f;
            }
            2 => {
                quad = 3;
                out.push(((left << 4) | (v >> 2)) as u8);
                left = v & 0x03;
            }
            _ => {
                quad = 0;
                out.push(((left << 6) | v) as u8);
                left = 0;
            }
        }
    }
    match quad {
        0 => Ok(out),
        1 => Err(fail(format!(
            "Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4",
            out.len() / 3 * 4 + 1
        ))),
        _ => Err(fail("Incorrect padding".into())),
    }
}

fn check_range(name: &str, v: i64, lo: i64, hi: i64) -> R<()> {
    if v < lo || v > hi {
        return Err(py_err(
            "ValueError",
            format!("{name} must be in {lo}..{hi}, not {v}"),
        ));
    }
    Ok(())
}

fn make_date(y: i64, m: i64, d: i64) -> R<Date> {
    check_range("year", y, 1, 9999)?;
    check_range("month", m, 1, 12)?;
    let dim = days_in_month(y as i32, m as u32) as i64;
    if d < 1 || d > dim {
        return Err(py_err(
            "ValueError",
            format!("day {d} must be in range 1..{dim} for month {m} in year {y}"),
        ));
    }
    Ok(Date {
        year: y as i32,
        month: m as u32,
        day: d as u32,
    })
}

/// `SafeConstructor.construct_yaml_timestamp`.
fn construct_timestamp(s: &str) -> R<PyValue> {
    let Some(c) = TIMESTAMP_PARTS_RE.captures(s) else {
        return Err(py_err(
            "AttributeError",
            "'NoneType' object has no attribute 'groupdict'".into(),
        ));
    };
    let num = |i: usize| {
        c.get(i)
            .map(|m| m.as_str().parse::<i64>().expect("regex matched digits"))
    };
    let (year, month, day) = (
        num(1).unwrap_or(0),
        num(2).unwrap_or(0),
        num(3).unwrap_or(0),
    );
    let Some(hour) = num(4) else {
        return make_date(year, month, day).map(PyValue::Date);
    };
    let (minute, second) = (num(5).unwrap_or(0), num(6).unwrap_or(0));
    let mut micro = 0;
    if let Some(f) = c.get(7).map(|m| m.as_str()).filter(|f| !f.is_empty()) {
        let mut f: String = f.chars().take(6).collect();
        while f.len() < 6 {
            f.push('0');
        }
        micro = f.parse::<u32>().expect("six digits");
    }
    let mut tz = None;
    if let Some(sign) = c.get(9) {
        let secs = num(10).unwrap_or(0) * 3600 + num(11).unwrap_or(0) * 60;
        let secs = if sign.as_str() == "-" { -secs } else { secs };
        if secs.abs() >= 86400 {
            return Err(py_err(
                "ValueError",
                format!(
                    "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not {}",
                    crate::py::value::timedelta_repr(secs)
                ),
            ));
        }
        tz = Some(secs as i32);
    } else if c.get(8).is_some() {
        tz = Some(0);
    }
    let date = make_date(year, month, day)?;
    check_range("hour", hour, 0, 23)?;
    check_range("minute", minute, 0, 59)?;
    check_range("second", second, 0, 59)?;
    Ok(PyValue::DateTime(DateTime {
        date,
        hour: hour as u32,
        minute: minute as u32,
        second: second as u32,
        microsecond: micro,
        tz,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repr(text: &str) -> String {
        safe_load(text)
            .map(|v| v.py_repr())
            .unwrap_or_else(|e| format!("{}: {}", e.class, e.message))
    }

    #[test]
    fn resolves_yaml_1_1_scalars() {
        assert_eq!(
            repr(
                "a: yes\nb: Off\nc: 0o17\nd: 017\ne: 0x1F\nf: 1:30\ng: 1_000\nh: 1e3\ni: .5\nj: -.Inf\nk: ~\nl: 2026-11-04\n"
            ),
            "{'a': True, 'b': False, 'c': '0o17', 'd': 15, 'e': 31, 'f': 90, 'g': 1000, 'h': '1e3', \
             'i': 0.5, 'j': -inf, 'k': None, 'l': datetime.date(2026, 11, 4)}"
        );
        assert_eq!(
            repr("t: 2026-11-04 10:00:00.5 +05:00"),
            "{'t': datetime.datetime(2026, 11, 4, 10, 0, 0, 500000, tzinfo=datetime.timezone(datetime.timedelta(seconds=18000)))}"
        );
        assert_eq!(repr(""), "None");
    }

    #[test]
    fn merges_and_equal_keys_follow_dict_rules() {
        assert_eq!(
            repr("b: &b {x: 1, y: 2}\nc: {<<: *b, y: 3}\n"),
            "{'b': {'x': 1, 'y': 2}, 'c': {'x': 1, 'y': 3}}"
        );
        assert_eq!(repr("{1: a, true: b, 1.0: c}"), "{1: 'c'}");
        assert_eq!(repr("&a [*a]"), "[[...]]");
    }

    const RECURSION: &str = "RecursionError: maximum recursion depth exceeded";

    /// Runs `f` on a thread with the 8 MiB stack of the main thread `sm` parses YAML on (a test
    /// thread has 2 MiB, and a debug build needs about 2 KiB a level to build and drop a value).
    fn on_main_stack(f: fn()) {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    fn flow(n: usize) -> String {
        format!("a: {}{}\n", "[".repeat(n), "]".repeat(n))
    }

    fn block(n: usize) -> String {
        let mut t = "a:\n".to_string();
        for i in 1..=n {
            t.push_str(&format!("{}-\n", "  ".repeat(i)));
        }
        t + &format!("{}- x\n", "  ".repeat(n + 1))
    }

    /// Python 3.15.1 through parse_tasklog_file and the bridge's py.yaml.safe_load: `a: [[...]]`
    /// loads with 491 brackets and raises RecursionError with 492; `a: {b: ...1}` with 490 and
    /// 491. Block nesting fails one level earlier in Python (accepted difference).
    #[test]
    fn deep_nesting_raises_recursion_error_at_pythons_depth() {
        on_main_stack(|| {
            assert!(repr(&flow(491)).starts_with("{'a': [[[["));
            assert_eq!(repr(&flow(492)), RECURSION);
            let map = |n: usize| format!("a: {}1{}\n", "{b: ".repeat(n), "}".repeat(n));
            assert!(repr(&map(490)).starts_with("{'a': {'b': {'b':"));
            assert_eq!(repr(&map(491)), RECURSION);
            assert!(repr(&block(489)).starts_with("{'a': [[[["));
            assert_eq!(repr(&block(490)), RECURSION);
            let e = safe_load(&flow(492)).unwrap_err();
            assert!(!e.is_yaml_error());
        });
    }

    #[test]
    fn nesting_100k_deep_is_an_error_not_a_stack_overflow() {
        on_main_stack(|| {
            assert_eq!(repr(&flow(100_000)), RECURSION);
            assert_eq!(repr(&format!("{}x\n", "- ".repeat(100_000))), RECURSION);
            let map = format!("{}1{}", "{b: ".repeat(100_000), "}".repeat(100_000));
            assert_eq!(repr(&map), RECURSION);
        });
    }

    /// `junk: [&a0 [x], &a1 [*a0], ...]`: anchor i is a list nested i + 1 deep, at value depth
    /// i + 2 under the root mapping, so its innermost `x` sits at depth i + 3. Python shares the
    /// aliased lists and loads any length; the port stops at a value depth of 1000.
    #[test]
    fn alias_chains_past_the_value_depth_raise_recursion_error() {
        on_main_stack(|| {
            let chain = |n: usize| {
                let mut t = "junk:\n- &a0 [x]\n".to_string();
                for i in 1..n {
                    t.push_str(&format!("- &a{i} [*a{}]\n", i - 1));
                }
                t
            };
            let ok = safe_load(&chain(997)).unwrap();
            // a996 holds 'x' in 997 lists, then the junk list and the root mapping close.
            assert!(ok.py_repr().ends_with(&format!("'x'{}}}", "]".repeat(998))));
            assert_eq!(repr(&chain(998)), RECURSION);
            assert_eq!(repr(&chain(100_000)), RECURSION);
        });
    }

    /// Python 3.15.1: `v: 1:1:...:1.5` with n parts (`yaml.safe_load(...)['v']`).
    #[test]
    fn sexagesimal_floats_use_an_int_base_and_overflow_past_the_float_range() {
        let sexa = |n: usize| format!("v: {}1.5", "1:".repeat(n - 1));
        assert_eq!(repr(&sexa(15)), "{'v': 7.969237026711864e+24}");
        assert_eq!(repr(&sexa(30)), "{'v': 3.747015588487006e+51}");
        assert_eq!(repr(&sexa(174)), "{'v': 4.240973464465452e+307}");
        assert_eq!(
            repr(&sexa(175)),
            "OverflowError: int too large to convert to float"
        );
        assert_eq!(
            repr(&sexa(200)),
            "OverflowError: int too large to convert to float"
        );
    }

    #[test]
    fn errors_match_pyyaml_text() {
        let e = safe_load("a: b: c").unwrap_err();
        assert_eq!(e.class, "ScannerError");
        assert_eq!(
            e.message,
            "mapping values are not allowed here\n  in \"<unicode string>\", line 1, column 5:\n    a: b: c\n        ^"
        );
        let e = safe_load("a: 1\n---\nb: 2\n").unwrap_err();
        assert_eq!(e.class, "ComposerError");
        assert!(e.is_yaml_error());
        let e = safe_load("d: 2026-13-01").unwrap_err();
        assert_eq!(
            (e.class, e.message.as_str(), e.is_yaml_error()),
            ("ValueError", "month must be in 1..12, not 13", false)
        );
        let e = safe_load("!!python/tuple [1]").unwrap_err();
        assert_eq!(e.class, "ConstructorError");
        assert!(e.message.starts_with(
            "could not determine a constructor for the tag 'tag:yaml.org,2002:python/tuple'"
        ));
    }
}
