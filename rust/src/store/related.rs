use std::collections::HashSet;

use indexmap::IndexMap;
use rusqlite::{Connection, OptionalExtension};

use crate::core::fold::fold;
use crate::core::item::Item;
use crate::core::refs::{RefsArg, source_session_key, split_refs};
use crate::error::SmError;
use crate::store::items::{ITEM_COLUMNS, bind_all, bind_text, row_to_item};

fn cols() -> String {
    ITEM_COLUMNS.join(", ")
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn items_where(conn: &Connection, where_sql: &str, params: &[&str]) -> Result<Vec<Item>, SmError> {
    bind_all(params)?;
    let mut stmt = conn.prepare(&format!("SELECT {} FROM items WHERE {where_sql}", cols()))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), row_to_item)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `supersede_chain(conn, item_id)`: the items it replaced ("replaces", breadth first), then the
/// ones that replaced it ("replaced-by"), any status.
pub fn supersede_chain(
    conn: &Connection,
    item_id: &str,
) -> Result<Vec<(Item, &'static str)>, SmError> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([item_id.to_string()]);
    let mut frontier = vec![item_id.to_string()];
    while !frontier.is_empty() {
        let refs: Vec<&str> = frontier.iter().map(String::as_str).collect();
        let rows = items_where(
            conn,
            &format!("superseded_by IN ({})", marks(refs.len())),
            &refs,
        )?;
        frontier = Vec::new();
        for r in rows {
            if seen.insert(r.id.clone()) {
                frontier.push(r.id.clone());
                out.push((r, "replaces"));
            }
        }
    }
    bind_text(item_id)?;
    let mut nxt: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM items WHERE id = ?",
            [item_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    while let Some(n) = nxt.filter(|n| !n.is_empty() && !seen.contains(n)) {
        seen.insert(n.clone());
        let Some(r) = items_where(conn, "id = ?", &[&n])?.into_iter().next() else {
            break;
        };
        nxt = r.superseded_by.clone();
        out.push((r, "replaced-by"));
    }
    Ok(out)
}

/// {id: shared refs} of active items.
pub type Holders = IndexMap<String, Vec<String>>;

/// `items_sharing_refs(conn, refs, max_holders=...)`: ({id: shared refs} of active items, refs
/// held by more than max_holders active items). Refs compare folded.
pub fn items_sharing_refs<S: AsRef<str>>(
    conn: &Connection,
    refs: &[S],
    max_holders: i64,
) -> Result<(Holders, Vec<String>), SmError> {
    let mut wanted: IndexMap<String, String> = IndexMap::new();
    for r in refs.iter().map(AsRef::as_ref).filter(|r| !r.is_empty()) {
        wanted.insert(fold(Some(r)), r.to_string());
    }
    let mut holders: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut per_ref: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut stmt =
        conn.prepare("SELECT id, refs FROM items WHERE status = 'active' AND refs != ''")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, item_refs) in rows {
        let mut folded: Vec<String> = Vec::new();
        for t in split_refs(Some(&RefsArg::Str(&item_refs))) {
            let f = fold(Some(&t));
            if !folded.contains(&f) {
                folded.push(f);
            }
        }
        let hit: Vec<String> = folded
            .iter()
            .filter_map(|f| wanted.get(f).cloned())
            .collect();
        if !hit.is_empty() {
            for h in &hit {
                *per_ref.entry(h.clone()).or_default() += 1;
            }
            holders.insert(id, hit);
        }
    }
    let common: Vec<String> = wanted
        .values()
        .filter(|r| per_ref.get(*r).copied().unwrap_or(0) > max_holders)
        .cloned()
        .collect();
    let mut kept = IndexMap::new();
    for (id, hit) in holders {
        let shared: Vec<String> = hit.into_iter().filter(|r| !common.contains(r)).collect();
        if !shared.is_empty() {
            kept.insert(id, shared);
        }
    }
    Ok((kept, common))
}

/// `items_by_id(conn, ids)`: {id: item} in the order SQLite returns them.
pub fn items_by_id<S: AsRef<str>>(
    conn: &Connection,
    ids: &[S],
) -> Result<IndexMap<String, Item>, SmError> {
    let list: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
    let rows = items_where(conn, &format!("id IN ({})", marks(list.len())), &list)?;
    Ok(rows.into_iter().map(|it| (it.id.clone(), it)).collect())
}

/// `session_items(conn, key)`: active items whose source names the session key.
pub fn session_items(conn: &Connection, key: &str) -> Result<Vec<Item>, SmError> {
    let like = format!("%{key}%");
    let rows = items_where(conn, "status = 'active' AND source LIKE ?", &[&like])?;
    Ok(rows
        .into_iter()
        .filter(|it| source_session_key(Some(&it.source)).as_deref() == Some(key))
        .collect())
}
