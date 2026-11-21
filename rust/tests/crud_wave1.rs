//! Wave 1 Lane A integration and boundary tests:
//! - Migration 0002 (fresh, v1 -> v2 upgrade, EXPLAIN QUERY PLAN index use)
//! - `sm retire` (reasons, redaction, show output, errors, catch-up non-resurrection)
//! - Append-only `sm amend` (fields, redaction, source detachment, legacy/turn, dedupe, no-op, errors)
//! - Widened `put_overlaps` (durable items, boundary inputs, ordering and cap)
mod common;

use std::path::{Path, PathBuf};

use common::{git, open, put, scratch, sm, write};
use rusqlite::Connection;
use serde_json::json;
use simple_memory::core::item::{Item, epoch_to_as_of, parse_as_of_epoch, validate_as_of};
use simple_memory::error::SmError;
use simple_memory::service::doctor::{import_lag, run_doctor};
use simple_memory::service::import_::{run_automemory_import, run_import};
use simple_memory::service::upkeep::put_overlaps;
use simple_memory::store::connect::connect;
use simple_memory::store::items::{
    AmendPatch, amend_item, get_item, put_item, retire_item, retire_item_with_reason,
};
use simple_memory::store::upkeep::durable_overlap_candidates;

const FAKE_AWS_KEY_A: &str = "AKIAQQQQQQQQQQQQQQQQ";
const FAKE_AWS_KEY_B: &str = "AKIAZZZZZZZZZZZZZZZZ";

fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn index_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn err_text(e: SmError) -> String {
    format!("{}: {}", e.class, e.msg)
}

fn sample_item(id: &str, repo: &str, kind: &str, body: &str, tag: &str) -> Item {
    Item {
        as_of: validate_as_of(Some("2026-11-01T10:00:00+00:00"), false, false).unwrap(),
        ..Item::bare(id, repo, kind, body, body, tag)
    }
    .post_init()
}

// ------------------------------------------------------------------ Migration 0002

#[test]
fn migration_0002_upgrades_v1_store_and_indexes_drive_query_plans() {
    let (_d, home) = scratch();
    let db_path = home.join("memory.db");

    // Seed a v1 store using only 0001_init.sql and user_version = 1.
    {
        let raw = Connection::open(&db_path).unwrap();
        let sql_v1 = include_str!("../migrations/0001_init.sql");
        // Register sm_fold so v1's FTS insert triggers run on the seeded rows.
        raw.create_scalar_function(
            "sm_fold",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let s: Option<String> = ctx.get(0)?;
                Ok(simple_memory::core::fold::fold(s.as_deref()))
            },
        )
        .unwrap();
        raw.execute_batch(sql_v1).unwrap();
        raw.execute_batch("PRAGMA user_version = 1;").unwrap();
        raw.execute(
            "INSERT INTO items (id, repo, kind, title, body, source, tag, as_of, status, superseded_by, content_hash) \
             VALUES ('m_old', 'alpha', 'fact', 'T1', 'Body one', 'task:t1', 'decided', '2026-10-01T00:00:00+00:00', 'superseded', 'm_cur', 'h1')",
            [],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO items (id, repo, kind, title, body, source, tag, as_of, status, superseded_by, content_hash) \
             VALUES ('m_cur', 'alpha', 'fact', 'T2', 'Body two', 'task:t2', 'decided', '2026-10-02T00:00:00+00:00', 'active', NULL, 'h2')",
            [],
        )
        .unwrap();
        let pre_indexes = index_names(&raw);
        assert!(!pre_indexes.contains(&"items_status_repo".to_string()));
        assert!(!pre_indexes.contains(&"items_source".to_string()));
        assert!(!pre_indexes.contains(&"items_superseded_by".to_string()));
    }

    // Reopening via connect() upgrades v1 -> v3, creates all 3 indexes, and preserves all rows.
    let conn = connect(db_path.to_str().unwrap(), None).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let post_indexes = index_names(&conn);
    for idx in ["items_status_repo", "items_source", "items_superseded_by"] {
        assert!(
            post_indexes.contains(&idx.to_string()),
            "missing {idx} in {post_indexes:?}"
        );
    }
    assert_eq!(get_item(&conn, "m_old").unwrap().unwrap().body, "Body one");
    assert_eq!(get_item(&conn, "m_cur").unwrap().unwrap().body, "Body two");

    // EXPLAIN QUERY PLAN verifies each new index is chosen for its target query shape.
    let p_status_repo = query_plan(
        &conn,
        "SELECT id FROM items WHERE status = 'active' AND repo = 'alpha'",
    );
    assert!(
        p_status_repo
            .iter()
            .any(|l| l.contains("USING COVERING INDEX items_status_repo")
                || l.contains("USING INDEX items_status_repo")),
        "{p_status_repo:?}"
    );
    let p_source = query_plan(&conn, "SELECT id FROM items WHERE source = 'task:t2'");
    assert!(
        p_source
            .iter()
            .any(|l| l.contains("USING COVERING INDEX items_source")
                || l.contains("USING INDEX items_source")),
        "{p_source:?}"
    );
    let p_sup = query_plan(&conn, "SELECT id FROM items WHERE superseded_by = 'm_cur'");
    assert!(
        p_sup
            .iter()
            .any(|l| l.contains("USING COVERING INDEX items_superseded_by")
                || l.contains("USING INDEX items_superseded_by")),
        "{p_sup:?}"
    );

    // Reconnecting is idempotent.
    drop(conn);
    let conn2 = connect(db_path.to_str().unwrap(), None).unwrap();
    let version2: i64 = conn2
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version2, 3);
}

// ------------------------------------------------------------------ sm retire

#[test]
fn cli_retire_sets_reason_redacts_secrets_and_updates_show_output() {
    let (_d, home) = scratch();
    let id_default = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "--tags",
            "ops, cache",
            "First item to retire with default reason",
        ],
    );
    let (code, out, err) = sm(&home, &["retire", &format!(" {id_default} ")]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, format!("{id_default} (retired: manual)\n").as_str(), "")
    );
    let (code, shown, err) = sm(&home, &["show", &id_default]);
    assert_eq!((code, err.as_str()), (0, ""));
    let lines: Vec<&str> = shown.lines().collect();
    assert!(lines.contains(&"status: superseded"), "{shown}");
    assert!(
        lines.contains(&"tags: ops, cache, retired:manual"),
        "{shown}"
    );
    assert!(lines.contains(&"retired: manual"), "{shown}");
    assert!(
        !lines.iter().any(|l| l.starts_with("superseded_by:")),
        "{shown}"
    );

    // Unicode + multi-whitespace + secret redaction in --reason.
    let id_secret = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "Second item with redacted retire reason",
        ],
    );
    let raw_reason = format!(
        "  café\n\tĐorđe leaked {FAKE_AWS_KEY_A} and <private>secret,with,comma</private>  "
    );
    let (code, out, err) = sm(&home, &["retire", &id_secret, "--reason", &raw_reason]);
    let expected_reason = "café Đorđe leaked [REDACTED:aws_key] and [REDACTED:private]";
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            format!("{id_secret} (retired: {expected_reason})\n").as_str(),
            ""
        )
    );
    let (code, shown, _) = sm(&home, &["show", &id_secret]);
    assert_eq!(code, 0);
    assert!(
        shown
            .lines()
            .any(|l| l == format!("retired: {expected_reason}")),
        "{shown}"
    );
    assert!(!shown.contains(FAKE_AWS_KEY_A), "{shown}");
    assert!(!shown.contains("secret,with,comma"), "{shown}");

    // Blank --reason falls back to "manual".
    let id_blank = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "Third item blank reason",
        ],
    );
    let (code, out, err) = sm(&home, &["retire", &id_blank, "--reason", "  \t\n  "]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (0, format!("{id_blank} (retired: manual)\n").as_str(), "")
    );

    // File-deleted item retired via store::items::retire_item (no retired:* tag) shows "file deleted".
    let id_deleted = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "Fourth item file deleted",
        ],
    );
    assert_eq!(retire_item(&open(&home), &id_deleted).unwrap(), 1);
    let (code, shown, _) = sm(&home, &["show", &id_deleted]);
    assert_eq!(code, 0);
    assert!(
        shown.lines().any(|l| l == "retired: file deleted"),
        "{shown}"
    );
}

#[test]
fn cli_retire_error_paths_and_stale_retired_tag_replacement() {
    let (_d, home) = scratch();
    // Missing item
    let (code, out, err) = sm(&home, &["retire", "m_nonexistent"]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            1,
            "",
            "error: item m_nonexistent not found (ids are case-sensitive and must be given in full)\n"
        )
    );

    // Comma in reason fails before mutating DB
    let id_a = put(
        &home,
        &["--repo", "alpha", "--tag", "decided", "Item alpha one"],
    );
    let (code, _, err) = sm(&home, &["retire", &id_a, "--reason", "stale, duplicate"]);
    assert_eq!(
        (code, err.as_str()),
        (1, "error: Retire reason cannot contain commas\n")
    );
    assert_eq!(
        get_item(&open(&home), &id_a).unwrap().unwrap().status,
        "active"
    );

    // Already retired
    let (code, _, _) = sm(&home, &["retire", &id_a, "--reason", "obsolete"]);
    assert_eq!(code, 0);
    let (code, _, err) = sm(&home, &["retire", &id_a]);
    assert_eq!(
        (code, err.as_str()),
        (
            1,
            format!("error: Item {id_a} is already retired\n").as_str()
        )
    );

    // Already superseded
    let id_b = put(
        &home,
        &["--repo", "alpha", "--tag", "decided", "Item beta old"],
    );
    let id_c = put(
        &home,
        &["--repo", "alpha", "--tag", "decided", "Item beta new"],
    );
    let (code, _, _) = sm(&home, &["supersede", &id_b, &id_c]);
    assert_eq!(code, 0);
    let (code, _, err) = sm(&home, &["retire", &id_b]);
    assert_eq!(
        (code, err.as_str()),
        (
            1,
            format!("error: Item {id_b} is already superseded by {id_c}\n").as_str()
        )
    );

    // Blank superseded_by on a superseded row is treated as retired, not superseded.
    {
        let conn = open(&home);
        conn.execute(
            "UPDATE items SET superseded_by = '   ' WHERE id = ?",
            [&id_b],
        )
        .unwrap();
    }
    let (code, _, err) = sm(&home, &["retire", &id_b]);
    assert_eq!(
        (code, err.as_str()),
        (
            1,
            format!("error: Item {id_b} is already retired\n").as_str()
        )
    );

    // An active item carrying a stale retired:* tag has it stripped and replaced cleanly.
    let id_d = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "--tags",
            "keep, retired:old",
            "Item delta stale tag",
        ],
    );
    let (code, out, _) = sm(&home, &["retire", &id_d, "--reason", "fresh"]);
    assert_eq!(
        (code, out.as_str()),
        (0, format!("{id_d} (retired: fresh)\n").as_str())
    );
    assert_eq!(
        get_item(&open(&home), &id_d).unwrap().unwrap().tags,
        "keep, retired:fresh"
    );
}

fn write_tasklog(path: &Path, id: &str, title: &str, body: &str) {
    write(
        path,
        &format!(
            "id: {id}\n\
             repo: alpha\n\
             date: 2026-11-01\n\
             title: {title}\n\
             status: done\n\
             summary: |\n  {body}\n"
        ),
    );
}

fn write_handoff(path: &Path, title: &str, body: &str) {
    write(path, &format!("# {title}\n\n{body}\n"));
}

fn write_automemory(root: &Path, project: &str, name: &str, body: &str) -> PathBuf {
    let f = root.join(project).join("memory").join(format!("{name}.md"));
    write(
        &f,
        &format!(
            "---\nname: {name}\ndescription: {name}\nmetadata:\n  type: project\n  tag: decided\n  modified: 2026-11-01T01:00:00Z\n---\n{body}\n"
        ),
    );
    f
}

#[test]
fn catchup_does_not_resurrect_manually_retired_tasklog_handoff_or_automemory() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    let gh = root.join("gh");
    let repo = gh.join("alpha");
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", repo.to_str().unwrap()]);

    let tl_file = repo.join("docs/tasklog/2026-11-01/100000-11223344-demo-abcd.yaml");
    write_tasklog(
        &tl_file,
        "20261101-100000-11223344-demo-abcd",
        "Task Demo",
        "Original task body",
    );

    let ho_dir = root.join("handoffs");
    let ho_file = ho_dir.join("20261101-100000-demo-session.md");
    write_handoff(&ho_file, "Handoff Demo", "Original handoff body");

    let am_root = root.join("projects");
    std::fs::create_dir_all(am_root.join("p-alpha/memory")).unwrap();
    write(
        &am_root.join("p-alpha/s1.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    let am_file = write_automemory(&am_root, "p-alpha", "note_one", "Original automemory body");

    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nenabled = true\nroot = \"{}\"\n\
             [upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = [\"{}\"]\n\
             [repos]\nroots = [\"{}\"]\n",
            am_root.display(),
            gh.display(),
            ho_dir.display(),
            gh.display(),
        ),
    );
    let h = home.to_str().unwrap();

    // 1) Import all three sources.
    assert_eq!(
        run_import("tasklog", tl_file.to_str().unwrap(), false, false, Some(h))
            .unwrap()
            .inserted,
        1
    );
    assert_eq!(
        run_import("handoff", ho_file.to_str().unwrap(), false, false, Some(h))
            .unwrap()
            .inserted,
        1
    );
    let am_r1 = run_automemory_import(None, false, Some(h)).unwrap();
    assert_eq!((am_r1.added, am_r1.updated, am_r1.unchanged), (1, 0, 0));

    let conn = open(&home);
    let id_for = |prefix: &str| -> String {
        conn.query_row(
            "SELECT id FROM items WHERE source LIKE ? AND status = 'active'",
            [format!("{prefix}%")],
            |r| r.get(0),
        )
        .unwrap()
    };
    let tl_id = id_for("task:");
    let ho_id = id_for("handoff:");
    let am_id = id_for("automemory:");

    // 2) Manually retire all three via retire_item_with_reason.
    retire_item_with_reason(&conn, &tl_id, "no longer needed").unwrap();
    retire_item_with_reason(&conn, &ho_id, "stale handoff").unwrap();
    retire_item_with_reason(&conn, &am_id, "wrong memory").unwrap();

    // 3) doctor --imports sees zero not_stored across tasklog, handoff, and automemory.
    let doc = run_doctor(false, false, false, true, Some(h)).unwrap();
    let (il, am_lag, _) = doc.lag.unwrap();
    assert!(il.not_stored.is_empty(), "not_stored={:?}", il.not_stored);
    assert!(
        am_lag.unwrap().automemory_not_stored.is_empty(),
        "automemory_not_stored should be empty"
    );

    // 4) Direct re-imports do NOT resurrect any of the three.
    let tl_re = run_import("tasklog", tl_file.to_str().unwrap(), false, false, Some(h)).unwrap();
    assert_eq!((tl_re.inserted, tl_re.deduped), (0, 1));
    let ho_re = run_import("handoff", ho_file.to_str().unwrap(), false, false, Some(h)).unwrap();
    assert_eq!((ho_re.inserted, ho_re.deduped), (0, 1));
    let am_re = run_automemory_import(None, false, Some(h)).unwrap();
    assert_eq!((am_re.added, am_re.updated, am_re.unchanged), (0, 0, 1));
    for id in [&tl_id, &ho_id, &am_id] {
        assert_eq!(
            get_item(&conn, id).unwrap().unwrap().status,
            "superseded",
            "{id} should remain retired"
        );
    }

    // 5) False-positive tag (`not_retired:x` or bare `retired:`) does NOT suppress re-import.
    conn.execute(
        "UPDATE items SET tags = 'not_retired:x, retired:' WHERE id = ?",
        [&tl_id],
    )
    .unwrap();
    let cfg = simple_memory::config::load_config(Some(h)).unwrap();
    let lag_fp = import_lag(&conn, &cfg).unwrap();
    assert_eq!(
        lag_fp.not_stored,
        vec![tl_file.to_str().unwrap().to_string()]
    );
    // Re-importing stores it because it has no real retired:<reason> tag (file-deleted
    // semantics); the row's tags no longer equal the file's, so a new version is stored in place
    // of the old row.
    let tl_restore =
        run_import("tasklog", tl_file.to_str().unwrap(), false, false, Some(h)).unwrap();
    assert_eq!(tl_restore.inserted, 1);
    assert_eq!(
        get_item(&conn, &tl_id).unwrap().unwrap().status,
        "superseded"
    );
    let active_tl: String = conn
        .query_row(
            "SELECT tags FROM items WHERE source LIKE 'task:%' AND status = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_tl, "as_of:mtime");

    // 6) Editing a manually retired automemory file DOES re-import the new version!
    write_automemory(&am_root, "p-alpha", "note_one", "Edited automemory body v2");
    let _ = am_file;
    let am_ed = run_automemory_import(None, false, Some(h)).unwrap();
    assert_eq!((am_ed.added, am_ed.updated, am_ed.unchanged), (1, 0, 0));
    let active_am: String = conn
        .query_row(
            "SELECT body FROM items WHERE source LIKE 'automemory:%' AND status = 'active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active_am, "Edited automemory body v2");
}

// ------------------------------------------------------------------ sm amend

#[test]
fn cli_amend_metadata_body_title_refs_tags_and_redaction() {
    let (_d, home) = scratch();
    // Seed an existing active durable item in repo alpha to verify overlap warning on body amend.
    let _peer = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "quartz basalt granite marble slate",
        ],
    );

    let orig_id = put(
        &home,
        &[
            "--repo",
            "alpha",
            "--kind",
            "fact",
            "--tag",
            "assumed",
            "--refs",
            "R1",
            "--tags",
            "Alpha, keep",
            "Initial line one\nSecond detail line",
        ],
    );

    // 1) Metadata-only amend (same repo & body -> exercises atomic UPDATE before INSERT under items_dedupe)
    //    and emits NO overlap warning on stderr since body didn't change.
    let (code, out, err) = sm(
        &home,
        &[
            "amend",
            &orig_id,
            "--kind",
            "DECISION",
            "--tag",
            "verified",
            "--evidence",
            "cargo test --locked",
            "--add-refs",
            "R2",
            "--add-refs",
            "R3",
            "--remove-refs",
            "r1",
            "--add-tags",
            "beta",
            "--add-tags",
            "gamma",
            "--remove-tags",
            "alpha",
            "--as-of",
            "2026-11-15T12:00:00+00:00",
        ],
    );
    assert_eq!((code, err.as_str()), (0, ""));
    let v2_id = out.trim().to_string();
    assert_ne!(v2_id, orig_id);

    let conn = open(&home);
    let old = get_item(&conn, &orig_id).unwrap().unwrap();
    assert_eq!(
        (old.status.as_str(), old.superseded_by.as_deref()),
        ("superseded", Some(v2_id.as_str()))
    );
    let v2 = get_item(&conn, &v2_id).unwrap().unwrap();
    assert_eq!(v2.status, "active");
    assert_eq!(v2.kind, "decision");
    assert_eq!(v2.tag, "verified");
    assert_eq!(v2.evidence, "cargo test --locked");
    assert_eq!(v2.refs, "R2 R3");
    assert_eq!(v2.tags, "keep, beta, gamma");
    assert_eq!(v2.title, "Initial line one");
    assert_eq!(v2.body, "Initial line one\nSecond detail line");

    // 2) Body amend when old.title == default_title(&old.body) recomputes title AND triggers
    //    put_overlaps warning on stderr!
    let (code, out, err) = sm(
        &home,
        &[
            "amend",
            &v2_id,
            "--body",
            "quartz basalt granite marble obsidian",
        ],
    );
    assert_eq!(code, 0);
    let v3_id = out.trim().to_string();
    assert!(
        err.contains(&format!(
            "sm put: {v3_id} shares 80% of its words with fact"
        )),
        "expected overlap warning on stderr, got: {err}"
    );
    let v3 = get_item(&conn, &v3_id).unwrap().unwrap();
    assert_eq!(v3.title, "quartz basalt granite marble obsidian");
    assert_eq!(v3.body, "quartz basalt granite marble obsidian");

    // 3) Custom title is preserved when --body changes without --title.
    let (code, out, _) = sm(&home, &["amend", &v3_id, "--title", "Pinned Custom Title"]);
    assert_eq!(code, 0);
    let v4_id = out.trim().to_string();
    let (code, out, _) = sm(
        &home,
        &["amend", &v4_id, "--body", "Completely different text R7"],
    );
    assert_eq!(code, 0);
    let v5_id = out.trim().to_string();
    let v5 = get_item(&conn, &v5_id).unwrap().unwrap();
    assert_eq!(v5.title, "Pinned Custom Title");
    assert_eq!(v5.refs, "R2 R3 R7");

    // 4) Secret redaction in --body (including a secret straddling char 120 in auto-title
    //    and a <private> block holding ref R99).
    let raw_id = put(&home, &["--repo", "beta", "--tag", "decided", "Short body"]);
    let long_prefix = "w".repeat(106);
    let secret_body = format!(
        "{long_prefix} password=SuperSecretValue12345 and {FAKE_AWS_KEY_A} <private>R99</private> R88"
    );
    let (code, out, err) = sm(&home, &["amend", &raw_id, "--body", &secret_body]);
    assert_eq!((code, err.as_str()), (0, ""));
    let red_id = out.trim().to_string();
    let red = get_item(&conn, &red_id).unwrap().unwrap();
    assert!(!red.body.contains("SuperSecret"), "{}", red.body);
    assert!(!red.title.contains("SuperSecret"), "{}", red.title);
    assert!(!red.title.contains("password="), "{}", red.title);
    assert!(!red.body.contains(FAKE_AWS_KEY_A), "{}", red.body);
    assert!(red.body.contains("[REDACTED:key_value]"), "{}", red.body);
    assert!(red.body.contains("[REDACTED:aws_key]"), "{}", red.body);
    assert!(red.body.contains("[REDACTED:private]"), "{}", red.body);
    assert_eq!(red.refs, "R88");
    assert_eq!(red.title.chars().count(), 120);

    // 5) Post-redaction no-op: replacing one AWS key with another AWS key produces the same
    //    redacted body -> rejected with No changes to apply!
    let secret_item_id = put(
        &home,
        &[
            "--repo",
            "beta",
            "--tag",
            "decided",
            &format!("token {FAKE_AWS_KEY_A}"),
        ],
    );
    let (code, _, err) = sm(
        &home,
        &[
            "amend",
            &secret_item_id,
            "--body",
            &format!("token {FAKE_AWS_KEY_B}"),
        ],
    );
    assert_eq!((code, err.as_str()), (1, "error: No changes to apply\n"));

    // 6) CLI no-flag amend exits 1 with the required-field error.
    let (code, out, err) = sm(&home, &["amend", &secret_item_id]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (1, "", "error: amend needs at least one field to change\n")
    );
}

#[test]
fn amend_handles_task_handoff_automemory_legacy_and_turn_sources() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    let gh = root.join("gh");
    let repo = gh.join("alpha");
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", repo.to_str().unwrap()]);

    let tl_file = repo.join("docs/tasklog/2026-11-01/100000-aabbccdd-amend-abcd.yaml");
    write_tasklog(
        &tl_file,
        "20261101-100000-aabbccdd-amend-abcd",
        "Task Amend",
        "Original task text",
    );

    let am_root = root.join("projects");
    std::fs::create_dir_all(am_root.join("p-alpha/memory")).unwrap();
    write(
        &am_root.join("p-alpha/s1.jsonl"),
        &format!("{}\n", json!({"cwd": repo.to_str().unwrap()})),
    );
    write_automemory(&am_root, "p-alpha", "mem_amend", "Original memory text");

    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    write(
        &home.join("config.toml"),
        &format!(
            "[automemory]\nenabled = true\nroot = \"{}\"\n\
             [upkeep]\ntasklog_roots = [\"{}\"]\nhandoff_dirs = []\n\
             [repos]\nroots = [\"{}\"]\n",
            am_root.display(),
            gh.display(),
            gh.display(),
        ),
    );
    let h = home.to_str().unwrap();

    run_import("tasklog", tl_file.to_str().unwrap(), false, false, Some(h)).unwrap();
    run_automemory_import(None, false, Some(h)).unwrap();

    let conn = open(&home);
    let tl_id: String = conn
        .query_row("SELECT id FROM items WHERE source LIKE 'task:%'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let am_id: String = conn
        .query_row(
            "SELECT id FROM items WHERE source LIKE 'automemory:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Amending task: gives new.source amended:<source> and automemory: clears it, so
    // catch-up/automemory never reverts it.
    let (tl_new, body_changed) = amend_item(
        &conn,
        &tl_id,
        &AmendPatch {
            body: Some("Amended task text"),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(body_changed);
    assert_eq!(
        get_item(&conn, &tl_new).unwrap().unwrap().source,
        "amended:task:20261101-100000-aabbccdd-amend-abcd"
    );
    let (am_new, _) = amend_item(
        &conn,
        &am_id,
        &AmendPatch {
            body: Some("Amended automemory text"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(get_item(&conn, &am_new).unwrap().unwrap().source, "");

    // Neither tasklog re-import nor automemory re-sync reverts the amendments.
    let cfg = simple_memory::config::load_config(Some(h)).unwrap();
    assert!(import_lag(&conn, &cfg).unwrap().not_stored.is_empty());
    let tl_re = run_import("tasklog", tl_file.to_str().unwrap(), false, false, Some(h)).unwrap();
    assert_eq!((tl_re.inserted, tl_re.deduped), (0, 1));
    let am_re = run_automemory_import(None, false, Some(h)).unwrap();
    assert_eq!((am_re.added, am_re.updated, am_re.unchanged), (0, 0, 1));
    assert_eq!(get_item(&conn, &tl_new).unwrap().unwrap().status, "active");
    assert_eq!(get_item(&conn, &am_new).unwrap().unwrap().status, "active");

    // A handoff item also keeps its file as amended:<source>.
    let ho_item = Item {
        source: "handoff:20261101-100000-demo-session.md".into(),
        ..sample_item("m_ho_1", "alpha", "handoff", "Handoff body", "derived")
    };
    put_item(&conn, &ho_item, None).unwrap();
    let (ho_new, _) = amend_item(
        &conn,
        "m_ho_1",
        &AmendPatch {
            tag: Some("decided"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        get_item(&conn, &ho_new).unwrap().unwrap().source,
        "amended:handoff:20261101-100000-demo-session.md"
    );

    // Legacy item with id = "legacy:memories:1", source = "", tag = "n/a":
    // omitting --tag preserves n/a by copying old.id into new.source; subsequent amend works too.
    put_item(
        &conn,
        &sample_item("legacy:memories:1", "alpha", "fact", "Legacy fact", "n/a"),
        None,
    )
    .unwrap();
    let (leg_v2, _) = amend_item(
        &conn,
        "legacy:memories:1",
        &AmendPatch {
            body: Some("Legacy fact clarified"),
            ..Default::default()
        },
    )
    .unwrap();
    let lv2 = get_item(&conn, &leg_v2).unwrap().unwrap();
    assert_eq!(lv2.tag, "n/a");
    assert_eq!(lv2.source, "legacy:memories:1");
    let (leg_v3, _) = amend_item(
        &conn,
        &leg_v2,
        &AmendPatch {
            add_tags: Some("cleaned"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        get_item(&conn, &leg_v3).unwrap().unwrap().source,
        "legacy:memories:1"
    );

    // Explicitly passing --tag n/a on legacy: is rejected.
    let err = err_text(
        amend_item(
            &conn,
            &leg_v3,
            &AmendPatch {
                tag: Some("n/a"),
                ..Default::default()
            },
        )
        .unwrap_err(),
    );
    assert!(err.starts_with("ValueError: Invalid tag 'n/a'"), "{err}");

    // Turn item with id = "turn:abcdef12", source = "", tag = "n/a" preserves turn: source too.
    put_item(
        &conn,
        &sample_item("turn:abcdef12", "alpha", "episode", "Turn note", "n/a"),
        None,
    )
    .unwrap();
    let (turn_v2, _) = amend_item(
        &conn,
        "turn:abcdef12",
        &AmendPatch {
            body: Some("Turn note clarified"),
            ..Default::default()
        },
    )
    .unwrap();
    let tv2 = get_item(&conn, &turn_v2).unwrap().unwrap();
    assert_eq!(
        (tv2.tag.as_str(), tv2.source.as_str()),
        ("n/a", "turn:abcdef12")
    );
}

#[test]
fn amend_dedupe_collision_noop_formatting_preservation_and_errors() {
    let (_d, home) = scratch();
    let conn = open(&home);

    // Seed items with legacy formatting in refs/tags and evidence holding R99.
    let it_a = Item {
        refs: "R1, R2".into(),
        tags: "one,two".into(),
        evidence: "see R99 in log".into(),
        ..sample_item("m_a", "alpha", "fact", "Body A", "verified")
    };
    let it_b = sample_item("m_b", "alpha", "fact", "Body B", "decided");
    let it_other_repo = sample_item("m_c", "beta", "fact", "Body A", "decided");
    put_item(&conn, &it_a, None).unwrap();
    put_item(&conn, &it_b, None).unwrap();
    put_item(&conn, &it_other_repo, None).unwrap();

    // 1) No flags -> rejected
    assert_eq!(
        err_text(amend_item(&conn, "m_a", &AmendPatch::default()).unwrap_err()),
        "ValueError: amend needs at least one field to change"
    );

    // 2) Missing id -> KeyError
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                "m_missing",
                &AmendPatch {
                    repo: Some("beta"),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "KeyError: item m_missing not found (ids are case-sensitive and must be given in full)"
    );

    // 3) No-op checks (unchanged values, no-op add/remove refs & tags, same as_of)
    for patch in [
        AmendPatch {
            kind: Some(" FACT "),
            ..Default::default()
        },
        AmendPatch {
            add_refs: Some("R1"),
            ..Default::default()
        },
        AmendPatch {
            remove_refs: Some("R404"),
            ..Default::default()
        },
        AmendPatch {
            add_tags: Some("one"),
            ..Default::default()
        },
        AmendPatch {
            remove_tags: Some("missing"),
            ..Default::default()
        },
        AmendPatch {
            as_of: Some("2026-11-01T10:00:00+00:00"),
            ..Default::default()
        },
    ] {
        assert_eq!(
            err_text(amend_item(&conn, "m_a", &patch).unwrap_err()),
            "ValueError: No changes to apply",
            "expected no-op for {patch:?}"
        );
    }

    // 4) Unchanged refs/tags keep exact original formatting and do NOT extract from unchanged evidence.
    let (a_v2, body_changed) = amend_item(
        &conn,
        "m_a",
        &AmendPatch {
            kind: Some("learning"),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!body_changed);
    let av2 = get_item(&conn, &a_v2).unwrap().unwrap();
    assert_eq!(av2.refs, "R1, R2");
    assert_eq!(av2.tags, "one,two");

    // 5) Dedupe collision: amending body to match active item m_b in same repo
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    body: Some("Body B"),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        format!(
            "ValueError: Repo 'alpha' already has an active item with the same body (m_b); use sm supersede {a_v2} m_b"
        )
    );
    assert_eq!(get_item(&conn, &a_v2).unwrap().unwrap().status, "active");

    // 6) Dedupe collision: amending repo into beta where m_c already holds "Body A"
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    repo: Some("beta"),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        format!(
            "ValueError: Repo 'beta' already has an active item with the same body (m_c); use sm supersede {a_v2} m_c"
        )
    );
    assert_eq!(get_item(&conn, &a_v2).unwrap().unwrap().status, "active");

    // 7) Clearing evidence on a verified item fails validation
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    evidence: Some("   "),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "ValueError: Tag 'verified' needs non-empty evidence"
    );

    // 8) Blank repo / title / body rejected
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    repo: Some("  "),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "ValueError: Repo cannot be empty"
    );
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    title: Some("  "),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "ValueError: Item title cannot be empty"
    );
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                &a_v2,
                &AmendPatch {
                    body: Some("  "),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "ValueError: Item body cannot be empty"
    );

    // 9) Superseded item error follows multi-hop chain to active head; retired item reports retired.
    let (a_v3, _) = amend_item(
        &conn,
        &a_v2,
        &AmendPatch {
            as_of: Some("2026-11-18T00:00:00+00:00"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                "m_a",
                &AmendPatch {
                    tag: Some("decided"),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        format!("ValueError: Cannot amend superseded item m_a (active head: {a_v3})")
    );
    retire_item_with_reason(&conn, "m_b", "done").unwrap();
    assert_eq!(
        err_text(
            amend_item(
                &conn,
                "m_b",
                &AmendPatch {
                    tag: Some("decided"),
                    ..Default::default()
                }
            )
            .unwrap_err()
        ),
        "ValueError: Cannot amend retired item m_b"
    );
}

// ------------------------------------------------------------------ Widened put_overlaps

#[test]
fn put_overlaps_widened_durable_items_boundaries_ordering_and_cap() {
    let (_d, home) = scratch();
    let h = home.to_str().unwrap();
    let conn = open(&home);
    let now = parse_as_of_epoch(Some("2026-11-20T12:00:00+00:00")).unwrap();
    let two_years_ago = "2024-05-10T09:00:00+00:00";

    // Seed old durable items (fact, decision, learning) plus non-matching kinds/repos/statuses.
    for (id, repo, kind, as_of, text) in [
        (
            "m_d1",
            "alpha",
            "decision",
            "2024-05-10T09:00:00+00:00",
            "cedar birch maple spruce willow walnut",
        ),
        (
            "m_d2",
            "alpha",
            "learning",
            "2025-01-01T09:00:00+00:00",
            "cedar birch maple spruce pine",
        ),
        (
            "m_d3",
            "alpha",
            "fact",
            "2025-06-01T09:00:00+00:00",
            "cedar birch maple spruce oak",
        ),
        (
            "m_d4",
            "alpha",
            "fact",
            "2025-06-01T09:00:00+00:00",
            "cedar birch maple spruce elm",
        ),
        (
            "m_d5",
            "alpha",
            "fact",
            two_years_ago,
            "cedar birch maple alder beech",
        ),
        (
            "m_d6",
            "alpha",
            "fact",
            "2024-01-01T09:00:00+00:00",
            "cedar birch maple aspen ash",
        ),
        (
            "m_d7",
            "alpha",
            "learning",
            "2023-01-01T09:00:00+00:00",
            "cedar birch maple larch fir",
        ),
        // Excluded: episode kind, other repo, superseded durable item, prefix-only match.
        (
            "m_ep",
            "alpha",
            "episode",
            &epoch_to_as_of(now - 600.0).unwrap(),
            "cedar birch maple spruce willow episode",
        ),
        (
            "m_other_repo",
            "beta",
            "fact",
            two_years_ago,
            "cedar birch maple spruce willow",
        ),
        (
            "m_sup_fact",
            "alpha",
            "fact",
            two_years_ago,
            "cedar birch maple spruce willow extra",
        ),
        (
            "m_prefix_only",
            "alpha",
            "fact",
            two_years_ago,
            "cedars birches maples spruces willows",
        ),
    ] {
        put_item(
            &conn,
            &Item {
                as_of: as_of.into(),
                ..Item::bare(id, repo, kind, text, text, "decided")
            }
            .post_init(),
            None,
        )
        .unwrap();
    }
    retire_item_with_reason(&conn, "m_sup_fact", "old").unwrap();

    let target = Item {
        as_of: "2026-11-20T12:00:00+00:00".into(),
        ..Item::bare(
            "m_target",
            "alpha",
            "fact",
            "cedar birch maple spruce willow",
            "cedar birch maple spruce willow",
            "decided",
        )
    }
    .post_init();
    assert_eq!(
        put_item(&conn, &target, None).unwrap(),
        ("m_target".to_string(), true)
    );

    // Prefix-only item is NOT returned by exact-quoted FTS5 durable_overlap_candidates.
    let cand_ids: Vec<String> = durable_overlap_candidates(&conn, &target, 10)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(
        !cand_ids.contains(&"m_prefix_only".to_string()),
        "{cand_ids:?}"
    );
    assert!(!cand_ids.contains(&"m_ep".to_string()), "{cand_ids:?}");
    assert!(
        !cand_ids.contains(&"m_other_repo".to_string()),
        "{cand_ids:?}"
    );
    assert!(
        !cand_ids.contains(&"m_sup_fact".to_string()),
        "{cand_ids:?}"
    );

    // Ordering: share DESC (100%: m_d1; 80%: m_d3 & m_d4 tied on 2025-06-01 -> id ASC, then m_d2 2025-01-01;
    // 60%: m_d5 2024-05-10), capped at 5 (m_d6 and m_d7 dropped).
    let lines = put_overlaps("m_target", Some(h), Some(now)).unwrap();
    assert_eq!(lines.len(), 5, "{lines:?}");
    let expected_order = [
        ("100%", "decision", "m_d1"),
        ("80%", "fact", "m_d3"),
        ("80%", "fact", "m_d4"),
        ("80%", "learning", "m_d2"),
        ("60%", "fact", "m_d5"),
    ];
    for (line, (pct, kind, id)) in lines.iter().zip(expected_order) {
        let want = format!("sm put: m_target shares {pct} of its words with {kind} {id} (");
        assert!(line.starts_with(&want), "got: {line}\nwant prefix: {want}");
    }

    // Boundary 1: Symbol-only body does NOT fail with FTS5 MATCH '' error.
    let sym = sample_item("m_sym", "alpha", "fact", "!!! --- ???", "decided");
    put_item(&conn, &sym, None).unwrap();
    assert_eq!(
        put_overlaps("m_sym", Some(h), Some(now)).unwrap(),
        Vec::<String>::new()
    );

    // Boundary 2: Stopword-only / 1-char-only body falls back to all deduped word_runs.
    put_item(
        &conn,
        &sample_item("m_stop_a", "gamma", "fact", "the and to a", "decided"),
        None,
    )
    .unwrap();
    put_item(
        &conn,
        &sample_item("m_stop_b", "gamma", "fact", "the and to b", "decided"),
        None,
    )
    .unwrap();
    let stop_lines = put_overlaps("m_stop_b", Some(h), Some(now)).unwrap();
    assert_eq!(stop_lines.len(), 1, "{stop_lines:?}");
    assert!(
        stop_lines[0].contains("shares 75% of its words with fact m_stop_a"),
        "{}",
        stop_lines[0]
    );

    // Boundary 3: FTS5 operator keywords + quotes + parens + Unicode accents work cleanly.
    put_item(
        &conn,
        &sample_item(
            "m_uni_a",
            "delta",
            "decision",
            "dorde cafe NOT NEAR quote parens",
            "decided",
        ),
        None,
    )
    .unwrap();
    put_item(
        &conn,
        &sample_item(
            "m_uni_b",
            "delta",
            "fact",
            "Đorđe café \"NOT\" (NEAR) *quote*",
            "decided",
        ),
        None,
    )
    .unwrap();
    let uni_lines = put_overlaps("m_uni_b", Some(h), Some(now)).unwrap();
    assert_eq!(uni_lines.len(), 1, "{uni_lines:?}");
    assert!(
        uni_lines[0].contains("shares 100% of its words with decision m_uni_a"),
        "{}",
        uni_lines[0]
    );

    // Boundary 4: non-positive limit returns empty rather than SQLite LIMIT -1 (unbounded).
    assert!(
        durable_overlap_candidates(&conn, &target, 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        durable_overlap_candidates(&conn, &target, -1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn amend_and_put_redact_before_extract_refs_and_strip_retired_tags() {
    let (_d, home) = scratch();
    let h = home.to_str().unwrap();

    // 1. Context-keyed secrets matching SHA_RE / TICKET_RE must NOT leak into refs or FTS.
    let (code, out, err) = sm(
        &home,
        &["put", "--repo", "alpha", "--tag", "decided", "initial"],
    );
    assert_eq!(code, 0, "{err}");
    let id1 = out.trim().to_string();

    let (code, out, err) = sm(
        &home,
        &[
            "amend",
            &id1,
            "--body",
            "auth Bearer deadbeef12345678 and sig=0123456789abcdef and password=AB-99999999",
            "--evidence",
            "curl -H 'Authorization: Bearer cafe0123456789ab'",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let id2 = out.trim().to_string();
    let conn = connect(home.join("memory.db").to_str().unwrap(), None).unwrap();
    let it2 = get_item(&conn, &id2).unwrap().unwrap();
    assert_eq!(it2.refs, "", "secret leaked into amend refs: {}", it2.refs);
    assert!(!it2.body.contains("deadbeef12345678"));
    assert!(!it2.evidence.contains("cafe0123456789ab"));
    let (code, qout, err) = sm(&home, &["query", "deadbeef12345678", "--repo", "alpha"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        qout.trim(),
        "missing: deadbeef12345678=0",
        "secret searchable in FTS: {qout}"
    );

    // 1b. `sm amend --title` also redacts before extract_refs.
    let (code, out, err) = sm(
        &home,
        &["amend", &id2, "--title", "key password=feedface1234"],
    );
    assert_eq!(code, 0, "{err}");
    let it2_title = get_item(&conn, out.trim()).unwrap().unwrap();
    assert_eq!(
        it2_title.refs, "",
        "amend --title leaked secret ref: {}",
        it2_title.refs
    );
    assert!(!it2_title.title.contains("feedface1234"));

    // 2. <private> block straddling char 120 must NOT leak R99 into title or refs.
    let (code, out, err) = sm(
        &home,
        &["put", "--repo", "alpha", "--tag", "decided", "Short body"],
    );
    assert_eq!(code, 0, "{err}");
    let id_priv = out.trim().to_string();
    let straddle = format!("{} <private>R99 secret</private>", "w".repeat(105));
    let (code, out, err) = sm(&home, &["amend", &id_priv, "--body", &straddle]);
    assert_eq!(code, 0, "{err}");
    let id_priv2 = out.trim().to_string();
    let it_priv = get_item(&conn, &id_priv2).unwrap().unwrap();
    assert_eq!(it_priv.refs, "", "private ref leaked: {}", it_priv.refs);
    assert!(!it_priv.title.contains("R99"), "title={}", it_priv.title);

    // 3. Post-redaction no-op must fail with "No changes to apply" and leave secret unindexed.
    let (code, out, err) = sm(
        &home,
        &[
            "put",
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "clean [REDACTED:key_value] text",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let id_noop = out.trim().to_string();
    let (code, _, err) = sm(
        &home,
        &[
            "amend",
            &id_noop,
            "--body",
            "clean password=deadbeef1234 text",
        ],
    );
    assert_eq!(code, 1);
    assert!(err.contains("No changes to apply"), "{err}");
    let (code, qout, err) = sm(&home, &["query", "deadbeef1234", "--repo", "alpha"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        qout.trim(),
        "missing: deadbeef1234=0",
        "no-op secret indexed: {qout}"
    );

    // 4. Peer fix: `sm put` also redacts body, title, and evidence before default_title and extract_refs.
    let (code, out, err) = sm(
        &home,
        &[
            "put",
            "--repo",
            "alpha",
            "--tag",
            "decided",
            "--title",
            "t Bearer aabbccdd11223344",
            "--evidence",
            "sig=1122334455667788",
            "check password=cafebabe5678 now",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let id_put = out.trim().to_string();
    let it_put = get_item(&conn, &id_put).unwrap().unwrap();
    assert_eq!(it_put.refs, "", "put leaked secret ref: {}", it_put.refs);
    assert!(!it_put.title.contains("aabbccdd11223344"));
    assert!(!it_put.evidence.contains("1122334455667788"));
    let straddle_put = format!("{} <private>R88 secret</private>", "q".repeat(105));
    let (code, out, err) = sm(
        &home,
        &["put", "--repo", "alpha", "--tag", "decided", &straddle_put],
    );
    assert_eq!(code, 0, "{err}");
    let it_put_priv = get_item(&conn, out.trim()).unwrap().unwrap();
    assert_eq!(it_put_priv.refs, "");
    assert!(!it_put_priv.title.contains("R88"));

    // 5. `sm amend --add-tags` ignores spoofed `retired:*` tags and strips stale `retired:*` on active items.
    let (code, out, err) = sm(
        &home,
        &["amend", &id_put, "--add-tags", "keep, retired:spoof"],
    );
    assert_eq!(code, 0, "{err}");
    let id_tag_clean = out.trim().to_string();
    let it_tag = get_item(&conn, &id_tag_clean).unwrap().unwrap();
    assert_eq!(it_tag.tags, "keep");

    // Seed an active item carrying a stale `retired:old` tag directly, then amend it.
    let mut stale_active = sample_item("m_stale_ret", "alpha", "fact", "stale tag body", "decided");
    stale_active.tags = "retired:old, retain".into();
    put_item(&conn, &stale_active, None).unwrap();
    let (code, out, err) = sm(
        &home,
        &["amend", "m_stale_ret", "--title", "Refreshed title"],
    );
    assert_eq!(code, 0, "{err}");
    let it_refreshed = get_item(&conn, out.trim()).unwrap().unwrap();
    assert_eq!(it_refreshed.tags, "retain");
    let _ = h;
}
