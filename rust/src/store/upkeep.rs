use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};

use crate::core::fold::fold;
use crate::core::item::{Item, parse_as_of_epoch};
use crate::core::terms::word_runs;
use crate::error::SmError;
use crate::py::str::translate;
use crate::store::connect::transaction;
use crate::store::items::{bind_all, validate_and_supersede_one};

/// handoff:<YYYYMMDD-HHMMSS>-<session uuid>[-anything].md, the handoff file name the importer reads.
static HANDOFF_SOURCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(
        r"^handoff:(\d{8}-\d{6})-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
    ))
    .expect("handoff source pattern")
});

/// The active item at the end of the supersede chain; None when it
/// ends retired or at a missing id.
pub fn chain_head(conn: &Connection, item_id: &str) -> Result<Option<String>, SmError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut curr = Some(item_id.to_string());
    while let Some(c) = curr {
        if !seen.insert(c.clone()) {
            break;
        }
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT status, superseded_by FROM items WHERE id = ?",
                [&c],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((status, sup)) = row else {
            return Ok(None);
        };
        if status == "active" {
            return Ok(Some(c));
        }
        curr = sup;
    }
    Ok(None)
}

/// The counts `supersede_session_handoffs` returns, in Python's key order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HandoffCounts {
    pub sessions: i64,
    pub superseded: i64,
    pub handoffs_without_session: i64,
}

struct HandoffRow {
    rid: i64,
    id: String,
    repo: String,
    source: String,
    status: String,
}

/// Each active handoff whose file has a `<file>.claimed` sibling, mapped to the claim's first
/// line: the id of the session that started from that handoff after `/clear`. It reads files, so it
/// runs before the write transaction. A claim that cannot be read links nothing, and the handoff
/// stays active.
pub fn handoff_claims(conn: &Connection) -> Result<HashMap<String, String>, SmError> {
    let mut stmt = conn.prepare(
        "SELECT id, evidence FROM items WHERE status = 'active' AND source LIKE 'handoff:%'",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, evidence)| Path::new(evidence).is_file())
        .filter_map(|(id, evidence)| {
            let text = std::fs::read_to_string(format!("{evidence}.claimed")).ok()?;
            Some((id, text.lines().next()?.to_string()))
        })
        .collect())
}

/// Where `id` ends after this pass's moves. The applied run's store already holds them, so there
/// `id` is never a key; the dry run wrote nothing and reads them here.
fn moved_to(moved: &HashMap<String, String>, id: &str) -> String {
    let mut t = id;
    while let Some(n) = moved.get(t) {
        t = n;
    }
    t.to_string()
}

/// True when the item has a `superseded_by` value: `validate_and_supersede_one` refuses an active
/// item with a pointer as a replacement. A blank value (none in the store, 2026-11-07) links
/// nothing too.
fn points_on(conn: &Connection, id: &str) -> Result<bool, SmError> {
    let sup: Option<String> =
        conn.query_row("SELECT superseded_by FROM items WHERE id = ?", [id], |r| {
            r.get(0)
        })?;
    Ok(sup.is_some())
}

fn supersede(conn: &Connection, dry_run: bool, old: &str, new: &str) -> Result<i64, SmError> {
    if dry_run {
        Ok(1)
    } else {
        validate_and_supersede_one(conn, old, new)
    }
}

/// `supersede_session_handoffs(conn, dry_run=...)`: in each (repo, session), the newest handoff
/// file's item, or the active end of its chain, supersedes the session's other active handoffs.
/// Then each active handoff in `claims` (from `handoff_claims`), oldest first, moves to the head
/// of the claiming session's handoffs in its repo when that head's file is newer.
pub fn supersede_session_handoffs(
    conn: &Connection,
    dry_run: bool,
    claims: &HashMap<String, String>,
) -> Result<HandoffCounts, SmError> {
    transaction(conn, None, |conn| {
        let mut stmt =
            conn.prepare("SELECT rowid AS rid, id, repo, source, status FROM items WHERE source LIKE 'handoff:%'")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(HandoffRow {
                    rid: r.get(0)?,
                    id: r.get(1)?,
                    repo: r.get(2)?,
                    source: r.get(3)?,
                    status: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = HandoffCounts::default();
        let mut groups: IndexMap<(&str, &str), Vec<&HandoffRow>> = IndexMap::new();
        for r in &rows {
            let Some(m) = HANDOFF_SOURCE_RE.captures(&r.source) else {
                out.handoffs_without_session += i64::from(r.status == "active");
                continue;
            };
            let session = m
                .get(2)
                .expect("the session group takes part in every match")
                .as_str();
            groups
                .entry((r.repo.as_str(), session))
                .or_default()
                .push(r);
        }
        let newest_of = |rs: &[&HandoffRow]| -> String {
            // max() keeps the first of equal keys; (source, rid) keys are unique
            rs.iter()
                .fold(None::<&HandoffRow>, |best, r| match best {
                    Some(b) if (b.source.as_str(), b.rid) >= (r.source.as_str(), r.rid) => Some(b),
                    _ => Some(*r),
                })
                .map(|r| r.id.clone())
                .unwrap_or_default()
        };
        let mut heads: HashMap<(&str, &str), String> = HashMap::new();
        // each id this pass retires, and the id it now points at
        let mut moved: HashMap<String, String> = HashMap::new();
        for (key, members) in &groups {
            let active: Vec<&HandoffRow> = members
                .iter()
                .copied()
                .filter(|r| r.status == "active")
                .collect();
            if active.is_empty() {
                continue;
            }
            let newest = newest_of(members);
            let head = match chain_head(conn, &newest)? {
                Some(h) if !h.is_empty() => moved_to(&moved, &h),
                _ => newest_of(&active),
            };
            heads.insert(*key, head.clone());
            let older: Vec<&str> = active
                .iter()
                .map(|r| r.id.as_str())
                .filter(|id| *id != head)
                .collect();
            if older.is_empty() {
                continue;
            }
            out.sessions += 1;
            for oid in older {
                out.superseded += supersede(conn, dry_run, oid, &head)?;
                moved.insert(oid.to_string(), head.clone());
            }
        }
        let stamp = |source: &str| HANDOFF_SOURCE_RE.captures(source).map(|m| m[1].to_string());
        let source_of: HashMap<&str, &str> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.source.as_str()))
            .collect();
        let mut claimed: Vec<&HandoffRow> = rows
            .iter()
            .filter(|r| r.status == "active" && claims.contains_key(&r.id))
            .collect();
        claimed.sort_by(|a, b| (a.source.as_str(), a.rid).cmp(&(b.source.as_str(), b.rid)));
        for p in claimed {
            if moved.contains_key(&p.id) {
                continue;
            }
            let Some(head) = heads.get(&(p.repo.as_str(), claims[&p.id].as_str())) else {
                continue;
            };
            let head_stamp = source_of.get(head.as_str()).and_then(|s| stamp(s));
            let (Some(newer), Some(older)) = (head_stamp, stamp(&p.source)) else {
                continue;
            };
            if newer <= older {
                continue;
            }
            // the head may have moved on in this pass: the session pass retires it as a member of
            // another session's group, and that group's head may be a handoff this loop moved
            let target = moved_to(&moved, head);
            if target == p.id || points_on(conn, &target)? {
                continue;
            }
            out.superseded += supersede(conn, dry_run, &p.id, &target)?;
            moved.insert(p.id.clone(), target);
        }
        Ok(out)
    })
}

/// A row of `active_title_rows`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleRow {
    pub id: String,
    pub repo: String,
    pub kind: String,
    pub title: String,
    pub as_of: String,
}

/// `active_title_rows(conn)`.
pub fn active_title_rows(conn: &Connection) -> Result<Vec<TitleRow>, SmError> {
    let mut stmt = conn.prepare(
        "SELECT id, repo, kind, title, as_of FROM items WHERE status = 'active' ORDER BY repo, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TitleRow {
            id: r.get(0)?,
            repo: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            as_of: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A row of `active_rows_with_tags`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagRow {
    pub id: String,
    pub repo: String,
    pub tag: String,
    pub title: String,
    pub as_of: String,
}

/// `active_rows_with_tags(conn, tags)`.
pub fn active_rows_with_tags<S: AsRef<str>>(
    conn: &Connection,
    tags: &[S],
) -> Result<Vec<TagRow>, SmError> {
    let list: Vec<&str> = tags.iter().map(AsRef::as_ref).collect();
    bind_all(&list)?;
    let sql = format!(
        "SELECT id, repo, tag, title, as_of FROM items WHERE status = 'active' AND tag IN ({})",
        vec!["?"; list.len()].join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(&list), |r| {
        Ok(TagRow {
            id: r.get(0)?,
            repo: r.get(1)?,
            tag: r.get(2)?,
            title: r.get(3)?,
            as_of: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A row of `recent_active_work_items`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub as_of: String,
}

/// `recent_active_work_items(conn, repo, since_epoch, exclude_id)`: active task and handoff items of
/// a repo with as_of at or after since_epoch.
pub fn recent_active_work_items(
    conn: &Connection,
    repo: &str,
    since_epoch: f64,
    exclude_id: &str,
) -> Result<Vec<WorkRow>, SmError> {
    bind_all(&[repo, exclude_id])?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, title, body, as_of FROM items \
         WHERE status = 'active' AND repo = ? AND kind IN ('task', 'handoff') AND id != ?",
    )?;
    let rows = stmt
        .query_map([repo, exclude_id], |r| {
            Ok(WorkRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
                as_of: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for r in rows {
        if parse_as_of_epoch(Some(&r.as_of))? >= since_epoch {
            out.push(r);
        }
    }
    Ok(out)
}

const OVERLAP_QUERY_MAX_TERMS: usize = 24;
const OVERLAP_STOPWORDS: &[&str] = &[
    "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "because",
    "been", "before", "being", "between", "both", "but", "by", "can", "could", "did", "do", "does",
    "doing", "done", "down", "during", "each", "for", "from", "further", "had", "has", "have",
    "having", "he", "her", "here", "hers", "herself", "him", "himself", "his", "how", "if", "in",
    "into", "is", "it", "its", "itself", "just", "me", "more", "most", "my", "myself", "no", "nor",
    "not", "now", "of", "off", "on", "once", "only", "or", "other", "our", "ours", "out", "over",
    "own", "same", "she", "should", "so", "some", "such", "than", "that", "the", "their", "theirs",
    "them", "then", "there", "these", "they", "this", "those", "through", "to", "too", "under",
    "until", "up", "very", "was", "we", "were", "what", "when", "where", "which", "while", "who",
    "whom", "why", "will", "with", "would", "you", "your", "yours",
];

/// `durable_overlap_candidates(conn, item, limit)`: top active durable items (`fact`, `decision`,
/// `learning`) in the same repo matching `item`'s folded words via `items_fold` FTS5.
pub fn durable_overlap_candidates(
    conn: &Connection,
    item: &Item,
    limit: i64,
) -> Result<Vec<WorkRow>, SmError> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let text = format!("{}\n{}", item.title, item.body);
    let folded = fold(Some(&text));
    let mut all_words: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for w in word_runs(&folded) {
        if seen.insert(w) {
            all_words.push(w);
        }
    }
    let mut terms: Vec<&str> = all_words
        .iter()
        .copied()
        .filter(|w| w.chars().count() > 1 && !OVERLAP_STOPWORDS.contains(w))
        .take(OVERLAP_QUERY_MAX_TERMS)
        .collect();
    if terms.is_empty() {
        terms = all_words
            .into_iter()
            .take(OVERLAP_QUERY_MAX_TERMS)
            .collect();
    }
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let fts_q = terms
        .iter()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    bind_all(&[&fts_q, &item.repo, &item.id])?;
    let mut stmt = conn.prepare(
        "SELECT i.id, i.kind, i.title, i.body, i.as_of \
         FROM items_fold \
         JOIN items i ON i.rowid = items_fold.rowid \
         WHERE items_fold MATCH ? \
           AND i.status = 'active' \
           AND i.repo = ? \
           AND i.kind IN ('fact', 'decision', 'learning') \
           AND i.id != ? \
         ORDER BY bm25(items_fold, 10.0, 5.0, 3.0, 1.0) \
         LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![fts_q, item.repo, item.id, limit], |r| {
            Ok(WorkRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
                as_of: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
