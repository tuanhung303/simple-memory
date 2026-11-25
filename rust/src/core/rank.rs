use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::clock;
use crate::core::fold::{fold, has_accents};
use crate::core::item::{Item, parse_as_of_epoch};
use crate::core::refs::{RefsArg, extract_refs, split_refs};
use crate::core::tags::{VALID_KINDS, VALID_TAGS};
use crate::core::terms::{Aliases, expand_aliases, search_word_literal, stands_as_word, word_runs};
use crate::error::SmError;
use crate::py;

pub const WEIGHT_REFS: f64 = 10.0;
pub const WEIGHT_TITLE: f64 = 5.0;
pub const WEIGHT_TAGS: f64 = 3.0;
pub const WEIGHT_BODY: f64 = 1.0;
pub const FTS_CANDIDATE_LIMIT: i64 = 50;
pub const BIASED_CANDIDATE_LIMIT: i64 = 100;
pub const RECENCY_MAX_BOOST: f64 = 0.3;
pub const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;
pub const ACCENT_MATCH_BOOST: f64 = 0.25;
pub const CURRENT_REPO_BOOST: f64 = 0.05;
pub const LEGACY_PENALTY: f64 = 0.05;
pub const UNASSIGNED_PENALTY: f64 = 0.05;
pub const DEFAULT_QUERY_LIMIT: i64 = 4;
#[cfg(test)]
pub const MAX_SNIPPET_TOKENS: i64 = 600;
pub const SECONDS_PER_DAY: f64 = 86400.0;
pub const BIAS_RECENCY: &str = "recency";
pub const BIAS_RELEVANCE: &str = "relevance";
pub const BIAS_SCOPE_PREFIX: &str = "scope:";
pub const DEFAULT_BIAS: [&str; 8] = [
    BIAS_RECENCY,
    "fact",
    "decision",
    "learning",
    "executed",
    "verified",
    "decided",
    "client-stated",
];
/// Per tag or kind bias value, the size of the largest recency boost.
pub const BIAS_BOOST: f64 = 0.3;

/// `BIAS_VALUES`: recency, relevance, then the tags, then the kinds.
pub fn bias_values() -> Vec<&'static str> {
    let mut out = vec![BIAS_RECENCY, BIAS_RELEVANCE];
    out.extend(VALID_TAGS);
    out.extend(VALID_KINDS);
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub item: Item,
    pub fold_bm25: Option<f64>,
    pub raw_bm25: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Why {
    pub ref_hits: Vec<String>,
    pub phrase: bool,
    pub accent: bool,
    pub terms_matched: Vec<String>,
    pub terms_missing: Vec<String>,
    pub bias: Vec<String>,
    pub recent: bool,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    pub item: Item,
    pub why: Why,
}

/// Return the `<path>` of a canonical `scope:<path>` bias token when non-empty.
pub fn parse_scope_bias_path(token: &str) -> Option<&str> {
    token
        .strip_prefix(BIAS_SCOPE_PREFIX)
        .filter(|p| !p.is_empty())
}

/// True iff `bias` is exactly `["relevance"]` (suppresses automatic hook scope bias).
pub fn is_pure_relevance<S: AsRef<str>>(bias: &[S]) -> bool {
    bias.len() == 1 && bias[0].as_ref() == BIAS_RELEVANCE
}

fn scope_level_ok(level: &str) -> bool {
    !level.is_empty()
        && level
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The loop of `parse_bias` over `(str(v), repr(v))` pairs.
fn parse_bias_items<I: IntoIterator<Item = (String, String)>>(
    raw: I,
) -> Result<Vec<String>, SmError> {
    let allowed = bias_values();
    let mut out: Vec<String> = Vec::new();
    for (text, repr) in raw {
        let trimmed = py::str::strip(&text);
        if trimmed.is_empty() {
            continue;
        }
        let entry = if trimmed
            .as_bytes()
            .get(..BIAS_SCOPE_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(BIAS_SCOPE_PREFIX.as_bytes()))
        {
            let path = py::str::strip(&trimmed[BIAS_SCOPE_PREFIX.len()..]);
            if path.is_empty() || !path.split('/').all(scope_level_ok) {
                return Err(SmError::value(format!(
                    "Invalid bias {repr}; scope path levels must match [A-Za-z0-9_.-]+"
                )));
            }
            format!("{BIAS_SCOPE_PREFIX}{path}")
        } else {
            let cleaned = py::str::lower(trimmed);
            if !allowed.contains(&cleaned.as_str()) {
                return Err(SmError::value(format!(
                    "Invalid bias {repr}; expected one of {}, or scope:<path>",
                    allowed.join(", ")
                )));
            }
            cleaned
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    if out.is_empty() {
        out = DEFAULT_BIAS.iter().map(|s| (*s).to_string()).collect();
    }
    Ok(out)
}

/// `parse_bias(values)` for a str: split on ",", strip, lower, dedupe in order; nothing left
/// gives `DEFAULT_BIAS`.
pub fn parse_bias(values: &str) -> Result<Vec<String>, SmError> {
    parse_bias_items(values.split(',').map(|v| (v.to_string(), py::str::repr(v))))
}

/// `parse_bias(values)` for an iterable of str, or None (the default).
pub fn parse_bias_list(values: Option<&[String]>) -> Result<Vec<String>, SmError> {
    parse_bias_items(
        values
            .unwrap_or(&[])
            .iter()
            .map(|v| (v.clone(), py::str::repr(v))),
    )
}

pub fn bias_widens_pool<S: AsRef<str>>(bias: &[S]) -> bool {
    bias.iter()
        .any(|b| b.as_ref() != BIAS_RECENCY && b.as_ref() != BIAS_RELEVANCE)
}

/// query_terms folds case, so these cover AND, Or and the like.
const FTS_OPERATOR_WORDS: [&str; 4] = ["and", "or", "not", "near"];

/// `is_hint_term(term)`: 2 or more characters, no FTS operator word.
pub fn is_hint_term(term: &str) -> bool {
    py::str::len(term) > 1 && !FTS_OPERATOR_WORDS.contains(&term)
}

/// `query_terms(query)`: folded query words in order, deduped.
pub fn query_terms(query: &str) -> Vec<String> {
    let folded = fold(Some(query));
    let mut out: Vec<String> = Vec::new();
    for w in word_runs(&folded) {
        if !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Split a raw query into `(positive_query, excluded)` folded terms/phrases.
///
/// Unquoted `-<word>` / `-"<phrase>"` tokens become exclusions only when at least one positive
/// token exists (preserving literal leading-dash queries like `sm query -- -R6`).
/// Standalone unquoted uppercase `OR` / `AND` tokens are stripped as boolean noise.
pub fn parse_query_exclusions(query: &str) -> (String, Vec<String>) {
    enum Token<'a> {
        Positive(&'a str),
        Exclusion { raw: &'a str, folded: String },
    }
    let mut tokens: Vec<Token<'_>> = Vec::new();
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = query[i..].chars().next().expect("i < bytes.len()");
        if py::str::isspace(ch) {
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        if query[i..].starts_with("-\"") {
            i += 2;
            let inner_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                let c = query[i..].chars().next().expect("i < bytes.len()");
                i += c.len_utf8();
            }
            let inner_end = i;
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
            }
            let raw = &query[start..i];
            let folded = word_runs(&fold(Some(&query[inner_start..inner_end]))).join(" ");
            if folded.is_empty() {
                tokens.push(Token::Positive(raw));
            } else {
                tokens.push(Token::Exclusion { raw, folded });
            }
        } else if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                let c = query[i..].chars().next().expect("i < bytes.len()");
                i += c.len_utf8();
            }
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
            }
            tokens.push(Token::Positive(&query[start..i]));
        } else {
            while i < bytes.len() {
                let c = query[i..].chars().next().expect("i < bytes.len()");
                if py::str::isspace(c) {
                    break;
                }
                i += c.len_utf8();
            }
            let tok = &query[start..i];
            if tok == "OR" || tok == "AND" {
                continue;
            }
            if let Some(rest) = tok.strip_prefix('-')
                && rest.chars().next().is_some_and(py::str::is_word)
            {
                let folded = word_runs(&fold(Some(rest))).join(" ");
                if !folded.is_empty() {
                    tokens.push(Token::Exclusion { raw: tok, folded });
                    continue;
                }
            }
            tokens.push(Token::Positive(tok));
        }
    }

    let has_positive = tokens.iter().any(|t| matches!(t, Token::Positive(_)));
    if !has_positive {
        let raw: Vec<&str> = tokens
            .into_iter()
            .map(|t| match t {
                Token::Positive(r) | Token::Exclusion { raw: r, .. } => r,
            })
            .collect();
        return (raw.join(" "), Vec::new());
    }

    let mut pos = Vec::new();
    let mut excl = Vec::new();
    for t in tokens {
        match t {
            Token::Positive(r) => pos.push(r),
            Token::Exclusion { folded, .. } => {
                if !excl.contains(&folded) {
                    excl.push(folded);
                }
            }
        }
    }
    (pos.join(" "), excl)
}

/// True when `item`'s folded `refs + title + tags + body` contains any excluded word or phrase.
pub fn item_matches_exclusion(item: &Item, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let haystack = haystack_words(item);
    let sub_haystack = split_underscores(&haystack);
    let mut joined_phrase: Option<String> = None;
    for ex in excluded {
        if ex.is_empty() {
            continue;
        }
        if ex.contains(' ') {
            let norm: Vec<&str> = ex.split([' ', '_']).filter(|p| !p.is_empty()).collect();
            if norm.is_empty() {
                continue;
            }
            let phrase_hay =
                joined_phrase.get_or_insert_with(|| format!(" {} ", sub_haystack.join(" ")));
            if phrase_hay.contains(&format!(" {} ", norm.join(" "))) {
                return true;
            }
        } else if ex.contains('_') {
            if haystack.iter().any(|hw| hw == ex) {
                return true;
            }
            let parts: Vec<&str> = ex.split('_').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2 {
                let phrase_hay =
                    joined_phrase.get_or_insert_with(|| format!(" {} ", sub_haystack.join(" ")));
                if phrase_hay.contains(&format!(" {} ", parts.join(" "))) {
                    return true;
                }
            } else if let Some(&only) = parts.first()
                && simple_word_matches(only, &sub_haystack)
            {
                return true;
            }
        } else if simple_word_matches(ex, &sub_haystack) {
            return true;
        }
    }
    false
}

/// Map each folded query term covered by an active alias key to the folded alias target words
/// that differ from that key's own words.
pub fn term_alias_targets(
    query: &str,
    aliases: Option<&Aliases>,
) -> Result<HashMap<String, Vec<String>>, SmError> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let Some(aliases) = aliases.filter(|a| !query.is_empty() && !a.is_empty()) else {
        return Ok(map);
    };
    for entry in aliases {
        let single = vec![entry.clone()];
        let exps = expand_aliases(query, Some(&single))?;
        if exps.is_empty() {
            continue;
        }
        let key_folded = fold(Some(py::str::strip(&entry.0)));
        let key_words: Vec<String> = word_runs(&key_folded)
            .into_iter()
            .map(String::from)
            .collect();
        let mut target_words: Vec<String> = Vec::new();
        for exp in exps {
            let exp_folded = fold(Some(&exp));
            for w in word_runs(&exp_folded) {
                if !key_words.iter().any(|kw| kw == w) && !target_words.iter().any(|tw| tw == w) {
                    target_words.push(w.to_string());
                }
            }
        }
        if target_words.is_empty() {
            continue;
        }
        for kw in key_words {
            let slot = map.entry(kw).or_default();
            for tw in &target_words {
                if !slot.contains(tw) {
                    slot.push(tw.clone());
                }
            }
        }
    }
    Ok(map)
}

fn haystack_words(item: &Item) -> Vec<String> {
    let text = fold(Some(&format!(
        "{} {} {} {}",
        item.refs, item.title, item.tags, item.body
    )));
    word_runs(&text).into_iter().map(String::from).collect()
}

fn simple_word_matches(qw: &str, haystack: &[&str]) -> bool {
    if py::str::len(qw) >= 4 && qw.is_ascii() && py::str::all_chars(qw, py::str::isalpha) {
        return haystack.iter().any(|hw| hw.starts_with(qw));
    }
    haystack.contains(&qw)
}

fn split_underscores(words: &[String]) -> Vec<&str> {
    words
        .iter()
        .flat_map(|hw| hw.split('_').filter(|p| !p.is_empty()))
        .collect()
}

fn word_matches_strict(qw: &str, haystack: &[String]) -> bool {
    if qw.contains('_') {
        let parts: Vec<&str> = qw.split('_').filter(|p| !p.is_empty()).collect();
        if !parts.is_empty() {
            let sub_haystack = split_underscores(haystack);
            return parts.iter().all(|p| simple_word_matches(p, &sub_haystack));
        }
    }
    let whole: Vec<&str> = haystack.iter().map(String::as_str).collect();
    simple_word_matches(qw, &whole)
}

fn word_matches(qw: &str, haystack: &[String]) -> bool {
    let sub_haystack = split_underscores(haystack);
    if qw.contains('_') {
        let parts: Vec<&str> = qw.split('_').filter(|p| !p.is_empty()).collect();
        if !parts.is_empty() {
            return parts.iter().all(|p| simple_word_matches(p, &sub_haystack));
        }
    }
    simple_word_matches(qw, &sub_haystack)
}

/// `terms_held(item, terms, alias_map)`: prefix for ASCII words of 4+ letters, snake_case
/// sub-parts matching FTS5 unicode61, or active alias target words.
fn terms_held(
    item: &Item,
    terms: &[String],
    alias_map: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let haystack = haystack_words(item);
    terms
        .iter()
        .filter(|t| {
            word_matches(t, &haystack)
                || alias_map
                    .get(t.as_str())
                    .is_some_and(|targets| targets.iter().any(|w| word_matches(w, &haystack)))
        })
        .cloned()
        .collect()
}

/// `_norm_ws(text)` = `re.sub(r"\s+", " ", text.strip())`: after strip every whitespace run is
/// inside, so this is `" ".join(text.split())`.
fn norm_ws(text: &str) -> String {
    py::str::split(text).join(" ")
}

/// Python `max(0.0, x)`: x only when it is greater, so nan gives 0.0.
fn max0(x: f64) -> f64 {
    if x > 0.0 { x } else { 0.0 }
}

/// `compute_recency_boost(as_of, now_epoch)`.
pub fn compute_recency_boost(as_of: &str, now_epoch: Option<f64>) -> Result<f64, SmError> {
    let now_ts = now_epoch.unwrap_or_else(clock::now_epoch);
    let item_ts = parse_as_of_epoch(Some(as_of))?;
    if item_ts <= 0.0 {
        return Ok(0.0);
    }
    let age_days = max0((now_ts - item_ts) / SECONDS_PER_DAY);
    // black_box keeps LLVM from turning pow(0.5, x) into exp2(-x), which differs from the libm
    // pow Python's `**` calls by 1 ulp on some inputs (bridge: 5 of 17,116 fuzz cases)
    Ok(RECENCY_MAX_BOOST * std::hint::black_box(0.5f64).powf(age_days / RECENCY_HALF_LIFE_DAYS))
}

pub fn is_legacy_unreorganized(item: &Item) -> bool {
    item.id.starts_with("legacy:") || item.source.starts_with("legacy:") || item.tag == "n/a"
}

/// Each alias key the query names whose words are all query words, as the AND form's groups
/// (`build_fts_query`): (the key's folded words, the folded words of each target).
type AliasGroup = (Vec<String>, Vec<Vec<String>>);

fn alias_word_groups(
    query: &str,
    aliases: Option<&Aliases>,
    q_words: &[String],
) -> Result<Vec<AliasGroup>, SmError> {
    let mut groups = Vec::new();
    for entry in aliases.into_iter().flatten() {
        let exps = expand_aliases(query, Some(&vec![entry.clone()]))?;
        let key_words: Vec<String> = word_runs(&fold(Some(py::str::strip(&entry.0))))
            .into_iter()
            .map(String::from)
            .collect();
        if !key_words.iter().all(|w| q_words.contains(w)) {
            continue;
        }
        let targets: Vec<Vec<String>> = exps
            .iter()
            .map(|e| {
                word_runs(&fold(Some(e)))
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .filter(|t| !t.is_empty())
            .collect();
        groups.push((key_words, targets));
    }
    Ok(groups)
}

/// Whether `item` holds every query word; for the words an alias group covers, every key word
/// or every word of one target, each word matched as its FTS term (a prefix for an ASCII word
/// of 4+ letters).
fn item_matches_all_query_words(item: &Item, q_words: &[String], groups: &[AliasGroup]) -> bool {
    if q_words.is_empty() {
        return true;
    }
    let haystack = haystack_words(item);
    let holds = |words: &[String]| words.iter().all(|w| word_matches_strict(w, &haystack));
    q_words
        .iter()
        .filter(|qw| !groups.iter().any(|(key, _)| key.contains(qw)))
        .all(|qw| word_matches_strict(qw, &haystack))
        && groups
            .iter()
            .all(|(key, targets)| holds(key) || targets.iter().any(|t| holds(t)))
}

/// Whether `item` matches any `[scopes]` matcher (exact parity with SQL `matcher_clauses` + `sm_word`).
pub fn item_matches_scope_matchers(item: &Item, matchers: &[String]) -> bool {
    matchers.iter().any(|m| {
        if let Some(r) = m.strip_prefix("repo:") {
            item.repo == r
        } else {
            [
                item.title.as_str(),
                item.body.as_str(),
                item.refs.as_str(),
                item.tags.as_str(),
            ]
            .iter()
            .any(|f| stands_as_word(f, m))
        }
    })
}

/// Keyword arguments of `score_candidates` and `rank_candidates`.
#[derive(Clone, Debug)]
pub struct ScoreOpts<'a> {
    pub query_refs: Option<&'a [String]>,
    pub current_repo: Option<&'a str>,
    pub recent: bool,
    pub now_epoch: Option<f64>,
    pub bias: &'a [String],
    pub scope_biases: &'a [(String, Vec<String>)],
    /// Only the first `explain` results get terms_matched and terms_missing; None: all. A
    /// negative value slices from the end, as `scored[:explain]` does.
    pub explain: Option<i64>,
    pub aliases: Option<&'a Aliases>,
}

/// Python's `<` on two floats of a key tuple: equal unless one is less (nan is never less).
fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// `score_candidates(candidates, query, ...)`: candidates in rank order, each with why it ranks
/// there. Sort key: ref hits, phrase hit, accent hit, the score, then date and id, all descending
/// (a stable sort, as Python's `reverse=True`). With recent, newest first.
pub fn score_candidates(
    candidates: &[Candidate],
    query: &str,
    opts: &ScoreOpts,
) -> Result<Vec<Scored>, SmError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let terms = query_terms(query);
    let alias_map = term_alias_targets(query, opts.aliases)?;
    if opts.recent {
        let groups = alias_word_groups(query, opts.aliases, &terms)?;
        let all_matched: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| item_matches_all_query_words(&c.item, &terms, &groups))
            .collect();
        let pool: Vec<&Candidate> = if all_matched.is_empty() {
            candidates.iter().collect()
        } else {
            all_matched
        };
        let mut keyed: Vec<(f64, &Candidate)> = pool
            .into_iter()
            .map(|c| Ok((parse_as_of_epoch(Some(&c.item.as_of))?, c)))
            .collect::<Result<_, SmError>>()?;
        keyed.sort_by(|(ta, a), (tb, b)| cmp_f64(*tb, *ta).then_with(|| b.item.id.cmp(&a.item.id)));
        let scored = keyed
            .into_iter()
            .map(|(_, c)| Scored {
                item: c.item.clone(),
                why: Why::default(),
            })
            .collect();
        return Ok(explain_terms(scored, &terms, &alias_map, opts.explain));
    }

    let now_ts = opts.now_epoch.unwrap_or_else(clock::now_epoch);
    let use_recency = opts.bias.iter().any(|b| b == BIAS_RECENCY);
    let q_refs: Vec<String> = match opts.query_refs {
        Some(r) => r.to_vec(),
        None => extract_refs(Some(query)),
    };
    let mut q_ref_folded: HashSet<String> = q_refs
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| fold(Some(r)))
        .collect();
    let q_clean = norm_ws(query);
    if !q_clean.is_empty() && !q_clean.contains(' ') {
        q_ref_folded.insert(fold(Some(&q_clean)));
    }

    let q_has_accents = has_accents(Some(query));
    let q_folded_phrase = word_runs(&fold(Some(query))).join(" ");
    let q_phrase_needle = format!(" {q_folded_phrase}");
    let q_raw_phrase = py::str::lower(&q_clean);
    let q_accented_words: Vec<&str> = word_runs(&q_raw_phrase)
        .into_iter()
        .filter(|w| *w != fold(Some(w)))
        .collect();

    let raw_rels: Vec<f64> = candidates
        .iter()
        .map(|c| {
            // `max(0.0, -(c.fold_bm25 or 0.0))`: `or` turns -0.0 into 0.0, which max0 does anyway
            let pos_fold = max0(-c.fold_bm25.unwrap_or(0.0));
            let pos_raw = max0(-c.raw_bm25.unwrap_or(0.0));
            pos_fold + pos_raw
        })
        .collect();
    let max_rel = raw_rels
        .iter()
        .skip(1)
        .fold(raw_rels[0], |m, &x| if x > m { x } else { m });

    struct Key {
        active: u8,
        ref_hits: usize,
        phrase: u8,
        accent: u8,
        score: f64,
        epoch: f64,
        id: String,
    }
    let mut keyed: Vec<(Key, Scored)> = Vec::with_capacity(candidates.len());
    for (idx, cand) in candidates.iter().enumerate() {
        let item = &cand.item;
        let ref_tokens = split_refs(Some(&RefsArg::Str(&item.refs)));
        let (hit_refs, ref_hits) = if q_ref_folded.is_empty() {
            (Vec::new(), 0)
        } else {
            let folded: Vec<String> = ref_tokens.iter().map(|r| fold(Some(r))).collect();
            let hits: Vec<String> = ref_tokens
                .iter()
                .zip(&folded)
                .filter(|(_, f)| q_ref_folded.contains(*f))
                .map(|(r, _)| r.clone())
                .collect();
            let distinct: HashSet<&String> = folded
                .iter()
                .filter(|f| q_ref_folded.contains(*f))
                .collect();
            (hits, distinct.len())
        };

        let raw_fields = py::str::lower(&norm_ws(&format!(
            "{} {} {} {}",
            item.refs, item.title, item.tags, item.body
        )));
        let title_body_fold = format!(
            " {}",
            word_runs(&fold(Some(&format!("{} {}", item.title, item.body)))).join(" ")
        );

        let matches_all_accented_words = q_accented_words
            .iter()
            .all(|w| search_word_literal(w, &raw_fields, false));
        let fold_phrase_hit = py::str::len(&q_folded_phrase) >= 2
            && title_body_fold.contains(&q_phrase_needle)
            && matches_all_accented_words;

        let matches_accented_word = !q_accented_words.is_empty()
            && q_accented_words
                .iter()
                .any(|w| search_word_literal(w, &raw_fields, false));
        let has_raw_accent_match = cand.raw_bm25.is_some() && matches_accented_word;
        let accent_hit = q_has_accents && has_raw_accent_match;

        let bm25_scaled = if max_rel > 0.0 {
            raw_rels[idx] / max_rel
        } else {
            1.0
        };
        let recency = if use_recency {
            compute_recency_boost(&item.as_of, Some(now_ts))?
        } else {
            0.0
        };
        // recency, relevance, and scope:<path> never equal a tag or a kind, so only tag and kind values match
        let mut bias_hits: Vec<String> = opts
            .bias
            .iter()
            .filter(|b| **b == item.tag || **b == item.kind)
            .cloned()
            .collect();
        for (label, matchers) in opts.scope_biases {
            if item_matches_scope_matchers(item, matchers) && !bias_hits.contains(label) {
                bias_hits.push(label.clone());
            }
        }
        let accent_boost = if accent_hit { ACCENT_MATCH_BOOST } else { 0.0 };
        let repo_boost = if opts
            .current_repo
            .is_some_and(|r| !r.is_empty() && item.repo == r)
        {
            CURRENT_REPO_BOOST
        } else {
            0.0
        };
        let legacy_pen = if is_legacy_unreorganized(item) {
            LEGACY_PENALTY
        } else {
            0.0
        };
        let unassigned_pen = if item.repo == "unassigned" {
            UNASSIGNED_PENALTY
        } else {
            0.0
        };
        let score = bm25_scaled
            * (1.0 + recency + BIAS_BOOST * bias_hits.len() as f64 + accent_boost)
            + repo_boost
            - legacy_pen
            - unassigned_pen;
        let why = Why {
            ref_hits: hit_refs,
            phrase: fold_phrase_hit,
            accent: accent_hit,
            bias: bias_hits,
            recent: recency >= RECENCY_MAX_BOOST / 2.0,
            score: py::float::round_ndigits(score, 4)?,
            ..Why::default()
        };
        let key = Key {
            active: u8::from(item.status == "active"),
            ref_hits,
            phrase: u8::from(fold_phrase_hit),
            accent: u8::from(accent_hit),
            score,
            epoch: parse_as_of_epoch(Some(&item.as_of))?,
            id: item.id.clone(),
        };
        keyed.push((
            key,
            Scored {
                item: item.clone(),
                why,
            },
        ));
    }

    keyed.sort_by(|(a, _), (b, _)| {
        b.active
            .cmp(&a.active)
            .then(b.ref_hits.cmp(&a.ref_hits))
            .then(b.phrase.cmp(&a.phrase))
            .then(b.accent.cmp(&a.accent))
            .then(cmp_f64(b.score, a.score))
            .then(cmp_f64(b.epoch, a.epoch))
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(explain_terms(
        keyed.into_iter().map(|(_, s)| s).collect(),
        &terms,
        &alias_map,
        opts.explain,
    ))
}

fn explain_terms(
    mut scored: Vec<Scored>,
    terms: &[String],
    alias_map: &HashMap<String, Vec<String>>,
    explain: Option<i64>,
) -> Vec<Scored> {
    let n = scored.len() as i64;
    let upto = match explain {
        None => n,
        Some(e) if e < 0 => (n + e).max(0),
        Some(e) => e.min(n),
    } as usize;
    for s in scored.iter_mut().take(upto) {
        s.why.terms_matched = terms_held(&s.item, terms, alias_map);
        s.why.terms_missing = terms
            .iter()
            .filter(|t| !s.why.terms_matched.contains(t) && is_hint_term(t))
            .cloned()
            .collect();
    }
    scored
}

/// `rank_candidates(candidates, query, ...)`: the items of `score_candidates`, in order.
#[cfg(test)]
pub fn rank_candidates(
    candidates: &[Candidate],
    query: &str,
    opts: &ScoreOpts,
) -> Result<Vec<Item>, SmError> {
    let opts = ScoreOpts {
        explain: None,
        ..opts.clone()
    };
    Ok(score_candidates(candidates, query, &opts)?
        .into_iter()
        .map(|s| s.item)
        .collect())
}

fn strip_line_suffix(s: &str) -> &str {
    let Some((base, suffix)) = s.rsplit_once(':') else {
        return s;
    };
    if base.is_empty() || suffix.is_empty() {
        return s;
    }
    let is_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let has_line_suffix = is_digits(suffix)
        || suffix
            .split_once('-')
            .is_some_and(|(a, b)| is_digits(a) && is_digits(b));
    if has_line_suffix { base } else { s }
}

fn find_earliest_match_span(
    text: &str,
    folded_terms: &[String],
    folded_refs: &[String],
) -> Option<(usize, usize)> {
    if folded_terms.is_empty() && folded_refs.is_empty() {
        return None;
    }
    let check_tok = |tok: &str| -> bool {
        let t_fold = fold(Some(tok));
        if !folded_terms.is_empty() {
            let runs: Vec<String> = word_runs(&t_fold).into_iter().map(String::from).collect();
            if folded_terms.iter().any(|term| word_matches(term, &runs)) {
                return true;
            }
        }
        if !folded_refs.is_empty() {
            let trimmed = t_fold.trim_matches(|c| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '\'' | '"' | '`'
                )
            });
            let base = strip_line_suffix(trimmed);
            if folded_refs
                .iter()
                .any(|r| r == &t_fold || r == trimmed || r == base)
            {
                return true;
            }
        }
        false
    };
    let mut tok_start: Option<(usize, usize)> = None;
    let mut char_idx = 0usize;
    for (byte_idx, c) in text.char_indices() {
        if py::str::isspace(c) {
            if let Some((b_start, c_start)) = tok_start.take()
                && check_tok(&text[b_start..byte_idx])
            {
                return Some((c_start, char_idx));
            }
        } else if tok_start.is_none() {
            tok_start = Some((byte_idx, char_idx));
        }
        char_idx += 1;
    }
    if let Some((b_start, c_start)) = tok_start
        && check_tok(&text[b_start..])
    {
        return Some((c_start, char_idx));
    }
    None
}

fn snap_win_start(chars: &[char], win_start: usize, start_char: usize) -> usize {
    if win_start == 0 || win_start >= start_char {
        return win_start;
    }
    if py::str::isspace(chars[win_start - 1]) && !py::str::isspace(chars[win_start]) {
        return win_start;
    }
    let limit = (win_start + 16).min(start_char);
    let mut i = win_start;
    while i <= limit && !py::str::isspace(chars[i]) {
        i += 1;
    }
    while i <= limit && py::str::isspace(chars[i]) {
        i += 1;
    }
    if i <= limit && !py::str::isspace(chars[i]) {
        i
    } else {
        win_start
    }
}

/// `text` when it fits in `cap` characters, else its first `cap - 3` characters, right-trimmed,
/// plus `...`.
pub fn clip(text: &str, cap: usize) -> String {
    if py::str::len(text) <= cap {
        return text.to_string();
    }
    format!(
        "{}...",
        py::str::rstrip(py::str::slice(text, None, Some(cap as isize - 3)))
    )
}

/// Build a snippet of at most `cap` characters, centering on the earliest matched term or ref
/// when it falls past the prefix cut; short and front matches keep the prefix cut.
pub fn match_snippet(text: &str, terms: &[String], ref_hits: &[String], cap: usize) -> String {
    let total_chars = py::str::len(text);
    if total_chars <= cap {
        return text.to_string();
    }
    if cap <= 3 {
        return py::str::slice(text, None, Some(cap as isize)).to_string();
    }
    let prefix_cut = || clip(text, cap);
    if cap <= 6 {
        return prefix_cut();
    }
    let folded_terms: Vec<String> = terms
        .iter()
        .map(|t| fold(Some(t)))
        .filter(|t| !t.is_empty())
        .collect();
    let folded_refs: Vec<String> = ref_hits
        .iter()
        .map(|r| fold(Some(r)))
        .filter(|r| !r.is_empty())
        .collect();
    let Some((start_char, end_char)) = find_earliest_match_span(text, &folded_terms, &folded_refs)
    else {
        return prefix_cut();
    };
    if end_char <= cap - 3 {
        return prefix_cut();
    }

    let inner_both = cap - 6;
    let lead = inner_both / 3;
    let mut win_start = start_char.saturating_sub(lead);
    if win_start == 0 {
        return prefix_cut();
    }
    if win_start + (cap - 3) >= total_chars {
        win_start = total_chars - (cap - 3);
    }
    let chars: Vec<char> = text.chars().collect();
    win_start = snap_win_start(&chars, win_start, start_char);

    let win_end = if win_start + (cap - 3) >= total_chars {
        total_chars
    } else {
        win_start + inner_both
    };
    let mut piece = py::str::slice(text, Some(win_start as isize), Some(win_end as isize));
    if win_start > 0 {
        piece = py::str::lstrip(piece);
    }
    if win_end < total_chars {
        piece = py::str::rstrip(piece);
    }
    let mut out = String::new();
    if win_start > 0 {
        out.push_str("...");
    }
    out.push_str(piece);
    if win_end < total_chars {
        out.push_str("...");
    }
    assert!(py::str::len(&out) <= cap);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::item::epoch_to_as_of;

    const NOW: f64 = 1_800_000_000.0;
    const DAY: f64 = 86400.0;

    fn item(id: &str, repo: &str, title: &str, body: &str, tag: &str, as_of: &str) -> Item {
        Item {
            as_of: as_of.into(),
            ..Item::bare(id, repo, "fact", title, body, tag)
        }
        .post_init()
    }

    fn cand(item: Item, fold_bm25: f64, raw_bm25: Option<f64>) -> Candidate {
        Candidate {
            item,
            fold_bm25: Some(fold_bm25),
            raw_bm25,
        }
    }

    fn opts(bias: &[String]) -> ScoreOpts<'_> {
        ScoreOpts {
            query_refs: None,
            current_repo: None,
            recent: false,
            now_epoch: Some(NOW),
            bias,
            scope_biases: &[],
            explain: None,
            aliases: None,
        }
    }

    fn bias(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    // Repo a, "Kiwi note", age in days before NOW.
    fn kiwi(id: &str, tag: &str, kind: &str, age_days: f64, refs: &str) -> Item {
        let as_of = epoch_to_as_of(NOW - age_days * DAY).unwrap();
        Item {
            refs: refs.into(),
            as_of,
            ..Item::bare(id, "a", kind, "Kiwi note", "Kiwi note body", tag)
        }
        .post_init()
    }

    // A: new, derived, bm25 10 (scaled 1.0). B: 90 days old, executed, bm25 8 (scaled 0.8).
    // Recency boost: A 0.3; B 0.3 * 0.5 ** 3 = 0.0375.
    fn pair() -> Vec<Candidate> {
        vec![
            cand(kiwi("m_a", "derived", "fact", 0.0, ""), -10.0, None),
            cand(kiwi("m_b", "executed", "fact", 90.0, ""), -8.0, None),
        ]
    }

    fn ranked(cands: &[Candidate], b: &[&str]) -> Vec<(String, f64)> {
        let b = bias(b);
        score_candidates(cands, "kiwi note", &opts(&b))
            .unwrap()
            .into_iter()
            .map(|s| (s.item.id, s.why.score))
            .collect()
    }

    fn pairs(v: &[(&str, f64)]) -> Vec<(String, f64)> {
        v.iter().map(|(a, b)| ((*a).to_string(), *b)).collect()
    }

    #[test]
    fn parse_bias_matches_python() {
        // values from simple_memory.core.rank.parse_bias under Python 3.15.1
        assert_eq!(
            parse_bias("").unwrap(),
            DEFAULT_BIAS.map(String::from).to_vec()
        );
        assert_eq!(
            parse_bias(" Decided ,decided,,task").unwrap(),
            vec!["decided", "task"]
        );
        assert_eq!(
            parse_bias(" Executed, recency,executed ").unwrap(),
            vec!["executed", "recency"]
        );
        assert_eq!(
            parse_bias_list(None).unwrap(),
            DEFAULT_BIAS.map(String::from).to_vec()
        );
        assert_eq!(
            parse_bias_list(Some(&bias(&["decision", "verified"]))).unwrap(),
            vec!["decision", "verified"]
        );
        let err = parse_bias("executed,newest").unwrap_err();
        assert_eq!(err.class, "ValueError");
        assert_eq!(
            err.msg,
            "Invalid bias 'newest'; expected one of recency, relevance, executed, verified, decided, \
             client-stated, derived, assumed, prior, planned, fact, decision, learning, episode, task, handoff, or scope:<path>"
        );
    }

    #[test]
    fn bias_boosts() {
        assert_eq!(
            ranked(&pair(), &["recency"]),
            pairs(&[("m_a", 1.3), ("m_b", 0.83)])
        );
        // executed alone: A 1.0, B 0.8 * (1 + 0.3) = 1.04
        assert_eq!(
            ranked(&pair(), &["executed"]),
            pairs(&[("m_b", 1.04), ("m_a", 1.0)])
        );
        // A 1.0 * 1.3 = 1.3; B 0.8 * (1 + 0.0375 + 0.3) = 1.07
        assert_eq!(
            ranked(&pair(), &["executed", "recency"]),
            pairs(&[("m_a", 1.3), ("m_b", 1.07)])
        );
        let b = bias(&["relevance"]);
        let scored = score_candidates(&pair(), "kiwi note", &opts(&b)).unwrap();
        let got: Vec<(&str, f64, bool)> = scored
            .iter()
            .map(|s| (s.item.id.as_str(), s.why.score, s.why.recent))
            .collect();
        assert_eq!(got, vec![("m_a", 1.0, false), ("m_b", 0.8, false)]);
        // B matches both values: 0.8 * (1 + 0.3 + 0.3) = 1.28
        let cands = vec![
            cand(kiwi("m_a", "derived", "fact", 0.0, ""), -10.0, None),
            cand(kiwi("m_b", "executed", "decision", 0.0, ""), -8.0, None),
        ];
        assert_eq!(
            ranked(&cands, &["executed", "decision"]),
            pairs(&[("m_b", 1.28), ("m_a", 1.0)])
        );
        let b = bias(&["decision"]);
        let scored = score_candidates(&cands, "kiwi note", &opts(&b)).unwrap();
        assert_eq!(scored[0].why.bias, vec!["decision"]);
        assert!(scored[1].why.bias.is_empty());
    }

    #[test]
    fn ref_hit_ranks_above_a_biased_item_and_recent_flag() {
        let cands = vec![
            cand(kiwi("m_exec", "executed", "fact", 0.0, ""), -10.0, None),
            cand(kiwi("m_ref", "derived", "fact", 0.0, "R6"), -1.0, None),
        ];
        let b = bias(&["executed"]);
        let scored = score_candidates(&cands, "R6", &opts(&b)).unwrap();
        assert_eq!(
            (scored[0].item.id.as_str(), scored[0].why.ref_hits.clone()),
            ("m_ref", vec!["R6".to_string()])
        );
        assert!(scored[1].why.ref_hits.is_empty());
        // boost at 30 days is exactly 0.15 (half of 0.3); at 31 days it is below half
        let cands = vec![
            cand(kiwi("m_30", "derived", "fact", 30.0, ""), -1.0, None),
            cand(kiwi("m_31", "derived", "fact", 31.0, ""), -1.0, None),
        ];
        let b = bias(&["recency"]);
        let got: Vec<(String, bool)> = score_candidates(&cands, "kiwi", &opts(&b))
            .unwrap()
            .into_iter()
            .map(|s| (s.item.id, s.why.recent))
            .collect();
        assert_eq!(
            got,
            vec![("m_30".to_string(), true), ("m_31".to_string(), false)]
        );
    }

    #[test]
    fn why_terms_and_explain() {
        // "kiwi" (4 ASCII letters) matches as a prefix of "kiwis"; "ab" must be a whole word
        let it = item(
            "m_t",
            "a",
            "Kiwis here",
            "abc only",
            "derived",
            &epoch_to_as_of(NOW).unwrap(),
        );
        let b = bias(&["recency"]);
        let why = score_candidates(&[cand(it, -1.0, None)], "kiwi ab", &opts(&b))
            .unwrap()
            .remove(0)
            .why;
        assert_eq!(
            (why.terms_matched, why.terms_missing),
            (vec!["kiwi".to_string()], vec!["ab".to_string()])
        );
        let o = ScoreOpts {
            explain: Some(1),
            ..opts(&b)
        };
        let cands = vec![
            cand(kiwi("m_a", "derived", "fact", 0.0, ""), -10.0, None),
            cand(kiwi("m_b", "derived", "fact", 0.0, ""), -8.0, None),
        ];
        let scored = score_candidates(&cands, "kiwi zebra", &o).unwrap();
        assert_eq!(
            (
                scored[0].why.terms_matched.clone(),
                scored[0].why.terms_missing.clone()
            ),
            (vec!["kiwi".to_string()], vec!["zebra".to_string()])
        );
        assert!(scored[1].why.terms_matched.is_empty() && scored[1].why.terms_missing.is_empty());
        // explain=-1 is `scored[:-1]`: every result but the last
        let o = ScoreOpts {
            explain: Some(-1),
            ..opts(&b)
        };
        let scored = score_candidates(&cands, "kiwi zebra", &o).unwrap();
        assert_eq!(scored[0].why.terms_matched, vec!["kiwi"]);
        assert!(scored[1].why.terms_matched.is_empty());
    }

    #[test]
    fn recency_boost_values() {
        let base = "2026-10-25T10:00:00+00:00";
        let now = parse_as_of_epoch(Some(base)).unwrap();
        assert_eq!(
            compute_recency_boost(base, Some(now)).unwrap(),
            RECENCY_MAX_BOOST
        );
        assert_eq!(
            compute_recency_boost(base, Some(now + SECONDS_PER_DAY * 30.0)).unwrap(),
            0.15
        );
        assert_eq!(
            compute_recency_boost(base, Some(now + SECONDS_PER_DAY * 60.0)).unwrap(),
            0.075
        );
        // an item newer than now gets the full boost; an unparsable as_of gets none
        assert_eq!(
            compute_recency_boost(base, Some(now - DAY)).unwrap(),
            RECENCY_MAX_BOOST
        );
        assert_eq!(compute_recency_boost("bad", Some(now)).unwrap(), 0.0);
        // exp2 rewrite of pow(0.5, x)
        assert_eq!(
            compute_recency_boost("2026-11-02T15:27:17+00:00", Some(1_800_000_000.0)).unwrap(),
            0.054663495224605584
        );
        let err = compute_recency_boost("0001-01-01T00:00+08:00", Some(now)).unwrap_err();
        assert_eq!(err.python_text(), "OverflowError: date value out of range");
    }

    #[test]
    fn rank_ref_phrase_accent_recency_and_recent_mode() {
        let b = bias(&["recency"]);
        let o = ScoreOpts {
            now_epoch: None,
            current_repo: Some("anvil"),
            ..opts(&b)
        };
        let old = item(
            "m_old",
            "anvil",
            "Deploy workflow",
            "Deploy workflow instructions",
            "decided",
            "2026-08-01T10:00:00+00:00",
        );
        let new = item(
            "m_new",
            "anvil",
            "Deploy workflow",
            "Deploy workflow instructions",
            "decided",
            "2026-10-25T10:00:00+00:00",
        );
        let r = rank_candidates(
            &[cand(old, -1.0, None), cand(new.clone(), -1.0, None)],
            "deploy",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_new", "m_old"]);
        let refi = Item {
            refs: "R5".into(),
            ..item(
                "m_ref",
                "anvil",
                "Note",
                "Short note",
                "decided",
                "2026-08-01T10:00:00+00:00",
            )
        };
        let o = ScoreOpts {
            now_epoch: None,
            ..opts(&b)
        };
        let r =
            rank_candidates(&[cand(new, -5.0, None), cand(refi, -0.5, None)], "R5", &o).unwrap();
        assert_eq!(r[0].id, "m_ref");

        let folded = item(
            "m_folded_phrase",
            "anvil",
            "Dorde resume data",
            "Dorde resume data pipeline",
            "decided",
            "2026-10-20T10:00:00+00:00",
        );
        let accented = item(
            "m_accented_phrase",
            "anvil",
            "Đorđe résumé data",
            "Đorđe résumé data pipeline",
            "decided",
            "2026-10-20T10:00:00+00:00",
        );
        let partial = item(
            "m_accented_partial",
            "anvil",
            "Đorđe schema",
            "Review café metrics separately",
            "decided",
            "2026-10-25T10:00:00+00:00",
        );
        let r = rank_candidates(
            &[
                cand(partial, -5.0, Some(-5.0)),
                cand(folded, -1.0, None),
                cand(accented, -1.0, Some(-1.0)),
            ],
            "Đorđe résumé data",
            &o,
        )
        .unwrap();
        assert_eq!(
            ids(&r),
            ["m_accented_phrase", "m_accented_partial", "m_folded_phrase"]
        );

        // --recent prefers items matching every query term, falls back to all, newest first
        let one = item(
            "m_one_newer",
            "anvil",
            "Queue only",
            "Mentioning queue without the second word",
            "decided",
            "2026-10-25T12:00:00+00:00",
        );
        let both_old = item(
            "m_both_older",
            "anvil",
            "Queue lag",
            "Both queue and lag appear here",
            "decided",
            "2026-10-20T12:00:00+00:00",
        );
        let both_new = item(
            "m_both_newer",
            "anvil",
            "Queue lag update",
            "Both queue and lag appear here too",
            "decided",
            "2026-10-24T12:00:00+00:00",
        );
        let o = ScoreOpts { recent: true, ..o };
        let r = rank_candidates(
            &[
                cand(one.clone(), -1.0, None),
                cand(both_old.clone(), -1.0, None),
                cand(both_new, -1.0, None),
            ],
            "queue lag",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_both_newer", "m_both_older"]);
        let r = rank_candidates(
            &[cand(both_old, -1.0, None), cand(one, -1.0, None)],
            "queue nonexistentword",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_one_newer", "m_both_older"]);
        assert!(score_candidates(&[], "x", &o).unwrap().is_empty());

        // an alias target word stands for the key word it covers; the item with neither goes
        let aliases: Aliases = vec![(
            "k8s".into(),
            crate::py::value::PyValue::Str("kubernetes".into()),
        )];
        let o = ScoreOpts {
            aliases: Some(&aliases),
            ..o
        };
        let target = item(
            "m_target",
            "anvil",
            "",
            "kubernetes deployq",
            "decided",
            "2026-10-25T12:00:00+00:00",
        );
        let key = item(
            "m_key",
            "anvil",
            "",
            "k8s deployq",
            "decided",
            "2026-10-20T12:00:00+00:00",
        );
        let neither = item(
            "m_neither",
            "anvil",
            "",
            "deployq alone",
            "decided",
            "2026-10-26T12:00:00+00:00",
        );
        let r = rank_candidates(
            &[
                cand(key, -1.0, None),
                cand(neither, -1.0, None),
                cand(target, -1.0, None),
            ],
            "k8s deployq",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_target", "m_key"]);
    }

    #[test]
    fn recent_filter_takes_each_alias_target_as_a_whole_with_fts_prefix_rules() {
        let b = bias(&[]);
        let recent = |json: &str, query: &str, bodies: &[(&str, &str)]| {
            let aliases: Aliases = serde_json::from_str::<serde_json::Value>(json)
                .unwrap()
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), crate::py::json::protocol_value(v).unwrap()))
                .collect();
            let o = ScoreOpts {
                recent: true,
                aliases: Some(&aliases),
                ..opts(&b)
            };
            // newest first in the order given
            let cands: Vec<Candidate> = bodies
                .iter()
                .enumerate()
                .map(|(i, (id, body))| {
                    let as_of = epoch_to_as_of(NOW - i as f64 * DAY).unwrap();
                    cand(item(id, "a", "", body, "derived", &as_of), -1.0, None)
                })
                .collect();
            rank_candidates(&cands, query, &o)
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        };
        let k8s = r#"{"k8s": "kubernetes cluster", "db": ["sqlite", "fts5"]}"#;
        // one word of a two-word target is no match; 4+ letter ASCII words match as prefixes,
        // as the FTS terms "kubernetes"* and "cluster"* do
        assert_eq!(
            recent(
                k8s,
                "k8s deployq",
                &[
                    ("m_one_word", "cluster deployq"),
                    ("m_prefix", "kubernetesx clusters deployqs"),
                    ("m_key", "k8s deployq"),
                    ("m_no_other", "kubernetes cluster"),
                ]
            ),
            ["m_prefix", "m_key"]
        );
        // each target of a list stands alone; "fts5" is no prefix term, so "fts50" is no match
        assert_eq!(
            recent(
                k8s,
                "db lock",
                &[
                    ("m_fts50", "fts50 lock"),
                    ("m_sqlite", "sqlitex lock"),
                    ("m_fts5", "fts5 locked"),
                ]
            ),
            ["m_sqlite", "m_fts5"]
        );
        // "ops" has 3 letters, so it matches only as the whole word, as the FTS term "ops"
        let ops = r#"{"teams poll": "ops-monitor"}"#;
        assert_eq!(
            recent(
                ops,
                "teams poll statusq",
                &[
                    ("m_ops_only", "ops statusq"),
                    ("m_opsx", "opsx monitor statusq"),
                    ("m_one_key_word", "teams statusq"),
                    ("m_target", "ops monitors statusq"),
                    ("m_key", "teams poll statusq"),
                ]
            ),
            ["m_target", "m_key"]
        );
        // an empty target holds no word, so it matches nothing; σ matches ς only under
        // IGNORECASE, so "σ x" is no group and the query words alone decide
        let empty = r#"{"k8s": ["kubernetes cluster", ""], "\u03c3 x": "tgt"}"#;
        assert_eq!(
            recent(
                empty,
                "k8s deployq",
                &[("m_one_word", "cluster deployq"), ("m_key", "k8s deployq")]
            ),
            ["m_key"]
        );
        assert_eq!(
            recent(
                empty,
                "\u{3c2} x",
                &[("m_tgt", "tgt"), ("m_words", "\u{3c2} x")]
            ),
            ["m_words"]
        );
    }

    #[test]
    fn rank_penalties_and_repo_boost() {
        let ts = "2026-10-25T10:00:00+00:00";
        let now = parse_as_of_epoch(Some(ts)).unwrap();
        let b = bias(&["recency"]);
        let o = ScoreOpts {
            now_epoch: Some(now),
            current_repo: Some("anvil"),
            ..opts(&b)
        };
        let mk = |id: &str, repo: &str| {
            item(
                id,
                repo,
                "Deploy retry rule",
                "Deploy retry rule details",
                "decided",
                ts,
            )
        };
        let r = rank_candidates(
            &[
                cand(mk("m_z_unassigned", "unassigned"), -1.0, None),
                cand(mk("m_a_global", "global"), -1.0, None),
                cand(mk("m_a_current", "anvil"), -1.0, None),
            ],
            "deploy retry",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_a_current", "m_a_global", "m_z_unassigned"]);

        let legacy = Item {
            source: "legacy:memory".into(),
            ..item(
                "legacy:mem_99",
                "anvil",
                "Cache invalidation rule",
                "Cache invalidation rule details",
                "n/a",
                ts,
            )
        };
        let active = Item {
            source: "cli".into(),
            ..item(
                "m_01",
                "anvil",
                "Cache invalidation rule",
                "Cache invalidation rule details",
                "decided",
                ts,
            )
        };
        assert!(is_legacy_unreorganized(&legacy) && !is_legacy_unreorganized(&active));
        let r = rank_candidates(
            &[cand(legacy, -1.0, None), cand(active, -1.0, None)],
            "cache invalidation",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_01", "legacy:mem_99"]);

        let g = item(
            "m_z_global",
            "global",
            "Schema migration check",
            "Schema migration check details",
            "decided",
            ts,
        );
        let a = item(
            "m_a_repo",
            "anvil",
            "Schema migration check",
            "Schema migration check details",
            "decided",
            ts,
        );
        let r = rank_candidates(
            &[cand(g, -1.0, None), cand(a, -1.0, None)],
            "schema migration",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_a_repo", "m_z_global"]);
    }

    #[test]
    fn phrase_tier_requires_accented_words() {
        let ts = "2026-10-25T10:00:00+00:00";
        let b = bias(&["recency"]);
        let o = ScoreOpts {
            now_epoch: Some(parse_as_of_epoch(Some(ts)).unwrap()),
            ..opts(&b)
        };
        let word = item(
            "m_cafe_word",
            "global",
            "cafe deploy",
            "cafe deploy",
            "derived",
            ts,
        );
        let split = item(
            "m_cafe_split",
            "global",
            "café when deploy finishes",
            "café when deploy finishes",
            "derived",
            ts,
        );
        let exact = item(
            "m_cafe_exact",
            "global",
            "café deploy",
            "café deploy",
            "derived",
            ts,
        );
        let r = rank_candidates(
            &[
                cand(word.clone(), -2.0, None),
                cand(split.clone(), -1.0, Some(-1.0)),
            ],
            "café deploy",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_cafe_split", "m_cafe_word"]);
        let r = rank_candidates(
            &[cand(split.clone(), -1.0, None), cand(word, -1.0, None)],
            "cafe deploy",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_cafe_word", "m_cafe_split"]);
        let r = rank_candidates(
            &[cand(split, -1.0, Some(-1.0)), cand(exact, -1.0, Some(-1.0))],
            "café deploy",
            &o,
        )
        .unwrap();
        assert_eq!(ids(&r), ["m_cafe_exact", "m_cafe_split"]);
    }

    fn tiers(cands: &[Candidate], q: &str) -> Vec<(String, Vec<String>, bool, bool, f64)> {
        let b = bias(&["recency"]);
        let o = ScoreOpts {
            now_epoch: Some(1_793_000_000.0),
            ..opts(&b)
        };
        score_candidates(cands, q, &o)
            .unwrap()
            .into_iter()
            .map(|s| {
                (
                    s.item.id,
                    s.why.ref_hits,
                    s.why.phrase,
                    s.why.accent,
                    s.why.score,
                )
            })
            .collect()
    }

    fn row(
        id: &str,
        refs: &[&str],
        phrase: bool,
        accent: bool,
        score: f64,
    ) -> (String, Vec<String>, bool, bool, f64) {
        (
            id.into(),
            refs.iter().map(|s| (*s).to_string()).collect(),
            phrase,
            accent,
            score,
        )
    }

    #[test]
    fn tier_guards() {
        // expected rows from score_candidates with now_epoch 1793000000.0
        let ts = "2026-10-25T03:00:00+00:00";
        let mk = |id: &str, t: &str, b: &str| item(id, "r", t, b, "derived", ts);
        // a one-word query also counts as a ref: "Kiwi" hits refs "kiwi"
        let refd = Item {
            refs: "kiwi".into(),
            ..mk("m_b", "note", "body")
        };
        assert_eq!(
            tiers(
                &[
                    cand(mk("m_a", "abc", "no"), -10.0, None),
                    cand(refd, -1.0, None)
                ],
                "Kiwi"
            ),
            vec![
                row("m_b", &["kiwi"], false, false, 0.1292),
                row("m_a", &[], false, false, 1.2919)
            ]
        );
        // a one-char phrase is no phrase hit
        assert_eq!(
            tiers(
                &[
                    cand(mk("m_a", "abc", "no"), -10.0, None),
                    cand(mk("m_b", "x", "x"), -1.0, None)
                ],
                "x"
            ),
            vec![
                row("m_a", &[], false, false, 1.2919),
                row("m_b", &[], false, false, 0.1292)
            ]
        );
        // the accent tier needs a raw (unfolded) FTS hit
        assert_eq!(
            tiers(
                &[
                    cand(mk("m_a", "café deploy", "café deploy"), -10.0, None),
                    cand(mk("m_b", "café deploy", "café deploy"), -1.0, Some(-1.0))
                ],
                "café deploy"
            ),
            vec![
                row("m_b", &[], true, true, 0.3084),
                row("m_a", &[], true, false, 1.2919)
            ]
        );
        // the legacy penalty alone puts m_a above m_z (the id tie-break would not)
        let legacy = Item {
            source: "legacy:memory".into(),
            ..mk("m_z", "Cache rule", "Cache rule")
        };
        assert_eq!(
            tiers(
                &[
                    cand(mk("m_a", "Cache rule", "Cache rule"), -1.0, None),
                    cand(legacy, -1.0, None)
                ],
                "cache rule"
            ),
            vec![
                row("m_a", &[], true, false, 1.2919),
                row("m_z", &[], true, false, 1.2419)
            ]
        );
    }

    #[test]
    fn hint_terms_and_query_terms() {
        assert!(is_hint_term("ab") && is_hint_term("Q3"));
        assert!(
            !is_hint_term("a")
                && !is_hint_term("and")
                && !is_hint_term("near")
                && !is_hint_term("")
        );
        assert_eq!(
            query_terms("Deploy db.pool DEPLOY Đorđe"),
            vec!["deploy", "db", "pool", "dorde"]
        );
        assert!(query_terms("").is_empty());
        assert!(
            bias_widens_pool(&["recency", "decided"])
                && !bias_widens_pool(&["recency", "relevance"])
        );
    }

    #[test]
    fn phrase_boundaries_and_multiplicative_iia_avoid_inversion() {
        let ts = "2026-10-25T10:00:00+00:00";
        let now = parse_as_of_epoch(Some(ts)).unwrap();
        let b = bias(&DEFAULT_BIAS);
        let o = ScoreOpts {
            now_epoch: Some(now),
            ..opts(&b)
        };
        // Word-boundary phrase: "test plan" must NOT hit "latest plan"; hyphenated matches spaced;
        // trailing word remains open for singular/plural ("retry rule" hits "retry rules").
        let latest = item("m_latest", "a", "latest plan", "notes", "derived", ts);
        let catchup = item("m_catch", "a", "catch-up interval", "notes", "derived", ts);
        let retry = item("m_retry", "a", "retry rules", "notes", "derived", ts);
        assert!(
            !score_candidates(&[cand(latest, -2.0, None)], "test plan", &o).unwrap()[0]
                .why
                .phrase
        );
        assert!(
            score_candidates(&[cand(catchup, -2.0, None)], "catch up interval", &o).unwrap()[0]
                .why
                .phrase
        );
        assert!(
            score_candidates(&[cand(retry, -2.0, None)], "retry rule", &o).unwrap()[0]
                .why
                .phrase
        );

        // IIA & weak-hit guard: adding a higher-BM25 Z must not invert X (raw 8, no bias) vs
        // Y (raw 4, +0.3 bias), and a 22% single-term biased note stays below a 100% 45d handoff.
        let x = cand(kiwi("m_x", "derived", "fact", 0.0, ""), -8.0, None);
        let y = cand(kiwi("m_y", "executed", "fact", 0.0, ""), -4.0, None);
        let z = cand(kiwi("m_z", "derived", "fact", 0.0, ""), -24.0, None);
        assert_eq!(ranked(&[x.clone(), y.clone()], &["executed"])[0].0, "m_x");
        assert_eq!(
            ranked(&[x, y, z], &["executed"])
                .into_iter()
                .map(|p| p.0)
                .collect::<Vec<_>>(),
            vec!["m_z", "m_x", "m_y"]
        );

        let strong_old_handoff = cand(
            kiwi("m_handoff", "derived", "handoff", 45.0, ""),
            -10.0,
            None,
        );
        let weak_new_dec = cand(kiwi("m_dec", "decided", "decision", 1.0, ""), -2.2, None);
        let r = score_candidates(&[weak_new_dec, strong_old_handoff], "kiwi note", &o).unwrap();
        assert_eq!(r[0].item.id, "m_handoff");

        // When max_rel == 0.0 (ref-only pool), bm25_scaled defaults to 1.0 so recency and bias rank.
        let ref_hi = cand(
            kiwi("m_ref_hi", "decided", "decision", 0.0, "R6"),
            0.0,
            None,
        );
        let ref_lo = cand(
            kiwi("m_ref_lo", "derived", "handoff", 30.0, "R6"),
            0.0,
            None,
        );
        let ref_scored = score_candidates(&[ref_lo, ref_hi], "R6", &opts(&b)).unwrap();
        assert_eq!(
            (ref_scored[0].item.id.as_str(), ref_scored[0].why.score),
            ("m_ref_hi", 1.9)
        );
        assert_eq!(
            (ref_scored[1].item.id.as_str(), ref_scored[1].why.score),
            ("m_ref_lo", 1.15)
        );
    }

    #[test]
    fn match_snippet_short_small_caps_and_front_matches() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        // Short text returns verbatim.
        assert_eq!(
            match_snippet("alpha beta", &s(&["beta"]), &[], 10),
            "alpha beta"
        );
        assert_eq!(
            match_snippet("alpha beta", &s(&["beta"]), &[], 20),
            "alpha beta"
        );
        // Tiny caps <= 3 and <= 6 never underflow and keep prefix cut even when a later token matches.
        assert_eq!(match_snippet("abcde ghij", &s(&["ghij"]), &[], 0), "");
        assert_eq!(match_snippet("abcde ghij", &s(&["ghij"]), &[], 3), "abc");
        assert_eq!(match_snippet("abcde ghij", &s(&["ghij"]), &[], 4), "a...");
        assert_eq!(match_snippet("abcde ghij", &s(&["ghij"]), &[], 5), "ab...");
        assert_eq!(match_snippet("abcde ghij", &s(&["ghij"]), &[], 6), "abc...");
        assert_eq!(match_snippet("a  bc ghij", &s(&["ghij"]), &[], 6), "a...");
        // No match or front match (end_char <= cap - 3) keeps prefix cut.
        let text = "front match here and then filler words beyond the cap limit";
        assert_eq!(
            match_snippet(text, &s(&["missing"]), &[], 20),
            "front match here..."
        );
        assert_eq!(
            match_snippet(text, &s(&["front"]), &[], 20),
            "front match here..."
        );
        // Boundary: token ending exactly at cap - 3 stays prefix cut.
        // "aaaa bbbb ccc" -> "ccc" is at [10..13], cap = 16 => cap - 3 = 13.
        assert_eq!(
            match_snippet("aaaa bbbb ccc dddd eeee", &s(&["ccc"]), &[], 16),
            "aaaa bbbb ccc..."
        );
        // Boundary: early long token starting within `lead` (win_start == 0) but ending past
        // cap - 3 still keeps the full cap (30 chars, not inner_both + 3 = 27).
        let early_long = format!("foo {}", "a".repeat(56));
        let out_early = match_snippet(&early_long, &s(&["aaaa"]), &[], 30);
        assert_eq!(out_early, format!("foo {}...", "a".repeat(23)));
        assert_eq!(py::str::len(&out_early), 30);
    }

    #[test]
    fn match_snippet_centers_middle_and_pulls_tail_left() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        // Middle match with word-boundary snap within 16 chars:
        // "juliet" starts at char 56; cap=36 -> inner_both=30, lead=10 -> win_start=46 ('t' in
        // "hotel" at 44..49), snaps forward to "india" at char 50; slice 50..80 -> 30 inner chars.
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar";
        let out = match_snippet(text, &s(&["juliet"]), &[], 36);
        assert_eq!(out, "...india juliet kilo lima mike no...");
        assert_eq!(py::str::len(&out), 36);

        // Long unbreakable token > 16 chars before match: does not snap past 16-char limit.
        let unbroken = format!("{} target {}", "z".repeat(40), "w".repeat(40));
        let out = match_snippet(&unbroken, &s(&["target"]), &[], 30);
        // inner_both = 24, lead = 8 -> win_start = 41 - 8 = 33; next word is at 41 (delta = 8 <= 16)
        assert_eq!(out, "...target wwwwwwwwwwwwwwwww...");
        assert_eq!(py::str::len(&out), 30);

        let far_unbroken = format!("{} target {}", "z".repeat(80), "w".repeat(40));
        // cap = 72 => inner_both = 66, lead = 22 => win_start = 81 - 22 = 59; next word is at 81 (delta 22 > 16), so no snap.
        let out_no_snap = match_snippet(&far_unbroken, &s(&["target"]), &[], 72);
        assert!(out_no_snap.starts_with("...zzzzzz"), "{out_no_snap}");
        assert!(out_no_snap.contains(" target "), "{out_no_snap}");
        assert_eq!(py::str::len(&out_no_snap), 72);

        // Tail match drops trailing "..." and pulls win_start left to fill cap.
        let tail = format!("{} finalword", "x".repeat(60));
        // total = 70, cap = 30 => win_start pulled to 70 - 27 = 43, no space within 16 chars (space at 60, delta 17 > 16)
        let out_tail = match_snippet(&tail, &s(&["finalword"]), &[], 30);
        assert_eq!(out_tail, format!("...{} finalword", "x".repeat(17)));
        assert_eq!(py::str::len(&out_tail), 30);
    }

    #[test]
    fn match_snippet_utf8_nfd_emdash_and_ref_punctuation() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        // NFD combining acute + em-dash + Vietnamese accented letters.
        let nfd = format!(
            "Tiêu đề cấu hình — {} bản ghi cafe\u{0301} Đorđe cuối cùng của tài liệu",
            "mô tả ngắn ".repeat(8)
        );
        let out = match_snippet(&nfd, &s(&["cafe"]), &[], 48);
        assert!(out.starts_with("...") && out.ends_with("..."), "{out}");
        assert!(out.contains("cafe\u{0301} Đorđe"), "{out}");
        assert!(py::str::len(&out) <= 48, "len={}", py::str::len(&out));

        // Ref hits with wrapping punctuation and file:line suffix.
        let ref_text = format!(
            "{} see (R5), and src/a.py:42. for the rollout details {}",
            "filler ".repeat(12),
            "after ".repeat(10)
        );
        let out_r5 = match_snippet(&ref_text, &[], &s(&["R5"]), 36);
        assert!(out_r5.contains("(R5),"), "{out_r5}");
        assert!(py::str::len(&out_r5) <= 36);

        let out_path = match_snippet(&ref_text, &[], &s(&["src/a.py"]), 36);
        assert!(out_path.contains("src/a.py:42."), "{out_path}");
        assert!(py::str::len(&out_path) <= 36);

        // Short term "ab" requires whole-word match (must not match "abc" earlier); 4+ ASCII letter
        // term "deploy" matches prefix "deploying".
        let prefix_text = format!(
            "abc {} ab {} deploying {}",
            "pad ".repeat(15),
            "pad ".repeat(15),
            "end ".repeat(10)
        );
        let out_ab = match_snippet(&prefix_text, &s(&["ab"]), &[], 30);
        assert!(
            out_ab.starts_with("...") && out_ab.contains(" ab "),
            "{out_ab}"
        );
        let out_dep = match_snippet(&prefix_text, &s(&["deploy"]), &[], 30);
        assert!(
            out_dep.starts_with("...") && out_dep.contains("deploying"),
            "{out_dep}"
        );
    }

    #[test]
    fn parse_query_exclusions_handles_terms_phrases_operators_and_pure_dash_literal() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_query_exclusions("postgres -legacy"),
            ("postgres".into(), s(&["legacy"]))
        );
        assert_eq!(
            parse_query_exclusions("-legacy postgres"),
            ("postgres".into(), s(&["legacy"]))
        );
        assert_eq!(
            parse_query_exclusions("auth -oauth -saml -oauth"),
            ("auth".into(), s(&["oauth", "saml"]))
        );
        assert_eq!(
            parse_query_exclusions("deploy -\"connection pool\" -café"),
            ("deploy".into(), s(&["connection pool", "cafe"]))
        );
        // Quoted leading-dash token and standalone -, --, --flag, hyphenated words stay positive.
        assert_eq!(
            parse_query_exclusions("\"-excl\" term"),
            ("\"-excl\" term".into(), Vec::<String>::new())
        );
        assert_eq!(
            parse_query_exclusions("deploy - -- --flag rate-limit ALP-489"),
            (
                "deploy - -- --flag rate-limit ALP-489".into(),
                Vec::<String>::new()
            )
        );
        // Zero positive tokens: pure leading-dash queries stay literal (preserve `sm query -- -R6`).
        assert_eq!(parse_query_exclusions("-R6"), ("-R6".into(), Vec::new()));
        assert_eq!(parse_query_exclusions("-yak"), ("-yak".into(), Vec::new()));
        // Standalone unquoted uppercase OR / AND stripped; lowercase and quoted preserved.
        assert_eq!(
            parse_query_exclusions("foo OR bar AND baz"),
            ("foo bar baz".into(), Vec::new())
        );
        assert_eq!(
            parse_query_exclusions("read and write or copy"),
            ("read and write or copy".into(), Vec::new())
        );
        assert_eq!(
            parse_query_exclusions("\"OR\" gate"),
            ("\"OR\" gate".into(), Vec::new())
        );
        assert_eq!(
            parse_query_exclusions("OR AND"),
            (String::new(), Vec::new())
        );
        // Unclosed negated quote and empty negated quote never panic.
        assert_eq!(
            parse_query_exclusions("deploy -\"open phrase"),
            ("deploy".into(), s(&["open phrase"]))
        );
        assert_eq!(
            parse_query_exclusions("deploy -\"   \""),
            ("deploy -\"   \"".into(), Vec::new())
        );
    }

    #[test]
    fn item_matches_exclusion_checks_fields_phrases_and_word_boundaries() {
        let mk = |title: &str, body: &str, refs: &str, tags: &str| {
            let mut it = Item::bare("m_1", "r", "fact", title, body, "decided");
            it.refs = refs.into();
            it.tags = tags.into();
            it
        };
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        // Matches across refs, title, tags, body.
        assert!(item_matches_exclusion(
            &mk("t", "b", "staging-1", ""),
            &s(&["staging"])
        ));
        assert!(item_matches_exclusion(
            &mk("Staging deploy", "b", "", ""),
            &s(&["staging"])
        ));
        assert!(item_matches_exclusion(
            &mk("t", "b", "", "env, staging"),
            &s(&["staging"])
        ));
        assert!(item_matches_exclusion(
            &mk("t", "PostgreSQL connection pool tuning", "", ""),
            &s(&["connection pool"])
        ));
        // Word-bounded phrase must not match partial words or non-adjacent words.
        assert!(!item_matches_exclusion(
            &mk("t", "reconnection poolparty", "", ""),
            &s(&["connection pool"])
        ));
        assert!(!item_matches_exclusion(
            &mk("t", "connection worker pool", "", ""),
            &s(&["connection pool"])
        ));
        // Short single term requires exact word match.
        assert!(!item_matches_exclusion(
            &mk("t", "db2 only", "", ""),
            &s(&["db"])
        ));
        assert!(item_matches_exclusion(
            &mk("t", "db only", "", ""),
            &s(&["db"])
        ));
    }

    #[test]
    fn active_items_rank_above_superseded_even_past_ref_and_phrase_hits() {
        let ts = "2026-10-25T10:00:00+00:00";
        let mut old_sup = item(
            "m_sup",
            "a",
            "R6 deploy rule",
            "R6 deploy rule",
            "executed",
            ts,
        );
        old_sup.refs = "R6".into();
        old_sup.status = "superseded".into();
        old_sup.superseded_by = Some("m_act".into());

        let mut retired = item(
            "m_ret",
            "a",
            "R6 deploy rule",
            "R6 deploy rule",
            "executed",
            ts,
        );
        retired.refs = "R6".into();
        retired.status = "superseded".into();

        let active = item("m_act", "a", "deploy note", "generic body", "derived", ts);

        let b = bias(&DEFAULT_BIAS);
        let o = ScoreOpts {
            now_epoch: Some(parse_as_of_epoch(Some(ts)).unwrap()),
            ..opts(&b)
        };
        let scored = score_candidates(
            &[
                cand(old_sup, -10.0, None),
                cand(retired, -10.0, None),
                cand(active, -0.5, None),
            ],
            "R6 deploy rule",
            &o,
        )
        .unwrap();
        assert_eq!(scored[0].item.id, "m_act");
        assert_eq!(scored[0].item.status, "active");
        assert!(scored[1..].iter().all(|s| s.item.status == "superseded"));
    }

    #[test]
    fn word_matches_snake_case_and_alias_explain_terms() {
        let ts = "2026-10-25T10:00:00+00:00";
        // snake_case query term matches exact, extended snake_case, and space-separated tokens.
        for body in [
            "uses foo_bar here",
            "uses foo_bar_baz here",
            "uses foo bar here",
        ] {
            let it = item("m_1", "a", "note", body, "derived", ts);
            let b = bias(&["recency"]);
            let why = score_candidates(&[cand(it, -1.0, None)], "foo_bar", &opts(&b))
                .unwrap()
                .remove(0)
                .why;
            assert_eq!(why.terms_matched, vec!["foo_bar"], "{body}");
            assert!(why.terms_missing.is_empty(), "{body}");
        }
        // Partial snake_case sub-part missing does not match; all-underscore does not match vacuously.
        let partial = item("m_p", "a", "note", "uses foo only", "derived", ts);
        assert!(!word_matches("foo_bar", &haystack_words(&partial)));
        assert!(!word_matches("___", &haystack_words(&partial)));

        // Alias-aware explain_terms: k8s -> kubernetes, db -> [sqlite, fts5], queue lag -> queue-worker.
        let aliases: Aliases = vec![
            (
                "k8s".into(),
                crate::py::value::PyValue::Str("kubernetes".into()),
            ),
            (
                "db".into(),
                crate::py::value::PyValue::List(vec![
                    crate::py::value::PyValue::Str("sqlite".into()),
                    crate::py::value::PyValue::Str("fts5".into()),
                ]),
            ),
            (
                "queue lag".into(),
                crate::py::value::PyValue::Str("queue-worker".into()),
            ),
        ];
        let b = bias(&["recency"]);
        let o = ScoreOpts {
            aliases: Some(&aliases),
            ..opts(&b)
        };
        let k8s_item = item(
            "m_k",
            "a",
            "kubernetes deploy",
            "sqlite cluster",
            "derived",
            ts,
        );
        let why = score_candidates(&[cand(k8s_item, -1.0, None)], "k8s db deploy", &o)
            .unwrap()
            .remove(0)
            .why;
        assert_eq!(why.terms_matched, vec!["k8s", "db", "deploy"]);
        assert!(why.terms_missing.is_empty());

        // Multi-word alias: "queue" alone does NOT falsely satisfy "lag"; "queue-worker" does.
        let q_only = item("m_q", "a", "queue tuning", "queue settings", "derived", ts);
        let why_q = score_candidates(&[cand(q_only, -1.0, None)], "queue lag", &o)
            .unwrap()
            .remove(0)
            .why;
        assert_eq!(why_q.terms_matched, vec!["queue"]);
        assert_eq!(why_q.terms_missing, vec!["lag"]);

        let q_worker = item(
            "m_qw",
            "a",
            "queue-worker",
            "handles retries",
            "derived",
            ts,
        );
        let why_qw = score_candidates(&[cand(q_worker, -1.0, None)], "queue lag", &o)
            .unwrap()
            .remove(0)
            .why;
        assert_eq!(why_qw.terms_matched, vec!["queue", "lag"]);
        assert!(why_qw.terms_missing.is_empty());
    }
}
