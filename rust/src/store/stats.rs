use indexmap::IndexMap;
use rusqlite::Connection;

use crate::error::SmError;
use crate::store::items::bind_all;

pub const TOP_REPOS: i64 = 10;
/// The rank penalty's predicate (`is_legacy_unreorganized`), in SQL.
const LEGACY_SQL: &str = "(id LIKE 'legacy:%' OR source LIKE 'legacy:%' OR tag = 'n/a')";

/// `store_counts(conn)`, keys in Python's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreCounts {
    pub status: IndexMap<String, i64>,
    pub active: i64,
    pub retired: i64,
    pub repos: IndexMap<String, i64>,
    pub repo_count: i64,
    pub kinds: IndexMap<String, i64>,
    pub tags: IndexMap<String, i64>,
    pub unassigned: i64,
    pub legacy: i64,
}

impl StoreCounts {
    /// Status counts with retired items separated from `superseded` when `retired > 0`.
    pub fn status_breakdown(&self) -> IndexMap<String, i64> {
        if self.retired <= 0 {
            return self.status.clone();
        }
        let mut out = IndexMap::new();
        if let Some(&a) = self.status.get("active") {
            out.insert("active".into(), a);
        }
        let raw_sup = self.status.get("superseded").copied().unwrap_or(0);
        out.insert("superseded".into(), raw_sup.saturating_sub(self.retired));
        out.insert("retired".into(), self.retired);
        for (k, &v) in &self.status {
            if k != "active" && k != "superseded" {
                out.insert(k.clone(), v);
            }
        }
        out
    }
}

fn counts(conn: &Connection, sql: &str) -> Result<IndexMap<String, i64>, SmError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<IndexMap<_, _>>>()?)
}

fn one(conn: &Connection, sql: &str) -> Result<i64, SmError> {
    Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?)
}

/// `store_counts(conn)`: every status; repo (top 10), kind and tag of the active items; the active
/// `unassigned` and legacy counts.
pub fn store_counts(conn: &Connection) -> Result<StoreCounts, SmError> {
    let active = "FROM items WHERE status = 'active'";
    Ok(StoreCounts {
        status: counts(
            conn,
            "SELECT status, count(*) FROM items GROUP BY status ORDER BY 2 DESC, 1",
        )?,
        active: one(conn, &format!("SELECT count(*) {active}"))?,
        retired: one(
            conn,
            "SELECT count(*) FROM items WHERE status = 'superseded' AND (superseded_by IS NULL OR superseded_by = '')",
        )?,
        repos: counts(
            conn,
            &format!(
                "SELECT repo, count(*) {active} GROUP BY repo ORDER BY 2 DESC, 1 LIMIT {TOP_REPOS}"
            ),
        )?,
        repo_count: one(conn, &format!("SELECT count(DISTINCT repo) {active}"))?,
        kinds: counts(
            conn,
            &format!("SELECT kind, count(*) {active} GROUP BY kind ORDER BY 2 DESC, 1"),
        )?,
        tags: counts(
            conn,
            &format!("SELECT tag, count(*) {active} GROUP BY tag ORDER BY 2 DESC, 1"),
        )?,
        unassigned: one(
            conn,
            &format!("SELECT count(*) {active} AND repo = 'unassigned'"),
        )?,
        legacy: one(conn, &format!("SELECT count(*) {active} AND {LEGACY_SQL}"))?,
    })
}

/// `item_labels(conn, ids)`: id -> (repo, kind, status) for the ids that exist.
pub fn item_labels<S: AsRef<str>>(
    conn: &Connection,
    ids: &[S],
) -> Result<IndexMap<String, (String, String, String)>, SmError> {
    let list: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
    bind_all(&list)?;
    let sql = format!(
        "SELECT id, repo, kind, status FROM items WHERE id IN ({})",
        vec!["?"; list.len()].join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(&list), |r| {
        Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<IndexMap<_, _>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::item::Item;
    use crate::store::connect::connect;
    use crate::store::items::put_item;

    #[test]
    fn store_counts_separates_retired_from_replaced_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let conn = connect(db.to_str().unwrap(), None).unwrap();

        // Empty store: retired=0 and breakdown is empty.
        let empty = store_counts(&conn).unwrap();
        assert_eq!(empty.retired, 0);
        assert!(empty.status_breakdown().is_empty());

        // Seed 1 active + 1 replaced superseded: retired=0 leaves status unchanged.
        let a = Item::bare("m_a", "alpha", "fact", "A", "body a", "decided");
        put_item(&conn, &a.post_init(), None).unwrap();
        let mut s = Item::bare("m_s", "alpha", "fact", "S", "body s", "decided");
        s.status = "superseded".into();
        s.superseded_by = Some("m_a".into());
        put_item(&conn, &s.post_init(), None).unwrap();

        let no_ret = store_counts(&conn).unwrap();
        assert_eq!(no_ret.retired, 0);
        assert_eq!(
            no_ret.status_breakdown().into_iter().collect::<Vec<_>>(),
            vec![("active".into(), 1), ("superseded".into(), 1)]
        );

        // Add 2 retired items (one NULL superseded_by, one empty-string superseded_by).
        let mut r1 = Item::bare("m_r1", "alpha", "fact", "R1", "body r1", "decided");
        r1.status = "superseded".into();
        r1.superseded_by = None;
        r1.tags = "retired:manual".into();
        put_item(&conn, &r1.post_init(), None).unwrap();

        let mut r2 = Item::bare("m_r2", "alpha", "fact", "R2", "body r2", "decided");
        r2.status = "superseded".into();
        r2.superseded_by = Some(String::new());
        r2.tags = "retired:obsolete".into();
        put_item(&conn, &r2.post_init(), None).unwrap();

        let with_ret = store_counts(&conn).unwrap();
        assert_eq!(with_ret.active, 1);
        assert_eq!(with_ret.retired, 2);
        assert_eq!(
            with_ret.status_breakdown().into_iter().collect::<Vec<_>>(),
            vec![
                ("active".into(), 1),
                ("superseded".into(), 1),
                ("retired".into(), 2),
            ]
        );
    }
}
