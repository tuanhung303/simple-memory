//! Ports of the Python upkeep tests that no Rust unit test plays with the
//! same inputs: the dedup rules, the `sm put` overlap warning and the two `sm upkeep` CLI
//! tests. Each Python test keeps its name (snake_case); every sm process gets HOME and
//! SIMPLE_MEMORY_HOME in scratch. Expected values are the Python tests' literals. The other
//! upkeep tests are covered by the unit tests of src/service/upkeep.rs (rows in the
//! lane report). The put-overlap and report tests use the real clock, as Python does.
mod common;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use common::sm;
use simple_memory::core::dedup::{idf, near_duplicate_score, normalize_title, word_overlap};
use simple_memory::core::item::{Item, epoch_to_as_of};
use simple_memory::store::connect::connect;
use simple_memory::store::items::{get_item, put_item};

const S1: &str = "11111111-aaaa-4bbb-8ccc-000000000001";

fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = std::fs::canonicalize(d.path()).unwrap();
    (d, p)
}

fn db(home: &Path) -> rusqlite::Connection {
    std::fs::create_dir_all(home).unwrap();
    connect(home.join("memory.db").to_str().unwrap(), None).unwrap()
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn handoff(home: &Path, id: &str, stamp: &str, session: &str) {
    let it = Item {
        source: format!("handoff:{stamp}-{session}.md"),
        as_of: "2026-11-01T10:00:00+00:00".into(),
        ..Item::bare(
            id,
            "a",
            "handoff",
            &format!("Handoff {id}"),
            &format!("body of {id}"),
            "derived",
        )
    };
    put_item(&db(home), &it.post_init(), None).unwrap();
}

#[test]
fn normalize_title_and_near_duplicate_rules() {
    assert_eq!(
        normalize_title(Some("  Đorđe: QRX_review (done)! ")),
        "dorde qrx review done"
    );
    assert_eq!(normalize_title(Some("!!! --- ")), "");
    let n = 1000;
    let df = |t: &str| -> f64 {
        let d = match t {
            "x1" | "x2" | "x3" => 20,
            "rare10" => 10,
            "df11" => 11,
            "common" => 400,
            other => panic!("no df for {other}"),
        };
        idf(d, n)
    };
    let theta = idf(11, n); // a token in 10 or fewer titles is rare
    let shared = ["x1", "x2", "x3"];
    let with = |extra: &'static str| -> Vec<&'static str> {
        shared.iter().copied().chain([extra]).collect()
    };
    // by hand: idf(20) = ln(1 + 1000/20.5) = 3.908, idf(11) = 4.477, so cosine = 0.834
    let score = near_duplicate_score(&shared, &with("df11"), df, theta).expect("a score");
    assert_eq!((score * 1000.0).round() as i64, 834);
    // idf(10) = 4.566 > theta: cosine 0.829 clears 0.8, but the one-sided rare token vetoes it
    assert_eq!(
        near_duplicate_score(&shared, &with("rare10"), df, theta),
        None
    );
    // one shared token is noise, whatever the cosine
    assert_eq!(
        near_duplicate_score(&["x1", "common"], &["x1"], df, theta),
        None
    );
    // two shared tokens plus one common word (idf 1.252): sqrt(30.54 / (30.54 + 1.57)) = 0.975
    assert!(near_duplicate_score(&["x1", "x2"], &["x1", "x2", "common"], df, theta).is_some());
    // x3 and df11 on one side only: sqrt(30.54 / (30.54 + 15.27 + 20.05)) = 0.681, under 0.8
    assert_eq!(
        near_duplicate_score(&["x1", "x2"], &["x1", "x2", "x3", "df11"], df, theta),
        None
    );
}

#[test]
fn word_overlap_share() {
    assert_eq!(
        word_overlap("alpha beta gamma delta", "beta alpha zeta"),
        0.5
    );
    assert_eq!(word_overlap("Đorđe café", "dorde cafe finished"), 1.0);
    assert_eq!(word_overlap("", "anything"), 0.0);
}

#[test]
fn put_overlap_warns_and_still_stores() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let t = now();
    let hour_ago = epoch_to_as_of(t - 3600.0).unwrap();
    let day_and_minute_ago = epoch_to_as_of(t - 24.0 * 3600.0 - 60.0).unwrap();
    {
        let conn = db(&home);
        for (id, repo, kind, as_of, text) in [
            (
                "m_task_recent",
                "a",
                "task",
                &hour_ago,
                "alpha beta gamma delta",
            ),
            (
                "m_task_old",
                "a",
                "task",
                &day_and_minute_ago,
                "alpha beta gamma delta epsilon",
            ),
            (
                "m_task_other_repo",
                "b",
                "task",
                &hour_ago,
                "alpha beta gamma delta epsilon",
            ),
            (
                "m_fact_recent",
                "a",
                "fact",
                &hour_ago,
                "alpha beta gamma delta epsilon fact",
            ),
            (
                "m_task_done",
                "a",
                "task",
                &hour_ago,
                "alpha beta gamma delta epsilon done",
            ),
            ("m_handoff_low", "a", "handoff", &hour_ago, "alpha zeta"),
        ] {
            let it = Item {
                as_of: as_of.clone(),
                ..Item::bare(id, repo, kind, text, text, "assumed")
            };
            put_item(&conn, &it.post_init(), None).unwrap();
        }
        conn.execute(
            "UPDATE items SET status = 'superseded' WHERE id = 'm_task_done'",
            [],
        )
        .unwrap();
    }

    // new item: alpha beta gamma delta epsilon theta -> 5 of 6 words in m_fact_recent (83%),
    // 4 of 6 in m_task_recent (67%), 1 of 6 in m_handoff_low (17%)
    let args = [
        "put",
        "--repo",
        "a",
        "--tag",
        "assumed",
        "alpha beta gamma delta epsilon theta",
    ];
    let (code, out, err) = sm(&home, &args);
    assert_eq!(code, 0);
    let new_id = out.trim().to_string();
    assert_eq!(
        get_item(&db(&home), &new_id).unwrap().unwrap().body,
        "alpha beta gamma delta epsilon theta"
    );
    assert_eq!(
        err,
        format!(
            "sm put: {new_id} shares 83% of its words with fact m_fact_recent ({h}): alpha beta gamma delta epsilon fact; \
             if it replaces that item, run sm supersede m_fact_recent {new_id}\n\
             sm put: {new_id} shares 67% of its words with task m_task_recent ({h}): alpha beta gamma delta; \
             if it replaces that item, run sm supersede m_task_recent {new_id}\n",
            h = &hour_ago[..16]
        )
    );
    // the same text again dedupes to the stored item: no new item, no warning
    let (code, out, err) = sm(&home, &args);
    assert_eq!((code, out.trim(), err.as_str()), (0, new_id.as_str(), ""));
}

#[test]
fn cli_upkeep_prints_counts_and_rejects_report_dry_run() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    handoff(&home, "m_h1", "20261101-080000", S1);
    handoff(&home, "m_h2", "20261101-090000", S1);
    let (code, out, _) = sm(&home, &["upkeep", "--dry-run"]);
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "upkeep (dry-run): sessions=1 superseded=1 handoffs_without_session=0\n"
        )
    );
    let (code, out, _) = sm(&home, &["upkeep"]);
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "upkeep (applied): sessions=1 superseded=1 handoffs_without_session=0\n"
        )
    );
    let (code, _, err) = sm(&home, &["upkeep", "--report", "--dry-run"]);
    assert_eq!(code, 1);
    assert!(err.contains("drop --dry-run"), "{err}");
    let (code, out, _) = sm(&home, &["upkeep", "--report"]);
    assert_eq!(code, 0);
    assert_eq!(
        out.lines().next(),
        Some("title_groups=0 items=0 near_duplicate_pairs=0")
    );
}

#[test]
fn cli_upkeep_report_prints_stale_section() {
    let (_d, tmp) = tmpdir();
    let home = tmp.join("home");
    let (code, out, _) = sm(&home, &["upkeep", "--report"]);
    assert_eq!(code, 0);
    assert_eq!(
        out.lines().last(),
        Some("stale_planned_assumed=0 older_than_days=30")
    );
    let old = epoch_to_as_of(now() - 40.0 * 24.0 * 3600.0).unwrap();
    let it = Item {
        as_of: old.clone(),
        ..Item::bare("m_s1", "z", "task", "Plan\n  next", "b1", "planned")
    };
    put_item(&db(&home), &it.post_init(), None).unwrap();
    let (code, out, _) = sm(&home, &["upkeep", "--report"]);
    assert_eq!(code, 0);
    let all: Vec<&str> = out.lines().collect();
    assert_eq!(
        all[all.len() - 3..],
        [
            "stale_planned_assumed=1 older_than_days=30",
            "stale repo=z n=1",
            &format!("  m_s1 planned {} Plan next", &old[..10])
        ]
    );
}
