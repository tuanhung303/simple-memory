//! Lane S wrote `ITEM_COLUMNS` and `row_to_item`; lane D1 the rest.

use std::collections::HashSet;

use indexmap::IndexMap;
use rusqlite::{Connection, OptionalExtension, params};

use crate::core::fold::fold;
use crate::core::item::{
    Item, check_stored_as_of, compute_content_hash, new_item_id, now_iso, validate_as_of,
};
use crate::core::redact::{redact_for_store, redact_text, refs_a_match_touches};
use crate::core::refs::{RefsArg, extract_refs, item_refs, split_refs};
use crate::core::tags::{
    LEGACY_NA_TAG, RETIRED_TAG_PREFIX, append_tag, has_retired_tag, strip_retired_tags,
    validate_kind, validate_tag,
};
use crate::error::{Base, SmError};
use crate::py::str::{slice, split, splitlines, strip};
use crate::store::connect::{execute, transaction};

/// `_ITEM_COLUMNS`: the columns every item SELECT reads, in this order.
pub const ITEM_COLUMNS: [&str; 14] = [
    "id",
    "repo",
    "kind",
    "title",
    "body",
    "refs",
    "tags",
    "source",
    "tag",
    "evidence",
    "as_of",
    "status",
    "superseded_by",
    "content_hash",
];

/// `row_to_item(row)`: columns by name, then `Item.__post_init__` (an empty as_of becomes now,
/// an empty content_hash the hash of the body).
pub fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let s = |name: &str| row.get::<_, String>(name);
    Ok(Item {
        id: s("id")?,
        repo: s("repo")?,
        kind: s("kind")?,
        title: s("title")?,
        body: s("body")?,
        tag: s("tag")?,
        refs: s("refs")?,
        tags: s("tags")?,
        source: s("source")?,
        evidence: s("evidence")?,
        as_of: s("as_of")?,
        status: s("status")?,
        superseded_by: row.get("superseded_by")?,
        content_hash: s("content_hash")?,
        supersedes: Vec::new(),
    }
    .post_init())
}

/// Python's sqlite3 binds a str as UTF-8: a stand-in (a surrogateescape byte from argv or a
/// `\udcXX` JSON escape) raises UnicodeEncodeError, positions counted in that parameter.
pub fn bind_text(s: &str) -> Result<(), SmError> {
    crate::py::path::encode_text(s).map(|_| ())
}

/// Every str parameter of one statement, in binding order.
pub fn bind_all(params: &[&str]) -> Result<(), SmError> {
    params.iter().try_for_each(|s| bind_text(s))
}

/// `compute_content_hash(body)` with the encode error Python's `body.strip().encode()` raises.
pub fn content_hash_checked(body: &str) -> Result<String, SmError> {
    bind_text(strip(body))?;
    Ok(compute_content_hash(body))
}

/// `redact_for_store(item)`, whose `compute_content_hash` of a changed body can raise.
pub fn redact_checked(item: &Item) -> Result<Item, SmError> {
    let r = redact_for_store(item, None);
    if r.body != item.body {
        bind_text(strip(&r.body))?;
    }
    Ok(r)
}

fn one_str(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Option<String>, SmError> {
    Ok(conn
        .query_row(sql, p, |r| r.get::<_, String>(0))
        .optional()?)
}

/// (status, superseded_by) of an id.
fn status_of(conn: &Connection, id: &str) -> Result<Option<(String, Option<String>)>, SmError> {
    bind_text(id)?;
    Ok(conn
        .query_row(
            "SELECT status, superseded_by FROM items WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The rows the UPDATE changed.
pub fn validate_and_supersede_one(
    conn: &Connection,
    old_id: &str,
    new_id: &str,
) -> Result<i64, SmError> {
    let oid = strip(old_id);
    let nid = strip(new_id);
    if oid == nid {
        return Err(SmError::value("Cannot supersede an item with itself"));
    }
    let Some(old_row) = status_of(conn, oid)? else {
        return Err(SmError::not_found(oid));
    };
    if status_of(conn, nid)?.is_none() {
        return Err(SmError::not_found(nid));
    }
    let mut visited: HashSet<String> = HashSet::from([oid.to_string()]);
    let mut curr: Option<String> = Some(nid.to_string());
    while let Some(c) = curr {
        if visited.contains(&c) {
            return Err(SmError::value(format!(
                "Supersede cycle detected: {oid} -> {nid}"
            )));
        }
        visited.insert(c.clone());
        let Some((status, sup)) = status_of(conn, &c)? else {
            break;
        };
        // `str(x).strip() if x else None`: a blank pointer is Some(""), which is falsy below.
        let nxt = sup.filter(|s| !s.is_empty()).map(|s| strip(&s).to_string());
        let nxt_truthy = nxt.as_deref().is_some_and(|s| !s.is_empty());
        if c == nid && (status == "superseded" || nxt_truthy) {
            if nxt.as_deref() == Some(oid) {
                return Err(SmError::value(format!(
                    "Supersede cycle detected: {oid} -> {nid}"
                )));
            }
            return Err(SmError::value(format!(
                "Cannot supersede {oid} with already-superseded item {nid}"
            )));
        }
        curr = nxt;
    }
    if old_row.0 == "superseded" && old_row.1.as_deref().unwrap_or("") == nid {
        return Ok(0);
    }
    Ok(conn.execute(
        "UPDATE items SET status = 'superseded', superseded_by = ? WHERE id = ?",
        params![nid, oid],
    )? as i64)
}

pub fn mark_superseded<S: AsRef<str>>(
    conn: &Connection,
    old_ids: &[S],
    new_id: &str,
) -> Result<i64, SmError> {
    let mut changed = 0;
    for old_id in old_ids {
        let oid = strip(old_id.as_ref());
        if oid.is_empty() {
            continue;
        }
        changed += validate_and_supersede_one(conn, oid, new_id)?;
    }
    Ok(changed)
}

pub fn allow_na_tag(item: &Item) -> bool {
    let p = |s: &str| s.starts_with("legacy:") || s.starts_with("turn:");
    p(&item.id) || p(&item.source)
}

/// The checks put_item, put_items_batch and import_items run, in Python's order.
fn validate_item(item: &Item) -> Result<(), SmError> {
    validate_kind(Some(&item.kind))?;
    validate_tag(Some(&item.tag), Some(&item.evidence), allow_na_tag(item))?;
    check_stored_as_of(&item.as_of)
}

/// A task log or handoff file: one item source per file.
pub fn file_backed(source: &str) -> bool {
    source.starts_with("task:") || source.starts_with("handoff:")
}

/// The source an amend gives a task log or handoff item: `amended:` and the file's source. It
/// is not the file's source, so the importer never takes the amended row for the file's
/// current version, and it keeps the item in a dedupe slot of its own.
const AMENDED_PREFIX: &str = "amended:";

/// The source's slot in the `items_dedupe` index (migration 0003): a file's own source or an
/// amended file item's source, or '' for every other item.
pub fn dedupe_slot(source: &str) -> &str {
    if file_backed(source) || source.strip_prefix(AMENDED_PREFIX).is_some_and(file_backed) {
        source
    } else {
        ""
    }
}

const SLOT_SQL: &str = "CASE WHEN substr(source, 1, 5) = 'task:' OR substr(source, 1, 8) = 'handoff:' \
                        OR substr(source, 1, 13) = 'amended:task:' \
                        OR substr(source, 1, 16) = 'amended:handoff:' THEN source ELSE '' END";

/// The active item of a repo, other than `other_than`, whose body an item from `source` repeats:
/// any row for an item no file backs, else a row of the same file only, so neither another file
/// nor a manual or amended row stands in for a file.
fn active_by_hash(
    conn: &Connection,
    repo: &str,
    hash: &str,
    source: &str,
    other_than: Option<&str>,
) -> Result<Option<String>, SmError> {
    let slot = dedupe_slot(source);
    bind_all(&[repo, hash, slot])?;
    one_str(
        conn,
        "SELECT id FROM items WHERE repo = ?1 AND content_hash = ?2 AND status = 'active' \
         AND id IS NOT ?4 AND (?3 = '' OR source = ?3)",
        params![repo, hash, slot, other_than],
    )
}

/// The active item, other than `other_than`, that holds the `items_dedupe` slot an active item
/// from `source` needs.
pub fn active_in_slot(
    conn: &Connection,
    repo: &str,
    hash: &str,
    source: &str,
    other_than: Option<&str>,
) -> Result<Option<String>, SmError> {
    let slot = dedupe_slot(source);
    bind_all(&[repo, hash, slot])?;
    one_str(
        conn,
        &format!(
            "SELECT id FROM items WHERE repo = ?1 AND content_hash = ?2 AND status = 'active' \
             AND id IS NOT ?4 AND {SLOT_SQL} = ?3"
        ),
        params![repo, hash, slot, other_than],
    )
}

const INSERT_SQL: &str = "INSERT INTO items (\
id, repo, kind, title, body, refs, tags, source, tag, evidence, \
as_of, status, superseded_by, content_hash\
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

pub fn insert_item(
    conn: &Connection,
    item: &Item,
    status: &str,
    superseded_by: Option<&str>,
) -> Result<(), SmError> {
    bind_all(&[
        &item.id,
        &item.repo,
        &item.kind,
        &item.title,
        &item.body,
        &item.refs,
        &item.tags,
        &item.source,
        &item.tag,
        &item.evidence,
        &item.as_of,
        status,
    ])?;
    if let Some(s) = superseded_by {
        bind_text(s)?;
    }
    bind_text(&item.content_hash)?;
    conn.execute(
        INSERT_SQL,
        params![
            item.id,
            item.repo,
            item.kind,
            item.title,
            item.body,
            item.refs,
            item.tags,
            item.source,
            item.tag,
            item.evidence,
            item.as_of,
            status,
            superseded_by,
            item.content_hash
        ],
    )?;
    Ok(())
}

/// `put_item(conn, item, supersedes=...)`: (id, created). A body the repo's active items already
/// hold gives that item's id and False.
pub fn put_item(
    conn: &Connection,
    item: &Item,
    supersedes: Option<&[String]>,
) -> Result<(String, bool), SmError> {
    let item = redact_checked(item)?;
    validate_item(&item)?;
    let sup_list: Vec<String> = supersedes
        .unwrap_or(&item.supersedes)
        .iter()
        .filter(|s| !strip(s).is_empty())
        .map(|s| strip(s).to_string())
        .collect();
    transaction(conn, None, |conn| {
        if let Some(existing_id) =
            active_by_hash(conn, &item.repo, &item.content_hash, &item.source, None)?
        {
            let other: Vec<&String> = sup_list.iter().filter(|o| **o != existing_id).collect();
            if !other.is_empty() {
                mark_superseded(conn, &other, &existing_id)?;
            }
            return Ok((existing_id, false));
        }
        insert_item(conn, &item, &item.status, item.superseded_by.as_deref())?;
        if !sup_list.is_empty() {
            mark_superseded(conn, &sup_list, &item.id)?;
        }
        Ok((item.id.clone(), true))
    })
}

/// The active ids of `source`, in `repo` only when one is given: a task log or handoff names
/// its file by task id or file name, which another repo can repeat, so it reads its own repo;
/// an auto-memory source names one file of one project.
fn active_ids_of_source(
    conn: &Connection,
    source: &str,
    repo: Option<&str>,
) -> Result<Vec<String>, SmError> {
    bind_text(source)?;
    if let Some(r) = repo {
        bind_text(r)?;
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM items WHERE source = ?1 AND status = 'active' AND (?2 IS NULL OR repo = ?2)",
    )?;
    let ids = stmt
        .query_map(params![source, repo], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// (superseded, unresolved).
pub fn resolve_tasklog_chains(
    conn: &Connection,
    pending_tasklog_sup: &[(String, String, String)],
    touched_tasks: &[(String, String)],
) -> Result<(i64, Vec<String>), SmError> {
    let mut edges: Vec<(String, String, String)> = pending_tasklog_sup.to_vec();
    let mut seen_edge_items: HashSet<String> = edges.iter().map(|e| e.0.clone()).collect();
    for (target_id, this_task_id) in touched_tasks {
        let marker = format!("superseded_by:{this_task_id}");
        let like = format!("%{marker}%");
        bind_text(&like)?;
        let mut stmt = conn.prepare(
            "SELECT id, source, tags FROM items WHERE status = 'active' AND tags LIKE ?",
        )?;
        let rows = stmt
            .query_map([&like], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (wid, wsource, wtags) in rows {
            if wid == *target_id || seen_edge_items.contains(&wid) {
                continue;
            }
            let has = wtags
                .split(',')
                .map(strip)
                .any(|t| !t.is_empty() && t == marker);
            if has {
                let src = if wsource.is_empty() {
                    wid.clone()
                } else {
                    wsource
                };
                edges.push((wid.clone(), src, this_task_id.clone()));
                seen_edge_items.insert(wid);
            }
        }
    }

    let mut unresolved: Vec<String> = Vec::new();
    let mut resolved: Vec<(String, String, String, String)> = Vec::new();
    let mut next_ptr: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (item_id, item_source, target_ref) in &edges {
        let target_source = if target_ref.starts_with("task:") {
            target_ref.clone()
        } else {
            format!("task:{target_ref}")
        };
        bind_all(&[&target_source, target_ref])?;
        let mut target = one_str(
            conn,
            "SELECT id FROM items WHERE (source = ? OR id = ?) AND status = 'active' ORDER BY rowid DESC LIMIT 1",
            [&target_source, target_ref],
        )?;
        if target.is_none() {
            target = one_str(
                conn,
                "SELECT id FROM items WHERE source = ? OR id = ? ORDER BY rowid DESC LIMIT 1",
                [&target_source, target_ref],
            )?;
        }
        match target {
            Some(tid) if tid != *item_id => {
                resolved.push((
                    item_id.clone(),
                    item_source.clone(),
                    target_ref.clone(),
                    tid.clone(),
                ));
                next_ptr.insert(item_id.clone(), tid);
            }
            _ => unresolved.push(format!("{item_source} -> {target_ref}")),
        }
    }

    let next_node = |node: &str| -> Result<Option<String>, SmError> {
        if let Some(n) = next_ptr.get(node) {
            return Ok(Some(n.clone()));
        }
        let row: Option<Option<String>> = conn
            .query_row(
                "SELECT superseded_by FROM items WHERE id = ?",
                [node],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match row.flatten() {
            Some(s) if !s.is_empty() => Some(strip(&s).to_string()).filter(|s| !s.is_empty()),
            _ => None,
        })
    };

    let mut superseded = 0;
    for (item_id, item_source, target_ref, target_id) in &resolved {
        let mut has_cycle = false;
        let mut visited: HashSet<String> = HashSet::from([item_id.clone()]);
        let mut curr = Some(target_id.clone());
        while let Some(c) = curr {
            if visited.contains(&c) {
                has_cycle = true;
                break;
            }
            visited.insert(c.clone());
            curr = next_node(&c)?;
        }
        if has_cycle {
            unresolved.push(format!("{item_source} -> {target_ref}"));
            continue;
        }
        superseded += conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = ? \
             WHERE id = ? AND (status != 'superseded' OR superseded_by IS NOT ?)",
            params![target_id, item_id, target_id],
        )? as i64;
    }
    Ok((superseded, unresolved))
}

/// The result of `put_items_batch`: (inserted, deduped, superseded, supersede_missing,
/// unresolved, failed_rows).
pub type BatchResult = (i64, i64, i64, i64, Vec<String>, Vec<String>);

/// put_items_batch catches these per row: ValueError, KeyError, TypeError, sqlite3.Error.
fn row_error(e: &SmError) -> bool {
    matches!(e.base, Base::Value | Base::Key | Base::Type | Base::Sqlite)
}

/// The item `by` that replaced a row of `repo` and `source` has that source in another repo: a
/// task log or handoff hidden by the rule before one item per repo.
fn hidden_by_another_repo(
    conn: &Connection,
    repo: &str,
    source: &str,
    by: &str,
) -> Result<bool, SmError> {
    bind_all(&[repo, source, by])?;
    Ok(one_str(
        conn,
        "SELECT id FROM items WHERE id = ?3 AND source = ?2 AND repo != ?1",
        params![repo, source, by],
    )?
    .is_some())
}

/// The fields of a stored task log or handoff row the import compares with the file's item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileRow {
    pub title: String,
    pub tags: String,
    pub tag: String,
    pub evidence: String,
    pub as_of: String,
    pub refs: String,
}

/// The same time: one instant, or one wall-clock time, since a date alone or a time with no
/// offset is read in the local zone of the run (`validate_as_of`).
fn same_as_of(stored: &str, file: &str) -> bool {
    use chrono::DateTime;
    match (
        DateTime::parse_from_rfc3339(stored),
        DateTime::parse_from_rfc3339(file),
    ) {
        (Ok(a), Ok(b)) => a == b || a.naive_local() == b.naive_local(),
        _ => stored == file,
    }
}

/// The stored row of a task log or handoff holds what the file's (redacted) item gives: the
/// same title, tag and set of tags; the same evidence when it is a task log's commit list
/// (`executed`; a `derived` item's evidence is the file path, which moves with the repo); the
/// same as_of unless it came from the file's mtime; and every ref the file states (a task log's
/// task id, commits, continues, superseded_by), since a later step (`fill_refs`) may add refs. Refs from the title and body
/// are left out: those fields are compared themselves, and a newer ref extractor must not make
/// a new version of every file.
pub fn file_row_holds(row: &FileRow, item: &Item) -> bool {
    let tokens = |t: &str| -> HashSet<String> {
        t.split(',')
            .map(strip)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect()
    };
    let file_tags = tokens(&item.tags);
    let stored_refs: HashSet<String> = split_refs(Some(&RefsArg::Str(&row.refs)))
        .into_iter()
        .collect();
    let text_refs: HashSet<String> = extract_refs(Some(&format!("{}\n{}", item.title, item.body)))
        .into_iter()
        .collect();
    row.title == item.title
        && tokens(&row.tags) == file_tags
        && row.tag == item.tag
        && (item.tag != "executed" || row.evidence == item.evidence)
        && (file_tags.contains("as_of:mtime") || same_as_of(&row.as_of, &item.as_of))
        && split_refs(Some(&RefsArg::Str(&item.refs)))
            .iter()
            .all(|r| text_refs.contains(r) || stored_refs.contains(r))
}

fn file_row(conn: &Connection, id: &str) -> Result<FileRow, SmError> {
    bind_text(id)?;
    Ok(conn.query_row(
        "SELECT title, tags, tag, evidence, as_of, refs FROM items WHERE id = ?",
        [id],
        |r| {
            Ok(FileRow {
                title: r.get(0)?,
                tags: r.get(1)?,
                tag: r.get(2)?,
                evidence: r.get(3)?,
                as_of: r.get(4)?,
                refs: r.get(5)?,
            })
        },
    )?)
}

/// A task log or handoff item whose stored row `id` does not hold its fields.
fn file_row_differs(conn: &Connection, id: &str, item: &Item) -> Result<bool, SmError> {
    Ok(file_backed(&item.source) && !file_row_holds(&file_row(conn, id)?, item))
}

/// The newest row of the file's repo, source and body that holds the file's fields: an
/// earlier version to make active again in place of a new one. Called when no active row of
/// that body holds them, so each row it reads is superseded.
fn holding_row(conn: &Connection, item: &Item) -> Result<Option<String>, SmError> {
    bind_all(&[&item.repo, &item.source, &item.content_hash])?;
    let mut stmt = conn.prepare(
        "SELECT id FROM items WHERE repo = ? AND source = ? AND content_hash = ? \
         ORDER BY rowid DESC",
    )?;
    let ids = stmt
        .query_map([&item.repo, &item.source, &item.content_hash], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        if !file_row_differs(conn, &id, item)? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Supersede the active row `old` by `by` before `by` takes its `items_dedupe` slot (the amend
/// order: `by` may not be stored yet).
fn supersede_first(conn: &Connection, old: &str, by: &str) -> Result<i64, SmError> {
    bind_all(&[old, by])?;
    Ok(conn.execute(
        "UPDATE items SET status = 'superseded', superseded_by = ? WHERE id = ? AND status = 'active'",
        params![by, old],
    )? as i64)
}

/// A stored row of a file-backed `source` in a concrete repo (`repo != 'unassigned'`), matching
/// `doctor::import_lag_by_repo`: excludes cross-repo `hidden` rows where another repo's item of
/// the same source replaced it, so a degraded/jailed scan never suppresses valid restoration while
/// never inserting an `unassigned` duplicate over a known repo.
fn stored_in_known_repo(conn: &Connection, source: &str) -> Result<Option<String>, SmError> {
    bind_text(source)?;
    let mut stmt = conn.prepare(
        "SELECT i.id, i.status, i.superseded_by, i.tags FROM items i \
         LEFT JOIN items b ON b.id = i.superseded_by \
         WHERE i.source = ?1 AND i.repo != 'unassigned' \
         AND (i.status = 'active' \
              OR (i.superseded_by IS NOT NULL AND ifnull(b.source = i.source AND b.repo != i.repo, 0) = 0) \
              OR i.tags LIKE '%retired:%') \
         ORDER BY (i.status = 'active') DESC, i.rowid DESC",
    )?;
    let mut rows = stmt.query([source])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let status: String = r.get(1)?;
        let superseded_by: Option<String> = r.get(2)?;
        let tags: String = r.get(3)?;
        if status == "active" || superseded_by.is_some() || has_retired_tag(&tags) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// One row of put_items_batch inside its savepoint: (inserted, deduped, superseded, missing, target).
fn batch_row(
    conn: &Connection,
    item: &Item,
    apply_supersedes: bool,
) -> Result<(i64, i64, i64, i64, String), SmError> {
    validate_item(item)?;
    if item.repo == "unassigned"
        && file_backed(&item.source)
        && let Some(known_id) = stored_in_known_repo(conn, &item.source)?
    {
        return Ok((0, 1, 0, 0, known_id));
    }
    let (mut ins, mut dup, mut sup, mut missing) = (0, 0, 0, 0);
    let mut existing = active_by_hash(conn, &item.repo, &item.content_hash, &item.source, None)?;
    // a task log or handoff whose title, tags, tag, evidence, as_of or refs changed with its
    // body kept is a changed file: its active row is replaced like the row of a changed body
    let mut stale: Option<String> = None;
    if let Some(id) = &existing
        && file_row_differs(conn, id, item)?
    {
        stale = existing.take();
    }
    let mut restore_id: Option<String> = None;
    if stale.is_some() {
        restore_id = holding_row(conn, item)?;
    } else if existing.is_none() && file_backed(&item.source) {
        bind_all(&[&item.repo, &item.source, &item.content_hash])?;
        let prior: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT id, superseded_by, tags FROM items \
                 WHERE repo = ? AND source = ? AND content_hash = ? ORDER BY rowid DESC LIMIT 1",
                [&item.repo, &item.source, &item.content_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match prior {
            Some((id, None, tags)) => {
                if has_retired_tag(&tags) {
                    existing = Some(id);
                } else {
                    restore_id = holding_row(conn, item)?;
                }
            }
            // Restore an earlier body when the file's own source still has an active version
            // (reverted A -> B -> A), or when another repo's item of the same source replaced it
            // (the rule before one item per repo); keep items an amend or a manual supersede
            // replaced alone.
            Some((id, Some(by), _)) => {
                if active_ids_of_source(conn, &item.source, Some(&item.repo))?.is_empty()
                    && !hidden_by_another_repo(conn, &item.repo, &item.source, &by)?
                {
                    existing = Some(id);
                } else {
                    restore_id = holding_row(conn, item)?;
                }
            }
            None => {}
        }
    }
    let others = |conn: &Connection, keep: &str| -> Result<Vec<String>, SmError> {
        Ok(active_ids_of_source(conn, &item.source, Some(&item.repo))?
            .into_iter()
            .filter(|id| id != keep && Some(id) != stale.as_ref())
            .collect())
    };
    let target_id;
    if let Some(id) = existing {
        target_id = id;
        dup = 1;
    } else if let Some(rid) = restore_id {
        let old_same_source = others(conn, &rid)?;
        if let Some(s) = &stale {
            sup += supersede_first(conn, s, &rid)?;
        }
        conn.execute(
            "UPDATE items SET status = 'active', superseded_by = NULL WHERE id = ?",
            [&rid],
        )?;
        target_id = rid;
        ins = 1;
        if !old_same_source.is_empty() {
            sup += mark_superseded(conn, &old_same_source, &target_id)?;
        }
    } else {
        bind_text(&item.id)?;
        if let Some(id) = one_str(conn, "SELECT id FROM items WHERE id = ?", [&item.id])? {
            target_id = id;
            dup = 1;
        } else {
            let old_same_source = if file_backed(&item.source) {
                others(conn, &item.id)?
            } else {
                Vec::new()
            };
            if let Some(s) = &stale {
                sup += supersede_first(conn, s, &item.id)?;
            }
            insert_item(conn, item, "active", None)?;
            target_id = item.id.clone();
            ins = 1;
            if !old_same_source.is_empty() {
                sup += mark_superseded(conn, &old_same_source, &target_id)?;
            }
        }
    }
    if apply_supersedes {
        for old_id in &item.supersedes {
            let oid = strip(old_id);
            if oid.is_empty() || oid == target_id {
                continue;
            }
            bind_text(oid)?;
            if one_str(conn, "SELECT id FROM items WHERE id = ?", [oid])?.is_none() {
                missing += 1;
            } else {
                sup += validate_and_supersede_one(conn, oid, &target_id)?;
            }
        }
    }
    Ok((ins, dup, sup, missing, target_id))
}

/// `put_items_batch(conn, items, apply_supersedes=...)`: one transaction, one savepoint per row; a
/// row that fails a check rolls back alone and lands in failed_rows.
pub fn put_items_batch(
    conn: &Connection,
    items: &[Item],
    apply_supersedes: bool,
) -> Result<BatchResult, SmError> {
    transaction(conn, None, |conn| {
        let (mut inserted, mut deduped, mut superseded, mut supersede_missing) = (0, 0, 0, 0);
        let mut failed_rows: Vec<String> = Vec::new();
        let mut pending: Vec<(String, String, String)> = Vec::new();
        let mut touched: Vec<(String, String)> = Vec::new();
        for (idx, raw) in items.iter().enumerate() {
            let sp = format!("sp_row_{idx}");
            execute(conn, &format!("SAVEPOINT {sp}"))?;
            let mut item = raw.clone();
            let res = redact_checked(raw).and_then(|redacted| {
                item = redacted;
                batch_row(conn, &item, apply_supersedes)
            });
            let (ins, dup, sup, missing, target_id) = match res {
                Ok(v) => {
                    execute(conn, &format!("RELEASE SAVEPOINT {sp}"))?;
                    v
                }
                Err(e) if row_error(&e) => {
                    execute(conn, &format!("ROLLBACK TO SAVEPOINT {sp}"))?;
                    execute(conn, &format!("RELEASE SAVEPOINT {sp}"))?;
                    failed_rows.push(format!("{}: {}", item.id, e.msg));
                    continue;
                }
                Err(e) => return Err(e),
            };
            inserted += ins;
            deduped += dup;
            superseded += sup;
            supersede_missing += missing;
            if let Some(task) = item.source.strip_prefix("task:") {
                if !task.is_empty() {
                    touched.push((target_id.clone(), task.to_string()));
                }
                if let Some(sb) = item.superseded_by.as_deref().filter(|s| !s.is_empty()) {
                    pending.push((target_id, item.source.clone(), sb.to_string()));
                }
            }
        }
        let (tl, unresolved) = resolve_tasklog_chains(conn, &pending, &touched)?;
        superseded += tl;
        Ok((
            inserted,
            deduped,
            superseded,
            supersede_missing,
            unresolved,
            failed_rows,
        ))
    })
}

const SYNC_FIELDS: [&str; 8] = [
    "repo",
    "kind",
    "title",
    "content_hash",
    "tags",
    "tag",
    "evidence",
    "as_of",
];

fn sync_field<'a>(item: &'a Item, f: &str) -> &'a str {
    match f {
        "repo" => &item.repo,
        "kind" => &item.kind,
        "title" => &item.title,
        "content_hash" => &item.content_hash,
        "tags" => &item.tags,
        "tag" => &item.tag,
        "evidence" => &item.evidence,
        _ => &item.as_of,
    }
}

/// A source's newest row: status, superseded_by, then the `SYNC_FIELDS` values.
type NewestRow = (String, Option<String>, Vec<String>);

fn newest_row(r: &rusqlite::Row, first: usize) -> rusqlite::Result<NewestRow> {
    Ok((
        r.get(first)?,
        r.get(first + 1)?,
        (first + 2..first + 2 + SYNC_FIELDS.len())
            .map(|i| r.get(i))
            .collect::<rusqlite::Result<_>>()?,
    ))
}

/// The newest row holds `item` and was not retired by file deletion (superseded with no
/// replacement and no manual `retired:<reason>` tag).
fn newest_holds(newest: Option<&NewestRow>, item: &Item) -> bool {
    match newest {
        Some((status, sup, vals)) => {
            let tags_val = SYNC_FIELDS
                .iter()
                .zip(vals)
                .find(|(f, _)| **f == "tags")
                .map_or("", |(_, v)| v.as_str());
            let retired = status == "superseded" && sup.is_none() && !has_retired_tag(tags_val);
            !retired
                && SYNC_FIELDS.iter().zip(vals).all(|(f, v)| {
                    if *f == "tags" {
                        strip_retired_tags(v) == strip_retired_tags(sync_field(item, f))
                    } else {
                        v == sync_field(item, f)
                    }
                })
        }
        None => false,
    }
}

/// The active id of another source holding the same body.
fn other_active_id(conn: &Connection, item: &Item) -> Result<Option<String>, SmError> {
    bind_all(&[&item.repo, &item.content_hash])?;
    one_str(
        conn,
        "SELECT id FROM items WHERE repo = ? AND content_hash = ? AND status = 'active' AND source != ?",
        [&item.repo, &item.content_hash, &item.source],
    )
}

/// (current, active ids of the source, active id of another source
/// holding the same body).
pub fn source_state(
    conn: &Connection,
    item: &Item,
) -> Result<(bool, Vec<String>, Option<String>), SmError> {
    bind_text(&item.source)?;
    let newest: Option<NewestRow> = conn
        .query_row(
            &format!(
                "SELECT status, superseded_by, {} FROM items WHERE source = ? ORDER BY rowid DESC LIMIT 1",
                SYNC_FIELDS.join(", ")
            ),
            [&item.source],
            |r| newest_row(r, 0),
        )
        .optional()?;
    let active = active_ids_of_source(conn, &item.source, None)?;
    let other_id = other_active_id(conn, item)?;
    let same = newest_holds(newest.as_ref(), item);
    Ok((same || other_id.is_some(), active, other_id))
}

/// The newest row of every source that starts with `prefix`, by source, from one query. A
/// source whose newest row has a cell that is not text, or that has an active row whose id is
/// not text, stays out of the map: its item then takes the per-item path, which reads those rows
/// again and returns that error at the same item as before.
fn newest_rows(
    conn: &Connection,
    prefix: &str,
) -> Result<std::collections::HashMap<String, NewestRow>, SmError> {
    // shortcut: the prefix filter is speed only (no result depends on it, since a source
    // outside the prefix is never looked up); it keeps the scan to automemory sources instead
    // of every source in the store, so no test can fail without it.
    let mut stmt = conn.prepare(&format!(
        "SELECT source, status, superseded_by, {} FROM items WHERE rowid IN \
         (SELECT max(rowid) FROM items WHERE substr(source, 1, ?) = ? AND source NOT IN \
         (SELECT source FROM items WHERE status = 'active' AND typeof(id) != 'text' \
         AND source IS NOT NULL) GROUP BY source)",
        SYNC_FIELDS.join(", ")
    ))?;
    let mut newest = std::collections::HashMap::new();
    let mut rows = stmt.query(params![prefix.chars().count() as i64, prefix])?;
    while let Some(r) = rows.next()? {
        if let (Ok(source), Ok(row)) = (r.get::<_, String>(0), newest_row(r, 1)) {
            newest.insert(source, row);
        }
    }
    Ok(newest)
}

/// `sync_source_item` over many items of sources that start with `prefix`: (added, updated,
/// unchanged).
///
/// One query reads the newest row of every such source. An item that row already holds,
/// with no other source's active row holding its body, is unchanged with no transaction, so a
/// run over unchanged files takes no write lock and skips the two per-item source lookups (no
/// index on `source`: on a 6,175-item store they took 82% of a 173-file run). Any other item
/// goes through `sync_source_item`, which reads its state again under the lock, and its source
/// leaves the scan: that sync may insert a newer row of the source, so a later item of the same
/// source takes the full path too. No sync changes another source's newest row in a way the
/// check reads (it inserts rows of its own source; it never retires a row).
pub fn sync_source_items<'a>(
    conn: &Connection,
    prefix: &str,
    items: impl IntoIterator<Item = &'a Item>,
) -> Result<(i64, i64, i64), SmError> {
    let mut newest = newest_rows(conn, prefix)?;
    let (mut added, mut updated, mut unchanged) = (0, 0, 0);
    for item in items {
        let red = redact_checked(item)?;
        let held =
            newest_holds(newest.get(&red.source), &red) && other_active_id(conn, &red)?.is_none();
        let state = if held {
            "unchanged"
        } else {
            newest.remove(&red.source);
            sync_source_item(conn, item)?
        };
        match state {
            "added" => added += 1,
            "updated" => updated += 1,
            _ => unchanged += 1,
        }
    }
    Ok((added, updated, unchanged))
}

/// `file_item_is_current(conn, item)`: the per-item answer `file_items_are_current` gives with
/// one scan.
#[cfg(test)]
pub fn file_item_is_current(conn: &Connection, item: &Item) -> Result<bool, SmError> {
    Ok(source_state(conn, &redact_checked(item)?)?.0)
}

/// `file_item_is_current` of each item, for items of sources that start with `prefix`, with one
/// scan of the newest rows (`newest_rows`) in place of the two source lookups per item: in
/// `sm doctor --imports` on a 6,178-item store those took 658 to 772 ms of 1.2 s for 175 files.
/// An item whose source the scan did not read takes the per-item lookup (`source_state`).
pub fn file_items_are_current<'a>(
    conn: &Connection,
    prefix: &str,
    items: impl IntoIterator<Item = &'a Item>,
) -> Result<Vec<bool>, SmError> {
    // shortcut: the scan is speed only (an item that takes `source_state` gets the same
    // answer), so no test fails when every item takes it.
    let newest = newest_rows(conn, prefix)?;
    items
        .into_iter()
        .map(|item| {
            let red = redact_checked(item)?;
            let Some(row) = newest.get(&red.source) else {
                return Ok(source_state(conn, &red)?.0);
            };
            bind_text(&red.source)?;
            let same = newest_holds(Some(row), &red);
            Ok(other_active_id(conn, &red)?.is_some() || same)
        })
        .collect()
}

/// `sync_source_item(conn, item)`: `added`, `updated` or `unchanged`, in one transaction.
pub fn sync_source_item(conn: &Connection, item: &Item) -> Result<&'static str, SmError> {
    let item = redact_checked(item)?;
    transaction(conn, None, |conn| {
        let (current, active, other_id) = source_state(conn, &item)?;
        if let Some(other) = other_id {
            let rest: Vec<&String> = active.iter().filter(|a| **a != other).collect();
            mark_superseded(conn, &rest, &other)?;
            return Ok("unchanged");
        }
        if current {
            return Ok("unchanged");
        }
        for a in &active {
            bind_text(&item.id)?;
            conn.execute(
                "UPDATE items SET status = 'superseded', superseded_by = ? WHERE id = ?",
                params![item.id, a],
            )?;
        }
        put_item(conn, &item, None)?;
        Ok(if active.is_empty() {
            "added"
        } else {
            "updated"
        })
    })
}

/// `retire_item(conn, item_id)`: 1 when an active item was superseded with no replacement.
pub fn retire_item(conn: &Connection, item_id: &str) -> Result<i64, SmError> {
    transaction(conn, None, |conn| {
        bind_text(item_id)?;
        Ok(conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = NULL WHERE id = ? AND status = 'active'",
            [item_id],
        )? as i64)
    })
}

/// Normalize and redact a manual retire reason: collapse whitespace, redact secrets, re-collapse;
/// empty becomes `"manual"`; commas are rejected.
pub(crate) fn normalize_retire_reason(reason: &str) -> Result<String, SmError> {
    let collapsed = split(reason).join(" ");
    let redacted = redact_text(&collapsed, None, None);
    let clean = split(&redacted).join(" ");
    if clean.contains(',') {
        return Err(SmError::value("Retire reason cannot contain commas"));
    }
    if clean.is_empty() {
        Ok("manual".to_string())
    } else {
        Ok(clean)
    }
}

/// `retire_item_with_reason(conn, item_id, reason)`: mark an active item retired with a
/// normalized `retired:<reason>` tag; returns `(clean_id, clean_reason)`.
pub fn retire_item_with_reason(
    conn: &Connection,
    item_id: &str,
    reason: &str,
) -> Result<(String, String), SmError> {
    let id = strip(item_id);
    bind_text(id)?;
    let clean_reason = normalize_retire_reason(reason)?;
    bind_text(&clean_reason)?;
    transaction(conn, None, |conn| {
        let row: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT status, superseded_by, tags FROM items WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((status, superseded_by, tags)) = row else {
            return Err(SmError::not_found(id));
        };
        let sup = superseded_by
            .as_deref()
            .map(strip)
            .filter(|s| !s.is_empty());
        if status != "active" {
            return Err(match sup {
                Some(by) => SmError::value(format!("Item {id} is already superseded by {by}")),
                None => SmError::value(format!("Item {id} is already retired")),
            });
        }
        let base_tags = strip_retired_tags(&tags);
        let tag_token = format!("{RETIRED_TAG_PREFIX}{clean_reason}");
        let new_tags = append_tag(
            if base_tags.is_empty() {
                None
            } else {
                Some(base_tags.as_str())
            },
            &tag_token,
        );
        bind_text(&new_tags)?;
        conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = NULL, tags = ? \
             WHERE id = ? AND status = 'active'",
            params![new_tags, id],
        )?;
        Ok((id.to_string(), clean_reason))
    })
}

/// Optional field overrides for `amend_item`; `None` preserves the old item's value.
#[derive(Clone, Debug, Default)]
pub struct AmendPatch<'a> {
    pub repo: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub evidence: Option<&'a str>,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub add_refs: Option<&'a str>,
    pub remove_refs: Option<&'a str>,
    pub replace_refs: Option<&'a str>,
    pub add_tags: Option<&'a str>,
    pub remove_tags: Option<&'a str>,
    pub replace_tags: Option<&'a str>,
    pub as_of: Option<&'a str>,
}

impl AmendPatch<'_> {
    pub fn is_empty(&self) -> bool {
        self.repo.is_none()
            && self.kind.is_none()
            && self.tag.is_none()
            && self.evidence.is_none()
            && self.title.is_none()
            && self.body.is_none()
            && self.add_refs.is_none()
            && self.remove_refs.is_none()
            && self.replace_refs.is_none()
            && self.add_tags.is_none()
            && self.remove_tags.is_none()
            && self.replace_tags.is_none()
            && self.as_of.is_none()
    }
}

fn default_title(body: &str) -> String {
    let first = splitlines(strip(body), false)
        .first()
        .copied()
        .unwrap_or("");
    slice(first, None, Some(120)).to_string()
}

/// `amend_item(conn, old_id, patch)`: insert a revised active copy of `old_id`, superseding the
/// old row atomically; returns `(new_id, body_changed)`.
pub fn amend_item(
    conn: &Connection,
    old_id: &str,
    patch: &AmendPatch<'_>,
) -> Result<(String, bool), SmError> {
    if patch.is_empty() {
        return Err(SmError::value("amend needs at least one field to change"));
    }
    let old_id = strip(old_id);
    bind_text(old_id)?;
    transaction(conn, None, |conn| {
        let Some(old) = get_item(conn, old_id)? else {
            return Err(SmError::not_found(old_id));
        };
        if old.status != "active" {
            return Err(match crate::store::upkeep::chain_head(conn, &old.id)? {
                Some(head) => SmError::value(format!(
                    "Cannot amend superseded item {old_id} (active head: {head})"
                )),
                None => SmError::value(format!("Cannot amend retired item {old_id}")),
            });
        }
        let new_repo = match patch.repo {
            Some(r) => {
                let clean = strip(r);
                if clean.is_empty() {
                    return Err(SmError::value("Repo cannot be empty"));
                }
                clean.to_string()
            }
            None => old.repo.clone(),
        };
        let new_kind = patch
            .kind
            .map(str::to_string)
            .unwrap_or_else(|| old.kind.clone());
        let new_body = match patch.body {
            Some(b) => {
                let clean = strip(b);
                if clean.is_empty() {
                    return Err(SmError::value("Item body cannot be empty"));
                }
                bind_text(clean)?;
                redact_text(clean, None, None)
            }
            None => old.body.clone(),
        };
        let auto_title =
            patch.title.is_none() && patch.body.is_some() && old.title == default_title(&old.body);
        let new_title = match patch.title {
            Some(t) => {
                let clean = strip(t);
                if clean.is_empty() {
                    return Err(SmError::value("Item title cannot be empty"));
                }
                bind_text(clean)?;
                redact_text(clean, None, None)
            }
            None => {
                if auto_title {
                    default_title(&new_body)
                } else {
                    old.title.clone()
                }
            }
        };
        let new_evidence = match patch.evidence {
            Some(e) => {
                let clean = strip(e);
                bind_text(clean)?;
                redact_text(clean, None, None)
            }
            None => old.evidence.clone(),
        };
        let new_tag = patch
            .tag
            .map(str::to_string)
            .unwrap_or_else(|| old.tag.clone());
        let mut new_source = if file_backed(&old.source) {
            format!("{AMENDED_PREFIX}{}", old.source)
        } else if old.source.starts_with("automemory:") {
            String::new()
        } else {
            old.source.clone()
        };
        if patch.tag.is_none()
            && old.tag == LEGACY_NA_TAG
            && new_source.is_empty()
            && (old.id.starts_with("legacy:") || old.id.starts_with("turn:"))
        {
            new_source = old.id.clone();
        }

        let old_ref_tokens = split_refs(Some(&RefsArg::Str(&old.refs)));
        let mut extracted_refs: Vec<String> = Vec::new();
        if new_title != old.title {
            extracted_refs.extend(extract_refs(Some(&new_title)));
        }
        if new_body != old.body {
            extracted_refs.extend(extract_refs(Some(&new_body)));
        }
        if new_evidence != old.evidence {
            extracted_refs.extend(extract_refs(Some(&new_evidence)));
        }
        let mut merged_refs: Vec<String> = Vec::new();
        let mut seen_refs: HashSet<String> = HashSet::new();
        if let Some(rep) = patch.replace_refs {
            for tok in split_refs(Some(&RefsArg::Str(rep)))
                .into_iter()
                .chain(extracted_refs)
            {
                if seen_refs.insert(tok.clone()) {
                    merged_refs.push(tok);
                }
            }
        } else {
            let add_ref_tokens = split_refs(patch.add_refs.map(RefsArg::Str).as_ref());
            let remove_ref_folded: HashSet<String> =
                split_refs(patch.remove_refs.map(RefsArg::Str).as_ref())
                    .into_iter()
                    .map(|t| fold(Some(&t)))
                    .collect();
            for tok in old_ref_tokens
                .iter()
                .cloned()
                .chain(extracted_refs)
                .chain(add_ref_tokens)
            {
                if seen_refs.insert(tok.clone()) {
                    merged_refs.push(tok);
                }
            }
            if !remove_ref_folded.is_empty() {
                merged_refs.retain(|t| !remove_ref_folded.contains(&fold(Some(t))));
            }
        }
        // a match in the refs this amend brings in drops what it touches; a ref the item held
        // stays unless new refs replace the old ones
        let secret = refs_a_match_touches(patch.add_refs.into_iter().chain(patch.replace_refs));
        let held = patch.replace_refs.is_none();
        merged_refs.retain(|t| !secret.contains(t) || (held && old_ref_tokens.contains(t)));
        let new_refs = if merged_refs == old_ref_tokens {
            old.refs.clone()
        } else {
            merged_refs.join(" ")
        };

        let old_tag_tokens: Vec<&str> = old
            .tags
            .split(',')
            .map(strip)
            .filter(|t| !t.is_empty())
            .collect();
        let had_stale_retired = old_tag_tokens
            .iter()
            .any(|t| t.starts_with(RETIRED_TAG_PREFIX));
        let mut merged_tags: Vec<&str> = Vec::new();
        if let Some(rep) = patch.replace_tags {
            for t in rep
                .split(',')
                .map(strip)
                .filter(|t| !t.is_empty() && !t.starts_with(RETIRED_TAG_PREFIX))
            {
                if !merged_tags.contains(&t) {
                    merged_tags.push(t);
                }
            }
        } else {
            let add_tag_tokens: Vec<&str> = patch
                .add_tags
                .unwrap_or("")
                .split(',')
                .map(strip)
                .filter(|t| !t.is_empty() && !t.starts_with(RETIRED_TAG_PREFIX))
                .collect();
            let remove_tag_folded: HashSet<String> = patch
                .remove_tags
                .unwrap_or("")
                .split(',')
                .map(strip)
                .filter(|t| !t.is_empty())
                .map(|t| fold(Some(t)))
                .collect();
            for &t in &old_tag_tokens {
                if t.starts_with(RETIRED_TAG_PREFIX) {
                    continue;
                }
                if !merged_tags.contains(&t) {
                    merged_tags.push(t);
                }
            }
            for &t in &add_tag_tokens {
                if !merged_tags.contains(&t) {
                    merged_tags.push(t);
                }
            }
            if !remove_tag_folded.is_empty() {
                merged_tags.retain(|t| !remove_tag_folded.contains(&fold(Some(t))));
            }
        }
        let new_tags = if !had_stale_retired && merged_tags == old_tag_tokens {
            old.tags.clone()
        } else {
            merged_tags.join(", ")
        };

        let new_as_of = match patch.as_of {
            Some(v) => validate_as_of(Some(v), false, false)?,
            None => now_iso(),
        };

        let content_hash = content_hash_checked(&new_body)?;
        let mut new_item = redact_checked(&Item {
            id: new_item_id(None),
            repo: new_repo,
            kind: new_kind,
            title: new_title,
            body: new_body,
            tag: new_tag,
            refs: new_refs,
            tags: new_tags,
            source: new_source,
            evidence: new_evidence,
            as_of: new_as_of,
            status: "active".into(),
            superseded_by: None,
            content_hash,
            supersedes: Vec::new(),
        })?;
        new_item.kind = validate_kind(Some(&new_item.kind))?;
        new_item.tag = validate_tag(
            Some(&new_item.tag),
            Some(&new_item.evidence),
            patch.tag.is_none() && allow_na_tag(&new_item),
        )?;
        new_item.as_of = validate_as_of(Some(&new_item.as_of), false, false)?;

        let fields_unchanged = new_item.repo == old.repo
            && new_item.kind == old.kind
            && new_item.title == old.title
            && new_item.body == old.body
            && new_item.refs == old.refs
            && new_item.tags == old.tags
            && new_item.tag == old.tag
            && new_item.evidence == old.evidence;
        let as_of_unchanged = patch.as_of.is_none() || new_item.as_of == old.as_of;
        if fields_unchanged && as_of_unchanged {
            return Err(SmError::value("No changes to apply"));
        }

        // refused only when another active item holds the new row's index slot (its
        // amended:<source>, or ''); an item of another slot with this body is a distinct item
        let (repo, hash) = (&new_item.repo, &new_item.content_hash);
        if let Some(other) = active_in_slot(conn, repo, hash, &new_item.source, Some(old_id))? {
            return Err(SmError::value(format!(
                "Repo '{}' already has an active item with the same body ({other}); use sm supersede {old_id} {other}",
                new_item.repo
            )));
        }

        conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = ? \
             WHERE id = ? AND status = 'active'",
            params![new_item.id, old_id],
        )?;
        insert_item(conn, &new_item, "active", None)?;
        Ok((new_item.id, new_item.body != old.body))
    })
}

/// `get_item(conn, item_id)`.
pub fn get_item(conn: &Connection, item_id: &str) -> Result<Option<Item>, SmError> {
    bind_text(item_id)?;
    let sql = format!("SELECT {} FROM items WHERE id = ?", ITEM_COLUMNS.join(", "));
    Ok(conn.query_row(&sql, [item_id], row_to_item).optional()?)
}

/// `supersede_item(conn, old_id, new_id)`.
pub fn supersede_item(conn: &Connection, old_id: &str, new_id: &str) -> Result<(), SmError> {
    transaction(conn, None, |conn| {
        validate_and_supersede_one(conn, old_id, new_id).map(|_| ())
    })
}

/// `fill_refs(conn, dry_run=...)`: refs_empty_before, refs_changed, refs_empty_after.
pub fn fill_refs(conn: &Connection, dry_run: bool) -> Result<IndexMap<&'static str, i64>, SmError> {
    transaction(conn, None, |conn| {
        let mut stmt = conn.prepare("SELECT id, refs, title, body, evidence, source FROM items")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let empty_before = rows.iter().filter(|r| strip(&r.1).is_empty()).count() as i64;
        let mut changes: Vec<(String, String)> = Vec::new();
        for (id, refs, title, body, evidence, source) in &rows {
            let ev = if source.is_empty() || source.starts_with(AMENDED_PREFIX) {
                evidence.as_str()
            } else {
                ""
            };
            let new = item_refs(refs, title, body, ev);
            if new != *refs {
                changes.push((new, id.clone()));
            }
        }
        if !dry_run {
            for (new, id) in &changes {
                conn.execute("UPDATE items SET refs = ? WHERE id = ?", params![new, id])?;
            }
        }
        let changed_ids: HashSet<&String> = changes.iter().map(|c| &c.1).collect();
        let empty_after = rows
            .iter()
            .filter(|r| strip(&r.1).is_empty() && !changed_ids.contains(&r.0))
            .count() as i64;
        Ok(IndexMap::from([
            ("refs_empty_before", empty_before),
            ("refs_changed", changes.len() as i64),
            ("refs_empty_after", empty_after),
        ]))
    })
}

/// `check_and_rebuild(conn, rebuild_fts=...)`.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    pub integrity: String,
    pub rebuilt_fts: bool,
    pub total: i64,
    pub active: i64,
    pub superseded: i64,
}

fn count(conn: &Connection, sql: &str) -> Result<i64, SmError> {
    Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?)
}

pub fn check_and_rebuild(conn: &Connection, rebuild_fts: bool) -> Result<CheckResult, SmError> {
    if rebuild_fts {
        transaction(conn, None, |conn| {
            conn.execute("DELETE FROM items_fold", [])?;
            conn.execute("DELETE FROM items_raw", [])?;
            conn.execute(
                "INSERT INTO items_fold(rowid, refs, title, tags, body) \
                 SELECT rowid, sm_fold(refs), sm_fold(title), sm_fold(tags), sm_fold(body) FROM items",
                [],
            )?;
            conn.execute(
                "INSERT INTO items_raw(rowid, title, body) SELECT rowid, title, body FROM items",
                [],
            )?;
            Ok(())
        })?;
    }
    // `str(row[0])`: integrity_check answers TEXT.
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
        .optional()?
        .unwrap_or_else(|| "unknown".into());
    conn.execute(
        "INSERT INTO items_fold(items_fold) VALUES('integrity-check')",
        [],
    )?;
    conn.execute(
        "INSERT INTO items_raw(items_raw) VALUES('integrity-check')",
        [],
    )?;
    Ok(CheckResult {
        integrity,
        rebuilt_fts: rebuild_fts,
        total: count(conn, "SELECT count(*) FROM items")?,
        active: count(conn, "SELECT count(*) FROM items WHERE status = 'active'")?,
        superseded: count(
            conn,
            "SELECT count(*) FROM items WHERE status = 'superseded'",
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_to_item_reads_columns_by_name_and_runs_post_init() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        let schema = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/0001_init.sql"
        ));
        let table = &schema[schema.find("CREATE TABLE items").unwrap()..];
        c.execute_batch(&table[..table.find(");").unwrap() + 2])
            .unwrap();
        c.execute(
            "INSERT INTO items (id, repo, kind, title, body, tag, as_of, superseded_by, content_hash) \
             VALUES ('m_1', 'r', 'fact', 'T', ' b ', 'decided', '2026-01-01T00:00:00+00:00', 'm_2', '')",
            [],
        )
        .unwrap();
        let sql = format!("SELECT {} FROM items WHERE id = ?", ITEM_COLUMNS.join(", "));
        let it = c.query_row(&sql, ["m_1"], row_to_item).unwrap();
        assert_eq!(it.status, "active");
        assert_eq!(it.superseded_by.as_deref(), Some("m_2"));
        // sha256 of "b": the stripped body.
        assert_eq!(
            it.content_hash,
            "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d"
        );
        let err = c
            .query_row("SELECT id FROM items", [], row_to_item)
            .unwrap_err();
        assert!(matches!(err, rusqlite::Error::InvalidColumnName(_)));
    }

    fn store() -> (tempfile::TempDir, Connection) {
        let d = tempfile::tempdir().unwrap();
        let conn =
            crate::store::connect::connect(d.path().join("memory.db").to_str().unwrap(), None)
                .unwrap();
        (d, conn)
    }

    fn it(id: &str, repo: &str, body: &str, tag: &str) -> Item {
        Item {
            as_of: "2026-10-25T09:00:00+00:00".into(),
            ..Item::bare(id, repo, "decision", "T", body, tag)
        }
        .post_init()
    }

    fn err_text<T: std::fmt::Debug>(r: Result<T, SmError>) -> String {
        r.unwrap_err().python_text()
    }

    #[test]
    fn amend_with_new_refs_drops_what_a_match_touches_in_the_whole_new_list() {
        // the new refs replace the old ones, so a ref the item held is brought in again
        let (_d, conn) = store();
        let old = Item {
            refs: "TKT-9 docs/x.md".into(),
            ..it("m_1", "anvil", "rotation note", "derived")
        };
        put_item(&conn, &old, None).unwrap();
        let patch = AmendPatch {
            replace_refs: Some("Bearer docs/x.md TKT-9"),
            ..Default::default()
        };
        let (new_id, _) = amend_item(&conn, "m_1", &patch).unwrap();
        assert_eq!(get_item(&conn, &new_id).unwrap().unwrap().refs, "TKT-9");
    }

    #[test]
    fn put_dedupes_by_repo_and_hash_and_supersede_checks_chains() {
        let (_d, conn) = store();
        assert_eq!(
            put_item(
                &conn,
                &it(
                    "m_1",
                    "anvil",
                    "SQLite is the only source of truth.",
                    "decided"
                ),
                None
            )
            .unwrap(),
            ("m_1".into(), true)
        );
        assert_eq!(
            put_item(
                &conn,
                &it(
                    "m_2",
                    "anvil",
                    "  SQLite is the only source of truth. \n",
                    "decided"
                ),
                None
            )
            .unwrap(),
            ("m_1".into(), false)
        );
        assert_eq!(
            put_item(
                &conn,
                &it(
                    "m_3",
                    "other",
                    "SQLite is the only source of truth.",
                    "decided"
                ),
                None
            )
            .unwrap(),
            ("m_3".into(), true)
        );
        put_item(
            &conn,
            &it("m_4", "anvil", "SQLite with WAL is the truth.", "decided"),
            None,
        )
        .unwrap();
        supersede_item(&conn, "m_1", "m_4").unwrap();
        let old = get_item(&conn, "m_1").unwrap().unwrap();
        assert_eq!(
            (old.status.as_str(), old.superseded_by.as_deref()),
            ("superseded", Some("m_4"))
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_1", " m_1 ")),
            "ValueError: Cannot supersede an item with itself"
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_none", "m_4")),
            "KeyError: item m_none not found (ids are case-sensitive and must be given in full)"
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_4", "m_none")),
            "KeyError: item m_none not found (ids are case-sensitive and must be given in full)"
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_4", "m_1")),
            "ValueError: Supersede cycle detected: m_4 -> m_1"
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_3", "m_1")),
            "ValueError: Cannot supersede m_3 with already-superseded item m_1"
        );
        // the same link again changes nothing
        assert_eq!(validate_and_supersede_one(&conn, "m_1", "m_4").unwrap(), 0);
        // an unknown supersedes id rolls the insert back
        let bad = it("m_5", "anvil", "Should not be committed.", "decided");
        assert_eq!(
            err_text(put_item(&conn, &bad, Some(&["m_TYPO".into()]))),
            "KeyError: item m_TYPO not found (ids are case-sensitive and must be given in full)"
        );
        assert_eq!(get_item(&conn, "m_5").unwrap(), None);
        assert!(conn.is_autocommit());
    }

    #[test]
    fn put_with_supersedes_on_a_duplicate_body_points_the_others_at_the_existing_item() {
        let (_d, conn) = store();
        put_item(&conn, &it("m_1", "r", "Body one", "decided"), None).unwrap();
        put_item(&conn, &it("m_2", "r", "Body two", "decided"), None).unwrap();
        let sup = vec![" ".to_string(), "m_1".into(), " m_2 ".into()];
        assert_eq!(
            put_item(&conn, &it("m_3", "r", "Body one", "decided"), Some(&sup)).unwrap(),
            ("m_1".into(), false)
        );
        assert_eq!(
            get_item(&conn, "m_2")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_1")
        );
        assert_eq!(get_item(&conn, "m_1").unwrap().unwrap().status, "active");
        // the item's own supersedes are used when the argument is None, and ignored when it is given
        let own = Item {
            supersedes: vec!["m_1".into()],
            ..it("m_4", "r", "Body four", "decided")
        };
        put_item(&conn, &own, Some(&[])).unwrap();
        assert_eq!(get_item(&conn, "m_1").unwrap().unwrap().status, "active");
        let own = Item {
            supersedes: vec!["m_1".into()],
            ..it("m_5", "r", "Body five", "decided")
        };
        put_item(&conn, &own, None).unwrap();
        assert_eq!(
            get_item(&conn, "m_1")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_5")
        );
    }

    #[test]
    fn put_validates_tag_kind_and_as_of_and_allows_na_on_legacy_and_turn_only() {
        let (_d, conn) = store();
        assert_eq!(
            err_text(put_item(&conn, &it("m_1", "r", "b1", "bogus"), None)),
            "ValueError: Invalid tag 'bogus'; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned"
        );
        let no_ev = Item {
            evidence: "   ".into(),
            ..it("m_2", "r", "b2", "verified")
        };
        assert_eq!(
            err_text(put_item(&conn, &no_ev, None)),
            "ValueError: Tag 'verified' needs non-empty evidence: a command, a sha, a file:line or a quote"
        );
        assert!(
            err_text(put_item(&conn, &it("m_3", "r", "b3", "n/a"), None))
                .starts_with("ValueError: Invalid tag 'n/a'")
        );
        let kind = Item {
            kind: "nope".into(),
            ..it("m_4", "r", "b4", "decided")
        };
        assert!(
            err_text(put_item(&conn, &kind, None)).starts_with("ValueError: Invalid kind 'nope'")
        );
        let date = Item {
            as_of: "2026-02-30".into(),
            ..it("m_5", "r", "b5", "decided")
        };
        assert_eq!(
            err_text(put_item(&conn, &date, None)),
            "ValueError: Invalid ISO 8601 timestamp '2026-02-30', for example 2026-11-21T09:30:00Z"
        );
        assert_eq!(
            put_item(&conn, &it("legacy:memories:1", "r", "b6", "n/a"), None).unwrap(),
            ("legacy:memories:1".into(), true)
        );
        let turn = Item {
            source: "turn:abcdef12".into(),
            ..it("m_7", "r", "b7", "n/a")
        };
        assert_eq!(put_item(&conn, &turn, None).unwrap(), ("m_7".into(), true));
    }

    #[test]
    fn a_stand_in_raises_where_sqlite_binds_it() {
        let (_d, conn) = store();
        let t = Item {
            title: "ab\u{10fdff}".into(),
            ..it("m_1", "r", "b", "decided")
        };
        assert_eq!(
            err_text(put_item(&conn, &t, None)),
            "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 2: surrogates not allowed"
        );
        assert!(conn.is_autocommit());
        assert_eq!(
            err_text(get_item(&conn, "\u{10fd80}\u{10fd81}x")),
            "UnicodeEncodeError: 'utf-8' codec can't encode characters in position 0-1: surrogates not allowed"
        );
        assert_eq!(
            content_hash_checked("  a\u{10fdfe}").unwrap_err().msg,
            "'utf-8' codec can't encode character '\\udcfe' in position 1: surrogates not allowed"
        );
    }

    #[test]
    fn batch_isolates_failed_rows() {
        let (_d, conn) = store();
        let items = vec![
            it("m_good_1", "anvil", "First valid row", "decided"),
            it("m_bad_tag", "anvil", "Non-legacy row with n/a", "n/a"),
            Item {
                kind: "not_a_kind".into(),
                ..it("m_bad_kind", "anvil", "Bad kind row", "decided")
            },
            Item {
                as_of: "not-an-iso-timestamp".into(),
                ..it("m_bad_as_of", "anvil", "Bad as_of row", "decided")
            },
            Item {
                evidence: "pytest -q".into(),
                ..it("m_good_2", "anvil", "Second valid row", "verified")
            },
            it("m_dup", "anvil", "First valid row", "decided"),
        ];
        let (ins, dup, sup, missing, unresolved, failed) =
            put_items_batch(&conn, &items, false).unwrap();
        assert_eq!((ins, dup, sup, missing), (2, 1, 0, 0));
        assert!(unresolved.is_empty());
        assert_eq!(
            failed,
            vec![
                "m_bad_tag: Invalid tag 'n/a'; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned",
                "m_bad_kind: Invalid kind 'not_a_kind'; expected one of fact, decision, learning, episode, task, handoff",
                "m_bad_as_of: Invalid ISO 8601 timestamp 'not-an-iso-timestamp', for example 2026-11-21T09:30:00Z",
            ]
        );
        assert_eq!(get_item(&conn, "m_bad_tag").unwrap(), None);
        assert!(get_item(&conn, "m_good_2").unwrap().is_some());
    }

    // Probed on Python 555b8f2: put_items_batch catches ValueError, KeyError, TypeError and
    // sqlite3.Error per row; as_of `9999-12-31T23:59:59-23:59` raises OverflowError, which leaves
    // the batch, and the transaction rolls every row back.
    #[test]
    fn batch_rolls_back_every_row_when_one_raises_outside_the_per_row_catch() {
        let (_d, conn) = store();
        put_item(
            &conn,
            &it("m_0", "r", "Stored before the batch", "decided"),
            None,
        )
        .unwrap();
        let overflow = Item {
            as_of: "9999-12-31T23:59:59-23:59".into(),
            ..it("m_2", "r", "Overflow row", "decided")
        };
        let items = vec![it("m_1", "r", "First row", "decided"), overflow];
        assert_eq!(
            err_text(put_items_batch(&conn, &items, false)),
            "OverflowError: date value out of range"
        );
        assert_eq!(get_item(&conn, "m_1").unwrap(), None);
        assert_eq!(get_item(&conn, "m_2").unwrap(), None);
        assert!(get_item(&conn, "m_0").unwrap().is_some());
        assert!(conn.is_autocommit());
    }

    // Probed on Python 555b8f2: `retire_item` leaves status superseded with no pointer, and
    // `supersede_item(m_1, m_2)` then raises `Cannot supersede m_1 with already-superseded item m_2`.
    #[test]
    fn supersede_refuses_a_replacement_that_is_retired_with_no_pointer() {
        let (_d, conn) = store();
        put_item(&conn, &it("m_1", "r", "One", "decided"), None).unwrap();
        put_item(&conn, &it("m_2", "r", "Two", "decided"), None).unwrap();
        assert_eq!(retire_item(&conn, "m_2").unwrap(), 1);
        let retired = get_item(&conn, "m_2").unwrap().unwrap();
        assert_eq!(
            (retired.status.as_str(), retired.superseded_by.as_deref()),
            ("superseded", None)
        );
        assert_eq!(
            err_text(supersede_item(&conn, "m_1", "m_2")),
            "ValueError: Cannot supersede m_1 with already-superseded item m_2"
        );
        let one = get_item(&conn, "m_1").unwrap().unwrap();
        assert_eq!(
            (one.status.as_str(), one.superseded_by.as_deref()),
            ("active", None)
        );
    }

    #[test]
    fn batch_restores_a_retired_task_item_and_supersedes_older_rows_of_its_source() {
        let (_d, conn) = store();
        let src = "task:20260101-010101-a1b2c3d4-one";
        let t = |id: &str, body: &str| Item {
            source: src.into(),
            kind: "task".into(),
            ..it(id, "r", body, "decided")
        };
        put_items_batch(&conn, &[t("m_1", "v1")], false).unwrap();
        assert_eq!(retire_item(&conn, "m_1").unwrap(), 1);
        assert_eq!(retire_item(&conn, "m_1").unwrap(), 0);
        let (ins, dup, sup, ..) = put_items_batch(&conn, &[t("m_2", "v2")], false).unwrap();
        assert_eq!((ins, dup, sup), (1, 0, 0));
        // v1 comes back: restored, and it supersedes the active v2
        let (ins, dup, sup, ..) = put_items_batch(&conn, &[t("m_3", "v1")], false).unwrap();
        assert_eq!((ins, dup, sup), (1, 0, 1));
        let m1 = get_item(&conn, "m_1").unwrap().unwrap();
        assert_eq!((m1.status.as_str(), m1.superseded_by), ("active", None));
        assert_eq!(
            get_item(&conn, "m_2")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_1")
        );
        assert_eq!(get_item(&conn, "m_3").unwrap(), None);
    }

    #[test]
    fn batch_unassigned_never_duplicates_or_overrides_known_repo() {
        let (_d, conn) = store();
        let ho = |id: &str, repo: &str, body: &str| Item {
            source: "handoff:2026-01-01-alpha.md".into(),
            kind: "handoff".into(),
            ..it(id, repo, body, "derived")
        };
        let (ins, dup, sup, ..) =
            put_items_batch(&conn, &[ho("m_alpha", "alpha", "v1")], false).unwrap();
        assert_eq!((ins, dup, sup), (1, 0, 0));

        // Even with changed body/hash, an unassigned import of the same file-backed source dedupes
        // onto the concrete repo row and leaves it untouched.
        let (ins, dup, sup, ..) =
            put_items_batch(&conn, &[ho("m_bad", "unassigned", "v2")], false).unwrap();
        assert_eq!((ins, dup, sup), (0, 1, 0));
        assert_eq!(get_item(&conn, "m_bad").unwrap(), None);
        assert_eq!(
            get_item(&conn, "m_alpha").unwrap().unwrap().status,
            "active"
        );

        // Same when the concrete repo row was already superseded by a newer item in that repo.
        let sup_item = Item {
            source: "handoff:2026-01-02-alpha.md".into(),
            kind: "handoff".into(),
            ..it("m_alpha2", "alpha", "newer", "derived")
        };
        put_item(&conn, &sup_item, Some(&["m_alpha".to_string()])).unwrap();
        let (ins, dup, sup, ..) =
            put_items_batch(&conn, &[ho("m_bad2", "unassigned", "v1")], false).unwrap();
        assert_eq!((ins, dup, sup), (0, 1, 0));
        assert_eq!(get_item(&conn, "m_bad2").unwrap(), None);

        // Legacy cross-repo `hidden` row (`b.source = i.source AND b.repo != i.repo`): matches
        // `doctor::import_lag_by_repo` so an updated file while jailed updates the active
        // unassigned row instead of letting doctor loop on lag while batch_row dedupes onto old alpha.
        let ho_cross = Item {
            source: "handoff:2026-01-03-cross.md".into(),
            kind: "handoff".into(),
            ..it("m_cross_old", "alpha", "v1", "derived")
        };
        let ho_repl = Item {
            source: "handoff:2026-01-03-cross.md".into(),
            kind: "handoff".into(),
            ..it("m_cross_unassigned", "unassigned", "v2", "derived")
        };
        insert_item(&conn, &ho_cross, "superseded", Some("m_cross_unassigned")).unwrap();
        insert_item(&conn, &ho_repl, "active", None).unwrap();
        let (ins, dup, sup, ..) = put_items_batch(
            &conn,
            &[Item {
                source: "handoff:2026-01-03-cross.md".into(),
                kind: "handoff".into(),
                ..it("m_cross_new", "unassigned", "v3", "derived")
            }],
            false,
        )
        .unwrap();
        assert_eq!((ins, dup, sup), (1, 0, 1));
        assert_eq!(
            get_item(&conn, "m_cross_new").unwrap().unwrap().repo,
            "unassigned"
        );

        // Standalone unassigned handoffs with no concrete repo row still import normally.
        let solo = Item {
            source: "handoff:solo.md".into(),
            kind: "handoff".into(),
            ..it("m_solo", "unassigned", "standalone", "derived")
        };
        let (ins, dup, sup, ..) = put_items_batch(&conn, &[solo], false).unwrap();
        assert_eq!((ins, dup, sup), (1, 0, 0));
        assert_eq!(
            get_item(&conn, "m_solo").unwrap().unwrap().repo,
            "unassigned"
        );
    }

    #[test]
    fn batch_resolves_tasklog_chains_and_reports_cycles_and_missing_targets() {
        let (_d, conn) = store();
        let t = |id: &str, task: &str, sup: Option<&str>| Item {
            source: format!("task:{task}"),
            kind: "task".into(),
            superseded_by: sup.map(String::from),
            ..it(id, "r", &format!("body {id}"), "decided")
        };
        let (_, _, sup, _, unresolved, _) = put_items_batch(
            &conn,
            &[
                t("m_b", "B", Some("A")),
                t("m_a", "A", Some("C")),
                t("m_x", "X", Some("Y")),
                t("m_s", "S", Some("S")),
            ],
            false,
        )
        .unwrap();
        assert_eq!(sup, 1);
        assert_eq!(
            unresolved,
            vec!["task:A -> C", "task:X -> Y", "task:S -> S"]
        );
        assert_eq!(
            get_item(&conn, "m_b")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_a")
        );
        // C arrives later and names B, which is present: C points at B (values from the Python reference)
        let (_, _, sup, _, unresolved, _) =
            put_items_batch(&conn, &[t("m_c", "C", Some("B"))], false).unwrap();
        assert_eq!((sup, unresolved), (1, Vec::<String>::new()));
        assert_eq!(
            get_item(&conn, "m_c")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_b")
        );
        assert_eq!(get_item(&conn, "m_a").unwrap().unwrap().superseded_by, None);
    }

    #[test]
    fn batch_apply_supersedes_counts_missing_targets_and_skips_its_own_id() {
        let (_d, conn) = store();
        put_item(&conn, &it("m_1", "r", "old", "decided"), None).unwrap();
        let new = Item {
            supersedes: vec!["m_1".into(), "m_gone".into(), " ".into(), "m_2".into()],
            ..it("m_2", "r", "new", "decided")
        };
        let (ins, _, sup, missing, ..) =
            put_items_batch(&conn, std::slice::from_ref(&new), true).unwrap();
        assert_eq!((ins, sup, missing), (1, 1, 1));
        let (ins, _, sup, missing, ..) = put_items_batch(
            &conn,
            &[Item {
                id: "m_3".into(),
                ..new
            }],
            false,
        )
        .unwrap();
        assert_eq!((ins, sup, missing), (0, 0, 0));
    }

    #[test]
    fn batch_rolls_back_every_write_of_a_row_that_fails_after_its_insert() {
        // values from the Python 3.15.1 reference: the stand-in supersedes id raises at its bind,
        // after the insert, the same-source supersede and the m_old link
        let (_d, conn) = store();
        put_item(&conn, &it("m_old", "r", "old", "decided"), None).unwrap();
        let src = "task:20260101-010101-a1b2c3d4-one";
        let task = |id: &str, body: &str| Item {
            kind: "task".into(),
            source: src.into(),
            ..it(id, "r", body, "decided")
        };
        put_item(&conn, &task("m_prev", "prev"), None).unwrap();
        let bad = Item {
            supersedes: vec!["m_old".into(), "m_\u{10fdff}".into()],
            ..task("m_n", "new")
        };
        let (ins, dup, sup, missing, unresolved, failed) =
            put_items_batch(&conn, &[bad, it("m_ok", "r", "ok", "decided")], true).unwrap();
        assert_eq!((ins, dup, sup, missing), (1, 0, 0, 0));
        assert!(unresolved.is_empty());
        assert_eq!(
            failed,
            [
                "m_n: 'utf-8' codec can't encode character '\\udcff' in position 2: surrogates not allowed"
            ]
        );
        assert_eq!(get_item(&conn, "m_n").unwrap(), None);
        for id in ["m_ok", "m_old", "m_prev"] {
            let x = get_item(&conn, id).unwrap().unwrap();
            assert_eq!(
                (x.status.as_str(), x.superseded_by),
                ("active", None),
                "{id}"
            );
        }
    }

    #[test]
    fn the_supersede_walk_stops_at_a_node_it_has_seen() {
        // reachable only with a row whose id is empty and a blank pointer, which strips to that id;
        // the Python 3.15.1 reference raises the cycle error for both spellings of the old id
        let (_d, conn) = store();
        conn.execute_batch(
            "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash) VALUES ('', 'r', 'fact', 'T', 'empty id', 'decided', 'a', 'h0');
             INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash, superseded_by) VALUES ('m_y', 'r', 'fact', 'T', 'y', 'decided', 'a', 'h1', ' ');",
        )
        .unwrap();
        for old in ["", " "] {
            assert_eq!(
                err_text(validate_and_supersede_one(&conn, old, "m_y")),
                "ValueError: Supersede cycle detected:  -> m_y"
            );
        }
    }

    #[test]
    fn sync_source_item_adds_updates_and_keeps_unchanged() {
        let (_d, conn) = store();
        let f = |id: &str, body: &str, title: &str| Item {
            source: "automemory:r/a.md".into(),
            title: title.into(),
            ..it(id, "r", body, "decided")
        };
        assert!(!file_item_is_current(&conn, &f("m_1", "v1", "T")).unwrap());
        assert_eq!(
            sync_source_item(&conn, &f("m_1", "v1", "T")).unwrap(),
            "added"
        );
        assert_eq!(
            sync_source_item(&conn, &f("m_2", "v1", "T")).unwrap(),
            "unchanged"
        );
        assert_eq!(
            sync_source_item(&conn, &f("m_3", "v1", "T2")).unwrap(),
            "updated"
        );
        assert_eq!(
            get_item(&conn, "m_1")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_3")
        );
        // another source's active item holds the body: the old one points at it
        let other = Item {
            source: "automemory:r/b.md".into(),
            ..it("m_9", "r", "shared", "decided")
        };
        put_item(&conn, &other, None).unwrap();
        assert_eq!(
            sync_source_item(&conn, &f("m_4", "shared", "T3")).unwrap(),
            "unchanged"
        );
        assert_eq!(
            get_item(&conn, "m_3")
                .unwrap()
                .unwrap()
                .superseded_by
                .as_deref(),
            Some("m_9")
        );
        // a retired newest item is not current
        retire_item(&conn, "m_9").unwrap();
        assert!(
            !file_item_is_current(
                &conn,
                &Item {
                    source: "automemory:r/b.md".into(),
                    ..it("m_8", "r", "shared", "decided")
                }
            )
            .unwrap()
        );
    }

    #[test]
    fn sync_source_items_counts_and_still_supersedes_through_another_source() {
        let (_d, conn) = store();
        let f = |id: &str, src: &str, body: &str| Item {
            source: src.into(),
            ..it(id, "r", body, "decided")
        };
        let a1 = f("m_1", "am:r/a.md", "v1");
        let b1 = f("m_2", "am:r/b.md", "v2");
        assert_eq!(
            sync_source_items(&conn, "am:", [&a1, &b1]).unwrap(),
            (2, 0, 0)
        );
        let a2 = f("m_3", "am:r/a.md", "v1b");
        let b2 = f("m_4", "am:r/b.md", "v2");
        assert_eq!(
            sync_source_items(&conn, "am:", [&a2, &b2]).unwrap(),
            (0, 1, 1)
        );
        // a source the scan has not seen goes through the full path; the second, identical item
        // then finds the row the first one wrote
        let c = f("m_5", "am:r/c.md", "v3");
        assert_eq!(
            sync_source_items(&conn, "am:", [&c, &c]).unwrap(),
            (1, 0, 1)
        );
        // a's newest row holds the body but was superseded by c's row, and a's older row m_1 is
        // still active: the item is unchanged, and m_1 now points at c's row too
        conn.execute(
            "UPDATE items SET status = 'active', superseded_by = NULL WHERE id = 'm_1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE items SET status = 'superseded', superseded_by = 'm_5' WHERE id = 'm_3'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE items SET content_hash = (SELECT content_hash FROM items WHERE id = 'm_3') WHERE id = 'm_5'",
            [],
        )
        .unwrap();
        assert_eq!(sync_source_items(&conn, "am:", [&a2]).unwrap(), (0, 0, 1));
        let m1 = get_item(&conn, "m_1").unwrap().unwrap();
        assert_eq!(
            (m1.status.as_str(), m1.superseded_by.as_deref()),
            ("superseded", Some("m_5"))
        );
        // back to the first body: only the newest row counts, so the old v1 row m_1 does not
        // make it unchanged
        let a4 = f("m_6", "am:r/a.md", "v1");
        assert_eq!(sync_source_items(&conn, "am:", [&a4]).unwrap(), (1, 0, 0));
        assert_eq!(get_item(&conn, "m_6").unwrap().unwrap().status, "active");
        // the same source twice in one call: the second item is checked against the row the
        // first one wrote, not the scan, as one sync_source_item per item does
        let a5 = f("m_7", "am:r/a.md", "v7");
        let a6 = f("m_8", "am:r/a.md", "v1");
        assert_eq!(
            sync_source_items(&conn, "am:", [&a5, &a6]).unwrap(),
            (0, 2, 0)
        );
        assert_eq!(get_item(&conn, "m_8").unwrap().unwrap().status, "active");
    }

    #[test]
    fn sync_source_items_stops_at_a_failing_item_and_keeps_the_ones_before() {
        let (_d, conn) = store();
        let f = |id: &str, src: &str, body: &str| Item {
            source: src.into(),
            ..it(id, "r", body, "decided")
        };
        let good = f("m_1", "am:r/a.md", "v1");
        let bad = Item {
            kind: "bogus".into(),
            ..f("m_2", "am:r/b.md", "v2")
        };
        let after = f("m_3", "am:r/c.md", "v3");
        assert_eq!(
            err_text(sync_source_items(&conn, "am:", [&good, &bad, &after])),
            "ValueError: Invalid kind 'bogus'; expected one of fact, decision, learning, episode, task, handoff"
        );
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM items ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids, vec!["m_1"]);
        assert!(conn.is_autocommit());
    }

    #[test]
    fn sync_source_items_reports_a_non_text_cell_at_its_own_item() {
        let (_d, conn) = store();
        let f = |id: &str, src: &str, body: &str| Item {
            source: src.into(),
            ..it(id, "r", body, "decided")
        };
        let b = f("m_1", "am:r/b.md", "v1");
        let t = f("m_2", "task:t1", "v2");
        put_item(&conn, &b, None).unwrap();
        put_item(&conn, &t, None).unwrap();
        // a non-text cell outside the prefix is never read
        conn.execute("UPDATE items SET as_of = x'41' WHERE id = 'm_2'", [])
            .unwrap();
        assert_eq!(sync_source_items(&conn, "am:", [&b]).unwrap(), (0, 0, 1));
        // one in the newest row of b fails at b's item, after a's item was committed
        conn.execute("UPDATE items SET as_of = x'41' WHERE id = 'm_1'", [])
            .unwrap();
        let a = f("m_3", "am:r/a.md", "v3");
        assert_eq!(
            err_text(sync_source_items(&conn, "am:", [&a, &b])),
            // index 9 of source_state's own query (the scan holds as_of at 10)
            "DatabaseError: Invalid column type Blob at index: 9, name: as_of"
        );
        assert_eq!(get_item(&conn, "m_3").unwrap().unwrap().status, "active");
    }

    #[test]
    fn file_items_are_current_answers_as_file_item_is_current_for_each_item() {
        let (_d, conn) = store();
        let f = |id: &str, src: &str, body: &str| Item {
            source: src.into(),
            ..it(id, "r", body, "decided")
        };
        let held = f("m_1", "am:r/a.md", "v1");
        let shared = f("m_2", "am:r/b.md", "shared");
        let gone = f("m_3", "am:r/e.md", "v3");
        let secret = f("m_4", "am:r/s.md", &format!("key AKIA{}", "Q".repeat(16)));
        for i in [&held, &shared, &gone, &secret] {
            sync_source_item(&conn, i).unwrap();
        }
        retire_item(&conn, "m_3").unwrap();
        let items = [
            &held,
            // a's newest row holds v1
            &f("m_5", "am:r/a.md", "v1 edited"),
            // b's active row holds this body, in a source the scan read and in one it did not
            &f("m_6", "am:r/a.md", "shared"),
            &f("m_7", "am:r/c.md", "shared"),
            &f("m_8", "am:r/d.md", "v8"),
            // the newest row of e was retired
            &gone,
            // the store holds the redacted item
            &secret,
        ];
        let one: Vec<bool> = items
            .iter()
            .map(|i| file_item_is_current(&conn, i).unwrap())
            .collect();
        let expected = vec![true, false, true, true, false, false, true];
        assert_eq!(one, expected);
        assert_eq!(
            file_items_are_current(&conn, "am:", items).unwrap(),
            expected
        );
        // a non-text cell in a's newest row: the error of the per-item lookup, at a's item
        conn.execute("UPDATE items SET as_of = x'41' WHERE id = 'm_1'", [])
            .unwrap();
        let blob = "DatabaseError: Invalid column type Blob at index: 9, name: as_of";
        assert_eq!(err_text(file_item_is_current(&conn, &held)), blob);
        assert_eq!(
            err_text(file_items_are_current(&conn, "am:", [&shared, &held])),
            blob
        );
        // an active row of g whose id is not text: the error of the per-item lookup
        let g = f("m_10", "am:r/g.md", "v10");
        sync_source_item(&conn, &g).unwrap();
        conn.execute("UPDATE items SET id = x'41' WHERE id = 'm_10'", [])
            .unwrap();
        let blob_id = "DatabaseError: Invalid column type Blob at index: 0, name: id";
        assert_eq!(err_text(file_item_is_current(&conn, &g)), blob_id);
        assert_eq!(
            err_text(file_items_are_current(&conn, "am:", [&shared, &g])),
            blob_id
        );
        // a stored source with a stand-in char that sm itself never writes: the bind error
        let odd = "am:r/\u{10fdff}.md";
        conn.execute(
            "UPDATE items SET source = ?, as_of = '2026-10-25T09:00:00+00:00' WHERE id = 'm_1'",
            [odd],
        )
        .unwrap();
        let odd_item = f("m_9", odd, "v1");
        let encode = "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 5: surrogates not allowed";
        assert_eq!(err_text(file_item_is_current(&conn, &odd_item)), encode);
        assert_eq!(
            err_text(file_items_are_current(&conn, "am:", [&odd_item])),
            encode
        );
    }

    #[test]
    fn fill_refs_adds_text_refs_and_keeps_imported_evidence_out() {
        let (_d, conn) = store();
        put_item(
            &conn,
            &Item {
                evidence: "ran QRS3".into(),
                ..it("m_1", "r", "names XYZ7", "verified")
            },
            None,
        )
        .unwrap();
        conn.execute("UPDATE items SET refs = ''", []).unwrap();
        conn.execute(
            "INSERT INTO items (id, repo, kind, title, body, tag, evidence, source, as_of, content_hash) \
             VALUES ('m_2', 'r', 'fact', 'T', 'plain', 'decided', 'ran QRS4', 'task:x', 'a', 'h2')",
            [],
        )
        .unwrap();
        let want = |a: i64, b: i64, c: i64| {
            IndexMap::from([
                ("refs_empty_before", a),
                ("refs_changed", b),
                ("refs_empty_after", c),
            ])
        };
        assert_eq!(fill_refs(&conn, true).unwrap(), want(2, 1, 1));
        assert_eq!(get_item(&conn, "m_1").unwrap().unwrap().refs, "");
        assert_eq!(fill_refs(&conn, false).unwrap(), want(2, 1, 1));
        assert_eq!(get_item(&conn, "m_1").unwrap().unwrap().refs, "XYZ7 QRS3");
        assert_eq!(fill_refs(&conn, false).unwrap(), want(1, 0, 1));
    }

    #[test]
    fn check_and_rebuild_counts_and_rebuilds_the_fts_tables() {
        let (_d, conn) = store();
        put_item(&conn, &it("m_1", "r", "Đorđe café", "decided"), None).unwrap();
        put_item(&conn, &it("m_2", "r", "two", "decided"), None).unwrap();
        supersede_item(&conn, "m_2", "m_1").unwrap();
        conn.execute("DELETE FROM items_fold", []).unwrap();
        let r = check_and_rebuild(&conn, true).unwrap();
        assert_eq!(
            r,
            CheckResult {
                integrity: "ok".into(),
                rebuilt_fts: true,
                total: 2,
                active: 1,
                superseded: 1
            }
        );
        let folded: String = conn
            .query_row("SELECT body FROM items_fold WHERE rowid = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(folded, "dorde cafe");
    }
}
