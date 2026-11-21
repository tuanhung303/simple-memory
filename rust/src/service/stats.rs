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

// ---------------------------------------------------------------- datetime.fromisoformat

/// An aware datetime: the wall fields and the UTC offset (seconds and microseconds, same sign).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dt {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
    pub off_s: i64,
    pub off_us: i64,
}

const US_PER_DAY: i128 = 86_400_000_000;

impl Dt {
    /// The instant in microseconds since 0001-01-01 UTC, which aware datetimes compare by.
    pub fn instant(&self) -> i128 {
        use chrono::Datelike;
        let date =
            chrono::NaiveDate::from_ymd_opt(self.year, self.month, self.day).expect("a valid date");
        let days = i128::from(date.num_days_from_ce() - 1);
        let wall = days * US_PER_DAY
            + i128::from(self.hour * 3600 + self.minute * 60 + self.second) * 1_000_000
            + i128::from(self.microsecond);
        wall - (i128::from(self.off_s) * 1_000_000 + i128::from(self.off_us))
    }

    /// The datetime at an instant, in UTC.
    pub fn from_instant(us: i128) -> Option<Dt> {
        let days = us.div_euclid(US_PER_DAY);
        let rest = us.rem_euclid(US_PER_DAY);
        let date = chrono::NaiveDate::from_num_days_from_ce_opt(i32::try_from(days + 1).ok()?)?;
        use chrono::Datelike;
        if !(1..=9999).contains(&date.year()) {
            return None;
        }
        let secs = (rest / 1_000_000) as u32;
        Some(Dt {
            year: date.year(),
            month: date.month(),
            day: date.day(),
            hour: secs / 3600,
            minute: secs / 60 % 60,
            second: secs % 60,
            microsecond: (rest % 1_000_000) as u32,
            off_s: 0,
            off_us: 0,
        })
    }

    fn wall(&self, sep: char, micro: bool) -> String {
        let mut s = format!(
            "{:04}-{:02}-{:02}{sep}{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if micro && self.microsecond != 0 {
            s.push_str(&format!(".{:06}", self.microsecond));
        }
        s
    }

    fn offset(&self) -> String {
        let total = i128::from(self.off_s) * 1_000_000 + i128::from(self.off_us);
        let sign = if total < 0 { '-' } else { '+' };
        let a = total.abs();
        let (secs, us) = ((a / 1_000_000) as u64, (a % 1_000_000) as u64);
        let mut s = format!("{sign}{:02}:{:02}", secs / 3600, secs / 60 % 60);
        if secs % 60 != 0 || us != 0 {
            s.push_str(&format!(":{:02}", secs % 60));
            if us != 0 {
                s.push_str(&format!(".{us:06}"));
            }
        }
        s
    }

    /// `isoformat()`.
    pub fn isoformat(&self) -> String {
        format!("{}{}", self.wall('T', true), self.offset())
    }

    /// `isoformat(timespec="seconds")`.
    pub fn isoformat_seconds(&self) -> String {
        format!("{}{}", self.wall('T', false), self.offset())
    }
}

fn digits(b: &[u8], at: usize, n: usize) -> Option<i64> {
    let mut v = 0i64;
    for i in 0..n {
        let c = *b.get(at + i)?;
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + i64::from(c - b'0');
    }
    Some(v)
}

/// `_find_isoformat_datetime_separator` (Modules/_datetimemodule.c), on the UTF-8 bytes.
fn separator_location(b: &[u8], len: usize) -> Option<usize> {
    if len == 7 {
        return Some(7);
    }
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    if at(4) == b'-' {
        if at(5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && at(10).is_ascii_digit() {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if at(4) == b'W' {
        let mut idx = 7;
        while idx < len && at(idx).is_ascii_digit() {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// `parse_isoformat_date` and `iso_to_ymd`: (year, month, day), unchecked except the ISO week.
fn parse_date(b: &[u8], len: usize) -> Option<(i64, i64, i64)> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let year = digits(b, 0, 4)?;
    let mut p = 4;
    let sep = at(p) == b'-';
    if sep {
        p += 1;
    }
    if at(p) == b'W' {
        p += 1;
        let week = digits(b, p, 2)?;
        p += 2;
        let day = if p < len {
            if sep {
                if at(p) != b'-' {
                    return None;
                }
                p += 1;
            }
            digits(b, p, 1)?
        } else {
            1
        };
        if !(1..=9999).contains(&year) {
            return None;
        }
        let jan1 = chrono::NaiveDate::from_ymd_opt(year as i32, 1, 1)?;
        if week <= 0 || week >= 53 {
            use chrono::Datelike;
            let first = jan1.weekday().num_days_from_monday();
            let leap = chrono::NaiveDate::from_ymd_opt(year as i32, 2, 29).is_some();
            if !(week == 53 && (first == 3 || (first == 2 && leap))) {
                return None;
            }
        }
        if !(1..=7).contains(&day) {
            return None;
        }
        use chrono::Datelike;
        let jan4 = chrono::NaiveDate::from_ymd_opt(year as i32, 1, 4)?;
        let monday =
            jan4 - chrono::Duration::days(i64::from(jan4.weekday().num_days_from_monday()));
        let d = monday + chrono::Duration::days((week - 1) * 7 + day - 1);
        return Some((
            i64::from(d.year()),
            i64::from(d.month()),
            i64::from(d.day()),
        ));
    }
    let month = digits(b, p, 2)?;
    p += 2;
    if sep {
        if at(p) != b'-' {
            return None;
        }
        p += 1;
    }
    let day = digits(b, p, 2)?;
    Some((year, month, day))
}

/// `parse_hh_mm_ss_ff(tstr, tstr_end)`: Ok(0) at the end of the string, Ok(1) when bytes are left.
fn parse_hms(b: &[u8], start: usize, end: usize) -> Result<(i32, [i64; 4]), ()> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut vals = [0i64; 4];
    let mut p = start;
    let mut has_sep = true;
    let mut fraction = false;
    for i in 0..3 {
        vals[i] = digits(b, p, 2).ok_or(())?;
        p += 2;
        let c = at(p);
        p += 1;
        if i == 0 {
            has_sep = c == b':';
        }
        if p >= end {
            return Ok((i32::from(c != 0), vals));
        } else if has_sep && c == b':' {
            if i == 2 {
                return Err(());
            }
            continue;
        } else if c == b'.' || c == b',' {
            if i < 2 {
                return Err(());
            }
            fraction = true;
            break;
        } else if !has_sep {
            p -= 1;
        } else {
            return Err(());
        }
    }
    let _ = fraction;
    let to_parse = (end - p).min(6);
    let us = digits(b, p, to_parse).ok_or(())?;
    if to_parse == 0 {
        return Err(());
    }
    vals[3] = us * [100_000, 10_000, 1000, 100, 10, 1][to_parse - 1];
    p += to_parse;
    while at(p).is_ascii_digit() {
        p += 1;
    }
    Ok((i32::from(at(p) != 0), vals))
}

/// `datetime.fromisoformat(s)` (the C implementation of Python 3.15), with a naive result as
/// UTC (`_ts`); None for any string Python rejects.
pub fn fromisoformat_utc(s: &str) -> Option<Dt> {
    // Python 3.15 takes any one code point as the separator, and a non-ASCII code point anywhere
    // else fails; one 0x80 byte per non-ASCII code point keeps byte and code point indexes equal
    let mut b: Vec<u8> = s
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { 0x80 })
        .collect();
    let len = b.len();
    b.extend_from_slice(&[0; 16]);
    let sep = separator_location(&b, len)?;
    let (mut y, mut mo, mut d) = parse_date(&b, sep)?;
    let (mut h, mut mi, mut se, mut us) = (0i64, 0i64, 0i64, 0i64);
    let mut tz: Option<(i64, i64)> = None;
    if len > sep {
        let start = sep + 1;
        let end = len;
        let mut tzpos = start;
        loop {
            let c = b[tzpos];
            if c == b'Z' || c == b'+' || c == b'-' {
                break;
            }
            tzpos += 1;
            if tzpos >= end {
                break;
            }
        }
        let (rv, v) = parse_hms(&b, start, tzpos).ok()?;
        [h, mi, se, us] = v;
        if tzpos >= end {
            if rv == 1 && tzpos == end {
                return None;
            }
            if tzpos > end {
                return None;
            }
        } else if b[tzpos] == b'Z' {
            if b[tzpos + 1] != 0 {
                return None;
            }
            tz = Some((0, 0));
        } else {
            let sign = if b[tzpos] == b'-' { -1 } else { 1 };
            let (rv, t) = parse_hms(&b, tzpos + 1, end).ok()?;
            if rv != 0 {
                return None;
            }
            tz = Some((sign * (t[0] * 3600 + t[1] * 60 + t[2]), sign * t[3]));
        }
    }
    if h == 24 && (1..=12).contains(&mo) {
        // the month and the top of the day range are checked first; day 0 becomes day 1
        let dim = i64::from(crate::py::value::days_in_month(y as i32, mo as u32));
        if d <= dim {
            if mi != 0 || se != 0 || us != 0 {
                return None;
            }
            h = 0;
            d += 1;
            if d > dim {
                d = 1;
                mo += 1;
                if mo > 12 {
                    mo = 1;
                    y += 1;
                }
            }
        }
    }
    if !(1..=9999).contains(&y) || !(1..=12).contains(&mo) {
        return None;
    }
    let dim = i64::from(crate::py::value::days_in_month(y as i32, mo as u32));
    if !(1..=dim).contains(&d)
        || !(0..24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..60).contains(&se)
    {
        return None;
    }
    let (off_s, off_us) = tz.unwrap_or((0, 0));
    if (off_s * 1_000_000 + off_us).abs() >= 86_400_000_000 {
        return None;
    }
    Some(Dt {
        year: y as i32,
        month: mo as u32,
        day: d as u32,
        hour: h as u32,
        minute: mi as u32,
        second: se as u32,
        microsecond: us as u32,
        off_s,
        off_us,
    })
}

/// `datetime.fromisoformat(str(value))`, naive read as UTC; None when invalid.
pub fn ts_of(value: Option<&PyValue>) -> Option<Dt> {
    fromisoformat_utc(&value.map_or_else(|| "None".to_string(), PyValue::py_str))
}

// ---------------------------------------------------------------- logs

type Rows = Vec<(Dt, PyValue)>;

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
            PyValue::Dict(d) => ts_of(Some(dict_get(d, "ts").unwrap_or(&PyValue::None))),
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
fn recall(home: &PyPath, since: i128) -> Result<(Recall, IndexMap<String, i64>), SmError> {
    let (hook_rows, hook_bad) = read_rows(&home.join("hook.log"))?;
    let (usage_all, usage_bad) = read_rows(&home.join(USAGE_LOG))?;
    let usage_rows: Vec<&(Dt, PyValue)> = usage_all
        .iter()
        .filter(|(t, _)| t.instant() >= since)
        .collect();
    let runs: Vec<&(Dt, PyValue)> = hook_rows
        .iter()
        .filter(|(t, r)| {
            t.instant() >= since
                && !matches!(r, PyValue::Dict(d) if dict_get(d, "catch_up_error").is_some())
        })
        .collect();
    let mut injections: Vec<(i128, String, &'static str)> = Vec::new();
    for (t, r) in &runs {
        for i in ids_of(r, "ids")? {
            injections.push((t.instant(), i, path_of(r)));
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
    let mut tracked_since: Option<&Dt> = None;
    for (t, _) in &usage_rows {
        if tracked_since.is_none_or(|m| t.instant() < m.instant()) {
            tracked_since = Some(t);
        }
    }
    let mut shown: HashMap<String, Vec<i128>> = HashMap::new();
    for (t, r) in &usage_rows {
        if is_str(get(r, "cmd"), "show") {
            for i in ids_of(r, "ids")? {
                shown.entry(i).or_default().push(t.instant());
            }
        }
    }
    let mut first_tracked: IndexMap<String, (i128, &'static str)> = IndexMap::new();
    let mut tracked_count: IndexMap<String, i64> = IndexMap::new();
    if let Some(ts) = tracked_since {
        let ts = ts.instant();
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
            tracked_since: tracked_since.map(Dt::isoformat),
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

fn catch_up(home: &PyPath, since: i128) -> Result<CatchUp, SmError> {
    let (rows, bad) = read_rows(&home.join("catchup.log"))?;
    let recent: Vec<&(Dt, PyValue)> = rows.iter().filter(|(t, _)| t.instant() >= since).collect();
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
        last: rows.last().map(|(t, r)| (t.isoformat(), is_true(r))),
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
fn days_us(days: f64) -> i128 {
    let int = days.trunc();
    (int as i128) * US_PER_DAY + ((days - int) * 86_400_000_000.0).round_ties_even() as i128
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
    use chrono::{Datelike, Timelike};
    let now = Dt {
        year: now_dt.year(),
        month: now_dt.month(),
        day: now_dt.day(),
        hour: now_dt.hour(),
        minute: now_dt.minute(),
        second: now_dt.second(),
        microsecond: now_dt.nanosecond() / 1000,
        off_s: 0,
        off_us: 0,
    };
    let since_us = now.instant() - days_us(days);
    let since =
        Dt::from_instant(since_us).ok_or_else(|| SmError::overflow("date value out of range"))?;
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
        since: since.isoformat_seconds(),
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

    fn iso(s: &str) -> Option<String> {
        fromisoformat_utc(s).map(|d| d.isoformat())
    }

    #[test]
    fn fromisoformat_accepts_the_forms_python_accepts() {
        assert_eq!(
            iso("2026-11-05T07:54:00+05:30").as_deref(),
            Some("2026-11-05T07:54:00+05:30")
        );
        assert_eq!(
            iso("2026-11-05").as_deref(),
            Some("2026-11-05T00:00:00+00:00")
        );
        assert_eq!(
            iso("20261105T0754Z").as_deref(),
            Some("2026-11-05T07:54:00+00:00")
        );
        assert_eq!(
            iso("2026-11-05 07:54:00.5").as_deref(),
            Some("2026-11-05T07:54:00.500000+00:00")
        );
        assert_eq!(iso("2026W01").as_deref(), Some("2025-12-29T00:00:00+00:00"));
        assert_eq!(
            iso("2026-12-31T24:00").as_deref(),
            Some("2027-01-01T00:00:00+00:00")
        );
        assert_eq!(
            iso("2026-01-01T00:00-05:30:15").as_deref(),
            Some("2026-01-01T00:00:00-05:30:15")
        );
        // Python 3.15.1 gives each of these
        assert_eq!(
            iso("2026-11-05é07:54").as_deref(),
            Some("2026-11-05T07:54:00+00:00")
        );
        assert_eq!(
            iso("2026W013😀07").as_deref(),
            Some("2025-12-31T07:00:00+00:00")
        );
        assert_eq!(
            iso("2026-01-00T24:00").as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(
            iso("0000-12-31T24:00").as_deref(),
            Some("0001-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn fromisoformat_rejects_what_python_rejects() {
        for bad in [
            "",
            "None",
            "2026-1-01",
            "2026-02-30",
            "2026-01-01T24:01",
            "2026-01-01T12:",
            "2026-01-01T1",
            "2026-01-01T00:00+24:00",
            "2026-00-00T24:00",
            "2026-00-28T24:00",
            "2026-13-28T24:00",
            "2026-11-05éé07:54",
            "2026-11é05",
            "2026-11-05T07:54é",
        ] {
            assert_eq!(iso(bad), None, "{bad}");
        }
    }

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
        std::fs::write(&p, "{\"ts\": \"2026-01-01T00:00:00+00:00\", \"ids\": [\"a\"]}\n\nnot json\n[1]\n{\"ts\": 5}\n{\"ts\": 20260101}\n").unwrap();
        let (rows, bad) = read_rows(&PyPath::new(p.to_str().unwrap())).unwrap();
        assert_eq!((rows.len(), bad), (2, 3));
        assert_eq!(rows[1].0.isoformat(), "2026-01-01T00:00:00+00:00");
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
