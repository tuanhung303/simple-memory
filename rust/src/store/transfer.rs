use rusqlite::{Connection, OptionalExtension};

use crate::core::item::{Item, validate_as_of};
use crate::core::tags::{validate_kind, validate_tag};
use crate::error::{Base, SmError};
use crate::store::connect::transaction;
use crate::store::items::{
    ITEM_COLUMNS, active_in_slot, allow_na_tag, bind_all, bind_text, insert_item, redact_checked,
    row_to_item,
};

/// `export_items(conn, repo=..., include_history=...)`: items sorted by id; one repo, or every repo
/// when repo is None; superseded ones only with history.
pub fn export_items(
    conn: &Connection,
    repo: Option<&str>,
    include_history: bool,
) -> Result<Vec<Item>, SmError> {
    let mut wheres: Vec<&str> = Vec::new();
    let mut params: Vec<&str> = Vec::new();
    if let Some(r) = repo {
        wheres.push("repo = ?");
        params.push(r);
    }
    if !include_history {
        wheres.push("status = 'active'");
    }
    let mut sql = format!("SELECT {} FROM items", ITEM_COLUMNS.join(", "));
    if !wheres.is_empty() {
        sql += &format!(" WHERE {}", wheres.join(" AND "));
    }
    bind_all(&params)?;
    let mut stmt = conn.prepare(&format!("{sql} ORDER BY id"))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(&params), row_to_item)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One `status_differs` row: the stored status, the file's, and the replacement the file names
/// when it is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusDiffers {
    pub id: String,
    pub store: String,
    pub file: String,
    pub replacement: Option<String>,
}

/// `import_items(conn, items)`, keys in Python's order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ImportResult {
    pub added: i64,
    pub unchanged: i64,
    pub conflicts: Vec<String>,
    pub status_differs: Vec<StatusDiffers>,
    pub redacted: Vec<String>,
    pub links_restored: i64,
    pub links_missing: Vec<String>,
}

fn exists(conn: &Connection, id: &str) -> Result<bool, SmError> {
    bind_text(id)?;
    Ok(conn
        .query_row("SELECT 1 FROM items WHERE id = ?", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// `import_items(conn, items)`: store items that keep their id, status and links, in one
/// transaction; a check that fails stops the whole import.
pub fn import_items(conn: &Connection, items: &[Item]) -> Result<ImportResult, SmError> {
    let mut res = ImportResult::default();
    let mut same_hash: Vec<Item> = Vec::new();
    let mut links: Vec<(String, String)> = Vec::new();
    transaction(conn, None, |conn| {
        for raw in items {
            let item = redact_checked(raw)?;
            if item != *raw {
                res.redacted.push(item.id.clone());
            }
            (|| -> Result<(), SmError> {
                validate_kind(Some(&item.kind))?;
                validate_tag(Some(&item.tag), Some(&item.evidence), allow_na_tag(&item))?;
                validate_as_of(Some(&item.as_of), false, false)?;
                Ok(())
            })()
            .map_err(|e| {
                if e.is(Base::Value) {
                    SmError::value(format!("{}: {}", item.id, e.msg))
                } else {
                    e
                }
            })?;
            bind_text(&item.id)?;
            let stored: Option<String> = conn
                .query_row(
                    "SELECT content_hash FROM items WHERE id = ?",
                    [&item.id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(h) = stored {
                if h == item.content_hash {
                    same_hash.push(item);
                } else {
                    res.conflicts.push(item.id.clone());
                }
                continue;
            }
            if item.status == "active"
                && active_in_slot(conn, &item.repo, &item.content_hash, &item.source, None)?
                    .is_some()
            {
                res.conflicts.push(item.id.clone());
                continue;
            }
            insert_item(conn, &item, &item.status, None)?;
            res.added += 1;
            if let Some(sb) = item.superseded_by.as_deref().filter(|s| !s.is_empty()) {
                links.push((item.id.clone(), sb.to_string()));
            }
        }
        for (old_id, new_id) in &links {
            if !exists(conn, new_id)? {
                res.links_missing.push(format!("{old_id} -> {new_id}"));
                continue;
            }
            conn.execute(
                "UPDATE items SET superseded_by = ? WHERE id = ?",
                [new_id, old_id],
            )?;
            res.links_restored += 1;
        }
        same_hash.sort_by(|a, b| a.id.cmp(&b.id));
        for item in &same_hash {
            let (status, sup): (String, Option<String>) = conn.query_row(
                "SELECT status, superseded_by FROM items WHERE id = ?",
                [&item.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if status == item.status && sup == item.superseded_by {
                res.unchanged += 1;
                continue;
            }
            let known = match &item.superseded_by {
                Some(t) => exists(conn, t)?,
                None => false,
            };
            res.status_differs.push(StatusDiffers {
                id: item.id.clone(),
                store: status,
                file: item.status.clone(),
                replacement: if known {
                    item.superseded_by.clone()
                } else {
                    None
                },
            });
        }
        Ok(())
    })?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::connect::connect;

    fn item(id: &str, body: &str, tag: &str, as_of: &str) -> Item {
        Item {
            as_of: as_of.into(),
            ..Item::bare(id, "r", "fact", "T", body, tag)
        }
        .post_init()
    }

    fn store() -> (tempfile::TempDir, Connection) {
        let d = tempfile::tempdir().unwrap();
        let conn = connect(d.path().join("memory.db").to_str().unwrap(), None).unwrap();
        (d, conn)
    }

    // A ValueError on an item is raised again as `ValueError(f"{item.id}: {exc}")`.
    // Probed on Python 555b8f2: a bad tag gives `ValueError: m_2: Invalid tag 'bogus'; ...`.
    #[test]
    fn a_value_error_from_a_check_gets_the_item_id_in_front() {
        let (_d, conn) = store();
        let e = import_items(
            &conn,
            &[item("m_2", "b2", "bogus", "2026-01-02T03:04:05+00:00")],
        )
        .unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: m_2: Invalid tag 'bogus'; expected one of executed, verified, decided, client-stated, derived, assumed, prior, planned"
        );
        let e = import_items(&conn, &[item("m_3", "b3", "decided", "2026-02-30")]).unwrap_err();
        assert_eq!(
            e.python_text(),
            "ValueError: m_3: Invalid ISO 8601 timestamp '2026-02-30', for example 2026-11-21T09:30:00Z"
        );
    }

    // Probed on Python 555b8f2: as_of `9999-12-31T23:59:59-23:59` raises
    // `OverflowError: date value out of range`, which `except ValueError` lets through unchanged.
    #[test]
    fn an_overflowing_as_of_passes_through_with_its_own_class_and_no_id() {
        let (_d, conn) = store();
        let e = import_items(
            &conn,
            &[item("m_1", "b1", "decided", "9999-12-31T23:59:59-23:59")],
        )
        .unwrap_err();
        assert_eq!(
            (e.base, e.class.as_ref(), e.msg.as_str()),
            (Base::Other, "OverflowError", "date value out of range")
        );
        assert!(conn.is_autocommit());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
