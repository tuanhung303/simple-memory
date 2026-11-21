use crate::core::item::{Item, check_stored_as_of, new_item_id, validate_as_of};
use crate::core::refs::{RefsArg, extract_refs, merge_refs};
use crate::core::tags::{validate_kind, validate_tag};
use crate::error::{Base, SmError};
use crate::py::json::loads;
use crate::py::path::{Errors, PyPath};
use crate::py::str::{splitlines, strip};
use crate::py::value::{PyValue, type_name};
use crate::sources::{dict_get, file_error, sorted_paths, truthy};

fn coerce_str_or_list(val: Option<&PyValue>) -> Result<String, SmError> {
    match val {
        None | Some(PyValue::None) => Ok(String::new()),
        Some(PyValue::Str(s)) => Ok(strip(s).to_string()),
        Some(PyValue::List(items)) => Ok(items
            .iter()
            .filter(|x| !matches!(x, PyValue::None) && !strip(&x.py_str()).is_empty())
            .map(|x| strip(&x.py_str()).to_string())
            .collect::<Vec<_>>()
            .join(" ")),
        Some(other) => Err(SmError::value(format!(
            "Expected string or list of strings, got {}",
            type_name(other)
        ))),
    }
}

/// `str(raw.get(key) or "").strip()`.
fn field(raw: &[(PyValue, PyValue)], key: &str) -> String {
    match dict_get(raw, key) {
        Some(v) if truthy(v) => strip(&v.py_str()).to_string(),
        _ => String::new(),
    }
}

/// `parse_proposal_line(line)`.
pub fn parse_proposal_line(line: &str) -> Result<Item, SmError> {
    let PyValue::Dict(raw) = loads(line)? else {
        return Err(SmError::type_("Proposal line must be a JSON object"));
    };

    let tag_raw = field(&raw, "tag");
    let evidence = field(&raw, "evidence");
    let tag = validate_tag(Some(&tag_raw), Some(&evidence), false)?;

    let repo = field(&raw, "repo");
    let kind_raw = field(&raw, "kind");
    let title = field(&raw, "title");
    let body = field(&raw, "body");
    if repo.is_empty() || kind_raw.is_empty() || title.is_empty() || body.is_empty() {
        return Err(SmError::value(
            "Proposal row requires non-empty repo, kind, title, and body",
        ));
    }
    let kind = validate_kind(Some(&kind_raw))?;

    let sup_list: Vec<String> = match dict_get(&raw, "supersedes") {
        None | Some(PyValue::None) => Vec::new(),
        Some(PyValue::List(items)) => items
            .iter()
            .filter(|x| !matches!(x, PyValue::None) && !strip(&x.py_str()).is_empty())
            .map(|x| strip(&x.py_str()).to_string())
            .collect(),
        Some(_) => {
            return Err(SmError::value(
                "Proposal 'supersedes' must be a list of item ids",
            ));
        }
    };

    let refs_raw = coerce_str_or_list(dict_get(&raw, "refs"))?;
    let tags_raw = coerce_str_or_list(dict_get(&raw, "tags"))?;
    let source = field(&raw, "source");
    let as_of_raw = match dict_get(&raw, "as_of") {
        Some(v) if truthy(v) => v.py_str(),
        _ => String::new(),
    };
    let as_of = validate_as_of(Some(&as_of_raw), false, false)?;
    check_stored_as_of(&as_of)?;
    let item_id = new_item_id(None);
    let refs = merge_refs(&[
        Some(RefsArg::Str(&refs_raw)),
        Some(RefsArg::List(extract_refs(Some(&format!(
            "{title}\n{body}"
        ))))),
    ]);

    Ok(Item {
        refs,
        tags: tags_raw,
        source,
        evidence,
        as_of,
        supersedes: sup_list,
        ..Item::bare(&item_id, &repo, &kind, &title, &body, &tag)
    }
    .post_init())
}

/// `strip_code_fences(text)`.
pub fn strip_code_fences(text: &str) -> String {
    let cleaned = strip(text);
    if !cleaned.starts_with("```") {
        return cleaned.to_string();
    }
    let lines: Vec<&str> = splitlines(cleaned, false)
        .into_iter()
        .filter(|ln| !strip(ln).starts_with("```"))
        .collect();
    strip(&lines.join("\n")).to_string()
}

/// `load_proposals_with_rejects(path)`: the items, and the count of lines that raised: a line
/// is one proposal, so its error (a bad JSON row, a field that cannot convert) rejects it alone.
pub fn load_proposals_with_rejects(path: &str) -> Result<(Vec<Item>, usize), SmError> {
    let p = PyPath::new(path).expanduser()?.resolve()?;
    let files = if p.is_file() {
        vec![p]
    } else if p.is_dir() {
        sorted_paths(p.rglob("*.jsonl")?)
    } else {
        return Err(SmError::new(
            Base::Os,
            "FileNotFoundError",
            format!("Proposal path not found: {p}"),
        ));
    };

    let mut items = Vec::new();
    let mut rejected = 0;
    for f in &files {
        let text = f
            .read_text(Errors::Strict)
            .map_err(|e| file_error(f.as_str(), e))?;
        let cleaned = strip_code_fences(&text);
        for raw_line in splitlines(&cleaned, false) {
            let s = strip(raw_line);
            if s.is_empty() {
                continue;
            }
            match parse_proposal_line(s) {
                Ok(it) => items.push(it),
                Err(_) => rejected += 1,
            }
        }
    }
    Ok((items, rejected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    fn row(over: &[(&str, &str)]) -> String {
        let mut m: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"id": "custom_id_to_ignore", "repo": "anvil", "kind": "decision", "title": "Use FTS5 unicode61",
                "body": "Use FTS5 unicode61 with CLI fold for R5.", "refs": ["D9"], "tags": ["fts5", "search"],
                "tag": "verified", "evidence": "docs/fts_test.py", "as_of": "2026-10-25T10:00:00+00:00",
                "source": "turn:chat-1", "supersedes": ["legacy:memories:1", "legacy:memories:2"]}"#,
        )
        .unwrap();
        for (k, v) in over {
            m.insert(k.to_string(), serde_json::from_str(v).unwrap());
        }
        Value::Object(m).to_string()
    }

    #[test]
    fn proposal_parser_and_validation() {
        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        let f = d.path().join("prop.jsonl");
        fs::write(&f, row(&[]) + "\n").unwrap();
        let (items, rejected) = load_proposals_with_rejects(f.to_str().unwrap()).unwrap();
        assert_eq!((items.len(), rejected), (1, 0));
        let it = &items[0];
        assert_ne!(it.id, "custom_id_to_ignore");
        assert!(
            it.id.starts_with("m_") && it.id.chars().count() == 28,
            "{}",
            it.id
        );
        assert_eq!(it.tag, "verified");
        assert_eq!(it.supersedes, ["legacy:memories:1", "legacy:memories:2"]);
        assert_eq!(it.refs, "D9 FTS5 R5");
        assert_eq!(
            (it.tags.as_str(), it.source.as_str(), it.as_of.as_str()),
            (
                "fts5 search",
                "turn:chat-1",
                crate::core::item::validate_as_of(Some("2026-10-25T10:00:00+00:00"), false, false)
                    .unwrap()
                    .as_str()
            )
        );

        let err = |over: &[(&str, &str)]| parse_proposal_line(&row(over)).unwrap_err();
        for over in [[("tag", r#""n/a""#)], [("tag", r#""bad-tag""#)]] {
            let e = err(&over);
            assert!(
                e.is(Base::Value) && e.msg.starts_with("Invalid tag"),
                "{}",
                e.msg
            );
        }
        for over in [
            [("tag", r#""verified""#), ("evidence", r#""""#)],
            [("tag", r#""executed""#), ("evidence", r#""  ""#)],
        ] {
            let e = err(&over);
            assert!(e.msg.contains("needs non-empty evidence"), "{}", e.msg);
        }
        assert!(
            err(&[("kind", r#""invalid_kind""#)])
                .msg
                .starts_with("Invalid kind")
        );
        assert!(
            err(&[("as_of", r#""not-an-iso-date""#)])
                .msg
                .starts_with("Invalid ISO 8601")
        );
    }

    #[test]
    fn value_shapes_and_their_errors() {
        let it = parse_proposal_line(&row(&[
            ("refs", r#"" a1b2c3d, D9 ""#),
            ("tags", "null"),
            ("supersedes", "null"),
        ]))
        .unwrap();
        assert_eq!(
            (it.refs.as_str(), it.tags.as_str(), it.supersedes.len()),
            ("a1b2c3d D9 FTS5 R5", "", 0)
        );
        let it = parse_proposal_line(&row(&[
            ("tags", r#"[" x ", null, "", 0, false, 1.5]"#),
            ("supersedes", r#"[null, " s1 ", 0]"#),
        ]))
        .unwrap();
        assert_eq!(
            it.tags, "x 0 False 1.5",
            "None is skipped; 0 and False are kept as str()"
        );
        assert_eq!(it.supersedes, ["s1", "0"]);
        let it = parse_proposal_line(&row(&[("title", "42"), ("body", r#"{"k": [1]}"#)])).unwrap();
        assert_eq!((it.title.as_str(), it.body.as_str()), ("42", "{'k': [1]}"));

        let text =
            |over: &[(&str, &str)]| parse_proposal_line(&row(over)).unwrap_err().python_text();
        assert_eq!(
            text(&[("tags", "7")]),
            "ValueError: Expected string or list of strings, got int"
        );
        assert_eq!(
            text(&[("refs", r#"{"a": 1}"#)]),
            "ValueError: Expected string or list of strings, got dict"
        );
        assert_eq!(
            text(&[("supersedes", r#""s1""#)]),
            "ValueError: Proposal 'supersedes' must be a list of item ids"
        );
        assert_eq!(
            text(&[("body", "0")]),
            "ValueError: Proposal row requires non-empty repo, kind, title, and body"
        );
        assert_eq!(
            parse_proposal_line("[1]").unwrap_err().python_text(),
            "TypeError: Proposal line must be a JSON object"
        );
        assert_eq!(
            parse_proposal_line("{").unwrap_err().python_name(),
            "JSONDecodeError"
        );
    }

    #[test]
    fn fences_and_rejects_over_a_folder() {
        assert_eq!(strip_code_fences("  ```json\na\n ``` x\nb\n```  "), "a\nb");
        assert_eq!(
            strip_code_fences("a\n```\nb"),
            "a\n```\nb",
            "a fence only counts at the start"
        );
        assert_eq!(strip_code_fences(" \n "), "");

        let d = tempfile::tempdir_in(crate::sources::TEST_TMP).unwrap();
        fs::create_dir_all(d.path().join("b")).unwrap();
        fs::write(
            d.path().join("b/2.jsonl"),
            format!("```jsonl\n{}\n```\n", row(&[])),
        )
        .unwrap();
        fs::write(
            d.path().join("a.jsonl"),
            format!("{}\n\n[1]\n{{bad\n{}\r\n", row(&[("tags", "3")]), row(&[])),
        )
        .unwrap();
        fs::write(d.path().join("skip.txt"), "{bad").unwrap();
        let (items, rejected) = load_proposals_with_rejects(d.path().to_str().unwrap()).unwrap();
        assert_eq!((items.len(), rejected), (2, 3));

        fs::write(d.path().join("c.jsonl"), b"\xff").unwrap();
        let e = load_proposals_with_rejects(d.path().to_str().unwrap()).unwrap_err();
        assert_eq!(
            e.python_name(),
            "UnicodeDecodeError",
            "a file that is not UTF-8 is not a reject"
        );
        let gone = d.path().join("gone");
        assert_eq!(
            load_proposals_with_rejects(gone.to_str().unwrap())
                .unwrap_err()
                .python_text(),
            format!(
                "FileNotFoundError: Proposal path not found: {}",
                gone.display()
            )
        );
    }
}
