//! Every wall-clock read and every random byte sm takes.
//!
//! Local timestamps use `SM_UTC_OFFSET` when set (for example `+00:00` or `-05:00`), or the host
//! timezone via `localtime_r`.
//! Under the cargo feature `parity`, `SM_PARITY_NOW` (epoch seconds as a decimal) pins the clock,
//! and `SM_PARITY_SEED` makes `urandom` return the stream `sha256(f"{seed}:{i}")` for i = 0, 1, 2,
//! ..., consumed in order within the process. `Instant` deadlines stay on the real monotonic clock.

use chrono::{DateTime, FixedOffset, TimeZone};

use crate::error::SmError;

pub fn utc() -> FixedOffset {
    FixedOffset::east_opt(0).expect("UTC is a valid offset")
}

/// Parse an `SM_UTC_OFFSET` override such as `Z`, `UTC`, `+00:00`, `-05:00`, `+0530`, or `+02`.
pub fn parse_utc_offset(spec: &str) -> Option<i32> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("Z") || s.eq_ignore_ascii_case("UTC") || s == "0" {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes()[0] {
        b'+' => (1i32, &s[1..]),
        b'-' => (-1i32, &s[1..]),
        _ => return None,
    };
    let (hh, mm) = if let Some((h, m)) = rest.split_once(':') {
        if h.is_empty()
            || h.len() > 2
            || m.len() != 2
            || !h.bytes().all(|b| b.is_ascii_digit())
            || !m.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)
    } else if rest.len() == 4 && rest.bytes().all(|b| b.is_ascii_digit()) {
        (
            rest[..2].parse::<u32>().ok()?,
            rest[2..].parse::<u32>().ok()?,
        )
    } else if (1..=2).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit()) {
        (rest.parse::<u32>().ok()?, 0)
    } else {
        return None;
    };
    if hh >= 24 || mm >= 60 {
        return None;
    }
    let secs = sign * (hh * 3600 + mm * 60) as i32;
    FixedOffset::east_opt(secs).map(|_| secs)
}

/// Host UTC offset in seconds for `epoch_secs`, honoring `SM_UTC_OFFSET` if set.
pub fn local_offset_seconds_at(epoch_secs: i64) -> i32 {
    if let Ok(spec) = std::env::var("SM_UTC_OFFSET")
        && let Some(secs) = parse_utc_offset(&spec)
    {
        return secs;
    }
    // Clamp pre-1900 / post-2100 timestamps deterministically rather than mixing wall-clock DST.
    let probe = epoch_secs.clamp(-2_208_988_800, 4_102_444_800);
    let Ok(st) = localtime_secs(probe) else {
        return 0;
    };
    let Ok(secs) = i32::try_from(st.tm_gmtoff) else {
        return 0;
    };
    // Keep offsets on whole-minute boundaries so formatted timestamps always satisfy RFC 3339.
    let rounded = if secs >= 0 {
        ((secs + 30) / 60) * 60
    } else {
        ((secs - 30) / 60) * 60
    };
    if FixedOffset::east_opt(rounded).is_some() {
        rounded
    } else {
        0
    }
}

/// Resolve local UTC offset for a naive wall-clock timestamp (UTC epoch representation) across DST transitions.
pub fn local_offset_for_naive_local(naive_as_utc_secs: i64) -> i32 {
    let off0 = local_offset_seconds_at(naive_as_utc_secs);
    let off1 = local_offset_seconds_at(naive_as_utc_secs.saturating_sub(i64::from(off0)));
    local_offset_seconds_at(naive_as_utc_secs.saturating_sub(i64::from(off1)))
}

/// Host `FixedOffset` at `epoch_secs`, honoring `SM_UTC_OFFSET` if set.
pub fn local_offset_at(epoch_secs: i64) -> FixedOffset {
    FixedOffset::east_opt(local_offset_seconds_at(epoch_secs)).unwrap_or_else(utc)
}

#[cfg(feature = "parity")]
mod pin {
    use std::sync::{LazyLock, Mutex};

    pub static NOW: LazyLock<Option<f64>> =
        LazyLock::new(|| super::parse_now(std::env::var("SM_PARITY_NOW").ok()));

    pub struct Stream {
        pub seed: String,
        pub i: u64,
        pub buf: Vec<u8>,
    }

    pub static SEED: LazyLock<Option<Mutex<Stream>>> = LazyLock::new(|| {
        std::env::var("SM_PARITY_SEED").ok().map(|seed| {
            Mutex::new(Stream {
                seed,
                i: 0,
                buf: Vec::new(),
            })
        })
    });
}

/// The pinned instant for an `SM_PARITY_NOW` value. Unset or empty leaves the clock real, as the
/// shim's `if _NOW:` does.
#[cfg(any(test, feature = "parity"))]
fn parse_now(v: Option<String>) -> Option<f64> {
    v.filter(|s| !s.is_empty())
        .map(|s| s.trim().parse().expect("SM_PARITY_NOW is a decimal"))
}

/// The next `n` bytes of the stream `sha256(f"{seed}:{i}")`: `buf` holds the unread bytes and
/// `i` the next block index.
#[cfg(any(test, feature = "parity"))]
pub fn seeded_bytes(seed: &str, i: &mut u64, buf: &mut Vec<u8>, n: usize) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    while buf.len() < n {
        buf.extend_from_slice(&Sha256::digest(format!("{seed}:{i}").as_bytes()));
        *i += 1;
    }
    buf.drain(..n).collect()
}

/// The real `time.time()`.
fn real_epoch() -> f64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock is after 1970");
    // time.time() is the ns count divided by 1e9 as a double.
    d.as_nanos() as f64 / 1e9
}

/// `time.time()`.
pub fn now_epoch() -> f64 {
    #[cfg(feature = "parity")]
    if let Some(t) = *pin::NOW {
        return t;
    }
    real_epoch()
}

/// `datetime.fromtimestamp(ts, tz)` for a ts in range: microseconds rounded half to even
/// (CPython `_PyTime_ROUND_HALF_EVEN`), so .9999996 s carries into the next second.
pub fn from_timestamp(ts: f64, tz: FixedOffset) -> Option<DateTime<FixedOffset>> {
    if !ts.is_finite() {
        return None;
    }
    // _PyTime_DoubleToDenominator: modf, scale, round half to even, carry either way.
    let mut secs = ts.trunc();
    let mut us = ((ts - secs) * 1e6).round_ties_even();
    if us >= 1e6 {
        secs += 1.0;
        us -= 1e6;
    } else if us < 0.0 {
        secs -= 1.0;
        us += 1e6;
    }
    if !(-(i64::MAX as f64)..(i64::MAX as f64)).contains(&secs) {
        return None;
    }
    tz.timestamp_opt(secs as i64, us as u32 * 1000).single()
}

/// `datetime.now(tz)` at the clock `now_epoch` reads. Unpinned it floors to the microsecond as
/// CPython's `datetime.now` does; pinned it is `fromtimestamp(SM_PARITY_NOW, tz)` like the shim.
pub fn now_tz(tz: FixedOffset) -> DateTime<FixedOffset> {
    #[cfg(feature = "parity")]
    if let Some(t) = *pin::NOW {
        return from_timestamp(t, tz).expect("SM_PARITY_NOW is a valid timestamp");
    }
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock is after 1970");
    let us = d.subsec_micros();
    tz.timestamp_opt(d.as_secs() as i64, us * 1000)
        .single()
        .expect("now is a valid time")
}

/// Local wall-clock time (`SM_UTC_OFFSET` or host timezone).
pub fn now_local() -> DateTime<FixedOffset> {
    let secs = now_epoch().floor() as i64;
    now_tz(local_offset_at(secs))
}

/// `datetime.now(UTC)`.
pub fn now_utc() -> DateTime<FixedOffset> {
    now_tz(utc())
}

/// `os.urandom(n)`.
pub fn urandom(n: usize) -> Vec<u8> {
    #[cfg(feature = "parity")]
    if let Some(m) = &*pin::SEED {
        let mut s = m.lock().expect("the seed stream lock is not poisoned");
        let pin::Stream { seed, i, buf } = &mut *s;
        return seeded_bytes(seed, i, buf, n);
    }
    let mut out = vec![0u8; n];
    getrandom::fill(&mut out).expect("the OS gives random bytes");
    out
}

/// `time.struct_time`, as `time.localtime` fills it.
#[derive(Clone, Debug, PartialEq)]
pub struct StructTime {
    pub tm_year: i64,
    pub tm_mon: i32,
    pub tm_mday: i32,
    pub tm_hour: i32,
    pub tm_min: i32,
    pub tm_sec: i32,
    /// Monday is 0, as in Python (C's `tm_wday` has Sunday 0).
    pub tm_wday: i32,
    /// January 1 is 1, as in Python.
    pub tm_yday: i32,
    pub tm_isdst: i32,
    pub tm_zone: String,
    pub tm_gmtoff: i64,
}

/// CPython's OverflowError text for a timestamp outside time_t; an int timestamp beyond i64
/// raises `SmError::overflow(TIME_T_RANGE)` before it reaches `localtime_secs`.
pub const TIME_T_RANGE: &str = "timestamp out of range for platform time_t";

/// `time.localtime(ts)` for a float `ts` (`_PyTime_ObjectToTime_t` with ROUND_FLOOR), or
/// `time.localtime()` at the clock `now_epoch` reads when `ts` is None. Uses the host timezone (`TZ`).
pub fn localtime(ts: Option<f64>) -> Result<StructTime, SmError> {
    let ts = ts.unwrap_or_else(now_epoch);
    if ts.is_nan() {
        return Err(SmError::value("Invalid value NaN (not a number)"));
    }
    let t = ts.floor();
    // i64::MIN and i64::MAX as f64 are -2**63 and 2**63, CPython's time_t range check.
    if !((i64::MIN as f64) <= t && t < (i64::MAX as f64)) {
        return Err(SmError::overflow(TIME_T_RANGE));
    }
    localtime_secs(t as i64)
}

/// `errno = 0` for this thread. std reads errno (`io::Error::last_os_error`) but cannot set it;
/// the location symbol differs by libc: `__error` on macOS, `__errno_location` on Linux.
fn clear_errno() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use libc::__errno_location as errno_location;
    #[cfg(target_vendor = "apple")]
    use libc::__error as errno_location;
    // SAFETY: the location is this thread's errno, valid for the thread's life.
    unsafe { *errno_location() = 0 };
}

/// `time.localtime(t)` for an int `t`: libc `localtime_r`, which reads TZ (macOS runs tzset in
/// it).
pub fn localtime_secs(t: i64) -> Result<StructTime, SmError> {
    // SAFETY: `tm` is a zeroed struct that localtime_r fills; `t` outlives the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // CPython `_PyTime_localtime`: errno is cleared first, and a NULL with errno still 0 is
    // EINVAL (a TZ with rules past the time_t range fails without setting errno).
    clear_errno();
    // SAFETY: as above.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(SmError::os(
            if errno == 0 { libc::EINVAL } else { errno },
            None,
            None,
        ));
    }
    // CPython adds 1900 in a C int, so a year past i32 wraps: 67767976233532799.0 in UTC is
    // year -2147483648.
    let tm_year = i64::from(tm.tm_year.wrapping_add(1900));
    let tm_zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null tm_zone points at a NUL-terminated zone name libc keeps alive.
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    Ok(StructTime {
        tm_year,
        tm_mon: tm.tm_mon + 1,
        tm_mday: tm.tm_mday,
        tm_hour: tm.tm_hour,
        tm_min: tm.tm_min,
        tm_sec: tm.tm_sec,
        tm_wday: (tm.tm_wday + 6) % 7,
        tm_yday: tm.tm_yday + 1,
        tm_isdst: tm.tm_isdst,
        tm_zone,
        tm_gmtoff: tm.tm_gmtoff as i64,
    })
}

/// `time.strftime("%H:%M", t)`: `gettmarg` refuses a year below `INT_MIN + 1900`.
pub fn strftime_hm(t: &StructTime) -> Result<String, SmError> {
    if t.tm_year < i64::from(i32::MIN) + 1900 {
        return Err(SmError::overflow("year out of range"));
    }
    Ok(format!("{:02}:{:02}", t.tm_hour, t.tm_min))
}

/// Parity entries.
#[cfg(any(test, feature = "parity"))]
pub fn parity(f: &str, call: &crate::parity::Call) -> Option<crate::parity::Reply> {
    use serde_json::{Value, json};
    if f != "py.time.localtime" {
        return None;
    }
    let t = match call.args.first() {
        None | Some(Value::Null) => localtime(None),
        Some(Value::Number(n)) if n.is_i64() => localtime_secs(n.as_i64().expect("an i64")),
        Some(Value::Number(n)) => localtime(n.as_f64()),
        Some(Value::Object(o)) => match o.get("__float").and_then(Value::as_str) {
            Some("nan") => localtime(Some(f64::NAN)),
            Some("inf") => localtime(Some(f64::INFINITY)),
            _ => localtime(Some(f64::NEG_INFINITY)),
        },
        Some(other) => Err(SmError::new(
            crate::error::Base::Type,
            "TypeError",
            format!("unsupported timestamp {other}"),
        )),
    };
    Some(t.and_then(|t| {
        let hm = strftime_hm(&t)?;
        Ok(json!([
            t.tm_year,
            t.tm_mon,
            t.tm_mday,
            t.tm_hour,
            t.tm_min,
            t.tm_sec,
            t.tm_wday,
            t.tm_yday,
            t.tm_isdst,
            t.tm_zone,
            t.tm_gmtoff,
            hm
        ]))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_errno_zeroes_this_threads_errno() {
        // SAFETY: close(-1) only sets errno to EBADF.
        unsafe { libc::close(-1) };
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        clear_errno();
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(0));
    }

    #[test]
    fn seeded_stream_matches_python() {
        // python3 -c "import hashlib; print((hashlib.sha256(b'7:0').digest()+hashlib.sha256(b'7:1').digest())[:40].hex())"
        let want =
            "f5ff61d7b533cd7371f120b74bb93602758cee22e3a30244fbe90fbe99ca4623d7a0cee7b61eb0e3";
        let (mut i, mut buf) = (0, Vec::new());
        let mut got = seeded_bytes("7", &mut i, &mut buf, 10);
        got.extend(seeded_bytes("7", &mut i, &mut buf, 30));
        let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, want);
        assert_eq!(i, 2);
    }

    #[test]
    fn an_empty_or_unset_now_leaves_the_clock_real() {
        assert_eq!(parse_now(None), None);
        assert_eq!(parse_now(Some(String::new())), None);
        assert_eq!(parse_now(Some("1762243200.5".into())), Some(1762243200.5));
    }

    #[test]
    fn from_timestamp_rounds_half_even_to_the_microsecond() {
        let t = from_timestamp(0.9999996, utc()).unwrap();
        assert_eq!(t.to_rfc3339(), "1970-01-01T00:00:01+00:00");
        let plus_five_thirty = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let t = from_timestamp(1700000000.0, plus_five_thirty).unwrap();
        assert_eq!(
            t.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "2023-11-15T03:43:20+05:30"
        );
        let t = from_timestamp(-1.5, utc()).unwrap();
        assert_eq!(
            t.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            "1969-12-31T23:59:58.500000"
        );
        assert!(from_timestamp(f64::NAN, utc()).is_none());
    }

    #[test]
    fn parse_utc_offset_accepts_iso_and_short_forms_and_rejects_bad_input() {
        assert_eq!(parse_utc_offset("Z"), Some(0));
        assert_eq!(parse_utc_offset("utc"), Some(0));
        assert_eq!(parse_utc_offset("0"), Some(0));
        assert_eq!(parse_utc_offset("+00:00"), Some(0));
        assert_eq!(parse_utc_offset("-05:00"), Some(-5 * 3600));
        assert_eq!(parse_utc_offset("+05:30"), Some(5 * 3600 + 1800));
        assert_eq!(parse_utc_offset("+0930"), Some(9 * 3600 + 1800));
        assert_eq!(parse_utc_offset("-7"), Some(-7 * 3600));
        assert_eq!(parse_utc_offset(""), None);
        assert_eq!(parse_utc_offset("+24:00"), None);
        assert_eq!(parse_utc_offset("+05:60"), None);
        assert_eq!(parse_utc_offset("EST"), None);
    }
}
