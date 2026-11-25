//! Read one Claude Code transcript, line by line, into what a session card needs. Transcripts
//! reach 580 MB, so the reader never loads a whole file: it keeps counts, the first owner prompt,
//! commit subjects, written paths and a bounded dialogue (start and end) of owner prompts and the
//! agent's text replies.

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use crate::core::item::parse_as_of_epoch;
use crate::error::{Base, SmError};
use crate::py::json::loads;
use crate::py::path::PyPath;
use crate::py::str::{PyRegex, slice, strip};
use crate::py::value::{PyValue, type_name};
use crate::sources::{TextLines, dict_get, get_attr, iterate, sorted_paths, truthy};

// Owner-prompt rule from the 2026-11-04 ingestion audit: main-thread user rows
// whose turnOrigin is absent or "human", minus meta, sidechain and compact summary rows, minus
// task notices, system reminders and harness lines.
static NOT_PROMPT_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"(?s)\s*(<task-notification>.*?</task-notification>|<system-reminder>.*?</system-reminder>)\s*").expect("not-prompt pattern")
});
static HARNESS_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(concat!(
        r"^\s*(<command-name>|<command-message>|<local-command-|<teammate-message|<bash-stdout>|<bash-stderr>",
        r"|Caveat: The messages below)"
    ))
    .expect("harness pattern")
});
// Python: `\bgit\b(?:\s+-C\s+\S+)?(?:\s+-c\s+\S+)*\s+commit\b`. Every way on after `git` starts
// with `\s`, which is never a word char, so the inner `\b` always holds and is left out.
static COMMIT_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"\bgit(?:\s+-C\s+\S+)?(?:\s+-c\s+\S+)*\s+commit\b").expect("commit pattern")
});
static SM_PUT_RE: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?:^|[\s;&|(`])sm\s+put\b").expect("sm put pattern"));
// A put that stores nothing in the default store (freshness audit D1, 2026-11-06): one that asks
// for help, or one after an inline `HOME=` or `SIMPLE_MEMORY_HOME=` (a lane's scratch home).
static HOME_SET_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"(?:^|[\s;&|(`])(?:SIMPLE_MEMORY_)?HOME=").expect("home pattern")
});
static HELP_RE: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?:^|\s)(?:--help|-h)(?:\s|$)").expect("help pattern"));
// Python: `^\[[^\]\n]*?\b([0-9a-f]{7,40})\] (.+)$` (MULTILINE). The `\b` before a hex digit
// holds when the char before it is `[` or a non-word char, so the lazy run before the sha is
// empty or ends on `[^\]\n\w]`. Both ways end the sha at the line's first `]`, and only one can
// match (the empty way needs every char up to it to be hex), so `?` needs no lazy `??`.
static COMMIT_LINE_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"(?m)^\[(?:[^\]\n]*?[^\]\n\w])?([0-9a-f]{7,40})\] (.+)$")
        .expect("commit line pattern")
});
// Task id `<YYYYMMDD>-<HHMMSS>-<sid8>-<slug>-<4hex>`; the lookahead keeps a handoff name
// (`<date>-<time>-<session uuid>`) from matching.
static TASK_ID_RE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"\b\d{8}-\d{6}-[0-9a-f]{8}-[a-z0-9-]+-[0-9a-f]{4}(?![\w-])")
        .expect("task id pattern")
});
const WRITE_TOOLS: [(&str, &str); 4] = [
    ("Write", "file_path"),
    ("Edit", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];
const MAX_MESSAGE_CHARS: isize = 3000;
const MAX_PATHS: usize = 200;

/// The `SessionScan` dataclass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionScan {
    pub session_id: String,
    pub cwd: String,
    pub prompts: i64,
    /// The row time (epoch s, 0 when none parses) of each owner prompt counted.
    pub prompt_times: Vec<f64>,
    pub first_prompt: String,
    pub commits: i64,
    /// The time of the tool_use row (epoch s, 0 when none parses) of each commit counted.
    pub commit_times: Vec<f64>,
    pub commit_subjects: Vec<String>,
    pub shas: Vec<String>,
    pub task_ids: Vec<String>,
    pub files_written: Vec<String>,
    /// The newest time (epoch s) a counting `sm put` covers the session up to: the last row
    /// time at its row, infinity when that time is missing or does not parse. None: no put counts.
    pub put_until: Option<f64>,
    pub last_ts: String,
    pub dialogue: Vec<String>,
    /// The scan counts only rows after this time (epoch s); None: every row.
    pub after: Option<f64>,
}

/// `value == "<s>"`: only a str equals a str.
fn is_str(v: Option<&PyValue>, s: &str) -> bool {
    matches!(v, Some(PyValue::Str(x)) if x == s)
}

/// `str(value or "")`.
fn str_or_empty(v: Option<&PyValue>) -> String {
    match v {
        Some(v) if truthy(v) => v.py_str(),
        _ => String::new(),
    }
}

/// The TypeError of `value in <set or dict of str>` for a value with no hash.
fn unhashable(v: &PyValue, as_what: &str) -> Option<SmError> {
    matches!(
        v,
        PyValue::List(_) | PyValue::Dict(_) | PyValue::Set(_) | PyValue::Cycle(_)
    )
    .then(|| {
        let t = type_name(v);
        SmError::type_(format!(
            "cannot use '{t}' as {as_what} (unhashable type: '{t}')"
        ))
    })
}

/// `(value or {}).get(key)`.
fn or_empty_get<'a>(v: Option<&'a PyValue>, key: &str) -> Result<Option<&'a PyValue>, SmError> {
    match v {
        Some(v) if truthy(v) => get_attr(v, key),
        _ => Ok(None),
    }
}

fn texts(content: Option<&PyValue>) -> Result<Vec<String>, SmError> {
    if let Some(PyValue::Str(s)) = content {
        return Ok(vec![s.clone()]);
    }
    let blocks = match content {
        Some(c) if truthy(c) => iterate(c)?,
        _ => Vec::new(),
    };
    Ok(blocks
        .iter()
        .filter_map(|b| match &**b {
            PyValue::Dict(d) if is_str(dict_get(d, "type"), "text") => {
                Some(str_or_empty(dict_get(d, "text")))
            }
            _ => None,
        })
        .collect())
}

/// The owner's prompt text of a main-thread row, or "" when the row
/// is not one.
fn owner_prompt_text(row: &[(PyValue, PyValue)]) -> Result<String, SmError> {
    let flag = |k: &str| dict_get(row, k).is_some_and(truthy);
    if !is_str(dict_get(row, "type"), "user") || flag("isMeta") || flag("isCompactSummary") {
        return Ok(String::new());
    }
    if let Some(origin) = dict_get(row, "turnOrigin")
        && truthy(origin)
        && !is_str(Some(origin), "human")
    {
        return Ok(String::new());
    }
    let mut kept = Vec::new();
    for text in texts(or_empty_get(dict_get(row, "message"), "content")?)? {
        let cleaned = NOT_PROMPT_RE.sub(&text, " ", 0);
        let cleaned = strip(&cleaned);
        if !cleaned.is_empty() && HARNESS_RE.search(cleaned).is_none() {
            kept.push(cleaned.to_string());
        }
    }
    Ok(kept.join("\n"))
}

fn result_text(block: &[(PyValue, PyValue)]) -> Result<String, SmError> {
    let content = dict_get(block, "content");
    if let Some(PyValue::Str(s)) = content {
        return Ok(s.clone());
    }
    Ok(texts(content)?.join("\n"))
}

/// `_Dialogue`: keep the first `half` characters of messages and the last `half` characters
/// after them.
struct Dialogue {
    half: i64,
    head: Vec<String>,
    head_chars: i64,
    tail: VecDeque<(String, i64)>,
    tail_chars: i64,
    dropped: bool,
}

impl Dialogue {
    fn new(half: i64) -> Dialogue {
        Dialogue {
            half,
            head: Vec::new(),
            head_chars: 0,
            tail: VecDeque::new(),
            tail_chars: 0,
            dropped: false,
        }
    }

    fn add(&mut self, text: String) {
        let n = text.chars().count() as i64;
        if self.head_chars + n <= self.half && self.tail.is_empty() {
            self.head.push(text);
            self.head_chars += n;
            return;
        }
        self.tail.push_back((text, n));
        self.tail_chars += n;
        while self.tail_chars > self.half && self.tail.len() > 1 {
            let (_, m) = self.tail.pop_front().expect("the tail holds more than one");
            self.tail_chars -= m;
            self.dropped = true;
        }
    }

    fn lines(self) -> Vec<String> {
        let mut out = self.head;
        if self.dropped {
            out.push("[...]".into());
        }
        out.extend(self.tail.into_iter().map(|(t, _)| t));
        out
    }
}

fn add_unique(target: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !target.iter().any(|t| t == value) && target.len() < MAX_PATHS {
        target.push(value.to_string());
    }
}

/// A shell context that `blank_quotes` walks through.
enum Frame {
    /// `Command(closer, open)`: the top level (no closer), or a `$( … )` or a backtick command in
    /// double quotes, with the count of `(` still open inside it.
    Command(Option<u8>, u32),
    /// `Quote(closer, escapes)`: `'…'`, `$'…'` or `"…"`; in the last two a backslash escapes.
    Quote(u8, bool),
}

/// `command` with every quoted string (`'…'`, `$'…'`, `"…"`, quotes included), every `#` comment
/// and every heredoc body turned into spaces of the same byte length, so offsets still match. A
/// `$( … )` or a backtick inside double quotes is a command again and stays; a backslash outside
/// quotes escapes the next char and stays; an unclosed quote runs to the end; a `#` at a word start
/// outside quotes runs to the end of its line; a heredoc body runs from the newline after its
/// `<<` (or `<<-`) through the line that holds only its delimiter, or to the end.
fn blank_quotes(command: &str) -> String {
    let raw = command.as_bytes();
    let mut out = raw.to_vec();
    let mut stack = vec![Frame::Command(None, 0)];
    // the delimiter and `<<-` flag of each heredoc whose body starts at the next newline
    let mut heredocs: Vec<(Vec<u8>, bool)> = Vec::new();
    // the index of the last char a backslash outside quotes escaped
    let mut escaped = usize::MAX;
    let mut i = 0;
    while i < raw.len() {
        let (b, next) = (raw[i], raw.get(i + 1).copied());
        let top = stack.last_mut().expect("the top level is never popped");
        // the bytes this step covers, and whether they are quoted text
        let (step, quoted) = match top {
            Frame::Quote(_, true) if b == b'\\' => (2, true),
            Frame::Quote(closer, _) if b == *closer => {
                stack.pop();
                (1, true)
            }
            Frame::Quote(b'"', _) if b == b'`' => {
                stack.push(Frame::Command(Some(b'`'), 0));
                (1, false)
            }
            Frame::Quote(b'"', _) if b == b'$' && next == Some(b'(') => {
                stack.push(Frame::Command(Some(b')'), 0));
                (2, false)
            }
            Frame::Quote(..) => (1, true),
            Frame::Command(closer, open) => match b {
                b'\\' => {
                    // a backslash before a newline joins the lines and escapes nothing
                    if next != Some(b'\n') {
                        escaped = i + 1;
                    }
                    (2, false)
                }
                b'\'' | b'"' => {
                    stack.push(Frame::Quote(b, b == b'"'));
                    (1, true)
                }
                b'$' if next == Some(b'\'') => {
                    stack.push(Frame::Quote(b'\'', true));
                    (2, true)
                }
                b'(' => {
                    *open += 1;
                    (1, false)
                }
                b')' if *open > 0 => {
                    *open -= 1;
                    (1, false)
                }
                b')' | b'`' if *closer == Some(b) => {
                    stack.pop();
                    (1, false)
                }
                b'<' if raw[i..].starts_with(b"<<<") => (3, false),
                b'<' if next == Some(b'<') => {
                    let (word, dash, end) = heredoc_word(raw, i + 2);
                    // a << with no word is no heredoc (bash rejects it): `$((a<<(b)))`
                    if !word.is_empty() {
                        heredocs.push((word, dash));
                    }
                    (end - i, false)
                }
                b'\n' if !heredocs.is_empty() => {
                    let end = heredocs
                        .drain(..)
                        .fold(i, |at, (word, dash)| heredoc_end(raw, at, &word, dash));
                    out[i + 1..end].fill(b' ');
                    (end - i, false)
                }
                b'#' if i == 0 || b" \t\n;&|()".contains(&raw[i - 1]) && i - 1 != escaped => {
                    let line = raw[i..].iter().position(|&c| c == b'\n');
                    (line.unwrap_or(raw.len() - i), true)
                }
                _ => (1, false),
            },
        };
        if quoted {
            out.iter_mut().skip(i).take(step).for_each(|o| *o = b' ');
        }
        i += step;
    }
    // still UTF-8: the frame changes only at an ASCII byte, so a char has all its bytes blanked
    // or none
    String::from_utf8(out).expect("blanking keeps UTF-8")
}

/// The delimiter of a heredoc whose `<<` ends at `at`, with its quotes and backslashes removed,
/// whether it is `<<-`, and where the word ends.
fn heredoc_word(raw: &[u8], mut at: usize) -> (Vec<u8>, bool, usize) {
    let dash = raw.get(at) == Some(&b'-');
    at += usize::from(dash);
    while matches!(raw.get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    let mut word = Vec::new();
    while let Some(&c) = raw.get(at).filter(|c| !b" \t\n;&|()<>".contains(c)) {
        if !matches!(c, b'\'' | b'"' | b'\\') {
            word.push(c);
        }
        at += 1;
    }
    (word, dash, at)
}

/// The end of a heredoc body that starts after the newline at `at`: the end of the first line
/// that holds only `word` (after leading tabs for `<<-`), or the end of `raw`.
fn heredoc_end(raw: &[u8], at: usize, word: &[u8], dash: bool) -> usize {
    let mut start = at + 1;
    while start < raw.len() {
        let end = raw[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(raw.len(), |n| start + n);
        let line = &raw[start..end];
        let tabs = if dash {
            line.iter().take_while(|&&c| c == b'\t').count()
        } else {
            0
        };
        if &line[tabs..] == word {
            return end;
        }
        start = end + 1;
    }
    raw.len()
}

/// True when a Bash command runs an `sm put` that writes to the default store: outside quoted
/// text, a put with no help flag in its own command and no `HOME=` or `SIMPLE_MEMORY_HOME=`
/// before it.
fn puts_to_store(command: &str) -> bool {
    let bare = blank_quotes(command);
    // a match that starts on a blanked byte touches a quote, which glues to its word (`"x"sm`)
    let unquoted = |at: usize| bare.as_bytes()[at] == command.as_bytes()[at];
    SM_PUT_RE.finditer(&bare).iter().any(|m| {
        let own = bare[m.end()..]
            .split([';', '&', '|', '\n'])
            .next()
            .unwrap_or("");
        unquoted(m.start())
            && !HOME_SET_RE
                .finditer(&bare[..m.start()])
                .iter()
                .any(|h| unquoted(h.start()))
            && HELP_RE.search(own).is_none()
    })
}

/// The `tool_use_id` of a `tool_result` block that reports an error.
fn errored_tool_id(block: &[(PyValue, PyValue)]) -> Option<&str> {
    if !is_str(dict_get(block, "type"), "tool_result")
        || !dict_get(block, "is_error").is_some_and(truthy)
    {
        return None;
    }
    match dict_get(block, "tool_use_id") {
        Some(PyValue::Str(s)) => Some(s),
        _ => None,
    }
}

/// The epoch seconds of a transcript timestamp; None when it does not parse.
fn epoch_of(ts: &str) -> Result<Option<f64>, SmError> {
    Ok(Some(parse_as_of_epoch(Some(ts))?).filter(|t| *t != 0.0))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `json.detect_encoding(b)`.
fn detect_encoding(b: &[u8]) -> &'static str {
    if b.starts_with(&[0, 0, 0xfe, 0xff]) || b.starts_with(&[0xff, 0xfe, 0, 0]) {
        return "utf-32";
    }
    if b.starts_with(&[0xfe, 0xff]) || b.starts_with(&[0xff, 0xfe]) {
        return "utf-16";
    }
    if b.starts_with(&[0xef, 0xbb, 0xbf]) {
        return "utf-8-sig";
    }
    if b.len() >= 4 {
        if b[0] == 0 {
            return if b[1] != 0 { "utf-16-be" } else { "utf-32-be" };
        }
        if b[1] == 0 {
            return if b[2] != 0 || b[3] != 0 {
                "utf-16-le"
            } else {
                "utf-32-le"
            };
        }
    } else if b.len() == 2 {
        if b[0] == 0 {
            return "utf-16-be";
        }
        if b[1] == 0 {
            return "utf-16-le";
        }
    }
    "utf-8"
}

/// A UTF-16 or UTF-32 decode with `errors="surrogatepass"`. A lone surrogate, which a Rust str
/// cannot hold, becomes U+FFFD (the accepted lone-surrogate difference).
fn decode_wide(
    b: &[u8],
    start: usize,
    unit: usize,
    big: bool,
    codec: &str,
) -> Result<String, SmError> {
    let err = |pos: usize, end: usize, why: &str| {
        let at = if end - pos == 1 {
            format!("byte 0x{:02x} in position {pos}", b[pos])
        } else {
            format!("bytes in position {pos}-{}", end - 1)
        };
        SmError::new(
            Base::Value,
            "UnicodeDecodeError",
            format!("'{codec}' codec can't decode {at}: {why}"),
        )
    };
    let mut units = Vec::new();
    let mut i = start;
    while i + unit <= b.len() {
        let w = &b[i..i + unit];
        let v = w.iter().fold(0u32, |acc, &x| acc << 8 | u32::from(x));
        let v = if big {
            v
        } else {
            w.iter().rev().fold(0u32, |acc, &x| acc << 8 | u32::from(x))
        };
        if unit == 4 && v > 0x10_FFFF {
            return Err(err(i, i + 4, "code point not in range(0x110000)"));
        }
        units.push(v);
        i += unit;
    }
    if i < b.len() {
        return Err(err(i, b.len(), "truncated data"));
    }
    if unit == 4 {
        return Ok(units
            .into_iter()
            .map(|v| char::from_u32(v).unwrap_or('\u{fffd}'))
            .collect());
    }
    Ok(char::decode_utf16(units.into_iter().map(|u| u as u16))
        .map(|c| c.unwrap_or('\u{fffd}'))
        .collect())
}

/// A UTF-8 decode from `skip` with `errors="surrogatepass"`: `ED A0-BF 80-BF` is a lone
/// surrogate, which a Rust str cannot hold, so it becomes U+FFFD (the accepted lone-surrogate
/// difference). Any other bad byte raises Python's UnicodeDecodeError, its position counted
/// after `skip` (utf-8-sig counts from after the BOM: `b'\xef\xbb\xbf"\xff"'` fails at 1).
fn utf8_surrogatepass(b: &[u8], skip: usize) -> Result<String, SmError> {
    let mut copy = b[skip..].to_vec();
    let mut off = 0;
    while let Err(e) = std::str::from_utf8(&copy[off..]) {
        let v = off + e.valid_up_to();
        if !(copy[v] == 0xed
            && copy.get(v + 1).is_some_and(|x| (0xa0..=0xbf).contains(x))
            && copy.get(v + 2).is_some_and(|x| (0x80..=0xbf).contains(x)))
        {
            break;
        }
        // U+FFFD is 3 bytes too, so every later position stays where it was
        copy[v..v + 3].copy_from_slice("\u{fffd}".as_bytes());
        off = v + 3;
    }
    // every surrogate before the first other bad byte is replaced, so that byte is the error
    std::str::from_utf8(&copy)
        .map(str::to_string)
        .map_err(|e| SmError::unicode_decode(&copy, e))
}

/// `json.loads(raw)` of bytes: the text is decoded first (`detect_encoding`, surrogatepass), so a
/// bad byte raises UnicodeDecodeError, which is not a JSONDecodeError.
fn loads_bytes(b: &[u8]) -> Result<PyValue, SmError> {
    let text = match detect_encoding(b) {
        "utf-8" => utf8_surrogatepass(b, 0)?,
        "utf-8-sig" => utf8_surrogatepass(b, 3)?,
        "utf-16" => {
            let big = b[0] == 0xfe;
            decode_wide(b, 2, 2, big, if big { "utf-16-be" } else { "utf-16-le" })?
        }
        "utf-32" => {
            let big = b[0] == 0;
            decode_wide(b, 4, 4, big, if big { "utf-32-be" } else { "utf-32-le" })?
        }
        codec => decode_wide(
            b,
            0,
            if codec.starts_with("utf-16") { 2 } else { 4 },
            codec.ends_with("be"),
            codec,
        )?,
    };
    // A U+FEFF left after the codec (two BOMs) is "Expecting value" in Python and "Unexpected
    // UTF-8 BOM" in `loads`; both are JSONDecodeError, which `row_of`, the only caller, skips.
    loads(&text)
}

/// `json.loads(raw)`, with a JSONDecodeError as None.
fn row_of(raw: &[u8]) -> Result<Option<PyValue>, SmError> {
    match loads_bytes(raw) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.python_name() == "JSONDecodeError" => Ok(None),
        Err(e) => Err(e),
    }
}

/// `scan_transcript(path, dialogue_chars=30000)`: stream one main transcript
/// (`<project>/<session id>.jsonl`) into a SessionScan. With `after`, prompts, commits, ids,
/// writes and the dialogue come only from rows whose timestamp, or the last one before them, is
/// later; cwd, last_ts and puts still come from every row.
pub fn scan_transcript(
    path: &PyPath,
    dialogue_chars: i64,
    after: Option<f64>,
) -> Result<SessionScan, SmError> {
    let mut scan = SessionScan {
        session_id: path.stem().to_string(),
        after,
        ..SessionScan::default()
    };
    let mut dialogue = Dialogue::new(dialogue_chars.div_euclid(2));
    // tool id -> the time of its row
    let mut pending_commits: HashMap<String, f64> = HashMap::new();
    // (tool id, time covered) of the puts that count, until a result shows an error
    let mut puts: Vec<(String, f64)> = Vec::new();
    let mut later = after.is_none();
    let mut lines = TextLines::open(path)?;
    while let Some(raw) = lines.next_bytes()? {
        if !contains(&raw, br#""type":"user""#) && !contains(&raw, br#""type":"assistant""#) {
            continue;
        }
        let Some(row) = row_of(&raw)? else { continue };
        let PyValue::Dict(row) = row else { continue };
        if dict_get(&row, "isSidechain").is_some_and(truthy) {
            continue;
        }
        if scan.cwd.is_empty()
            && let Some(cwd) = dict_get(&row, "cwd").filter(|v| truthy(v))
        {
            scan.cwd = cwd.py_str();
        }
        if let Some(ts) = dict_get(&row, "timestamp").filter(|v| truthy(v)) {
            scan.last_ts = ts.py_str();
            if let Some(t) = after {
                later = epoch_of(&scan.last_ts)?.is_some_and(|r| r > t);
            }
        }
        let content = or_empty_get(dict_get(&row, "message"), "content")?;
        let kind = dict_get(&row, "type");
        if is_str(kind, "user") {
            if let Some(PyValue::List(blocks)) = content {
                for block in blocks {
                    let PyValue::Dict(block) = block else {
                        continue;
                    };
                    if let Some(id) = errored_tool_id(block) {
                        puts.retain(|p| p.0 != id);
                    }
                    if !is_str(dict_get(block, "type"), "tool_result") {
                        continue;
                    }
                    let id = match dict_get(block, "tool_use_id") {
                        Some(PyValue::Str(s)) if pending_commits.contains_key(s) => s.clone(),
                        Some(v) => match unhashable(v, "a set element") {
                            Some(e) => return Err(e),
                            None => continue,
                        },
                        None => continue,
                    };
                    let at = pending_commits.remove(&id).unwrap_or_default();
                    let result = result_text(block)?;
                    if dict_get(block, "is_error").is_some_and(truthy)
                        || result.contains("nothing to commit")
                        || result.contains("no changes added")
                    {
                        continue;
                    }
                    scan.commits += 1;
                    scan.commit_times.push(at);
                    for m in COMMIT_LINE_RE.finditer(&result) {
                        add_unique(&mut scan.shas, m.group(1).unwrap_or(""));
                        add_unique(&mut scan.commit_subjects, strip(m.group(2).unwrap_or("")));
                    }
                }
            }
            let prompt = owner_prompt_text(&row)?;
            if later && !prompt.is_empty() {
                scan.prompts += 1;
                scan.prompt_times
                    .push(epoch_of(&scan.last_ts)?.unwrap_or(0.0));
                if scan.first_prompt.is_empty() {
                    scan.first_prompt = prompt.clone();
                }
                for m in TASK_ID_RE.finditer(&prompt) {
                    add_unique(&mut scan.task_ids, m.as_str());
                }
                dialogue.add(format!(
                    "Owner: {}",
                    slice(&prompt, None, Some(MAX_MESSAGE_CHARS))
                ));
            }
        } else if is_str(kind, "assistant")
            && let Some(PyValue::List(blocks)) = content
        {
            for block in blocks {
                let PyValue::Dict(block) = block else {
                    continue;
                };
                let btype = dict_get(block, "type");
                if !later && !is_str(btype, "tool_use") {
                    continue;
                }
                if is_str(btype, "text")
                    && !strip(&str_or_empty(dict_get(block, "text"))).is_empty()
                {
                    let full = dict_get(block, "text")
                        .map(PyValue::py_str)
                        .unwrap_or_default();
                    let text = strip(&full);
                    for m in TASK_ID_RE.finditer(text) {
                        add_unique(&mut scan.task_ids, m.as_str());
                    }
                    dialogue.add(format!(
                        "Agent: {}",
                        slice(text, None, Some(MAX_MESSAGE_CHARS))
                    ));
                } else if is_str(btype, "tool_use") {
                    let input = dict_get(block, "input").filter(|v| truthy(v));
                    let name = dict_get(block, "name");
                    if is_str(name, "Bash") {
                        let command = str_or_empty(
                            input.map(|i| get_attr(i, "command")).transpose()?.flatten(),
                        );
                        if later
                            && COMMIT_RE.search(&command).is_some()
                            && !command.contains("--dry-run")
                        {
                            pending_commits.insert(
                                dict_get(block, "id")
                                    .map_or_else(|| "None".to_string(), PyValue::py_str),
                                epoch_of(&scan.last_ts)?.unwrap_or(0.0),
                            );
                        }
                        if puts_to_store(&command) {
                            puts.push((
                                dict_get(block, "id")
                                    .map_or_else(|| "None".to_string(), PyValue::py_str),
                                epoch_of(&scan.last_ts)?.unwrap_or(f64::INFINITY),
                            ));
                        }
                    } else if !later {
                        continue;
                    } else if let Some(name) = name {
                        if let Some(e) = unhashable(name, "a dict key") {
                            return Err(e);
                        }
                        let PyValue::Str(name) = name else { continue };
                        let Some((_, key)) = WRITE_TOOLS.iter().find(|(t, _)| t == name) else {
                            continue;
                        };
                        let target =
                            str_or_empty(input.map(|i| get_attr(i, key)).transpose()?.flatten());
                        add_unique(&mut scan.files_written, &target);
                    }
                }
            }
        }
    }
    scan.dialogue = dialogue.lines();
    scan.put_until = newest(&puts);
    Ok(scan)
}

/// The newest time of the puts that count.
fn newest(puts: &[(String, f64)]) -> Option<f64> {
    puts.iter().map(|p| p.1).reduce(f64::max)
}

/// `lanes_put_until(path)`: the newest time a counting `sm put` in a lane of this session
/// (`<project>/<session id>/subagents/*.jsonl`) covers it up to, as `SessionScan::put_until`
/// (a put row with no time of its own covers it all).
pub fn lanes_put_until(path: &PyPath) -> Result<Option<f64>, SmError> {
    let lane_dir = path.parent().join(path.stem()).join("subagents");
    if !lane_dir.is_dir() {
        return Ok(None);
    }
    let mut until: Option<f64> = None;
    for lane in sorted_paths(lane_dir.glob("*.jsonl")?) {
        let mut lines = TextLines::open(&lane)?;
        let mut puts: Vec<(String, f64)> = Vec::new();
        while let Some(raw) = lines.next_bytes()? {
            let put_row = contains(&raw, b"sm put") && contains(&raw, br#""type":"assistant""#);
            // an error result matters only after a put that counts (and parsing every error
            // row of a long lane costs seconds over a full scan)
            if !put_row && (puts.is_empty() || !contains(&raw, br#""is_error":true"#)) {
                continue;
            }
            let Some(row) = row_of(&raw)? else { continue };
            let message = match get_attr(&row, "message")? {
                Some(m) if truthy(m) => get_attr(m, "content")?,
                _ => None,
            };
            let blocks = match message {
                Some(c) if truthy(c) => iterate(c)?,
                _ => Vec::new(),
            };
            for block in &blocks {
                let PyValue::Dict(b) = &**block else { continue };
                if let Some(id) = errored_tool_id(b) {
                    puts.retain(|p| p.0 != id);
                }
                if !is_str(dict_get(b, "type"), "tool_use") || !is_str(dict_get(b, "name"), "Bash")
                {
                    continue;
                }
                let input = dict_get(b, "input").filter(|v| truthy(v));
                let command =
                    str_or_empty(input.map(|i| get_attr(i, "command")).transpose()?.flatten());
                if puts_to_store(&command) {
                    let ts = str_or_empty(get_attr(&row, "timestamp")?);
                    puts.push((
                        dict_get(b, "id").map_or_else(|| "None".to_string(), PyValue::py_str),
                        epoch_of(&ts)?.unwrap_or(f64::INFINITY),
                    ));
                }
            }
        }
        until = until.into_iter().chain(newest(&puts)).reduce(f64::max);
    }
    Ok(until)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap()
    }

    fn write(dir: &std::path::Path, rel: &str, data: &[u8]) -> PyPath {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        PyPath::new(p.to_str().unwrap())
    }

    fn all(re: &PyRegex, text: &str) -> Vec<String> {
        re.finditer(text)
            .iter()
            .map(|m| m.as_str().to_string())
            .collect()
    }

    // Values from Python 3.15.1 on the original patterns (probe, 2026-11-05). The two rewritten
    // patterns drop a middle `\b`; these are the inputs where a word char sits on either side.
    #[test]
    fn rewritten_patterns_match_like_the_originals() {
        assert_eq!(
            all(&COMMIT_RE, "git -C /r -c a=b commit -m x"),
            ["git -C /r -c a=b commit"]
        );
        assert!(all(&COMMIT_RE, "gitcommit").is_empty());
        assert_eq!(all(&COMMIT_RE, "git\u{3000}commit"), ["git\u{3000}commit"]);
        assert!(all(&COMMIT_RE, "\u{b2}git commit").is_empty());
        assert_eq!(all(&COMMIT_RE, "git commit\u{301}"), ["git commit"]);
        assert!(all(&COMMIT_RE, "git -C commit").is_empty());
        let lines = |t: &str| -> Vec<(String, String)> {
            COMMIT_LINE_RE
                .finditer(t)
                .iter()
                .map(|m| {
                    (
                        m.group(1).unwrap_or("").to_string(),
                        m.group(2).unwrap_or("").to_string(),
                    )
                })
                .collect()
        };
        assert_eq!(
            lines("[main (root-commit) 1a2b3c4] init"),
            [("1a2b3c4".to_string(), "init".to_string())]
        );
        assert!(lines("[x_1a2b3c4] s").is_empty());
        assert!(lines("[\u{e9}1a2b3c4] s").is_empty());
        assert_eq!(lines("[x 1234567] a\n[y 89abcde] b").len(), 2);
    }

    #[test]
    fn sm_put_and_task_id_edges() {
        assert_eq!(all(&SM_PUT_RE, "a;sm put"), [";sm put"]);
        assert!(all(&SM_PUT_RE, "xsm put").is_empty());
        assert_eq!(all(&SM_PUT_RE, "sm put\u{301}"), ["sm put"]);
        assert_eq!(all(&SM_PUT_RE, "\u{1c}sm put"), ["\u{1c}sm put"]);
        let id = "20261104-123456-abcdef12-my-slug-1a2b";
        assert_eq!(all(&TASK_ID_RE, id), [id]);
        assert_eq!(
            all(&TASK_ID_RE, "20261104-123456-abcdef12-s-1a2b\u{301}"),
            ["20261104-123456-abcdef12-s-1a2b"]
        );
        assert!(all(&TASK_ID_RE, "x20261104-123456-abcdef12-s-1a2b").is_empty());
    }

    #[test]
    fn a_put_counts_only_when_it_writes_to_the_default_store() {
        let counts = [
            ("sm put --kind fact x", true),
            ("sm put --help", false),
            ("sm put x -h", false),
            ("sm put --kind fact x; sm put --help", true),
            ("sm put --help && sm put x", true),
            ("ls -h; sm put x", true),
            ("sm put x; du -h", true),
            // a help flag inside a quoted body is text, not a flag (review U1 row 1)
            (
                "sm put --kind fact --tag decided 'rg -h prints the short help; use --help for all'",
                true,
            ),
            (r#"sm put "say \" --help \" here""#, true),
            ("sm put --help 'body'", false),
            ("sm put '\u{e9}t\u{e9} -h' -h", false),
            ("sm put '\u{e9}t\u{e9} -h'", true),
            ("sm put 'unclosed -h", true),
            (r"sm put it\'s -h", false),
            ("sm put --kind fact -hx", true),
            ("HOME=/s sm put x", false),
            ("cd /w && SIMPLE_MEMORY_HOME=/s/.sm sm put x", false),
            ("export SIMPLE_MEMORY_HOME=/s; sm put x", false),
            ("MYHOME=/s sm put x", true),
            ("sm query put", false),
            // quoted text is never a put or a HOME= prefix, only text
            ("echo \"sm put --help\"", false),
            ("echo \"use sm put --help\"", false),
            ("echo 'a; sm put x'", false),
            ("echo $'sm put x'", false),
            ("echo 'HOME=/x'; sm put y", true),
            ("echo 'set HOME=/x'; sm put y", true),
            ("echo \"run SIMPLE_MEMORY_HOME=/x first\" && sm put y", true),
            ("sm put \"body with sm put inside\"", true),
            ("sm put \"body with sm put --help inside\"", true),
            ("echo \"a\"; sm put x", true),
            // a quote that touches the match glues to its word: `xsm`, `xHOME=` are no command
            ("echo \"x\"sm put y", false),
            ("echo \"x\"HOME=/s; sm put y", true),
            // a `#` at a word start opens a comment to the end of the line; its apostrophe is text
            (
                "# lane's scratch home\nexport HOME=/x\n# don't write the real store\nsm put --kind fact y",
                false,
            ),
            ("echo a#b; sm put x", true),
            ("ls # sm put x\nls", false),
            ("echo \"#\"; sm put x", true),
            // a comment starts after a newline or `;` too, and ends at its line
            ("echo a\n# it's\nexport HOME=/x\n# don't\nsm put y", false),
            ("ls;# it's\nexport HOME=/x\nsm put y", false),
            ("ls;# sm put x\nls", false),
            ("# note\nsm put x", true),
            // a `#` after an escaped blank is inside a word
            ("echo \\ #x; sm put y", true),
            ("echo \\\\ #x; sm put y", false),
            // every HOME= before the put is judged, and none after it
            ("echo \"x\"HOME=/s; HOME=/t sm put y", false),
            ("sm put x; HOME=/s true", true),
            // a `$( … )` or a backtick inside double quotes still runs in the shell
            ("echo \"$(sm put x)\"", true),
            ("ID=\"$(sm put --kind fact y)\"", true),
            ("echo \"`sm put x`\"", true),
            ("echo \"$( (cd /w) ; sm put x )\"", true),
            ("echo \"$(echo ')'; sm put x)\"", true),
            ("echo \"$(true) sm put x\"", false),
            ("echo \"`true` sm put x\"", false),
            ("echo \"$(sm put 'a -h')\"", true),
            // in `$'…'` a backslash escapes the next char
            (r"echo $'it\'s'; sm put x", true),
            (r"echo $'it\'s; sm put x'", false),
            // a heredoc body is text, up to the line that holds only its delimiter
            (
                "git commit -m \"$(cat <<'EOF'\nrun sm put x here\nEOF\n)\"",
                false,
            ),
            ("cat <<'EOF' > f\nrun sm put x\nEOF", false),
            ("cat <<EOF\nfix: use `sm put` for a fact\nEOF", false),
            ("cat <<-EOF > f\n\tsm put x\n\tEOF\nsm put y", true),
            ("cat <<\"EOF\" > f\nsm put x\nEOF\nsm put y", true),
            ("cat <<EOF > f\nsm put x\nEOF2\nsm put y", false),
            ("cat <<A <<B\nsm put x\nA\nsm put y\nB\nls", false),
            ("cat <<<'sm put x'; sm put y", true),
            ("cat <<<'x'\nsm put y", true),
            ("cat << EOF > f\nsm put x\nEOF\nsm put y", true),
            ("cat <<EOF>f\nsm put x\nEOF\nsm put y", true),
            // a << with no word opens no heredoc (bash rejects it), so the empty line ends nothing
            ("echo $((a<<(b)))\nexport HOME=/s\n\nsm put y", false),
            // a backslash before a newline joins the lines, so the # still starts a comment
            ("ls \\\n# sm put x", false),
            ("ls\t# sm put x\nls", false),
            // lead decision 2026-11-06: with heredoc bodies blanked the real put after EOF runs in
            // the shell and writes the default store; it was false only because the body's
            // apostrophe opened a quote
            ("cat <<'EOF' > f\nA's note\nEOF\nsm put x", true),
            // a known false miss (lead decision 2026-11-06: an extra card is the harmless side):
            // a `bash -c '…'` or `bash -c "…"` script is not read as commands
            ("bash -c 'cd /w && sm put x'", false),
            ("bash -c \"cd /w && sm put x\"", false),
            ("bash <<EOF\nsm put x\nEOF", false),
        ];
        let wrong: Vec<&str> = counts
            .iter()
            .filter(|(command, want)| puts_to_store(command) != *want)
            .map(|(command, _)| *command)
            .collect();
        assert!(wrong.is_empty(), "wrong: {wrong:?}");
    }

    const SESSION: &str = concat!(
        r#"{"type":"user","message":{"content":"Fix 20261104-123456-abcdef12-my-slug-1a2b"},"cwd":"/w/anvil","timestamp":"T1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":" ok "},"#,
        r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"git commit -m one"}},"#,
        r#"{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git commit --dry-run"}},"#,
        r#"{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"sm put x"}},"#,
        r#"{"type":"tool_use","name":"Write","input":{"file_path":"/x/a.py"}},"#,
        r#"{"type":"tool_use","name":"Edit","input":{"file_path":"/x/a.py"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"[main 1a2b3c4] one"},"#,
        r#"{"type":"tool_result","tool_use_id":"t2","content":"[main 7654321] dry"}]},"timestamp":"T2"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t4","name":"Bash","input":{"command":"git commit -m two"}},"#,
        r#"{"type":"tool_use","id":"t5","name":"Bash","input":{"command":"git commit -m none"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t4","content":[{"type":"text","text":"[main abcdef1] two"}]},"#,
        r#"{"type":"tool_result","tool_use_id":"t5","content":"nothing to commit"}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":"side"},"isSidechain":true}"#,
        "\n",
        r#"{"type":"user","message":{"content":"meta"},"isMeta":true}"#,
        "\n",
        r#"{"type":"user","message":{"content":"<command-name>/x</command-name>"}}"#,
        "\n",
        r#"{"type":"user","message":{"content":"Two <system-reminder>r</system-reminder>"}}"#,
        "\n",
    );

    #[test]
    fn scan_counts_prompts_commits_writes_and_dialogue() {
        let d = tmp();
        let scan =
            scan_transcript(&write(d.path(), "s.jsonl", SESSION.as_bytes()), 30000, None).unwrap();
        let id = "20261104-123456-abcdef12-my-slug-1a2b";
        assert_eq!(
            scan,
            SessionScan {
                session_id: "s".into(),
                cwd: "/w/anvil".into(),
                prompts: 2,
                prompt_times: vec![0.0, 0.0],
                first_prompt: format!("Fix {id}"),
                commits: 2,
                commit_times: vec![0.0, 0.0],
                commit_subjects: vec!["one".into(), "two".into()],
                shas: vec!["1a2b3c4".into(), "abcdef1".into()],
                task_ids: vec![id.into()],
                files_written: vec!["/x/a.py".into()],
                put_until: Some(f64::INFINITY),
                last_ts: "T2".into(),
                dialogue: vec![
                    format!("Owner: Fix {id}"),
                    "Agent: ok".into(),
                    "Owner: Two".into()
                ],
                after: None,
            }
        );
    }

    #[test]
    fn no_changes_added_is_not_a_commit() {
        let d = tmp();
        let rows = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"git commit -am x"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"no changes added to commit (use git add)\n[m 1111111] fake"}]}}"#,
            "\n",
        );
        let scan =
            scan_transcript(&write(d.path(), "n.jsonl", rows.as_bytes()), 30000, None).unwrap();
        assert_eq!((scan.commits, scan.shas.len()), (0, 0));
    }

    #[test]
    fn dialogue_keeps_head_and_tail() {
        let d = tmp();
        let mut text = String::new();
        for i in 0..4 {
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"p{i} zzzzzzzzzz\"}}}}\n"
            ));
        }
        text.push_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"reply"}]}}"#,
        );
        let p = write(d.path(), "dia.jsonl", text.as_bytes());
        let dia = |n| scan_transcript(&p, n, None).unwrap().dialogue;
        assert_eq!(dia(0), ["[...]", "Agent: reply"]);
        assert_eq!(dia(20), ["[...]", "Agent: reply"]);
        assert_eq!(dia(41), ["Owner: p0 zzzzzzzzzz", "[...]", "Agent: reply"]);
        assert_eq!(dia(30000).len(), 5);
    }

    #[test]
    fn malformed_rows_raise_pythons_errors() {
        let d = tmp();
        let err = |name: &str, row: &str| {
            scan_transcript(
                &write(d.path(), name, format!("{row}\n").as_bytes()),
                30000,
                None,
            )
            .unwrap_err()
            .python_text()
        };
        assert_eq!(
            err(
                "idlist.jsonl",
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":["x"]}]}}"#
            ),
            "TypeError: cannot use 'list' as a set element (unhashable type: 'list')"
        );
        assert_eq!(
            err("msgstr.jsonl", r#"{"type":"user","message":"s"}"#),
            "AttributeError: 'str' object has no attribute 'get'"
        );
        assert_eq!(
            err(
                "namelist.jsonl",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":["Bash"],"input":{}}]}}"#
            ),
            "TypeError: cannot use 'list' as a dict key (unhashable type: 'list')"
        );
        let bad = write(
            d.path(),
            "bad.jsonl",
            b"{\"type\":\"user\",\"message\":{\"content\":\"\xff\"}}\n",
        );
        assert_eq!(
            scan_transcript(&bad, 30000, None)
                .unwrap_err()
                .python_text(),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 37: invalid start byte"
        );
    }

    #[test]
    fn deep_rows_scan_without_overflowing_the_stack() {
        let d = tmp();
        let row = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":{}{}}}}}\n",
            "[".repeat(9000),
            "]".repeat(9000)
        );
        let path = write(d.path(), "deep.jsonl", row.as_bytes());
        // On an 8 MiB thread, the main thread's stack (py/json.rs depth tests): the test thread's
        // 2 MiB cannot drop a value this deep. Before `iterate` borrowed, its deep clone aborted here.
        let scan = std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(8 << 20)
                .spawn_scoped(s, || scan_transcript(&path, 30000, None))
                .unwrap()
                .join()
                .unwrap()
        })
        .unwrap();
        assert_eq!(scan.prompts, 0);
    }

    #[test]
    fn lanes_put_until_reads_bash_commands_only() {
        let d = tmp();
        let main = write(d.path(), "s.jsonl", b"");
        let other = write(d.path(), "dia.jsonl", b"");
        assert_eq!(lanes_put_until(&main).unwrap(), None);
        let bash = |c: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"l","name":"Bash","input":{{"command":"{c}"}}}}]}}}}"#
            )
        };
        let text =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sm put"}]}}"#;
        write(
            d.path(),
            "s/subagents/a.jsonl",
            format!("{text}\n{}\n", bash("xsm put")).as_bytes(),
        );
        assert_eq!(lanes_put_until(&main).unwrap(), None);
        // `sm\tput` matches the pattern, but the raw line holds `sm\\tput`, not "sm put", so
        // Python never parses it.
        write(
            d.path(),
            "s/subagents/a2.jsonl",
            format!("{}\n", bash("sm\\tput x")).as_bytes(),
        );
        assert_eq!(lanes_put_until(&main).unwrap(), None);
        write(
            d.path(),
            "s/subagents/b.jsonl",
            format!("{}\n", bash("cd x && sm put --kind fact y")).as_bytes(),
        );
        assert_eq!(lanes_put_until(&main).unwrap(), Some(f64::INFINITY));
        assert_eq!(lanes_put_until(&other).unwrap(), None);
        // a later lane with an earlier time does not lower the newest
        let timed = bash("sm put x").replace(
            r#"{"type""#,
            r#"{"timestamp":"2027-01-15T08:00:00Z","type""#,
        );
        write(
            d.path(),
            "s/subagents/c.jsonl",
            format!("{timed}\n").as_bytes(),
        );
        assert_eq!(lanes_put_until(&main).unwrap(), Some(f64::INFINITY));
    }

    /// The newest put that counts sets put_until; a put whose result is an error drops out.
    #[test]
    fn put_until_is_the_time_of_the_newest_put_that_counts() {
        let d = tmp();
        let put = |id: &str, ts: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"sm put x"}}}}]}}}}"#
            )
        };
        let failed = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"p2","content":"boom","is_error":true}]}}"#;
        let puts = format!(
            "{}\n{}\n",
            put("p1", "2027-01-15T08:00:00Z"),
            put("p2", "2027-01-15T09:00:00Z")
        );
        // 2027-01-15T08:00:00Z is epoch 1800000000
        let two = write(d.path(), "two.jsonl", puts.as_bytes());
        let until = |p: &PyPath| scan_transcript(p, 30000, None).unwrap().put_until;
        assert_eq!(until(&two), Some(1_800_003_600.0));
        let one = write(
            d.path(),
            "one.jsonl",
            format!("{puts}{failed}\n").as_bytes(),
        );
        assert_eq!(until(&one), Some(1_800_000_000.0));
    }

    // json.loads(bytes) under Python 3.15.1 (probe 2026-11-05) gives each right side
    #[test]
    fn loads_bytes_decodes_as_json_loads_does() {
        let text = |x: &str| PyValue::Str(x.into());
        let err = |b: &[u8]| loads_bytes(b).unwrap_err().python_text();
        // utf-8-sig: the BOM goes, and a bad byte counts from after it
        assert_eq!(loads_bytes(b"\xef\xbb\xbf\"a\"").unwrap(), text("a"));
        assert_eq!(
            err(b"\xef\xbb\xbf\"\xff\""),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 1: invalid start byte"
        );
        // surrogatepass: a CESU-8 surrogate decodes (to U+FFFD here, Python's lone \ud83d)
        assert_eq!(
            loads_bytes(b"\"x \xed\xa0\xbd y\"").unwrap(),
            text("x \u{fffd} y")
        );
        assert_eq!(
            err(b"\"\xed\xa0\xbd \xff\""),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 5: invalid start byte"
        );
        assert_eq!(
            err(b"\"\xed\xa0\""),
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xed in position 1: invalid continuation byte"
        );
        // a wide encoding with a partial unit left over
        assert_eq!(
            err(b"\xff\xfe{\x00a"),
            "UnicodeDecodeError: 'utf-16-le' codec can't decode byte 0x61 in position 4: truncated data"
        );
        assert_eq!(
            err(b"\x00\x00\xfe\xff\x00\x00\x00{\x00"),
            "UnicodeDecodeError: 'utf-32-be' codec can't decode byte 0x00 in position 8: truncated data"
        );
    }
}
