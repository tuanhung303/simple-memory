use std::collections::HashSet;

#[cfg(test)]
use serde_json::Value;

use crate::core::fold::fold;
use crate::error::SmError;
use crate::py;
use crate::py::value::PyValue;

/// Does `x` match the pattern literal `c` under re.IGNORECASE (Unicode, not LOCALE)?
///
/// Python matches an uncased `c` exactly and a cased one by simple lowercase plus the extra-case
/// group. One rule covers both here: in Unicode 17.0.0 no char lowers to an uncased char other
/// than itself, and every extra-case member is cased (both checked over every code point with
/// `_sre.unicode_tolower` and `_sre.unicode_iscased`, lane C report).
fn ignorecase_eq(c: char, x: char) -> bool {
    let lo = py::str::sre_lower(c);
    let lx = py::str::sre_lower(x);
    lx == lo || py::str::extra_cases(lo).contains(&(lx as u32))
}

/// `re.search(r"(?<!\w)" + re.escape(key) + r"(?!\w)", text, flags) is not None`, with
/// re.IGNORECASE when `ignorecase`.
pub fn search_word_literal(key: &str, text: &str, ignorecase: bool) -> bool {
    let key: Vec<char> = key.chars().collect();
    let text: Vec<char> = text.chars().collect();
    if key.len() > text.len() {
        return false;
    }
    (0..=text.len() - key.len()).any(|i| {
        (i == 0 || !py::str::is_word(text[i - 1]))
            && key.iter().zip(&text[i..]).all(|(&k, &x)| {
                if ignorecase {
                    ignorecase_eq(k, x)
                } else {
                    k == x
                }
            })
            && text
                .get(i + key.len())
                .is_none_or(|&n| !py::str::is_word(n))
    })
}

/// `re.search(rf"(?<!\w){re.escape(code)}(?!\w)", text)`.
pub(crate) fn stands_as_word(text: &str, code: &str) -> bool {
    let mut at = 0;
    loop {
        if text[at..].starts_with(code) {
            let before = text[..at].chars().next_back();
            let after = text[at + code.len()..].chars().next();
            if !before.is_some_and(py::str::is_word) && !after.is_some_and(py::str::is_word) {
                return true;
            }
        }
        match text[at..].chars().next() {
            Some(c) => at += c.len_utf8(),
            None => return false,
        }
    }
}

/// Python `type(x).__name__` of a JSON value.
/// `keyword_aliases` as Python's dict holds them: str keys in order, each target any value.
pub type Aliases = Vec<(String, PyValue)>;

/// `for x in target`: the items Python's iteration gives (a dict gives its keys, bytes its ints),
/// or its TypeError.
fn iterate(target: &PyValue) -> Result<Vec<&PyValue>, SmError> {
    match target {
        PyValue::List(x) | PyValue::Tuple(x) | PyValue::Set(x) => Ok(x.iter().collect()),
        PyValue::Dict(p) => Ok(p.iter().map(|(k, _)| k).collect()),
        other => Err(SmError::type_(format!(
            "'{}' object is not iterable",
            other.tp_name()
        ))),
    }
}

/// `expand_aliases(query, aliases)`. A str target is one expansion; any other target is
/// iterated and each member goes through `str()`.
pub fn expand_aliases(query: &str, aliases: Option<&Aliases>) -> Result<Vec<String>, SmError> {
    let Some(aliases) = aliases.filter(|a| !query.is_empty() && !a.is_empty()) else {
        return Ok(Vec::new());
    };
    let q_folded = fold(Some(query));
    let mut expansions = Vec::new();
    for (key, target) in aliases {
        let key_clean = py::str::strip(key);
        if key_clean.is_empty() {
            continue;
        }
        if search_word_literal(key_clean, query, true)
            || search_word_literal(&fold(Some(key_clean)), &q_folded, true)
        {
            if let PyValue::Str(s) = target {
                expansions.push(s.clone());
                continue;
            }
            if let PyValue::Bytes(b) = target {
                expansions.extend(b.iter().map(u8::to_string));
                continue;
            }
            for x in iterate(target)? {
                x.str_digits_ok()?;
                expansions.push(x.py_str());
            }
        }
    }
    Ok(expansions)
}

/// `quote_fts_term(word)`: an ASCII word of 4+ letters becomes a prefix term.
pub fn quote_fts_term(word: &str) -> String {
    let escaped = word.replace('"', "\"\"");
    if py::str::len(word) >= 4 && word.is_ascii() && py::str::all_chars(word, py::str::isalpha) {
        format!("\"{escaped}\"*")
    } else {
        format!("\"{escaped}\"")
    }
}

/// `re.findall(r"\w+", text)`.
pub fn word_runs(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (py::str::is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(&text[s..i]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

/// Python's `str.upper()`: full mappings, no context, as Rust's `to_uppercase` (both Unicode 17).
fn upper(s: &str) -> String {
    s.chars().flat_map(char::to_uppercase).collect()
}

/// `build_fts_query(query, aliases, fold_terms=..., operator=...)`.
pub fn build_fts_query(
    query: Option<&str>,
    aliases: Option<&Aliases>,
    fold_terms: bool,
    operator: &str,
) -> Result<String, SmError> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(String::new());
    };
    let is_and = upper(py::str::strip(operator)) == "AND";
    let extra = if is_and {
        Vec::new()
    } else {
        expand_aliases(query, aliases)?
    };
    let full_text = if extra.is_empty() {
        query.to_string()
    } else {
        std::iter::once(query.to_string())
            .chain(extra)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let processed = if fold_terms {
        fold(Some(&full_text))
    } else {
        py::str::lower(&full_text)
    };
    let mut words = word_runs(&processed);
    if !fold_terms {
        words.retain(|w| *w != fold(Some(w)));
    }
    let mut quoted: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for w in words {
        let term = quote_fts_term(w);
        if seen.insert(term.clone()) {
            quoted.push(term);
        }
    }
    Ok(quoted.join(if is_and { " AND " } else { " OR " }))
}

/// A JSON object of aliases in Python's form (`py::json::protocol_value` of each target).
#[cfg(test)]
pub fn aliases_of_json(v: Option<&Value>) -> Result<Option<Aliases>, SmError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(
            m.iter()
                .map(|(k, t)| Ok((k.clone(), py::json::protocol_value(t)?)))
                .collect::<Result<_, SmError>>()?,
        )),
        Some(_) => Err(SmError::type_("parity: aliases must be a dict or None")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al(json: &str) -> Aliases {
        aliases_of_json(Some(&serde_json::from_str(json).unwrap()))
            .unwrap()
            .unwrap()
    }

    fn aliases() -> Aliases {
        al(r#"{"queue lag": "queue-worker", "db": ["sqlite", "fts5"]}"#)
    }

    #[test]
    fn prefix_aliases_and_quoting() {
        assert_eq!(quote_fts_term("deploy"), "\"deploy\"*");
        assert_eq!(quote_fts_term("may"), "\"may\"");
        assert_eq!(quote_fts_term("R5"), "\"R5\"");
        assert_eq!(quote_fts_term("qu\"ote"), "\"qu\"\"ote\"");
        assert_eq!(quote_fts_term("wörd"), "\"wörd\"");
        let a = aliases();
        assert_eq!(
            expand_aliases("R5 queue lag café", Some(&a)).unwrap(),
            vec!["queue-worker"]
        );
        assert!(expand_aliases("queuelag", Some(&a)).unwrap().is_empty());
        assert_eq!(
            build_fts_query(Some("R5 queue lag café"), Some(&a), true, "OR").unwrap(),
            "\"r5\" OR \"queue\"* OR \"lag\" OR \"cafe\"* OR \"worker\"*"
        );
        assert_eq!(
            build_fts_query(Some("café deploy"), None, false, "OR").unwrap(),
            "\"café\""
        );
        assert_eq!(
            build_fts_query(Some("plain deploy"), None, false, "OR").unwrap(),
            ""
        );
        assert_eq!(
            build_fts_query(Some("R5 queue lag"), Some(&a), true, "AND").unwrap(),
            "\"r5\" AND \"queue\"* AND \"lag\""
        );
        assert_eq!(
            build_fts_query(
                Some("AND OR NOT NEAR \"unclosed (paren) * : ^ -"),
                None,
                true,
                "OR"
            )
            .unwrap(),
            "\"and\" OR \"or\" OR \"not\" OR \"near\"* OR \"unclosed\"* OR \"paren\"*"
        );
        assert_eq!(build_fts_query(Some(""), None, true, "OR").unwrap(), "");
        assert_eq!(build_fts_query(None, None, true, "OR").unwrap(), "");
        // " and " strips and uppers to AND; fullwidth letters upper to fullwidth, so OR
        assert_eq!(
            build_fts_query(Some("a b"), Some(&a), true, " and ").unwrap(),
            "\"a\" AND \"b\""
        );
        assert_eq!(
            build_fts_query(Some("a b"), None, true, "ａｎｄ").unwrap(),
            "\"a\" OR \"b\""
        );
    }

    #[test]
    fn ignorecase_follows_python_re() {
        // Python 3.15.1: re.search("(?<!\\w)i(?!\\w)", x, re.I) matches I, U+0130 and U+0131;
        // k matches U+212A, s matches U+017F; sigma and final sigma share a group
        let hit = |key: &str, q: &str| {
            let a: Aliases = vec![(key.to_string(), PyValue::Str("hit".into()))];
            expand_aliases(q, Some(&a)).unwrap() == vec!["hit"]
        };
        assert!(hit("i", "x \u{130} y") && hit("i", "\u{131}") && hit("\u{131}", "I"));
        assert!(hit("k", "\u{212a}") && hit("s", "\u{17f}") && hit("\u{3c3}", "\u{3c2}"));
        // ß is cased under _sre (its uppercase has two chars), so ẞ (lowercase ß) matches it
        assert!(hit("\u{df}", "\u{1e9e}") && hit("\u{1e9e}", "\u{df}"));
        // boundaries use Python's \w: a mark (U+0301) and Pc (U+203F) are not word chars, ² is
        assert!(!hit("ab", "xab") && !hit("ab", "ab_") && !hit("ab", "\u{b2}ab"));
        assert!(hit("ab", "ab\u{301}") && hit("ab", "ab\u{203f}x"));
        assert!(
            hit("c++", "use c++ now") && hit("Đorđe Café", "dorde cafe") && hit("  k8s ", "K8S")
        );
        // U+0130 lowers to i on the raw query; the folded query has "xi", no boundary
        assert!(hit("i", "x\u{301}\u{130}"));
        // a key that strips to "" is skipped; "" would match at " x"'s start
        assert!(!hit("  ", " x"));
    }

    #[test]
    fn alias_targets_go_through_str() {
        let a = al(r#"{"n": [1, 2.5, true, null, "x", [1, "a"]], "  ": "blank", "d": {"k": 1}}"#);
        assert_eq!(
            expand_aliases("n d", Some(&a)).unwrap(),
            vec!["1", "2.5", "True", "None", "x", "[1, 'a']", "k"]
        );
        let bad = al(r#"{"k8s": 5}"#);
        assert_eq!(
            expand_aliases("k8s", Some(&bad)).unwrap_err().python_text(),
            "TypeError: 'int' object is not iterable"
        );
        assert!(expand_aliases("", Some(&a)).unwrap().is_empty());
        assert!(expand_aliases("n", Some(&Vec::new())).unwrap().is_empty());
        // Python: str() of each byte's int; an int past 4,300 digits has no str().
        let b: Aliases = vec![
            ("k".into(), PyValue::Bytes(b"A\x00".to_vec())),
            (
                "big".into(),
                PyValue::List(vec![PyValue::Int("9".repeat(4301))]),
            ),
        ];
        assert_eq!(expand_aliases("k", Some(&b)).unwrap(), vec!["65", "0"]);
        assert_eq!(
            expand_aliases("big", Some(&b)).unwrap_err().python_text(),
            "ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"
        );
        let tuple: Aliases = vec![(
            "t".into(),
            PyValue::Tuple(vec![PyValue::Float(1.0), PyValue::None]),
        )];
        assert_eq!(
            expand_aliases("t", Some(&tuple)).unwrap(),
            vec!["1.0", "None"]
        );
    }
}
