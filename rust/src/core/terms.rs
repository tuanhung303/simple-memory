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
    Ok(matched_aliases(query, aliases)?
        .into_iter()
        .flat_map(|(_, targets)| targets)
        .collect())
}

/// Each alias key the query names, in config order: (the stripped key, its expansions).
fn matched_aliases(
    query: &str,
    aliases: Option<&Aliases>,
) -> Result<Vec<(String, Vec<String>)>, SmError> {
    let Some(aliases) = aliases.filter(|a| !query.is_empty() && !a.is_empty()) else {
        return Ok(Vec::new());
    };
    let q_folded = fold(Some(query));
    let mut matched = Vec::new();
    for (key, target) in aliases {
        let key_clean = py::str::strip(key);
        if key_clean.is_empty() {
            continue;
        }
        if search_word_literal(key_clean, query, true)
            || search_word_literal(&fold(Some(key_clean)), &q_folded, true)
        {
            let expansions = match target {
                PyValue::Str(s) => vec![s.clone()],
                PyValue::Bytes(b) => b.iter().map(u8::to_string).collect(),
                _ => {
                    let mut out = Vec::new();
                    for x in iterate(target)? {
                        x.str_digits_ok()?;
                        out.push(x.py_str());
                    }
                    out
                }
            };
            matched.push((key_clean.to_string(), expansions));
        }
    }
    Ok(matched)
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

/// The quoted FTS terms of `text`, first occurrence kept: folded, or with `fold_terms` false
/// only the lowercased words that differ from their fold.
fn fts_terms(text: &str, fold_terms: bool) -> Vec<String> {
    let processed = if fold_terms {
        fold(Some(text))
    } else {
        py::str::lower(text)
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
    quoted
}

/// `a`, or `(a AND b ...)` for two or more terms.
fn and_alternative(terms: &[String]) -> String {
    match terms {
        [one] => one.clone(),
        _ => format!("({})", terms.join(" AND ")),
    }
}

/// The AND form of the folded `terms`: each alias key the query names whose terms are all in
/// `terms` becomes one group `(key OR target 1 OR ...)` at the place of its first term.
fn and_with_alias_groups(
    query: &str,
    aliases: Option<&Aliases>,
    terms: Vec<String>,
) -> Result<String, SmError> {
    // (key terms, group text)
    let mut groups: Vec<(Vec<String>, String)> = Vec::new();
    for (key, targets) in matched_aliases(query, aliases)? {
        let key_terms = fts_terms(&key, true);
        if !key_terms.iter().all(|t| terms.contains(t)) {
            continue;
        }
        let mut alts = vec![and_alternative(&key_terms)];
        for target in targets {
            let alt_terms = fts_terms(&target, true);
            if alt_terms.is_empty() {
                continue;
            }
            let alt = and_alternative(&alt_terms);
            if !alts.contains(&alt) {
                alts.push(alt);
            }
        }
        if alts.len() > 1 {
            groups.push((key_terms, format!("({})", alts.join(" OR "))));
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for term in terms {
        let mut grouped = false;
        for (key_terms, text) in &groups {
            if key_terms.contains(&term) {
                grouped = true;
                if !parts.contains(text) {
                    parts.push(text.clone());
                }
            }
        }
        if !grouped {
            parts.push(term);
        }
    }
    Ok(parts.join(" AND "))
}

/// `build_fts_query(query, aliases, fold_terms=..., operator=...)`. The OR form adds every
/// alias target's words; the folded AND form puts each named alias key in one OR group with its
/// targets. The accent AND form takes no alias: its rows only add to the folded AND rows.
pub fn build_fts_query(
    query: Option<&str>,
    aliases: Option<&Aliases>,
    fold_terms: bool,
    operator: &str,
) -> Result<String, SmError> {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(String::new());
    };
    if upper(py::str::strip(operator)) == "AND" {
        let terms = fts_terms(query, fold_terms);
        return if fold_terms {
            and_with_alias_groups(query, aliases, terms)
        } else {
            Ok(terms.join(" AND "))
        };
    }
    let extra = expand_aliases(query, aliases)?;
    let full_text = if extra.is_empty() {
        query.to_string()
    } else {
        std::iter::once(query.to_string())
            .chain(extra)
            .collect::<Vec<_>>()
            .join(" ")
    };
    Ok(fts_terms(&full_text, fold_terms).join(" OR "))
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
            "\"r5\" AND ((\"queue\"* AND \"lag\") OR (\"queue\"* AND \"worker\"*))"
        );
        assert_eq!(
            build_fts_query(Some("R5 lag queue"), Some(&a), true, "AND").unwrap(),
            "\"r5\" AND \"lag\" AND \"queue\"*"
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
    fn and_form_puts_each_named_alias_key_in_one_or_group() {
        let and = |q: &str, json: &str| build_fts_query(Some(q), Some(&al(json)), true, "AND");
        let k8s = r#"{"k8s": "kubernetes"}"#;
        assert_eq!(
            and("k8s deployq", k8s).unwrap(),
            "(\"k8s\" OR \"kubernetes\"*) AND \"deployq\"*"
        );
        assert_eq!(
            and("teams poll statusq", r#"{"teams poll": "ops-monitor"}"#).unwrap(),
            "((\"teams\"* AND \"poll\"*) OR (\"ops\" AND \"monitor\"*)) AND \"statusq\"*"
        );
        // the group stands where the key's first word stands; a list gives one alternative each
        assert_eq!(
            and("lock on DB", r#"{"db": ["sqlite", "fts5", "sqlite"]}"#).unwrap(),
            "\"lock\"* AND \"on\" AND (\"db\" OR \"sqlite\"* OR \"fts5\")"
        );
        // two keys with the same words give one group; a query word already a target stays
        assert_eq!(
            and(
                "K8S kubernetes x",
                r#"{"k8s": "kubernetes", "K8S": "kubernetes"}"#
            )
            .unwrap(),
            "(\"k8s\" OR \"kubernetes\"*) AND \"kubernetes\"* AND \"x\""
        );
        // no alias named, a target equal to the key, an empty target or a key with no word:
        // no group, the AND form of the query words alone
        assert_eq!(and("deployq x", k8s).unwrap(), "\"deployq\"* AND \"x\"");
        assert_eq!(
            and("k8s x", r#"{"k8s": "K8S"}"#).unwrap(),
            "\"k8s\" AND \"x\""
        );
        assert_eq!(
            and("k8s x", r#"{"k8s": ["", "++"]}"#).unwrap(),
            "\"k8s\" AND \"x\""
        );
        assert_eq!(
            and("c ++ d", r#"{"++": "plus"}"#).unwrap(),
            "\"c\" AND \"d\""
        );
        // σ matches ς only under IGNORECASE, so the key's "σ" is no query term: no group, as
        // every item the query words find must stay found
        assert_eq!(
            and("\u{3c2} x", r#"{"σ x": "tgt"}"#).unwrap(),
            "\"\u{3c2}\" AND \"x\""
        );
        // the accent form keeps no alias; the OR form does not change
        let accent = al(r#"{"k8s": "kubé"}"#);
        assert_eq!(
            build_fts_query(Some("café k8s"), Some(&accent), false, "AND").unwrap(),
            "\"café\""
        );
        assert_eq!(
            build_fts_query(Some("café k8s"), Some(&accent), false, "OR").unwrap(),
            "\"café\" OR \"kubé\""
        );
        assert_eq!(
            build_fts_query(Some("k8s deployq"), Some(&al(k8s)), true, "OR").unwrap(),
            "\"k8s\" OR \"deployq\"* OR \"kubernetes\"*"
        );
        // a target Python cannot iterate fails as the OR form fails
        assert_eq!(
            and("k8s x", r#"{"k8s": 5}"#).unwrap_err().python_text(),
            "TypeError: 'int' object is not iterable"
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
