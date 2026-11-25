//! `sm stats`: store counts, how the prompt hook's recall gets used, catch-up runs and backups.
//! Reads memory.db, hook.log, usage.log and catchup.log; writes nothing.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::config::{get_db_path, get_home, load_config, scope_matchers, scopes};
use crate::error::SmError;
use crate::py::float::{repr as float_repr, round_ndigits};
use crate::py::path::{Errors, PyPath};
use crate::py::str::{splitlines, strip};
use crate::py::value::PyValue;
use crate::service::backup::{auto_backups, backup_age_hours};
use crate::service::upkeep::{STALE_DAYS, STALE_TAGS, run_stale};
use crate::service::usage::USAGE_LOG;
use crate::sources::{dict_get, get_attr, iterate, truthy};
use crate::store::connect::connect;
use crate::store::search::{Filters, count_scope};
use crate::store::stats::{item_labels, store_counts};

pub const NEVER_OPENED_MIN: i64 = 5;
pub const NEVER_OPENED_TOP: usize = 10;
/// 100 years: a larger window overflows datetime.
pub const MAX_DAYS: i64 = 36_500;

// ---------------------------------------------------------------- timestamps

/// A log row's time, with the offset it was written with.
type Ts = chrono::DateTime<chrono::FixedOffset>;

const US_PER_DAY: i64 = 86_400_000_000;

/// The `ts` of a log row as sm writes it: RFC 3339 (`+00:00` or `Z`, any fraction), or with no
/// offset, read as UTC; None for any other value.
fn ts_of(value: Option<&PyValue>) -> Option<Ts> {
    let Some(PyValue::Str(s)) = value else {
        return None;
    };
    chrono::DateTime::parse_from_rfc3339(s).ok().or_else(|| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|n| n.and_utc().fixed_offset())
    })
}

/// Python's `isoformat()`: the microseconds only when they are not 0.
fn isoformat(t: &Ts) -> String {
    let us = t.timestamp_subsec_micros();
    let frac = if us == 0 {
        String::new()
    } else {
        format!(".{us:06}")
    };
    format!("{}{frac}{}", t.format("%Y-%m-%dT%H:%M:%S"), t.format("%:z"))
}

// ---------------------------------------------------------------- logs

type Rows = Vec<(Ts, PyValue)>;

/// (rows with a readable ts, oldest first as written; count of lines that are
/// not such a row).
pub fn read_rows(path: &PyPath) -> Result<(Rows, i64), SmError> {
    if !path.is_file() {
        return Ok((Vec::new(), 0));
    }
    let mut rows = Vec::new();
    let mut bad = 0;
    let text = path.read_text(Errors::Replace)?;
    for line in splitlines(&text, false) {
        if strip(line).is_empty() {
            continue;
        }
        let row = match crate::py::json::loads(line) {
            Ok(r) => r,
            Err(e) if e.class == "JSONDecodeError" => {
                bad += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let ts = match &row {
            PyValue::Dict(d) => ts_of(dict_get(d, "ts")),
            _ => None,
        };
        match ts {
            Some(ts) => rows.push((ts, row)),
            None => bad += 1,
        }
    }
    Ok((rows, bad))
}

fn get<'a>(r: &'a PyValue, key: &str) -> Option<&'a PyValue> {
    match r {
        PyValue::Dict(d) => dict_get(d, key),
        _ => None,
    }
}

fn is_str(v: Option<&PyValue>, s: &str) -> bool {
    matches!(v, Some(PyValue::Str(x)) if x == s)
}

fn truthy_opt(v: Option<&PyValue>) -> bool {
    v.is_some_and(truthy)
}

/// `str(v)` of `r.get(key)`.
fn str_get(r: &PyValue, key: &str) -> String {
    get(r, key).map_or_else(|| "None".to_string(), PyValue::py_str)
}

/// `for i in r.get(key) or []`, each as `str(i)`.
fn ids_of(r: &PyValue, key: &str) -> Result<Vec<String>, SmError> {
    match get(r, key).filter(|v| truthy(v)) {
        None => Ok(Vec::new()),
        Some(v) => Ok(iterate(v)?.iter().map(|x| x.py_str()).collect()),
    }
}

fn path_of(r: &PyValue) -> &'static str {
    if is_str(get(r, "path"), "local") {
        "local"
    } else {
        "haiku"
    }
}

const PATHS: [&str; 2] = ["haiku", "local"];

/// `round(n / d, 3)`, or None when `d` is 0.
fn share(n: usize, d: usize) -> Result<Option<f64>, SmError> {
    if d == 0 {
        return Ok(None);
    }
    Ok(Some(round_ndigits(n as f64 / d as f64, 3)?))
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathCounts {
    pub with_injection: i64,
    pub tracked_ids: i64,
    pub opened_ids: i64,
    pub opened_share: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recall {
    pub hook_runs: i64,
    pub with_injection: i64,
    pub injections: i64,
    pub injected_ids: i64,
    pub usage_rows: i64,
    pub tracked_since: Option<String>,
    pub tracked_ids: i64,
    pub opened_ids: i64,
    pub opened_share: Option<f64>,
    pub failed_runs: IndexMap<String, i64>,
    pub fallback_runs: IndexMap<String, i64>,
    pub by_path: IndexMap<&'static str, PathCounts>,
    pub bad_rows: (i64, i64),
}

fn bump(m: &mut IndexMap<String, i64>, k: String) {
    *m.entry(k).or_default() += 1;
}

fn sorted_by_key(m: IndexMap<String, i64>) -> IndexMap<String, i64> {
    let mut v: Vec<(String, i64)> = m.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.into_iter().collect()
}

fn sorted_most_first(m: IndexMap<String, i64>) -> IndexMap<String, i64> {
    let mut v: Vec<(String, i64)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter().collect()
}

/// (recall counts, injections per id never opened).
fn recall(home: &PyPath, since: i64) -> Result<(Recall, IndexMap<String, i64>), SmError> {
    let (hook_rows, hook_bad) = read_rows(&home.join("hook.log"))?;
    let (usage_all, usage_bad) = read_rows(&home.join(USAGE_LOG))?;
    let usage_rows: Vec<&(Ts, PyValue)> = usage_all
        .iter()
        .filter(|(t, _)| t.timestamp_micros() >= since)
        .collect();
    let runs: Vec<&(Ts, PyValue)> = hook_rows
        .iter()
        .filter(|(t, r)| {
            t.timestamp_micros() >= since
                && !matches!(r, PyValue::Dict(d) if dict_get(d, "catch_up_error").is_some())
        })
        .collect();
    let mut injections: Vec<(i64, String, &'static str)> = Vec::new();
    for (t, r) in &runs {
        for i in ids_of(r, "ids")? {
            injections.push((t.timestamp_micros(), i, path_of(r)));
        }
    }
    let mut failed: IndexMap<String, i64> = IndexMap::new();
    for (_, r) in &runs {
        let path_in = matches!(r, PyValue::Dict(d) if dict_get(d, "path").is_some());
        if is_str(get(r, "path"), "error")
            || (!path_in && truthy_opt(get(r, "error")) && !truthy_opt(get(r, "ids")))
        {
            let e = get(r, "error");
            bump(
                &mut failed,
                if is_str(e, "budget") || is_str(e, "lock") {
                    str_get(r, "error")
                } else {
                    "rewrite".into()
                },
            );
        }
    }
    for (_, r) in &runs {
        if truthy_opt(get(r, "local_error")) {
            let e = get(r, "local_error");
            bump(
                &mut failed,
                if is_str(e, "budget") || is_str(e, "lock") {
                    str_get(r, "local_error")
                } else {
                    "local".into()
                },
            );
        }
    }
    let mut fallback: IndexMap<String, i64> = IndexMap::new();
    for (_, r) in &runs {
        if is_str(get(r, "path"), "local") {
            bump(&mut fallback, str_get(r, "fallback"));
        }
    }
    let mut tracked_since: Option<&Ts> = None;
    for (t, _) in &usage_rows {
        if tracked_since.is_none_or(|m| t.timestamp_micros() < m.timestamp_micros()) {
            tracked_since = Some(t);
        }
    }
    let mut shown: HashMap<String, Vec<i64>> = HashMap::new();
    for (t, r) in &usage_rows {
        if is_str(get(r, "cmd"), "show") {
            for i in ids_of(r, "ids")? {
                shown.entry(i).or_default().push(t.timestamp_micros());
            }
        }
    }
    let mut first_tracked: IndexMap<String, (i64, &'static str)> = IndexMap::new();
    let mut tracked_count: IndexMap<String, i64> = IndexMap::new();
    if let Some(ts) = tracked_since {
        let ts = ts.timestamp_micros();
        for (t, i, p) in &injections {
            if *t >= ts {
                first_tracked.entry(i.clone()).or_insert((*t, p));
                *tracked_count.entry(i.clone()).or_default() += 1;
            }
        }
    }
    let opened: std::collections::HashSet<&String> = first_tracked
        .iter()
        .filter(|(i, (first, _))| {
            shown
                .get(*i)
                .is_some_and(|ts| ts.iter().any(|t| t >= first))
        })
        .map(|(i, _)| i)
        .collect();
    let mut by_path = IndexMap::new();
    for path in PATHS {
        let tracked: Vec<&String> = first_tracked
            .iter()
            .filter(|(_, (_, p))| *p == path)
            .map(|(i, _)| i)
            .collect();
        let n_opened = tracked.iter().filter(|i| opened.contains(*i)).count();
        by_path.insert(
            path,
            PathCounts {
                with_injection: runs
                    .iter()
                    .filter(|(_, r)| truthy_opt(get(r, "ids")) && path_of(r) == path)
                    .count() as i64,
                tracked_ids: tracked.len() as i64,
                opened_ids: n_opened as i64,
                opened_share: share(n_opened, tracked.len())?,
            },
        );
    }
    let never: IndexMap<String, i64> = tracked_count
        .into_iter()
        .filter(|(i, _)| !shown.contains_key(i))
        .collect();
    let injected: std::collections::HashSet<&String> =
        injections.iter().map(|(_, i, _)| i).collect();
    Ok((
        Recall {
            hook_runs: runs.len() as i64,
            with_injection: runs
                .iter()
                .filter(|(_, r)| truthy_opt(get(r, "ids")))
                .count() as i64,
            injections: injections.len() as i64,
            injected_ids: injected.len() as i64,
            usage_rows: usage_rows.len() as i64,
            tracked_since: tracked_since.map(isoformat),
            tracked_ids: first_tracked.len() as i64,
            opened_ids: opened.len() as i64,
            opened_share: share(opened.len(), first_tracked.len())?,
            failed_runs: sorted_by_key(failed),
            fallback_runs: sorted_by_key(fallback),
            by_path,
            bad_rows: (hook_bad, usage_bad),
        },
        never,
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatchUp {
    pub runs: i64,
    pub failed: i64,
    pub part_failures: IndexMap<String, i64>,
    /// (ts isoformat, ok)
    pub last: Option<(String, bool)>,
    pub bad_rows: i64,
}

fn catch_up(home: &PyPath, since: i64) -> Result<CatchUp, SmError> {
    let (rows, bad) = read_rows(&home.join("catchup.log"))?;
    let recent: Vec<&(Ts, PyValue)> = rows
        .iter()
        .filter(|(t, _)| t.timestamp_micros() >= since)
        .collect();
    let mut parts: IndexMap<String, i64> = IndexMap::new();
    for (_, r) in &recent {
        let Some(p) = get(r, "parts").filter(|v| truthy(v)) else {
            continue;
        };
        let PyValue::Dict(items) = p else {
            return Err(SmError::from_python(
                "AttributeError",
                format!(
                    "'{}' object has no attribute 'items'",
                    crate::py::value::type_name(p)
                ),
            ));
        };
        for (name, part) in items {
            let ok = matches!(part, PyValue::Dict(_)) && get_attr(part, "ok")?.is_some_and(truthy);
            if !ok {
                bump(&mut parts, name.py_str());
            }
        }
    }
    let is_true = |r: &PyValue| matches!(get(r, "ok"), Some(PyValue::Bool(true)));
    Ok(CatchUp {
        runs: recent.len() as i64,
        failed: recent.iter().filter(|(_, r)| !is_true(r)).count() as i64,
        part_failures: sorted_most_first(parts),
        last: rows.last().map(|(t, r)| (isoformat(t), is_true(r))),
        bad_rows: bad,
    })
}

/// `_never_row`: (id, injections, repo, kind, status); repo and kind are None for a missing id.
pub type NeverRow = (String, i64, Option<String>, Option<String>, String);

/// The dict `run_stats` returns.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub days: f64,
    pub since: String,
    pub store: crate::store::stats::StoreCounts,
    pub stale_total: i64,
    pub stale_repos: IndexMap<String, i64>,
    pub recall: Recall,
    pub never_opened: Vec<NeverRow>,
    pub catch_up: CatchUp,
    pub backups: (i64, Option<f64>),
    /// With `[scopes]`: (path, active items in it or below it) in `config::scopes` order, and
    /// the active items in no path.
    pub scopes: Option<(Vec<(String, i64)>, i64)>,
}

/// `timedelta(days=days)` in microseconds: the int part exactly, the fraction rounded half to
/// even (Modules/_datetimemodule.c `accum`).
fn days_us(days: f64) -> i64 {
    let int = days.trunc();
    (int as i64) * US_PER_DAY + ((days - int) * 86_400_000_000.0).round_ties_even() as i64
}

/// `--days` as typed in an error: `0`, not `0.0`.
fn days_text(days: f64) -> String {
    if days.is_finite() {
        days.to_string()
    } else {
        float_repr(days)
    }
}

/// `run_stats(days=..., home=..., now=...)`: `now` (epoch seconds, default the clock) sets the
/// log window and the stale cutoff.
pub fn run_stats(days: f64, home: Option<&str>, now: Option<f64>) -> Result<Stats, SmError> {
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(days > 0.0) {
        return Err(SmError::value(format!(
            "--days must be above 0, got {}",
            days_text(days)
        )));
    }
    if days > MAX_DAYS as f64 {
        return Err(SmError::value(format!(
            "--days must be at most {MAX_DAYS}, got {}",
            days_text(days)
        )));
    }
    let h = get_home(home)?;
    let now_arg = now;
    let now_dt = match now {
        None => crate::clock::now_utc(),
        Some(t) => crate::clock::from_timestamp(t, crate::clock::utc())
            .ok_or_else(|| SmError::overflow("timestamp out of range for platform time_t"))?,
    };
    let since_us = now_dt.timestamp_micros() - days_us(days);
    let since = chrono::DateTime::from_timestamp_micros(since_us)
        .filter(|t| chrono::Datelike::year(t) >= 1)
        .ok_or_else(|| SmError::overflow("date value out of range"))?;
    let (recall, never) = recall(&h, since_us)?;
    let mut top: Vec<(String, i64)> = never
        .into_iter()
        .filter(|(_, n)| *n >= NEVER_OPENED_MIN)
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(NEVER_OPENED_TOP);
    let conn = connect(get_db_path(Some(h.as_str()))?.as_str(), None)?;
    let store = store_counts(&conn)?;
    let paths = scopes(&load_config(Some(h.as_str()))?)?;
    let active_in = |p: &[&str]| -> Result<i64, SmError> {
        let f = Filters {
            scope: Some(scope_matchers(&paths, p)?),
            ..Filters::default()
        };
        count_scope(&conn, &f)
    };
    let scope_counts = if paths.is_empty() {
        None
    } else {
        let each = paths
            .iter()
            .map(|(p, _)| Ok((p.clone(), active_in(&[p])?)))
            .collect::<Result<Vec<_>, SmError>>()?;
        let all: Vec<&str> = paths.iter().map(|(p, _)| p.as_str()).collect();
        Some((each, store.active - active_in(&all)?))
    };
    let ids: Vec<&String> = top.iter().map(|(i, _)| i).collect();
    let labels = item_labels(&conn, &ids)?;
    drop(conn);
    let age = backup_age_hours(Some(h.as_str()))?;
    let stale = run_stale(Some(h.as_str()), now_arg)?;
    let mut repos: IndexMap<String, i64> = IndexMap::new();
    for r in &stale {
        bump(&mut repos, r.repo.clone());
    }
    let never_opened = top
        .into_iter()
        .map(|(i, n)| match labels.get(&i) {
            Some((repo, kind, status)) => {
                (i, n, Some(repo.clone()), Some(kind.clone()), status.clone())
            }
            None => (i, n, None, None, "missing".to_string()),
        })
        .collect();
    let catch_up = catch_up(&h, since_us)?;
    let count = auto_backups(Some(h.as_str()))?.len() as i64;
    Ok(Stats {
        days,
        since: since.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        store,
        stale_total: stale.len() as i64,
        stale_repos: sorted_most_first(repos),
        recall,
        never_opened,
        catch_up,
        backups: (count, age.map(|a| round_ndigits(a, 1)).transpose()?),
        scopes: scope_counts,
    })
}

fn s(x: &str) -> PyValue {
    PyValue::Str(x.into())
}

fn counts_value<K: AsRef<str>>(m: &IndexMap<K, i64>) -> PyValue {
    PyValue::Dict(
        m.iter()
            .map(|(k, v)| (s(k.as_ref()), PyValue::int(*v)))
            .collect(),
    )
}

fn opt_float(x: Option<f64>) -> PyValue {
    x.map_or(PyValue::None, PyValue::Float)
}

fn opt_str(x: &Option<String>) -> PyValue {
    x.as_deref().map_or(PyValue::None, s)
}

impl Stats {
    /// The dict Python's `run_stats` returns, for `json.dumps(st, ensure_ascii=False)`.
    pub fn value(&self) -> PyValue {
        let st = &self.store;
        let store = PyValue::Dict(vec![
            (s("status"), counts_value(&st.status_breakdown())),
            (s("active"), PyValue::int(st.active)),
            (s("repos"), counts_value(&st.repos)),
            (s("repo_count"), PyValue::int(st.repo_count)),
            (s("kinds"), counts_value(&st.kinds)),
            (s("tags"), counts_value(&st.tags)),
            (s("unassigned"), PyValue::int(st.unassigned)),
            (s("legacy"), PyValue::int(st.legacy)),
        ]);
        let stale = PyValue::Dict(vec![
            (s("older_than_days"), PyValue::int(STALE_DAYS)),
            (
                s("tags"),
                PyValue::List(STALE_TAGS.iter().map(|t| s(t)).collect()),
            ),
            (s("total"), PyValue::int(self.stale_total)),
            (s("repos"), counts_value(&self.stale_repos)),
        ]);
        let r = &self.recall;
        let by_path = PyValue::Dict(
            r.by_path
                .iter()
                .map(|(p, b)| {
                    (
                        s(p),
                        PyValue::Dict(vec![
                            (s("with_injection"), PyValue::int(b.with_injection)),
                            (s("tracked_ids"), PyValue::int(b.tracked_ids)),
                            (s("opened_ids"), PyValue::int(b.opened_ids)),
                            (s("opened_share"), opt_float(b.opened_share)),
                        ]),
                    )
                })
                .collect(),
        );
        let recall = PyValue::Dict(vec![
            (s("hook_runs"), PyValue::int(r.hook_runs)),
            (s("with_injection"), PyValue::int(r.with_injection)),
            (s("injections"), PyValue::int(r.injections)),
            (s("injected_ids"), PyValue::int(r.injected_ids)),
            (s("usage_rows"), PyValue::int(r.usage_rows)),
            (s("tracked_since"), opt_str(&r.tracked_since)),
            (s("tracked_ids"), PyValue::int(r.tracked_ids)),
            (s("opened_ids"), PyValue::int(r.opened_ids)),
            (s("opened_share"), opt_float(r.opened_share)),
            (s("failed_runs"), counts_value(&r.failed_runs)),
            (s("fallback_runs"), counts_value(&r.fallback_runs)),
            (s("by_path"), by_path),
            (
                s("bad_rows"),
                PyValue::Dict(vec![
                    (s("hook.log"), PyValue::int(r.bad_rows.0)),
                    (s(USAGE_LOG), PyValue::int(r.bad_rows.1)),
                ]),
            ),
        ]);
        let never = PyValue::List(
            self.never_opened
                .iter()
                .map(|(i, n, repo, kind, status)| {
                    PyValue::Dict(vec![
                        (s("id"), s(i)),
                        (s("injections"), PyValue::int(*n)),
                        (s("repo"), opt_str(repo)),
                        (s("kind"), opt_str(kind)),
                        (s("status"), s(status)),
                    ])
                })
                .collect(),
        );
        let c = &self.catch_up;
        let last = match &c.last {
            Some((ts, ok)) => PyValue::Dict(vec![(s("ts"), s(ts)), (s("ok"), PyValue::Bool(*ok))]),
            None => PyValue::None,
        };
        let catch_up = PyValue::Dict(vec![
            (s("runs"), PyValue::int(c.runs)),
            (s("failed"), PyValue::int(c.failed)),
            (s("part_failures"), counts_value(&c.part_failures)),
            (s("last"), last),
            (s("bad_rows"), PyValue::int(c.bad_rows)),
        ]);
        let backups = PyValue::Dict(vec![
            (s("count"), PyValue::int(self.backups.0)),
            (s("newest_age_hours"), opt_float(self.backups.1)),
        ]);
        let mut out = vec![
            (s("days"), PyValue::Float(self.days)),
            (s("since"), s(&self.since)),
            (s("store"), store),
            (s("stale"), stale),
            (s("recall"), recall),
            (s("never_opened"), never),
            (s("catch_up"), catch_up),
            (s("backups"), backups),
        ];
        if let Some((paths, outside)) = &self.scopes {
            let paths = paths
                .iter()
                .map(|(p, n)| {
                    PyValue::Dict(vec![(s("path"), s(p)), (s("active"), PyValue::int(*n))])
                })
                .collect();
            out.push((
                s("scopes"),
                PyValue::Dict(vec![
                    (s("paths"), PyValue::List(paths)),
                    (s("outside"), PyValue::int(*outside)),
                ]),
            ));
        }
        PyValue::Dict(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_must_be_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path().to_str();
        assert_eq!(
            run_stats(f64::NAN, h, None).unwrap_err().python_text(),
            "ValueError: --days must be above 0, got nan"
        );
        assert_eq!(
            run_stats(0.0, h, None).unwrap_err().python_text(),
            "ValueError: --days must be above 0, got 0"
        );
        assert_eq!(
            run_stats(36_500.5, h, None).unwrap_err().python_text(),
            "ValueError: --days must be at most 36500, got 36500.5"
        );
    }

    #[test]
    fn read_rows_counts_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hook.log");
        std::fs::write(&p, "{\"ts\": \"2026-01-01T00:00:00+00:00\", \"ids\": [\"a\"]}\n\nnot json\n[1]\n{\"ts\": 5}\n{\"ts\": \"2026-01-01T00:00:00.250000Z\"}\n").unwrap();
        let (rows, bad) = read_rows(&PyPath::new(p.to_str().unwrap())).unwrap();
        assert_eq!((rows.len(), bad), (2, 3));
        assert_eq!(isoformat(&rows[1].0), "2026-01-01T00:00:00.250000+00:00");
    }

    #[test]
    fn ts_reads_the_forms_sm_writes_and_skips_the_rest() {
        let ts = |x: &str| ts_of(Some(&PyValue::Str(x.into())));
        // 2026-01-01T00:00:00Z is epoch 1767225600 (2025-01-01 is 1735689600, plus 365 days)
        for (text, us, iso) in [
            (
                "2026-01-01T00:00:00+00:00",
                1_767_225_600_000_000,
                "2026-01-01T00:00:00+00:00",
            ),
            (
                "2026-01-01T00:00:00Z",
                1_767_225_600_000_000,
                "2026-01-01T00:00:00+00:00",
            ),
            (
                "2026-01-01T00:00:00.000250+00:00",
                1_767_225_600_000_250,
                "2026-01-01T00:00:00.000250+00:00",
            ),
            (
                "2026-01-01T00:00:00.000250Z",
                1_767_225_600_000_250,
                "2026-01-01T00:00:00.000250+00:00",
            ),
            (
                "2026-01-01T00:00:00",
                1_767_225_600_000_000,
                "2026-01-01T00:00:00+00:00",
            ),
            (
                "2026-01-01T05:30:00+05:30",
                1_767_225_600_000_000,
                "2026-01-01T05:30:00+05:30",
            ),
        ] {
            let t = ts(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(
                (t.timestamp_micros(), isoformat(&t).as_str()),
                (us, iso),
                "{text}"
            );
        }
        for bad in [
            "",
            "None",
            "2026-02-30T00:00:00+00:00",
            "2026-01-01T25:00:00Z",
            "yesterday",
        ] {
            assert_eq!(ts(bad), None, "{bad}");
        }
        assert_eq!(ts_of(Some(&PyValue::int(20_260_101))), None);
        assert_eq!(ts_of(None), None);
    }

    #[test]
    fn stats_value_reports_retired_separately_in_store_status() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let conn = connect(&format!("{home}/memory.db"), None).unwrap();
        let a = crate::core::item::Item::bare("m_a", "alpha", "fact", "A", "body a", "decided");
        crate::store::items::put_item(&conn, &a.post_init(), None).unwrap();
        let mut r = crate::core::item::Item::bare("m_r", "alpha", "fact", "R", "body r", "decided");
        r.status = "superseded".into();
        r.tags = "retired:manual".into();
        crate::store::items::put_item(&conn, &r.post_init(), None).unwrap();
        drop(conn);

        let st = run_stats(7.0, Some(home), None).unwrap();
        let json_str =
            crate::py::json::dumps(&st.value(), &crate::py::json::Opts::unicode()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(
            v["store"]["status"],
            serde_json::json!({"active": 1, "superseded": 0, "retired": 1})
        );
        assert_eq!(v["store"]["active"], 1);
    }
}
