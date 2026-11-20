use crate::config::{aliases, get_db_path, load_config, resolve_repo, roots};
use crate::core::item::{Item, new_item_id, validate_as_of};
use crate::core::redact::redact_text;
use crate::core::refs::{RefsArg, item_refs, split_refs};
use crate::core::tags::{validate_kind, validate_tag};
use crate::error::SmError;
use crate::py::str::{slice, splitlines, strip};
use crate::store::connect::connect;
use crate::store::items::{content_hash_checked, put_item};

/// The fields `run_put` takes; every str defaults to "" and kind to "fact".
#[derive(Clone, Debug)]
pub struct PutArgs<'a> {
    pub tag: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub evidence: &'a str,
    pub refs: &'a str,
    pub tags: &'a str,
    pub source: &'a str,
    pub repo: &'a str,
    pub as_of: &'a str,
    pub supersedes: RefsArg<'a>,
    pub home: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

impl Default for PutArgs<'_> {
    fn default() -> Self {
        PutArgs {
            tag: "",
            kind: "fact",
            title: "",
            evidence: "",
            refs: "",
            tags: "",
            source: "",
            repo: "",
            as_of: "",
            supersedes: RefsArg::List(Vec::new()),
            home: None,
            cwd: None,
        }
    }
}

/// `run_put(body, ...)`: (id, created); a body the repo already holds gives that item's id.
pub fn run_put(body: &str, a: &PutArgs) -> Result<(String, bool), SmError> {
    let clean_body = strip(body);
    if clean_body.is_empty() {
        return Err(SmError::value("Item body cannot be empty"));
    }
    let valid_tag = validate_tag(Some(a.tag), Some(a.evidence), false)?;
    let valid_kind = validate_kind(Some(a.kind))?;
    let resolved_as_of = if a.as_of.is_empty() {
        String::new()
    } else {
        validate_as_of(Some(a.as_of), false, false)?
    };

    let cfg = load_config(a.home)?;
    let resolved_repo = if strip(a.repo).is_empty() {
        resolve_repo(a.cwd, &aliases(&cfg), &roots(&cfg)?)?
    } else {
        strip(a.repo).to_string()
    };
    // Check stand-in surrogate bytes on the raw body before redacting or connecting.
    let _ = content_hash_checked(clean_body)?;
    let redacted_body = redact_text(clean_body, None, None);
    let redacted_evidence = redact_text(strip(a.evidence), None, None);
    let resolved_title = if strip(a.title).is_empty() {
        let first = splitlines(&redacted_body, false)
            .first()
            .copied()
            .unwrap_or("");
        slice(first, None, Some(120)).to_string()
    } else {
        redact_text(strip(a.title), None, None)
    };
    let resolved_refs = item_refs(a.refs, &resolved_title, &redacted_body, &redacted_evidence);
    let sup_list = split_refs(Some(&a.supersedes));

    let id = new_item_id(None);
    let content_hash = content_hash_checked(&redacted_body)?;
    let item = Item {
        refs: resolved_refs,
        tags: strip(a.tags).into(),
        source: strip(a.source).into(),
        evidence: redacted_evidence,
        as_of: resolved_as_of,
        content_hash,
        supersedes: sup_list.clone(),
        ..Item::bare(
            &id,
            &resolved_repo,
            &valid_kind,
            &resolved_title,
            &redacted_body,
            &valid_tag,
        )
    }
    .post_init();

    let conn = connect(get_db_path(a.home)?.as_str(), None)?;
    put_item(&conn, &item, Some(&sup_list))
}

#[cfg(test)]
mod tests {
    // The put, show and supersede tests; each home in a tempdir.
    use super::*;
    use crate::service::show::{run_show, run_show_many};
    use crate::service::supersede::run_supersede;

    fn put<'a>(home: &'a str, tag: &'a str, f: impl FnOnce(&mut PutArgs<'a>)) -> PutArgs<'a> {
        let mut a = PutArgs {
            tag,
            repo: "anvil",
            home: Some(home),
            ..PutArgs::default()
        };
        f(&mut a);
        a
    }

    fn err(body: &str, a: &PutArgs) -> String {
        run_put(body, a).unwrap_err().python_text()
    }

    #[test]
    fn service_put_show_supersede() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().to_str().unwrap();
        assert_eq!(
            err("Missing tag text", &put(h, "", |_| {})),
            "ValueError: Evidence tag is required; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned"
        );
        assert_eq!(
            err("Verified without evidence", &put(h, "verified", |_| {})),
            "ValueError: Tag 'verified' requires non-empty --evidence"
        );
        assert_eq!(
            err(" \n ", &put(h, "decided", |_| {})),
            "ValueError: Item body cannot be empty"
        );
        assert!(
            err("Body", &put(h, "decided", |a| a.kind = "not-a-kind"))
                .starts_with("ValueError: Invalid kind 'not-a-kind'")
        );
        assert_eq!(
            err("Body", &put(h, "decided", |a| a.as_of = "not-a-date")),
            "ValueError: Invalid ISO 8601 timestamp 'not-a-date'"
        );

        let body = "R5 queue lag café schema sync";
        let (id1, created) = run_put(
            body,
            &put(h, "verified", |a| {
                a.evidence = "pytest -q";
                a.refs = "R5";
            }),
        )
        .unwrap();
        assert!(created);
        assert_eq!(
            run_put(body, &put(h, "verified", |a| a.evidence = "pytest -q")).unwrap(),
            (id1.clone(), false)
        );
        let shown = run_show(&id1, Some(h)).unwrap();
        assert_eq!(
            (shown.id.as_str(), shown.tag.as_str(), shown.title.as_str()),
            (id1.as_str(), "verified", body)
        );
        assert_eq!(run_show(&format!(" {id1} "), Some(h)).unwrap().id, id1);
        assert_eq!(
            run_show("m_nope", Some(h)).unwrap_err().python_text(),
            "KeyError: 'Item not found: m_nope'"
        );

        let (id2, _) = run_put(
            "R5 replacement fix after refactor",
            &put(h, "executed", |a| {
                a.evidence = "commit 1234567";
                a.refs = "R5";
            }),
        )
        .unwrap();
        assert_eq!(
            run_supersede(&id1, &id2, Some(h)).unwrap(),
            (id1.clone(), id2.clone())
        );
        let old = run_show(&id1, Some(h)).unwrap();
        assert_eq!(
            (old.status.as_str(), old.superseded_by),
            ("superseded", Some(id2.clone()))
        );
        assert_eq!(
            run_supersede(&id2, &id2, Some(h))
                .unwrap_err()
                .python_text(),
            "ValueError: Cannot supersede an item with itself"
        );

        let (found, missing) =
            run_show_many(&[id2.as_str(), " ", "m_nope", &id1, &id2], Some(h)).unwrap();
        assert_eq!(
            found.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            [id2.as_str(), id1.as_str()]
        );
        assert_eq!(missing, ["m_nope"]);
    }

    #[test]
    fn service_put_takes_refs_from_evidence_and_title_from_the_first_line() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().to_str().unwrap();
        let (id, _) = run_put("Rows may differ by 30 cents", &put(h, "decided", |a| {
            a.evidence = "session 0a1b2c3d; tasklog 20261101-101010-0a1b2c3d-made-up-slug-9f9f; commit ab12cd3";
            a.refs = "R4 ab12cd3";
        }))
        .unwrap();
        assert_eq!(
            run_show(&id, Some(h)).unwrap().refs,
            "R4 ab12cd3 20261101-101010-0a1b2c3d-made-up-slug-9f9f 0a1b2c3d"
        );
        let (plain, _) = run_put(
            "Mia prefers short replies",
            &put(h, "decided", |a| a.evidence = "Mia in chat"),
        )
        .unwrap();
        assert_eq!(run_show(&plain, Some(h)).unwrap().refs, "");
        let long = format!("  {}\nsecond line", "x".repeat(130));
        let (id, _) = run_put(
            &long,
            &put(h, "decided", |a| a.supersedes = RefsArg::Str(&plain)),
        )
        .unwrap();
        let it = run_show(&id, Some(h)).unwrap();
        assert_eq!(it.title, "x".repeat(120));
        assert_eq!(run_show(&plain, Some(h)).unwrap().superseded_by, Some(id));
    }
}
