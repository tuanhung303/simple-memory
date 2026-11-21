//! Each pattern runs on the regex crate, which has no backtrack limit to hit and no lookaround.
//! Every lookaround of Python's pattern is a check in code, and each one is exact because it
//! looks at one char or a fixed-length text (argued at each pattern below):
//! - A lookbehind on one char class goes into the regex as `(?:\A|[^X])`; the core of each such
//!   pattern starts with a char of X, so the core start is the match start (`\A`) or one char on.
//!   Unlike Python, a literal escape `\n`, `\r` or `\t` before the core also passes (two chars on).
//! - `\b` before a word char (`\bBearer`, `\bsig=`, ...) is `(?<!\w)`, with Python's `\w` from
//!   `py::str`, and `\s` is Python's `\s`.
//! - re.IGNORECASE letters are sre's sets (`py::str::ci_chars`): `i` also matches U+0130 and
//!   U+0131, `k` U+212A, `s` U+017F. A pattern that matches less than Python's leaks a secret.
//! - key_value and the `<private>` tag are scanners: Python's backtracking over them is
//!   deterministic once the spans are fixed (see `key_value_find`), and a scan is linear where
//!   Python's search is quadratic on a long run of key chars.

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::core::item::{Item, compute_content_hash};
use crate::core::refs::{RefsArg, extract_refs, split_refs};
use crate::py::str::{S_INNER, W_INNER, ci_chars, ci_set, is_word, isspace};

/// Counts per kind, in the order Python's dict gets them.
pub type Counts = IndexMap<String, i64>;

pub const PRIVATE_MARK: &str = "[REDACTED:private]";

fn char_at(t: &str, i: usize) -> Option<char> {
    t[i..].chars().next()
}

/// Is there a literal escape `\n`, `\r` or `\t` (a backslash, then that letter) at `i`?
fn escape_at(t: &str, i: usize) -> bool {
    matches!(t.as_bytes().get(i..), Some([b'\\', b'n' | b'r' | b't', ..]))
}

/// The end of the run of chars that pass `pred` from `i`, and its length in chars.
fn run(t: &str, i: usize, pred: impl Fn(char) -> bool) -> (usize, usize) {
    let mut n = 0;
    for (off, c) in t[i..].char_indices() {
        if !pred(c) {
            return (i + off, n);
        }
        n += 1;
    }
    (t.len(), n)
}

/// An ASCII literal under re.IGNORECASE, one sre set per char.
fn ci_seq(lit: &str) -> Vec<Vec<char>> {
    lit.chars().map(ci_chars).collect()
}

/// Does `t` at `i` start with the IGNORECASE literal `seq`? Gives the end.
fn ci_at(t: &str, i: usize, seq: &[Vec<char>]) -> Option<usize> {
    let mut it = t[i..].char_indices();
    for set in seq {
        let (_, c) = it.next()?;
        if !set.contains(&c) {
            return None;
        }
    }
    Some(it.next().map_or(t.len(), |(off, _)| i + off))
}

/// A pattern `(?<![X])core`: the regex is `(?:\A|[^X])(?:core)`.
struct Guarded {
    re: Regex,
    /// X, the class of the lookbehind. The first char of every core match is in X.
    in_x: fn(char) -> bool,
    /// For a fixed-length core with a trailing `(?![Y])`: a char of Y.
    in_y: Option<fn(char) -> bool>,
}

fn alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn alnum_us_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl Guarded {
    fn new(
        x_inner: &str,
        core: &str,
        in_x: fn(char) -> bool,
        in_y: Option<fn(char) -> bool>,
    ) -> Guarded {
        // the letter of a literal escape (`\n`, `\r`, `\t` written as a backslash and a letter, as
        // in JSON or a shell string) is no char of X there: a token after it is one after a break
        let re = Regex::new(&format!("(?:\\A|[^{x_inner}]|\\\\[nrt])(?:{core})"))
            .expect("redact pattern compiles");
        Guarded { re, in_x, in_y }
    }

    /// The first match whose core starts at or after `pos`.
    fn find(&self, t: &str, pos: usize) -> Option<(usize, usize)> {
        // the char before `pos` is the lookbehind's char for a core at `pos`, so the regex may
        // consume it; a core cannot start before `pos`, since its own lookbehind char would sit
        // before the search start (`\A` matches only at 0)
        let mut from = t[..pos]
            .chars()
            .next_back()
            .map_or(0, |c| pos - c.len_utf8());
        loop {
            let m = self.re.find_at(t, from)?;
            let first = char_at(t, m.start())?;
            // no core starts with `n`, `r` or `t`, so a match that starts with an escape took it whole
            let s = if m.start() == 0 && (self.in_x)(first) {
                0
            } else if escape_at(t, m.start()) {
                m.start() + 2
            } else {
                m.start() + first.len_utf8()
            };
            let e = m.end();
            match (self.in_y, char_at(t, e)) {
                // a fixed-length core has no shorter match at `s`; Python goes on at `s` + 1, and a
                // core there would have the X char at `s` before it, so the search resumes past it
                (Some(in_y), Some(next)) if in_y(next) => from = s + char_at(t, s)?.len_utf8(),
                _ => return Some((s, e)),
            }
        }
    }
}

enum Matcher {
    Plain(Regex),
    Guarded(Guarded),
    KeyValue,
    QuerySignature(Regex),
}

impl Matcher {
    /// Every match, left to right, none overlapping.
    fn spans(&self, t: &str) -> Vec<(usize, usize)> {
        let find: Box<dyn Fn(usize) -> Option<(usize, usize)>> = match self {
            Matcher::Plain(re) => Box::new(|pos| re.find_at(t, pos).map(|m| (m.start(), m.end()))),
            Matcher::Guarded(g) => Box::new(|pos| g.find(t, pos)),
            Matcher::KeyValue => Box::new(|pos| key_value_find(t, pos)),
            Matcher::QuerySignature(re) => return query_signature_spans(re, t),
        };
        let mut out = Vec::new();
        let mut pos = 0;
        // no pattern matches empty, so the next search starts at the end of the last match
        while let Some((s, e)) = find(pos) {
            // a match that starts on the letter of a literal escape (a key run, a URL scheme) is
            // the match a search past the letter finds inside it, so the escape stays in the text
            // as a line break would; with none there it is kept as found
            let (s, e) = if s > 0 && escape_at(t, s - 1) {
                find(s + 1).filter(|&(s2, _)| s2 < e).unwrap_or((s, e))
            } else {
                (s, e)
            };
            out.push((s, e));
            pos = e;
        }
        out
    }
}

/// The patterns in Python's order. A trailing `(?![X])` after a greedy run of a class that holds
/// X always holds at the run's end and is left out (github_token, slack_token, sk_key, jwt);
/// after a fixed-length core it is `in_y` (aws_key, google_api_key). bearer's
/// `(?!\[REDACTED:)` is left out: `[` is not in the value class, so the value fails there anyway.
static SECRET_PATTERNS: LazyLock<Vec<(&'static str, Matcher)>> = LazyLock::new(|| {
    let w = &*W_INNER;
    let s_inner = &*S_INNER;
    let an = "A-Za-z0-9";
    let word = |core: &str| Matcher::Guarded(Guarded::new(w, core, is_word, None));
    let token_mark = "\\[REDACTED:(?:private|private_key|aws_key|github_token|slack_token|google_api_key|sk_key|jwt|bearer)\\]";
    vec![
        (
            "private_key",
            Matcher::Plain(
                Regex::new("(?s)-----BEGIN [A-Z0-9 _-]*PRIVATE KEY-----.*?-----END [A-Z0-9 _-]*PRIVATE KEY-----")
                    .expect("redact pattern compiles"),
            ),
        ),
        ("aws_key", Matcher::Guarded(Guarded::new(an, "AKIA[0-9A-Z]{16}", alnum, Some(alnum)))),
        (
            "github_token",
            Matcher::Guarded(Guarded::new(an, "gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{20,}", alnum, None)),
        ),
        ("slack_token", Matcher::Guarded(Guarded::new(an, "xox[abposr]-[A-Za-z0-9-]{10,}", alnum, None))),
        ("google_api_key", Matcher::Guarded(Guarded::new(an, "AIza[0-9A-Za-z_-]{35}", alnum, Some(alnum)))),
        (
            "sk_key",
            Matcher::Guarded(Guarded::new("A-Za-z0-9_\\-", "sk-(?:ant-|proj-)?[A-Za-z0-9_-]{32,}", alnum_us_dash, None)),
        ),
        (
            "jwt",
            Matcher::Guarded(Guarded::new(
                an,
                "eyJ[A-Za-z0-9_-]{5,}\\.eyJ[A-Za-z0-9_-]{5,}\\.[A-Za-z0-9_-]{5,}",
                alnum,
                None,
            )),
        ),
        ("bearer", word(&format!("Bearer[{s_inner}]+[A-Za-z0-9._~+/=-]{{8,}}"))),
        // `scheme://user:password@`, the user may be empty. A `/` in the password is taken only
        // where the text cannot be `host:port/path`: the user holds no `.`, the password holds no
        // `:`, and its text before the first `/` is not empty and not all digits. No guard before
        // the scheme: in `DB_postgres://` or `2redis://` the scheme starts after the word char,
        // and all after `://` is the same from any start in the scheme. The marker of a pattern
        // above (or of a private block) stands for a token, so in the user and the password it
        // counts as one char that is no `:`, `.` or digit. A token is never a host, so after a
        // user that holds a marker all up to `@` is the password, whatever it starts with or
        // holds. Every class here leaves out `@`, so a match still ends at the first `@` after
        // `://`
        (
            "url_credentials",
            Matcher::Plain(
                Regex::new(&format!(
                    "[A-Za-z][A-Za-z0-9+.-]*://(?:{u}*:[^{s_inner}/@\"']+@|(?:[^{s_inner}/:@\"'.]|{token_mark})*:{u}*(?:[^{s_inner}/:@\"'0-9]|{token_mark}){u}*(?:/{u}*)+@|{u}*{token_mark}{u}*:[^{s_inner}@\"']+@)",
                    u = format!("(?:[^{s_inner}/:@\"']|{token_mark})"),
                ))
                .expect("redact pattern compiles"),
            ),
        ),
        ("azure_sas", word("sig=[A-Za-z0-9%+/=_-]{16,}")),
        // before key_value, whose `Token=` value would run to the end of the URL
        (
            "signed_url",
            word("(?i:X-(?:Amz|Goog)-(?:Signature|Security-Token))=[A-Za-z0-9%+/=_-]{16,}"),
        ),
        (
            "signed_url",
            Matcher::QuerySignature(
                Regex::new("Signature=[A-Za-z0-9%+/=_-]{16,}").expect("redact pattern compiles"),
            ),
        ),
        ("azure_key", word("(?:AccountKey|SharedAccessKey)=[A-Za-z0-9+/=]{16,}")),
        ("key_value", Matcher::KeyValue),
    ]
});

// ---------------------------------------------------------------- key_value

/// `[A-Za-z0-9_.-]` under re.IGNORECASE (adds U+0130, U+0131, U+017F, U+212A).
static KEY_SET: LazyLock<Vec<char>> = LazyLock::new(|| {
    ci_set(
        ('A'..='Z')
            .chain('a'..='z')
            .chain('0'..='9')
            .chain(['_', '.', '-']),
    )
});

fn key_char(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
    } else {
        KEY_SET.binary_search(&c).is_ok()
    }
}

/// The AWS secret access key names; they also take `kv_aws_secret`.
const ACCESS_KEY_WORDS: [&str; 2] = ["secret_access_key", "secretaccesskey"];

/// `(?:password|...|apikey)(?:_key|key|_value|s)?` as IGNORECASE char sets, every word with every
/// suffix (and none).
static KEY_TAILS: LazyLock<Vec<Vec<Vec<char>>>> = LazyLock::new(|| {
    let words = ["password", "passwd", "secret", "token", "api_key", "apikey"];
    let suffixes = ["", "_key", "key", "_value", "s"];
    words
        .iter()
        .chain(&ACCESS_KEY_WORDS)
        .flat_map(|w| suffixes.iter().map(move |s| ci_seq(&format!("{w}{s}"))))
        .collect()
});

static ACCESS_KEY_TAILS: LazyLock<Vec<Vec<Vec<char>>>> =
    LazyLock::new(|| ACCESS_KEY_WORDS.iter().map(|w| ci_seq(w)).collect());

/// `\[REDACTED:` under re.IGNORECASE.
static REDACTED_SEQ: LazyLock<Vec<Vec<char>>> = LazyLock::new(|| ci_seq("[REDACTED:"));

/// `_V`: `[^\s"'`\],;}{]` (no char in it has a case).
fn is_v(c: char) -> bool {
    !isspace(c) && !matches!(c, '"' | '\'' | '`' | ']' | ',' | ';' | '}' | '{')
}

/// `_BARE_V`: `_V` without `:` and `/`.
fn is_bare_v(c: char) -> bool {
    is_v(c) && c != ':' && c != '/'
}

fn skip_space(t: &str, i: usize) -> usize {
    run(t, i, isspace).0
}

/// Does the key run end with one of `tails`?
fn ends_with_any(seg: &str, tails: &[Vec<Vec<char>>]) -> bool {
    let longest = tails.iter().map(Vec::len).max().unwrap_or(0);
    let tail: Vec<char> = seg.chars().rev().take(longest).collect();
    tails.iter().any(|seq| {
        seq.len() <= tail.len() && seq.iter().rev().zip(&tail).all(|(set, c)| set.contains(c))
    })
}

/// After an access key name's `:`, at `i`: a bare 40-char value of the AWS secret alphabet, which
/// `kv_bare` refuses when it holds a `/`, followed by no V char or by a literal escape (a line
/// break there). The end of the match.
fn kv_aws_secret(t: &str, i: usize) -> Option<usize> {
    let w = run(t, i, |c| c == ' ' || c == '\t').0;
    let (end, n) = run(t, w, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '+' | '=')
    });
    (n == 40 && (escape_at(t, end) || !char_at(t, end).is_some_and(is_v))).then_some(end)
}

/// Does the value at `w` start with a marker an earlier pattern left, with more of the value
/// after its `]`? The V run from `w` ends at that `]` (V holds no `]`), so it reads the marker only.
fn marker_then_more(t: &str, w: usize) -> bool {
    let close = run(t, w, is_v).0;
    t[w..].starts_with("[REDACTED:")
        && t.as_bytes().get(close) == Some(&b']')
        && char_at(t, close + 1).is_some_and(is_v)
}

/// The end of the marker an earlier pattern left at `i`, `[REDACTED:<kind>]`.
fn marker_end(t: &str, i: usize) -> Option<usize> {
    let k = i + "[REDACTED:".len();
    if !t[i..].starts_with("[REDACTED:") {
        return None;
    }
    let (e, n) = run(t, k, |c| c.is_ascii_lowercase() || c == '_');
    (n > 0 && t.as_bytes().get(e) == Some(&b']')).then_some(e + 1)
}

/// The end of the value at `w` whose first V run ends at `end`: it goes on past each marker an
/// earlier pattern left in it (V holds no `]`), so the marker and the rest of the value go with it.
fn past_markers(t: &str, w: usize, mut end: usize) -> usize {
    let mut from = w;
    while t.as_bytes().get(end) == Some(&b']') && t[from..end].contains("[REDACTED:") {
        from = end + 1;
        end = run(t, from, is_v).0;
    }
    end
}

/// `(?!\[REDACTED:)(?![0-9]+(?!V))V{8,}["']?` at `w`: the end of the match. A value that starts
/// with a marker is taken only when more of the value follows it.
fn kv_value(t: &str, w: usize) -> Option<usize> {
    if ci_at(t, w, &REDACTED_SEQ).is_some() && !marker_then_more(t, w) {
        return None;
    }
    // `[0-9]+(?!V)`: digits are V, so only the longest digit run can end before a non-V char
    let (d_end, digits) = run(t, w, |c| c.is_ascii_digit());
    if digits > 0 && !char_at(t, d_end).is_some_and(is_v) {
        return None;
    }
    let (end, n) = run(t, w, is_v);
    if n < 8 {
        return None;
    }
    let end = past_markers(t, w, end);
    // V holds no quote, so the greedy run is followed by the optional quote or by nothing
    Some(end + usize::from(matches!(t.as_bytes().get(end), Some(b'"' | b'\''))))
}

/// `_BARE_COLON` after its `:`, at `i`: the end of the match.
fn kv_bare(t: &str, i: usize) -> Option<usize> {
    let b = t.as_bytes();
    // a shorter `[ \t]*`, or no backtick, leaves a space or a backtick where a BV char must be
    let mut w = run(t, i, |c| c == ' ' || c == '\t').0;
    if b.get(w) == Some(&b'`') {
        w += 1;
    }
    // a marker that a literal escape or no value char ends is no value, as a line break after it
    // would end it, and so are literal escapes before it: else a second pass takes
    // `key:\n[REDACTED:...]` as a new value
    let mut m = w;
    while escape_at(t, m) {
        m += 2;
    }
    if marker_end(t, m).is_some_and(|e| escape_at(t, e) || !char_at(t, e).is_some_and(is_v)) {
        return None;
    }
    // a value that starts with a marker is a token: it goes on over `:` and `/` as `kv_value` does
    if ci_at(t, w, &REDACTED_SEQ).is_some() {
        let close = run(t, w, is_v).0;
        return marker_then_more(t, w).then(|| past_markers(t, w, close));
    }
    // `(?!m_[0-9A-Z]{26}(?!V))`, case-sensitive
    if b[w..].starts_with(b"m_")
        && b.len() >= w + 28
        && b[w + 2..w + 28]
            .iter()
            .all(|x| x.is_ascii_digit() || x.is_ascii_uppercase())
        && !char_at(t, w + 28).is_some_and(is_v)
    {
        return None;
    }
    // `(?=BV*[0-9])` and the upper and lower ones: digits and ASCII letters are BV, so each
    // holds when the BV run from `w` has one. A marker an earlier pattern left in the run is part
    // of it, as the token it stands for was, and holds all three, as a token does
    let (mut digit, mut upper, mut lower, mut n) = (false, false, false, 0usize);
    let mut end = w;
    let mut first_escape = true;
    while let Some(c) = char_at(t, end) {
        if let Some(m) = marker_end(t, end) {
            (digit, upper, lower) = (true, true, true);
            n += m - end;
            end = m;
            continue;
        }
        // the first literal escape (`\n`, `\r`, `\t` as a backslash and a letter) after the value
        // starts is a line break here when the text before it is a value: the value ends there;
        // else it is read as before
        if escape_at(t, end)
            && n > 0
            && std::mem::take(&mut first_escape)
            && n >= 12
            && digit
            && upper
            && lower
        {
            break;
        }
        if !is_bare_v(c) {
            break;
        }
        digit |= c.is_ascii_digit();
        upper |= c.is_ascii_uppercase();
        lower |= c.is_ascii_lowercase();
        n += 1;
        end += c.len_utf8();
    }
    // `BV{12,}(?!V|:)`: a shorter run is followed by a BV char, which is V, so only the longest
    // run can pass, and a char after it that is not BV is V only when it is `:` or `/`
    if n < 12 || !(digit && upper && lower) || matches!(b.get(end), Some(b':' | b'/')) {
        return None;
    }
    Some(end)
}

/// The value part after the key run that ends at `e`: the end of the match. Each choice here is
/// forced: a quote after the key is taken (else no `:` or `=` follows), and `\s*` is greedy
/// (else a space stands where `:` or `=` must be).
fn kv_after_key(t: &str, e: usize, access_key: bool) -> Option<usize> {
    let b = t.as_bytes();
    let q = e + usize::from(matches!(b.get(e), Some(b'"' | b'\'')));
    let v = skip_space(t, q);
    match b.get(v) {
        Some(b':') => {
            let w = skip_space(t, v + 1);
            let quoted = if matches!(b.get(w), Some(b'"' | b'\'')) {
                kv_value(t, w + 1)
            } else {
                None
            };
            quoted
                .or_else(|| kv_bare(t, v + 1))
                .or_else(|| access_key.then(|| kv_aws_secret(t, v + 1)).flatten())
        }
        Some(b'=') => {
            let w = skip_space(t, v + 1);
            kv_value(
                t,
                w + usize::from(matches!(b.get(w), Some(b'"' | b'\'' | b'`'))),
            )
        }
        _ => None,
    }
}

/// The first key_value match from `pos`.
///
/// `_KEY` is `["']?K*(?:word)(?:suffix)?(?!K)["']?` with K the key class, and word and suffix
/// are K chars, so from a start the key covers the K run through its end `e` whatever split K*
/// takes, and all that follows depends on `e` alone. So a run matches from its first char when
/// it ends with a word and suffix and the value part matches after `e`, from a quote just before
/// it the same, and from any later char of the run never. The scan visits each run once.
fn key_value_find(t: &str, pos: usize) -> Option<(usize, usize)> {
    let b = t.as_bytes();
    let mut i = pos;
    while i < t.len() {
        let (r0, _) = run(t, i, |c| !key_char(c));
        if r0 == t.len() {
            return None;
        }
        let (e, _) = run(t, r0, key_char);
        let seg = &t[r0..e];
        if ends_with_any(seg, &KEY_TAILS)
            && let Some(end) = kv_after_key(t, e, ends_with_any(seg, &ACCESS_KEY_TAILS))
        {
            let start = if r0 > pos && matches!(b[r0 - 1], b'"' | b'\'') {
                r0 - 1
            } else {
                r0
            };
            return Some((start, end));
        }
        i = e;
    }
    None
}

/// The end of the text a match that ends at `end` goes on through: past the pair of a key word
/// that ends its value and has its own separator and value (`TOKEN=` newline `password: value`),
/// and along a chain of such pairs. The key word is the run of key chars before `end`, past one
/// closing quote and then one `:` or `=`. Its value starts after `end`, so each step moves `end`
/// on and reads back over the last value only.
fn through_pairs(t: &str, mut end: usize) -> usize {
    let b = t.as_bytes();
    loop {
        let mut k = end;
        k -= usize::from(matches!(b[..k].last(), Some(b'"' | b'\'')));
        k -= usize::from(matches!(b[..k].last(), Some(b':' | b'=')));
        let seg = &t[t[..k].trim_end_matches(key_char).len()..k];
        if !ends_with_any(seg, &KEY_TAILS) {
            return end;
        }
        match kv_after_key(t, k, ends_with_any(seg, &ACCESS_KEY_TAILS)) {
            Some(e) => end = e,
            None => return end,
        }
    }
}

/// `[?&]Signature=value` in a URL that also carries `AWSAccessKeyId=` or `GoogleAccessId=` (S3 and
/// GCS query-string signing): each span from `Signature`. The URL is the run of non-space chars;
/// each run is read once, so the cost is linear in the text.
fn query_signature_spans(re: &Regex, t: &str) -> Vec<(usize, usize)> {
    signature_spans_with(t, |url, from| {
        re.find_at(url, from).map(|m| (m.start(), m.end()))
    })
}

/// `query_signature_spans` with `find(url, from)`, the first `Signature=value` in `url` at or
/// after `from`, as its search.
fn signature_spans_with(
    t: &str,
    mut find: impl FnMut(&str, usize) -> Option<(usize, usize)>,
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let start = run(t, i, isspace).0;
        let end = run(t, start, |c| !isspace(c)).0;
        let url = &t[start..end];
        if url.contains("AWSAccessKeyId=") || url.contains("GoogleAccessId=") {
            let mut from = 0;
            while let Some((ms, me)) = find(url, from) {
                if matches!(url[..ms].bytes().next_back(), Some(b'?' | b'&')) {
                    out.push((start + ms, start + me));
                }
                // after a refused match too: one that starts inside it, or at its end, needs a
                // `?` or `&` just before it, and every byte of it is `Signature=` or the value
                // class, which hold neither
                from = me;
            }
        }
        i = end;
    }
    out
}

// ---------------------------------------------------------------- private blocks

/// `private` under re.IGNORECASE.
static PRIVATE_SEQ: LazyLock<Vec<Vec<char>>> = LazyLock::new(|| ci_seq("private"));

/// One `<private>` tag: its byte span and whether it closes.
struct Tag {
    start: usize,
    end: usize,
    close: bool,
}

/// `_PRIVATE_TAG_RE.finditer`: `<(/?)private\b[^>]*>` under re.IGNORECASE. `\b` after `e` is
/// "no word char next", and `[^>]*>` ends at the first `>`.
fn private_tags(text: &str) -> Vec<Tag> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = text[pos..].find('<') {
        let start = pos + off;
        pos = start + 1;
        let close = b.get(pos) == Some(&b'/');
        let Some(k) = ci_at(text, pos + usize::from(close), &PRIVATE_SEQ) else {
            continue;
        };
        if char_at(text, k).is_some_and(is_word) {
            continue;
        }
        // no `>` after this tag means none after any later `<` either
        let Some(gt) = text[k..].find('>') else { break };
        let end = k + gt + 1;
        out.push(Tag { start, end, close });
        pos = end;
    }
    out
}

/// The span of each `<private>` block through the close tag that balances it.
fn private_spans(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let tags = private_tags(text);
    let mut i = 0;
    while i < tags.len() {
        if tags[i].close {
            i += 1;
            continue;
        }
        let mut depth: i64 = 0;
        let mut end_idx = None;
        let mut last_close = None;
        for (j, tag) in tags.iter().enumerate().skip(i) {
            depth += if tag.close { -1 } else { 1 };
            if tag.close {
                last_close = Some(j);
            }
            if depth == 0 {
                end_idx = Some(j);
                break;
            }
        }
        let Some(end_idx) = end_idx.or(last_close) else {
            break;
        };
        out.push((tags[i].start, tags[end_idx].end));
        i = end_idx + 1;
    }
    out
}

/// `text` with each span, left to right and none overlapping, replaced by `repl`.
fn replace_spans(text: &str, spans: &[(usize, usize)], repl: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for &(s, e) in spans {
        out.push_str(&text[last..s]);
        out.push_str(repl);
        last = e;
    }
    out.push_str(&text[last..]);
    out
}

/// Where the text at `p` goes when each span is replaced by `m` bytes, with `cut[i]` the length
/// of the first `i` spans. A position inside a span goes to the end of its replacement.
fn after_replace(p: usize, spans: &[(usize, usize)], cut: &[usize], m: usize) -> usize {
    let i = spans.partition_point(|&(_, e)| e <= p);
    let p = match spans.get(i) {
        Some(&(s, _)) if s < p => s + m,
        _ => p,
    };
    p - cut[i] + i * m
}

/// The passes of `redact_text`: `_strip_private` (each `<private>` block through the close tag
/// that balances it), then each pattern's `subn` on the text the passes before it left, each
/// span replaced by its kind's marker. Then the marker of each bearer or key_value match takes
/// in the pairs after it (`through_pairs`, read in the text that pass matched), with each marker
/// a later pass put there: this pass removes the text after the marker, so each pass before it
/// matches as it would without it. Last, a key_value pass keeps each match that holds a marker
/// that took text in: a key whose value that text refused (a bare value with a `:` after it)
/// takes its value, as a second pass would. `each(kind, text, spans)` sees each pass before it
/// replaces, the pass that takes text in with the kind "". The redacted text.
fn redact_passes(text: &str, mut each: impl FnMut(&str, &str, &[(usize, usize)])) -> String {
    let spans = private_spans(text);
    each("private", text, &spans);
    let mut out = replace_spans(text, &spans, PRIVATE_MARK);
    // (end of a marker, end of the pairs it takes in), in `out`
    let mut through: Vec<(usize, usize)> = Vec::new();
    for (kind, pat) in SECRET_PATTERNS.iter() {
        let spans = pat.spans(&out);
        each(kind, &out, &spans);
        if matches!(*kind, "bearer" | "key_value") {
            // a match that ends inside the text an earlier one takes in is taken in with it: a
            // second read of that text would make a chain of N pairs cost N^2
            let mut reach = 0;
            for &(_, e) in &spans {
                if e > reach {
                    reach = through_pairs(&out, e);
                    through.push((e, reach));
                }
            }
        }
        let mark = format!("[REDACTED:{kind}]");
        let cut: Vec<usize> = std::iter::once(0)
            .chain(spans.iter().scan(0, |n, &(s, e)| {
                *n += e - s;
                Some(*n)
            }))
            .collect();
        for (e, end) in &mut through {
            (*e, *end) = (
                after_replace(*e, &spans, &cut, mark.len()),
                after_replace(*end, &spans, &cut, mark.len()),
            );
        }
        out = replace_spans(&out, &spans, &mark);
    }
    through.sort_unstable();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (e, end) in through {
        match taken.last_mut() {
            Some(last) if e <= last.1 => last.1 = last.1.max(end),
            _ if end > e => taken.push((e, end)),
            _ => {}
        }
    }
    // with no marker that took text in, the last pass keeps no match
    if taken.is_empty() {
        return out;
    }
    each("", &out, &taken);
    let out = replace_spans(&out, &taken, "");
    // the end of each marker that took text in, in `out`
    let mut cut = 0;
    let ends: Vec<usize> = taken
        .iter()
        .map(|&(s, e)| {
            let p = s - cut;
            cut += e - s;
            p
        })
        .collect();
    let spans: Vec<(usize, usize)> = Matcher::KeyValue
        .spans(&out)
        .into_iter()
        .filter(|&(s, e)| {
            let i = ends.partition_point(|&p| p <= s);
            ends.get(i).is_some_and(|&p| p <= e)
        })
        .collect();
    each("key_value", &out, &spans);
    replace_spans(&out, &spans, "[REDACTED:key_value]")
}

/// `redact_text(text, counts, removed)`.
pub fn redact_text(
    text: &str,
    mut counts: Option<&mut Counts>,
    mut removed: Option<&mut Vec<String>>,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    redact_passes(text, |kind, t, spans| {
        if kind == "private"
            && let Some(r) = removed.as_deref_mut()
        {
            r.extend(spans.iter().map(|&(s, e)| t[s..e].to_string()));
        }
        if !spans.is_empty()
            && !kind.is_empty()
            && let Some(c) = counts.as_deref_mut()
        {
            *c.entry(kind.into()).or_insert(0) += spans.len() as i64;
        }
    })
}

/// For each byte of `text`, does `redact_text` replace it? Every byte of a pass's output keeps
/// the byte of `text` it came from (none inside a marker).
fn redacted_bytes(text: &str) -> Vec<bool> {
    let mut hit = vec![false; text.len()];
    let mut origin: Vec<Option<usize>> = (0..text.len()).map(Some).collect();
    redact_passes(text, |kind, _, spans| {
        let mark = if kind.is_empty() {
            0
        } else {
            "[REDACTED:".len() + kind.len() + 1
        };
        let mut next = Vec::with_capacity(origin.len());
        let mut last = 0;
        for &(s, e) in spans {
            next.extend_from_slice(&origin[last..s]);
            origin[s..e].iter().flatten().for_each(|&o| hit[o] = true);
            next.extend(std::iter::repeat_n(None, mark));
            last = e;
        }
        next.extend_from_slice(&origin[last..]);
        origin = next;
    });
    hit
}

/// The refs in `refs` that a match of `redact_text` over the whole string covers or touches, so
/// a secret written with spaces in it (`password: value`) goes with each ref it spans.
fn refs_redaction_touches(refs: &str) -> HashSet<&str> {
    let hit = redacted_bytes(refs);
    REF_TOKEN_RE
        .find_iter(refs)
        .filter(|m| hit[m.range()].contains(&true))
        .map(|m| m.as_str())
        .collect()
}

/// The refs that a match covers or touches in each raw refs string, read as its writer gave it
/// and, when that differs, with its refs joined by one space, so a pair that only a comma or
/// another separator parts (`password:,value`) is read as a pair. A write path drops them by
/// value from the refs it builds: its split dedupes, and a dedupe can take the key word of a
/// second `key: value` pair from its value.
pub fn refs_a_match_touches<'a>(raws: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    let mut touched = HashSet::new();
    for raw in raws {
        touched.extend(refs_redaction_touches(raw).into_iter().map(String::from));
        let spaced = REF_TOKEN_RE
            .find_iter(raw)
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if spaced != raw {
            touched.extend(
                refs_redaction_touches(&spaced)
                    .into_iter()
                    .map(String::from),
            );
        }
    }
    touched
}

/// A run of chars `split_refs` keeps as one ref: it splits on `\s` and `,`.
static REF_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("[^{},]+", *S_INNER)).expect("ref token pattern"));

/// `redact_for_store(item, counts)`: title, body, evidence, tags and source redacted, and refs
/// dropped that are secret-shaped or that the text gave only in a part redaction removed (a
/// private block, or a secret an importer took a path ref from before redaction).
pub fn redact_for_store(item: &Item, mut counts: Option<&mut Counts>) -> Item {
    let mut removed: Vec<String> = Vec::new();
    let title = redact_text(&item.title, counts.as_deref_mut(), Some(&mut removed));
    let body = redact_text(&item.body, counts.as_deref_mut(), Some(&mut removed));
    let evidence = redact_text(&item.evidence, counts.as_deref_mut(), Some(&mut removed));
    let tags = redact_text(&item.tags, counts.as_deref_mut(), None);
    let source = redact_text(&item.source, counts, None);
    let mut redacted_refs = HashSet::new();
    if title != item.title || body != item.body || evidence != item.evidence {
        let in_kept: HashSet<String> = extract_refs(Some(&format!("{title}\n{body}\n{evidence}")))
            .into_iter()
            .collect();
        let raw_text = format!("{}\n{}\n{}", item.title, item.body, item.evidence);
        redacted_refs = extract_refs(Some(&raw_text))
            .into_iter()
            .chain(extract_refs(Some(&removed.join("\n"))))
            .filter(|r| !in_kept.contains(r))
            .collect();
    }
    let all_refs = split_refs(Some(&RefsArg::Str(&item.refs)));
    // read as written and single-spaced, as the write paths read: a pair that a comma or a
    // line break parts (`password:,value`) is still a pair
    let touched = refs_a_match_touches([item.refs.as_str()]);
    let kept: Vec<&String> = all_refs
        .iter()
        .filter(|r| {
            !redacted_refs.contains(*r)
                && !touched.contains(r.as_str())
                && redact_text(r, None, None) == **r
        })
        .collect();
    let refs = if kept.len() != all_refs.len() {
        kept.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        item.refs.clone()
    };
    if title == item.title
        && body == item.body
        && evidence == item.evidence
        && refs == item.refs
        && tags == item.tags
        && source == item.source
    {
        return item.clone();
    }
    let content_hash = if body != item.body {
        compute_content_hash(&body)
    } else {
        item.content_hash.clone()
    };
    // dataclasses.replace runs __init__ again, so __post_init__ runs on the new fields
    Item {
        title,
        body,
        evidence,
        refs,
        tags,
        source,
        content_hash,
        ..item.clone()
    }
    .post_init()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made-up values in the shape each kind has; none is a real credential or id.
    fn fake_sk() -> String {
        format!("sk-ant-api03-{}", "Abc123xyz_".repeat(4))
    }
    fn fake_gh() -> String {
        format!("ghp_{}", "a1B2c3D4e5".repeat(4))
    }
    const FAKE_ULID_ID: &str = "m_01ABCDEFGHJKMNPQRSTVWXYZ00";
    const FAKE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn counted(text: &str) -> (String, Counts) {
        let mut c = Counts::new();
        let out = redact_text(text, Some(&mut c), None);
        (out, c)
    }

    fn counts(pairs: &[(&str, i64)]) -> Counts {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn replaces_keys_and_private_blocks() {
        let (out, c) = counted(&format!(
            "key {} and {}; <private>client figure 42</private> end",
            fake_sk(),
            fake_gh()
        ));
        assert_eq!(
            out,
            "key [REDACTED:sk_key] and [REDACTED:github_token]; [REDACTED:private] end"
        );
        assert_eq!(
            c,
            counts(&[("private", 1), ("sk_key", 1), ("github_token", 1)])
        );
        // dict order: private first, then the pattern order
        assert_eq!(
            c.keys().map(String::as_str).collect::<Vec<_>>(),
            ["private", "github_token", "sk_key"]
        );
    }

    #[test]
    fn leaves_ids_shas_task_ids_and_refs() {
        let text = format!(
            "see {FAKE_ULID_ID} at {FAKE_SHA} and 1a2b3c4; task:20261101-090000-aabbccdd-redact-test-1a2b; \
             refs R5 D9 src/simple_memory/store/items.py:120 tokens=4 sk-short"
        );
        assert_eq!(counted(&text), (text.clone(), Counts::new()));
    }

    #[test]
    fn private_blocks_nested_unclosed_and_case() {
        let r = |t: &str| redact_text(t, None, None);
        assert_eq!(
            r("a <PRIVATE note='x'>b</Private> c"),
            "a [REDACTED:private] c"
        );
        assert_eq!(
            r("a <private>b<private>c</private>d</private> e"),
            "a [REDACTED:private] e"
        );
        assert_eq!(
            r("a <private>b<private>c</private>d"),
            "a [REDACTED:private]d"
        );
        assert_eq!(r("explain the <private> tag"), "explain the <private> tag");
        assert_eq!(r("a </private> b"), "a </private> b");
        assert_eq!(r(""), "");
        // Python's re.I: U+0130 and U+0131 match i; `private\b` needs a non-word char after
        assert_eq!(r("a <PRİVATE>b</prıvate> c"), "a [REDACTED:private] c");
        assert_eq!(
            r("a <privates>b</privates> c"),
            "a <privates>b</privates> c"
        );
        let mut removed = Vec::new();
        redact_text(
            "x <private>p1</private> y <private>p2</private>",
            None,
            Some(&mut removed),
        );
        assert_eq!(removed, ["<private>p1</private>", "<private>p2</private>"]);
    }

    #[test]
    fn colon_sas_and_connection_string_forms() {
        let (out, c) = counted(
            "password: Hunter2Secret99\napi_key: AbCdEf1234567890xyz\nAZURE_CLIENT_SECRET: Abc8Q~def.Ghij-Klmn_Opqr12\n",
        );
        assert_eq!(
            out,
            "[REDACTED:key_value]\n[REDACTED:key_value]\n[REDACTED:key_value]\n"
        );
        assert_eq!(c, counts(&[("key_value", 3)]));
        assert_eq!(
            redact_text(
                "use `password: Hunter2Secret99` or `token=AbCdEf1234567890xyz`",
                None,
                None
            ),
            "use `[REDACTED:key_value]` or `[REDACTED:key_value]`"
        );
        assert_eq!(
            redact_text(
                "https://acct.blob.core.windows.net/c/f.csv?sv=2022-11-02&sp=r&sig=AbCdEfGh1234%2BijKlMnOp%3D",
                None,
                None
            ),
            "https://acct.blob.core.windows.net/c/f.csv?sv=2022-11-02&sp=r&[REDACTED:azure_sas]"
        );
        assert_eq!(
            redact_text(
                "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=AbCdEf1234567890ghIJ+/kl==;EndpointSuffix=core.windows.net",
                None,
                None
            ),
            "DefaultEndpointsProtocol=https;AccountName=acct;[REDACTED:azure_key];EndpointSuffix=core.windows.net"
        );
        assert_eq!(
            redact_text(
                "Endpoint=sb://ns.example.invalid/;SharedAccessKeyName=Root;SharedAccessKey=AbCdEf1234567890ghIJ+/kl==",
                None,
                None
            ),
            "Endpoint=sb://ns.example.invalid/;SharedAccessKeyName=Root;[REDACTED:azure_key]"
        );
    }

    #[test]
    fn aws_secret_access_key_assignments() {
        let r = |t: &str| redact_text(t, None, None);
        let key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            r(&format!("AWS_SECRET_ACCESS_KEY={key}")),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            r(&format!("[default]\naws_secret_access_key = {key}\n")),
            "[default]\n[REDACTED:key_value]\n"
        );
        assert_eq!(
            r(&format!("export AWS_SECRET_ACCESS_KEY=\"{key}\"")),
            "export [REDACTED:key_value]"
        );
        // prose, a short value and a key name alone stay
        for t in [
            "rotate the secret access key monthly",
            "secret_access_key=short",
            "set aws_secret_access_key in the vault",
            "access_key=Zx9Qw8Er7Ty6Uu5Ii4",
        ] {
            assert_eq!(r(t), t);
        }
    }

    #[test]
    fn aws_secret_access_key_colon_and_json_forms() {
        let r = |t: &str| redact_text(t, None, None);
        let key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            r(&format!("aws_secret_access_key: {key}\nregion: x")),
            "[REDACTED:key_value]\nregion: x"
        );
        assert_eq!(
            r(&format!("AWS_SECRET_ACCESS_KEY: {key} done")),
            "[REDACTED:key_value] done"
        );
        assert_eq!(
            r(&format!(
                "{{\"AccessKeyId\": \"x\", \"SecretAccessKey\": \"{key}\", \"Expiration\": \"y\"}}"
            )),
            "{\"AccessKeyId\": \"x\", [REDACTED:key_value], \"Expiration\": \"y\"}"
        );
        // a 40-char path stays after other key names, and a path or a longer name after this one
        let path = "Kv2026/Prod/AppConfig/Region/Ab12Cd34EfG";
        for t in [
            format!("secrets: {path}"),
            "see docs/secret_access_key.md for details".to_string(),
            "the field secret_access_key_id_rotation_policy_name is documented".to_string(),
            "aws_secret_access_key: ~/.aws/credentials".to_string(),
            "the column api_key_access_key=customer_facing_value_name is a label".to_string(),
        ] {
            assert_eq!(r(&t), t);
        }
        assert_eq!(
            r(&format!("aws_secret_access_key: {path}")),
            "[REDACTED:key_value]"
        );
    }

    #[test]
    fn query_string_signed_urls_with_an_access_id() {
        let r = |t: &str| redact_text(t, None, None);
        assert_eq!(
            r(
                "https://b.s3.example.invalid/f?AWSAccessKeyId=AKIAABCDEFGHIJKLMNOP&Expires=1700000000&Signature=AbCdEf1234567890%2BgHiJ%3D"
            ),
            "https://b.s3.example.invalid/f?AWSAccessKeyId=[REDACTED:aws_key]&Expires=1700000000&[REDACTED:signed_url]"
        );
        assert_eq!(
            r(
                "get https://storage.example.invalid/b/o?Signature=BxNQtk4lyAbCdEf%2Fabc%3D&GoogleAccessId=svc@p.example.invalid&Expires=1 now"
            ),
            "get https://storage.example.invalid/b/o?[REDACTED:signed_url]&GoogleAccessId=svc@p.example.invalid&Expires=1 now"
        );
        let hex = "0123456789abcdef".repeat(2);
        assert_eq!(
            r(&format!(
                "https://b.s3.example.invalid/f?x-amz-signature={hex}"
            )),
            "https://b.s3.example.invalid/f?[REDACTED:signed_url]"
        );
        // no access id in the same URL, or the id in another word: Signature stays
        for t in [
            "https://cb.example.invalid/r?Signature=AbCdEf1234567890abcd&state=1",
            "AWSAccessKeyId=x then https://cb.example.invalid/r?Signature=AbCdEf1234567890abcd",
        ] {
            assert_eq!(r(t), t);
        }
    }

    #[test]
    fn many_query_signatures_in_one_run_take_linear_time() {
        // 8000 signatures in one 248 KB run with no space; a scan of the run per match took
        // over 20 s here in a debug build
        let sigs = "&Signature=AAAAAAAAAAAAAAAAAAAA".repeat(8000);
        let started = std::time::Instant::now();
        assert_eq!(
            redact_text(&format!("x{sigs}"), None, None),
            format!("x{sigs}")
        );
        assert_eq!(
            redact_text(&format!("h?AWSAccessKeyId=x{sigs}"), None, None),
            format!(
                "h?AWSAccessKeyId=x{}",
                "&[REDACTED:signed_url]".repeat(8000)
            )
        );
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
    }

    fn signature_re() -> &'static Regex {
        SECRET_PATTERNS
            .iter()
            .find_map(|(_, m)| match m {
                Matcher::QuerySignature(re) => Some(re),
                _ => None,
            })
            .expect("a query signature pattern")
    }

    /// Spans from Python 3.15.1 `(?<=[?&])Signature=[A-Za-z0-9%+/=_-]{16,}` finditer on each run
    /// (each run holds an access id): after `?`, after `&`, refused after `=` and after `x`, two
    /// in one run, and one that ends the text.
    #[test]
    fn query_signature_spans_after_a_separator_only() {
        let cases: [(&str, &[(usize, usize)]); 5] = [
            (
                "u?Signature=AbCdEf1234567890abcd&AWSAccessKeyId=k",
                &[(2, 32)],
            ),
            (
                "u?AWSAccessKeyId=k&Signature=AbCdEf1234567890abcd&e=1",
                &[(19, 49)],
            ),
            (
                "u?AWSAccessKeyId=Signature=AbCdEf1234567890abcd&xSignature=AbCdEf1234567890abcd",
                &[],
            ),
            (
                "u?AWSAccessKeyId=k&Signature=AAAAAAAAAAAAAAAAAAAA&Signature=BBBBBBBBBBBBBBBBBBBB",
                &[(19, 49), (50, 80)],
            ),
            (
                "see u?GoogleAccessId=k&Signature=AbCdEf1234567890abcd",
                &[(23, 53)],
            ),
        ];
        for (t, want) in cases {
            assert_eq!(query_signature_spans(signature_re(), t), want, "{t}");
        }
    }

    /// The bytes the searches read, at least: from each search start to its match end, or to
    /// the end of the run when nothing matches.
    fn signature_scan_work(t: &str) -> usize {
        let re = signature_re();
        let mut work = 0;
        signature_spans_with(t, |url, from| {
            let m = re.find_at(url, from).map(|m| (m.start(), m.end()));
            work += m.map_or(url.len(), |(_, e)| e) - from;
            m
        });
        work
    }

    /// One run of `AWSAccessKeyId=Signature=` with no space: every candidate is refused (`=`
    /// before it) and its greedy value runs to the end of the run. The searches must read each
    /// byte a bounded number of times, so 4 times the text is under 5 times the work.
    #[test]
    fn refused_query_signatures_in_one_run_are_read_once() {
        let unit = "AWSAccessKeyId=Signature=";
        let n = unit.repeat(400);
        let n4 = unit.repeat(1600);
        assert!(query_signature_spans(signature_re(), &n4).is_empty());
        let (w, w4) = (signature_scan_work(&n), signature_scan_work(&n4));
        assert!(
            w <= 2 * n.len() && w4 < 5 * w,
            "work {w} at {} bytes, {w4} at {} bytes",
            n.len(),
            n4.len()
        );
    }

    #[test]
    fn s3_and_gcs_signed_url_signatures_and_tokens() {
        let r = |t: &str| redact_text(t, None, None);
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(
            r(&format!(
                "https://b.s3.example.invalid/f.csv?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20261101T000000Z\
                 &X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Security-Token=IQoJb3JpZ2luX2Vj%2F%2BAbCd1234\
                 &X-Amz-Signature={hex}"
            )),
            "https://b.s3.example.invalid/f.csv?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20261101T000000Z\
             &X-Amz-Expires=900&X-Amz-SignedHeaders=host&[REDACTED:signed_url]&[REDACTED:signed_url]"
        );
        assert_eq!(
            r(&format!(
                "https://storage.example.invalid/b/o?X-Goog-Algorithm=GOOG4-RSA-SHA256&X-Goog-Signature={hex}&X-Goog-Expires=900"
            )),
            "https://storage.example.invalid/b/o?X-Goog-Algorithm=GOOG4-RSA-SHA256&[REDACTED:signed_url]&X-Goog-Expires=900"
        );
        // the header name in prose and a short value stay
        for t in [
            "the X-Amz-Signature header is missing",
            "X-Amz-Signature=abc123",
            "X-Goog-Signature: see the docs",
        ] {
            assert_eq!(r(t), t);
        }
    }

    #[test]
    fn redact_for_store_drops_refs_taken_from_redacted_text() {
        // an importer takes refs from the raw text, so a secret with `/` arrives as a path ref
        for (secret, fragment) in [
            (
                "aws_secret_access_key=Zx9Qw8Er7Ty6/Uu5Ii4Oo3Pp/2Aa1Ss0Dd9Ff8Gg7",
                "Uu5Ii4Oo3Pp",
            ),
            (
                "Authorization: Bearer abcDEF123/ghiJKL456/mnoPQR789",
                "ghiJKL456",
            ),
            ("password=Hunter2abc/def/ghijk", "Hunter2abc"),
        ] {
            let body = format!("set {secret} then see docs/run.md for ABC-123 at {FAKE_SHA}");
            let item = Item {
                refs: format!("{} R5", extract_refs(Some(&body)).join(" ")),
                ..Item::bare("m_r", "a", "fact", "T", &body, "assumed")
            }
            .post_init();
            assert!(item.refs.contains(fragment), "{}", item.refs);
            let out = redact_for_store(&item, None);
            assert_eq!(
                out.refs,
                format!("ABC-123 docs/run.md {FAKE_SHA} R5"),
                "{secret}"
            );
        }
    }

    /// Plain refs as people write them: ids, paths, hashes, dates and URLs with no credentials,
    /// split by spaces, commas or both.
    const PLAIN_REFS: [&str; 14] = [
        "TKT-9",
        "TKT-9 TKT-10",
        "TKT-9,TKT-10, ABC-123",
        "docs/run.md src/core/redact.rs",
        "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        "685438b, 1107c518a",
        "https://github.com/org/repo/pull/12 TKT-9",
        "https://example.com:8443/a/b@c, docs/x.md",
        "PR-77,https://cdn.example.com/img/logo@2x.png",
        "rust/tests/port_transfer.rs:281 TKT-1",
        "ssh://git@github.com/org/repo.git",
        "token_rotation.md, password-reset.md secret_store.md",
        "2026-11-20 TKT-4 m_01JABCDEFGHJKMNPQRSTVWXYZ0",
        "TKT-5, , TKT-6,,TKT-7",
    ];

    #[test]
    fn redact_for_store_keeps_plain_refs_as_they_are() {
        for refs in PLAIN_REFS {
            let item = Item {
                refs: refs.to_string(),
                ..Item::bare("m_r", "a", "fact", "T", "plain body", "assumed")
            };
            assert_eq!(redact_for_store(&item, None).refs, refs);
        }
    }

    #[test]
    fn redact_for_store_drops_every_ref_a_secret_written_with_spaces_spans() {
        for (refs, secret, want) in [
            (
                "TKT-9 aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "wJalrXUtnFEMI",
                "TKT-9",
            ),
            (
                "TKT-9 password: Hunter2-example-Zz9",
                "Hunter2-example-Zz9",
                "TKT-9",
            ),
            (
                "TKT-9, Bearer Hunter2-example-token-0001 docs/run.md",
                "Hunter2-example-token-0001",
                "TKT-9 docs/run.md",
            ),
            (
                "TKT-9 token = Hunter2-example,TKT-10",
                "Hunter2-example",
                "TKT-9 TKT-10",
            ),
            // the match starts inside the ref, not at its first byte
            (
                "TKT-9 x<private>Pv7example </private> TKT-10",
                "Pv7example",
                "TKT-9 TKT-10",
            ),
        ] {
            let item = Item {
                refs: refs.to_string(),
                ..Item::bare("m_r", "a", "fact", "T", "plain body", "assumed")
            };
            let out = redact_for_store(&item, None).refs;
            assert!(!out.contains(secret), "{refs} -> {out}");
            assert_eq!(out, want, "{refs}");
        }
    }

    /// A key word or `Bearer` whose value would be a second key word that has its own value after
    /// it, each with the text it must give: one marker from the first key through the second
    /// pair (`{v}` is the secret).
    const PAIR_AFTER_AN_EMPTY_VALUE: [(&str, &str); 11] = [
        ("TOKEN=\npassword: {v}", "[REDACTED:key_value]"),
        ("api_key= password: {v}", "[REDACTED:key_value]"),
        ("SECRET_KEY=\nDB_PASSWORD: {v}", "[REDACTED:key_value]"),
        ("TOKEN=\npassword = {v}", "[REDACTED:key_value]"),
        ("TOKEN=\n\npassword: {v}", "[REDACTED:key_value]"),
        ("Bearer password: {v}", "[REDACTED:bearer]"),
        // three in a row: `SECRET=` is too short to be the value of `API_KEY=`, and the match
        // from `SECRET` goes on through the last pair
        (
            "API_KEY=\nSECRET=\nPASSWORD: {v}",
            "API_KEY=\n[REDACTED:key_value]",
        ),
        // the quoted colon form and the bare colon form
        ("TOKEN: \"password: {v}\"", "[REDACTED:key_value]\""),
        ("TOKEN: Db2_password = {v}", "[REDACTED:key_value]"),
        // an access key name's bare 40-char value, which only `kv_aws_secret` takes
        (
            "TOKEN=\nsecret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "[REDACTED:key_value]",
        ),
        // the second value starts right where the first match ends, after its optional quote
        ("TOKEN=\npassword:\"{v}\"", "[REDACTED:key_value]"),
    ];

    #[test]
    fn a_key_whose_value_is_a_second_key_with_its_own_value_redacts_the_second_pair() {
        let v = "Hunter2-example-Zz9";
        let mut wrong = Vec::new();
        for (shape, want) in PAIR_AFTER_AN_EMPTY_VALUE {
            let raw = shape.replace("{v}", v);
            let got = counted(&raw);
            let kind = if want.starts_with("[REDACTED:bearer]") {
                "bearer"
            } else {
                "key_value"
            };
            if got != (want.to_string(), counts(&[(kind, 1)])) {
                wrong.push(format!("{raw:?} -> {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        // a value that holds the second pair whole, or a second key with no value of its own,
        // keeps one marker over all of it, as before
        for (raw, want, kind) in [
            (
                "TOKEN=password=secret12",
                "[REDACTED:key_value]",
                "key_value",
            ),
            ("TOKEN=\npassword:", "[REDACTED:key_value]", "key_value"),
            (
                "TOKEN=\npassword: short",
                "[REDACTED:key_value] short",
                "key_value",
            ),
            (
                "Bearer password=Hunter2-example-Zz9",
                "[REDACTED:bearer]",
                "bearer",
            ),
            ("Bearer password", "[REDACTED:bearer]", "bearer"),
            (
                "Bearer password: short",
                "[REDACTED:bearer]: short",
                "bearer",
            ),
            // a second word that is no key word is the first key's value, whatever follows it
            (
                "api_key= user_name: Alice-example-01",
                "[REDACTED:key_value] Alice-example-01",
                "key_value",
            ),
        ] {
            assert_eq!(
                counted(raw),
                (want.to_string(), counts(&[(kind, 1)])),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_chain_of_keys_with_empty_values_takes_linear_time() {
        // 256 KB of key words, each with an empty `=` value before the next one: one match runs
        // through the chain and ends at the last key, whose value is empty; a scan to the end of
        // the chain from each key would read about 4 GB
        let started = std::time::Instant::now();
        for (unit, want) in [
            ("api_key= ", Some("[REDACTED:key_value] ")),
            ("PASSWORD=\n", Some("[REDACTED:key_value]\n")),
            ("a_key=", None),
        ] {
            let n = (1 << 18) / unit.len();
            let raw = unit.repeat(n);
            let got = redact_text(&raw, None, None);
            let end = &got[got.len().saturating_sub(60)..];
            assert!(got == want.unwrap_or(&raw), "{unit:?} ends {end:?}");
        }
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
    }

    #[test]
    fn redact_for_store_drops_the_ref_of_a_second_pair_after_an_empty_value() {
        let v = "Hunter2-example-Zz9";
        let mut cases: Vec<(String, String)> = PAIR_AFTER_AN_EMPTY_VALUE[..6]
            .iter()
            .map(|(shape, _)| {
                (
                    format!("TKT-1 {}", shape.replace("{v}", v)),
                    "TKT-1".to_string(),
                )
            })
            .collect();
        // a comma and a vertical tab part the refs; read single-spaced they are one match, from
        // `api_key=` (whose value is the bearer marker and the `:` after it) through the secret
        cases.push((
            format!("TKT-1 api_key=,,Bearer db_password:\x0b{v}"),
            "TKT-1".to_string(),
        ));
        let mut wrong = Vec::new();
        for (refs, want) in cases {
            let item = Item {
                refs: refs.clone(),
                ..Item::bare("m_r", "a", "fact", "T", "plain body", "assumed")
            };
            let got = redact_for_store(&item, None).refs;
            if got != want {
                wrong.push(format!("{refs:?} -> {got:?}, want {want:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// More shapes of a pair after a key or `Bearer`, each with the body and the refs (written
    /// after `TKT-1 `) it must give (`{v}` is the secret).
    const PAIRS_AFTER_A_MATCH: [(&str, &str, &str); 24] = [
        ("TOKEN=\npassword: {v}", "[REDACTED:key_value]", "TKT-1"),
        ("api_key= password: {v}", "[REDACTED:key_value]", "TKT-1"),
        ("TOKEN=\npassword = {v}", "[REDACTED:key_value]", "TKT-1"),
        ("Bearer password: {v}", "[REDACTED:bearer]", "TKT-1"),
        (
            "Bearer api_key= password: {v}",
            "[REDACTED:bearer]",
            "TKT-1",
        ),
        ("TOKEN=\npassword:\"{v}\"", "[REDACTED:key_value]", "TKT-1"),
        // a chain of empty values, even and odd in length
        (
            ".env of staging:\nGITHUB_TOKEN=\nSLACK_TOKEN=\nSTRIPE_SECRET=\nSENTRY_TOKEN=\nNPM_TOKEN=\nDB_PASSWORD={v}",
            ".env of staging:\n[REDACTED:key_value]",
            "TKT-1 .env of staging:",
        ),
        (
            "GITHUB_TOKEN=\nSLACK_TOKEN=\nNPM_TOKEN=\nAPI_KEY=\nPASSWORD: {v}",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        (
            "api_key= api_key= api_key= password: {v}",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        // a second key glued to a prefix that holds no key char
        ("TOKEN=\n#password: {v}", "[REDACTED:key_value]", "TKT-1"),
        ("TOKEN=\n$password = {v}", "[REDACTED:key_value]", "TKT-1"),
        ("TOKEN=\n@password: {v}", "[REDACTED:key_value]", "TKT-1"),
        (
            "TOKEN=\nprod/db/password: {v}",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        ("Bearer x/password: {v}", "[REDACTED:bearer]", "TKT-1"),
        ("TOKEN=\\npassword: {v}", "[REDACTED:key_value]", "TKT-1"),
        // an access key name's bare 40-char value
        (
            "TOKEN=\nsecret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        // a bare value that ends with a bearer match: the `:` after it, which refused the value,
        // goes with the pair the bearer match takes in
        (
            "passwd: SECRET_KEY$Bearer password: {v}",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        // the pair taken in ends inside a later match, which goes with it whole
        (
            "api_key= password: secret_access_key={v}'",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        // a match of the last key_value pass after text an earlier marker took in, with a ref
        // between them that no match touches
        (
            "TOKEN=\npassword: {v} TKT-2 passwd: SECRET_KEY$Bearer password: {v}",
            "[REDACTED:key_value] TKT-2 [REDACTED:key_value]",
            "TKT-1 TKT-2",
        ),
        // two steps of the chain end where a later match ends: the chain still takes both
        (
            "api_key= api_key= api_key=password = {v}",
            "[REDACTED:key_value]",
            "TKT-1",
        ),
        // the next match starts where the first ends, and the pair taken in holds it whole
        ("TOKEN=password='TOKEN={v}", "[REDACTED:key_value]", "TKT-1"),
        // a chain over a bearer match whose own pair is empty ends past that match's end
        (
            "TOKEN=api_key= Bearer password: ",
            "[REDACTED:key_value] ",
            "TKT-1",
        ),
        // a closing quote, then the separator of the second key
        ("TOKEN=\"password\" = {v}", "[REDACTED:key_value]", "TKT-1"),
        // a second key word that does not end its value is no second key
        (
            "TOKEN=\npassword_hint: hint-example-01",
            "[REDACTED:key_value] hint-example-01",
            "TKT-1 hint-example-01",
        ),
    ];

    #[test]
    fn a_match_goes_on_through_the_pairs_after_it_in_the_body_and_the_refs() {
        let v = "Hunter2-example-Zz9";
        let mut wrong = Vec::new();
        for (shape, body, refs) in PAIRS_AFTER_A_MATCH {
            let raw = shape.replace("{v}", v);
            let item = Item {
                refs: format!("TKT-1 {raw}"),
                ..Item::bare("m_r", "a", "fact", "T", &raw, "assumed")
            };
            let stored = redact_for_store(&item, None);
            let want = (body.replace("{v}", v), refs.replace("{v}", v));
            if (&stored.body, &stored.refs) != (&want.0, &want.1) {
                wrong.push(format!(
                    "{raw:?} -> body {:?} refs {:?}, want {want:?}",
                    stored.body, stored.refs
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_second_pass_leaves_each_pair_after_a_match_as_the_first_pass_left_it() {
        let v = "Hunter2-example-Zz9";
        let mut wrong = Vec::new();
        let shapes = PAIR_AFTER_AN_EMPTY_VALUE.iter().map(|(s, _)| *s);
        for shape in shapes.chain(PAIRS_AFTER_A_MATCH.iter().map(|(s, _, _)| *s)) {
            let raw = shape.replace("{v}", v);
            let once = redact_text(&raw, None, None);
            let twice = redact_text(&once, None, None);
            let item = Item {
                refs: format!("TKT-1 {raw}"),
                ..Item::bare("m_r", "a", "fact", "T", &raw, "assumed")
            };
            let s1 = redact_for_store(&item, None);
            let s2 = redact_for_store(&s1, None);
            if twice != once || (&s2.body, &s2.refs) != (&s1.body, &s1.refs) {
                wrong.push(format!(
                    "{raw:?}: {once:?} -> {twice:?}; refs {:?} -> {:?}",
                    s1.refs, s2.refs
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_match_whose_value_ends_with_no_paired_key_word_ends_where_it_did() {
        // each output is what the build before the chain rule gave
        for (raw, want) in [
            ("TOKEN=password=secret12", "[REDACTED:key_value]"),
            (
                "TOKEN=\nuser_name: Alice-example-01",
                "[REDACTED:key_value] Alice-example-01",
            ),
            ("Bearer token-of-8-or-more-chars", "[REDACTED:bearer]"),
            ("Bearer abc.token", "[REDACTED:bearer]"),
            (
                "HOST=\nPORT=\nTOKEN=\nDEBUG=true\nNAME=demo-app",
                "HOST=\nPORT=\n[REDACTED:key_value]\nNAME=demo-app",
            ),
            ("API_KEY=\nPASSWORD=\n", "[REDACTED:key_value]\n"),
            // the marker took no text in, so the key before it keeps the value it had
            (
                "TOKEN=\npasswd'GITHUB_TOKEN=Hunter2-example-Zz9",
                "TOKEN=\npasswd[REDACTED:key_value]",
            ),
        ] {
            assert_eq!(redact_text(raw, None, None), want, "{raw:?}");
        }
        // a key that starts right where a marker that took text in ends does not hold that
        // marker, so the last pass leaves it as the first pass left it
        let raw = "TOKEN=\"password: Hunter2-example-Zz9\"TOKEN=TOKEN: 'Hunter2-example-Zz9'x";
        let want = "[REDACTED:key_value]\"TOKEN=[REDACTED:key_value]x";
        assert_eq!(redact_text(raw, None, None), want);
    }

    #[test]
    fn a_chain_of_pairs_after_bearer_or_mixed_keys_takes_linear_time() {
        // 256 KB each; a scan that read to the end of the chain from each key would read GBs
        let started = std::time::Instant::now();
        let n = 1 << 18;
        let bearer = "Bearer password= ".repeat(n / 17);
        assert_eq!(
            redact_text(&bearer, None, None),
            "[REDACTED:bearer] ".repeat(n / 17)
        );
        let chain = format!(
            "Bearer api_key= {}password: Hunter2-example-Zz9",
            "api_key= ".repeat(n / 9)
        );
        assert_eq!(redact_text(&chain, None, None), "[REDACTED:bearer]");
        let mixed = "api_key= PASSWORD=\n".repeat(n / 19);
        assert_eq!(redact_text(&mixed, None, None), "[REDACTED:key_value]\n");
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
    }

    #[test]
    fn redact_for_store_redacts_tags_and_source() {
        let item = Item {
            tags: format!("t, {}", fake_gh()),
            source: format!("src {}", fake_gh()),
            ..Item::bare("m_t", "a", "fact", "T", "body", "assumed")
        }
        .post_init();
        let mut c = Counts::new();
        let out = redact_for_store(&item, Some(&mut c));
        assert_eq!(
            (out.tags.as_str(), out.source.as_str()),
            ("t, [REDACTED:github_token]", "src [REDACTED:github_token]")
        );
        assert_eq!(out.content_hash, item.content_hash);
        assert_eq!(c, counts(&[("github_token", 2)]));
        // the importers' sources hold no secret shape and stay as they are
        for source in [
            "task:20261101-090000-aabbccdd-redact-test-1a2b",
            "handoff:20261025-020000-handoff-sample.md",
            "automemory:-Users-a-GitHub-b/token-notes.md",
        ] {
            let it = Item {
                source: source.into(),
                ..Item::bare("m_s", "a", "fact", "T", "body", "assumed")
            }
            .post_init();
            assert_eq!(redact_for_store(&it, None), it);
        }
    }

    #[test]
    fn ignorecase_and_python_classes_catch_what_the_regex_crate_would_miss() {
        // each is redacted by Python 3.15.1; a plain [iI] class would leave the first two, and the
        // regex crate's \s would leave the Bearer one
        assert_eq!(
            redact_text("APİ_KEY=Zx9Qw8Er7Ty6", None, None),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            redact_text("apı_key=Zx9Qw8Er7Ty6", None, None),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            redact_text("PAſſWORD: Hunter2Secret99", None, None),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            redact_text("to\u{212a}en=Zx9Qw8Er7Ty6", None, None),
            "[REDACTED:key_value]"
        );
        // U+0130 is in the key's [A-Za-z0-9_.-] under re.I: it joins the key before, and it
        // blocks the key's end after (Python leaves the second one)
        assert_eq!(
            redact_text("\u{130}_password=Zx9Qw8Er7Ty6", None, None),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            redact_text("pa\u{17f}\u{17f}word\u{130}=Zx9Qw8Er7Ty6", None, None),
            "pa\u{17f}\u{17f}word\u{130}=Zx9Qw8Er7Ty6"
        );
        // \x1c is \s to Python between the key and `=`
        assert_eq!(
            redact_text("token\x1c=Zx9Qw8Er7Ty6", None, None),
            "[REDACTED:key_value]"
        );
        // \x1c is whitespace to Python's \s
        assert_eq!(
            redact_text("Bearer\x1cAbCdEf123456", None, None),
            "[REDACTED:bearer]"
        );
        // [redacted: counts as already redacted under re.I; Bearer's check is case-sensitive
        assert_eq!(
            redact_text("toKen=[redacted:x]Zx9Qw8Er7Ty6", None, None),
            "toKen=[redacted:x]Zx9Qw8Er7Ty6"
        );
        // ² is a word char, so `\bBearer` does not start after it
        assert_eq!(
            redact_text("\u{b2}Bearer AbCdEf123456", None, None),
            "\u{b2}Bearer AbCdEf123456"
        );
    }

    #[test]
    fn leaves_digit_values_prose_timestamps_ids_and_paths() {
        let text = "max_tokens=32000000 \"tokens\": 1200 max_tokens: 4500000000 \"max_tokens\": \"64000000\"\n\
                    password: requirements are listed below\n\
                    token: 2026-11-04T03:49:00+00:00\n\
                    design=signature1234567890 sigma=AbCdEf1234567890\n\
                    set `max_tokens=32000000` and `tokens: 1200`; color tokens: `#1a2b3c`\n\
                    tasklog token: 20261029-145946-9ba58a1c\n\
                    secrets: s3cr3t/path/file.yaml\n\
                    tokens: claude-haiku-4-5\n\
                    secret: 1Password vault\n\
                    secrets: Kv2026/Prod/AppConfig.yaml\n\
                    secret: KeyVaultReference\n\
                    token: m_01ABCDEFGHJKMNPQRSTVWXYZ00\n\
                    token: 01ABCDEFGHJKMNPQRSTVWXYZ00\n\
                    token: 12345678-abcd-ef01-2345-6789abcdef01\n\
                    token: 12345678-ABCD-EF01-2345-6789ABCDEF01\n";
        assert_eq!(counted(text), (text.to_string(), Counts::new()));
        assert_eq!(
            redact_text("token: \"20261029-145946-9ba58a1c\"", None, None),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            redact_text("tokens=claude-haiku-4-5", None, None),
            "[REDACTED:key_value]"
        );
    }

    #[test]
    fn url_credentials_with_an_empty_user_or_a_slash_in_the_password() {
        let cases = [
            (
                "REDIS_URL=redis://:Rd5Pw4Qz3Xc2@cache.example.invalid:6379/0",
                "REDIS_URL=[REDACTED:url_credentials]cache.example.invalid:6379/0",
            ),
            (
                "amqp://:Gu3stPw@mq.example.invalid:5672/",
                "[REDACTED:url_credentials]mq.example.invalid:5672/",
            ),
            (
                "postgres://app:Hunter2/pass@db.example.invalid:5432/app",
                "[REDACTED:url_credentials]db.example.invalid:5432/app",
            ),
            (
                "mongodb+srv://user:p/w@cluster0.example.invalid/db",
                "[REDACTED:url_credentials]cluster0.example.invalid/db",
            ),
            (
                "redis://:pa/ss//w0rd@cache.example.invalid",
                "[REDACTED:url_credentials]cache.example.invalid",
            ),
            (
                "postgres://app:Pg7Normal@db.example.invalid:5432/app",
                "[REDACTED:url_credentials]db.example.invalid:5432/app",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(
                counted(raw),
                (want.to_string(), counts(&[("url_credentials", 1)])),
                "{raw}"
            );
        }
    }

    #[test]
    fn url_credentials_after_a_word_char() {
        let cases = [
            (
                "DB_postgres://bob:Hunter2-example@db.example.com/app",
                "DB_[REDACTED:url_credentials]db.example.com/app",
            ),
            (
                "x_redis://:Hunter2-example@cache.example.com",
                "x_[REDACTED:url_credentials]cache.example.com",
            ),
            (
                "2postgres://bob:Hunter2-example@db.example.com",
                "2[REDACTED:url_credentials]db.example.com",
            ),
            (
                "url=éredis://:Hunter2-example@cache.example.com",
                "url=é[REDACTED:url_credentials]cache.example.com",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(
                counted(raw),
                (want.to_string(), counts(&[("url_credentials", 1)])),
                "{raw}"
            );
        }
    }

    #[test]
    fn url_credentials_whose_user_or_password_is_a_token_an_earlier_pattern_replaced() {
        // the token's marker holds `:`, which once made the URL rule miss a `/` password
        // two more passwords, put together from the parts of the documented example key: one
        // starts with `/`, one has only a digit before its first `/`
        let example = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let slash_first = format!(
            "s3a://AKIAIOSFODNN7EXAMPLE:{}{}@my-bucket/data",
            &example[13..],
            &example[..13]
        );
        let digit_first = format!(
            "s3a://AKIAIOSFODNN7EXAMPLE:3/E{}{}{}@my-bucket/data",
            &example[..13],
            &example[14..21],
            &example[22..39]
        );
        assert!(slash_first.contains(":/K7MDENG/") && digit_first.contains(":3/EwJalr"));
        assert_eq!((slash_first.len(), digit_first.len()), (82, 82));
        let cases = [
            (
                "s3a://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY@my-bucket/data",
                "[REDACTED:url_credentials]my-bucket/data",
                "aws_key",
            ),
            (
                "https://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY@s3.amazonaws.com/b",
                "[REDACTED:url_credentials]s3.amazonaws.com/b",
                "aws_key",
            ),
            (
                "git clone https://ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01:Hunter2/example@github.com/o/r",
                "git clone [REDACTED:url_credentials]github.com/o/r",
                "github_token",
            ),
            (
                "https://bob:AKIAIOSFODNN7EXAMPLE/Hunter2-example@h.example.com/p",
                "[REDACTED:url_credentials]h.example.com/p",
                "aws_key",
            ),
            (
                "https://bob:Hunter2-example/AKIAIOSFODNN7EXAMPLE@h.example.com/p",
                "[REDACTED:url_credentials]h.example.com/p",
                "aws_key",
            ),
            // after a token user, the text up to `@` is the password whatever it starts with or
            // holds: a leading `/`, only digits before the first `/`, a `:` and a `/`
            (
                slash_first.as_str(),
                "[REDACTED:url_credentials]my-bucket/data",
                "aws_key",
            ),
            (
                digit_first.as_str(),
                "[REDACTED:url_credentials]my-bucket/data",
                "aws_key",
            ),
            (
                "s3a://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI:K7MDENG/bPxRfiCYEXAMPLEKEY@my-bucket/data",
                "[REDACTED:url_credentials]my-bucket/data",
                "aws_key",
            ),
            (
                "git clone https://ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01:/Hunter2-example@git.example.com/o/r",
                "git clone [REDACTED:url_credentials]git.example.com/o/r",
                "github_token",
            ),
        ];
        for (raw, want, kind) in cases {
            assert_eq!(
                counted(raw),
                (
                    want.to_string(),
                    counts(&[(kind, 1), ("url_credentials", 1)])
                ),
                "{raw}"
            );
        }
        // a marker in the authority is never a host, so digits after its `:` are no port but the
        // password
        assert_eq!(
            counted("https://AKIAIOSFODNN7EXAMPLE:8080/a@b"),
            (
                "[REDACTED:url_credentials]b".to_string(),
                counts(&[("aws_key", 1), ("url_credentials", 1)])
            )
        );
    }

    #[test]
    fn a_key_value_value_that_starts_with_a_marker_goes_on_past_it() {
        for raw in [
            "password=AKIAIOSFODNN7EXAMPLE/Hunter2-example",
            "password=AKIAIOSFODNN7EXAMPLE:Hunter2-example",
            "password=\"AKIAIOSFODNN7EXAMPLE/Hunter2-example\"",
        ] {
            assert_eq!(
                counted(raw),
                (
                    "[REDACTED:key_value]".to_string(),
                    counts(&[("aws_key", 1), ("key_value", 1)])
                ),
                "{raw}"
            );
        }
        assert_eq!(
            counted("secret=ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01/Hunter2-example"),
            (
                "[REDACTED:key_value]".to_string(),
                counts(&[("github_token", 1), ("key_value", 1)])
            )
        );
        // a marker that is the whole value stays the one marker
        for raw in [
            "password=AKIAIOSFODNN7EXAMPLE",
            "password=AKIAIOSFODNN7EXAMPLE, next",
            "password=AKIAIOSFODNN7EXAMPLE]",
        ] {
            let want = raw.replace("AKIAIOSFODNN7EXAMPLE", "[REDACTED:aws_key]");
            assert_eq!(counted(raw), (want, counts(&[("aws_key", 1)])), "{raw}");
        }
        // only a marker the patterns wrote goes on: text that spells one in lower case stays
        let typed = "password=[redacted:aws_key]/Hunter2-example";
        assert_eq!(counted(typed), (typed.to_string(), Counts::new()));
    }

    #[test]
    fn a_key_value_value_with_a_colon_after_a_starting_token_is_one_marker() {
        let ak = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let gh = "ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01";
        let cases = [
            (format!("aws_token=AKIAIOSFODNN7EXAMPLE:{ak}"), "aws_key"),
            (format!("secret={gh}:Hunter2-example"), "github_token"),
            (
                "apikey=sk-proj-EXAMPLEexampleEXAMPLEexampleEXAMPLE.Hunter2-example".to_string(),
                "sk_key",
            ),
            (format!("aws_token: AKIAIOSFODNN7EXAMPLE:{ak}"), "aws_key"),
            (format!("secret: {gh}:Hunter2-example"), "github_token"),
            (
                "password: `AKIAIOSFODNN7EXAMPLE/Hunter2-example".to_string(),
                "aws_key",
            ),
            // a marker later in a bare value is part of it, as the token it stands for was
            (
                "secret: Hunter2-example9-AKIAIOSFODNN7EXAMPLE".to_string(),
                "aws_key",
            ),
            (format!("token: Hunter2-example9-{gh}-Zz"), "github_token"),
            (
                "secret: hunter-example-AKIAIOSFODNN7EXAMPLE".to_string(),
                "aws_key",
            ),
        ];
        for (raw, kind) in cases {
            assert_eq!(
                counted(&raw),
                (
                    "[REDACTED:key_value]".to_string(),
                    counts(&[(kind, 1), ("key_value", 1)])
                ),
                "{raw}"
            );
        }
        // the colon form keeps a marker that is the whole value, as the `=` form does
        for raw in [
            "token: AKIAIOSFODNN7EXAMPLE",
            "token: AKIAIOSFODNN7EXAMPLE and more",
        ] {
            let want = raw.replace("AKIAIOSFODNN7EXAMPLE", "[REDACTED:aws_key]");
            assert_eq!(counted(raw), (want, counts(&[("aws_key", 1)])), "{raw}");
        }
    }

    /// One made-up token per pattern that runs before url_credentials or key_value, with the
    /// part of it that must not stay in clear.
    const TOKENS_BEFORE_URL_AND_KEY_VALUE: [(&str, &str); 15] = [
        ("<private>Pv7example</private>", "Pv7example"),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEexampleKEY\n-----END RSA PRIVATE KEY-----",
            "MIIEexampleKEY",
        ),
        ("AKIAIOSFODNN7EXAMPLE", "IOSFODNN7EXAMPLE"),
        (
            "ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01",
            "EXAMPLEexampleEXAMPLE",
        ),
        ("github_pat_EXAMPLE_example_EXAMPLE_01", "EXAMPLE_example"),
        ("xoxb-EXAMPLE-example-01", "EXAMPLE-example"),
        (
            "AIzaEXAMPLEexampleEXAMPLEexampleEXAMPLE",
            "EXAMPLEexampleEXAMPLE",
        ),
        (
            "sk-EXAMPLEexampleEXAMPLEexampleEXAMPLE",
            "EXAMPLEexampleEXAMPLE",
        ),
        ("eyJEXAMPLE.eyJexample.EXAMPLEsig", "EXAMPLEsig"),
        ("Bearer EXAMPLEexample01", "EXAMPLEexample01"),
        ("postgres://bob:Pw0example@db.example.com", "Pw0example"),
        ("sig=EXAMPLEexampleEXAMPLE01", "EXAMPLEexampleEXAMPLE01"),
        (
            "X-Amz-Signature=EXAMPLEexampleEXAMPLE01",
            "EXAMPLEexampleEXAMPLE01",
        ),
        (
            "https://b.example.com/f?AWSAccessKeyId=EX&Signature=EXAMPLEexampleEXAMPLE01",
            "EXAMPLEexampleEXAMPLE01",
        ),
        (
            "AccountKey=EXAMPLEexampleEXAMPLE01==",
            "EXAMPLEexampleEXAMPLE01",
        ),
    ];

    #[test]
    fn a_token_next_to_a_second_secret_leaves_neither_in_clear() {
        let shapes = [
            "s3a://{t}:Hunter2-example/x@h.example.com/p",
            "https://{t}:Hunter2-example@h.example.com/p",
            "https://bob:{t}/Hunter2-example@h.example.com/p",
            "https://bob:Hunter2-example/{t}@h.example.com/p",
            "password={t}/Hunter2-example",
            "password={t}:Hunter2-example",
            "password=Hunter2-example/{t}/Hunter2-example",
            "password=Hunter2-example/{t}",
            "secret: {t}:Hunter2-example",
            "secret: Hunter2-example9-{t}-Zz",
            "secret: Hunter2-example9-{t}",
        ];
        // a password that reads as `host:port/path` after a plain user: it is caught only after
        // a token user, so only for the patterns that run before url_credentials (no `name=`)
        let token_user_shapes = [
            "s3a://{t}:/Hunter2-example@h.example.com/p",
            "s3a://{t}:3/Hunter2-example@h.example.com/p",
            "s3a://{t}:Hunter2-example:x/y@h.example.com/p",
        ];
        let mut leaks = Vec::new();
        for (token, core) in TOKENS_BEFORE_URL_AND_KEY_VALUE {
            let user_shapes = token_user_shapes.iter().filter(|_| !token.contains('='));
            for shape in shapes.iter().chain(user_shapes) {
                // a URL inside a URL's user or password part is no credential URL shape, and a
                // bare colon value stops at `:` and `/` (`kv_bare`)
                if token.contains("://") && (shape.contains("://") || shape.contains(": ")) {
                    continue;
                }
                let raw = shape.replace("{t}", token);
                let got = redact_text(&raw, None, None);
                if got.contains("Hunter2-example") || got.contains(core) {
                    leaks.push(format!("{raw:?} -> {got:?}"));
                }
            }
        }
        assert!(leaks.is_empty(), "{}", leaks.join("\n"));
    }

    #[test]
    fn a_key_value_holding_url_credentials_after_a_word_char_is_one_marker() {
        // url_credentials runs first and leaves its marker inside the value; key_value then
        // takes the value to its end, the marker with it
        for raw in [
            "password=x_postgres://bob:Hunter2-example@db.example.com",
            "secret=x_redis://:Hunter2-example@cache.example.com",
            "api_key=1postgres://bob:Hunter2-example@db.example.com",
        ] {
            assert_eq!(
                counted(raw),
                (
                    "[REDACTED:key_value]".to_string(),
                    counts(&[("url_credentials", 1), ("key_value", 1)])
                ),
                "{raw}"
            );
        }
        let two = "token=x_postgres://bob:Hunter2-example@db.example.com/x_redis://:Hunter2-example@c.example.com";
        assert_eq!(
            counted(two),
            (
                "[REDACTED:key_value]".to_string(),
                counts(&[("url_credentials", 2), ("key_value", 1)])
            )
        );
        // a `]` with no marker before it in the value still ends the value
        assert_eq!(
            counted("list=[password=Hunter2-example]"),
            (
                "list=[[REDACTED:key_value]]".to_string(),
                counts(&[("key_value", 1)])
            )
        );
    }

    #[test]
    fn url_credentials_leave_ports_paths_and_emails_in_urls() {
        // a `/` ends the authority, so each of these is a host, a port and a path
        let text = "https://cdn.example.com:443/img/logo@2x.png\n\
                    http://localhost:3000/@scope/pkg\n\
                    http://example.com:8080/users/bob@example.com/profile\n\
                    ssh://github.com:org/repo.git@v1.2\n\
                    https://example.com/a:b/c@d\n\
                    see https://example.com:8080/docs,mailto:bob@example.com\n\
                    https://example.com/contact;bob@example.com\n\
                    http://intranet:8080/team/bob@corp\n\
                    http://localhost:/x@y\n\
                    http://[::1]:8080/@scope/pkg\n\
                    http://[fe80::1]/x@y\n\
                    file://C:/Users/bob@corp/notes.txt\n\
                    docker://registry:5000/team/img@sha256:abc123\n\
                    https://bob@example.com:8443/a/b@c\n\
                    git+ssh://git@github.com:org/repo.git\n\
                    mailto:bob@example.com\n";
        assert_eq!(counted(text), (text.to_string(), Counts::new()));
        // Known limit, listed in the README: on a host without a dot, `word:word/path@suffix`
        // is read as a user and a password with a `/`, so an scp-style path in an ssh:// URL is
        // replaced as if it were a credential. The same text on a host with a dot stays (above).
        assert_eq!(
            counted("ssh://host:org/repo.git@v1.2"),
            (
                "[REDACTED:url_credentials]v1.2".to_string(),
                counts(&[("url_credentials", 1)])
            )
        );
        // a long run of authority-like fragments stays one linear scan
        let run = "x://a:b/".repeat(150_000);
        assert_eq!(redact_text(&run, None, None), run);
        let got = redact_text(&format!("{run}@h"), None, None);
        let want = format!("{}[REDACTED:url_credentials]h", "x://a:b/".repeat(149_999));
        assert!(got == want, "long run ends: {:?}", &got[got.len() - 40..]);
        // a token user with a long password of many `/` and no `@` is one linear scan too
        let pw = "/a".repeat(150_000);
        let got = redact_text(&format!("s3a://AKIAIOSFODNN7EXAMPLE:{pw}"), None, None);
        assert!(
            got == format!("s3a://[REDACTED:aws_key]:{pw}"),
            "no @: {:?}",
            &got[..40]
        );
        let got = redact_text(&format!("s3a://AKIAIOSFODNN7EXAMPLE:{pw}@h"), None, None);
        assert!(got == "[REDACTED:url_credentials]h", "@h: {:?}", &got[..40]);
    }

    #[test]
    fn a_token_user_takes_the_password_only_up_to_an_at_in_the_authority() {
        let unchanged = [
            "https://example.com:8080/a@b",
            "ssh://git@host.example.com:22/path",
            "https://user@host/p",
            "scp://host:/path/to/file and mail me@example.com",
        ];
        for raw in unchanged {
            assert_eq!(counted(raw), (raw.to_string(), Counts::new()), "{raw}");
        }
        // the token is replaced, the rest stays: no `@` before the whitespace, or the token is
        // in the path or the query, not the user part
        let cases = [
            (
                "see s3://AKIAIOSFODNN7EXAMPLE:/prefix/ for details",
                "see s3://[REDACTED:aws_key]:/prefix/ for details",
            ),
            (
                "see s3://AKIAIOSFODNN7EXAMPLE:/prefix/ or mail me@example.com",
                "see s3://[REDACTED:aws_key]:/prefix/ or mail me@example.com",
            ),
            (
                "https://host.example.com/AKIAIOSFODNN7EXAMPLE:/x@y",
                "https://host.example.com/[REDACTED:aws_key]:/x@y",
            ),
            (
                "https://host.example.com:8080/k/AKIAIOSFODNN7EXAMPLE:3/x@y",
                "https://host.example.com:8080/k/[REDACTED:aws_key]:3/x@y",
            ),
            (
                "https://host.example.com/p?k=AKIAIOSFODNN7EXAMPLE:/x@y",
                "https://host.example.com/p?k=[REDACTED:aws_key]:/x@y",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(
                counted(raw),
                (want.to_string(), counts(&[("aws_key", 1)])),
                "{raw}"
            );
        }
    }

    #[test]
    fn runs_over_a_million_chars_redact_as_python_does() {
        // fancy-regex's 1M-entry backtrack stack panicked on each of these; Python 3.15.1 gives
        // the right side (review w23-core block row)
        let r = |t: &str| redact_text(t, None, None);
        assert_eq!(
            r(&format!("ghp_{}", "a".repeat(1_200_000))),
            "[REDACTED:github_token]"
        );
        assert_eq!(
            r(&format!("sk-{}", "a".repeat(1_200_000))),
            "[REDACTED:sk_key]"
        );
        assert_eq!(
            r(&format!("token: {}", "aA1".repeat(400_000))),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            r(&format!("password={}x", "1".repeat(1_200_000))),
            "[REDACTED:key_value]"
        );
        assert_eq!(
            r(&format!("<private>{}</private>", "a".repeat(1_200_000))),
            "[REDACTED:private]"
        );
        let open = format!("<private{}", "a".repeat(1_200_000));
        assert_eq!(r(&open), open);
        // a long key run with no value: Python's search is quadratic here, the scan is not
        let run = format!("token {}", "a1".repeat(600_000));
        assert_eq!(r(&run), run);
    }

    #[test]
    fn lookarounds_checked_in_code_match_python() {
        // expected values from Python 3.15.1 redact_text
        let r = |t: &str| redact_text(t, None, None);
        // a fixed-length core with an alnum after it is no match at that start
        assert_eq!(r("AKIAABCDEFGHIJKLMNOPQ"), "AKIAABCDEFGHIJKLMNOPQ");
        assert_eq!(r("AKIAAKIAABCDEFGHIJKLMNOP"), "AKIAAKIAABCDEFGHIJKLMNOP");
        assert_eq!(
            r("xAKIAABCDEFGHIJKLMNOP AKIAABCDEFGHIJKLMNOP"),
            "xAKIAABCDEFGHIJKLMNOP [REDACTED:aws_key]"
        );
        assert_eq!(
            r(&format!("AIza{}b", "a".repeat(35))),
            format!("AIza{}b", "a".repeat(35))
        );
        assert_eq!(
            r(&format!("-AIza{}", "a".repeat(35))),
            "-[REDACTED:google_api_key]"
        );
        // the lookbehind char may sit inside a run: `sk-` after `-` is no start, after the run start is
        assert_eq!(r(&format!("sk-sk-{}", "a".repeat(40))), "[REDACTED:sk_key]");
        assert_eq!(
            r(&format!("xsk-{}", "a".repeat(40))),
            format!("xsk-{}", "a".repeat(40))
        );
        // key_value: a quote before the run starts the match; a key mid-run takes the whole run
        assert_eq!(r("\"token\": \"abcdefgh1\""), "[REDACTED:key_value]");
        assert_eq!(r("xxpassword=Zx9Qw8Er7Ty6"), "[REDACTED:key_value]");
        assert_eq!(
            r("xpassword_valuex=Zx9Qw8Er7Ty6"),
            "xpassword_valuex=Zx9Qw8Er7Ty6"
        );
        assert_eq!(r("'apikey'='abcdefgh'tail"), "[REDACTED:key_value]tail");
        // bare colon: an m_ id of exactly 26, a `/` or `:` after, and fewer than 12 chars stay
        assert_eq!(
            r("secret: m_01ABCDEFGHJKMNPQRSTVWXYZ00"),
            "secret: m_01ABCDEFGHJKMNPQRSTVWXYZ00"
        );
        assert_eq!(
            r("secret: m_01ABCDEFGHJKMNPQRSTVWXYZ00X"),
            "[REDACTED:key_value]"
        );
        assert_eq!(r("token: Ab1cdefghijk/"), "token: Ab1cdefghijk/");
        assert_eq!(r("token: Ab1cdefghijk:"), "token: Ab1cdefghijk:");
        assert_eq!(r("token: Ab1cdefghij"), "token: Ab1cdefghij");
        assert_eq!(r("token=12345678 "), "token=12345678 ");
        assert_eq!(r("token=12345678x"), "[REDACTED:key_value]");
        assert_eq!(r("Bearer  [REDACTED:x]"), "Bearer  [REDACTED:x]");
        // `private\b` and a tag with no `>` after it
        assert_eq!(
            r("a <privatex>b</private> <private>c</private>"),
            "a <privatex>b</private> [REDACTED:private]"
        );
        assert_eq!(r("a <private b </private>"), "a <private b </private>");
        assert_eq!(r("<privateaaaaaaaaaa"), "<privateaaaaaaaaaa");
    }

    /// A token of each pattern that refuses a match after a word char, with its marker's kind.
    const GUARDED_TOKENS: [(&str, &str); 10] = [
        ("AKIAIOSFODNN7EXAMPLE", "aws_key"),
        ("ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01", "github_token"),
        ("xoxb-EXAMPLE-example-01", "slack_token"),
        ("AIzaEXAMPLEexampleEXAMPLEexampleEXAMPLE", "google_api_key"),
        ("sk-EXAMPLEexampleEXAMPLEexampleEXAMPLE", "sk_key"),
        ("eyJEXAMPLE.eyJexample.EXAMPLEsig", "jwt"),
        ("Bearer Hunter2-example-Zz9", "bearer"),
        ("sig=EXAMPLEexampleEXAMPLE01", "azure_sas"),
        ("X-Amz-Signature=EXAMPLEexampleEXAMPLE01", "signed_url"),
        ("AccountKey=EXAMPLEexampleEXAMPLE01==", "azure_key"),
    ];

    /// Each `(raw, want)` through `redact_text` twice and, with `TKT-1 <raw>` as refs, through
    /// `redact_for_store` three times: the body is `want` each time, the refs are `refs` each time.
    fn check_body_and_refs(
        rows: &[(String, String)],
        refs: impl Fn(&str) -> String,
    ) -> Vec<String> {
        let mut wrong = Vec::new();
        for (raw, want) in rows {
            let once = redact_text(raw, None, None);
            let twice = redact_text(&once, None, None);
            let item = Item {
                refs: format!("TKT-1 {raw}"),
                ..Item::bare("m_r", "a", "fact", "T", raw, "assumed")
            };
            let s1 = redact_for_store(&item, None);
            let s2 = redact_for_store(&s1, None);
            let s3 = redact_for_store(&s2, None);
            let want_refs = refs(raw);
            if (&once, &twice, &s1.body, &s2.body, &s3.body) != (want, want, want, want, want)
                || (&s1.refs, &s2.refs, &s3.refs) != (&want_refs, &want_refs, &want_refs)
            {
                wrong.push(format!(
                    "{raw:?}: {once:?} then {twice:?}; stored {:?} then {:?} then {:?}, refs {:?} then {:?} then {:?}; want {want:?}, refs {want_refs:?}",
                    s1.body, s2.body, s3.body, s1.refs, s2.refs, s3.refs
                ));
            }
        }
        wrong
    }

    /// `check_body_and_refs` over `(raw, body, refs)` rows, with `{v}` in each raw replaced by `v`.
    fn check_rows(rows: &[(&str, &str, &str)], v: &str) -> Vec<String> {
        let rows: Vec<(String, String, String)> = rows
            .iter()
            .map(|(r, b, f)| (r.replace("{v}", v), b.to_string(), f.to_string()))
            .collect();
        let pairs: Vec<(String, String)> = rows
            .iter()
            .map(|(r, b, _)| (r.clone(), b.clone()))
            .collect();
        check_body_and_refs(&pairs, |raw| {
            rows.iter()
                .find(|(r, _, _)| r == raw)
                .map(|(_, _, f)| f.clone())
                .unwrap_or_default()
        })
    }

    #[test]
    fn a_token_right_after_a_literal_escape_is_redacted_in_the_body_and_the_refs() {
        let mut rows = Vec::new();
        for (tok, kind) in GUARDED_TOKENS {
            // `\r\n` is two escapes; a doubled backslash before the letter is still a backslash
            // then the letter, so the rule, which reads one backslash back, takes the token too
            for esc in ["\\n", "\\r", "\\t", "\\r\\n", "\\\\n"] {
                for before in ["note", "", "\"line one"] {
                    rows.push((
                        format!("{before}{esc}{tok}"),
                        format!("{before}{esc}[REDACTED:{kind}]"),
                    ));
                }
            }
        }
        let wrong = check_body_and_refs(&rows, |raw| {
            let quoted = raw.starts_with('"');
            if quoted { "TKT-1 \"line" } else { "TKT-1" }.to_string()
        });
        assert!(
            wrong.is_empty(),
            "{} of {} rows:\n{}",
            wrong.len(),
            rows.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn a_token_glued_to_a_word_char_that_is_no_escape_letter_stays_as_it_was() {
        let mut rows = Vec::new();
        for (tok, _) in GUARDED_TOKENS {
            // a plain word, a letter that ends no escape, a backslash then a letter of no escape
            for before in ["noten", "x", "note\\x", "note\\a", "\\N", "C:\\temp\\nt"] {
                let raw = format!("{before}{tok}");
                rows.push((raw.clone(), raw));
            }
        }
        let wrong = check_body_and_refs(&rows, |raw| format!("TKT-1 {raw}"));
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn honest_text_with_escapes_and_no_token_stays_as_it_is() {
        let rows: Vec<(String, String)> = [
            "C:\\new\\temp\\report.txt",
            "C:\\tools\\next-app",
            "split on \\n\\t\\r and \\r\\n",
            "printf 'a\\nBearer\\n'",
            "\\nBearer short",
            "\\nabla \\tau \\rho",
        ]
        .iter()
        .map(|t| (t.to_string(), t.to_string()))
        .collect();
        let wrong = check_body_and_refs(&rows, |raw| format!("TKT-1 {raw}"));
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_key_with_an_empty_value_then_an_escaped_bearer_leaves_no_value_in_clear() {
        let v = "Hunter2-example-Zz9";
        // the bearer pass takes `Bearer <v>` first; key_value then reads `TOKEN=\n[REDACTED:bearer]`,
        // whose value is the marker, and takes it all
        let rows: Vec<(String, String)> = [
            ("TOKEN=\\nBearer {v}", "[REDACTED:key_value]"),
            ("TOKEN=\\r\\nBearer {v}", "[REDACTED:key_value]"),
            (
                "note TOKEN=\\tBearer {v} done",
                "note [REDACTED:key_value] done",
            ),
        ]
        .iter()
        .map(|(raw, want)| (raw.replace("{v}", v), want.to_string()))
        .collect();
        let wrong = check_body_and_refs(&rows, |raw| {
            if raw.starts_with("note") {
                "TKT-1 note done".to_string()
            } else {
                "TKT-1".to_string()
            }
        });
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn redact_for_store_drops_private_and_secret_refs_and_rehashes() {
        let item = Item {
            refs: format!("docs/a.md docs/b.md {} R5", fake_gh()),
            evidence: format!("curl -H 'Authorization: Bearer {}'", fake_sk()),
            ..Item::bare(
                FAKE_ULID_ID,
                "a",
                "fact",
                "Title",
                "keep docs/a.md <private>hidden docs/b.md</private>",
                "assumed",
            )
        }
        .post_init();
        let mut c = Counts::new();
        let out = redact_for_store(&item, Some(&mut c));
        assert_eq!(
            out.evidence,
            "curl -H 'Authorization: Bearer [REDACTED:sk_key]'"
        );
        assert_eq!(out.body, "keep docs/a.md [REDACTED:private]");
        assert_eq!(out.refs, "docs/a.md R5");
        assert_eq!(
            out.content_hash,
            compute_content_hash("keep docs/a.md [REDACTED:private]")
        );
        assert_eq!(out.id, FAKE_ULID_ID);
        assert_eq!(c, counts(&[("private", 1), ("sk_key", 1)]));

        let clean = Item {
            refs: FAKE_SHA.into(),
            ..Item::bare("m_x", "a", "fact", "T", &format!("b {FAKE_SHA}"), "assumed")
        }
        .post_init();
        assert_eq!(redact_for_store(&clean, None), clean);
        // a redacted title alone keeps the body's hash
        let t = Item::bare("m_y", "a", "fact", "token=Zx9Qw8Er7Ty6", "body", "assumed").post_init();
        let out = redact_for_store(&t, None);
        assert_eq!(
            (out.title.as_str(), out.content_hash.as_str()),
            ("[REDACTED:key_value]", t.content_hash.as_str())
        );
    }

    #[test]
    fn a_bare_colon_value_ends_at_a_literal_escape_after_a_whole_value() {
        let v = "Hunter2-example-Zz9";
        // (raw, body, refs): the marker ends at the escape, the text after it stays; a key word
        // glued to an escape's letter (`\npassword`) starts after the letter, so the escape stays
        let rows: [(&str, &str, &str); 15] = [
            (
                "user: bob\\npassword: {v}\\nhost: db.example.com",
                "user: bob\\n[REDACTED:key_value]\\nhost: db.example.com",
                "TKT-1 user: db.example.com",
            ),
            (
                "{\"config\": \"user: bob\\npassword: {v}\\nhost: db\"}",
                "{\"config\": \"user: bob\\n[REDACTED:key_value]\\nhost: db\"}",
                "TKT-1 {\"config\": \"user: db\"}",
            ),
            (
                "password: {v}\\thost: db",
                "[REDACTED:key_value]\\thost: db",
                "TKT-1 db",
            ),
            (
                "password: {v}\\r\\nhost: db",
                "[REDACTED:key_value]\\r\\nhost: db",
                "TKT-1 db",
            ),
            (
                "api_key: {v}\\nregion: us-east-1",
                "[REDACTED:key_value]\\nregion: us-east-1",
                "TKT-1 us-east-1",
            ),
            (
                "password: {v}\\nurl: https://example.com/x",
                "[REDACTED:key_value]\\nurl: https://example.com/x",
                "TKT-1 https://example.com/x",
            ),
            (
                "password: {v}\\nBearer abcdefghij: x",
                "[REDACTED:key_value]\\n[REDACTED:bearer]: x",
                "TKT-1 x",
            ),
            (
                "password: {v}\\ndone",
                "[REDACTED:key_value]\\ndone",
                "TKT-1",
            ),
            // the head before the first escape is too short to be a value: the value is read on
            // over it and over every later escape, as before (the second row stays in clear, as
            // it does before this rule: its value runs on to `host:`)
            (
                "password: Hunter2\\nexample-Zz9 rest",
                "[REDACTED:key_value] rest",
                "TKT-1 rest",
            ),
            (
                "password: Hunter2\\nexample-Zz9\\nhost: db",
                "password: Hunter2\\nexample-Zz9\\nhost: db",
                "TKT-1 password: Hunter2\\nexample-Zz9\\nhost: db",
            ),
            (
                "password: AKIAIOSFODNN7EXAMPLETOKEN=\\n\\nexample-Zz9Hunter2",
                "[REDACTED:key_value]",
                "TKT-1",
            ),
            // a key whose value is that marker: the escape after the marker ends it there too, so
            // a second pass leaves the text as the first left it
            (
                "secret: password: {v}\\nhost: db",
                "secret: [REDACTED:key_value]\\nhost: db",
                "TKT-1 secret: db",
            ),
            (
                "secret: Bearer abcdefghij\\nhost: db",
                "secret: [REDACTED:bearer]\\nhost: db",
                "TKT-1 secret: db",
            ),
            // an escape at the start of the value is part of it: the first escape that counts is
            // one after the value has started
            (
                "password: \\t{v}\\thost: db",
                "[REDACTED:key_value]\\thost: db",
                "TKT-1 db",
            ),
            // as before: a run that never makes a value is no match
            (
                "password: abc\\ndone",
                "password: abc\\ndone",
                "TKT-1 password: abc\\ndone",
            ),
        ];
        let wrong = check_rows(&rows, v);
        assert!(
            wrong.is_empty(),
            "{} of {} rows:\n{}",
            wrong.len(),
            rows.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn honest_colon_text_with_escapes_stays_as_it_is() {
        let rows: Vec<(String, String)> = [
            "note: see C:\\new\\temp\\report.txt",
            "path: src\\tools\\next",
            "token: none\\nexpires: never",
            "password: see\\nthe vault",
        ]
        .iter()
        .map(|t| (t.to_string(), t.to_string()))
        .collect();
        let wrong = check_body_and_refs(&rows, |raw| format!("TKT-1 {raw}"));
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_match_that_would_start_on_the_letter_of_a_literal_escape_starts_after_it() {
        // (raw, body, refs): a key run or a URL scheme glued to an escape's letter starts after
        // the letter, so the escape stays in the text and a second pass reads it as the first did
        let rows: [(&str, &str, &str); 17] = [
            (
                "token: ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01\\npassword: {v}\\nhost: db",
                "token: [REDACTED:github_token]\\n[REDACTED:key_value]\\nhost: db",
                "TKT-1 token: db",
            ),
            (
                "env: token: ghp_EXAMPLEexampleEXAMPLEexampleEXAMPLE01\\npassword: {v}\\nhost: db",
                "env: token: [REDACTED:github_token]\\n[REDACTED:key_value]\\nhost: db",
                "TKT-1 env: token: db",
            ),
            (
                "secret:\\npassword: {v}\\nregion: x",
                "secret:\\n[REDACTED:key_value]\\nregion: x",
                "TKT-1 x",
            ),
            (
                "secret:\\r\\npassword: {v}\\r\\nregion: x",
                "secret:\\r\\n[REDACTED:key_value]\\r\\nregion: x",
                "TKT-1 x",
            ),
            (
                "secret:\\r\\npassword: {v}\\r\\ndone",
                "secret:\\r\\n[REDACTED:key_value]\\r\\ndone",
                "TKT-1",
            ),
            // a marker after the escapes that ends the text is no value either; with more value
            // after it, it is a value as before
            (
                "secrets:\\ntoken: {v}",
                "secrets:\\n[REDACTED:key_value]",
                "TKT-1",
            ),
            (
                "secret:\\nBearer abcdefghij",
                "secret:\\n[REDACTED:bearer]",
                "TKT-1",
            ),
            (
                "secret:\\nBearer abcdefghij#x",
                "[REDACTED:key_value]",
                "TKT-1",
            ),
            (
                "password: {v}\\nhttps://bob:Hunter2-example@h.example.com/p",
                "[REDACTED:key_value]\\n[REDACTED:url_credentials]h.example.com/p",
                "TKT-1",
            ),
            (
                "login: admin\\npassword: {v}\\npostgres://bob:Pw0example@db.example.com",
                "login: admin\\n[REDACTED:key_value]\\n[REDACTED:url_credentials]db.example.com",
                "TKT-1 login:",
            ),
            (
                "{\"log\": \"password: {v}\\r\\nhttps://bob:Hunter2-example@h.example.com/p\"}",
                "{\"log\": [REDACTED:key_value]\\r\\n[REDACTED:url_credentials]h.example.com/p\"}",
                "TKT-1 {\"log\":",
            ),
            // the search past the letter finds the match further in, where a line break would
            (
                "password: {v}\\n2postgres://bob:Hunter2-example@db.example.com",
                "[REDACTED:key_value]\\n2[REDACTED:url_credentials]db.example.com",
                "TKT-1",
            ),
            // past the letter there is no match (`oken` is no key word): the match is kept as found
            ("C:\\token: {v}", "C:\\[REDACTED:key_value]", "TKT-1"),
            // and a match that starts past its end is the next match, not this one
            (
                "C:\\token: {v}\\npassword: {v}",
                "C:\\[REDACTED:key_value]\\n[REDACTED:key_value]",
                "TKT-1",
            ),
            // a backslash before a letter of no escape: as before
            ("\\password: {v}", "\\[REDACTED:key_value]", "TKT-1"),
            // a backslash before a letter that starts no escape: the match is not moved
            ("\\xtoken: {v}", "\\[REDACTED:key_value]", "TKT-1"),
            // the next match starts right at the end of this one: this one is kept, then that one
            (
                "C:\\token: {v}\"password\": \"{v}\"",
                "C:\\[REDACTED:key_value][REDACTED:key_value]",
                "TKT-1",
            ),
        ];
        let wrong = check_rows(&rows, "Hunter2-example-Zz9");
        assert!(
            wrong.is_empty(),
            "{} of {} rows:\n{}",
            wrong.len(),
            rows.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn the_aws_secret_in_the_colon_form_ends_at_a_literal_escape() {
        let rows: [(&str, &str, &str); 4] = [
            (
                "AWS_SECRET_ACCESS_KEY: {v}\\nAWS_REGION: us-east-1",
                "[REDACTED:key_value]\\nAWS_REGION: us-east-1",
                "TKT-1 us-east-1",
            ),
            (
                "AWS_SECRET_ACCESS_KEY: {v}\\r\\nAWS_REGION: us-east-1",
                "[REDACTED:key_value]\\r\\nAWS_REGION: us-east-1",
                "TKT-1 us-east-1",
            ),
            (
                "{\"env\": \"AWS_ACCESS_KEY_ID: AKIAIOSFODNN7EXAMPLE\\naws_secret_access_key: {v}\\nregion: us-east-1\"}",
                "{\"env\": \"AWS_ACCESS_KEY_ID: [REDACTED:aws_key]\\n[REDACTED:key_value]\\nregion: us-east-1\"}",
                "TKT-1 {\"env\": \"AWS_ACCESS_KEY_ID: us-east-1\"}",
            ),
            (
                "aws_secret_access_key: {v}\\tdone",
                "[REDACTED:key_value]\\tdone",
                "TKT-1",
            ),
        ];
        let wrong = check_rows(&rows, "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_bare_colon_value_is_cut_at_an_escape_only_after_twelve_chars_with_a_digit_and_both_cases()
    {
        // a head that is no value is read on over the escape, as before: with more value after
        // it the whole run is the value, and with a `:` after it there is no match
        let rows: [(&str, &str, &str); 5] = [
            (
                "password: Hunter2-exam\\nhost: db",
                "[REDACTED:key_value]\\nhost: db",
                "TKT-1 db",
            ),
            (
                "password: Hunter2-exa\\nhost: db",
                "password: Hunter2-exa\\nhost: db",
                "TKT-1 password: Hunter2-exa\\nhost: db",
            ),
            (
                "password: Hunter2-exa\\nmple rest",
                "[REDACTED:key_value] rest",
                "TKT-1 rest",
            ),
            (
                "password: Hunter-example\\nZz9 rest",
                "[REDACTED:key_value] rest",
                "TKT-1 rest",
            ),
            (
                "password: hunter2-example\\nZz9 rest",
                "[REDACTED:key_value] rest",
                "TKT-1 rest",
            ),
        ];
        let wrong = check_rows(&rows, "");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}
