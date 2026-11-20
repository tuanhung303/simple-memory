//! Integration and boundary tests for Wave 2 Lane A:
//! 1. `sm_epoch` scalar function & `--since` / `--until` date filtering across mixed UTC offsets
//! 2. `--source` case-sensitive prefix filtering, `items_source` index plan & Unicode boundaries
//! 3. `sm query` footer flag quoting (`shlex_quote`) and `usage.log` flag recording
//! 4. `sm list` exact-repo / `--all` / `--in` scope mutex, `--status` / `--include-history` mutex,
//!    pagination (`i128` / `BigInt` safety), `brief`/`text`/`json` formats, and `usage.log`
//!
//! Every fixture uses made-up names (`anvil`, `acme`, `globex`, `initech`, `GLX`) and runs
//! against a scratch `SIMPLE_MEMORY_HOME`.

mod common;

use std::path::Path;
use std::process::Command;

use common::{git, open, put, scratch, sm, sm_env, write};
use serde_json::{Value, json};
use simple_memory::core::item::{
    Item, parse_as_of_epoch_opt, parse_date_bound, validate_date_range,
};
use simple_memory::store::items::put_item;
use simple_memory::store::search::{
    Filters, StatusFilter, build_item_filters, list_items, source_prefix_bounds,
};

#[allow(clippy::too_many_arguments)]
fn seed_raw(
    home: &Path,
    id: &str,
    repo: &str,
    kind: &str,
    tag: &str,
    title: &str,
    body: &str,
    source: &str,
    as_of: &str,
    status: &str,
    superseded_by: Option<&str>,
    tags: &str,
    refs: &str,
) {
    let conn = open(home);
    conn.execute(
        "INSERT INTO items (id, repo, kind, title, body, refs, tags, source, tag, evidence, as_of, status, superseded_by, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'made-up check', ?10, ?11, ?12, ?13)",
        rusqlite::params![
            id,
            repo,
            kind,
            title,
            body,
            refs,
            tags,
            source,
            tag,
            as_of,
            status,
            superseded_by,
            format!("hash_{id}"),
        ],
    )
    .expect("insert raw item");
}

fn sm_in_cwd(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sm"));
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home.parent().unwrap_or(home))
        .env("SIMPLE_MEMORY_HOME", home)
        .env_remove("SM_PARITY_NOW")
        .env_remove("SM_PARITY_SEED")
        .env_remove("SM_PARITY_TRACE");
    let out = cmd.output().expect("spawn sm");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

fn parse_json(out: &str) -> Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("invalid json ({e}): {out}"))
}

fn json_item_ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|it| it["id"].as_str().unwrap().to_string())
        .collect()
}

fn json_result_ids(v: &Value) -> Vec<String> {
    v["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|it| it["id"].as_str().unwrap().to_string())
        .collect()
}

fn usage_rows(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("usage.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

// ---------------------------------------------------------------- 1. Mixed UTC offsets & date bounds

#[test]
fn date_filters_handle_mixed_utc_offsets_inclusive_day_exact_iso_and_errors() {
    let (_d, home) = scratch();

    // Mixed UTC offsets on the same calendar boundary in +05:30:
    // 2026-06-15 in +05:30 spans [2026-06-14T18:30:00Z, 2026-06-15T18:30:00Z).
    seed_raw(
        &home,
        "m_before",
        "anvil",
        "fact",
        "decided",
        "Walrus before day",
        "walrus before local midnight in +05:30",
        "task:T-1",
        "2026-06-14T18:29:59+00:00", // 2026-06-14 23:59:59 +05:30
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_start",
        "anvil",
        "fact",
        "decided",
        "Walrus start of day",
        "walrus right at local midnight in +05:30",
        "task:T-2",
        "2026-06-14T14:30:00-04:00", // 2026-06-14T18:30:00Z == 2026-06-15 00:00:00 +05:30
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_end",
        "anvil",
        "fact",
        "decided",
        "Walrus end of day",
        "walrus last second of local day in +05:30",
        "task:T-3",
        "2026-06-15T20:29:59+02:00", // 2026-06-15T18:29:59Z == 2026-06-15 23:59:59 +05:30
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_after",
        "anvil",
        "fact",
        "decided",
        "Walrus next midnight",
        "walrus next local midnight in +05:30",
        "task:T-4",
        "2026-06-15T18:30:00Z", // 2026-06-16 00:00:00 +05:30
        "active",
        None,
        "",
        "",
    );

    // Date-only --since and --until under SM_UTC_OFFSET=+05:30 include the full local day only.
    let (code, out, err) = sm_env(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--since",
            "2026-06-15",
            "--until",
            "2026-06-15",
            "--format",
            "json",
        ],
        &[("SM_UTC_OFFSET", "+05:30")],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_end", "m_start"]);

    // Same bounds on `sm query`: matches m_start and m_end only, excludes m_before and m_after.
    let (code, out, err) = sm_env(
        &home,
        &[
            "query",
            "walrus",
            "--repo",
            "anvil",
            "--since",
            "2026-06-15",
            "--until",
            "2026-06-15",
            "--format",
            "json",
        ],
        &[("SM_UTC_OFFSET", "+05:30")],
    );
    assert_eq!(code, 0, "{err}");
    let mut q_ids = json_result_ids(&parse_json(&out));
    q_ids.sort();
    assert_eq!(q_ids, ["m_end", "m_start"]);

    // Exact ISO timestamp --until is inclusive (<=), NOT end-of-day.
    let (code, out, err) = sm_env(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--since",
            "2026-06-14T18:30:00Z",
            "--until",
            "2026-06-14T20:30:00+02:00", // same instant: 18:30:00Z
            "--format",
            "json",
        ],
        &[("SM_UTC_OFFSET", "+05:30")],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_start"]);

    // Mixed offsets sort by true UTC epoch descending, not text order.
    let (code, out, err) = sm_env(
        &home,
        &["list", "--repo", "anvil", "--format", "json"],
        &[("SM_UTC_OFFSET", "+05:30")],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        json_item_ids(&parse_json(&out)),
        ["m_after", "m_end", "m_start", "m_before"]
    );

    // --since > --until errors on both query and list (date-only and exact timestamp).
    for (cmd, extra) in [
        ("list", vec!["--repo", "anvil"]),
        ("query", vec!["walrus", "--repo", "anvil"]),
    ] {
        let mut a1 = vec![cmd];
        a1.extend_from_slice(&extra);
        a1.extend(["--since", "2026-06-16", "--until", "2026-06-15"]);
        let (code, _, err) = sm(&home, &a1);
        assert_eq!(code, 1);
        assert_eq!(err.trim(), "error: --since cannot be after --until");

        let mut a2 = vec![cmd];
        a2.extend_from_slice(&extra);
        a2.extend([
            "--since",
            "2026-06-15T10:00:01Z",
            "--until",
            "2026-06-15T10:00:00Z",
        ]);
        let (code, _, err) = sm(&home, &a2);
        assert_eq!(code, 1);
        assert_eq!(err.trim(), "error: --since cannot be after --until");

        // Trailing prose and blank bounds are rejected strictly.
        let mut a3 = vec![cmd];
        a3.extend_from_slice(&extra);
        a3.extend(["--since", "2026-06-15 trailing"]);
        let (code, _, err) = sm(&home, &a3);
        assert_eq!(code, 1);
        assert!(err.contains("Invalid ISO 8601 timestamp"), "{err}");

        let mut a4 = vec![cmd];
        a4.extend_from_slice(&extra);
        a4.extend(["--until", "   "]);
        let (code, _, err) = sm(&home, &a4);
        assert_eq!(code, 1);
        assert!(err.contains("empty value"), "{err}");
    }
}

#[test]
fn date_boundaries_cover_9999_unicode_digits_1970_pre1970_and_unparseable_as_of() {
    let (_d, home) = scratch();

    // Pre-1970, exact 1970 epoch 0.0, 2026-01-01 local, 9999-12-31 local, and unparseable as_of.
    seed_raw(
        &home,
        "m_pre70",
        "anvil",
        "fact",
        "decided",
        "Epoch pre-1970",
        "aurora pre-1970 body",
        "",
        "1969-12-31T23:59:59Z",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_zero",
        "anvil",
        "fact",
        "decided",
        "Epoch zero",
        "aurora exact 1970 epoch zero body",
        "",
        "1970-01-01T00:00:00Z",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_uni_in",
        "anvil",
        "fact",
        "decided",
        "Unicode day in",
        "aurora on 2026-01-01 local",
        "",
        "2026-01-01T15:00:00",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_uni_out",
        "anvil",
        "fact",
        "decided",
        "Unicode day out",
        "aurora on 2026-01-02 local",
        "",
        "2026-01-02T00:00:00",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_max_date",
        "anvil",
        "fact",
        "decided",
        "Max year 9999",
        "aurora at 9999-12-31 23:59:59 local",
        "",
        "9999-12-31T23:59:59",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_bad",
        "anvil",
        "fact",
        "decided",
        "Bad as_of",
        "aurora with unparseable timestamp",
        "",
        "not-a-timestamp",
        "active",
        None,
        "",
        "",
    );

    // Direct helper assertions for 1970, pre-1970, 9999-12-31, and Unicode digits.
    assert_eq!(
        parse_as_of_epoch_opt(Some("1970-01-01T00:00:00Z")).unwrap(),
        Some(0.0)
    );
    assert_eq!(
        parse_as_of_epoch_opt(Some("1969-12-31T23:59:59Z")).unwrap(),
        Some(-1.0)
    );
    assert_eq!(
        parse_as_of_epoch_opt(Some("not-a-timestamp")).unwrap(),
        None
    );
    let (ep_9999, excl_9999) = parse_date_bound("9999-12-31", true).unwrap();
    assert!(!excl_9999);
    let (ep_9999_start, _) = parse_date_bound("9999-12-31", false).unwrap();
    assert_eq!(ep_9999 - ep_9999_start, 86399.0);
    assert_eq!(
        validate_date_range(Some("9999-12-31"), Some("9999-12-31")).unwrap(),
        (Some(ep_9999_start), Some(ep_9999), false)
    );

    // Pre-1970 through 1970-01-01T00:00:00Z keeps m_zero and m_pre70 and EXCLUDES m_bad (NULL epoch != 0.0).
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--since",
            "1969-12-31T00:00:00Z",
            "--until",
            "1970-01-01T00:00:00Z",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_zero", "m_pre70"]);

    // Full-width Unicode digits `２０２６-01-01` work identically for --since and --until.
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--since",
            "２０２６-01-01",
            "--until",
            "２０２６-01-01",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_uni_in"]);

    // --until 9999-12-31 includes m_max_date (23:59:59 local) and still excludes m_bad (NULL epoch).
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--since",
            "9999-12-31",
            "--until",
            "9999-12-31",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_max_date"]);

    // Without date filters, m_bad appears LAST (NULLS LAST).
    let (code, out, err) = sm(&home, &["list", "--repo", "anvil", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let all_ids = json_item_ids(&parse_json(&out));
    assert_eq!(
        all_ids,
        [
            "m_max_date",
            "m_uni_out",
            "m_uni_in",
            "m_zero",
            "m_pre70",
            "m_bad",
        ]
    );
}

// ---------------------------------------------------------------- 2. --source prefix & index plan

#[test]
fn source_filter_is_case_sensitive_uses_items_source_index_and_handles_unicode_edges() {
    let (_d, home) = scratch();
    let ts = "2026-07-01T10:00:00+00:00";

    seed_raw(
        &home,
        "m_task",
        "anvil",
        "fact",
        "decided",
        "Task source",
        "comet task item",
        "task:T-10",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_upper",
        "anvil",
        "fact",
        "decided",
        "Upper Task source",
        "comet upper Task item",
        "Task:T-11",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_nocolon",
        "anvil",
        "fact",
        "decided",
        "No colon task",
        "comet plain task item",
        "task",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_semi",
        "anvil",
        "fact",
        "decided",
        "Semicolon source",
        "comet task; item",
        "task;T-12",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_agent",
        "anvil",
        "fact",
        "decided",
        "Agent source",
        "comet agent item",
        "agent:sub-1",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_pua_lo",
        "anvil",
        "fact",
        "decided",
        "PUA 10FD7F",
        "comet pua 10FD7F item",
        "\u{10FD7F}tail",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_pua_hi",
        "anvil",
        "fact",
        "decided",
        "PUA 10FE00",
        "comet pua 10FE00 item",
        "\u{10FE00}tail",
        ts,
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_max_cp",
        "anvil",
        "fact",
        "decided",
        "Max code point",
        "comet max code point item",
        "\u{10FFFF}tail",
        ts,
        "active",
        None,
        "",
        "",
    );

    // Case-sensitive prefix match: `task:` keeps only `task:T-10` (not `Task:`, `task`, `task;`).
    let (code, out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--source", "task:", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_task"]);

    // Multi-prefix OR via comma list and repeated `--source` flag on both `list` and `query`.
    let (code, out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--source", "task:", "--source", "agent:", "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_item_ids(&parse_json(&out)), ["m_task", "m_agent"]);

    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "comet",
            "--repo",
            "anvil",
            "--source",
            "task:,agent:",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let mut q_ids = json_result_ids(&parse_json(&out));
    q_ids.sort();
    assert_eq!(q_ids, ["m_agent", "m_task"]);

    // EXPLAIN QUERY PLAN verifies SQLite uses `items_source` for single and multi-prefix filters.
    let conn = open(&home);
    let f_one = Filters {
        include_all_repos: true,
        status_filter: Some(StatusFilter::All),
        sources_filter: Some(vec!["task:".into()]),
        ..Default::default()
    };
    let (where_one, params_one) = build_item_filters("i", &f_one);
    let plan_one: Vec<String> = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT i.id FROM items i WHERE {where_one}"
        ))
        .unwrap()
        .query_map(rusqlite::params_from_iter(params_one), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan_one
            .iter()
            .any(|l| l.contains("USING INDEX items_source")),
        "expected items_source in single-prefix plan: {plan_one:?}"
    );

    let f_multi = Filters {
        include_all_repos: true,
        status_filter: Some(StatusFilter::All),
        sources_filter: Some(vec!["task:".into(), "agent:".into()]),
        ..Default::default()
    };
    let (where_multi, params_multi) = build_item_filters("i", &f_multi);
    let plan_multi: Vec<String> = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT i.id FROM items i WHERE {where_multi}"
        ))
        .unwrap()
        .query_map(rusqlite::params_from_iter(params_multi), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan_multi
            .iter()
            .any(|l| l.contains("USING INDEX items_source")),
        "expected items_source in multi-prefix plan: {plan_multi:?}"
    );

    // U+10FD7F boundary skips 0x10FD80..=0x10FDFF stand-in surrogate range cleanly!
    assert_eq!(
        source_prefix_bounds("\u{10FD7F}"),
        ("\u{10FD7F}".into(), Some("\u{10FE00}".into()))
    );
    let pua_res = list_items(
        &conn,
        &Filters {
            repo: Some("anvil".into()),
            exact_repo: true,
            sources_filter: Some(vec!["\u{10FD7F}".into()]),
            ..Default::default()
        },
        10,
        0,
    )
    .unwrap();
    assert_eq!(
        pua_res
            .items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["m_pua_lo"]
    );

    // U+10FFFF emits lower-bound-only (`source >= ?`) and matches `\u{10FFFF}tail`.
    let max_res = list_items(
        &conn,
        &Filters {
            repo: Some("anvil".into()),
            exact_repo: true,
            sources_filter: Some(vec!["\u{10FFFF}".into()]),
            ..Default::default()
        },
        10,
        0,
    )
    .unwrap();
    assert_eq!(
        max_res
            .items
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["m_max_cp"]
    );

    // Empty-prefix guard: empty prefixes never emit `AND ()` and filter nothing.
    let empty_f = Filters {
        repo: Some("anvil".into()),
        exact_repo: true,
        sources_filter: Some(vec!["".into(), "".into()]),
        ..Default::default()
    };
    assert_eq!(
        build_item_filters("i", &empty_f).0,
        "i.status = 'active' AND i.repo = ?"
    );
    let empty_res = list_items(&conn, &empty_f, 20, 0).unwrap();
    assert_eq!(empty_res.total, 8);

    // Stand-in surrogate inside a source prefix raises UnicodeEncodeError before reaching SQLite.
    let bad_prefix = format!("task:{}", char::from_u32(0x10FDFF).unwrap());
    let err = list_items(
        &conn,
        &Filters {
            sources_filter: Some(vec![bad_prefix]),
            ..Default::default()
        },
        10,
        0,
    )
    .unwrap_err();
    assert_eq!(err.class, "UnicodeEncodeError");
}

// ---------------------------------------------------------------- 3. sm query footer quoting & usage.log

#[test]
fn query_footer_preserves_and_quotes_flags_with_spaces_and_shell_metacharacters() {
    let (_d, home) = scratch();
    let repo_space = "anvil ops";
    for i in 1..=3 {
        let mut it = Item::bare(
            &format!("m_q{i}"),
            repo_space,
            "fact",
            &format!("Quasar note {i}"),
            &format!("quasar alpha body {i}"),
            "decided",
        );
        it.source = "task:a b;c".into();
        it.as_of = format!("2026-06-1{i}T12:00:00+00:00");
        put_item(&open(&home), &it.post_init(), None).unwrap();
    }
    // Item in another repo matching same source/date so `other repos` footer appears too.
    let mut other = Item::bare(
        "m_other",
        "globex",
        "fact",
        "Quasar other",
        "quasar other repo item",
        "decided",
    );
    other.source = "task:a b;c".into();
    other.as_of = "2026-06-12T12:00:00+00:00".into();
    put_item(&open(&home), &other.post_init(), None).unwrap();

    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "quasar alpha",
            "--repo",
            repo_space,
            "--source",
            " task:a b;c , ",
            "--since",
            "2026-06-10 00:00:00",
            "--until",
            "2026-06-20 23:59:59",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let more = out
        .lines()
        .find(|l| l.starts_with("more:"))
        .expect("more line");
    assert!(
        more.contains("--repo 'anvil ops'"),
        "expected quoted --repo in more line: {more}"
    );
    assert!(
        more.contains("--source 'task:a b;c'"),
        "expected normalized & quoted --source in more line: {more}"
    );
    assert!(
        more.contains("--since '2026-06-10 00:00:00'"),
        "expected quoted --since in more line: {more}"
    );
    assert!(
        more.contains("--until '2026-06-20 23:59:59'"),
        "expected quoted --until in more line: {more}"
    );
    assert!(
        more.contains("other repos globex=1 -> sm query \"quasar alpha\" --all --source 'task:a b;c' --since '2026-06-10 00:00:00' --until '2026-06-20 23:59:59'"),
        "expected other repos command to carry quoted filters: {more}"
    );

    // usage.log records query with source/since/until in flags.
    let rows = usage_rows(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cmd"], "query");
    assert_eq!(rows[0]["flags"]["source"], " task:a b;c , ");
    assert_eq!(rows[0]["flags"]["since"], "2026-06-10 00:00:00");
    assert_eq!(rows[0]["flags"]["until"], "2026-06-20 23:59:59");
}

// ---------------------------------------------------------------- 4. sm list scopes, status, pagination, formats, usage.log

#[test]
fn list_scopes_exact_repo_mutex_status_and_unknown_scope() {
    let (d, home) = scratch();
    let repo_dir = d.path().join("anvil");
    std::fs::create_dir_all(&repo_dir).unwrap();
    git(&["init", "-q", repo_dir.to_str().unwrap()]);
    write(
        &home.join("config.toml"),
        "[scopes]\n\"acme\" = [\"repo:globex\", \"GLX\"]\n",
    );

    // Seed active, superseded, retired in anvil; active in global, unassigned, globex, and misc with GLX.
    seed_raw(
        &home,
        "m_act",
        "anvil",
        "fact",
        "decided",
        "Active anvil",
        "active anvil body",
        "task:T-1",
        "2026-08-05T10:00:00+00:00",
        "active",
        None,
        "",
        "R-1",
    );
    seed_raw(
        &home,
        "m_sup",
        "anvil",
        "decision",
        "decided",
        "Superseded anvil",
        "superseded anvil body",
        "task:T-2",
        "2026-08-04T10:00:00+00:00",
        "superseded",
        Some("m_act"),
        "",
        "",
    );
    seed_raw(
        &home,
        "m_ret_null",
        "anvil",
        "fact",
        "decided",
        "Retired null",
        "retired null body",
        "task:T-3",
        "2026-08-03T10:00:00+00:00",
        "superseded",
        None,
        "retired:obsolete",
        "",
    );
    seed_raw(
        &home,
        "m_ret_empty",
        "anvil",
        "fact",
        "decided",
        "Retired empty string",
        "retired empty superseded_by body",
        "task:T-4",
        "2026-08-02T10:00:00+00:00",
        "superseded",
        Some(""),
        "",
        "",
    );
    seed_raw(
        &home,
        "m_global",
        "global",
        "fact",
        "decided",
        "Global item",
        "global item body",
        "",
        "2026-08-06T10:00:00+00:00",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_unassigned",
        "unassigned",
        "fact",
        "decided",
        "Unassigned item",
        "unassigned item body",
        "",
        "2026-08-07T10:00:00+00:00",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_globex",
        "globex",
        "fact",
        "decided",
        "Globex item",
        "globex body",
        "",
        "2026-08-08T10:00:00+00:00",
        "active",
        None,
        "",
        "",
    );
    seed_raw(
        &home,
        "m_glx_word",
        "initech",
        "fact",
        "decided",
        "GLX note",
        "mentions GLX scope code",
        "",
        "2026-08-09T10:00:00+00:00",
        "active",
        None,
        "",
        "",
    );

    // Default inside git repo `anvil` uses exact_repo=true (excludes global & unassigned!).
    let (code, out, err) = sm_in_cwd(&home, &repo_dir, &["list", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["scope"], json!({"repo": "anvil", "all_repos": false}));
    assert_eq!(v["status"], "active");
    assert_eq!(json_item_ids(&v), ["m_act"]);

    // `--all` returns every active repo including global/unassigned, with scope.repo == null.
    let (code, out, err) = sm(&home, &["list", "--all", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["scope"], json!({"repo": null, "all_repos": true}));
    assert_eq!(
        json_item_ids(&v),
        [
            "m_glx_word",
            "m_globex",
            "m_unassigned",
            "m_global",
            "m_act"
        ]
    );

    // `--in acme` searches configured scope across all repos with scope.repo == null and scope.in == ["acme"].
    let (code, out, err) = sm(&home, &["list", "--in", "acme", "--format", "json"]);
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(
        v["scope"],
        json!({"repo": null, "all_repos": true, "in": ["acme"]})
    );
    assert_eq!(json_item_ids(&v), ["m_glx_word", "m_globex"]);

    // Unknown `--in` scope exits 2 like `sm query`.
    let (code, _, err) = sm(&home, &["list", "--in", "unknown-scope"]);
    assert_eq!(code, 2);
    assert!(err.contains("unknown scope: unknown-scope"), "{err}");

    // Scope mutex errors exit 2.
    for bad_args in [
        vec!["list", "--repo", "anvil", "--all"],
        vec!["list", "--repo", "anvil", "--in", "acme"],
        vec!["list", "--all", "--in", "acme"],
    ] {
        let (code, _, err) = sm(&home, &bad_args);
        assert_eq!(code, 2, "expected exit 2 for {bad_args:?}, got {err}");
        assert!(err.contains("not allowed with argument"), "{err}");
    }

    // Status variants: superseded vs retired vs all vs --include-history.
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--status",
            "superseded",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["status"], "superseded");
    assert_eq!(json_item_ids(&v), ["m_sup"]);
    assert_eq!(v["items"][0]["superseded_by"], "m_act");
    assert!(v["items"][0].get("retired").is_none());

    let (code, out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--status", "retired", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["status"], "retired");
    assert_eq!(json_item_ids(&v), ["m_ret_null", "m_ret_empty"]);
    assert_eq!(v["items"][0]["retired"], "obsolete");
    assert_eq!(v["items"][1]["retired"], "file deleted");
    assert!(v["items"][0].get("superseded_by").is_none());

    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--include-history",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["status"], "all");
    assert_eq!(
        json_item_ids(&v),
        ["m_act", "m_sup", "m_ret_null", "m_ret_empty"]
    );

    // Status mutex: --status and --include-history cannot combine.
    let (code, _, err) = sm(&home, &["list", "--status", "active", "--include-history"]);
    assert_eq!(code, 2);
    assert!(err.contains("not allowed with argument"), "{err}");
}

#[test]
fn list_pagination_bigint_safety_all_formats_and_usage_log() {
    let (_d, home) = scratch();
    let long_body = format!(
        "Paragraph about quartz failover procedures. {}",
        "extended operational details for budget truncation. ".repeat(40)
    );
    let mut created_ids = Vec::new();
    for i in 1..=5 {
        let id = put(
            &home,
            &[
                "--repo",
                "anvil",
                "--kind",
                "fact",
                "--tag",
                "verified",
                "--evidence",
                "runbook check",
                "--source",
                &format!("task:Q-{i}"),
                "--refs",
                &format!("Q-{i} R-{i}"),
                "--title",
                &format!("Quartz item {i}"),
                "--as-of",
                &format!("2026-09-0{i}T10:00:00+00:00"),
                &format!("Item {i}: {long_body}"),
            ],
        );
        created_ids.push(id);
    }
    // Newest first: item 5, 4, 3, 2, 1.
    let expected_order: Vec<String> = created_ids.into_iter().rev().collect();

    // Page 1 (brief default): limit 2, offset 0 -> shows #1, #2 + more: + next: sm show ...
    let (code, out, err) = sm(&home, &["list", "--repo", "anvil", "--limit", "2"]);
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with(&format!(
        "[{}] #1 anvil fact verified 2026-09-05 ",
        expected_order[0]
    )));
    assert!(lines[0].ends_with(": Quartz item 5"));
    assert!(lines[1].starts_with(&format!(
        "[{}] #2 anvil fact verified 2026-09-04 ",
        expected_order[1]
    )));
    assert_eq!(
        lines[2],
        "more: 3 remaining (showing 1-2 of 5) -> sm list --repo anvil --limit 2 --offset 2"
    );
    assert_eq!(
        lines[3],
        format!("next: sm show {} {}", expected_order[0], expected_order[1])
    );

    // Page 2 (text format): limit 2, offset 2 -> shows #3, #4, caps total output to MAX_TOTAL_CHARS.
    let (code, out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--limit", "2", "--offset", "2", "--format", "text",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&format!(
        "[{}] #3 repo=anvil kind=fact tag=verified ",
        expected_order[2]
    )));
    assert!(out.contains("source=task:Q-3 refs=R-3"));
    assert!(out.contains(&format!(
        "[{}] #4 repo=anvil kind=fact tag=verified ",
        expected_order[3]
    )));
    assert!(out.contains("more: 1 remaining (showing 3-4 of 5) -> sm list --repo anvil --format text --limit 2 --offset 4"));
    assert!(
        out.chars().count() <= 2450,
        "text output should respect MAX_TOTAL_CHARS cap: {}",
        out.chars().count()
    );

    // Final page (json format): limit 2, offset 4 -> shows #5, no more, next has 1 id.
    let (code, out, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--limit", "2", "--offset", "4", "--format", "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["total"], 5);
    assert_eq!(v["limit"], 2);
    assert_eq!(v["offset"], 4);
    assert_eq!(v["status"], "active");
    assert_eq!(v["scope"], json!({"repo": "anvil", "all_repos": false}));
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["rank"], 5);
    assert_eq!(v["items"][0]["id"], expected_order[4]);
    assert_eq!(v["items"][0]["source"], "task:Q-1");
    assert_eq!(v["items"][0]["refs"], json!(["Q-1", "R-1"]));
    // 297-char cut ends on a space before rstrip, leaving 296 chars + "..." = 299 chars.
    let snippet = v["items"][0]["snippet"].as_str().unwrap();
    assert!(snippet.ends_with("..."), "{snippet}");
    assert_eq!(snippet.chars().count(), 299);
    assert_eq!(v["next"], json!([format!("sm show {}", expected_order[4])]));

    // BigInt --offset and --limit past i64::MAX never overflow in pagination or usage logging!
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            "anvil",
            "--limit",
            "99999999999999999999",
            "--offset",
            "99999999999999999999",
            "--format",
            "json",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v = parse_json(&out);
    assert_eq!(v["total"], 5);
    assert_eq!(v["limit"], i64::MAX);
    assert_eq!(v["offset"], i64::MAX);
    assert_eq!(v["items"], json!([]));
    assert_eq!(v["next"], json!([]));

    // Brief with offset past total prints nothing and exits 0.
    let (code, out, err) = sm(&home, &["list", "--repo", "anvil", "--offset", "99"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");

    // Invalid --limit / --offset rejected BEFORE tag/kind/config/DB validation.
    let (code, _, err) = sm(
        &home,
        &["list", "--repo", "anvil", "--limit", "0", "--tag", "bogus"],
    );
    assert_eq!(code, 1);
    assert_eq!(err.trim(), "error: --limit must be at least 1");

    let (code, _, err) = sm(
        &home,
        &[
            "list", "--repo", "anvil", "--offset", "-1", "--tag", "bogus",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(err.trim(), "error: --offset must be at least 0");

    // Verify usage.log recorded all 5 successful `list` calls with `"cmd": "list"` and exact flags.
    let rows = usage_rows(&home);
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert_eq!(row["cmd"], "list");
        assert_eq!(row["repo"], "anvil");
    }
    assert_eq!(
        rows[0]["ids"],
        json!([expected_order[0], expected_order[1]])
    );
    assert_eq!(rows[0]["flags"]["status"], "active");
    assert_eq!(rows[0]["flags"]["format"], "brief");
    assert_eq!(rows[0]["flags"]["limit"], 2);
    assert_eq!(rows[0]["flags"]["offset"], 0);
    assert_eq!(rows[0]["flags"]["all"], false);
    let raw_usage = std::fs::read_to_string(home.join("usage.log")).unwrap();
    let raw_lines: Vec<&str> = raw_usage.lines().collect();
    assert!(
        raw_lines[3].contains("\"offset\": 99999999999999999999"),
        "{}",
        raw_lines[3]
    );
}

#[test]
fn list_ties_order_by_id_desc_quotes_footer_flags_and_logs_null_repo_for_all() {
    let (_d, home) = scratch();
    let repo_space = "anvil lab";
    let same_ts = "2026-10-05T12:00:00+00:00";
    for id in ["m_a", "m_c", "m_b"] {
        seed_raw(
            &home,
            id,
            repo_space,
            "fact",
            "decided",
            &format!("Tie {id}"),
            &format!("tie body {id}"),
            "task:a b;c",
            same_ts,
            "active",
            None,
            "",
            "",
        );
    }

    // Equal epochs tie-break by id DESC: m_c, m_b, m_a.
    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--repo",
            repo_space,
            "--source",
            " task:a b;c , ",
            "--since",
            "2026-10-05 00:00:00",
            "--until",
            "2026-10-06",
            "--status",
            "active",
            "--limit",
            "2",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("[m_c] #1 "), "{out}");
    assert!(lines[1].starts_with("[m_b] #2 "), "{out}");
    assert_eq!(
        lines[2],
        "more: 1 remaining (showing 1-2 of 3) -> sm list --repo 'anvil lab' --source 'task:a b;c' --since '2026-10-05 00:00:00' --until 2026-10-06 --status active --limit 2 --offset 2"
    );
    assert_eq!(lines[3], "next: sm show m_c m_b");

    // Comma-only --source/--kind/--tag filter nothing and are omitted; spaced lists normalize to comma-joined tokens.
    let (code, out, err) = sm(
        &home,
        &[
            "list", "--all", "--source", " , , ", "--kind", " , ", "--tag", " , ", "--limit", "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let more = out.lines().find(|l| l.starts_with("more:")).unwrap();
    assert!(!more.contains("--source"), "{more}");
    assert!(!more.contains("--kind"), "{more}");
    assert!(!more.contains("--tag"), "{more}");
    assert_eq!(
        more,
        "more: 2 remaining (showing 1-1 of 3) -> sm list --all --limit 1 --offset 1"
    );

    let (code, out, err) = sm(
        &home,
        &[
            "list",
            "--all",
            "--kind",
            "fact, decision",
            "--tag",
            "decided, verified",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let more = out.lines().find(|l| l.starts_with("more:")).unwrap();
    assert_eq!(
        more,
        "more: 2 remaining (showing 1-1 of 3) -> sm list --all --kind fact,decision --tag decided,verified --limit 1 --offset 1"
    );

    // Same normalization holds on `sm query` footer commands.
    let (code, out, err) = sm(
        &home,
        &[
            "query",
            "Tie",
            "--all",
            "--kind",
            "fact, decision",
            "--tag",
            "decided, verified",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let qmore = out.lines().find(|l| l.starts_with("more:")).unwrap();
    assert!(
        qmore.contains("--tag decided,verified --kind fact,decision"),
        "{qmore}"
    );

    // usage.log records repo: null when --all is used.
    let rows = usage_rows(&home);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["repo"], repo_space);
    assert_eq!(rows[1]["cmd"], "list");
    assert!(rows[1]["repo"].is_null(), "{}", rows[1]);
    assert_eq!(rows[1]["flags"]["all"], true);
}
