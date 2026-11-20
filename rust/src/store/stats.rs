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
    pub repos: IndexMap<String, i64>,
    pub repo_count: i64,
    pub kinds: IndexMap<String, i64>,
    pub tags: IndexMap<String, i64>,
    pub unassigned: i64,
    pub legacy: i64,
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
