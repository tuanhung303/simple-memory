//! Each pattern runs on the regex crate, which has no backtrack limit to hit and no lookaround.
//! Every lookaround of Python's pattern is a check in code, and each one is exact because it
//! looks at one char or a fixed-length text (argued at each pattern below):
//! - A lookbehind on one char class goes into the regex as `(?:\A|[^X])`; the core of each such
//!   pattern starts with a char of X, so the core start is the match start (`\A`) or one char on.
//! - `\b` before a word char (`\bBearer`, `\bsig=`, ...) is `(?<!\w)`, with Python's `\w` from
//!   `py::str`, and `\s` is Python's `\s`.
//! - re.IGNORECASE letters are sre's sets (`py::str::ci_chars`): `i` also matches U+0130 and
//!   U+0131, `k` U+212A, `s` U+017F. A pattern that matches less than Python's leaks a secret.
//! - key_value and the `<private>` tag are scanners: Python's backtracking over them is
//!   deterministic once the spans are fixed (see `key_value_find`), and a scan is linear where
//!   Python's search is quadratic on a long run of key chars.

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
        let re = Regex::new(&format!("(?:\\A|[^{x_inner}])(?:{core})"))
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
            let s = if m.start() == 0 && (self.in_x)(first) {
                0
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
        // and all after `://` is the same from any start in the scheme
        (
            "url_credentials",
            Matcher::Plain(
                Regex::new(&format!(
                    "[A-Za-z][A-Za-z0-9+.-]*://(?:{u}*:[^{s_inner}/@\"']+@|[^{s_inner}/:@\"'.]*:{u}*[^{s_inner}/:@\"'0-9]{u}*(?:/{u}*)+@)",
                    u = format!("[^{s_inner}/:@\"']"),
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
/// `kv_bare` refuses when it holds a `/`. The end of the match.
fn kv_aws_secret(t: &str, i: usize) -> Option<usize> {
    let w = run(t, i, |c| c == ' ' || c == '\t').0;
    let (end, n) = run(t, w, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '+' | '=')
    });
    (n == 40 && !char_at(t, end).is_some_and(is_v)).then_some(end)
}

/// `(?!\[REDACTED:)(?![0-9]+(?!V))V{8,}["']?` at `w`: the end of the match.
fn kv_value(t: &str, w: usize) -> Option<usize> {
    if ci_at(t, w, &REDACTED_SEQ).is_some() {
        return None;
    }
    // `[0-9]+(?!V)`: digits are V, so only the longest digit run can end before a non-V char
    let (d_end, digits) = run(t, w, |c| c.is_ascii_digit());
    if digits > 0 && !char_at(t, d_end).is_some_and(is_v) {
        return None;
    }
    let (mut end, n) = run(t, w, is_v);
    if n < 8 {
        return None;
    }
    // a marker an earlier pattern left in the value (V holds no `]`): the value goes on past its
    // `]`, so the marker and the rest of the value go with it
    let mut from = w;
    while t.as_bytes().get(end) == Some(&b']') && t[from..end].contains("[REDACTED:") {
        from = end + 1;
        end = run(t, from, is_v).0;
    }
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
    if ci_at(t, w, &REDACTED_SEQ).is_some() {
        return None;
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
    // holds when the BV run from `w` has one
    let (mut digit, mut upper, mut lower, mut n) = (false, false, false, 0usize);
    let mut end = t.len();
    for (off, c) in t[w..].char_indices() {
        if !is_bare_v(c) {
            end = w + off;
            break;
        }
        digit |= c.is_ascii_digit();
        upper |= c.is_ascii_uppercase();
        lower |= c.is_ascii_lowercase();
        n += 1;
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

/// `_strip_private`: drop each `<private>` block through the close tag that balances it.
fn strip_private(
    text: &str,
    mut counts: Option<&mut Counts>,
    mut removed: Option<&mut Vec<String>>,
) -> String {
    let mut out = String::new();
    let mut pos = 0;
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
        out.push_str(&text[pos..tags[i].start]);
        out.push_str(PRIVATE_MARK);
        if let Some(r) = removed.as_deref_mut() {
            r.push(text[tags[i].start..tags[end_idx].end].to_string());
        }
        pos = tags[end_idx].end;
        if let Some(c) = counts.as_deref_mut() {
            *c.entry("private".into()).or_insert(0) += 1;
        }
        i = end_idx + 1;
    }
    out.push_str(&text[pos..]);
    out
}

/// `pat.subn(repl, text)` for a literal replacement.
fn subn(pat: &Matcher, repl: &str, text: &str) -> (String, i64) {
    let mut out = String::new();
    let mut last = 0;
    let mut n = 0;
    for (s, e) in pat.spans(text) {
        out.push_str(&text[last..s]);
        out.push_str(repl);
        last = e;
        n += 1;
    }
    out.push_str(&text[last..]);
    (out, n)
}

/// `redact_text(text, counts, removed)`.
pub fn redact_text(
    text: &str,
    mut counts: Option<&mut Counts>,
    removed: Option<&mut Vec<String>>,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = strip_private(text, counts.as_deref_mut(), removed);
    for (kind, pat) in SECRET_PATTERNS.iter() {
        let (next, n) = subn(pat, &format!("[REDACTED:{kind}]"), &out);
        out = next;
        if n != 0
            && let Some(c) = counts.as_deref_mut()
        {
            *c.entry((*kind).into()).or_insert(0) += n;
        }
    }
    out
}

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
    let mut redacted_refs = std::collections::HashSet::new();
    if title != item.title || body != item.body || evidence != item.evidence {
        let in_kept: std::collections::HashSet<String> =
            extract_refs(Some(&format!("{title}\n{body}\n{evidence}")))
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
    let kept: Vec<&String> = all_refs
        .iter()
        .filter(|r| !redacted_refs.contains(*r) && redact_text(r, None, None) == **r)
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
}
