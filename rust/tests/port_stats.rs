//! Port of the Python stats tests (simple-memory main 555b8f2). Same inputs, same literal
//! expected values. Python's `NOW = datetime.now(UTC)` is the real time, taken once per test.
mod common;

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use common::{open, put, scratch, sm};
use serde_json::{Value, json};
use simple_memory::core::item::{Item, epoch_to_as_of, parse_as_of_epoch};
use simple_memory::py::json::{Opts, dumps};
use simple_memory::service::stats::run_stats;
use simple_memory::store::items::put_item;

struct Clock {
    now: DateTime<Utc>,
}

impl Clock {
    fn new() -> Clock {
        let secs = Utc::now().timestamp();
        Clock {
            now: DateTime::from_timestamp(secs, 0).unwrap(),
        }
    }

    /// `isoformat(timespec="seconds")` of NOW minus the hours.
    fn ts(&self, hours_ago: f64) -> String {
        let t = self.now - Duration::seconds((hours_ago * 3600.0) as i64);
        t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    }

    fn hook(&self, hours_ago: f64, ids: &[&str], extra: Value) -> String {
        merge(
            json!({"ts": self.ts(hours_ago), "harness": "claude-code", "query": "q", "ids": ids}),
            extra,
        )
    }

    fn usage(&self, hours_ago: f64, cmd: &str, ids: &[&str]) -> String {
        json!({"ts": self.ts(hours_ago), "cmd": cmd, "ids": ids, "repo": null, "flags": {}})
            .to_string()
    }

    /// `_error_row`: a `path: "error"` row as `run_prompt_hook` writes it.
    fn error_row(&self, hours_ago: f64, error: &str, extra: Value) -> String {
        merge(
            json!({"ts": self.ts(hours_ago), "harness": "claude-code", "session": "s1", "pos": "later",
                   "path": "error", "ms": 12750, "rewrite_ms": 4000, "dropped": [], "error": error}),
            extra,
        )
    }

    /// `_local`: a `path: "local"` row.
    fn local(&self, hours_ago: f64, ids: &[&str], fallback: &str, extra: Value) -> String {
        let mut e = json!({"path": "local", "fallback": fallback});
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        self.hook(hours_ago, ids, e)
    }
}

/// `{**base, **extra}`.
fn merge(mut base: Value, extra: Value) -> String {
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base.to_string()
}

fn none() -> Value {
    json!({})
}

fn write_rows(path: &Path, rows: &[String]) {
    let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(path, text).unwrap();
}

/// `run_stats(days=..., now=...)` as the dict Python returns, read back as JSON.
fn stats_at(home: &Path, days: f64, now: Option<f64>) -> Value {
    let st = run_stats(days, home.to_str(), now).unwrap();
    serde_json::from_str(&dumps(&st.value(), &Opts::unicode()).unwrap()).unwrap()
}

fn stats(home: &Path) -> Value {
    stats_at(home, 7.0, None)
}

fn lines(out: &str) -> Vec<&str> {
    out.lines().collect()
}

#[test]
fn store_counts_by_status_and_for_the_active_items() {
    let (_d, home) = scratch();
    let old = put(&home, &["--repo", "a", "--tag", "derived", "Note one"]);
    let new = put(
        &home,
        &[
            "--repo", "a", "--tag", "decided", "--kind", "decision", "Note two",
        ],
    );
    put(
        &home,
        &[
            "--repo",
            "b",
            "--tag",
            "derived",
            "--source",
            "legacy:x",
            "Note three",
        ],
    );
    put(
        &home,
        &["--repo", "unassigned", "--tag", "prior", "Note four"],
    );
    assert_eq!(sm(&home, &["supersede", &old, &new]).0, 0);
    let conn = open(&home);
    // two legacy rows the put path cannot write: an old id, an untagged row
    for (item_id, tag) in [("legacy:t:1", "derived"), ("m_untagged", "n/a")] {
        conn.execute(
            "INSERT INTO items (id, repo, kind, title, body, tag, as_of, content_hash) \
             VALUES (?, 'b', 'fact', 't', 'b', ?, '2026-01-01T00:00:00+00:00', ?)",
            (item_id, tag, item_id),
        )
        .unwrap();
    }
    drop(conn);
    assert_eq!(
        stats(&home)["store"],
        json!({
            "status": {"active": 5, "superseded": 1},
            "active": 5,
            "repos": {"b": 3, "a": 1, "unassigned": 1},
            "repo_count": 3,
            "kinds": {"fact": 4, "decision": 1},
            "tags": {"derived": 2, "decided": 1, "n/a": 1, "prior": 1},
            "unassigned": 1,
            "legacy": 3,
        })
    );
}

#[test]
fn an_empty_home_reports_zeros_and_no_logs() {
    let (_d, home) = scratch();
    let st = stats(&home);
    assert_eq!(st["store"]["status"], json!({}));
    let zero =
        json!({"with_injection": 0, "tracked_ids": 0, "opened_ids": 0, "opened_share": null});
    assert_eq!(
        st["recall"],
        json!({
            "hook_runs": 0,
            "with_injection": 0,
            "injections": 0,
            "injected_ids": 0,
            "usage_rows": 0,
            "tracked_since": null,
            "tracked_ids": 0,
            "opened_ids": 0,
            "opened_share": null,
            "failed_runs": {},
            "fallback_runs": {},
            "by_path": {"haiku": zero, "local": zero},
            "bad_rows": {"hook.log": 0, "usage.log": 0},
        })
    );
    assert_eq!(st["never_opened"], json!([]));
    assert_eq!(
        st["catch_up"],
        json!({"runs": 0, "failed": 0, "part_failures": {}, "last": null, "bad_rows": 0})
    );
    assert_eq!(st["backups"], json!({"count": 0, "newest_age_hours": null}));
}

#[test]
fn recall_counts_runs_in_the_window_and_opens_after_the_injection() {
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            c.hook(24.0 * 9.0, &["m_old"], none()), // outside --days 7
            c.hook(30.0, &["m_a", "m_b"], none()),  // before usage.log began: counted, not tracked
            c.hook(10.0, &["m_a", "m_c"], none()),
            c.hook(9.0, &[], none()),
            json!({"ts": c.ts(8.0), "harness": "claude-code", "catch_up_error": "boom"})
                .to_string(), // not a prompt
            c.hook(5.0, &["m_c", "m_d"], none()),
            "not json".into(),
            r#"{"harness": "claude-code", "ids": ["m_e"]}"#.into(),
            r#"["m_e"]"#.into(),
        ],
    );
    write_rows(
        &home.join("usage.log"),
        &[
            c.usage(20.0, "query", &["m_a"]), // usage.log begins here
            c.usage(11.0, "show", &["m_c"]),  // before m_c's first tracked injection: not an open
            c.usage(4.0, "show", &["m_a", "m_x"]),
            c.usage(3.0, "related", &["m_d"]), // related is not an open
        ],
    );
    assert_eq!(
        stats(&home)["recall"],
        json!({
            "hook_runs": 4,
            "with_injection": 3,
            "injections": 6,
            "injected_ids": 4,
            "usage_rows": 4,
            "tracked_since": c.ts(20.0),
            "tracked_ids": 3, // m_a, m_c, m_d
            "opened_ids": 1,  // m_a
            "opened_share": 0.333,
            "failed_runs": {},
            "fallback_runs": {},
            "by_path": {
                "haiku": {"with_injection": 3, "tracked_ids": 3, "opened_ids": 1, "opened_share": 0.333},
                "local": {"with_injection": 0, "tracked_ids": 0, "opened_ids": 0, "opened_share": null},
            },
            "bad_rows": {"hook.log": 3, "usage.log": 0},
        })
    );
}

#[test]
fn error_rows_count_as_failed_runs_by_cause_never_as_injections() {
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            c.hook(6.0, &["m_a"], json!({"path": "haiku"})),
            c.error_row(5.0, "budget", json!({"query": "q", "ids": []})),
            c.error_row(4.0, "lock", json!({"query": "q", "ids": []})),
            c.error_row(3.0, "lock", json!({"query": "q", "ids": []})),
            c.error_row(2.0, "rewrite timed out after 8.0 s", none()), // a failed rewrite: no query, no ids key
            c.hook(1.0, &[], json!({"path": "none"})),
        ],
    );
    let rec = &stats(&home)["recall"];
    assert_eq!(
        (
            &rec["hook_runs"],
            &rec["with_injection"],
            &rec["injections"],
            &rec["injected_ids"]
        ),
        (&json!(6), &json!(1), &json!(1), &json!(1))
    );
    assert_eq!(
        rec["failed_runs"],
        json!({"budget": 1, "lock": 2, "rewrite": 1})
    );
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    let l = lines(&out);
    assert!(
        l[6].ends_with("hook_runs=6 with_injection=1 injections=1 injected_ids=1"),
        "{}",
        l[6]
    );
    assert_eq!(
        l[7],
        "  failed runs (nothing injected): 4 (budget=1 lock=2 rewrite=1)"
    );
}

#[test]
fn an_old_error_row_with_no_path_counts_as_a_failed_run_by_its_error_text() {
    // rows the hook wrote before it logged `path` (before f548a06): ts, harness and error only
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            json!({"ts": c.ts(6.0), "harness": "claude", "error": "rewrite timed out after 8.0 s"}).to_string(),
            json!({"ts": c.ts(5.0), "harness": "claude", "error": "claude exited 1"}).to_string(),
            json!({"ts": c.ts(3.0), "harness": "claude", "error": "", "query": "q", "ids": []}).to_string(), // no error text
            json!({"ts": c.ts(2.0), "harness": "claude", "error": "late", "query": "q", "ids": ["m_a"]}).to_string(), // it injected
            json!({"ts": c.ts(1.0), "harness": "claude", "catch_up_error": "boom", "error": "boom"}).to_string(), // not a prompt
            c.hook(1.0, &["m_b"], none()), // an old row that injected: no path, no error
            c.hook(1.0, &[], json!({"path": "none", "error": "kept text"})), // a path row fails only with `path: "error"`
        ],
    );
    let rec = &stats(&home)["recall"];
    assert_eq!(
        (
            &rec["hook_runs"],
            &rec["with_injection"],
            &rec["injections"]
        ),
        (&json!(6), &json!(2), &json!(2))
    );
    assert_eq!(rec["failed_runs"], json!({"rewrite": 2}));
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)[7],
        "  failed runs (nothing injected): 2 (rewrite=2)"
    );
}

#[test]
fn local_rows_count_as_fallback_runs_and_split_injections_and_opens_by_path() {
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            c.hook(9.0, &["m_a"], json!({"path": "haiku"})),
            c.hook(8.0, &["m_b"], none()), // a row from before the hook logged `path`: the model's
            c.local(
                7.0,
                &["m_c"],
                "timeout",
                json!({"error": "rewrite timed out after 8.0 s"}),
            ),
            c.local(6.0, &["m_a", "m_d"], "backoff", none()), // m_a came first from haiku
            c.local(
                5.0,
                &[],
                "error",
                json!({"query": "", "error": "claude exited 1: boom"}),
            ),
            c.local(4.0, &[], "off", json!({"query": "", "local_error": "lock"})), // the store gave up: a failed run
            c.local(3.0, &["m_e"], "off", none()),
        ],
    );
    write_rows(
        &home.join("usage.log"),
        &[
            c.usage(10.0, "query", &[]),
            c.usage(1.0, "show", &["m_a", "m_c", "m_e"]),
        ],
    );
    let rec = &stats(&home)["recall"];
    assert_eq!(
        (
            &rec["hook_runs"],
            &rec["with_injection"],
            &rec["injections"]
        ),
        (&json!(7), &json!(5), &json!(6))
    );
    assert_eq!(rec["failed_runs"], json!({"lock": 1}));
    assert_eq!(
        rec["fallback_runs"],
        json!({"backoff": 1, "error": 1, "off": 2, "timeout": 1})
    );
    assert_eq!(
        rec["by_path"],
        json!({
            "haiku": {"with_injection": 2, "tracked_ids": 2, "opened_ids": 1, "opened_share": 0.5},
            "local": {"with_injection": 3, "tracked_ids": 3, "opened_ids": 2, "opened_share": 0.667},
        })
    );
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)[7..10],
        [
            "  failed runs (nothing injected): 1 (lock=1)",
            "  fallback runs (local keywords): 5 (backoff=1 error=1 off=2 timeout=1)",
            "  by path: haiku with_injection=2 opened=1 of 2 (50.0%); local with_injection=3 opened=2 of 3 (66.7%)",
        ]
    );
}

#[test]
fn no_error_rows_print_no_failed_runs_line() {
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[c.hook(2.0, &["m_a"], json!({"path": "haiku"}))],
    );
    assert_eq!(stats(&home)["recall"]["failed_runs"], json!({}));
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)
            .iter()
            .filter(|l| l.contains("failed runs"))
            .count(),
        0
    );
}

#[test]
fn a_ts_with_no_offset_reads_as_utc() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let naive = (c.now - Duration::hours(2))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    write_rows(
        &home.join("hook.log"),
        &[json!({"ts": naive, "ids": ["m_a"]}).to_string()],
    );
    assert_eq!(stats(&home)["recall"]["injections"], json!(1));
}

#[test]
fn never_opened_lists_ids_injected_5_or_more_times_top_10_by_count() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let mut hook: Vec<String> = (0..5)
        .map(|n| c.hook(50.0 - n as f64, &["m_five"], none()))
        .collect();
    hook.extend((0..4).map(|n| c.hook(50.0 - n as f64, &["m_four"], none())));
    hook.extend((0..6).map(|n| c.hook(40.0 - n as f64, &["m_shown"], none())));
    let ks: Vec<String> = (0..11).map(|n| format!("m_k{n:02}")).collect();
    let ks: Vec<&str> = ks.iter().map(String::as_str).collect();
    hook.extend((0..5).map(|_| c.hook(30.0, &ks, none())));
    hook.extend((0..7).map(|n| c.hook(20.0 - n as f64, &["m_seven"], none())));
    write_rows(&home.join("hook.log"), &hook);
    write_rows(
        &home.join("usage.log"),
        &[
            c.usage(60.0, "query", &[]),
            c.usage(1.0, "show", &["m_shown"]),
        ],
    );
    let rows = stats(&home)["never_opened"].clone();
    let got: Vec<(String, i64)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["injections"].as_i64().unwrap(),
            )
        })
        .collect();
    let mut want = vec![("m_seven".to_string(), 7), ("m_five".to_string(), 5)];
    want.extend((0..8).map(|n| (format!("m_k{n:02}"), 5)));
    assert_eq!(got, want);
    assert_eq!(
        rows[0],
        json!({"id": "m_seven", "injections": 7, "repo": null, "kind": null, "status": "missing"})
    );
}

#[test]
fn never_opened_shows_repo_kind_and_status_of_a_stored_item() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let item = put(
        &home,
        &[
            "--repo", "a", "--tag", "derived", "--kind", "learning", "Note one",
        ],
    );
    let hook: Vec<String> = (0..5)
        .map(|n| {
            let ids: Vec<&str> = if n > 0 {
                vec![item.as_str(), "m_four"]
            } else {
                vec![item.as_str()]
            };
            c.hook(5.0 - n as f64, &ids, none())
        })
        .collect();
    write_rows(&home.join("hook.log"), &hook);
    write_rows(&home.join("usage.log"), &[c.usage(6.0, "query", &[])]);
    assert_eq!(
        stats(&home)["never_opened"],
        json!([{"id": item, "injections": 5, "repo": "a", "kind": "learning", "status": "active"}])
    );
}

#[test]
fn catch_up_counts_runs_failures_per_part_and_the_last_run() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let ok_part = json!({"ok": true, "ms": 1, "counts": {}, "error": null});
    let bad_part = json!({"ok": false, "ms": 1, "counts": {}, "error": "OSError: x"});
    write_rows(
        &home.join("catchup.log"),
        &[
            json!({"ts": c.ts(24.0 * 8.0), "ok": false, "parts": {"backup": bad_part}}).to_string(), // outside the window
            json!({"ts": c.ts(30.0), "ok": false, "parts": {"backup": ok_part, "imports": bad_part, "upkeep": bad_part}}).to_string(),
            "{torn".into(),
            json!({"ts": c.ts(20.0), "ok": false, "parts": {"imports": bad_part}}).to_string(),
            json!({"ts": c.ts(2.0), "ok": true, "parts": {"imports": ok_part}}).to_string(),
        ],
    );
    assert_eq!(
        stats(&home)["catch_up"],
        json!({
            "runs": 3,
            "failed": 2,
            "part_failures": {"imports": 2, "upkeep": 1},
            "last": {"ts": c.ts(2.0), "ok": true},
            "bad_rows": 1,
        })
    );
}

#[test]
fn backups_count_only_the_auto_copies() {
    let (_d, home) = scratch();
    let bdir = home.join("backups");
    std::fs::create_dir(&bdir).unwrap();
    for name in [
        "memory-20261101-000000-auto.db",
        "memory-20261102-000000-auto.db",
        "memory-20261101-pre-refs.db",
    ] {
        std::fs::write(bdir.join(name), b"").unwrap();
    }
    let bk = &stats(&home)["backups"];
    assert_eq!(bk["count"], json!(2));
    assert_eq!(bk["newest_age_hours"].as_f64(), Some(0.0));
}

#[test]
fn days_narrows_every_log_window() {
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            c.hook(30.0, &["m_a"], none()),
            c.hook(2.0, &["m_b"], none()),
        ],
    );
    write_rows(
        &home.join("catchup.log"),
        &[
            json!({"ts": c.ts(30.0), "ok": true, "parts": {}}).to_string(),
            json!({"ts": c.ts(2.0), "ok": true, "parts": {}}).to_string(),
        ],
    );
    let st = stats_at(&home, 1.0, None);
    assert_eq!(
        (&st["recall"]["hook_runs"], &st["catch_up"]["runs"]),
        (&json!(1), &json!(1))
    );
}

#[test]
fn days_windows_usage_log_by_the_same_cut_off() {
    // A show 9 days ago is outside --days 7: it opens nothing, and tracking starts at the in-window row.
    let (_d, home) = scratch();
    let c = Clock::new();
    let hook: Vec<String> = (0..5)
        .map(|n| c.hook(30.0 - n as f64, &["m_a"], none()))
        .collect();
    write_rows(&home.join("hook.log"), &hook);
    write_rows(
        &home.join("usage.log"),
        &[
            c.usage(24.0 * 9.0, "show", &["m_a"]),
            c.usage(40.0, "query", &[]),
        ],
    );
    let st = stats_at(&home, 7.0, None);
    assert_eq!(
        (&st["recall"]["usage_rows"], &st["recall"]["tracked_since"]),
        (&json!(1), &json!(c.ts(40.0)))
    );
    assert_eq!(
        (&st["recall"]["tracked_ids"], &st["recall"]["opened_ids"]),
        (&json!(1), &json!(0))
    );
    let rows: Vec<(&str, i64)> = st["never_opened"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap(), r["injections"].as_i64().unwrap()))
        .collect();
    assert_eq!(rows, vec![("m_a", 5)]);
}

/// `--days <days>` fails with the expected stderr and exit 1 (no traceback).
fn days_error(days: &str, want: &str) {
    let (_d, home) = scratch();
    let (code, out, err) = sm(&home, &["stats", "--days", days]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (1, "", want),
        "--days {days}"
    );
}

#[test]
fn days_above_100_years_is_an_error_not_a_traceback() {
    // the value as typed, with no ".0", for each of "1000000", "1e12", "inf"
    days_error(
        "1000000",
        "error: --days must be at most 36500, got 1000000\n",
    );
    days_error(
        "1e12",
        "error: --days must be at most 36500, got 1000000000000\n",
    );
    days_error("inf", "error: --days must be at most 36500, got inf\n");
}

#[test]
fn days_at_the_bound_still_runs() {
    let (_d, home) = scratch();
    assert_eq!(
        stats_at(&home, 36500.0, None)["days"].as_f64(),
        Some(36500.0)
    );
}

#[test]
fn days_nan_is_an_error() {
    days_error("nan", "error: --days must be above 0, got nan\n");
}

#[test]
fn days_must_be_above_zero() {
    days_error("0", "error: --days must be above 0, got 0\n");
    days_error("-1", "error: --days must be above 0, got -1\n");
}

#[test]
fn stats_text_output() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let item = put(&home, &["--repo", "a", "--tag", "derived", "Note one"]);
    let hook: Vec<String> = (0..5)
        .map(|n| c.hook(5.0 - n as f64, &[item.as_str()], none()))
        .collect();
    write_rows(&home.join("hook.log"), &hook);
    write_rows(&home.join("usage.log"), &[c.usage(6.0, "query", &[])]);
    write_rows(
        &home.join("catchup.log"),
        &[json!({"ts": c.ts(1.0), "ok": false, "parts": {"imports": {"ok": false}}}).to_string()],
    );
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    let l = lines(&out);
    assert_eq!(l[0], "store: items=1 active=1");
    assert_eq!(l[1], "  repos (active, top 1 of 1): a=1");
    assert_eq!(l[4], "  unassigned=0 (0.0%) legacy=0 (0.0%) of active");
    assert_eq!(l[5], "  stale (planned or assumed, older than 30 days): 0");
    assert!(
        l[6].ends_with("hook_runs=5 with_injection=5 injections=5 injected_ids=1"),
        "{}",
        l[6]
    );
    assert_eq!(
        l[7],
        format!(
            "  opened with sm show: 0 of 1 ids injected since usage.log began {} (0.0%); usage_rows=1",
            c.ts(6.0)
        )
    );
    assert_eq!(
        l[8..10],
        [
            "never opened, injected 5+ times since usage.log began: 1".to_string(),
            format!("  [{item}] injections=5 repo=a kind=fact status=active"),
        ]
    );
    assert_eq!(
        l[10],
        format!(
            "catch-up (last 7 days): runs=1 failed=1 last={} failed",
            c.ts(1.0)
        )
    );
    assert_eq!(
        l[11..],
        [
            "  part failures: imports=1",
            "backups: count=0 newest_age=none"
        ]
    );
}

#[test]
fn stats_json_is_one_object_with_every_section() {
    let (_d, home) = scratch();
    let (code, out, _) = sm(&home, &["stats", "--format", "json", "--days", "2"]);
    assert_eq!(code, 0);
    let st: Value = serde_json::from_str(&out).unwrap();
    let mut keys: Vec<&String> = st.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "backups",
            "catch_up",
            "days",
            "never_opened",
            "recall",
            "since",
            "stale",
            "store"
        ]
    );
    assert_eq!(st["days"].as_f64(), Some(2.0));
}

#[test]
fn stats_text_output_with_no_usage_rows_and_unreadable_rows() {
    let (_d, home) = scratch();
    write_rows(&home.join("hook.log"), &["torn".into()]);
    write_rows(&home.join("catchup.log"), &["torn".into(), "torn".into()]);
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    let l = lines(&out);
    assert_eq!(l[0], "store: items=0");
    assert_eq!(l[4], "  unassigned=0 (n/a) legacy=0 (n/a) of active");
    assert_eq!(l[5], "  stale (planned or assumed, older than 30 days): 0");
    assert_eq!(
        l[7..],
        [
            "  opened: no usage.log rows yet",
            "  unreadable rows: hook.log=1",
            "never opened, injected 5+ times since usage.log began: 0",
            "catch-up (last 7 days): runs=0 failed=0 last=none",
            "  unreadable rows: catchup.log=2",
            "backups: count=0 newest_age=none",
        ]
    );
}

/// Task items (iid, repo, tag, as_of).
fn stale_fixture(home: &Path, rows: &[(&str, &str, &str, &str)], superseded: &[&str]) {
    let conn = open(home);
    for (iid, repo, tag, as_of) in rows {
        let item = Item {
            as_of: (*as_of).into(),
            ..Item::bare(iid, repo, "task", &format!("T {iid}"), iid, tag)
        }
        .post_init();
        put_item(&conn, &item, None).unwrap();
    }
    for iid in superseded {
        conn.execute("UPDATE items SET status = 'superseded' WHERE id = ?", [iid])
            .unwrap();
    }
}

#[test]
fn stale_counts_active_planned_and_assumed_older_than_30_days_by_repo() {
    let (_d, home) = scratch();
    let now = parse_as_of_epoch(Some("2026-11-04T12:00:00+00:00")).unwrap();
    stale_fixture(
        &home,
        &[
            ("m_1", "a", "planned", "2026-09-01T09:00:00+00:00"),
            ("m_2", "a", "assumed", "2026-10-01T09:00:00+00:00"),
            ("m_edge", "a", "assumed", "2026-10-05T12:00:00+00:00"), // exactly 30 days: not older
            ("m_3", "b", "planned", "2026-10-05T11:59:59+00:00"),    // one second past 30 days
            ("m_decided", "a", "decided", "2026-08-01T09:00:00+00:00"),
            ("m_gone", "c", "planned", "2026-08-01T09:00:00+00:00"),
            ("m_fresh", "a", "planned", "2026-11-01T09:00:00+00:00"),
        ],
        &["m_gone"],
    );
    assert_eq!(
        stats_at(&home, 7.0, Some(now))["stale"],
        json!({"older_than_days": 30, "tags": ["planned", "assumed"], "total": 3, "repos": {"a": 2, "b": 1}})
    );
    let earlier = parse_as_of_epoch(Some("2026-09-01T00:00:00+00:00")).unwrap();
    assert_eq!(
        stats_at(&home, 7.0, Some(earlier))["stale"]["total"],
        json!(0)
    );
}

#[test]
fn stats_text_prints_the_stale_line_under_store() {
    let (_d, home) = scratch();
    let c = Clock::new();
    let old = epoch_to_as_of(c.now.timestamp() as f64 - 40.0 * 24.0 * 3600.0).unwrap();
    stale_fixture(
        &home,
        &[
            ("m_b1", "b", "assumed", &old),
            ("m_a1", "a", "planned", &old),
            ("m_b2", "b", "planned", &old),
        ],
        &[],
    );
    let (code, out, _) = sm(&home, &["stats"]);
    assert_eq!(code, 0);
    assert_eq!(
        lines(&out)[5],
        "  stale (planned or assumed, older than 30 days): 3 (b=2 a=1)"
    );
    let (code, out, _) = sm(&home, &["stats", "--format", "json"]);
    assert_eq!(code, 0);
    let st: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["stale"]["repos"], json!({"b": 2, "a": 1}));
}

#[test]
fn a_local_row_whose_search_failed_counts_as_a_failed_run_by_local_error() {
    // spec change 2026-11-04 06:42: a run that injected nothing because the store gave up or the
    // local path raised is a failed run on any path; `error` on a local row is the rewrite's message
    let (_d, home) = scratch();
    let c = Clock::new();
    write_rows(
        &home.join("hook.log"),
        &[
            c.local(6.0, &[], "timeout", json!({"query": "", "error": "rewrite timed out after 8.0 s", "local_error": "budget"})),
            c.local(5.0, &[], "off", json!({"query": "", "local_error": "lock"})),
            c.local(4.0, &[], "error", json!({"query": "", "error": "claude exited 1: boom", "local_error": "KeyError: 'the'"})),
            c.local(3.0, &[], "timeout", json!({"query": "", "error": "rewrite timed out after 8.0 s"})), // nothing cleared the floor
            c.local(2.0, &["m_a"], "error", json!({"error": "claude exited 1: boom"})), // it injected
            c.error_row(1.0, "lock", json!({"query": "q", "ids": []})),
        ],
    );
    assert_eq!(
        stats(&home)["recall"]["failed_runs"],
        json!({"budget": 1, "local": 1, "lock": 2})
    );
}
