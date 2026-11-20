//! The `related` command and its text output.

use crate::cli::query::{HEADER_REFS_CHARS, HOOK_TITLE_CHARS, compact_refs};
use crate::core::item::Item;
use crate::error::{Base, SmError};
use crate::py;
use crate::py::argparse::Namespace;
use crate::py::io::Out;
use crate::py::json::{Opts, dumps_value};
use crate::service::query::{item_text, item_tokens};
use crate::service::related::{Related, related_json, run_related};
use crate::service::usage::log_usage;

pub const RELATED_SNIPPET_CHARS: usize = 200;

/// `_print_related(target, related, skipped, fmt)` as one string, without the final newline.
pub fn format_related(target: &Item, related: &[Related], skipped: &[String], fmt: &str) -> String {
    if fmt == "json" {
        return dumps_value(&related_json(target, related, skipped), &Opts::unicode());
    }
    let title = py::str::slice(
        &py::str::split(&target.title).join(" "),
        None,
        Some(HOOK_TITLE_CHARS as isize),
    )
    .to_string();
    let mut lines = vec![format!(
        "related to [{}] {} {} {}: {title}",
        target.id,
        target.repo,
        target.kind,
        py::str::slice(&target.as_of, None, Some(10))
    )];
    for (n, r) in related.iter().enumerate() {
        let it = &r.item;
        let mut parts = vec![
            format!("[{}]", it.id),
            format!("#{}", n + 1),
            format!("repo={}", it.repo),
            format!("kind={}", it.kind),
            format!("tag={}", it.tag),
            format!("as_of={}", it.as_of),
            format!("~{} tok", item_tokens(it)),
            format!("rel={}", r.relation.join(",")),
        ];
        if !r.shared_refs.is_empty() {
            parts.push(format!(
                "shared={}",
                compact_refs(&r.shared_refs.join(","), HEADER_REFS_CHARS)
            ));
        }
        if it.status != "active" {
            parts.push(format!("status={}", it.status));
        }
        let text = item_text(it);
        let snippet = if py::str::len(&text) <= RELATED_SNIPPET_CHARS {
            text
        } else {
            format!(
                "{}...",
                py::str::rstrip(py::str::slice(
                    &text,
                    None,
                    Some(RELATED_SNIPPET_CHARS as isize - 3)
                ))
            )
        };
        lines.push(parts.join(" "));
        lines.push(format!("  {snippet}"));
    }
    if !skipped.is_empty() {
        lines.push(format!("skipped refs (too common): {}", skipped.join(" ")));
    }
    if !related.is_empty() {
        let ids: Vec<&str> = related.iter().take(3).map(|r| r.item.id.as_str()).collect();
        lines.push(format!("next: sm show {}", ids.join(" ")));
    }
    lines.join("\n")
}

pub fn run(ns: &Namespace, out: &mut Out) -> Result<i32, SmError> {
    let id = ns.get_str("id");
    let (target, related, skipped) = match run_related(id, ns.get_int("limit"), None) {
        Ok(found) => found,
        Err(e) if e.is(Base::Key) => {
            out.eprint(&format!("not found: {id}"))?;
            return Ok(1);
        }
        Err(e) => return Err(e),
    };
    out.print(&format_related(
        &target,
        &related,
        &skipped,
        ns.get_str("format"),
    ))?;
    let mut ids = vec![target.id];
    ids.extend(related.iter().map(|r| r.item.id.clone()));
    log_usage("related", &ids, None, None, None)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str, body: &str) -> Item {
        Item {
            id: id.into(),
            repo: "r".into(),
            kind: "fact".into(),
            title: title.into(),
            body: body.into(),
            tag: "decided".into(),
            refs: String::new(),
            tags: String::new(),
            source: String::new(),
            evidence: String::new(),
            as_of: "2026-11-01T10:00:00+00:00".into(),
            status: "active".into(),
            superseded_by: None,
            content_hash: String::new(),
            supersedes: Vec::new(),
        }
    }

    #[test]
    fn text_lists_each_item_with_its_relation_and_the_next_command() {
        let t = item("m_t", "Target  title", "body");
        let mut old = item("m_o", "Old", "x".repeat(250).as_str());
        old.status = "superseded".into();
        let rel = vec![
            Related {
                item: old,
                relation: vec!["replaces".into()],
                shared_refs: vec![],
            },
            Related {
                item: item("m_s", "S", "S body"),
                relation: vec!["refs".into()],
                shared_refs: vec!["A-1".into(), "B-2".into()],
            },
        ];
        let text = format_related(&t, &rel, &["common".into()], "text");
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "related to [m_t] r fact 2026-11-01: Target title");
        assert_eq!(
            lines[1],
            "[m_o] #1 repo=r kind=fact tag=decided as_of=2026-11-01T10:00:00+00:00 ~64 tok rel=replaces status=superseded"
        );
        // 200 chars: "Old — " (6) + 191 x + "..."
        assert_eq!(lines[2], format!("  Old — {}...", "x".repeat(191)));
        assert_eq!(
            lines[3],
            "[m_s] #2 repo=r kind=fact tag=decided as_of=2026-11-01T10:00:00+00:00 ~2 tok rel=refs shared=A-1,B-2"
        );
        assert_eq!(lines[4], "  S body");
        assert_eq!(lines[5], "skipped refs (too common): common");
        assert_eq!(lines[6], "next: sm show m_o m_s");
    }

    #[test]
    fn no_related_items_prints_only_the_header() {
        let t = item("m_t", "T", "b");
        assert_eq!(
            format_related(&t, &[], &[], "text"),
            "related to [m_t] r fact 2026-11-01: T"
        );
        assert_eq!(
            format_related(&t, &[], &[], "json"),
            r#"{"id": "m_t", "skipped_refs": [], "results": []}"#
        );
    }
}
