//! `clock::localtime` and local `as_of` conversion honor `TZ` and `SM_UTC_OFFSET`.
//! Env vars are read per process/helper, so timezone cases run `sm` in a child process.
//! Reference expectations: `TZ=<zone> python3 -c "import time;
//! t=time.localtime(1762243200); print(tuple(t), t.tm_zone, t.tm_gmtoff)"`.
#![cfg(feature = "parity")]

use std::io::Write;
use std::process::{Command, Stdio};

fn localtime(tz: &str, now: Option<&str>, arg: &str) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.arg("__parity")
        .env("TZ", tz)
        .env_remove("SM_PARITY_NOW");
    if let Some(n) = now {
        cmd.env("SM_PARITY_NOW", n);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sm");
    let line = format!("{{\"fn\": \"py.time.localtime\", \"args\": [{arg}], \"kwargs\": {{}}}}\n");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(line.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim_end()
        .to_string()
}

#[test]
fn localtime_uses_the_tz_zone() {
    assert_eq!(
        localtime("America/New_York", None, "1762243200"),
        r#"{"ok":[2025,11,4,3,0,0,1,308,0,"EST",-18000,"03:00"]}"#
    );
    assert_eq!(
        localtime("Asia/Kolkata", None, "1762243200"),
        r#"{"ok":[2025,11,4,13,30,0,1,308,0,"IST",19800,"13:30"]}"#
    );
    // Floats floor: -1.5 is 1969-12-31 23:59:58 UTC, a Wednesday (2), day 365.
    assert_eq!(
        localtime("UTC", None, "-1.5"),
        r#"{"ok":[1969,12,31,23,59,58,2,365,0,"UTC",0,"23:59"]}"#
    );
}

#[test]
fn localtime_without_a_timestamp_reads_the_pinned_clock() {
    assert_eq!(
        localtime("Asia/Kolkata", Some("1762243200.5"), "null"),
        r#"{"ok":[2025,11,4,13,30,0,1,308,0,"IST",19800,"13:30"]}"#
    );
}

#[test]
fn localtime_errors_match_python() {
    assert_eq!(
        localtime("UTC", None, r#"{"__float": "nan"}"#),
        r#"{"err":"ValueError: Invalid value NaN (not a number)"}"#
    );
    assert_eq!(
        localtime("UTC", None, "1e20"),
        r#"{"err":"OverflowError: timestamp out of range for platform time_t"}"#
    );
    assert_eq!(
        localtime("UTC", None, "9223372036854775808"),
        r#"{"err":"OverflowError: timestamp out of range for platform time_t"}"#
    );
    // localtime_r refuses 1e18 with the platform's errno: macOS gives EOVERFLOW (84) in UTC and
    // EINVAL in New York; glibc gives EOVERFLOW (75) in both (ubuntu-latest / macOS behavior).
    if cfg!(target_os = "macos") {
        assert_eq!(
            localtime("UTC", None, "1e18"),
            r#"{"err":"OSError: [Errno 84] Value too large to be stored in data type"}"#
        );
        assert_eq!(
            localtime("America/New_York", None, "1e18"),
            r#"{"err":"OSError: [Errno 22] Invalid argument"}"#
        );
    } else {
        for tz in ["UTC", "America/New_York"] {
            assert_eq!(
                localtime(tz, None, "1e18"),
                r#"{"err":"OSError: [Errno 75] Value too large for defined data type"}"#
            );
        }
    }
    // A year past i32 wraps in localtime, as in CPython; strftime then refuses it in UTC, where
    // it wraps to -2147483648, and prints it in New York, where it stays 2147483647.
    assert_eq!(
        localtime("UTC", None, "67767976233532799.0"),
        r#"{"err":"OverflowError: year out of range"}"#
    );
    assert_eq!(
        localtime("America/New_York", None, "67767976233532799.0"),
        r#"{"ok":[2147483647,12,31,19,0,0,1,365,0,"EST",-18000,"19:00"]}"#
    );
}

fn parity_call_at(
    tz: Option<&str>,
    utc_offset: Option<&str>,
    now: Option<&str>,
    fn_name: &str,
    args_json: &str,
) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.arg("__parity")
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_UTC_OFFSET");
    if let Some(t) = tz {
        cmd.env("TZ", t);
    }
    if let Some(off) = utc_offset {
        cmd.env("SM_UTC_OFFSET", off);
    }
    if let Some(n) = now {
        cmd.env("SM_PARITY_NOW", n);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sm");
    let line = format!("{{\"fn\": \"{fn_name}\", \"args\": {args_json}, \"kwargs\": {{}}}}\n");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(line.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim_end()
        .to_string()
}

fn parity_call(
    tz: Option<&str>,
    utc_offset: Option<&str>,
    fn_name: &str,
    args_json: &str,
) -> String {
    parity_call_at(tz, utc_offset, None, fn_name, args_json)
}

#[test]
fn portable_as_of_honors_sm_utc_offset_and_tz() {
    assert_eq!(
        parity_call(
            Some("UTC"),
            Some("+00:00"),
            "core.item.validate_as_of",
            r#"["2026-10-17T21:44:33Z", false, false]"#,
        ),
        r#"{"ok":"2026-10-17T21:44:33+00:00"}"#
    );
    assert_eq!(
        parity_call(
            Some("UTC"),
            Some("-05:00"),
            "core.item.validate_as_of",
            r#"["2026-10-17T21:44:33Z", false, false]"#,
        ),
        r#"{"ok":"2026-10-17T16:44:33-05:00"}"#
    );
    assert_eq!(
        parity_call(
            Some("UTC"),
            Some("+05:30"),
            "core.item.epoch_to_as_of",
            r#"[1700000000.0]"#,
        ),
        r#"{"ok":"2023-11-15T03:43:20+05:30"}"#
    );
    assert_eq!(
        parity_call(
            Some("UTC"),
            Some("+05:00"),
            "core.item.validate_as_of",
            r#"["9999-12-31T20:00", true, false]"#,
        ),
        r#"{"err":"OverflowError: date value out of range"}"#
    );
    assert_eq!(
        parity_call(
            Some("UTC"),
            Some("-05:00"),
            "core.item.validate_as_of",
            r#"["0001-01-01T02:00Z", false, false]"#,
        ),
        r#"{"err":"OverflowError: date value out of range"}"#
    );
    assert_eq!(
        parity_call(
            Some("America/New_York"),
            None,
            "core.item.validate_as_of",
            r#"["2026-01-15T12:00:00Z", false, false]"#,
        ),
        r#"{"ok":"2026-01-15T07:00:00-05:00"}"#
    );
    assert_eq!(
        parity_call(
            Some("America/New_York"),
            None,
            "core.item.validate_as_of",
            r#"["2026-07-15T12:00:00Z", false, false]"#,
        ),
        r#"{"ok":"2026-07-15T08:00:00-04:00"}"#
    );
    // Naive wall-clock times resolve across DST transitions without being off by 1 hour.
    assert_eq!(
        parity_call(
            Some("America/New_York"),
            None,
            "core.item.validate_as_of",
            r#"["2026-03-08 03:00:00", false, false]"#,
        ),
        r#"{"ok":"2026-03-08T03:00:00-04:00"}"#
    );
    assert_eq!(
        parity_call(
            Some("America/New_York"),
            None,
            "core.item.validate_as_of",
            r#"["2026-11-01 05:00:00", false, false]"#,
        ),
        r#"{"ok":"2026-11-01T05:00:00-05:00"}"#
    );
    // Pre-1970 epochs stay deterministic regardless of current winter/summer wall clock.
    let winter_now = (20_468_i64 * 86_400 + 43_200).to_string();
    let summer_now = (20_649_i64 * 86_400 + 43_200).to_string();
    assert_eq!(
        parity_call_at(
            Some("America/New_York"),
            None,
            Some(&winter_now),
            "core.item.epoch_to_as_of",
            r#"[-0.3]"#,
        ),
        r#"{"ok":"1969-12-31T18:59:59-05:00"}"#
    );
    assert_eq!(
        parity_call_at(
            Some("America/New_York"),
            None,
            Some(&summer_now),
            "core.item.epoch_to_as_of",
            r#"[-0.3]"#,
        ),
        r#"{"ok":"1969-12-31T18:59:59-05:00"}"#
    );
}
