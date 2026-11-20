#[cfg(any(test, feature = "parity"))]
use serde_json::Value;

use crate::error::SmError;
#[cfg(any(test, feature = "parity"))]
use crate::parity::{Call, Reply};
use crate::py::str::{lower, repr, strip};

pub const VALID_TAGS: [&str; 8] = [
    "executed",
    "verified",
    "decided",
    "client-stated",
    "derived",
    "assumed",
    "prior",
    "planned",
];
pub const LEGACY_NA_TAG: &str = "n/a";
pub const ALL_TAGS: [&str; 9] = [
    "executed",
    "verified",
    "decided",
    "client-stated",
    "derived",
    "assumed",
    "prior",
    "planned",
    LEGACY_NA_TAG,
];
pub const EVIDENCE_REQUIRED_TAGS: [&str; 2] = ["executed", "verified"];
pub const LEGACY_TAG_MAP: [(&str, &str); 2] = [("inferred", "derived"), ("speculated", "prior")];
pub const VALID_KINDS: [&str; 6] = ["fact", "decision", "learning", "episode", "task", "handoff"];
pub const RETIRED_LABEL: &str = "file deleted";
pub const RETIRED_TAG_PREFIX: &str = "retired:";

/// `map_legacy_tag(raw_tag)` -> (canonical_tag, extra_tag_to_keep).
pub fn map_legacy_tag(raw_tag: Option<&str>) -> (String, Option<String>) {
    let Some(raw) = raw_tag.filter(|r| !r.is_empty()) else {
        return (LEGACY_NA_TAG.into(), None);
    };
    let cleaned = lower(strip(raw));
    if cleaned.is_empty() {
        return (LEGACY_NA_TAG.into(), None);
    }
    let mapped = LEGACY_TAG_MAP
        .iter()
        .find(|(k, _)| *k == cleaned)
        .map_or(cleaned.as_str(), |(_, v)| *v);
    if VALID_TAGS.contains(&mapped) {
        return (mapped.into(), None);
    }
    if mapped == LEGACY_NA_TAG {
        return (LEGACY_NA_TAG.into(), None);
    }
    (LEGACY_NA_TAG.into(), Some(cleaned))
}

/// `validate_tag(tag, evidence="", allow_na=False)`.
pub fn validate_tag(
    tag: Option<&str>,
    evidence: Option<&str>,
    allow_na: bool,
) -> Result<String, SmError> {
    let Some(tag) = tag.filter(|t| !strip(t).is_empty()) else {
        return Err(SmError::value("Evidence tag is required"));
    };
    let cleaned = lower(strip(tag));
    let allowed: &[&str] = if allow_na { &ALL_TAGS } else { &VALID_TAGS };
    if !allowed.contains(&cleaned.as_str()) {
        return Err(SmError::value(format!(
            "Invalid tag {}; expected one of {}",
            repr(tag),
            allowed.join(", ")
        )));
    }
    if EVIDENCE_REQUIRED_TAGS.contains(&cleaned.as_str())
        && strip(evidence.unwrap_or("")).is_empty()
    {
        return Err(SmError::value(format!(
            "Tag {} requires non-empty --evidence",
            repr(&cleaned)
        )));
    }
    Ok(cleaned)
}

/// `validate_kind(kind)`.
pub fn validate_kind(kind: Option<&str>) -> Result<String, SmError> {
    let Some(kind) = kind.filter(|k| !strip(k).is_empty()) else {
        return Err(SmError::value("Item kind is required"));
    };
    let cleaned = lower(strip(kind));
    if !VALID_KINDS.contains(&cleaned.as_str()) {
        return Err(SmError::value(format!(
            "Invalid kind {}; expected one of {}",
            repr(kind),
            VALID_KINDS.join(", ")
        )));
    }
    Ok(cleaned)
}

/// `append_tag(tags, new_tag)`.
pub fn append_tag(tags: Option<&str>, new_tag: &str) -> String {
    let mut existing: Vec<&str> = tags
        .unwrap_or("")
        .split(',')
        .map(strip)
        .filter(|t| !t.is_empty())
        .collect();
    if !existing.contains(&new_tag) {
        existing.push(new_tag);
    }
    existing.join(", ")
}

/// The reason carried by the last `retired:<reason>` tag, or `RETIRED_LABEL` when none.
pub fn retired_label(tags: &str) -> String {
    tags.split(',')
        .map(strip)
        .filter_map(|t| {
            t.strip_prefix(RETIRED_TAG_PREFIX)
                .map(strip)
                .filter(|s| !s.is_empty())
        })
        .next_back()
        .map_or_else(|| RETIRED_LABEL.to_string(), str::to_string)
}

/// True when any comma-separated trimmed tag starts with `retired:` and has a non-empty suffix.
pub fn has_retired_tag(tags: &str) -> bool {
    tags.split(',').map(strip).any(|t| {
        t.strip_prefix(RETIRED_TAG_PREFIX)
            .is_some_and(|rest| !strip(rest).is_empty())
    })
}

/// Comma-joined trimmed non-empty tags excluding any `retired:*` token.
pub fn strip_retired_tags(tags: &str) -> String {
    tags.split(',')
        .map(strip)
        .filter(|t| !t.is_empty() && !t.starts_with(RETIRED_TAG_PREFIX))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(any(test, feature = "parity"))]
pub fn parity(f: &str, call: &Call) -> Option<Reply> {
    Some(match f {
        "core.tags.map_legacy_tag" => (|| {
            let (a, b) = map_legacy_tag(call.opt_str(0, "raw_tag")?);
            Ok(Value::from(vec![
                Value::from(a),
                b.map_or(Value::Null, Value::from),
            ]))
        })(),
        "core.tags.validate_tag" => (|| {
            let evidence = match call.arg(1, "evidence") {
                None => Some(""),
                Some(_) => call.opt_str(1, "evidence")?,
            };
            let allow_na = call.opt_bool(2, "allow_na")?.unwrap_or(false);
            validate_tag(call.opt_str(0, "tag")?, evidence, allow_na).map(Value::from)
        })(),
        "core.tags.validate_kind" => {
            (|| validate_kind(call.opt_str(0, "kind")?).map(Value::from))()
        }
        "core.tags.append_tag" => {
            (|| Ok(append_tag(call.opt_str(0, "tags")?, call.str(1, "new_tag")?).into()))()
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_validation_and_mapping() {
        assert_eq!(
            validate_tag(Some("decided"), Some(""), false).unwrap(),
            "decided"
        );
        assert_eq!(
            validate_tag(Some("verified"), Some("pytest -q"), false).unwrap(),
            "verified"
        );
        assert_eq!(
            validate_tag(Some("executed"), Some("git commit abc1234"), false).unwrap(),
            "executed"
        );
        assert_eq!(validate_tag(Some("n/a"), Some(""), true).unwrap(), "n/a");
        let err = |t: &str, e: &str, na: bool| {
            validate_tag(Some(t), Some(e), na)
                .unwrap_err()
                .python_text()
        };
        assert_eq!(
            err("", "some evidence", false),
            "ValueError: Evidence tag is required"
        );
        assert_eq!(
            err("n/a", "", false),
            "ValueError: Invalid tag 'n/a'; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned"
        );
        assert!(
            err("made-up-tag", "ev", false).starts_with("ValueError: Invalid tag 'made-up-tag'")
        );
        assert_eq!(
            err("verified", "   ", false),
            "ValueError: Tag 'verified' requires non-empty --evidence"
        );
        assert_eq!(
            err(" EXECUTED ", "", false),
            "ValueError: Tag 'executed' requires non-empty --evidence"
        );
        assert_eq!(
            validate_tag(Some("executed"), None, false).unwrap_err().msg,
            "Tag 'executed' requires non-empty --evidence"
        );

        assert_eq!(map_legacy_tag(Some("inferred")), ("derived".into(), None));
        assert_eq!(map_legacy_tag(Some("speculated")), ("prior".into(), None));
        assert_eq!(map_legacy_tag(Some("verified")), ("verified".into(), None));
        assert_eq!(
            map_legacy_tag(Some("user-stated")),
            ("n/a".into(), Some("user-stated".into()))
        );
        assert_eq!(
            map_legacy_tag(Some(" Heard ")),
            ("n/a".into(), Some("heard".into()))
        );
        assert_eq!(map_legacy_tag(Some(" N/A")), ("n/a".into(), None));
        assert_eq!(map_legacy_tag(None), ("n/a".into(), None));
        assert_eq!(map_legacy_tag(Some("\x1c")), ("n/a".into(), None));
        assert_eq!(
            append_tag(Some("alpha, beta"), "as_of:mtime"),
            "alpha, beta, as_of:mtime"
        );
        assert_eq!(
            append_tag(Some("as_of:mtime"), "as_of:mtime"),
            "as_of:mtime"
        );
        assert_eq!(append_tag(None, "x"), "x");
    }

    #[test]
    fn kind_validation() {
        for k in VALID_KINDS {
            assert_eq!(validate_kind(Some(k)).unwrap(), k);
            assert_eq!(
                validate_kind(Some(&format!("  {}  ", k.to_uppercase()))).unwrap(),
                k
            );
        }
        assert_eq!(
            validate_kind(Some("note")).unwrap_err().python_text(),
            "ValueError: Invalid kind 'note'; expected one of fact, decision, learning, episode, task, handoff"
        );
        assert_eq!(
            validate_kind(Some("   ")).unwrap_err().msg,
            "Item kind is required"
        );
        assert_eq!(
            validate_kind(None).unwrap_err().msg,
            "Item kind is required"
        );
    }

    #[test]
    fn retired_tag_helpers() {
        assert_eq!(retired_label(""), RETIRED_LABEL);
        assert_eq!(retired_label("alpha, beta"), RETIRED_LABEL);
        assert_eq!(retired_label("retired:"), RETIRED_LABEL);
        assert_eq!(retired_label("retired:   "), RETIRED_LABEL);
        assert_eq!(retired_label("not_retired:x"), RETIRED_LABEL);
        assert_eq!(retired_label("alpha, retired:manual"), "manual");
        assert_eq!(
            retired_label("alpha, retired:old reason, beta, retired: café "),
            "café"
        );
        assert_eq!(retired_label("retired:first, retired:   "), "first");

        assert!(!has_retired_tag(""));
        assert!(!has_retired_tag("alpha, beta"));
        assert!(!has_retired_tag("retired:"));
        assert!(!has_retired_tag("retired:   "));
        assert!(!has_retired_tag("not_retired:manual"));
        assert!(has_retired_tag("retired:manual"));
        assert!(has_retired_tag("alpha, retired: duplicate "));

        assert_eq!(strip_retired_tags(""), "");
        assert_eq!(strip_retired_tags("alpha, beta"), "alpha, beta");
        assert_eq!(
            strip_retired_tags(" alpha , retired:manual , beta , retired: , gamma "),
            "alpha, beta, gamma"
        );
        assert_eq!(
            strip_retired_tags("not_retired:x, retired:y"),
            "not_retired:x"
        );
    }
}
