//! `fold` backs the `sm_fold` SQL function that FTS triggers call on every write: a char is
//! stripped when its canonical combining class is not 0 (`unicodedata.combining`), matching
//! Python's NFD normalization rules for accented Latin characters.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use crate::py::str::lower;

/// `fold(text)`; None and "" give "".
pub fn fold(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let s = text.replace('đ', "d").replace('Đ', "D");
    let stripped: String = s
        .nfd()
        .filter(|&c| canonical_combining_class(c) == 0)
        .collect();
    lower(&stripped)
}

/// `has_accents(text)`.
pub fn has_accents(text: Option<&str>) -> bool {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return false;
    };
    text.nfd()
        .any(|c| c == 'đ' || c == 'Đ' || canonical_combining_class(c) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_vectors_from_python_fixture() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fold_vectors.json"
        ));
        let cases: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(cases.len() >= 30);
        for c in &cases {
            assert_eq!(
                fold(c["input"].as_str()),
                c["expected"].as_str().unwrap(),
                "input {:?}",
                c["input"]
            );
        }
    }

    #[test]
    fn fold_boundary_and_has_accents() {
        assert_eq!(fold(None), "");
        assert_eq!(fold(Some("")), "");
        assert!(has_accents(Some("Đorđe Café")));
        assert!(has_accents(Some("naïve")));
        assert!(!has_accents(Some("R5 D9")));
        assert!(!has_accents(Some("")));
        assert!(!has_accents(None));
    }

    #[test]
    fn combining_class_not_mark_category() {
        // U+034F COMBINING GRAPHEME JOINER is a Mark with combining class 0: Python keeps it.
        assert_eq!(fold(Some("a\u{34f}b")), "a\u{34f}b");
        assert!(!has_accents(Some("\u{34f}")));
        // İ decomposes to I + U+0307; the mark goes, then lower() gives plain i.
        assert_eq!(fold(Some("İ")), "i");
        assert_eq!(fold(Some("ΣΑΣ")), "σας");
    }
}
