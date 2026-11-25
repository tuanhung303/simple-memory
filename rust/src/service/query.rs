use std::collections::{HashMap, HashSet};
use std::time::Instant;

use indexmap::IndexMap;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::cli::query::shlex_quote;
use crate::config::{
    aliases, get_db_path, load_config, resolve_repo_until, roots, scope_matchers, scopes,
};
use crate::core::fold::fold;
use crate::core::item::{Item, validate_date_range};
pub use crate::core::rank::parse_bias;
use crate::core::rank::{
    BIAS_SCOPE_PREFIX, DEFAULT_QUERY_LIMIT, Scored, is_hint_term, is_pure_relevance, match_snippet,
    parse_bias_list, parse_query_exclusions, parse_scope_bias_path, query_terms,
    term_alias_targets,
};
use crate::core::refs::{RefsArg, split_refs};
use crate::core::tags::{validate_kind, validate_tag};
use crate::core::terms::quote_fts_term;
use crate::error::SmError;
use crate::py;
use crate::py::value::PyValue;
use crate::service::recall::{HAIKU_FLOOR_MIN_SCOPE, is_stopword};
use crate::sources::value_get;
use crate::store::connect::connect;
use crate::store::search::{
    Filters, KeywordAliases, SearchOpts, count_matches, count_scope, head, head_cap, ids_matching,
    search, search_pool, search_scored,
};

pub const MAX_MISSING_TERMS: usize = 5;
pub const MAX_OTHER_REPOS: usize = 3;
pub const MAX_SHARED_REFS: usize = 3;
pub const LOCAL_MAX_TERMS: usize = 12;
/// An item from the local search is injected only when the idf, ln(scope items / df), of the
/// query terms it matches sums to at least LOCAL_TAU (lead decision 2026-11-04 06:22).
pub const LOCAL_TAU: f64 = 8.0;
pub const LOCAL_SEARCH_DEPTH: i64 = 20;
pub const JSON_SNIPPET_CHARS: usize = 300;

/// One `sm query` for the formatters: the shown hits in rank order plus the footer facts.
#[derive(Clone, Debug, PartialEq)]
pub struct Lookup {
    pub query: String,
    pub bias: Vec<String>,
    pub repo: Option<String>,
    pub all_repos: bool,
    /// The `--in` paths.
    pub scopes: Vec<String>,
    pub limit: i64,
    pub hits: Vec<Scored>,
    pub missing: Vec<(String, i64)>,
    pub beyond_limit: i64,
    pub pool_cap: Option<i64>,
    pub other_repos: IndexMap<String, i64>,
    pub other_repos_cap: Option<i64>,
    pub shared_refs: Vec<(String, i64)>,
    /// Per missing term with no item in scope: the items its status, a filter or the repo
    /// scope hid.
    pub hidden: IndexMap<String, Hidden>,
}

/// Items that hold a missing term but that the query's status, filters or repo scope hid.
#[derive(Clone, Debug, PartialEq)]
pub struct Hidden {
    pub n: i64,
    /// Retired or superseded: `--include-history` shows them.
    pub retired: bool,
    /// Outside `--tag`, `--kind`, `--source`, `--since` or `--until`.
    pub filtered: bool,
    /// In other repos: `--all` shows them (with `retired` and `filtered` as above).
    pub other_repos: bool,
    /// What hid them, as the footer says it: `retired or superseded`, `outside --kind fact`.
    pub by: String,
}

/// A `Sequence[str] | str | None` flag value.
#[derive(Clone, Copy, Debug, Default)]
pub enum StrList<'a> {
    #[default]
    None,
    Str(&'a str),
    List(&'a [String]),
}

/// A str splits on ","; each part stripped, empty parts dropped.
pub fn split_list(values: StrList) -> Vec<String> {
    let parts: Vec<&str> = match values {
        StrList::None => Vec::new(),
        StrList::Str(s) => s.split(',').collect(),
        StrList::List(v) => v.iter().map(String::as_str).collect(),
    };
    parts
        .into_iter()
        .map(py::str::strip)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// The keyword arguments of `run_query` and `run_lookup`.
#[derive(Clone, Copy, Debug)]
pub struct QueryArgs<'a> {
    pub limit: i64,
    pub recent: bool,
    pub tags: StrList<'a>,
    pub kinds: StrList<'a>,
    pub sources: StrList<'a>,
    pub since: &'a str,
    pub until: &'a str,
    pub bias: StrList<'a>,
    pub all_repos: bool,
    pub repo: &'a str,
    pub include_history: bool,
    /// `--in`: `[scopes]` paths; any path searches every repo.
    pub scopes: StrList<'a>,
    /// Additive `[scopes]` paths ORed with the resolved repo when `scopes` is empty and
    /// `all_repos` is false (prompt hook routing).
    pub extra_scopes: StrList<'a>,
    pub home: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

impl Default for QueryArgs<'_> {
    fn default() -> Self {
        QueryArgs {
            limit: DEFAULT_QUERY_LIMIT,
            recent: false,
            tags: StrList::None,
            kinds: StrList::None,
            sources: StrList::None,
            since: "",
            until: "",
            bias: StrList::None,
            all_repos: false,
            repo: "",
            include_history: false,
            scopes: StrList::None,
            extra_scopes: StrList::None,
            home: None,
            cwd: None,
        }
    }
}

/// `cfg.get("keyword_aliases")` in the form `build_fts_query` takes: each target as the value
/// tomllib gave, so a target Python cannot iterate raises its TypeError there.
pub fn keyword_aliases(cfg: &PyValue) -> Option<KeywordAliases> {
    let PyValue::Dict(pairs) = value_get(cfg, "keyword_aliases")? else {
        return None;
    };
    Some(pairs.iter().map(|(k, v)| (k.py_str(), v.clone())).collect())
}

/// `[query] default_bias`: override the built-in default bias list from config.toml.
pub fn default_bias_from_cfg(cfg: &PyValue) -> Result<Vec<String>, SmError> {
    let Some(query_val) = value_get(cfg, "query") else {
        return parse_bias_list(None);
    };
    let PyValue::Dict(_) = query_val else {
        return Err(SmError::value("[query]: expected a table"));
    };
    let Some(raw) = value_get(query_val, "default_bias") else {
        return parse_bias_list(None);
    };
    match raw {
        PyValue::Str(v) => {
            parse_bias(v).map_err(|e| SmError::value(format!("[query] default_bias: {}", e.msg)))
        }
        PyValue::List(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for item in items {
                let PyValue::Str(s) = item else {
                    return Err(SmError::value(
                        "[query] default_bias: expected a list of strings",
                    ));
                };
                vals.push(s.clone());
            }
            parse_bias_list(Some(&vals))
                .map_err(|e| SmError::value(format!("[query] default_bias: {}", e.msg)))
        }
        _ => Err(SmError::value(
            "[query] default_bias: expected a list of strings",
        )),
    }
}

pub type ScopeBiasList = Vec<(String, Vec<String>)>;

/// Resolve `scope:<path>` entries from `bias_list` plus automatic hook `extra_scope_paths`
/// (suppressed when `bias_list` is pure `["relevance"]`) into `(label, matchers)` pairs.
/// Skips reading `[scopes]` when no scope paths are active.
pub fn resolve_scope_biases(
    cfg: &PyValue,
    bias_list: &[String],
    extra_scope_paths: &[String],
) -> Result<ScopeBiasList, SmError> {
    let mut paths: Vec<&str> = Vec::new();
    for b in bias_list {
        if let Some(p) = parse_scope_bias_path(b)
            && !paths.contains(&p)
        {
            paths.push(p);
        }
    }
    if !extra_scope_paths.is_empty() && !is_pure_relevance(bias_list) {
        for p in extra_scope_paths {
            let p = p.as_str();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let all_scopes = scopes(cfg)?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let matchers = scope_matchers(&all_scopes, &[path])?;
        out.push((format!("{BIAS_SCOPE_PREFIX}{path}"), matchers));
    }
    Ok(out)
}

/// `_prepare`: (store filters, bias, scope biases, config) after validating every flag, in Python's order.
fn prepare(
    a: &QueryArgs,
    deadline: Option<Instant>,
) -> Result<(Filters, Vec<String>, ScopeBiasList, PyValue), SmError> {
    if a.limit <= 0 {
        return Err(SmError::value("--limit must be at least 1"));
    }
    let tag_list = split_list(a.tags)
        .iter()
        .map(|t| validate_tag(Some(t), Some("query-filter"), true))
        .collect::<Result<Vec<_>, _>>()?;
    let kind_list = split_list(a.kinds)
        .iter()
        .map(|k| validate_kind(Some(k)))
        .collect::<Result<Vec<_>, _>>()?;
    let source_list = split_list(a.sources);
    let (since_epoch, until_epoch, until_exclusive) =
        validate_date_range(Some(a.since), Some(a.until))?;
    let explicit_bias = match a.bias {
        StrList::Str(s) => {
            let parsed = parse_bias(s)?;
            (!split_list(StrList::Str(s)).is_empty()).then_some(parsed)
        }
        StrList::List(v) => {
            let parsed = parse_bias_list(Some(v))?;
            (!v.iter().all(|x| py::str::strip(x).is_empty())).then_some(parsed)
        }
        StrList::None => None,
    };
    let cfg = load_config(a.home)?;
    let bias_list = match explicit_bias {
        Some(b) if !b.is_empty() && b.iter().all(|t| parse_scope_bias_path(t).is_some()) => {
            let mut merged = default_bias_from_cfg(&cfg)?;
            for tok in b {
                if !merged.contains(&tok) {
                    merged.push(tok);
                }
            }
            merged
        }
        Some(b) => b,
        None => default_bias_from_cfg(&cfg)?,
    };
    let in_paths = split_list(a.scopes);
    let scope = if in_paths.is_empty() {
        None
    } else {
        Some(scope_matchers(&scopes(&cfg)?, &in_paths)?)
    };
    let all_repos = a.all_repos || scope.is_some();
    let extra_paths = split_list(a.extra_scopes);
    let extra_scope = if scope.is_none() && !all_repos && !extra_paths.is_empty() {
        Some(scope_matchers(&scopes(&cfg)?, &extra_paths)?)
    } else {
        None
    };
    let extra_paths_used: &[String] = if extra_scope.is_some() {
        &extra_paths
    } else {
        &[]
    };
    let scope_biases = resolve_scope_biases(&cfg, &bias_list, extra_paths_used)?;
    let repo = py::str::strip(a.repo);
    let resolved = if !repo.is_empty() {
        Some(repo.to_string())
    } else if all_repos {
        None
    } else {
        Some(resolve_repo_until(
            a.cwd,
            &aliases(&cfg),
            &roots(&cfg)?,
            deadline,
        )?)
    };
    let filters = Filters {
        repo: resolved,
        include_all_repos: all_repos,
        tags_filter: (!tag_list.is_empty()).then_some(tag_list),
        kinds_filter: (!kind_list.is_empty()).then_some(kind_list),
        include_history: false,
        scope,
        extra_scope,
        sources_filter: (!source_list.is_empty()).then_some(source_list),
        since_epoch,
        until_epoch,
        until_exclusive,
        ..Default::default()
    };
    Ok((filters, bias_list, scope_biases, cfg))
}

fn open(home: Option<&str>, deadline: Option<Instant>) -> Result<Connection, SmError> {
    connect(get_db_path(home)?.as_str(), deadline)
}

/// `run_query`: `deadline` bounds the store's lock waits (the prompt hook).
pub fn run_query(
    query_text: &str,
    a: &QueryArgs,
    deadline: Option<Instant>,
) -> Result<Vec<Item>, SmError> {
    let (filters, bias_list, scope_biases, cfg) = prepare(a, deadline)?;
    let conn = open(a.home, deadline)?;
    let ka = keyword_aliases(&cfg);
    let f = Filters {
        include_history: a.include_history,
        ..filters
    };
    search(
        &conn,
        query_text,
        &f,
        &SearchOpts {
            recent: a.recent,
            aliases: ka.as_ref(),
            bias: &bias_list,
            scope_biases: &scope_biases,
        },
        a.limit,
    )
}

/// `scope_df`: per folded term, the items in scope that hold it by the search's FTS rule.
pub fn scope_df<S: AsRef<str>>(
    conn: &Connection,
    terms: &[S],
    f: &Filters,
) -> Result<IndexMap<String, i64>, SmError> {
    let mut out = IndexMap::new();
    for t in terms {
        let t = t.as_ref();
        if !out.contains_key(t) {
            out.insert(t.to_string(), count_matches(conn, &quote_fts_term(t), f)?);
        }
    }
    Ok(out)
}

pub(crate) fn prefix_overlaps(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 4 && short.bytes().all(|b| b.is_ascii_alphabetic()) && long.starts_with(short)
}

/// `pick_local_units`: the codes the store holds, then up to LOCAL_MAX_TERMS rarest held terms
/// sorted by ascending document frequency, skipping terms in or prefix-overlapping a kept unit.
pub fn pick_local_units<S: AsRef<str>>(
    terms: &[S],
    codes: &[S],
    df: &IndexMap<String, i64>,
) -> Vec<String> {
    let held = |k: &str| df.get(k).copied().unwrap_or(0) > 0;
    let kept_codes: Vec<String> = codes
        .iter()
        .map(|c| c.as_ref().to_string())
        .filter(|c| held(&fold(Some(c))))
        .collect();
    let taken: Vec<String> = kept_codes
        .iter()
        .flat_map(|c| {
            py::str::split(&fold(Some(c)))
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect();
    let mut indexed: Vec<(usize, &str)> = terms
        .iter()
        .map(AsRef::as_ref)
        .enumerate()
        .filter(|(_, t)| held(t) && !taken.iter().any(|k| k == t || prefix_overlaps(k, t)))
        .collect();
    indexed.sort_by_key(|(idx, t)| (df.get(*t).copied().unwrap_or(i64::MAX), *idx));
    let mut kept: Vec<(usize, &str)> = Vec::with_capacity(LOCAL_MAX_TERMS);
    for (idx, t) in indexed {
        if kept.len() >= LOCAL_MAX_TERMS {
            break;
        }
        if !kept.iter().any(|(_, k)| *k == t || prefix_overlaps(k, t)) {
            kept.push((idx, t));
        }
    }
    kept.sort_by_key(|(idx, _)| *idx);
    kept_codes
        .into_iter()
        .chain(kept.into_iter().map(|(_, t)| t.to_string()))
        .collect()
}

/// `_over_floor`: the items, in order, whose matched units' idf sums to at least LOCAL_TAU.
#[cfg(test)]
fn over_floor(
    conn: &Connection,
    items: Vec<Item>,
    units: &[String],
    df: &IndexMap<String, i64>,
    n_scope: i64,
) -> Result<Vec<Item>, SmError> {
    Ok(over_floor_at(conn, items, units, df, n_scope, LOCAL_TAU)?.0)
}

/// `_over_floor` with the floor as an argument (the Python tests patch LOCAL_TAU), and the best
/// item's idf sum.
fn over_floor_at(
    conn: &Connection,
    items: Vec<Item>,
    units: &[String],
    df: &IndexMap<String, i64>,
    n_scope: i64,
    tau: f64,
) -> Result<(Vec<Item>, f64), SmError> {
    let ids: Vec<String> = items.iter().map(|it| it.id.clone()).collect();
    let mut score: HashMap<String, f64> = HashMap::new();
    for unit in units {
        let key = fold(Some(unit));
        for item_id in ids_matching(conn, &quote_fts_term(&key), &ids)? {
            let d = *df.get(&key).ok_or_else(|| SmError::key(&key))?;
            let ratio = n_scope as f64 / d as f64;
            if ratio <= 0.0 {
                return Err(SmError::value(format!(
                    "expected a positive input, got {}",
                    py::float::repr(ratio)
                )));
            }
            *score.entry(item_id).or_insert(0.0) += ratio.ln();
        }
    }
    let best = score.values().copied().fold(0.0, f64::max);
    Ok((
        items
            .into_iter()
            .filter(|it| score.get(&it.id).copied().unwrap_or(0.0) >= tau)
            .collect(),
        best,
    ))
}

/// Parse and validate `[prompt_hook] local_min_idf`: default `LOCAL_TAU` when missing or None;
/// rejects booleans, non-finite floats, negative numbers, and non-numeric values.
pub fn local_min_idf_from_cfg(cfg: &PyValue) -> Result<f64, SmError> {
    const ERR: &str = "[prompt_hook] local_min_idf must be a finite number >= 0";
    let Some(hook_cfg) = value_get(cfg, "prompt_hook") else {
        return Ok(LOCAL_TAU);
    };
    let Some(val) = value_get(hook_cfg, "local_min_idf") else {
        return Ok(LOCAL_TAU);
    };
    match val {
        PyValue::None => Ok(LOCAL_TAU),
        PyValue::Bool(_) => Err(SmError::value(ERR)),
        PyValue::Float(x) if x.is_finite() && *x >= 0.0 => Ok(*x),
        PyValue::Int(s) => match s.parse::<f64>() {
            Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
            _ => Err(SmError::value(ERR)),
        },
        PyValue::Str(s) => match py::float::from_str(s) {
            Ok(x) if x.is_finite() && x >= 0.0 => Ok(x),
            _ => Err(SmError::value(ERR)),
        },
        _ => Err(SmError::value(ERR)),
    }
}

/// The local search found items but the idf floor dropped every one: what it searched, the floor
/// and the best item's idf sum, for hook.log.
#[derive(Clone, Debug, PartialEq)]
pub struct BelowFloor {
    pub query: String,
    pub min_idf: f64,
    pub best: f64,
}

/// (the local query, its items, why the floor left none).
pub type LocalFound = (String, Vec<Item>, Option<BelowFloor>);

/// `run_local_query`: (the local query, its top `limit + extra` items over the idf floor) on one
/// connection with the hook's deadline and `run_query`'s scope; ("", []) when none clears it. The
/// floor applies only from HAIKU_FLOOR_MIN_SCOPE active items in scope, as the model's does.
#[allow(clippy::too_many_arguments)]
pub fn run_local_query<S: AsRef<str>>(
    terms: &[S],
    codes: &[S],
    limit: i64,
    extra: i64,
    repo: &str,
    extra_scopes: StrList<'_>,
    home: Option<&str>,
    cwd: Option<&str>,
    deadline: Option<Instant>,
) -> Result<LocalFound, SmError> {
    let a = QueryArgs {
        limit,
        repo,
        extra_scopes,
        home,
        cwd,
        ..Default::default()
    };
    local_query_at(terms, codes, extra, &a, deadline, None)
}

fn local_query_at<S: AsRef<str>>(
    terms: &[S],
    codes: &[S],
    extra: i64,
    a: &QueryArgs,
    deadline: Option<Instant>,
    tau_override: Option<f64>,
) -> Result<LocalFound, SmError> {
    let (limit, home) = (a.limit, a.home);
    let (filters, bias_list, scope_biases, cfg) = prepare(a, deadline)?;
    let tau = match tau_override {
        Some(t) => t,
        None => local_min_idf_from_cfg(&cfg)?,
    };
    let conn = open(home, deadline)?;
    let mut all: Vec<String> = terms.iter().map(|t| t.as_ref().to_string()).collect();
    all.extend(codes.iter().map(|c| fold(Some(c.as_ref()))));
    let df = scope_df(&conn, &all, &filters)?;
    let units = pick_local_units(terms, codes, &df);
    if units.is_empty() {
        return Ok((String::new(), Vec::new(), None));
    }
    let query = units.join(" ");
    let ka = keyword_aliases(&cfg);
    let o = SearchOpts {
        recent: false,
        aliases: ka.as_ref(),
        bias: &bias_list,
        scope_biases: &scope_biases,
    };
    let found = search(
        &conn,
        &query,
        &filters,
        &o,
        limit.max(LOCAL_SEARCH_DEPTH) + extra,
    )?;
    let n_scope = if tau == 0.0 || found.is_empty() {
        0
    } else {
        count_scope(&conn, &filters)?
    };
    if n_scope < HAIKU_FLOOR_MIN_SCOPE {
        let kept = head(found, limit + extra);
        return Ok(if kept.is_empty() {
            (String::new(), Vec::new(), None)
        } else {
            (query, kept, None)
        });
    }
    let (kept, best) = over_floor_at(&conn, found, &units, &df, n_scope, tau)?;
    Ok(if kept.is_empty() {
        let below = BelowFloor {
            query,
            min_idf: tau,
            best,
        };
        (String::new(), Vec::new(), Some(below))
    } else {
        (query, head(kept, limit + extra), None)
    })
}

/// The flags that leave items out, as a footer command carries them: `--tag`, `--kind`,
/// `--source`, `--since`, `--until`.
pub fn narrowing_flags(a: &QueryArgs) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let mut flags = Vec::new();
    for (flag, vals) in [("--tag", a.tags), ("--kind", a.kinds)] {
        let vals = split_list(vals);
        if !vals.is_empty() {
            flags.extend([s(flag), vals.join(",")]);
        }
    }
    let sources = split_list(a.sources);
    if !sources.is_empty() {
        flags.extend([s("--source"), shlex_quote(&sources.join(","))]);
    }
    for (flag, val) in [("--since", a.since), ("--until", a.until)] {
        if !val.is_empty() {
            flags.extend([s(flag), shlex_quote(val)]);
        }
    }
    flags
}

/// Drops the hits that matched only stopwords; (the pool size when the pool still counts as
/// full: more items in `f` match a topic word of `topic_fts` than the hits kept).
fn drop_filler_hits(
    conn: &Connection,
    scored: &mut Vec<Scored>,
    cap: Option<i64>,
    topic_fts: &str,
    f: &Filters,
) -> Result<Option<i64>, SmError> {
    let n = scored.len();
    scored.retain(|s| {
        !s.why.ref_hits.is_empty()
            || s.why.terms_matched.is_empty()
            || !s.why.terms_matched.iter().all(|t| is_stopword(t))
    });
    Ok(match cap {
        Some(_)
            if scored.len() < n && count_matches(conn, topic_fts, f)? <= scored.len() as i64 =>
        {
            None
        }
        _ => cap,
    })
}

/// `_shared_refs`: refs that 2 or more hits hold and the query does not name, most shared first.
pub fn shared_refs(hits: &[Scored], query_text: &str) -> Vec<(String, i64)> {
    let q_folded = fold(Some(query_text));
    let mut counts: IndexMap<String, i64> = IndexMap::new();
    for s in hits {
        let mut seen: Vec<String> = Vec::new();
        for r in split_refs(Some(&RefsArg::Str(&s.item.refs))) {
            // `fold(r) not in q_folded` is a substring test
            if !q_folded.contains(fold(Some(&r)).as_str()) && !seen.contains(&r) {
                seen.push(r);
            }
        }
        for r in seen {
            *counts.entry(r).or_insert(0) += 1;
        }
    }
    let mut shared: Vec<(String, i64)> = counts.into_iter().filter(|(_, n)| *n >= 2).collect();
    shared.sort_by_key(|(_, n)| -n);
    shared.truncate(MAX_SHARED_REFS);
    shared
}

/// `_other_repos`: (hits per repo outside the scope among the best matches across all repos,
/// top 3; the pool cap when the pool was full, so the counts are lower bounds). With
/// `topic_fts`, the hits that matched only stopwords do not count.
fn other_repos(
    conn: &Connection,
    query_text: &str,
    f: &Filters,
    o: &SearchOpts,
    topic_fts: Option<&str>,
) -> Result<(IndexMap<String, i64>, Option<i64>), SmError> {
    let all = Filters {
        include_all_repos: true,
        ..f.clone()
    };
    let (pool, cap) = match topic_fts {
        None => search_pool(conn, query_text, &all, o)?,
        Some(topic) => {
            let (mut scored, cap) = search_scored(conn, query_text, &all, o, None)?;
            let cap = drop_filler_hits(conn, &mut scored, cap, topic, &all)?;
            let items: Vec<Item> = scored.into_iter().map(|s| s.item).collect();
            (head_cap(items, cap), cap)
        }
    };
    let in_scope = |r: &str| f.repo.as_deref() == Some(r) || r == "global" || r == "unassigned";
    let mut counts: IndexMap<String, i64> = IndexMap::new();
    for item in pool.iter().filter(|it| !in_scope(&it.repo)) {
        *counts.entry(item.repo.clone()).or_insert(0) += 1;
    }
    let mut top: Vec<(String, i64)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(MAX_OTHER_REPOS);
    Ok((top.into_iter().collect(), cap))
}

/// `run_lookup`: `run_query` plus why per hit and the footer counts.
pub fn run_lookup(query_text: &str, a: &QueryArgs) -> Result<Lookup, SmError> {
    let (filters, bias_list, scope_biases, cfg) = prepare(a, None)?;
    let ka = keyword_aliases(&cfg);
    let o = SearchOpts {
        recent: a.recent,
        aliases: ka.as_ref(),
        bias: &bias_list,
        scope_biases: &scope_biases,
    };
    let f = Filters {
        include_history: a.include_history,
        ..filters
    };
    let conn = open(a.home, None)?;
    let (positive_query, _excluded) = parse_query_exclusions(query_text);
    let terms = query_terms(&positive_query);
    let has_content = terms.iter().any(|t| is_hint_term(t) && !is_stopword(t));
    // a hit made only of stopwords drops out when the query names a topic; every hit then needs
    // its terms, not only the first `limit`
    let drop_filler = has_content && terms.iter().any(|t| is_stopword(t));
    let explain = if drop_filler { None } else { Some(a.limit) };
    let alias_map = term_alias_targets(&positive_query, ka.as_ref())?;
    let term_fts = |t: &str| {
        let mut fts_parts = vec![quote_fts_term(t)];
        for w in alias_map.get(t).into_iter().flatten() {
            let q = quote_fts_term(w);
            if !fts_parts.contains(&q) {
                fts_parts.push(q);
            }
        }
        fts_parts.join(" OR ")
    };
    let topic_fts = drop_filler.then(|| {
        terms
            .iter()
            .filter(|t| is_hint_term(t) && !is_stopword(t))
            .map(|t| term_fts(t))
            .collect::<Vec<_>>()
            .join(" OR ")
    });
    let (mut scored, mut pool_cap) = search_scored(&conn, query_text, &f, &o, explain)?;
    if let Some(topic) = &topic_fts {
        pool_cap = drop_filler_hits(&conn, &mut scored, pool_cap, topic, &f)?;
    }
    let n_scored = scored.len() as i64;
    let hits = head(scored, a.limit);
    let full_folded = fold(Some(py::str::strip(query_text)));
    let pos_folded = fold(Some(py::str::strip(&positive_query)));
    let held: HashSet<&String> = hits
        .iter()
        .flat_map(|s| s.why.terms_matched.iter())
        .collect();
    let mut missing = Vec::new();
    for t in terms {
        if !held.contains(&t) && is_hint_term(&t) && !(has_content && is_stopword(&t)) {
            let fts = term_fts(&t);
            let n = count_matches(&conn, &fts, &f)?;
            if n > 0 && (t == full_folded || t == pos_folded) {
                continue;
            }
            missing.push((t, n, fts));
        }
    }
    missing.truncate(MAX_MISSING_TERMS);
    let other_scope = f.repo.as_deref().is_some_and(|r| !r.is_empty()) && !f.include_all_repos;
    let narrow = narrowing_flags(a);
    let mut hidden = IndexMap::new();
    for (t, n, fts) in &missing {
        if *n == 0
            && let Some(h) = hidden_matches(&conn, fts, &f, &narrow, other_scope)?
        {
            hidden.insert(t.clone(), h);
        }
    }
    let missing = missing.into_iter().map(|(t, n, _)| (t, n)).collect();
    let (other, other_cap) = if other_scope {
        other_repos(&conn, query_text, &f, &o, topic_fts.as_deref())?
    } else {
        (IndexMap::new(), None)
    };
    drop(conn);
    let shared = shared_refs(&hits, query_text);
    Ok(Lookup {
        query: query_text.to_string(),
        bias: if a.recent { Vec::new() } else { bias_list },
        repo: f.repo,
        all_repos: f.include_all_repos,
        scopes: split_list(a.scopes),
        limit: a.limit,
        beyond_limit: n_scored - hits.len() as i64,
        hits,
        missing,
        pool_cap,
        other_repos: other,
        other_repos_cap: other_cap,
        shared_refs: shared,
        hidden,
    })
}

/// The items that match `fts` but that the status or the filters `narrow` of `f` hid, when no
/// item `f` keeps matches it; else, with `other_scope`, the ones in other repos, under the same
/// status and filters first.
fn hidden_matches(
    conn: &Connection,
    fts: &str,
    f: &Filters,
    narrow: &[String],
    other_scope: bool,
) -> Result<Option<Hidden>, SmError> {
    let unfiltered = Filters {
        tags_filter: None,
        kinds_filter: None,
        sources_filter: None,
        since_epoch: None,
        until_epoch: None,
        ..f.clone()
    };
    let with_history = |g: &Filters| Filters {
        include_history: true,
        ..g.clone()
    };
    let all_repos = |g: &Filters| Filters {
        include_all_repos: true,
        ..g.clone()
    };
    let filtered = unfiltered != *f;
    // first this scope, then other repos: the query's own status and filters, then wider ones
    let scopes = std::iter::once((f.clone(), unfiltered.clone(), false))
        .chain(other_scope.then(|| (all_repos(f), all_repos(&unfiltered), true)));
    let mut tries = Vec::new();
    for (base, unf, other) in scopes {
        if other {
            tries.push((base.clone(), false, false, true));
        }
        if !f.include_history {
            tries.push((with_history(&base), true, false, other));
        }
        if filtered {
            tries.push((unf.clone(), false, true, other));
        }
        if filtered && !f.include_history {
            tries.push((with_history(&unf), true, true, other));
        }
    }
    for (g, retired, filtered, other_repos) in tries {
        let n = count_matches(conn, fts, &g)?;
        if n > 0 {
            let outside = format!("outside {}", narrow.join(" "));
            let parts = [
                retired.then_some("retired or superseded"),
                filtered.then_some(outside.as_str()),
                other_repos.then_some("in other repos"),
            ];
            let by = parts.into_iter().flatten().collect::<Vec<_>>().join(", ");
            return Ok(Some(Hidden {
                n,
                retired,
                filtered,
                other_repos,
                by,
            }));
        }
    }
    Ok(None)
}

/// `item_tokens`: title and body characters / 4, rounded up.
pub fn item_tokens(it: &Item) -> i64 {
    let n = (py::str::len(&it.title) + py::str::len(&it.body)) as i64;
    -((-n).div_euclid(4))
}

/// `item_text`: title and body on one line; no title when the body starts with it.
pub fn item_text(it: &Item) -> String {
    let body_one_line = py::str::split(&it.body).join(" ");
    let title = py::str::strip(&it.title);
    if !title.is_empty() && !body_one_line.starts_with(title) {
        return format!("{title} — {body_one_line}");
    }
    body_one_line
}

pub fn next_cmds(lk: &Lookup) -> Vec<String> {
    let ids: Vec<&str> = lk.hits.iter().take(3).map(|s| s.item.id.as_str()).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    vec![
        format!("sm show {}", ids.join(" ")),
        format!("sm related {}", ids[0]),
    ]
}

/// The object `sm query --format json` prints and the MCP tool `memory_query` returns.
pub fn lookup_json(lk: &Lookup) -> Value {
    let results: Vec<Value> = lk
        .hits
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let it = &s.item;
            let text = item_text(it);
            let snippet = match_snippet(
                &text,
                &s.why.terms_matched,
                &s.why.ref_hits,
                JSON_SNIPPET_CHARS,
            );
            json!({
                "rank": i + 1,
                "id": it.id,
                "repo": it.repo,
                "kind": it.kind,
                "tag": it.tag,
                "as_of": it.as_of,
                "status": it.status,
                "title": it.title,
                "snippet": snippet,
                "refs": py::str::split(&it.refs),
                "tokens": item_tokens(it),
                "why": {
                    "ref_hits": s.why.ref_hits,
                    "phrase": s.why.phrase,
                    "terms_matched": s.why.terms_matched,
                    "terms_missing": s.why.terms_missing,
                    "bias": s.why.bias,
                    "score": s.why.score,
                },
            })
        })
        .collect();
    let mut scope = json!({"repo": lk.repo, "all_repos": lk.all_repos});
    if !lk.scopes.is_empty() {
        scope["in"] = json!(lk.scopes);
    }
    json!({
        "query": lk.query,
        "bias": lk.bias,
        "scope": scope,
        "results": results,
        "missing": lk.missing.iter().map(|(t, n)| {
            let mut entry = json!({"term": t, "in_scope": n});
            match lk.hidden.get(t) {
                Some(h) if h.other_repos => entry["in_other_repos"] = json!(h.n),
                Some(h) => {
                    entry["hidden"] = json!(h.n);
                    entry["hidden_by"] = json!(h.by);
                }
                None => {}
            }
            entry
        }).collect::<Vec<_>>(),
        "more": {
            "beyond_limit": lk.beyond_limit,
            "pool_full": lk.pool_cap.is_some(),
            "other_repos": Value::Object(lk.other_repos.iter().map(|(k, v)| (k.clone(), Value::from(*v))).collect()),
            "other_repos_pool_full": lk.other_repos_cap.is_some(),
        },
        "refs": lk.shared_refs.iter().map(|(r, n)| json!({"ref": r, "count": n})).collect::<Vec<_>>(),
        "next": next_cmds(lk),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, body: &str, refs: &str) -> Item {
        let mut it = Item::bare("m_1", "r", "fact", title, body, "decided");
        it.refs = refs.to_string();
        it
    }

    fn scored(id: &str, refs: &str) -> Scored {
        let mut it = item("", "b", refs);
        it.id = id.to_string();
        Scored {
            item: it,
            why: Default::default(),
        }
    }

    #[test]
    fn split_list_forms() {
        assert_eq!(split_list(StrList::Str(" a, ,b,")), ["a", "b"]);
        assert_eq!(
            split_list(StrList::List(&[" x ".into(), "\x1c".into()])),
            ["x"]
        );
        assert!(split_list(StrList::None).is_empty());
    }

    #[test]
    fn item_text_and_tokens() {
        assert_eq!(
            item_text(&item("Title", "Title and\n more", "")),
            "Title and more"
        );
        assert_eq!(item_text(&item(" T ", "body\x1ctext", "")), "T — body text");
        assert_eq!(item_text(&item("", "  ", "")), "");
        // 5 + 4 chars: ceil(9 / 4) = 3
        assert_eq!(item_tokens(&item("Café!", "body", "")), 3);
        assert_eq!(item_tokens(&item("", "", "")), 0);
        assert_eq!(item_tokens(&item("abcd", "", "")), 1);
    }

    #[test]
    fn shared_refs_skips_refs_the_query_names_and_keeps_first_seen_order() {
        let hits = [
            scored("a", "Y-2 X-1 Z-3 Y-2"),
            scored("b", "X-1 Y-2"),
            scored("c", "Z-3 W-4"),
            scored("d", "W-4"),
        ];
        // four refs are shared; the three first seen stay
        assert_eq!(
            shared_refs(&hits, "q"),
            [
                ("Y-2".to_string(), 2),
                ("X-1".to_string(), 2),
                ("Z-3".to_string(), 2)
            ]
        );
        // fold("X-1") = "x-1" is a substring of the folded query
        assert_eq!(
            shared_refs(&hits, "about x-12"),
            [
                ("Y-2".to_string(), 2),
                ("Z-3".to_string(), 2),
                ("W-4".to_string(), 2)
            ]
        );
        let more = [
            scored("a", "A-1"),
            scored("b", "B-1 A-1"),
            scored("c", "B-1"),
            scored("d", "B-1"),
        ];
        assert_eq!(
            shared_refs(&more, "q"),
            [("B-1".to_string(), 3), ("A-1".to_string(), 2)]
        );
        assert!(shared_refs(&hits[..1], "q").is_empty());
    }

    #[test]
    fn pick_local_units_codes_first_and_their_words_not_twice() {
        let df: IndexMap<String, i64> = [("the forge", 2), ("forge", 5), ("bolt", 1), ("zero", 0)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let terms = ["forge", "bolt", "zero", "absent"].map(String::from);
        let codes = ["The Forge", "Nope"].map(String::from);
        assert_eq!(pick_local_units(&terms, &codes, &df), ["The Forge", "bolt"]);
        let many: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
        let df_many: IndexMap<String, i64> = many
            .iter()
            .enumerate()
            .map(|(i, t)| (t.clone(), (20 - i) as i64))
            .collect();
        let picked = pick_local_units(&many, &[], &df_many);
        assert_eq!(picked.len(), LOCAL_MAX_TERMS);
        // Rarest 12 terms (t8..=t19, df 12 down to 1) are selected over common earlier terms
        // (t0..=t7, df 20 down to 13), while preserving original slice order.
        let expected_rare: Vec<String> = (8..20).map(|i| format!("t{i}")).collect();
        assert_eq!(picked, expected_rare);
        // Prefix-overlapping family keeps only the rarest unit ("ranking" df=3 drops "rank" df=20).
        let df_fam: IndexMap<String, i64> = [("rank", 20), ("ranking", 3), ("alpha", 5)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert_eq!(
            pick_local_units(&["rank", "ranking", "alpha"], &[], &df_fam),
            ["ranking", "alpha"]
        );
        // Digits in short token ("port80") prevent FTS prefix expansion, so "port8080" stays.
        let df_ports: IndexMap<String, i64> = [("port80", 2), ("port8080", 1)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert_eq!(
            pick_local_units(&["port80", "port8080"], &[], &df_ports),
            ["port80", "port8080"]
        );
        // Kept code words also drop prefix-overlapping terms ("Alpha" covers "alphabet").
        let df_taken: IndexMap<String, i64> = [("alpha", 2), ("alphabet", 1)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert_eq!(
            pick_local_units(&["alphabet"], &["Alpha"], &df_taken),
            ["Alpha"]
        );
        assert!(pick_local_units::<String>(&[], &[], &df).is_empty());
    }

    #[test]
    fn next_cmds_and_json_snippet() {
        let mut lk = Lookup {
            query: "q".into(),
            bias: vec![],
            repo: None,
            all_repos: true,
            scopes: vec![],
            limit: 4,
            hits: vec![],
            missing: vec![],
            hidden: IndexMap::new(),
            beyond_limit: 0,
            pool_cap: None,
            other_repos: IndexMap::new(),
            other_repos_cap: None,
            shared_refs: vec![],
        };
        assert!(next_cmds(&lk).is_empty());
        let mut long = scored("m_9", "A-1");
        long.item.body = format!("{}  {}", "x".repeat(296), "y".repeat(10));
        lk.hits = vec![
            scored("m_1", ""),
            scored("m_2", ""),
            scored("m_3", ""),
            long,
        ];
        assert_eq!(next_cmds(&lk), ["sm show m_1 m_2 m_3", "sm related m_1"]);
        let v = lookup_json(&lk);
        // 296 x then 1 space survive the 297-char cut; rstrip drops the space
        assert_eq!(
            v["results"][3]["snippet"],
            format!("{}...", "x".repeat(296))
        );
        assert_eq!(v["results"][3]["refs"], json!(["A-1"]));
        assert_eq!(v["more"]["pool_full"], json!(false));
    }

    #[test]
    fn prepare_validates_in_order() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let err = |a: QueryArgs| {
            prepare(
                &QueryArgs {
                    home: Some(home),
                    ..a
                },
                None,
            )
            .unwrap_err()
            .msg
        };
        assert_eq!(
            err(QueryArgs {
                limit: 0,
                tags: StrList::Str("bad"),
                ..Default::default()
            }),
            "--limit must be at least 1"
        );
        assert!(
            err(QueryArgs {
                tags: StrList::Str("bad"),
                kinds: StrList::Str("bad"),
                ..Default::default()
            })
            .starts_with("Invalid tag 'bad'")
        );
        assert!(
            err(QueryArgs {
                kinds: StrList::Str("note"),
                bias: StrList::Str("x"),
                ..Default::default()
            })
            .starts_with("Invalid kind 'note'")
        );
        assert!(
            err(QueryArgs {
                bias: StrList::Str(" Nope"),
                ..Default::default()
            })
            .starts_with("Invalid bias ' Nope'; expected one of recency, relevance")
        );
        let (f, b, _, _) = prepare(
            &QueryArgs {
                home: Some(home),
                repo: " r ",
                tags: StrList::Str("n/a,Executed"),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            (f.repo.as_deref(), f.tags_filter, b),
            (
                Some("r"),
                Some(vec!["n/a".to_string(), "executed".to_string()]),
                crate::core::rank::DEFAULT_BIAS.map(String::from).to_vec(),
            )
        );
        let (f, _, _, _) = prepare(
            &QueryArgs {
                home: Some(home),
                all_repos: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(f.repo, None);

        // default_bias_from_cfg branches and empty-bias fallback
        assert_eq!(
            default_bias_from_cfg(
                &crate::config::toml_loads("[query]\ndefault_bias = \"executed, recency\"\n")
                    .unwrap()
            )
            .unwrap(),
            vec!["executed".to_string(), "recency".to_string()]
        );
        for (bad_toml, want) in [
            (
                "[query]\ndefault_bias = [\"fact\", 1]\n",
                "expected a list of strings",
            ),
            ("[query]\ndefault_bias = 5\n", "expected a list of strings"),
            ("query = 5\n", "[query]: expected a table"),
        ] {
            let cfg = crate::config::toml_loads(bad_toml).unwrap();
            let msg = default_bias_from_cfg(&cfg).unwrap_err().msg;
            assert!(msg.contains(want), "{bad_toml}: {msg}");
        }
        let custom_home = tempfile::tempdir().unwrap();
        std::fs::write(
            custom_home.path().join("config.toml"),
            "[query]\ndefault_bias = [\"decided\"]\n",
        )
        .unwrap();
        let (_, b_empty_str, _, _) = prepare(
            &QueryArgs {
                home: custom_home.path().to_str(),
                all_repos: true,
                bias: StrList::Str(" , "),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(b_empty_str, vec!["decided".to_string()]);
        let empty_list = vec!["".to_string(), "  ".to_string()];
        let (_, b_empty_list, _, _) = prepare(
            &QueryArgs {
                home: custom_home.path().to_str(),
                all_repos: true,
                bias: StrList::List(&empty_list),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(b_empty_list, vec!["decided".to_string()]);
    }

    #[test]
    fn keyword_aliases_from_config() {
        let cfg = crate::config::toml_loads(
            "[keyword_aliases]\ncdn = \"edge cache\"\nab = [\"x\", 2, 07:32]\nd = 1979-05-27\nt = 07:32\nn = 99999999999999999999\n",
        )
        .unwrap();
        let mut root = crate::config::defaults();
        if let PyValue::Dict(p) = &mut root {
            for (k, v) in p.iter_mut() {
                if matches!(k, PyValue::Str(s) if s == "keyword_aliases") {
                    *v = value_get(&cfg, "keyword_aliases").unwrap().clone();
                }
            }
        }
        let ka = keyword_aliases(&root).unwrap();
        let expand =
            |q: &str| crate::core::terms::expand_aliases(q, Some(&ka)).map_err(|e| e.python_text());
        // Python 3.15.1: expand_aliases(q, load_config(home)["keyword_aliases"]) on this file.
        assert_eq!(
            expand("cdn ab"),
            Ok(vec![
                "edge cache".to_string(),
                "x".into(),
                "2".into(),
                "07:32:00".into()
            ])
        );
        assert_eq!(
            expand("d"),
            Err("TypeError: 'datetime.date' object is not iterable".to_string())
        );
        assert_eq!(
            expand("t"),
            Err("TypeError: 'datetime.time' object is not iterable".to_string())
        );
        assert_eq!(
            expand("n"),
            Err("TypeError: 'int' object is not iterable".to_string())
        );
        assert_eq!(
            keyword_aliases(&crate::config::defaults()),
            Some(Vec::new())
        );
    }

    #[test]
    fn lookup_end_to_end_with_footer_counts() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();
        let ins = |id: &str, repo: &str, title: &str, refs: &str| {
            conn.execute(
                "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash, refs) VALUES (?, ?, 'fact', ?, 'b', 'decided', '2026-01-01T00:00:00+00:00', ?1, ?)",
                [id, repo, title, refs],
            )
            .unwrap();
        };
        ins("m_1", "r", "loader one", "S-1");
        ins("m_2", "r", "loader two", "S-1");
        ins("m_3", "other", "loader three", "");
        ins("m_4", "other", "absentword", "");
        ins("m_5", "global", "loader global", "");
        let lk = run_lookup(
            "loader absentword",
            &QueryArgs {
                limit: 1,
                repo: "r",
                home: Some(home),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(lk.hits.len(), 1);
        // m_1, m_2 and the global m_5 are in scope r: one shown, two beyond
        assert_eq!(lk.beyond_limit, 2);
        // absentword is in the store but not in scope r
        assert_eq!(lk.missing, [("absentword".to_string(), 0)]);
        // global is in scope, so only "other" counts, with m_3 and m_4
        assert_eq!(
            lk.other_repos.into_iter().collect::<Vec<_>>(),
            [("other".to_string(), 2)]
        );
        // across all repos there is no other-repo footer; recent shows no bias
        let lk = run_lookup(
            "loader",
            &QueryArgs {
                all_repos: true,
                recent: true,
                home: Some(home),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((lk.other_repos.len(), lk.bias.len(), lk.repo), (0, 0, None));
        let ids = ["m_1".to_string(), "m_3".to_string()];
        let items: Vec<Item> = ids
            .iter()
            .map(|i| Item::bare(i, "r", "fact", "", "", "decided"))
            .collect();
        let df: IndexMap<String, i64> = [("loader".to_string(), 2)].into_iter().collect();
        let units = ["Loader".to_string()];
        // ln(10000 / 2) = 8.5 clears the floor; ln(2 / 2) = 0 does not
        let kept: Vec<String> = over_floor(&conn, items.clone(), &units, &df, 10000)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(kept, ids);
        assert!(
            over_floor(&conn, items.clone(), &units, &df, 2)
                .unwrap()
                .is_empty()
        );
        let err = over_floor(&conn, items.clone(), &units, &IndexMap::new(), 2).unwrap_err();
        assert_eq!(err.python_text(), "KeyError: 'loader'");
        let err = over_floor(&conn, items, &units, &df, 0).unwrap_err();
        assert_eq!(
            err.python_text(),
            "ValueError: expected a positive input, got 0.0"
        );
        let err = run_lookup(
            "x",
            &QueryArgs {
                limit: -3,
                home: Some(home),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.msg, "--limit must be at least 1");
    }

    // ---- query property tests: the seeds and the store follow the
    // Python tests; the draws come from a small LCG, not Python's random.

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn unit(&mut self) -> f64 {
            self.next() as f64 / (1u64 << 31) as f64
        }
    }

    const WORDS: [&str; 15] = [
        "alder", "birch", "cedar", "dune", "ember", "fjord", "grove", "heath", "inlet", "juniper",
        "kelp", "lagoon", "marsh", "nectar", "orchid",
    ];
    const CODES: [&str; 4] = ["ZZ", "Kappa", "the Forge", "Blue Fox"];
    const NOISE: [&str; 16] = [
        "the", "and", "please", "café", "x", "AND", "NEAR", "*", "\"", "(", "-", "2026", "é",
        "v1.2", "a/b", "😀",
    ];

    fn prop_store() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap().to_string();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();
        let mut rng = Lcg(20261104);
        for n in 0..48 {
            // word k lands in about 1 item in k + 1, so df runs from 48 down to a few
            let mut words: Vec<&str> = WORDS
                .iter()
                .enumerate()
                .filter(|(k, _)| rng.unit() < 1.0 / (*k as f64 + 1.0))
                .map(|(_, w)| *w)
                .collect();
            if n % 4 == 0 {
                words.push(CODES[rng.below(CODES.len())]);
            }
            let body = format!(
                "note {n}: {}",
                if words.is_empty() {
                    "empty".to_string()
                } else {
                    words.join(" ")
                }
            );
            let id = format!("m_{n:02}");
            conn.execute(
                "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash) VALUES (?1, 'alpha', 'fact', ?2, ?2, 'decided', '2026-01-01T00:00:00+00:00', ?1)",
                [&id, &body],
            )
            .unwrap();
        }
        d
    }

    /// The hook's split of a prompt: folded words as terms, the codes the prompt names.
    fn prompt_units(rng: &mut Lcg, n_words: usize) -> (Vec<String>, Vec<String>) {
        let pool: Vec<String> = WORDS
            .iter()
            .chain(WORDS.iter())
            .chain(CODES.iter())
            .chain(NOISE.iter())
            .map(|w| w.to_string())
            .chain(WORDS[..4].iter().map(|w| w.to_uppercase()))
            .collect();
        let prompt: Vec<String> = (0..n_words)
            .map(|_| pool[rng.below(pool.len())].clone())
            .collect();
        let text = prompt.join(" ");
        let codes: Vec<String> = CODES
            .iter()
            .filter(|c| text.contains(*c))
            .map(|c| c.to_string())
            .collect();
        let mut terms: Vec<String> = Vec::new();
        for w in crate::core::terms::word_runs(&fold(Some(&text))) {
            if !terms.iter().any(|t| t == w) {
                terms.push(w.to_string());
            }
        }
        (terms, codes)
    }

    #[test]
    fn p1_local_query_never_raises_and_returns_a_query_iff_items() {
        let d = prop_store();
        let home = d.path().to_str().unwrap();
        let mut rng = Lcg(20261105);
        let mut injected = 0;
        for _ in 0..120 {
            let n = rng.below(61);
            let (terms, codes) = prompt_units(&mut rng, n);
            let a = QueryArgs {
                limit: 1 + rng.below(6) as i64,
                repo: "alpha",
                home: Some(home),
                ..Default::default()
            };
            let extra = rng.below(4) as i64;
            let (query, items, _) =
                local_query_at(&terms, &codes, extra, &a, None, Some(2.5)).unwrap();
            assert_eq!(query.is_empty(), items.is_empty());
            assert!(items.len() as i64 <= a.limit + extra);
            injected += usize::from(!items.is_empty());
        }
        // the cases reach the floor and the search, not only the empty path
        assert!(injected > 30, "{injected}");
        // at the real floor of 8.0 one rare word (ln(48) = 3.87 at most) never injects
        let a = QueryArgs {
            limit: 4,
            repo: "alpha",
            home: Some(home),
            ..Default::default()
        };
        let (query, items, _) = run_local_query(
            &["orchid".to_string()],
            &[],
            4,
            0,
            "alpha",
            StrList::None,
            Some(home),
            None,
            None,
        )
        .unwrap();
        assert_eq!((query, items), (String::new(), vec![]));
        assert!(
            local_query_at(&["orchid".to_string()], &[], 0, &a, None, Some(0.0))
                .unwrap()
                .1
                .len()
                <= 4
        );
    }

    #[test]
    fn p2_a_kept_codes_words_are_never_also_units_and_terms_stay_capped() {
        let mut rng = Lcg(20261106);
        let vocab: Vec<&str> = WORDS
            .iter()
            .copied()
            .chain(["blue", "forge", "fox", "kappa", "zz"])
            .collect();
        for _ in 0..480 {
            let mut codes: Vec<String> = CODES.iter().map(|c| c.to_string()).collect();
            codes.truncate(rng.below(CODES.len() + 1));
            let mut terms: Vec<String> = Vec::new();
            for _ in 0..20 {
                let w = fold(Some(vocab[rng.below(vocab.len())]));
                if !terms.contains(&w) {
                    terms.push(w);
                }
            }
            let df: IndexMap<String, i64> = terms
                .iter()
                .cloned()
                .chain(codes.iter().map(|c| fold(Some(c))))
                .map(|t| (t, [0, 0, 1, 3, 9][rng.below(5)]))
                .collect();
            let units = pick_local_units(&terms, &codes, &df);
            let kept_codes: Vec<&String> = units.iter().filter(|u| codes.contains(u)).collect();
            let kept_terms = &units[kept_codes.len()..];
            assert_eq!(
                kept_codes,
                codes
                    .iter()
                    .filter(|c| df[&fold(Some(c))] > 0)
                    .collect::<Vec<_>>()
            );
            let code_words: HashSet<String> = kept_codes
                .iter()
                .flat_map(|c| {
                    py::str::split(&fold(Some(c)))
                        .into_iter()
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .collect();
            assert!(
                kept_terms
                    .iter()
                    .all(|t| !code_words.contains(t) && df[t] > 0),
                "{codes:?} {terms:?} {df:?}"
            );
            assert!(kept_terms.len() <= LOCAL_MAX_TERMS);
        }
    }

    #[test]
    fn p4_a_higher_tau_never_adds_an_id() {
        let d = prop_store();
        let home = d.path().to_str().unwrap();
        let a = QueryArgs {
            limit: LOCAL_SEARCH_DEPTH,
            repo: "alpha",
            home: Some(home),
            ..Default::default()
        };
        let mut rng = Lcg(20261108);
        let mut strict_hits = 0;
        for (low, high) in [(0.0, 1.0), (1.0, 2.5), (2.5, 4.0), (4.0, 8.0)] {
            for _ in 0..30 {
                let n = 1 + rng.below(30);
                let (terms, codes) = prompt_units(&mut rng, n);
                let ids = |tau| -> HashSet<String> {
                    local_query_at(&terms, &codes, 0, &a, None, Some(tau))
                        .unwrap()
                        .1
                        .into_iter()
                        .map(|it| it.id)
                        .collect()
                };
                let (loose, strict) = (ids(low), ids(high));
                assert!(strict.is_subset(&loose), "{terms:?} {codes:?}");
                strict_hits += strict.len();
            }
        }
        assert!(strict_hits > 0);
    }

    // ---- lookup tests

    fn seed(rows: &[(&str, &str, &str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let conn = connect(&format!("{}/memory.db", d.path().to_str().unwrap()), None).unwrap();
        for (id, repo, body, refs) in rows {
            let mut it = Item::bare(id, repo, "fact", body, body, "derived");
            it.refs = refs.to_string();
            it.as_of = "2026-10-25T10:00:00+00:00".into();
            crate::store::items::put_item(&conn, &it.post_init(), None).unwrap();
        }
        d
    }

    fn look(d: &tempfile::TempDir, q: &str, a: QueryArgs) -> Lookup {
        run_lookup(
            q,
            &QueryArgs {
                home: d.path().to_str(),
                ..a
            },
        )
        .unwrap()
    }

    fn pairs(v: &[(&str, i64)]) -> Vec<(String, i64)> {
        v.iter().map(|(s, n)| (s.to_string(), *n)).collect()
    }

    #[test]
    fn shared_refs_need_two_holders_and_skip_refs_the_query_names() {
        let d = seed(&[
            ("m_1", "a", "Kiwi one", "R6 x.md y.md solo1"),
            ("m_2", "a", "Kiwi two", "R6 x.md y.md"),
            ("m_3", "a", "Kiwi three", "y.md"),
        ]);
        let lk = look(
            &d,
            "kiwi R6",
            QueryArgs {
                repo: "a",
                limit: 3,
                ..Default::default()
            },
        );
        assert_eq!(lk.shared_refs, pairs(&[("y.md", 3), ("x.md", 2)]));
    }

    #[test]
    fn footer_lists_are_capped() {
        let mut rows = vec![
            ("m_hit", "a", "Kiwi base", "r1 r2 r3 r4"),
            ("m_hit2", "a", "Kiwi again", "r1 r2 r3 r4"),
        ];
        rows.extend([
            ("m_b", "b", "Kiwi elsewhere", ""),
            ("m_c", "c", "Kiwi elsewhere", ""),
            ("m_d", "d", "Kiwi elsewhere", ""),
            ("m_e", "e", "Kiwi elsewhere", ""),
        ]);
        let d = seed(&rows);
        let lk = look(
            &d,
            "kiwi qa qb qc qd qe qf",
            QueryArgs {
                repo: "a",
                limit: 2,
                ..Default::default()
            },
        );
        assert_eq!(
            lk.missing
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["qa", "qb", "qc", "qd", "qe"]
        );
        assert_eq!(
            lk.other_repos.into_iter().collect::<Vec<_>>(),
            pairs(&[("b", 1), ("c", 1), ("d", 1)])
        );
        assert_eq!(lk.shared_refs, pairs(&[("r1", 2), ("r2", 2), ("r3", 2)]));
    }

    #[test]
    fn all_repos_counts_no_other_repo() {
        let d = seed(&[("m_a", "a", "Kiwi a", ""), ("m_b", "b", "Kiwi b", "")]);
        let lk = look(
            &d,
            "kiwi",
            QueryArgs {
                all_repos: true,
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            (lk.repo, lk.other_repos.len(), lk.beyond_limit),
            (None, 0, 1)
        );
        // --repo with --all: the repo wins for the boost, every repo is in scope, so none is "other"
        let lk = look(
            &d,
            "kiwi",
            QueryArgs {
                repo: "a",
                all_repos: true,
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            (lk.repo.as_deref(), lk.other_repos.len(), lk.beyond_limit),
            (Some("a"), 0, 1)
        );
    }

    #[test]
    fn recent_reports_no_bias() {
        let d = seed(&[("m_a", "a", "Kiwi a", "")]);
        assert!(
            look(
                &d,
                "kiwi",
                QueryArgs {
                    repo: "a",
                    recent: true,
                    ..Default::default()
                }
            )
            .bias
            .is_empty()
        );
        assert_eq!(
            look(
                &d,
                "kiwi",
                QueryArgs {
                    repo: "a",
                    bias: StrList::Str("executed"),
                    ..Default::default()
                }
            )
            .bias,
            ["executed"]
        );
    }

    #[test]
    fn a_ref_only_hit_never_gives_a_negative_beyond_count() {
        // "+++" has no word for FTS, so the in-scope count is 0 while the ref match finds the item
        let d = seed(&[("m_p", "a", "Plus signs", "+++")]);
        let lk = look(
            &d,
            "+++",
            QueryArgs {
                repo: "a",
                ..Default::default()
            },
        );
        let ids: Vec<&str> = lk.hits.iter().map(|s| s.item.id.as_str()).collect();
        assert_eq!(
            (ids, lk.beyond_limit, lk.missing.len()),
            (vec!["m_p"], 0, 0)
        );
    }

    #[test]
    fn other_repos_count_only_the_first_cap_of_a_wider_union() {
        // 60 strong "cafe" items in b fill the folded pool; 5 weak "café" items in c come in through
        // the accented index, so the union holds 67; the pool keeps the best 50: c 5, global 1,
        // unassigned 1 (in scope, never listed), b 43
        let filler = (0..80)
            .map(|n| format!("filler{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ids_b: Vec<(String, String)> = (0..60)
            .map(|i| (format!("m_b{i:02}"), format!("Cafe cafe cafe {i}")))
            .collect();
        let ids_c: Vec<(String, String)> = (0..5)
            .map(|i| (format!("m_c{i}"), format!("café {i} {filler}")))
            .collect();
        let mut rows: Vec<(&str, &str, &str, &str)> = ids_b
            .iter()
            .map(|(i, b)| (i.as_str(), "b", b.as_str(), ""))
            .collect();
        rows.extend(ids_c.iter().map(|(i, b)| (i.as_str(), "c", b.as_str(), "")));
        rows.extend([
            ("m_g", "global", "Café café global", ""),
            ("m_u", "unassigned", "Café café unassigned", ""),
        ]);
        let d = seed(&rows);
        let lk = look(
            &d,
            "café",
            QueryArgs {
                repo: "a",
                bias: StrList::Str("recency"),
                ..Default::default()
            },
        );
        assert_eq!(
            (
                lk.other_repos.into_iter().collect::<Vec<_>>(),
                lk.other_repos_cap
            ),
            (pairs(&[("b", 43), ("c", 5)]), Some(50))
        );
    }

    #[test]
    fn missing_and_why_leave_out_one_letter_terms_and_fts_operator_words() {
        let d = seed(&[("m_k", "a", "Kiwi base", "")]);
        let lk = look(
            &d,
            "kiwi NEAR(a b) Q3 OR And not Near",
            QueryArgs {
                repo: "a",
                ..Default::default()
            },
        );
        assert_eq!(
            lk.hits
                .iter()
                .map(|s| s.item.id.as_str())
                .collect::<Vec<_>>(),
            ["m_k"]
        );
        // a 2-letter id such as Q3 stays; one letter and the words and, or, not, near (any case) go
        assert_eq!(lk.missing, pairs(&[("q3", 0)]));
        assert_eq!(
            (
                lk.hits[0].why.terms_matched.clone(),
                lk.hits[0].why.terms_missing.clone()
            ),
            (vec!["kiwi".to_string()], vec!["q3".to_string()])
        );
    }

    #[test]
    fn query_date_and_source_filters_validate_and_narrow_results() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let conn = open(Some(home), None).unwrap();
        for (id, as_of, src) in [
            ("m_early", "2026-05-01T10:00:00+00:00", "task:T-1"),
            ("m_mid", "2026-05-10T12:00:00+00:00", "agent:A-1"),
            ("m_late", "2026-05-20T15:00:00+00:00", "task:T-2"),
        ] {
            let mut it = Item::bare(
                id,
                "a",
                "fact",
                &format!("Kiwi item {id}"),
                &format!("Kiwi body {id}"),
                "decided",
            );
            it.as_of = as_of.into();
            it.source = src.into();
            crate::store::items::put_item(&conn, &it.post_init(), None).unwrap();
        }
        drop(conn);

        let err = run_query(
            "kiwi",
            &QueryArgs {
                repo: "a",
                since: "2026-05-20",
                until: "2026-05-10",
                home: Some(home),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.python_text(),
            "ValueError: --since cannot be after --until"
        );

        let by_src = run_query(
            "kiwi",
            &QueryArgs {
                repo: "a",
                sources: StrList::Str("task:"),
                home: Some(home),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let mut src_ids: Vec<&str> = by_src.iter().map(|i| i.id.as_str()).collect();
        src_ids.sort();
        assert_eq!(src_ids, ["m_early", "m_late"]);

        let by_date = run_query(
            "kiwi",
            &QueryArgs {
                repo: "a",
                since: "2026-05-05T00:00:00Z",
                until: "2026-05-15T00:00:00Z",
                home: Some(home),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            by_date.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["m_mid"]
        );
    }
}
