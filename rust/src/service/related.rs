use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::config::get_db_path;
use crate::core::item::{Item, parse_as_of_epoch};
use crate::core::refs::{RefsArg, source_session_key, split_refs};
use crate::error::SmError;
use crate::py::str::strip;
use crate::service::query::item_tokens;
use crate::store::connect::connect;
use crate::store::items::get_item;
use crate::store::related::{items_by_id, items_sharing_refs, session_items, supersede_chain};

pub const DEFAULT_RELATED_LIMIT: i64 = 10;
/// A ref held by more active items than this (a repo path, a common skill) links nothing useful.
pub const MAX_REF_HOLDERS: i64 = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Related {
    pub item: Item,
    pub relation: Vec<String>,
    pub shared_refs: Vec<String>,
}

/// `run_related(item_id, limit=..., home=...)`: (the item, related items in rank order, refs
/// skipped as too common). Rank: the supersede chain first, then the most shared refs, then the
/// newest.
pub fn run_related(
    item_id: &str,
    limit: i64,
    home: Option<&str>,
) -> Result<(Item, Vec<Related>, Vec<String>), SmError> {
    if limit <= 0 {
        return Err(SmError::value("--limit must be at least 1"));
    }
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    let target = get_item(&conn, strip(item_id))?
        .ok_or_else(|| SmError::key(&format!("Item not found: {item_id}")))?;
    let mut found: IndexMap<String, Related> = IndexMap::new();
    fn entry(found: &mut IndexMap<String, Related>, it: Item) -> &mut Related {
        found.entry(it.id.clone()).or_insert_with(|| Related {
            item: it,
            relation: Vec::new(),
            shared_refs: Vec::new(),
        })
    }
    for (it, rel) in supersede_chain(&conn, &target.id)? {
        entry(&mut found, it).relation.push(rel.to_string());
    }
    let (mut sharing, skipped) = items_sharing_refs(
        &conn,
        &split_refs(Some(&RefsArg::Str(&target.refs))),
        MAX_REF_HOLDERS,
    )?;
    sharing.shift_remove(&target.id);
    let missing_ids: Vec<&String> = sharing.keys().filter(|i| !found.contains_key(*i)).collect();
    let mut loaded = items_by_id(&conn, &missing_ids)?;
    for (i, refs) in sharing {
        let it = match found.get(&i) {
            Some(r) => r.item.clone(),
            // `loaded[i]`: an id that left the store between the two reads is a KeyError
            None => loaded.shift_remove(&i).ok_or_else(|| SmError::key(&i))?,
        };
        let rel = entry(&mut found, it);
        rel.relation.push("refs".into());
        rel.shared_refs = refs;
    }
    if let Some(key) = source_session_key(Some(&target.source)).filter(|k| !k.is_empty()) {
        for it in session_items(&conn, &key)? {
            if it.id != target.id {
                entry(&mut found, it).relation.push("session".into());
            }
        }
    }
    drop(conn);
    let mut keyed = Vec::with_capacity(found.len());
    for r in found.into_values() {
        let on_chain = r
            .relation
            .iter()
            .any(|x| x == "replaces" || x == "replaced-by");
        let epoch = parse_as_of_epoch(Some(&r.item.as_of))?;
        keyed.push(((on_chain, r.shared_refs.len(), epoch, r.item.id.clone()), r));
    }
    // `sorted(..., reverse=True)` is stable; the keys end in the id, which is unique
    keyed.sort_by(|(a, _), (b, _)| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(b.2.total_cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
    });
    let ranked: Vec<Related> = keyed
        .into_iter()
        .map(|(_, r)| r)
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect();
    Ok((target, ranked, skipped))
}

/// The object `sm related --format json` prints and the MCP tool `memory_related` returns.
pub fn related_json(target: &Item, related: &[Related], skipped: &[String]) -> Value {
    let results: Vec<Value> = related
        .iter()
        .enumerate()
        .map(|(n, r)| {
            json!({
                "rank": n + 1,
                "id": r.item.id,
                "repo": r.item.repo,
                "kind": r.item.kind,
                "tag": r.item.tag,
                "as_of": r.item.as_of,
                "status": r.item.status,
                "title": r.item.title,
                "tokens": item_tokens(&r.item),
                "relation": r.relation,
                "shared_refs": r.shared_refs,
            })
        })
        .collect();
    json!({"id": target.id, "skipped_refs": skipped, "results": results})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::connect::connect;
    use crate::store::items::put_item;

    fn put(
        conn: &rusqlite::Connection,
        id: &str,
        body: &str,
        refs: &str,
        source: &str,
        as_of: &str,
        sup: &[&str],
    ) {
        let mut it = Item::new(id, "a", "fact", body, body, "derived");
        it.refs = refs.into();
        it.source = source.into();
        if !as_of.is_empty() {
            it.as_of = as_of.into();
        }
        let sup: Vec<String> = sup.iter().map(|s| s.to_string()).collect();
        put_item(conn, &it, if sup.is_empty() { None } else { Some(&sup) }).unwrap();
    }

    /// The CLI related test at the service level.
    #[test]
    fn related_ranks_the_chain_then_shared_refs_then_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();
        let sess = "0a1b2c3d-1111-2222-3333-444455556666";
        put(&conn, "m_old", "Plan v1", "R6", "", "", &[]);
        put(
            &conn,
            "m_mid",
            "Plan v2",
            "R6 abc1234 common.md",
            &format!("handoff:20261101-090000-{sess}.md"),
            "2026-11-01T00:00:00+00:00",
            &["m_old"],
        );
        put(
            &conn,
            "m_new",
            "Plan v3",
            "",
            "",
            "2026-10-01T00:00:00+00:00",
            &["m_mid"],
        );
        put(
            &conn,
            "m_both",
            "Shares two refs",
            "R6 abc1234",
            "",
            "2026-09-01T00:00:00+00:00",
            &[],
        );
        put(
            &conn,
            "m_one",
            "Shares one ref",
            "R6",
            "",
            "2026-11-02T00:00:00+00:00",
            &[],
        );
        put(
            &conn,
            "m_sess",
            "Task log of the session",
            "",
            "task:20261101-090000-0a1b2c3d-plan-1a2b",
            "2026-08-01T00:00:00+00:00",
            &[],
        );
        for n in 0..21 {
            put(
                &conn,
                &format!("m_c{n:02}"),
                &format!("Common path holder {n}"),
                "common.md",
                "",
                "",
                &[],
            );
        }
        put(
            &conn,
            "m_other",
            "Other session",
            "",
            "task:20261101-090000-ffffffff-x-1a2b",
            "",
            &[],
        );
        drop(conn);
        let (target, related, skipped) =
            run_related("m_mid", DEFAULT_RELATED_LIMIT, Some(home)).unwrap();
        assert_eq!(target.id, "m_mid");
        assert_eq!(skipped, ["common.md"]);
        let got: Vec<(&str, Vec<&str>, Vec<&str>)> = related
            .iter()
            .map(|r| {
                (
                    r.item.id.as_str(),
                    r.relation.iter().map(String::as_str).collect(),
                    r.shared_refs.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("m_old", vec!["replaces"], vec![]),
                ("m_new", vec!["replaced-by"], vec![]),
                ("m_both", vec!["refs"], vec!["R6", "abc1234"]),
                ("m_one", vec!["refs"], vec!["R6"]),
                ("m_sess", vec!["session"], vec![]),
            ]
        );
        assert_eq!(
            (
                related[0].item.status.as_str(),
                related[1].item.status.as_str()
            ),
            ("superseded", "active")
        );
        let (_, two, _) = run_related("m_mid", 2, Some(home)).unwrap();
        assert_eq!(
            two.iter().map(|r| r.item.id.as_str()).collect::<Vec<_>>(),
            ["m_old", "m_new"]
        );
        assert_eq!(
            run_related("m_gone", 1, Some(home)).unwrap_err().base,
            crate::error::Base::Key
        );
        assert_eq!(
            run_related("m_mid", 0, Some(home)).unwrap_err().msg,
            "--limit must be at least 1"
        );
    }

    #[test]
    fn an_item_with_no_link_has_no_related_and_a_bare_hex_is_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();
        put(&conn, "m_lone", "Lone item", "", "note:x", "", &[]);
        put(&conn, "m_none", "Other item", "", "note:None", "", &[]);
        put(
            &conn,
            "m_first",
            "Session card",
            "R9",
            "session:0a1b2c3d-1111-2222-3333-444455556666",
            "",
            &[],
        );
        put(
            &conn,
            "m_second",
            "Task of that session",
            "R9",
            "task:20261101-090000-0a1b2c3d-x-1a2b",
            "",
            &[],
        );
        put(
            &conn,
            "m_hex",
            "Note naming the hex",
            "",
            "note:0a1b2c3d",
            "",
            &[],
        );
        drop(conn);
        assert!(run_related("m_lone", 10, Some(home)).unwrap().1.is_empty());
        let (_, rel, _) = run_related("m_first", 10, Some(home)).unwrap();
        assert_eq!(rel.len(), 1);
        assert_eq!(
            (
                rel[0].item.id.as_str(),
                &rel[0].relation,
                &rel[0].shared_refs
            ),
            (
                "m_second",
                &vec!["refs".to_string(), "session".to_string()],
                &vec!["R9".to_string()]
            )
        );
    }
}
