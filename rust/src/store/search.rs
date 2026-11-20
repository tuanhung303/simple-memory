use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;
use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use unicode_normalization::UnicodeNormalization;

use crate::core::fold::{fold, has_accents};
use crate::core::item::{Item, parse_as_of_epoch};
use crate::core::rank::{
    BIASED_CANDIDATE_LIMIT, Candidate, FTS_CANDIDATE_LIMIT, ScoreOpts, Scored, WEIGHT_BODY,
    WEIGHT_REFS, WEIGHT_TAGS, WEIGHT_TITLE, bias_widens_pool, item_matches_exclusion,
    parse_query_exclusions, score_candidates,
};
use crate::core::refs::{RefsArg, extract_refs, split_refs};
use crate::core::terms::build_fts_query;
use crate::error::SmError;
use crate::store::items::{ITEM_COLUMNS, row_to_item};

/// `keyword_aliases` as `build_fts_query` takes them.
pub type KeywordAliases = crate::core::terms::Aliases;

/// Status filter for `Filters::status_filter` (`sm list --status ...`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Active,
    Superseded,
    Retired,
    All,
}

impl StatusFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusFilter::Active => "active",
            StatusFilter::Superseded => "superseded",
            StatusFilter::Retired => "retired",
            StatusFilter::All => "all",
        }
    }
}

/// The scope keyword arguments every search function takes (`repo`, `include_all_repos`,
/// `tags_filter`, `kinds_filter`, `include_history`, plus Wave 2 filters). An empty list filters
/// nothing, as in Python.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filters {
    pub repo: Option<String>,
    pub include_all_repos: bool,
    /// When true and `repo` is active (not `include_all_repos`), emit `{p}repo = ?` instead of
    /// `{p}repo IN (?, 'global', 'unassigned')`.
    pub exact_repo: bool,
    pub tags_filter: Option<Vec<String>>,
    pub kinds_filter: Option<Vec<String>>,
    pub include_history: bool,
    /// Explicit status filter; overrides `include_history` when `Some`.
    pub status_filter: Option<StatusFilter>,
    /// `[scopes]` matchers (`config::scope_matchers`): an item passes when any matches it, so an
    /// empty list passes none.
    pub scope: Option<Vec<String>>,
    /// Additive `[scopes]` matchers ORed with the repo clause when `repo` is active (not
    /// `include_all_repos`).
    pub extra_scope: Option<Vec<String>>,
    /// Case-sensitive prefix filters on `items.source` (OR-joined across non-empty prefixes).
    pub sources_filter: Option<Vec<String>>,
    /// Inclusive lower bound on `sm_epoch(as_of)` (`>=`).
    pub since_epoch: Option<f64>,
    /// Upper bound on `sm_epoch(as_of)` (`<` when `until_exclusive`, else `<=`).
    pub until_epoch: Option<f64>,
    pub until_exclusive: bool,
}

/// Inclusive lower bound and exclusive upper bound for a case-sensitive prefix match on an indexed
/// SQLite TEXT column (`source >= low AND source < high`). Skips UTF-16 surrogates
/// (`0xD800..=0xDFFF`) and `py::str` surrogateescape stand-ins (`0x10FD80..=0x10FDFF`).
pub fn source_prefix_bounds(prefix: &str) -> (String, Option<String>) {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(c) = chars.pop() {
        let mut next = c as u32 + 1;
        if (0xD800..=0xDFFF).contains(&next) {
            next = 0xE000;
        }
        if (crate::py::str::STAND_IN_BASE + 0x80..=crate::py::str::STAND_IN_BASE + 0xFF)
            .contains(&next)
        {
            next = crate::py::str::STAND_IN_BASE + 0x100;
        }
        if let Some(nc) = char::from_u32(next) {
            chars.push(nc);
            return (prefix.to_string(), Some(chars.into_iter().collect()));
        }
    }
    (prefix.to_string(), None)
}

fn non_empty(v: &Option<Vec<String>>) -> Option<&[String]> {
    v.as_deref().filter(|v| !v.is_empty())
}

/// sqlite3 encodes each str parameter in order before it runs a statement, so a stand-in (a
/// surrogateescape byte, a lone surrogate to Python) raises UnicodeEncodeError naming its
/// position in that parameter.
fn check_params(params: &[Sql]) -> Result<(), SmError> {
    for p in params {
        if let Sql::Text(s) = p {
            crate::py::path::encode_text(s)?;
        }
    }
    Ok(())
}

fn text(s: &str) -> Sql {
    Sql::Text(s.to_string())
}

fn matcher_clauses(p: &str, matchers: &[String], params: &mut Vec<Sql>) -> String {
    let mut any: Vec<String> = Vec::new();
    for m in matchers {
        if let Some(r) = m.strip_prefix("repo:") {
            any.push(format!("{p}repo = ?"));
            params.push(text(r));
        }
    }
    for m in matchers {
        if m.strip_prefix("repo:").is_none() {
            any.push(format!("sm_word(?, {p}title, {p}body, {p}refs, {p}tags)"));
            params.push(text(m));
        }
    }
    any.join(" OR ")
}

/// (the WHERE clauses joined by AND, else `1=1`; params).
pub fn build_item_filters(alias: &str, f: &Filters) -> (String, Vec<Sql>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Sql> = Vec::new();
    let p = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    match f.status_filter {
        None => {
            if !f.include_history {
                clauses.push(format!("{p}status = 'active'"));
            }
        }
        Some(StatusFilter::Active) => {
            clauses.push(format!("{p}status = 'active'"));
        }
        Some(StatusFilter::Superseded) => {
            clauses.push(format!(
                "({p}status = 'superseded' AND {p}superseded_by IS NOT NULL AND {p}superseded_by != '')"
            ));
        }
        Some(StatusFilter::Retired) => {
            clauses.push(format!(
                "({p}status = 'superseded' AND ({p}superseded_by IS NULL OR {p}superseded_by = ''))"
            ));
        }
        Some(StatusFilter::All) => {}
    }
    if let Some(repo) = f
        .repo
        .as_deref()
        .filter(|r| !f.include_all_repos && !r.is_empty())
    {
        params.push(text(repo));
        let repo_clause = if f.exact_repo {
            format!("{p}repo = ?")
        } else {
            format!("{p}repo IN (?, 'global', 'unassigned')")
        };
        if let Some(extra) = non_empty(&f.extra_scope) {
            let extra_sql = matcher_clauses(&p, extra, &mut params);
            clauses.push(format!("({repo_clause} OR ({extra_sql}))"));
        } else {
            clauses.push(repo_clause);
        }
    }
    if let Some(tags) = non_empty(&f.tags_filter) {
        clauses.push(format!("{p}tag IN ({})", vec!["?"; tags.len()].join(", ")));
        params.extend(tags.iter().map(|t| text(t)));
    }
    if let Some(kinds) = non_empty(&f.kinds_filter) {
        clauses.push(format!(
            "{p}kind IN ({})",
            vec!["?"; kinds.len()].join(", ")
        ));
        params.extend(kinds.iter().map(|k| text(k)));
    }
    if let Some(matchers) = &f.scope {
        if matchers.is_empty() {
            clauses.push("0".to_string());
        } else {
            let scope_sql = matcher_clauses(&p, matchers, &mut params);
            clauses.push(format!("({scope_sql})"));
        }
    }
    if let Some(sources) = &f.sources_filter {
        let non_empty_sources: Vec<&str> = sources
            .iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        if !non_empty_sources.is_empty() {
            let mut parts = Vec::with_capacity(non_empty_sources.len());
            for prefix in non_empty_sources {
                let (low, high) = source_prefix_bounds(prefix);
                if let Some(hi) = high {
                    parts.push(format!("({p}source >= ? AND {p}source < ?)"));
                    params.push(text(&low));
                    params.push(text(&hi));
                } else {
                    parts.push(format!("{p}source >= ?"));
                    params.push(text(&low));
                }
            }
            clauses.push(format!("({})", parts.join(" OR ")));
        }
    }
    if let Some(since) = f.since_epoch {
        clauses.push(format!("sm_epoch({p}as_of) >= ?"));
        params.push(Sql::Real(since));
    }
    if let Some(until) = f.until_epoch {
        let op = if f.until_exclusive { "<" } else { "<=" };
        clauses.push(format!("sm_epoch({p}as_of) {op} ?"));
        params.push(Sql::Real(until));
    }
    let where_sql = if clauses.is_empty() {
        "1=1".to_string()
    } else {
        clauses.join(" AND ")
    };
    (where_sql, params)
}

fn sql_err(e: rusqlite::Error) -> SmError {
    SmError::sqlite(&e)
}

/// `_capped_scores`: rowid -> bm25 of the best `limit` rows by score, or with by_date the newest
/// `limit` by parsed as_of then id (as_of strings may carry different offsets, so not by text).
fn capped_scores(
    conn: &Connection,
    sql: &str,
    params: Vec<Sql>,
    limit: i64,
    by_date: bool,
) -> Result<IndexMap<i64, f64>, SmError> {
    struct Row {
        rid: i64,
        id: String,
        as_of: String,
        score: f64,
    }
    let read = |sql: &str, params: Vec<Sql>| -> Result<Vec<Row>, SmError> {
        check_params(&params)?;
        let mut stmt = conn.prepare(sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok(Row {
                    rid: r.get("rid")?,
                    id: r.get("id")?,
                    as_of: r.get("as_of")?,
                    score: r.get("score")?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)
    };
    let rows = if !by_date {
        let mut p = params;
        p.push(Sql::Integer(limit));
        read(&format!("{sql} ORDER BY score LIMIT ?"), p)?
    } else {
        let all = read(sql, params)?;
        let mut keyed = all
            .into_iter()
            .map(|r| Ok((parse_as_of_epoch(Some(&r.as_of))?, r)))
            .collect::<Result<Vec<_>, SmError>>()?;
        // sorted(..., reverse=True) is stable: equal keys keep their row order
        keyed.sort_by(|(ta, a), (tb, b)| {
            tb.partial_cmp(ta)
                .unwrap_or(Ordering::Equal)
                .then_with(|| b.id.cmp(&a.id))
        });
        keyed
            .into_iter()
            .take(usize::try_from(limit.max(0)).unwrap_or(usize::MAX))
            .map(|(_, r)| r)
            .collect()
    };
    Ok(rows.into_iter().map(|r| (r.rid, r.score)).collect())
}

/// The keyword arguments of `search_candidates` past the connection.
#[derive(Clone, Debug)]
pub struct CandidateQuery<'a> {
    pub query_refs: &'a [String],
    pub fold_fts_query: &'a str,
    pub raw_fts_query: &'a str,
    pub candidate_limit: i64,
    pub by_date: bool,
}

impl Default for CandidateQuery<'_> {
    fn default() -> Self {
        CandidateQuery {
            query_refs: &[],
            fold_fts_query: "",
            raw_fts_query: "",
            candidate_limit: FTS_CANDIDATE_LIMIT,
            by_date: false,
        }
    }
}

/// `search_candidates`: FTS matches capped at candidate_limit (the best bm25 scores, or with
/// by_date the newest), plus every item whose refs hold a query ref.
pub fn search_candidates(
    conn: &Connection,
    q: &CandidateQuery,
    f: &Filters,
) -> Result<Vec<Candidate>, SmError> {
    let (where_i, filter_params) = build_item_filters("i", f);
    let mut fold_scores: IndexMap<i64, f64> = IndexMap::new();
    let mut raw_scores: IndexMap<i64, f64> = IndexMap::new();
    let mut ref_rowids: HashSet<i64> = HashSet::new();

    if !q.query_refs.is_empty() {
        let mut wanted_folded: Vec<String> = Vec::new();
        for r in q.query_refs.iter().filter(|r| !r.is_empty()) {
            let folded = fold(Some(r));
            if !wanted_folded.contains(&folded) {
                wanted_folded.push(folded);
            }
        }
        let like_clauses = vec!["lower(i.refs) LIKE ?"; wanted_folded.len()].join(" OR ");
        let mut params: Vec<Sql> = wanted_folded
            .iter()
            .map(|r| Sql::Text(format!("%{r}%")))
            .collect();
        params.extend(filter_params.iter().cloned());
        check_params(&params)?;
        let sql = format!(
            "SELECT i.rowid AS rid, i.refs FROM items i WHERE ({like_clauses}) AND {where_i}"
        );
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok((r.get::<_, i64>("rid")?, r.get::<_, String>("refs")?))
            })
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        let wanted: HashSet<&String> = wanted_folded.iter().collect();
        for (rid, refs) in rows {
            if split_refs(Some(&RefsArg::Str(&refs)))
                .iter()
                .any(|t| wanted.contains(&fold(Some(t))))
            {
                ref_rowids.insert(rid);
            }
        }
    }

    if !q.fold_fts_query.is_empty() {
        let sql_fold = format!(
            "SELECT i.rowid AS rid, i.id, i.as_of, bm25(items_fold, {}, {}, {}, {}) AS score FROM items_fold \
             JOIN items i ON i.rowid = items_fold.rowid WHERE items_fold MATCH ? AND {where_i}",
            py_float(WEIGHT_REFS),
            py_float(WEIGHT_TITLE),
            py_float(WEIGHT_TAGS),
            py_float(WEIGHT_BODY),
        );
        let mut params = vec![text(q.fold_fts_query)];
        params.extend(filter_params.iter().cloned());
        fold_scores = capped_scores(conn, &sql_fold, params, q.candidate_limit, q.by_date)?;
    }

    if !q.raw_fts_query.is_empty() {
        let sql_raw = format!(
            "SELECT i.rowid AS rid, i.id, i.as_of, bm25(items_raw, {}, {}) AS score FROM items_raw \
             JOIN items i ON i.rowid = items_raw.rowid WHERE items_raw MATCH ? AND {where_i}",
            py_float(WEIGHT_TITLE),
            py_float(WEIGHT_BODY),
        );
        let mut params = vec![text(q.raw_fts_query)];
        params.extend(filter_params.iter().cloned());
        raw_scores = capped_scores(conn, &sql_raw, params, q.candidate_limit, q.by_date)?;
    }

    let all_rowids: BTreeSet<i64> = ref_rowids
        .iter()
        .chain(fold_scores.keys())
        .chain(raw_scores.keys())
        .copied()
        .collect();
    if all_rowids.is_empty() {
        return Ok(Vec::new());
    }
    let cols_sql = ITEM_COLUMNS
        .iter()
        .map(|c| format!("i.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let all_rowids: Vec<i64> = all_rowids.into_iter().collect();
    let mut rows = Vec::with_capacity(all_rowids.len());
    // SQLite binds at most 32,766 variables per statement
    for chunk in all_rowids.chunks(10_000) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT i.rowid AS rid, {cols_sql} FROM items i WHERE i.rowid IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let found = stmt
            .query_map(rusqlite::params_from_iter(chunk), |r| {
                Ok((r.get::<_, i64>("rid")?, row_to_item(r)?))
            })
            .map_err(sql_err)?;
        for row in found {
            rows.push(row.map_err(sql_err)?);
        }
    }
    Ok(rows
        .into_iter()
        .map(|(rid, item)| Candidate {
            item,
            fold_bm25: fold_scores.get(&rid).copied(),
            raw_bm25: raw_scores.get(&rid).copied(),
        })
        .collect())
}

/// Python's f-string of a float weight, as it lands in the bm25 SQL (`10.0`).
fn py_float(x: f64) -> String {
    crate::py::float::repr(x)
}

/// The query options every ranked search takes past the scope.
#[derive(Clone, Copy, Debug)]
pub struct SearchOpts<'a> {
    pub recent: bool,
    pub aliases: Option<&'a KeywordAliases>,
    pub bias: &'a [String],
    pub scope_biases: &'a [(String, Vec<String>)],
}

/// (unranked candidates, query refs, the pool size when its fetch filled, positive query).
type Gathered = (Vec<Candidate>, Vec<String>, Option<i64>, String);

/// `_gather`: (unranked candidates, query refs, the FTS rows of the pool when its last fetch
/// filled, else None, and the positive query after stripping exclusions and standalone uppercase
/// boolean noise).
fn gather(
    conn: &Connection,
    query: &str,
    f: &Filters,
    o: &SearchOpts,
) -> Result<Gathered, SmError> {
    // \w stops at a combining mark, so an NFD query splits its accented words: the raw FTS terms
    // and the accent and phrase tiers need the stored (NFC) form
    let query: String = query.nfc().collect();
    let (positive_query, excluded) = parse_query_exclusions(&query);
    let q_clean = crate::py::str::strip(&positive_query);
    if q_clean.is_empty() {
        return Ok((Vec::new(), Vec::new(), None, positive_query));
    }
    let mut query_refs = extract_refs(Some(&positive_query));
    if !q_clean.contains(' ') && !query_refs.iter().any(|r| r == q_clean) {
        query_refs.push(q_clean.to_string());
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut cap = FTS_CANDIDATE_LIMIT;
    let mut pool = None;
    let fold_q_and = build_fts_query(Some(&positive_query), o.aliases, true, "AND")?;
    let raw_q_and = if has_accents(Some(&positive_query)) {
        build_fts_query(Some(&positive_query), o.aliases, false, "AND")?
    } else {
        String::new()
    };
    if o.recent {
        (candidates, pool) = fetch_kept(cap, &excluded, |limit| {
            let q = CandidateQuery {
                fold_fts_query: &fold_q_and,
                raw_fts_query: &raw_q_and,
                candidate_limit: limit,
                by_date: true,
                ..Default::default()
            };
            search_candidates(conn, &q, f)
        })?;
    }
    if candidates.is_empty() {
        cap = if (bias_widens_pool(o.bias) || !o.scope_biases.is_empty() || !excluded.is_empty())
            && !o.recent
        {
            BIASED_CANDIDATE_LIMIT
        } else {
            FTS_CANDIDATE_LIMIT
        };
        let fold_q = build_fts_query(Some(&positive_query), o.aliases, true, "OR")?;
        let raw_q = if has_accents(Some(&positive_query)) {
            build_fts_query(Some(&positive_query), o.aliases, false, "OR")?
        } else {
            String::new()
        };
        // an item with every term can lose the OR pool to many one-term rows; the AND form
        // finds it at the same bm25 (one term: same rows, so it is skipped)
        let add_and = !o.recent && fold_q_and.contains(" AND ");
        (candidates, pool) = fetch_kept(cap, &excluded, |limit| {
            let q = CandidateQuery {
                query_refs: &query_refs,
                fold_fts_query: &fold_q,
                raw_fts_query: &raw_q,
                candidate_limit: limit,
                by_date: o.recent,
            };
            let mut found = search_candidates(conn, &q, f)?;
            if add_and {
                let q = CandidateQuery {
                    fold_fts_query: &fold_q_and,
                    raw_fts_query: &raw_q_and,
                    candidate_limit: limit,
                    ..Default::default()
                };
                let seen: HashSet<String> = found.iter().map(|c| c.item.id.clone()).collect();
                let extra = search_candidates(conn, &q, f)?;
                found.extend(extra.into_iter().filter(|c| !seen.contains(&c.item.id)));
            }
            Ok(found)
        })?;
    }
    Ok((candidates, query_refs, pool, positive_query))
}

/// (the fetched candidates less the excluded ones, the FTS rows fetched when they filled the
/// limit: the limit, plus the AND rows the OR rows lacked). The
/// exclusions run after the SQL limit, so a full fetch with fewer than `cap` FTS matches left
/// is fetched again 4 times wider. Each accented-index match also matches the folded index,
/// so the folded count decides.
fn fetch_kept(
    cap: i64,
    excluded: &[String],
    mut fetch: impl FnMut(i64) -> Result<Vec<Candidate>, SmError>,
) -> Result<(Vec<Candidate>, Option<i64>), SmError> {
    let fts = |cs: &[Candidate]| cs.iter().filter(|c| c.fold_bm25.is_some()).count() as i64;
    let mut limit = cap;
    loop {
        let mut found = fetch(limit)?;
        let fetched = fts(&found);
        let full = fetched >= limit;
        found.retain(|c| !item_matches_exclusion(&c.item, excluded));
        if !full || fts(&found) >= cap {
            return Ok((found, full.then_some(fetched)));
        }
        limit *= 4;
    }
}

fn score_opts<'a>(
    query_refs: &'a [String],
    f: &'a Filters,
    o: &'a SearchOpts,
    explain: Option<i64>,
) -> ScoreOpts<'a> {
    ScoreOpts {
        query_refs: Some(query_refs),
        current_repo: f.repo.as_deref(),
        recent: o.recent,
        now_epoch: None,
        bias: o.bias,
        scope_biases: o.scope_biases,
        explain,
        aliases: o.aliases,
    }
}

/// `search_scored`: (every candidate in rank order with why, the pool size when its fetch
/// filled, else None). Terms go on the first `explain` only.
pub fn search_scored(
    conn: &Connection,
    query: &str,
    f: &Filters,
    o: &SearchOpts,
    explain: Option<i64>,
) -> Result<(Vec<Scored>, Option<i64>), SmError> {
    let (candidates, query_refs, cap, positive_query) = gather(conn, query, f, o)?;
    let scored = score_candidates(
        &candidates,
        &positive_query,
        &score_opts(&query_refs, f, o, explain),
    )?;
    Ok((scored, cap))
}

/// `search_pool`: (the items of the best `size` ranked matches, in no set order; the size when
/// the pool was full). Ranking runs only when a full pool holds more than its size, or with recent.
pub fn search_pool(
    conn: &Connection,
    query: &str,
    f: &Filters,
    o: &SearchOpts,
) -> Result<(Vec<Item>, Option<i64>), SmError> {
    let (candidates, query_refs, cap, positive_query) = gather(conn, query, f, o)?;
    if !o.recent && cap.is_none_or(|c| candidates.len() as i64 <= c) {
        return Ok((candidates.into_iter().map(|c| c.item).collect(), cap));
    }
    let scored = score_candidates(
        &candidates,
        &positive_query,
        &score_opts(&query_refs, f, o, Some(0)),
    )?;
    let items: Vec<Item> = scored.into_iter().map(|s| s.item).collect();
    Ok(match cap {
        Some(c) if c != 0 => (head(items, c), cap),
        _ => (items, cap),
    })
}

/// Python's `seq[:n]`: a negative n counts from the end.
pub fn head<T>(mut v: Vec<T>, n: i64) -> Vec<T> {
    let len = v.len() as i64;
    let keep = if n < 0 { (len + n).max(0) } else { n.min(len) };
    v.truncate(keep as usize);
    v
}

/// `search(conn, query, ..., limit=...)`: the first `limit` items in rank order.
pub fn search(
    conn: &Connection,
    query: &str,
    f: &Filters,
    o: &SearchOpts,
    limit: i64,
) -> Result<Vec<Item>, SmError> {
    if limit <= 0 {
        return Err(SmError::value("--limit must be at least 1"));
    }
    let (scored, _) = search_scored(conn, query, f, o, Some(0))?;
    Ok(head(scored, limit).into_iter().map(|s| s.item).collect())
}

/// `count_matches`: items in scope that match fts_query on the folded index; 0 for an empty query.
pub fn count_matches(conn: &Connection, fts_query: &str, f: &Filters) -> Result<i64, SmError> {
    if fts_query.is_empty() {
        return Ok(0);
    }
    let (where_i, params) = build_item_filters("i", f);
    let mut all = vec![text(fts_query)];
    all.extend(params);
    count(
        conn,
        &format!(
            "SELECT COUNT(*) FROM items_fold JOIN items i ON i.rowid = items_fold.rowid WHERE items_fold MATCH ? AND {where_i}"
        ),
        all,
    )
}

fn count(conn: &Connection, sql: &str, params: Vec<Sql>) -> Result<i64, SmError> {
    check_params(&params)?;
    conn.query_row(sql, rusqlite::params_from_iter(params), |r| r.get(0))
        .map_err(sql_err)
}

/// `count_scope`: items in scope (`count_matches`'s filters, no text match).
pub fn count_scope(conn: &Connection, f: &Filters) -> Result<i64, SmError> {
    let (where_i, params) = build_item_filters("i", f);
    count(
        conn,
        &format!("SELECT COUNT(*) FROM items i WHERE {where_i}"),
        params,
    )
}

/// Result of a paginated `list_items` query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListResult {
    pub items: Vec<Item>,
    pub total: i64,
}

/// Paginated items matching `f`, ordered newest `sm_epoch(as_of)` first (`NULL` last), ties by `id DESC`.
pub fn list_items(
    conn: &Connection,
    f: &Filters,
    limit: i64,
    offset: i64,
) -> Result<ListResult, SmError> {
    if limit <= 0 {
        return Err(SmError::value("--limit must be at least 1"));
    }
    if offset < 0 {
        return Err(SmError::value("--offset must be at least 0"));
    }
    let total = count_scope(conn, f)?;
    let (where_i, mut params) = build_item_filters("i", f);
    params.push(Sql::Integer(limit));
    params.push(Sql::Integer(offset));
    check_params(&params)?;
    let cols_sql = ITEM_COLUMNS
        .iter()
        .map(|c| format!("i.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {cols_sql} FROM items i WHERE {where_i} ORDER BY sm_epoch(i.as_of) DESC NULLS LAST, i.id DESC LIMIT ? OFFSET ?"
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), row_to_item)
        .map_err(sql_err)?;
    let items = rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)?;
    Ok(ListResult { items, total })
}

/// Counts of items matching `f` in repos other than `exclude_repo`, ordered by count descending
/// then repo name ascending, capped at `limit`.
pub fn list_other_repos(
    conn: &Connection,
    f: &Filters,
    exclude_repo: &str,
    limit: usize,
) -> Result<Vec<(String, i64)>, SmError> {
    let (where_i, mut params) = build_item_filters("i", f);
    params.push(text(exclude_repo));
    params.push(Sql::Integer(limit as i64));
    check_params(&params)?;
    let sql = format!(
        "SELECT i.repo, COUNT(*) FROM items i WHERE ({where_i}) AND i.repo != ? GROUP BY i.repo ORDER BY 2 DESC, 1 LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(sql_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)
}

/// `ids_matching`: the ids among `ids` whose folded text matches fts_query.
pub fn ids_matching(
    conn: &Connection,
    fts_query: &str,
    ids: &[String],
) -> Result<HashSet<String>, SmError> {
    if fts_query.is_empty() || ids.is_empty() {
        return Ok(HashSet::new());
    }
    let marks = vec!["?"; ids.len()].join(", ");
    let mut params = vec![text(fts_query)];
    params.extend(ids.iter().map(|i| text(i)));
    check_params(&params)?;
    let sql = format!(
        "SELECT i.id FROM items_fold JOIN items i ON i.rowid = items_fold.rowid WHERE items_fold MATCH ? AND i.id IN ({marks})"
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            r.get::<_, String>(0)
        })
        .map_err(sql_err)?;
    rows.collect::<Result<HashSet<_>, _>>().map_err(sql_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        crate::store::connect::connect(":memory:", None).unwrap()
    }

    fn put(
        conn: &Connection,
        id: &str,
        repo: &str,
        title: &str,
        body: &str,
        extra: &[(&str, &str)],
    ) {
        let mut cols = vec![
            "id",
            "repo",
            "kind",
            "title",
            "body",
            "tag",
            "as_of",
            "content_hash",
        ];
        let mut vals: Vec<String> = [
            id,
            repo,
            "fact",
            title,
            body,
            "decided",
            "2026-01-01T00:00:00+00:00",
            id,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for (k, v) in extra {
            if let Some(i) = cols.iter().position(|c| c == k) {
                vals[i] = v.to_string();
            } else {
                cols.push(k);
                vals.push(v.to_string());
            }
        }
        let marks = vec!["?"; cols.len()].join(", ");
        conn.execute(
            &format!("INSERT INTO items ({}) VALUES ({marks})", cols.join(", ")),
            rusqlite::params_from_iter(vals),
        )
        .unwrap();
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    fn bias() -> Vec<String> {
        vec!["recency".to_string()]
    }

    fn opts(b: &[String]) -> SearchOpts<'_> {
        SearchOpts {
            recent: false,
            aliases: None,
            bias: b,
            scope_biases: &[],
        }
    }

    #[test]
    fn filters_sql_and_params() {
        let f = Filters {
            repo: Some("r".into()),
            tags_filter: Some(vec!["decided".into()]),
            ..Default::default()
        };
        let (sql, p) = build_item_filters("i", &f);
        assert_eq!(
            sql,
            "i.status = 'active' AND i.repo IN (?, 'global', 'unassigned') AND i.tag IN (?)"
        );
        assert_eq!(p, vec![text("r"), text("decided")]);
        // all repos and history: no clause; an empty list filters nothing
        let f = Filters {
            repo: Some("r".into()),
            include_all_repos: true,
            include_history: true,
            kinds_filter: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(build_item_filters("", &f), ("1=1".to_string(), vec![]));
        let f = Filters {
            repo: Some(String::new()),
            kinds_filter: Some(vec!["fact".into(), "task".into()]),
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("", &f).0,
            "status = 'active' AND kind IN (?, ?)"
        );
    }

    #[test]
    fn scope_and_history() {
        let c = store();
        put(&c, "m_a", "r", "loader alpha", "x", &[]);
        put(&c, "m_b", "other", "loader beta", "x", &[]);
        put(&c, "m_g", "global", "loader gamma", "x", &[]);
        put(
            &c,
            "m_old",
            "r",
            "loader old",
            "x",
            &[("status", "superseded")],
        );
        let b = bias();
        let scoped = Filters {
            repo: Some("r".into()),
            ..Default::default()
        };
        let mut got = ids(&search(&c, "loader", &scoped, &opts(&b), 10).unwrap())
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        got.sort();
        assert_eq!(got, ["m_a", "m_g"]);
        assert_eq!(count_matches(&c, "\"loader\"*", &scoped).unwrap(), 2);
        let all = Filters {
            include_all_repos: true,
            include_history: true,
            ..scoped.clone()
        };
        assert_eq!(count_matches(&c, "\"loader\"*", &all).unwrap(), 4);
        assert_eq!(count_scope(&c, &scoped).unwrap(), 2);
        assert_eq!(count_matches(&c, "", &all).unwrap(), 0);
        let err = search(&c, "loader", &scoped, &opts(&b), 0).unwrap_err();
        assert_eq!(err.python_text(), "ValueError: --limit must be at least 1");
    }

    #[test]
    fn a_single_word_query_is_a_ref_and_refs_match_by_token() {
        let c = store();
        put(
            &c,
            "m_1",
            "r",
            "unrelated",
            "body",
            &[("refs", "PROJ-12 x.py")],
        );
        put(&c, "m_2", "r", "unrelated", "body", &[("refs", "PROJ-123")]);
        let b = bias();
        let f = Filters::default();
        // LIKE finds both, the token check keeps only the exact ref, folded
        let refs = vec!["Proj-12".to_string(), String::new()];
        let q = CandidateQuery {
            query_refs: &refs,
            ..Default::default()
        };
        let got: Vec<String> = search_candidates(&c, &q, &f)
            .unwrap()
            .into_iter()
            .map(|c| c.item.id)
            .collect();
        assert_eq!(got, ["m_1"]);
        // the FTS terms "proj"* OR "12" match both, and the ref hit ranks first
        assert_eq!(
            ids(&search(&c, "proj-12", &f, &opts(&b), 5).unwrap()),
            ["m_1", "m_2"]
        );
        assert_eq!(
            ids(&search(&c, "  ", &f, &opts(&b), 5).unwrap()),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn a_stand_in_reaching_sqlite_raises_unicode_encode_error() {
        let c = store();
        let b = bias();
        let q = format!("lo{}ader", char::from_u32(0x10FDFF).unwrap());
        let err = search(&c, &q, &Filters::default(), &opts(&b), 5).unwrap_err();
        // the LIKE param `%lo\udcffader%`: position 3
        assert_eq!(
            err.python_text(),
            "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcff' in position 3: surrogates not allowed"
        );
        // with a space the query is no ref, and \udcff is no word char, so nothing reaches sqlite
        assert!(
            search(
                &c,
                &format!("a b{}", char::from_u32(0x10FDFF).unwrap()),
                &Filters::default(),
                &opts(&b),
                5
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn pool_cap_and_by_date_order() {
        let c = store();
        for i in 0..55 {
            put(
                &c,
                &format!("m_{i:02}"),
                "r",
                "loader",
                "x",
                &[(
                    "as_of",
                    &format!("2026-01-{:02}T00:00:00+00:00", i % 28 + 1),
                )],
            );
        }
        let b = bias();
        let f = Filters::default();
        let (scored, cap) = search_scored(&c, "loader", &f, &opts(&b), Some(0)).unwrap();
        assert_eq!((scored.len(), cap), (50, Some(50)));
        let biased = vec!["decided".to_string()];
        let (scored, cap) = search_scored(&c, "loader", &f, &opts(&biased), Some(0)).unwrap();
        assert_eq!((scored.len(), cap), (55, None));
        // by date: newest first, ties by id descending; an offset counts in the parsed time
        put(
            &c,
            "m_z",
            "r",
            "loader",
            "x",
            &[("as_of", "2026-01-28T01:00:00+02:00")],
        );
        let q = CandidateQuery {
            fold_fts_query: "\"loader\"*",
            candidate_limit: 3,
            by_date: true,
            ..Default::default()
        };
        let got: Vec<String> = search_candidates(&c, &q, &f)
            .unwrap()
            .into_iter()
            .map(|c| c.item.id)
            .collect();
        // the newest 3 by parsed time: m_27 (01-28 00:00Z), m_z (01-27 23:00Z), then of
        // the two 01-27 00:00Z items the larger id, m_54; candidates come back in rowid order
        assert_eq!(got, ["m_27", "m_54", "m_z"]);
        let (pool, cap) = search_pool(&c, "loader", &f, &opts(&b)).unwrap();
        assert_eq!((pool.len(), cap), (50, Some(50)));
    }

    #[test]
    fn ids_matching_and_head() {
        let c = store();
        put(&c, "m_1", "r", "alpha", "x", &[]);
        put(&c, "m_2", "r", "beta", "x", &[]);
        let ids: Vec<String> = vec!["m_1".into(), "m_2".into()];
        assert_eq!(
            ids_matching(&c, "\"alpha\"", &ids).unwrap(),
            HashSet::from(["m_1".to_string()])
        );
        assert!(ids_matching(&c, "", &ids).unwrap().is_empty());
        assert!(ids_matching(&c, "\"alpha\"", &[]).unwrap().is_empty());
        assert_eq!(head(vec![1, 2, 3], -1), [1, 2]);
        assert_eq!(head(vec![1, 2, 3], -5), Vec::<i32>::new());
        assert_eq!(head(vec![1, 2, 3], 9), [1, 2, 3]);
    }

    // ---- lookup tests (put_item's insert as plain SQL)

    fn put_py(conn: &Connection, id: &str, body: &str, repo: &str, tag: &str, kind: &str) {
        let title: String = body.chars().take(40).collect();
        conn.execute(
            "INSERT INTO items (id, repo, kind, title, body, tag, evidence, as_of, content_hash) VALUES (?, ?, ?, ?, ?, ?, 'made-up check', '2026-10-25T10:00:00+00:00', ?)",
            [id, repo, kind, &title, body, tag, id],
        )
        .unwrap();
    }

    #[test]
    fn a_tag_bias_widens_the_candidate_pool_to_100() {
        // 70 strong "nebula" items fill the default pool of 50; the executed item holds the word
        // once in a long body, so only a pool of 100 reaches it
        let c = store();
        for i in 0..70 {
            put_py(
                &c,
                &format!("m_s{i:02}"),
                &format!("Nebula nebula nebula {i}"),
                "a",
                "derived",
                "fact",
            );
        }
        let long = format!(
            "nebula {}",
            (0..200)
                .map(|n| format!("filler{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        put_py(&c, "m_exec", &long, "a", "executed", "fact");
        let f = Filters {
            repo: Some("a".into()),
            ..Default::default()
        };
        let b = bias();
        let (default, cap) = search_scored(&c, "nebula", &f, &opts(&b), None).unwrap();
        assert_eq!(
            (
                default.len(),
                default.iter().any(|s| s.item.id == "m_exec"),
                cap
            ),
            (50, false, Some(50))
        );
        let exec = vec!["executed".to_string()];
        let (biased, cap) = search_scored(&c, "nebula", &f, &opts(&exec), None).unwrap();
        assert_eq!(
            (
                biased.len(),
                biased.iter().any(|s| s.item.id == "m_exec"),
                cap
            ),
            (71, true, None)
        );
    }

    #[test]
    fn kind_filter_keeps_only_the_kinds_named() {
        let c = store();
        put_py(&c, "m_f", "Quartz fact", "a", "derived", "fact");
        put_py(&c, "m_d", "Quartz decision", "a", "derived", "decision");
        put_py(&c, "m_t", "Quartz task", "a", "derived", "task");
        let b = bias();
        let f = Filters {
            repo: Some("a".into()),
            kinds_filter: Some(vec!["decision".into(), "task".into()]),
            ..Default::default()
        };
        let mut got = ids(&search(&c, "quartz", &f, &opts(&b), 10).unwrap())
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        got.sort();
        assert_eq!(got, ["m_d", "m_t"]);
        let f = Filters {
            repo: Some("a".into()),
            kinds_filter: Some(vec!["learning".into()]),
            ..Default::default()
        };
        assert!(search(&c, "quartz", &f, &opts(&b), 4).unwrap().is_empty());
    }

    #[test]
    fn count_matches_in_scope() {
        let c = store();
        for (id, body, repo, kind) in [
            ("m_a1", "Zephyr one", "a", "fact"),
            ("m_a2", "Zephyr two", "a", "decision"),
            ("m_g", "Zephyr global", "global", "fact"),
            ("m_b1", "Zephyr bee one", "b", "fact"),
            ("m_b2", "Zephyr bee two", "b", "fact"),
            ("m_c", "Zephyr sea", "c", "fact"),
        ] {
            put_py(&c, id, body, repo, "derived", kind);
        }
        let a = Filters {
            repo: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(count_matches(&c, "\"zephyr\"*", &a).unwrap(), 3);
        let ad = Filters {
            kinds_filter: Some(vec!["decision".into()]),
            ..a.clone()
        };
        assert_eq!(count_matches(&c, "\"zephyr\"*", &ad).unwrap(), 1);
        let all = Filters {
            include_all_repos: true,
            ..Default::default()
        };
        assert_eq!(count_matches(&c, "\"zephyr\"*", &all).unwrap(), 6);
        assert_eq!(count_matches(&c, "", &a).unwrap(), 0);
    }

    #[test]
    fn a_scope_keeps_the_items_any_matcher_matches_and_an_empty_one_keeps_none() {
        let c = store();
        for (id, body, repo) in [
            ("m_a1", "Zephyr GLX", "a"),
            ("m_b1", "Zephyr", "b"),
            ("m_c1", "Zephyr GLXY", "c"),
        ] {
            put_py(&c, id, body, repo, "derived", "fact");
        }
        let scoped = |m: &[&str]| {
            let f = Filters {
                scope: Some(m.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            };
            count_scope(&c, &f).unwrap()
        };
        assert_eq!(
            (
                scoped(&["GLX"]),
                scoped(&["repo:b"]),
                scoped(&["GLX", "repo:b"]),
                scoped(&[])
            ),
            (1, 1, 2, 0)
        );
    }

    #[test]
    fn search_pool_with_recent_keeps_only_what_the_ranking_keeps() {
        // the index splits "stg_google_ads" at the underscores, so both match the AND query; the
        // recent ranking keeps only items that hold every word as a word, so the pool must rank
        let c = store();
        put_py(
            &c,
            "m_joined",
            "QRX stg_google_ads model",
            "b",
            "derived",
            "fact",
        );
        put_py(
            &c,
            "m_exact",
            "QRX Google Ads report",
            "b",
            "derived",
            "fact",
        );
        let b = bias();
        let f = Filters {
            repo: Some("a".into()),
            include_all_repos: true,
            ..Default::default()
        };
        let o = SearchOpts {
            recent: true,
            aliases: None,
            bias: &b,
            scope_biases: &[],
        };
        let (ranked, _) = search_scored(&c, "QRX Google Ads", &f, &o, None).unwrap();
        let (pool, cap) = search_pool(&c, "QRX Google Ads", &f, &o).unwrap();
        let ranked: Vec<&str> = ranked.iter().map(|s| s.item.id.as_str()).collect();
        assert_eq!(
            (ranked, ids(&pool), cap),
            (vec!["m_exact"], vec!["m_exact"], None)
        );
    }

    #[test]
    fn query_refs_that_are_all_empty_leave_an_empty_like_list() {
        // Python's `{fold(r) for r in query_refs if r}` is empty here, so the SQL has `WHERE ()`
        let c = store();
        let refs = vec![String::new()];
        let q = CandidateQuery {
            query_refs: &refs,
            ..Default::default()
        };
        let err = search_candidates(&c, &q, &Filters::default()).unwrap_err();
        assert_eq!(
            err.python_text(),
            "OperationalError: near \")\": syntax error"
        );
    }

    #[test]
    fn recent_falls_back_to_or_and_its_cap_ignores_the_bias() {
        let c = store();
        for i in 0..55 {
            put(&c, &format!("m_{i:02}"), "r", "loader", "x", &[]);
        }
        let decided = vec!["decided".to_string()];
        let o = SearchOpts {
            recent: true,
            aliases: None,
            bias: &decided,
            scope_biases: &[],
        };
        // "loader zzz" has no AND match, so the OR search runs, by date, capped at 50 even with a tag bias
        let (scored, cap) = search_scored(&c, "loader zzz", &Filters::default(), &o, None).unwrap();
        assert_eq!((scored.len(), cap), (50, Some(50)));
    }

    #[test]
    fn a_full_pool_with_ref_hits_past_the_cap_is_ranked_and_cut() {
        // 55 items hold "loader" as text and as a ref: the FTS caps at 50, the ref scan adds all 55
        let c = store();
        for i in 0..55 {
            put(
                &c,
                &format!("m_{i:02}"),
                "r",
                "loader",
                "x",
                &[("refs", "loader")],
            );
        }
        let b = bias();
        let (pool, cap) = search_pool(&c, "loader", &Filters::default(), &opts(&b)).unwrap();
        assert_eq!((pool.len(), cap), (50, Some(50)));
    }

    #[test]
    fn extra_scope_is_additive_and_emits_repo_matchers_before_sm_word() {
        let f = Filters {
            repo: Some("anvil".into()),
            extra_scope: Some(vec!["GLX".into(), "repo:globex-web".into(), "INI".into()]),
            ..Default::default()
        };
        let (sql, p) = build_item_filters("i", &f);
        assert_eq!(
            sql,
            "i.status = 'active' AND (i.repo IN (?, 'global', 'unassigned') OR \
             (i.repo = ? OR sm_word(?, i.title, i.body, i.refs, i.tags) OR sm_word(?, i.title, i.body, i.refs, i.tags)))"
        );
        assert_eq!(
            p,
            vec![text("anvil"), text("globex-web"), text("GLX"), text("INI")]
        );
        // Empty extra_scope emits plain repo clause without an empty OR ().
        let empty_extra = Filters {
            repo: Some("anvil".into()),
            extra_scope: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &empty_extra),
            (
                "i.status = 'active' AND i.repo IN (?, 'global', 'unassigned')".to_string(),
                vec![text("anvil")]
            )
        );
        // scope also puts repo = ? before sm_word(...).
        let scoped = Filters {
            scope: Some(vec!["GLX".into(), "repo:globex-web".into()]),
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &scoped),
            (
                "i.status = 'active' AND (i.repo = ? OR sm_word(?, i.title, i.body, i.refs, i.tags))"
                    .to_string(),
                vec![text("globex-web"), text("GLX")]
            )
        );

        let c = store();
        put(&c, "m_cwd", "anvil", "kiwi cwd", "cwd body", &[]);
        put(&c, "m_glob", "global", "kiwi global", "global body", &[]);
        put(&c, "m_web", "globex-web", "kiwi web", "web body", &[]);
        put(&c, "m_word", "misc", "kiwi GLX", "GLX body", &[]);
        put(&c, "m_out", "other", "kiwi other", "other body", &[]);
        put(
            &c,
            "m_old",
            "globex-web",
            "kiwi old",
            "old body",
            &[("status", "superseded")],
        );
        let b = bias();
        let mut got: Vec<String> = search(&c, "kiwi", &f, &opts(&b), 10)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        got.sort();
        assert_eq!(got, ["m_cwd", "m_glob", "m_web", "m_word"]);
    }

    #[test]
    fn source_prefix_bounds_handles_unicode_and_surrogate_stand_in_boundaries() {
        assert_eq!(
            source_prefix_bounds("task:"),
            ("task:".into(), Some("task;".into()))
        );
        // UTF-16 surrogate gap 0xD800..=0xDFFF skipped to 0xE000.
        assert_eq!(
            source_prefix_bounds("\u{D7FF}"),
            ("\u{D7FF}".into(), Some("\u{E000}".into()))
        );
        // py::str stand-in surrogate range 0x10FD80..=0x10FDFF skipped to 0x10FE00.
        assert_eq!(
            source_prefix_bounds("\u{10FD7F}"),
            ("\u{10FD7F}".into(), Some("\u{10FE00}".into()))
        );
        // Carry from U+10FFFF into previous character; all-max has no upper bound.
        assert_eq!(
            source_prefix_bounds("a\u{10FFFF}"),
            ("a\u{10FFFF}".into(), Some("b".into()))
        );
        assert_eq!(
            source_prefix_bounds("\u{10FFFF}"),
            ("\u{10FFFF}".into(), None)
        );
    }

    #[test]
    fn wave2_filters_sql_and_list_items_semantics() {
        // exact_repo alone and with extra_scope.
        let exact = Filters {
            repo: Some("anvil".into()),
            exact_repo: true,
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &exact),
            (
                "i.status = 'active' AND i.repo = ?".to_string(),
                vec![text("anvil")]
            )
        );
        let exact_extra = Filters {
            repo: Some("anvil".into()),
            exact_repo: true,
            extra_scope: Some(vec!["repo:ops".into()]),
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &exact_extra),
            (
                "i.status = 'active' AND (i.repo = ? OR (i.repo = ?))".to_string(),
                vec![text("anvil"), text("ops")]
            )
        );

        // Empty-only sources_filter emits no clause (never AND ()).
        let empty_src = Filters {
            sources_filter: Some(vec!["".into(), "".into()]),
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &empty_src),
            ("i.status = 'active'".to_string(), vec![])
        );

        // StatusFilter variants + since/until exclusive vs inclusive.
        let sup_f = Filters {
            status_filter: Some(StatusFilter::Superseded),
            since_epoch: Some(100.0),
            until_epoch: Some(200.0),
            until_exclusive: true,
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &sup_f),
            (
                "(i.status = 'superseded' AND i.superseded_by IS NOT NULL AND i.superseded_by != '') AND sm_epoch(i.as_of) >= ? AND sm_epoch(i.as_of) < ?".to_string(),
                vec![Sql::Real(100.0), Sql::Real(200.0)]
            )
        );
        let ret_f = Filters {
            status_filter: Some(StatusFilter::Retired),
            until_epoch: Some(200.0),
            until_exclusive: false,
            ..Default::default()
        };
        assert_eq!(
            build_item_filters("i", &ret_f),
            (
                "(i.status = 'superseded' AND (i.superseded_by IS NULL OR i.superseded_by = '')) AND sm_epoch(i.as_of) <= ?".to_string(),
                vec![Sql::Real(200.0)]
            )
        );
        let all_f = Filters {
            status_filter: Some(StatusFilter::All),
            ..Default::default()
        };
        assert_eq!(build_item_filters("i", &all_f), ("1=1".to_string(), vec![]));

        // list_items ordering, status separation, validation, and NULLS LAST.
        let c = store();
        put(
            &c,
            "m_1",
            "anvil",
            "t1",
            "b1",
            &[("as_of", "2026-01-02T01:00:00+00:00"), ("source", "task:1")],
        );
        put(
            &c,
            "m_2",
            "anvil",
            "t2",
            "b2",
            &[
                ("as_of", "2026-01-01T23:30:00-04:00"), // 2026-01-02T03:30:00Z -> newest
                ("source", "agent:a"),
            ],
        );
        put(
            &c,
            "m_bad",
            "anvil",
            "tbad",
            "bbad",
            &[("as_of", "not-a-date"), ("source", "task:2")],
        );
        put(&c, "m_glob", "global", "tg", "bg", &[]);
        put(
            &c,
            "m_sup",
            "anvil",
            "tsup",
            "bsup",
            &[("status", "superseded"), ("superseded_by", "m_1")],
        );
        put(
            &c,
            "m_ret",
            "anvil",
            "tret",
            "bret",
            &[("status", "superseded")],
        );

        let err_lim = list_items(&c, &exact, 0, 0).unwrap_err();
        assert_eq!(
            err_lim.python_text(),
            "ValueError: --limit must be at least 1"
        );
        let err_off = list_items(&c, &exact, 10, -1).unwrap_err();
        assert_eq!(
            err_off.python_text(),
            "ValueError: --offset must be at least 0"
        );

        // exact_repo excludes 'global', orders newest epoch first, NULLS LAST.
        let page = list_items(&c, &exact, 10, 0).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(ids(&page.items), ["m_2", "m_1", "m_bad"]);

        let sup_page = list_items(
            &c,
            &Filters {
                repo: Some("anvil".into()),
                exact_repo: true,
                status_filter: Some(StatusFilter::Superseded),
                ..Default::default()
            },
            10,
            0,
        )
        .unwrap();
        assert_eq!(ids(&sup_page.items), ["m_sup"]);

        let ret_page = list_items(
            &c,
            &Filters {
                repo: Some("anvil".into()),
                exact_repo: true,
                status_filter: Some(StatusFilter::Retired),
                ..Default::default()
            },
            10,
            0,
        )
        .unwrap();
        assert_eq!(ids(&ret_page.items), ["m_ret"]);
    }

    #[test]
    fn recent_with_exclusion_falls_back_to_or_when_and_match_is_excluded() {
        let c = store();
        put(&c, "m_and_excl", "r", "alpha beta staging", "x", &[]);
        put(&c, "m_or_keep", "r", "alpha only", "x", &[]);
        let b = bias();
        let o = SearchOpts {
            recent: true,
            aliases: None,
            bias: &b,
            scope_biases: &[],
        };
        let got = search(&c, "alpha beta -staging", &Filters::default(), &o, 5).unwrap();
        assert_eq!(ids(&got), ["m_or_keep"]);
    }
}
