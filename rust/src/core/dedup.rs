//! Duplicate signals: normalized titles, IDF-weighted title cosine with a rare-token veto, and
//! word overlap for the `sm put` warning.

use std::collections::{BTreeSet, HashSet};

use unicode_normalization::UnicodeNormalization;

use crate::core::fold::fold;
use crate::core::terms::folded_words;
use crate::py;

pub const COSINE_THRESHOLD: f64 = 0.8;
pub const VETO_DF: usize = 10;
pub const MIN_SHARED_TOKENS: usize = 2;
pub const OVERLAP_WARN_SHARE: f64 = 0.5;

/// `normalize_title(title)`: folded, punctuation and `_` to spaces (`[^\w\s]|_`), spaces collapsed.
pub fn normalize_title(title: Option<&str>) -> String {
    let nfc: String = title.unwrap_or("").nfc().collect();
    let s = fold(Some(&nfc));
    let spaced: String = s
        .chars()
        .map(|c| {
            if c == '_' || !(py::str::is_word(c) || py::str::isspace(c)) {
                ' '
            } else {
                c
            }
        })
        .collect();
    py::str::split(&spaced).join(" ")
}

/// `title_tokens(title)`.
#[cfg(test)]
pub fn title_tokens(title: Option<&str>) -> Vec<String> {
    py::str::split(&normalize_title(title))
        .into_iter()
        .map(String::from)
        .collect()
}

/// `idf(df, n)` = `math.log(1 + n / (df + 0.5))`. Counts are never negative, so the log's input
/// is at least 1 and Python's ValueError and ZeroDivisionError cannot happen.
pub fn idf(df: usize, n: usize) -> f64 {
    (1.0 + n as f64 / (df as f64 + 0.5)).ln()
}

/// Distinct tokens in first-seen order. Python iterates `set(a)` in hash order, which changes
/// with PYTHONHASHSEED; with sum()'s compensated summation the order did not change a result in
/// 20,000 log-weight cases under 4 hash seeds (lane C report).
fn distinct<'a, I: IntoIterator<Item = &'a str>>(a: I) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    a.into_iter().filter(|t| seen.insert(*t)).collect()
}

/// CPython's `sum()` of floats (bltinmodule.c, 3.12+): the int start 0 plus the first item, then
/// Neumaier compensated summation, the compensation added at the end when it is finite and not 0.
/// An empty sum is the int 0, which compares equal to 0.0 here.
fn py_sum<I: IntoIterator<Item = f64>>(items: I) -> f64 {
    let mut it = items.into_iter();
    let Some(first) = it.next() else {
        return 0.0;
    };
    let mut f = 0.0 + first;
    let mut c = 0.0;
    for x in it {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// `tfidf_cosine(a, b, idf_of)`.
pub fn tfidf_cosine<'a>(a: &[&'a str], b: &[&'a str], mut idf_of: impl FnMut(&str) -> f64) -> f64 {
    let mut weigh = |s: &[&'a str]| -> Vec<(&'a str, f64)> {
        distinct(s.iter().copied())
            .into_iter()
            .map(|t| (t, idf_of(t)))
            .collect()
    };
    let va = weigh(a);
    let vb = weigh(b);
    let weight_b = |t: &str| vb.iter().find(|(u, _)| *u == t).map(|(_, w)| *w);
    let dot = py_sum(va.iter().filter_map(|(t, w)| weight_b(t).map(|wb| w * wb)));
    let na = py_sum(va.iter().map(|(_, w)| w * w));
    let nb = py_sum(vb.iter().map(|(_, w)| w * w));
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb).sqrt()
}

/// `veto_fires(a, b, idf_of, theta)`: a token only one side holds, rarer than theta.
pub fn veto_fires(a: &[&str], b: &[&str], mut idf_of: impl FnMut(&str) -> f64, theta: f64) -> bool {
    let sa: BTreeSet<&str> = a.iter().copied().collect();
    let sb: BTreeSet<&str> = b.iter().copied().collect();
    sa.symmetric_difference(&sb).any(|t| idf_of(t) > theta)
}

/// `near_duplicate_score(a, b, idf_of, theta)`: the cosine when the pair is a near-duplicate
/// candidate, else None.
pub fn near_duplicate_score(
    a: &[&str],
    b: &[&str],
    mut idf_of: impl FnMut(&str) -> f64,
    theta: f64,
) -> Option<f64> {
    let sa: HashSet<&str> = a.iter().copied().collect();
    let shared = b
        .iter()
        .copied()
        .collect::<HashSet<&str>>()
        .intersection(&sa)
        .count();
    if shared < MIN_SHARED_TOKENS {
        return None;
    }
    let score = tfidf_cosine(a, b, &mut idf_of);
    (score >= COSINE_THRESHOLD && !veto_fires(a, b, &mut idf_of, theta)).then_some(score)
}

/// `word_overlap(new_text, other_text)`: share of the new text's distinct words the other holds.
pub fn word_overlap(new_text: &str, other_text: &str) -> f64 {
    let new_words: HashSet<String> = folded_words(new_text);
    if new_words.is_empty() {
        return 0.0;
    }
    let other: HashSet<String> = folded_words(other_text);
    new_words.intersection(&other).count() as f64 / new_words.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table<'a>(pairs: &'a [(&'a str, f64)]) -> impl Fn(&str) -> f64 + 'a {
        move |t| pairs.iter().find(|(k, _)| *k == t).unwrap().1
    }

    #[test]
    fn titles_and_words() {
        assert_eq!(
            normalize_title(Some("Fix_the  DB.POOL loader!²\u{203f}\u{301}x")),
            "fix the db pool loader ² x"
        );
        assert_eq!(normalize_title(None), "");
        assert!(title_tokens(Some(" !! ")).is_empty());
        let mut words: Vec<String> = folded_words::<HashSet<String>>("Café Đorđe café ĐORĐE x_y")
            .into_iter()
            .collect();
        words.sort();
        assert_eq!(words, vec!["cafe", "dorde", "x_y"]);
        assert_eq!(word_overlap("a b c d", "c d e"), 0.5);
        assert_eq!(word_overlap("", "c d e"), 0.0);
    }

    #[test]
    fn idf_values() {
        assert_eq!(idf(0, 0), 0.0);
        assert_eq!(idf(1, 10), (1.0f64 + 10.0 / 1.5).ln());
        assert_eq!(idf(0, 1), 1.0986122886681098);
    }

    #[test]
    fn cosine_veto_and_near_duplicates() {
        let t = [("a", 1.0), ("b", 2.0), ("c", 2.0)];
        assert_eq!(
            tfidf_cosine(&["a", "b", "b"], &["b", "c"], table(&t)),
            0.6324555320336759
        );
        assert_eq!(tfidf_cosine(&[], &["b"], table(&t)), 0.0);
        let v = [("a", 1.0), ("b", 0.5), ("c", 3.0)];
        assert!(veto_fires(&["a", "b"], &["a", "c"], table(&v), 2.0));
        assert!(!veto_fires(&["a", "b"], &["a", "b"], table(&v), 2.0));
        let n = [("a", 1.0), ("b", 2.0), ("c", 0.5), ("d", 0.5)];
        assert_eq!(
            near_duplicate_score(&["a", "b", "c"], &["a", "b", "c", "c"], table(&n), 9.0),
            Some(1.0)
        );
        // one shared token is below MIN_SHARED_TOKENS
        assert_eq!(
            near_duplicate_score(&["a", "b"], &["a", "c"], table(&n), 9.0),
            None
        );
        // one shared token, though the cosine (100 / sqrt(101 * 100) = 0.995) passes
        assert_eq!(
            near_duplicate_score(&["a", "b"], &["a"], table(&[("a", 10.0), ("b", 1.0)]), 9.0),
            None
        );
        // two shared, no veto, but the cosine is 2 / 3
        let ones = [("a", 1.0), ("b", 1.0), ("c", 1.0), ("d", 1.0)];
        assert_eq!(
            near_duplicate_score(&["a", "b", "c"], &["a", "b", "d"], table(&ones), 9.0),
            None
        );
        // the cosine passes (5 / 5.25 = 0.95) but c and d are rarer than theta: veto
        assert_eq!(
            near_duplicate_score(&["a", "b", "c"], &["a", "b", "d"], table(&n), 0.25),
            None
        );
    }

    #[test]
    fn py_sum_is_cpython_neumaier() {
        // Python 3.15.1: sum([1e16, 1.0, -1e16]) == 1.0; a plain left fold gives 0.0
        assert_eq!(py_sum([1e16, 1.0, -1e16]), 1.0);
        assert_eq!(py_sum([0.1; 10]), 1.0);
        assert_eq!(py_sum([]), 0.0);
    }
}
