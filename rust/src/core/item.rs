use std::sync::LazyLock;

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone,
    Timelike,
};
use regex::Regex;
#[cfg(any(test, feature = "parity"))]
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::clock;
use crate::error::SmError;
#[cfg(any(test, feature = "parity"))]
use crate::parity::Call;
use crate::py::str::{ci_literal, repr, strip, translate};
use crate::py::tables;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `generate_ulid(ts_ms)`: 10 chars of milliseconds, 16 of `os.urandom(10)`, Crockford base32.
/// Python shifts an unbounded int; i128 holds every value sm can pass.
pub fn generate_ulid(ts_ms: Option<i128>) -> String {
    let t = ts_ms.unwrap_or_else(|| (clock::now_epoch() * 1000.0) as i128);
    let r = clock::urandom(10)
        .iter()
        .fold(0u128, |acc, &b| (acc << 8) | b as u128);
    let mut out = String::with_capacity(26);
    for i in 0..10 {
        out.push(CROCKFORD[((t >> (45 - 5 * i)) & 31) as usize] as char);
    }
    for i in 0..16 {
        out.push(CROCKFORD[((r >> (75 - 5 * i)) & 31) as usize] as char);
    }
    out
}

/// `new_item_id(ts_ms)`.
pub fn new_item_id(ts_ms: Option<i128>) -> String {
    format!("m_{}", generate_ulid(ts_ms))
}

/// `compute_content_hash(body)`: sha256 hex of the stripped body.
pub fn compute_content_hash(body: &str) -> String {
    Sha256::digest(strip(body).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn iso_seconds(dt: &DateTime<FixedOffset>) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        offset_text(dt.offset().local_minus_utc())
    )
}

/// `utcoffset` as isoformat writes it, with seconds only when they are not 0.
fn offset_text(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    if a.is_multiple_of(60) {
        format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60)
    } else {
        format!("{sign}{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60)
    }
}

/// `now_iso()`: current local time formatted to whole seconds with local UTC offset.
pub fn now_iso() -> String {
    iso_seconds(&clock::now_local())
}

static ISO_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(
        r"^(\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?)(?:\s+.*)?(?:\n?\z)",
    ))
    .expect("ISO prefix pattern compiles")
});
static ISO_FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(
        r"^\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?(?:\n?\z)",
    ))
    .expect("ISO full pattern compiles")
});
// re.IGNORECASE as sre's case sets (py::str::ci_literal).
static AS_OF_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(&format!(r"^{}\s+", ci_literal("as_of"))))
        .expect("as_of pattern compiles")
});
static DATE_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&translate(r"\A\d{4}-\d{2}-\d{2}\z")).expect("date pattern compiles")
});

/// The decimal value of a char `\d` matches: Nd chars come in runs of ten from 0, and the
/// generated ranges are unions of whole runs.
fn digit_value(c: char) -> Option<u32> {
    let cp = c as u32;
    tables::RE_DIGIT
        .iter()
        .find(|&&(a, b)| a <= cp && cp <= b)
        .map(|&(a, _)| (cp - a) % 10)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `datetime.strptime(s, "%Y-%m-%d")` for an s that fullmatches `\d{4}-\d{2}-\d{2}`: `%Y` takes
/// any four `\d` (Unicode) digits, `%m` only ASCII `1[0-2]|0[1-9]`, `%d` `3[01]|[12]\d|0[1-9]`.
fn strptime_date(s: &str) -> Option<NaiveDate> {
    let c: Vec<char> = s.chars().collect();
    let year = c[..4]
        .iter()
        .try_fold(0i32, |acc, &d| digit_value(d).map(|v| acc * 10 + v as i32))?;
    let month = match (c[5], c[6]) {
        ('1', m @ '0'..='2') => 10 + m.to_digit(10)?,
        ('0', m @ '1'..='9') => m.to_digit(10)?,
        _ => return None,
    };
    let day = match (c[8], c[9]) {
        ('3', d @ '0'..='1') => 30 + d.to_digit(10)?,
        (t @ '1'..='2', d) => t.to_digit(10)? * 10 + digit_value(d)?,
        ('0', d @ '1'..='9') => d.to_digit(10)?,
        _ => return None,
    };
    if !(1..=9999).contains(&year) {
        return None;
    }
    // from_ymd_opt rejects a day past the month's end.
    NaiveDate::from_ymd_opt(year, month, day)
}

/// `datetime.fromisoformat(s)` for an s the ISO patterns passed (a date and a time): ASCII digits
/// only, hour 24 only as `24:00[:00[.0]]` (the next midnight), fraction digits past six dropped,
/// and an offset below 24 h with minutes below 60. Returns the naive time and its offset.
fn fromisoformat(s: &str) -> Option<(NaiveDateTime, Option<i32>)> {
    if !s.is_ascii() || s.len() < 16 {
        return None;
    }
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let t = s.get(r)?;
        if t.bytes().all(|x| x.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0..4)? as i32, num(5..7)?, num(8..10)?);
    let (hour, minute) = (num(11..13)?, num(14..16)?);
    let mut p = 16;
    let (mut second, mut micro) = (0, 0);
    if b.get(p) == Some(&b':') {
        second = num(p + 1..p + 3)?;
        p += 3;
        if b.get(p) == Some(&b'.') {
            let start = p + 1;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            let digits = &s[start..end];
            let kept = &digits[..digits.len().min(6)];
            micro = kept.parse::<u32>().ok()? * 10u32.pow(6 - kept.len() as u32);
            p = end;
        }
    }
    let offset = match b.get(p) {
        None => None,
        Some(&sign @ (b'+' | b'-')) => {
            let rest = &s[p + 1..];
            let (h, m) = match rest.len() {
                5 if rest.as_bytes()[2] == b':' => (num(p + 1..p + 3)?, num(p + 4..p + 6)?),
                4 => (num(p + 1..p + 3)?, num(p + 3..p + 5)?),
                _ => return None,
            };
            if m >= 60 || h >= 24 {
                return None;
            }
            let secs = (h * 3600 + m * 60) as i32;
            Some(if sign == b'-' { -secs } else { secs })
        }
        _ => return None,
    };
    if minute > 59 || second > 59 {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    let naive = if hour == 24 {
        // CPython rolls 24:00 into the next day before it checks the date, so day 0 becomes
        // day 1 and 0000-12-31 becomes 0001-01-01 (a CPython quirk, kept).
        if minute != 0 || second != 0 || micro != 0 {
            return None;
        }
        let dim = days_in_month(year, month);
        let (y, m, d) = if day > dim {
            return None;
        } else if day < dim {
            (year, month, day + 1)
        } else if month == 12 {
            (year + 1, 1, 1)
        } else {
            (year, month + 1, 1)
        };
        if !(1..=9999).contains(&y) {
            return None;
        }
        NaiveDate::from_ymd_opt(y, m, d)?.and_time(NaiveTime::MIN)
    } else if hour > 23 || year < 1 || day < 1 || day > days_in_month(year, month) {
        return None;
    } else {
        NaiveDate::from_ymd_opt(year, month, day)?.and_hms_micro_opt(hour, minute, second, micro)?
    };
    Some((naive, offset))
}

fn in_range(dt: &NaiveDateTime) -> bool {
    (1..=9999).contains(&dt.year())
}

/// Convert `naive` from `offset` seconds east of UTC to local time through UTC, rejecting
/// years outside `1..=9999`.
fn to_local(naive: NaiveDateTime, offset: i32) -> Result<(NaiveDateTime, i32), SmError> {
    let overflow = || SmError::overflow("date value out of range");
    let utc = naive
        .checked_sub_signed(Duration::seconds(offset as i64))
        .filter(in_range)
        .ok_or_else(overflow)?;
    let local_off = clock::local_offset_seconds_at(utc.and_utc().timestamp());
    let local = utc
        .checked_add_signed(Duration::seconds(local_off as i64))
        .filter(in_range)
        .ok_or_else(overflow)?;
    Ok((local, local_off))
}

fn local_iso_text(naive: NaiveDateTime, offset: i32) -> String {
    let tz = FixedOffset::east_opt(offset).unwrap_or_else(clock::utc);
    iso_seconds(
        &tz.from_local_datetime(&naive)
            .single()
            .expect("a fixed offset maps every time"),
    )
}

/// `validate_as_of(value, assume_utc=, allow_prefix=)`.
pub fn validate_as_of(
    value: Option<&str>,
    assume_utc: bool,
    allow_prefix: bool,
) -> Result<String, SmError> {
    let value = match value {
        Some(v) if !strip(v).is_empty() => v,
        _ => return Err(SmError::value("Invalid ISO 8601 timestamp: empty value")),
    };
    let invalid = || SmError::value(format!("Invalid ISO 8601 timestamp {}", repr(value)));
    let s0 = strip(value);
    let replaced = AS_OF_WORD_RE.replace(s0, "");
    let mut s: String = strip(&replaced).to_string();
    if allow_prefix {
        let Some(m) = ISO_PREFIX_RE.captures(&s) else {
            return Err(invalid());
        };
        s = m[1].to_string();
    } else if !ISO_FULL_RE.is_match(&s) {
        return Err(invalid());
    }

    if DATE_ONLY_RE.is_match(&s) {
        let d = strptime_date(&s).ok_or_else(invalid)?;
        let naive = d.and_time(NaiveTime::MIN);
        let off = clock::local_offset_for_naive_local(naive.and_utc().timestamp());
        return Ok(local_iso_text(naive, off));
    }

    if let Some(head) = s.strip_suffix('Z') {
        s = format!("{head}+00:00");
    }
    let (naive, offset) = fromisoformat(&s).ok_or_else(invalid)?;
    let (local, local_off) = match offset {
        // Naive timestamps stay at local wall clock unless `assume_utc` converts from UTC.
        None if !assume_utc => {
            let off = clock::local_offset_for_naive_local(naive.and_utc().timestamp());
            (naive, off)
        }
        None => to_local(naive, 0)?,
        Some(off) => to_local(naive, off)?,
    };
    Ok(local_iso_text(local, local_off))
}

/// `parse_as_of_epoch_opt(value)`: `Ok(None)` for empty or unparseable values; preserves `0.0`
/// (`1970-01-01T00:00:00Z`) and negative pre-1970 epochs; `OverflowError` passes through.
pub fn parse_as_of_epoch_opt(value: Option<&str>) -> Result<Option<f64>, SmError> {
    let Some(v) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    match validate_as_of(Some(v), false, true) {
        Ok(norm) => {
            let dt =
                DateTime::parse_from_rfc3339(&norm).expect("validate_as_of gives an RFC 3339 time");
            Ok(Some(dt.timestamp() as f64))
        }
        Err(e) if e.is(crate::error::Base::Value) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `parse_as_of_epoch(value)`: 0.0 for an empty or invalid value; OverflowError passes through.
pub fn parse_as_of_epoch(value: Option<&str>) -> Result<f64, SmError> {
    Ok(parse_as_of_epoch_opt(value)?.unwrap_or(0.0))
}

fn local_naive_epoch(naive: NaiveDateTime) -> f64 {
    let off = clock::local_offset_for_naive_local(naive.and_utc().timestamp());
    let tz = FixedOffset::east_opt(off).unwrap_or_else(clock::utc);
    tz.from_local_datetime(&naive)
        .single()
        .expect("a fixed offset maps every time")
        .timestamp() as f64
}

/// Parse a `--since` (`end_of_day = false`) or `--until` (`end_of_day = true`) ISO 8601 bound.
/// Returns `(epoch_secs, exclusive)`: date-only `--until` advances to next local midnight with
/// `exclusive = true` (`< ?`), except `9999-12-31` which uses `23:59:59` local with `exclusive = false`.
pub fn parse_date_bound(value: &str, end_of_day: bool) -> Result<(f64, bool), SmError> {
    let norm = validate_as_of(Some(value), false, false)?;
    let s0 = strip(value);
    let replaced = AS_OF_WORD_RE.replace(s0, "");
    let stripped = strip(&replaced);
    if end_of_day && DATE_ONLY_RE.is_match(stripped) {
        let d = strptime_date(stripped).expect("validate_as_of already validated date-only input");
        if let Some(next_day) = d.succ_opt().filter(|n| n.year() <= 9999) {
            return Ok((local_naive_epoch(next_day.and_time(NaiveTime::MIN)), true));
        }
        let eod = d.and_hms_opt(23, 59, 59).expect("23:59:59 is a valid time");
        return Ok((local_naive_epoch(eod), false));
    }
    let dt = DateTime::parse_from_rfc3339(&norm).expect("validate_as_of gives an RFC 3339 time");
    Ok((dt.timestamp() as f64, false))
}

/// Validate optional `--since` and `--until` bounds, returning `(since_epoch, until_epoch, until_exclusive)`.
pub fn validate_date_range(
    since: Option<&str>,
    until: Option<&str>,
) -> Result<(Option<f64>, Option<f64>, bool), SmError> {
    let since_ep = match since.filter(|s| !s.is_empty()) {
        Some(v) => Some(parse_date_bound(v, false)?.0),
        None => None,
    };
    let (until_ep, until_excl) = match until.filter(|s| !s.is_empty()) {
        Some(v) => {
            let (ep, excl) = parse_date_bound(v, true)?;
            (Some(ep), excl)
        }
        None => (None, false),
    };
    if let (Some(s), Some(u)) = (since_ep, until_ep)
        && ((until_excl && s >= u) || (!until_excl && s > u))
    {
        return Err(SmError::value("--since cannot be after --until"));
    }
    Ok((since_ep, until_ep, until_excl))
}

/// `datetime.fromtimestamp(ts, tz=local)` with CPython-style bounds checks.
fn fromtimestamp_local(ts: f64) -> Result<(NaiveDateTime, i32), SmError> {
    if ts.is_nan() {
        return Err(SmError::value("Invalid value NaN (not a number)"));
    }
    // _PyTime_DoubleToDenominator: modf, scale, round half to even, carry.
    let mut intpart = ts.trunc();
    let mut frac = ((ts - intpart) * 1e6).round_ties_even();
    if frac >= 1e6 {
        frac -= 1e6;
        intpart += 1.0;
    } else if frac < 0.0 {
        frac += 1e6;
        intpart -= 1.0;
    }
    // time_t is i64: [-2^63, 2^63).
    if !(-(2f64.powi(63))..2f64.powi(63)).contains(&intpart) {
        return Err(SmError::overflow(
            "timestamp out of range for platform time_t",
        ));
    }
    let secs = intpart as i64;
    let days = secs.div_euclid(86400);
    let year = civil_year(days);
    if year - 1900 > i32::MAX as i64 || year - 1900 < i32::MIN as i64 {
        return Err(SmError::from_python(
            "OSError",
            "[Errno 84] Value too large to be stored in data type",
        ));
    }
    if !(1..=9999).contains(&year) {
        return Err(SmError::value(format!(
            "year must be in 1..9999, not {year}"
        )));
    }
    let utc = DateTime::from_timestamp(secs, frac as u32 * 1000)
        .expect("a year in 1..9999 is a valid time")
        .naive_utc();
    let local_off = clock::local_offset_seconds_at(utc.and_utc().timestamp());
    let local = utc
        .checked_add_signed(Duration::seconds(local_off as i64))
        .filter(in_range)
        .ok_or_else(|| SmError::overflow("date value out of range"))?;
    Ok((local, local_off))
}

/// The proleptic Gregorian year of a day count from 1970-01-01 (Hinnant's civil_from_days).
fn civil_year(days: i64) -> i64 {
    let z = days as i128 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i128::from(m <= 2)) as i64
}

/// `epoch_to_as_of(ts)`.
pub fn epoch_to_as_of(ts: f64) -> Result<String, SmError> {
    let (local, off) = fromtimestamp_local(ts)?;
    Ok(local_iso_text(local, off))
}

/// The Item dataclass, fields in Python's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub repo: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tag: String,
    pub refs: String,
    pub tags: String,
    pub source: String,
    pub evidence: String,
    pub as_of: String,
    pub status: String,
    pub superseded_by: Option<String>,
    pub content_hash: String,
    pub supersedes: Vec<String>,
}

#[cfg(test)]
const FIELDS: [&str; 15] = [
    "id",
    "repo",
    "kind",
    "title",
    "body",
    "tag",
    "refs",
    "tags",
    "source",
    "evidence",
    "as_of",
    "status",
    "superseded_by",
    "content_hash",
    "supersedes",
];
#[cfg(test)]
const REQUIRED: usize = 6;

impl Item {
    /// The fields as `Item(id, repo, kind, title, body, tag)` sets them before `__post_init__`;
    /// set other fields with struct update syntax, then call `post_init`.
    pub fn bare(id: &str, repo: &str, kind: &str, title: &str, body: &str, tag: &str) -> Item {
        Item {
            id: id.into(),
            repo: repo.into(),
            kind: kind.into(),
            title: title.into(),
            body: body.into(),
            tag: tag.into(),
            refs: String::new(),
            tags: String::new(),
            source: String::new(),
            evidence: String::new(),
            as_of: String::new(),
            status: "active".into(),
            superseded_by: None,
            content_hash: String::new(),
            supersedes: Vec::new(),
        }
    }

    /// `__post_init__`: an empty as_of becomes now, an empty content_hash the hash of the body.
    pub fn post_init(mut self) -> Item {
        if self.as_of.is_empty() {
            self.as_of = now_iso();
        }
        if self.content_hash.is_empty() {
            self.content_hash = compute_content_hash(&self.body);
        }
        self
    }

    /// `Item(id, repo, kind, title, body, tag)` with every other field at its default.
    #[cfg(test)]
    pub fn new(id: &str, repo: &str, kind: &str, title: &str, body: &str, tag: &str) -> Item {
        Item::bare(id, repo, kind, title, body, tag).post_init()
    }

    /// `Item(*args, **kwargs)` from a parity call, with the TypeErrors Python's `__init__` raises.
    #[cfg(test)]
    pub fn from_call(call: &Call) -> Result<Item, SmError> {
        let name = "Item.__init__()";
        if call.args.len() > FIELDS.len() {
            return Err(SmError::type_(format!(
                "{name} takes from {} to {} positional arguments but {} were given",
                REQUIRED + 1,
                FIELDS.len() + 1,
                call.args.len() + 1
            )));
        }
        let mut vals: Vec<Option<&Value>> = vec![None; FIELDS.len()];
        for (i, v) in call.args.iter().enumerate() {
            vals[i] = Some(v);
        }
        for (k, v) in &call.kwargs {
            let Some(i) = FIELDS.iter().position(|f| f == k) else {
                return Err(SmError::type_(format!(
                    "{name} got an unexpected keyword argument {}",
                    repr(k)
                )));
            };
            if vals[i].is_some() {
                return Err(SmError::type_(format!(
                    "{name} got multiple values for argument {}",
                    repr(k)
                )));
            }
            vals[i] = Some(v);
        }
        let missing: Vec<String> = (0..REQUIRED)
            .filter(|&i| vals[i].is_none())
            .map(|i| repr(FIELDS[i]))
            .collect();
        if !missing.is_empty() {
            let list = match missing.len() {
                1 => missing[0].clone(),
                2 => format!("{} and {}", missing[0], missing[1]),
                n => format!("{}, and {}", missing[..n - 1].join(", "), missing[n - 1]),
            };
            let noun = if missing.len() == 1 {
                "argument"
            } else {
                "arguments"
            };
            return Err(SmError::type_(format!(
                "{name} missing {} required positional {noun}: {list}",
                missing.len()
            )));
        }
        let text = |i: usize| -> Result<Option<String>, SmError> {
            match vals[i] {
                None => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(Value::Null) if FIELDS[i] == "superseded_by" => Ok(None),
                Some(v) => Err(SmError::type_(format!(
                    "parity: Item.{} must be a str, got {v}",
                    FIELDS[i]
                ))),
            }
        };
        let req = |i: usize| text(i).map(|v| v.expect("required fields are present"));
        let mut it = Item::bare(&req(0)?, &req(1)?, &req(2)?, &req(3)?, &req(4)?, &req(5)?);
        for (i, slot) in [
            (6, &mut it.refs),
            (7, &mut it.tags),
            (8, &mut it.source),
            (9, &mut it.evidence),
            (10, &mut it.as_of),
            (11, &mut it.status),
            (13, &mut it.content_hash),
        ] {
            if let Some(v) = text(i)? {
                *slot = v;
            }
        }
        it.superseded_by = text(12)?;
        if let Some(v) = vals[14] {
            let Value::Array(a) = v else {
                return Err(SmError::type_("parity: Item.supersedes must be a list"));
            };
            it.supersedes = a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(String::from)
                        .ok_or_else(|| SmError::type_("parity: supersedes holds strs"))
                })
                .collect::<Result<_, _>>()?;
        }
        Ok(it.post_init())
    }
}

#[cfg(any(test, feature = "parity"))]
pub fn parity(f: &str, call: &Call) -> Option<crate::parity::Reply> {
    match f {
        "core.item.validate_as_of" => {
            let val = call.args.first().and_then(|v| v.as_str());
            let assume_utc = call.args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
            let allow_prefix = call.args.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            Some(validate_as_of(val, assume_utc, allow_prefix).map(Value::String))
        }
        "core.item.epoch_to_as_of" => {
            let ts = call.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            Some(epoch_to_as_of(ts).map(Value::String))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_shape_and_order() {
        let u1 = generate_ulid(Some(1700000000000));
        let u2 = generate_ulid(Some(1700000001000));
        assert_eq!(u1.len(), 26);
        assert_eq!(u2.len(), 26);
        assert!(u1[..10] < u2[..10]);
        // 1700000000000 ms in Crockford base32, from Python's generate_ulid.
        assert_eq!(&u1[..10], "01HF7YAT00");
        let mid = new_item_id(Some(1700000000000));
        assert!(mid.starts_with("m_") && mid.len() == 28);
    }

    fn utc_to_local_iso(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> String {
        let utc = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, ss)
            .unwrap();
        let off = clock::local_offset_seconds_at(utc.and_utc().timestamp());
        local_iso_text(utc + Duration::seconds(off as i64), off)
    }

    fn naive_local_iso(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> String {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, ss)
            .unwrap();
        let off = clock::local_offset_seconds_at(naive.and_utc().timestamp());
        local_iso_text(naive, off)
    }

    #[test]
    fn validate_as_of_cases_from_python_tests() {
        assert_eq!(
            validate_as_of(Some("2026-10-11 02:04:20"), true, false).unwrap(),
            utc_to_local_iso(2026, 10, 11, 2, 4, 20)
        );
        assert_eq!(
            validate_as_of(Some("2026-10-17T21:44:33Z"), false, false).unwrap(),
            utc_to_local_iso(2026, 10, 17, 21, 44, 33)
        );
        assert_eq!(
            validate_as_of(Some("2026-09-09"), false, false).unwrap(),
            naive_local_iso(2026, 9, 9, 0, 0, 0)
        );
        assert_eq!(
            validate_as_of(Some("2026-10-18T10:10:00Z (18:10 local)"), false, true).unwrap(),
            utc_to_local_iso(2026, 10, 18, 10, 10, 0)
        );
        for bad in ["not-a-timestamp", "2026-99-99"] {
            let e = validate_as_of(Some(bad), false, false).unwrap_err();
            assert_eq!(
                e.python_text(),
                format!("ValueError: Invalid ISO 8601 timestamp '{bad}'")
            );
        }
        assert!(validate_as_of(Some("2026-10-18T10:10:00Z (extra)"), false, false).is_err());
        assert_eq!(
            validate_as_of(Some("   "), false, false).unwrap_err().msg,
            "Invalid ISO 8601 timestamp: empty value"
        );
    }

    #[test]
    fn validate_as_of_python_quirks() {
        // strptime takes Unicode digits in %Y and in %d's second place, fromisoformat takes 24:00,
        // and astimezone with an explicit offset goes through UTC.
        let v =
            |s: &str, utc: bool| validate_as_of(Some(s), utc, false).map_err(|e| e.python_text());
        assert_eq!(
            v("２０２６-01-01", false).unwrap(),
            naive_local_iso(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(
            v("2026-01-1٥", false).unwrap(),
            naive_local_iso(2026, 1, 15, 0, 0, 0)
        );
        assert!(v("2026-1٣-01", false).is_err());
        assert!(v("2026-02-29", false).is_err());
        assert_eq!(
            v("0000-01-01", false).unwrap_err(),
            "ValueError: Invalid ISO 8601 timestamp '0000-01-01'"
        );
        assert_eq!(
            v("2024-02-29", false).unwrap(),
            naive_local_iso(2024, 2, 29, 0, 0, 0)
        );
        assert_eq!(
            v("2026-10-10T24:00", true).unwrap(),
            utc_to_local_iso(2026, 10, 11, 0, 0, 0)
        );
        assert!(v("9999-12-31T24:00", false).is_err());
        let off_y1 = clock::local_offset_seconds_at(-62_135_596_800);
        if off_y1 >= 0 {
            assert_eq!(
                v("0001-01-00T24:00-00:00", false).unwrap(),
                utc_to_local_iso(1, 1, 1, 0, 0, 0)
            );
        } else {
            assert_eq!(
                v("0001-01-00T24:00-00:00", false).unwrap_err(),
                "OverflowError: date value out of range"
            );
        }
        assert_eq!(
            v("0000-12-31T24:00", false).unwrap(),
            naive_local_iso(1, 1, 1, 0, 0, 0)
        );
        assert!(v("2026-02-29T24:00", false).is_err());
        assert_eq!(
            v("2024-02-29T24:00", false).unwrap(),
            naive_local_iso(2024, 3, 1, 0, 0, 0)
        );
        assert_eq!(
            v("2026-10-10T10:00+23:59", false).unwrap(),
            utc_to_local_iso(2026, 10, 9, 10, 1, 0)
        );
        assert!(v("2026-10-10T10:00+05:99", false).is_err());
        assert_eq!(
            v("0001-01-01T00:00+05:00", false).unwrap_err(),
            "OverflowError: date value out of range"
        );
        assert_eq!(
            v("9999-12-31T23:00-05:00", false).unwrap_err(),
            "OverflowError: date value out of range"
        );
        assert_eq!(
            v("0001-01-01T00:00", false).unwrap(),
            naive_local_iso(1, 1, 1, 0, 0, 0)
        );
        let off_y9999 = clock::local_offset_seconds_at(253_402_286_400);
        if off_y9999 >= 4 * 3600 {
            assert_eq!(
                v("9999-12-31T20:00", true).unwrap_err(),
                "OverflowError: date value out of range"
            );
        } else {
            assert_eq!(
                v("9999-12-31T20:00", true).unwrap(),
                utc_to_local_iso(9999, 12, 31, 20, 0, 0)
            );
        }
        assert_eq!(
            v("aſ_of 2026-10-10", false).unwrap(),
            naive_local_iso(2026, 10, 10, 0, 0, 0)
        );
        assert_eq!(
            v("2026-10-10T10:00:00.9999999Z", false).unwrap(),
            utc_to_local_iso(2026, 10, 10, 10, 0, 0)
        );
    }

    #[test]
    fn epoch_and_parse() {
        assert_eq!(
            epoch_to_as_of(1700000000.0).unwrap(),
            utc_to_local_iso(2023, 11, 14, 22, 13, 20)
        );
        assert_eq!(
            epoch_to_as_of(-0.3).unwrap(),
            utc_to_local_iso(1969, 12, 31, 23, 59, 59)
        );
        assert_eq!(
            epoch_to_as_of(1700000000.9999996).unwrap(),
            utc_to_local_iso(2023, 11, 14, 22, 13, 21)
        );
        assert_eq!(
            epoch_to_as_of(f64::NAN).unwrap_err().python_text(),
            "ValueError: Invalid value NaN (not a number)"
        );
        assert_eq!(
            epoch_to_as_of(2.6e11).unwrap_err().python_text(),
            "ValueError: year must be in 1..9999, not 10209"
        );
        let off_9999_17 = clock::local_offset_seconds_at(253_402_275_600);
        if off_9999_17 >= 7 * 3600 {
            assert_eq!(
                epoch_to_as_of(253402275600.0).unwrap_err().python_text(),
                "OverflowError: date value out of range"
            );
        } else {
            assert_eq!(
                epoch_to_as_of(253402275600.0).unwrap(),
                utc_to_local_iso(9999, 12, 31, 17, 0, 0)
            );
        }
        assert_eq!(
            epoch_to_as_of(1e17).unwrap_err().python_text(),
            "OSError: [Errno 84] Value too large to be stored in data type"
        );
        assert_eq!(
            epoch_to_as_of(f64::INFINITY).unwrap_err().python_text(),
            "OverflowError: timestamp out of range for platform time_t"
        );
        let local_2026_10_10_utc = NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let expected_epoch = (local_2026_10_10_utc
            - clock::local_offset_seconds_at(local_2026_10_10_utc) as i64)
            as f64;
        assert_eq!(
            parse_as_of_epoch(Some("2026-10-10 junk")).unwrap(),
            expected_epoch
        );
        assert_eq!(parse_as_of_epoch(Some("x")).unwrap(), 0.0);
        assert_eq!(parse_as_of_epoch(None).unwrap(), 0.0);
        assert!(parse_as_of_epoch(Some("0001-01-01T00:00+08:00")).is_err());

        // parse_as_of_epoch_opt distinguishes 1970 epoch (0.0) and pre-1970 (<0) from None.
        assert_eq!(
            parse_as_of_epoch_opt(Some("1970-01-01T00:00:00Z")).unwrap(),
            Some(0.0)
        );
        assert_eq!(
            parse_as_of_epoch_opt(Some("1969-12-31T23:59:59Z")).unwrap(),
            Some(-1.0)
        );
        assert_eq!(parse_as_of_epoch_opt(None).unwrap(), None);
        assert_eq!(parse_as_of_epoch_opt(Some("")).unwrap(), None);
        assert_eq!(parse_as_of_epoch_opt(Some("   ")).unwrap(), None);
        assert_eq!(parse_as_of_epoch_opt(Some("not-a-date")).unwrap(), None);
        assert!(parse_as_of_epoch_opt(Some("0001-01-01T00:00+08:00")).is_err());
    }

    #[test]
    fn date_bounds_and_range_validation() {
        let local_ep = |y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32| -> f64 {
            let naive = NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(hh, mm, ss)
                .unwrap();
            local_naive_epoch(naive)
        };

        // Date-only since is inclusive start of local day; until is exclusive next local midnight.
        assert_eq!(
            parse_date_bound("2026-06-15", false).unwrap(),
            (local_ep(2026, 6, 15, 0, 0, 0), false)
        );
        assert_eq!(
            parse_date_bound("2026-06-15", true).unwrap(),
            (local_ep(2026, 6, 16, 0, 0, 0), true)
        );
        // Unicode digits handled identically to validate_as_of.
        assert_eq!(
            parse_date_bound("２０２６-01-01", true).unwrap(),
            (local_ep(2026, 1, 2, 0, 0, 0), true)
        );
        // 9999-12-31 end_of_day stays in year 9999 at 23:59:59 local with exclusive=false.
        assert_eq!(
            parse_date_bound("9999-12-31", true).unwrap(),
            (local_ep(9999, 12, 31, 23, 59, 59), false)
        );
        assert_eq!(
            parse_date_bound("9999-12-30", true).unwrap(),
            (local_ep(9999, 12, 31, 0, 0, 0), true)
        );
        // Exact ISO timestamp with offset is inclusive (exclusive=false) for both since and until.
        assert_eq!(
            parse_date_bound("2026-06-15T12:00:00+02:00", true).unwrap(),
            (1_781_517_600.0, false)
        );
        // Blank, empty, and trailing prose are rejected.
        for bad in ["", "   ", "not-a-date", "2026-06-15 extra"] {
            assert!(parse_date_bound(bad, false).is_err(), "{bad}");
            assert!(parse_date_bound(bad, true).is_err(), "{bad}");
        }

        // validate_date_range: empty / None -> (None, None, false)
        assert_eq!(
            validate_date_range(None, None).unwrap(),
            (None, None, false)
        );
        assert_eq!(
            validate_date_range(Some(""), Some("")).unwrap(),
            (None, None, false)
        );
        // Same date-only day is valid (s < next_midnight, exclusive=true).
        assert_eq!(
            validate_date_range(Some("2026-06-15"), Some("2026-06-15")).unwrap(),
            (
                Some(local_ep(2026, 6, 15, 0, 0, 0)),
                Some(local_ep(2026, 6, 16, 0, 0, 0)),
                true
            )
        );
        // Next day since vs previous day until has s == u with until_excl=true -> rejected!
        let err = validate_date_range(Some("2026-06-16"), Some("2026-06-15")).unwrap_err();
        assert_eq!(
            err.python_text(),
            "ValueError: --since cannot be after --until"
        );
        // Exact equal timestamps are valid (until_excl=false).
        assert_eq!(
            validate_date_range(
                Some("2026-06-15T10:00:00Z"),
                Some("2026-06-15T12:00:00+02:00")
            )
            .unwrap(),
            (Some(1_781_517_600.0), Some(1_781_517_600.0), false)
        );
        // Exact since > until rejected.
        let err = validate_date_range(Some("2026-06-15T10:00:01Z"), Some("2026-06-15T10:00:00Z"))
            .unwrap_err();
        assert_eq!(
            err.python_text(),
            "ValueError: --since cannot be after --until"
        );
        // Whitespace-only bound is rejected as invalid timestamp.
        assert!(validate_date_range(Some("   "), None).is_err());
    }

    #[test]
    fn item_dataclass() {
        let it = Item::new("m_1", "anvil", "fact", "T", "  hello  ", "decided");
        assert_eq!(it.content_hash, compute_content_hash("hello"));
        let expected_off = offset_text(clock::local_offset_seconds_at(
            clock::now_epoch().floor() as i64
        ));
        assert!(it.as_of.ends_with(&expected_off));
        let call = Call {
            args: vec![],
            kwargs: serde_json::from_str(
                r#"{"id": "m_1", "repo": "anvil", "kind": "fact", "title": "T", "body": "B"}"#,
            )
            .unwrap(),
        };
        let e = Item::from_call(&call).unwrap_err();
        assert_eq!(
            e.python_text(),
            "TypeError: Item.__init__() missing 1 required positional argument: 'tag'"
        );

        // Python 3.15.1 texts for the other __init__ failures.
        let kw = |extra: &str| -> serde_json::Map<String, Value> {
            serde_json::from_str(&format!(
                r#"{{"id": "m_1", "repo": "r", "kind": "fact", "title": "T", "body": "B", "tag": "t", "as_of": "x"{extra}}}"#
            ))
            .unwrap()
        };
        let err = |args: Vec<Value>, kwargs| {
            Item::from_call(&Call { args, kwargs })
                .unwrap_err()
                .python_text()
        };
        assert_eq!(
            err(vec![Value::from("m_1")], kw("")),
            "TypeError: Item.__init__() got multiple values for argument 'id'"
        );
        assert_eq!(
            err(vec![], kw(r#", "bogus": "x""#)),
            "TypeError: Item.__init__() got an unexpected keyword argument 'bogus'"
        );
        let sixteen: Vec<Value> = "abcdefghijklmnop"
            .chars()
            .map(|c| Value::from(c.to_string()))
            .collect();
        assert_eq!(
            err(sixteen, serde_json::Map::new()),
            "TypeError: Item.__init__() takes from 7 to 16 positional arguments but 17 were given"
        );
        let only_id = serde_json::from_str(r#"{"id": "m"}"#).unwrap();
        assert_eq!(
            err(vec![], only_id),
            "TypeError: Item.__init__() missing 5 required positional arguments: 'repo', 'kind', 'title', 'body', and 'tag'"
        );
    }
}
